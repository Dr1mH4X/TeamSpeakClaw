//! TeamSpeak 加密模块
//!
//! 连接链路按 TS3AudioBot 的关键规则实现：
//! - Init1 使用固定 MAC: "TS3INIT1"
//! - Unencrypted 包使用 fake signature（由 iv_struct 派生）
//! - 会话加密：AES-EAX(tag 8 bytes)，AAD = 线协议 header

use aes::Aes128;
use base64::Engine;
use curve25519_dalek::constants::ED25519_BASEPOINT_TABLE;
use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsPoint};
use curve25519_dalek::scalar::Scalar;
use eax::aead::consts::U8;
use eax::aead::{AeadInPlace, KeyInit};
use eax::{Eax, Nonce, Tag};
use num_bigint::BigUint;
use p256::ecdsa::{signature::Verifier, Signature, VerifyingKey};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::PublicKey;
use sha2::{Digest, Sha512};

use super::error::{HeadlessError, Result};
use super::identity::{ts_hash, ts_hash256, Identity};
use super::packet::{Packet, PacketDirection, PacketType, MAC_LEN};

/// 固定 Init1 MAC
const TS3_INIT_MAC: [u8; MAC_LEN] = *b"TS3INIT1";
/// 方向字节（与 TS3AudioBot 对齐）
const DIR_CLIENT_TO_SERVER: u8 = 0x31;
const DIR_SERVER_TO_CLIENT: u8 = 0x30;

/// AES-EAX，tag 长度 8 字节
type Aes128Eax8 = Eax<Aes128, U8>;

/// TeamSpeak 加密处理器
pub struct TsCrypto {
    identity: Identity,
    shared_secret: Option<[u8; 20]>,
    iv_struct: Option<Vec<u8>>,
    alpha_tmp: Option<[u8; 10]>,
    crypto_init_complete: bool,
    fake_signature: [u8; MAC_LEN],
}

impl Clone for TsCrypto {
    fn clone(&self) -> Self {
        Self {
            identity: self.identity.clone(),
            shared_secret: self.shared_secret,
            iv_struct: self.iv_struct.clone(),
            alpha_tmp: self.alpha_tmp,
            crypto_init_complete: self.crypto_init_complete,
            fake_signature: self.fake_signature,
        }
    }
}

impl TsCrypto {
    pub fn new(identity: Identity) -> Self {
        Self {
            identity,
            shared_secret: None,
            iv_struct: None,
            alpha_tmp: None,
            crypto_init_complete: false,
            fake_signature: [0u8; MAC_LEN],
        }
    }

    pub fn crypto_init(&mut self, alpha: &[u8], beta: &[u8], omega: &[u8]) -> Result<()> {
        let server_public_key = self.import_server_public_key(omega)?;
        let shared_secret = self.identity.compute_shared_secret_sha1(&server_public_key);
        self.set_shared_secret_from_raw(alpha, beta, &shared_secret)
    }

    pub fn crypto_init2(
        &mut self,
        license_b64: &str,
        omega_b64: &str,
        proof_b64: &str,
        beta_b64: &str,
        temporary_private_key: &[u8; 32],
    ) -> Result<()> {
        let license = base64::engine::general_purpose::STANDARD
            .decode(license_b64)
            .map_err(|e| {
                HeadlessError::CryptoError(format!("license base64 decode failed: {e}"))
            })?;
        let omega = base64::engine::general_purpose::STANDARD
            .decode(omega_b64)
            .map_err(|e| HeadlessError::CryptoError(format!("omega base64 decode failed: {e}")))?;
        let proof = base64::engine::general_purpose::STANDARD
            .decode(proof_b64)
            .map_err(|e| HeadlessError::CryptoError(format!("proof base64 decode failed: {e}")))?;
        let beta = base64::engine::general_purpose::STANDARD
            .decode(beta_b64)
            .map_err(|e| HeadlessError::CryptoError(format!("beta base64 decode failed: {e}")))?;

        let server_key = self.import_server_public_key(&omega)?;
        if !verify_ecdsa_sha256_der(&server_key, &license, &proof)? {
            return Err(HeadlessError::CryptoError(
                "initivexpand2 proof signature verification failed".into(),
            ));
        }

        let license_chain = parse_licenses(&license)?;
        let derived_key = derive_license_key(&license_chain)?;
        let shared2 = get_shared_secret2(&derived_key, temporary_private_key)?;
        let alpha = self.alpha_tmp.ok_or_else(|| {
            HeadlessError::CryptoError("Missing alpha_tmp for CryptoInit2".into())
        })?;

        self.set_shared_secret_from_raw(&alpha, &beta, &shared2)
    }

    pub fn generate_temporary_keypair() -> ([u8; 32], [u8; 32]) {
        let mut sk = [0u8; 32];
        fill_random_bytes(&mut sk);
        sk[0] &= 248;
        sk[31] &= 127;
        sk[31] |= 64;

        let pk_point: EdwardsPoint = &Scalar::from_bytes_mod_order(sk) * ED25519_BASEPOINT_TABLE;
        let pk = pk_point.compress().to_bytes();
        (pk, sk)
    }

    fn import_server_public_key(&self, omega: &[u8]) -> Result<PublicKey> {
        if omega.len() == 65 && omega[0] == 0x04 {
            return PublicKey::from_sec1_bytes(omega)
                .map_err(|e| HeadlessError::CryptoError(format!("Invalid public key: {e}")));
        }

        if let Ok(key) = self.parse_ts_der_public_key(omega) {
            return Ok(key);
        }

        Err(HeadlessError::CryptoError(
            "Could not parse server public key".into(),
        ))
    }

    fn parse_ts_der_public_key(&self, data: &[u8]) -> Result<PublicKey> {
        let mut idx = 0usize;
        if data.get(idx).copied() != Some(0x30) {
            return Err(HeadlessError::CryptoError("DER is not a SEQUENCE".into()));
        }
        idx += 1;
        let (seq_len, seq_len_len) = parse_der_len(&data[idx..])?;
        idx += seq_len_len;
        if idx + seq_len > data.len() {
            return Err(HeadlessError::CryptoError(
                "DER sequence length overflow".into(),
            ));
        }

        // Field 1: BIT STRING (flags)
        if data.get(idx).copied() != Some(0x03) {
            return Err(HeadlessError::CryptoError("Missing DER BIT STRING".into()));
        }
        idx += 1;
        let (field1_len, field1_len_len) = parse_der_len(&data[idx..])?;
        idx += field1_len_len + field1_len;

        // Field 2: INTEGER (size)
        if data.get(idx).copied() != Some(0x02) {
            return Err(HeadlessError::CryptoError("Missing DER key size".into()));
        }
        idx += 1;
        let (field2_len, field2_len_len) = parse_der_len(&data[idx..])?;
        idx += field2_len_len + field2_len;

        // Field 3: INTEGER x
        if data.get(idx).copied() != Some(0x02) {
            return Err(HeadlessError::CryptoError(
                "Missing DER x coordinate".into(),
            ));
        }
        idx += 1;
        let (x_len, x_len_len) = parse_der_len(&data[idx..])?;
        idx += x_len_len;
        if idx + x_len > data.len() {
            return Err(HeadlessError::CryptoError("DER x length overflow".into()));
        }
        let x = data[idx..idx + x_len].to_vec();
        idx += x_len;

        // Field 4: INTEGER y
        if data.get(idx).copied() != Some(0x02) {
            return Err(HeadlessError::CryptoError(
                "Missing DER y coordinate".into(),
            ));
        }
        idx += 1;
        let (y_len, y_len_len) = parse_der_len(&data[idx..])?;
        idx += y_len_len;
        if idx + y_len > data.len() {
            return Err(HeadlessError::CryptoError("DER y length overflow".into()));
        }
        let y = data[idx..idx + y_len].to_vec();

        let mut point = [0u8; 65];
        point[0] = 0x04;
        write_32_be(&mut point[1..33], &x);
        write_32_be(&mut point[33..65], &y);

        PublicKey::from_sec1_bytes(&point)
            .map_err(|e| HeadlessError::CryptoError(format!("Invalid public key point: {e}")))
    }

    fn set_shared_secret_from_raw(
        &mut self,
        alpha: &[u8],
        beta: &[u8],
        shared_key: &[u8],
    ) -> Result<()> {
        if beta.len() != 10 && beta.len() != 54 {
            return Err(HeadlessError::CryptoError(format!(
                "Invalid beta size: {}",
                beta.len()
            )));
        }
        if alpha.len() != 10 {
            return Err(HeadlessError::CryptoError(format!(
                "Invalid alpha size: {}",
                alpha.len()
            )));
        }
        let needed = 10 + beta.len();
        if shared_key.len() < needed {
            return Err(HeadlessError::CryptoError(format!(
                "Shared key too short: need {needed}, got {}",
                shared_key.len()
            )));
        }

        let mut iv_struct = vec![0u8; 10 + beta.len()];
        xor_bytes(
            &mut iv_struct[..alpha.len()],
            &shared_key[..alpha.len()],
            alpha,
        );
        xor_bytes(
            &mut iv_struct[10..10 + beta.len()],
            &shared_key[10..10 + beta.len()],
            beta,
        );

        let hash = ts_hash(&iv_struct);
        self.fake_signature.copy_from_slice(&hash[..MAC_LEN]);

        let mut shared_sha1 = [0u8; 20];
        let n = shared_key.len().min(20);
        shared_sha1[..n].copy_from_slice(&shared_key[..n]);

        self.shared_secret = Some(shared_sha1);
        self.iv_struct = Some(iv_struct);
        self.alpha_tmp = None;
        self.crypto_init_complete = true;
        Ok(())
    }

    pub fn process_init1_start(&self) -> Vec<u8> {
        const INIT_VERSION: u32 = 1566914096;
        let mut data = vec![0u8; 21];
        data[0..4].copy_from_slice(&INIT_VERSION.to_be_bytes());
        data[4] = 0x00;
        let unix_now = chrono::Utc::now().timestamp() as u32;
        data[5..9].copy_from_slice(&unix_now.to_be_bytes());
        data[9..13].copy_from_slice(&rand::random::<u32>().to_be_bytes());
        data
    }

    pub fn process_init1_reply(&mut self, packet_data: &[u8]) -> Result<Option<Vec<u8>>> {
        const INIT_VERSION: u32 = 1566914096;

        if packet_data.is_empty() {
            return Err(HeadlessError::ProtocolError("Invalid Init1 packet".into()));
        }

        let server_type = packet_data[0];
        match server_type {
            0x01 => {
                if packet_data.len() == 21 {
                    let mut out = vec![0u8; 25];
                    out[0..4].copy_from_slice(&INIT_VERSION.to_be_bytes());
                    out[4] = 0x02;
                    out[5..25].copy_from_slice(&packet_data[1..21]);
                    Ok(Some(out))
                } else if packet_data.len() == 5 {
                    Err(HeadlessError::ProtocolError("Init1 error response".into()))
                } else {
                    Err(HeadlessError::ProtocolError(
                        "Invalid Init1(1) length".into(),
                    ))
                }
            }
            0x03 => {
                if packet_data.len() != 233 {
                    return Err(HeadlessError::ProtocolError(
                        "Invalid Init1(3) length".into(),
                    ));
                }

                let mut alpha = [0u8; 10];
                fill_random_bytes(&mut alpha);
                self.alpha_tmp = Some(alpha);
                let alpha_b64 = base64::engine::general_purpose::STANDARD.encode(alpha);
                let cmd = format!(
                    "clientinitiv alpha={} omega={} ot=1 ip=",
                    alpha_b64,
                    self.identity.public_key_ts_base64()
                );
                let level = i32::from_be_bytes(
                    packet_data[1 + 64 + 64..1 + 64 + 64 + 4]
                        .try_into()
                        .map_err(|_| {
                            HeadlessError::ProtocolError("Invalid Init1 level bytes".into())
                        })?,
                );
                if !(0..=1_000_000).contains(&level) {
                    return Err(HeadlessError::ProtocolError(format!(
                        "Invalid Init1 RSA level: {level}"
                    )));
                }

                let x = BigUint::from_bytes_be(&packet_data[1..1 + 64]);
                let n = BigUint::from_bytes_be(&packet_data[1 + 64..1 + 64 + 64]);
                let exp = BigUint::from(2u8).pow(level as u32);
                let y = x.modpow(&exp, &n);
                let y_bytes = y.to_bytes_be();

                let cmd_bytes = cmd.as_bytes();
                let mut out = vec![0u8; 4 + 1 + 232 + 64 + cmd_bytes.len()];
                out[0..4].copy_from_slice(&INIT_VERSION.to_be_bytes());
                out[4] = 0x04;
                out[5..5 + 232].copy_from_slice(&packet_data[1..1 + 232]);
                let y_offset = 5 + 232;
                let y_start = y_offset + 64 - y_bytes.len().min(64);
                out[y_start..y_offset + 64]
                    .copy_from_slice(&y_bytes[y_bytes.len().saturating_sub(64)..]);
                out[y_offset + 64..].copy_from_slice(cmd_bytes);

                Ok(Some(out))
            }
            0x7F => Ok(Some(self.process_init1_start())),
            _ => Ok(None),
        }
    }

    pub fn encrypt(&self, packet: &mut Packet, direction: PacketDirection) -> Result<()> {
        if packet.header.packet_type == PacketType::Init1 {
            packet.mac = TS3_INIT_MAC;
            return Ok(());
        }

        if packet
            .header
            .flags
            .contains(super::packet::PacketFlags::UNENCRYPTED)
        {
            packet.mac = self.fake_signature;
            return Ok(());
        }

        let (key, nonce) = self.get_key_nonce(
            direction,
            packet.header.packet_id,
            packet.header.generation_id,
            packet.header.packet_type,
            !self.crypto_init_complete,
        )?;

        let cipher =
            Aes128Eax8::new_from_slice(&key).map_err(|_| HeadlessError::EncryptionFailed)?;
        let nonce = Nonce::from_slice(&nonce);
        let header = packet.header.to_wire_header(direction);
        let header_len = direction.header_len();
        let tag = cipher
            .encrypt_in_place_detached(nonce, &header[..header_len], &mut packet.data)
            .map_err(|_| HeadlessError::EncryptionFailed)?;
        packet.mac.copy_from_slice(tag.as_slice());
        Ok(())
    }

    pub fn decrypt(&self, packet: &mut Packet, direction: PacketDirection) -> Result<bool> {
        if packet.header.packet_type == PacketType::Init1 {
            return Ok(packet.mac == TS3_INIT_MAC);
        }

        if packet
            .header
            .flags
            .contains(super::packet::PacketFlags::UNENCRYPTED)
        {
            return Ok(packet.mac == self.fake_signature);
        }

        let (key, nonce) = self.get_key_nonce(
            direction,
            packet.header.packet_id,
            packet.header.generation_id,
            packet.header.packet_type,
            !self.crypto_init_complete,
        )?;

        let cipher =
            Aes128Eax8::new_from_slice(&key).map_err(|_| HeadlessError::DecryptionFailed)?;
        let nonce = Nonce::from_slice(&nonce);
        let header = packet.header.to_wire_header(direction);
        let header_len = direction.header_len();
        let tag = Tag::<U8>::from_slice(&packet.mac);
        let decrypt_ok = cipher
            .decrypt_in_place_detached(nonce, &header[..header_len], &mut packet.data, tag)
            .map(|_| true);

        if decrypt_ok.is_ok() {
            return Ok(true);
        }

        // TS3AudioBot workaround: clientek/clientinit 并发发送时，Ack(<=2) 可能使用 dummy key
        if packet.header.packet_type == PacketType::Ack && packet.header.packet_id <= 2 {
            let (key2, nonce2) = self.get_key_nonce(
                direction,
                packet.header.packet_id,
                packet.header.generation_id,
                packet.header.packet_type,
                true,
            )?;
            let cipher2 =
                Aes128Eax8::new_from_slice(&key2).map_err(|_| HeadlessError::DecryptionFailed)?;
            let nonce2 = Nonce::from_slice(&nonce2);
            let header2 = packet.header.to_wire_header(direction);
            let tag2 = Tag::<U8>::from_slice(&packet.mac);
            return Ok(cipher2
                .decrypt_in_place_detached(nonce2, &header2[..header_len], &mut packet.data, tag2)
                .is_ok());
        }

        Err(HeadlessError::DecryptionFailed)
    }

    fn get_key_nonce(
        &self,
        direction: PacketDirection,
        packet_id: u16,
        generation_id: u32,
        packet_type: PacketType,
        dummy_encryption: bool,
    ) -> Result<([u8; 16], [u8; 16])> {
        if dummy_encryption || !self.crypto_init_complete {
            let mut key = [0u8; 16];
            let mut nonce = [0u8; 16];
            key.copy_from_slice(b"c:\\windows\\syste");
            nonce.copy_from_slice(b"m\\firewall32.cpl");
            return Ok((key, nonce));
        }

        let iv_struct = self
            .iv_struct
            .as_ref()
            .ok_or_else(|| HeadlessError::CryptoError("IV struct not initialized".into()))?;

        let mut input = Vec::with_capacity(2 + 4 + iv_struct.len());
        let from_server = matches!(direction, PacketDirection::S2C);
        input.push(if from_server {
            DIR_SERVER_TO_CLIENT
        } else {
            DIR_CLIENT_TO_SERVER
        });
        input.push(packet_type as u8);
        input.extend_from_slice(&generation_id.to_be_bytes());
        input.extend_from_slice(iv_struct);

        let hash = ts_hash256(&input);
        let mut key = [0u8; 16];
        let mut nonce = [0u8; 16];
        key.copy_from_slice(&hash[..16]);
        nonce.copy_from_slice(&hash[16..32]);

        key[0] ^= (packet_id >> 8) as u8;
        key[1] ^= (packet_id & 0xFF) as u8;

        Ok((key, nonce))
    }
}

fn verify_ecdsa_sha256_der(server_key: &PublicKey, data: &[u8], proof_der: &[u8]) -> Result<bool> {
    let verify_key = VerifyingKey::from_sec1_bytes(server_key.to_encoded_point(false).as_bytes())
        .map_err(|e| HeadlessError::CryptoError(format!("Invalid verify key: {e}")))?;
    let sig = Signature::from_der(proof_der)
        .map_err(|e| HeadlessError::CryptoError(format!("Invalid ECDSA DER signature: {e}")))?;
    Ok(verify_key.verify(data, &sig).is_ok())
}

#[derive(Debug, Clone)]
struct LicenseBlockParsed {
    key: [u8; 32],
    hash: [u8; 32],
}

fn parse_licenses(data: &[u8]) -> Result<Vec<LicenseBlockParsed>> {
    if data.is_empty() {
        return Err(HeadlessError::CryptoError("License too short".into()));
    }
    if data[0] != 1 {
        return Err(HeadlessError::CryptoError(
            "Unsupported license version".into(),
        ));
    }

    let mut blocks = Vec::new();
    let mut i = 1usize;
    while i < data.len() {
        if data.len() - i < 42 {
            return Err(HeadlessError::CryptoError("License block too short".into()));
        }
        let block = &data[i..];
        if block[0] != 0 {
            return Err(HeadlessError::CryptoError(format!(
                "Wrong key kind {} in license",
                block[0]
            )));
        }

        let block_type = block[33];
        let extra = match block_type {
            0 => {
                let rest = &block[46..];
                let term = rest.iter().position(|b| *b == 0).ok_or_else(|| {
                    HeadlessError::CryptoError(
                        "Non-null-terminated issuer string in intermediate block".into(),
                    )
                })?;
                5 + term
            }
            2 => {
                let rest = &block[47..];
                let term = rest.iter().position(|b| *b == 0).ok_or_else(|| {
                    HeadlessError::CryptoError(
                        "Non-null-terminated issuer string in server block".into(),
                    )
                })?;
                6 + term
            }
            32 => 0,
            _ => {
                return Err(HeadlessError::CryptoError(format!(
                    "Invalid license block type {block_type}"
                )))
            }
        };

        let all_len = 42 + extra;
        if block.len() < all_len {
            return Err(HeadlessError::CryptoError(
                "License block length overflow".into(),
            ));
        }

        let mut key = [0u8; 32];
        key.copy_from_slice(&block[1..33]);

        let mut hash = [0u8; 32];
        let digest = Sha512::digest(&block[1..all_len]);
        hash.copy_from_slice(&digest[..32]);
        hash[0] &= 248;
        hash[31] &= 127;
        hash[31] |= 64;

        blocks.push(LicenseBlockParsed { key, hash });
        i += all_len;
    }

    Ok(blocks)
}

fn derive_license_key(blocks: &[LicenseBlockParsed]) -> Result<[u8; 32]> {
    let mut round = LICENSE_ROOT_KEY;
    for block in blocks {
        round = derive_license_block_key(round, block)?;
    }
    Ok(round)
}

fn derive_license_block_key(parent: [u8; 32], block: &LicenseBlockParsed) -> Result<[u8; 32]> {
    let key_point = -CompressedEdwardsY(block.key)
        .decompress()
        .ok_or_else(|| HeadlessError::CryptoError("Invalid license block public key".into()))?;
    let parent_point = -CompressedEdwardsY(parent)
        .decompress()
        .ok_or_else(|| HeadlessError::CryptoError("Invalid parent license key".into()))?;

    let hash_scalar = Scalar::from_bytes_mod_order(block.hash);
    let out = key_point * hash_scalar + parent_point;
    let mut bytes = out.compress().to_bytes();
    bytes[31] ^= 0x80;
    Ok(bytes)
}

fn get_shared_secret2(public_key: &[u8; 32], private_key: &[u8; 32]) -> Result<[u8; 64]> {
    let mut sk = *private_key;
    sk[31] &= 0x7F;

    let pub_point = -CompressedEdwardsY(*public_key)
        .decompress()
        .ok_or_else(|| HeadlessError::CryptoError("Invalid CryptoInit2 public key".into()))?;

    let shared_point = pub_point * Scalar::from_bytes_mod_order(sk);
    let mut shared = shared_point.compress().to_bytes();
    shared[31] ^= 0x80;

    let mut out = [0u8; 64];
    let digest = Sha512::digest(shared);
    out.copy_from_slice(&digest);
    Ok(out)
}

const LICENSE_ROOT_KEY: [u8; 32] = [
    0xcd, 0x0d, 0xe2, 0xae, 0xd4, 0x63, 0x45, 0x50, 0x9a, 0x7e, 0x3c, 0xfd, 0x8f, 0x68, 0xb3, 0xdc,
    0x75, 0x55, 0xb2, 0x9d, 0xcc, 0xec, 0x73, 0xcd, 0x18, 0x75, 0x0f, 0x99, 0x38, 0x12, 0x40, 0x8a,
];

fn xor_bytes(dst: &mut [u8], a: &[u8], b: &[u8]) {
    for i in 0..dst.len() {
        dst[i] = a[i] ^ b[i];
    }
}

fn write_32_be(dst: &mut [u8], src: &[u8]) {
    let copy_len = src.len().min(32);
    let start = src.len() - copy_len;
    dst[32 - copy_len..].copy_from_slice(&src[start..]);
}

fn fill_random_bytes(buf: &mut [u8]) {
    use p256::elliptic_curve::rand_core::{OsRng, RngCore};
    OsRng.fill_bytes(buf);
}

fn parse_der_len(data: &[u8]) -> Result<(usize, usize)> {
    let first = *data
        .first()
        .ok_or_else(|| HeadlessError::CryptoError("DER length missing".into()))?;
    if first & 0x80 == 0 {
        return Ok((first as usize, 1));
    }

    let octets = (first & 0x7F) as usize;
    if octets == 0 || octets > 4 || data.len() < 1 + octets {
        return Err(HeadlessError::CryptoError("Invalid DER length".into()));
    }

    let mut len = 0usize;
    for b in &data[1..=octets] {
        len = (len << 8) | (*b as usize);
    }
    Ok((len, 1 + octets))
}

#[cfg(test)]
mod tests {
    use super::super::packet::{PacketFlags, PacketType};
    use super::*;

    #[test]
    fn test_init1_mac() {
        let crypto = TsCrypto::new(Identity::generate());
        let mut packet = Packet::new(PacketType::Init1, 101, 0, vec![1]);
        packet.header.flags = PacketFlags::UNENCRYPTED;
        crypto.encrypt(&mut packet, PacketDirection::C2S).unwrap();
        assert_eq!(packet.mac, *b"TS3INIT1");
    }
}
