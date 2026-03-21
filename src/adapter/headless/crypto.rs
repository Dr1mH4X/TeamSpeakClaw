//! TeamSpeak 加密模块
//!
//! 实现 ECDH 密钥交换和 AES-EAX 加密

use aes::Aes128;
use eax::{AeadInPlace, Eax, KeyInit, Nonce, Tag};
use p256::PublicKey;

use super::{
    error::{HeadlessError, Result},
    identity::{ts_hash256, Identity},
    packet::{Packet, PacketType},
};

/// 包类型数量
const PACKET_TYPE_KINDS: usize = 9;

/// 假签名（用于未加密包）
const FAKE_SIGNATURE: [u8; 8] = [0u8; 8];

/// TeamSpeak 加密处理器
pub struct TsCrypto {
    /// 客户端身份
    identity: Identity,
    /// 共享密钥
    shared_secret: Option<[u8; 32]>,
    /// IV 结构
    iv_struct: Option<Vec<u8>>,
    /// 是否完成加密初始化
    crypto_init_complete: bool,
    /// 按包类型缓存的密钥
    cached_keys: [[u8; 16]; PACKET_TYPE_KINDS],
    /// 按包类型缓存的 IV
    cached_ivs: [[u8; 16]; PACKET_TYPE_KINDS],
    /// 缓存的代数
    cached_generations: [u32; PACKET_TYPE_KINDS],
}

impl Clone for TsCrypto {
    fn clone(&self) -> Self {
        Self {
            identity: self.identity.clone(),
            shared_secret: self.shared_secret,
            iv_struct: self.iv_struct.clone(),
            crypto_init_complete: self.crypto_init_complete,
            cached_keys: self.cached_keys,
            cached_ivs: self.cached_ivs,
            cached_generations: self.cached_generations,
        }
    }
}

impl TsCrypto {
    /// 创建新的加密处理器
    pub fn new(identity: Identity) -> Self {
        Self {
            identity,
            shared_secret: None,
            iv_struct: None,
            crypto_init_complete: false,
            cached_keys: [[0u8; 16]; PACKET_TYPE_KINDS],
            cached_ivs: [[0u8; 16]; PACKET_TYPE_KINDS],
            cached_generations: [0u32; PACKET_TYPE_KINDS],
        }
    }

    /// 获取客户端身份
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// 初始化加密（处理 alpha, beta, omega）
    ///
    /// - alpha: 客户端随机数
    /// - beta: 服务器随机数
    /// - omega: 服务器公钥
    pub fn crypto_init(&mut self, alpha: &[u8], beta: &[u8], omega: &[u8]) -> Result<()> {
        // 导入服务器公钥
        let server_public_key = self.import_server_public_key(omega)?;

        // ECDH 密钥交换
        let shared_secret = self.identity.compute_shared_secret(&server_public_key);
        self.shared_secret = Some(shared_secret);

        // 派生密钥
        self.derive_keys(alpha, beta, &shared_secret)?;

        self.crypto_init_complete = true;
        Ok(())
    }

    /// 导入服务器公钥
    fn import_server_public_key(&self, omega: &[u8]) -> Result<PublicKey> {
        // 尝试解析 DER 编码的公钥
        // 格式可能是：ASN.1 SEQUENCE { BITSTRING { x, y } }

        // 简化解析：假设 omega 是 DER 编码的公钥
        // 实际需要解析 ASN.1 结构

        // 尝试直接解析为未压缩点
        if omega.len() == 65 && omega[0] == 0x04 {
            return PublicKey::from_sec1_bytes(omega)
                .map_err(|e| HeadlessError::CryptoError(format!("Invalid public key: {e}")));
        }

        // 尝试解析 DER 格式
        if let Ok(key) = self.parse_der_public_key(omega) {
            return Ok(key);
        }

        Err(HeadlessError::CryptoError(
            "Could not parse server public key".into(),
        ))
    }

    /// 解析 DER 编码的公钥
    fn parse_der_public_key(&self, data: &[u8]) -> Result<PublicKey> {
        // 简化的 DER 解析
        // 实际应该使用 der crate 完整解析

        // 查找公钥数据（通常是 64 字节的 x + y）
        if data.len() >= 64 {
            // 尝试从末尾提取 64 字节
            let key_data = &data[data.len() - 64..];
            let mut point = vec![0x04]; // 未压缩点标记
            point.extend_from_slice(key_data);

            return PublicKey::from_sec1_bytes(&point)
                .map_err(|e| HeadlessError::CryptoError(format!("DER parse failed: {e}")));
        }

        Err(HeadlessError::CryptoError("DER data too short".into()))
    }

    /// 派生加密密钥
    fn derive_keys(&mut self, alpha: &[u8], beta: &[u8], shared_secret: &[u8; 32]) -> Result<()> {
        if beta.len() != 10 && beta.len() != 54 {
            return Err(HeadlessError::CryptoError(format!(
                "Invalid beta size: {}",
                beta.len()
            )));
        }

        // 准备 IV 结构
        let iv_len = 10 + beta.len();
        let mut iv_struct = vec![0u8; iv_len];

        // XOR shared_secret[0..alpha_len] with alpha -> iv_struct[0..alpha_len]
        xor_bytes(&mut iv_struct, shared_secret, alpha, alpha.len());

        // XOR shared_secret[10..10+beta_len] with beta -> iv_struct[10..10+beta_len]
        let beta_start = 10;
        xor_bytes_range(&mut iv_struct, &shared_secret[10..], beta, beta_start);

        self.iv_struct = Some(iv_struct);

        // 为每个包类型派生密钥
        self.derive_packet_keys()?;

        Ok(())
    }

    /// 为每个包类型派生密钥
    fn derive_packet_keys(&mut self) -> Result<()> {
        let iv_struct = self
            .iv_struct
            .as_ref()
            .ok_or_else(|| HeadlessError::CryptoError("IV struct not initialized".into()))?;

        // 对每个包类型派生唯一的 key 和 nonce
        // 参考 TS3AudioBot: hash = SHA256([direction(1) | packet_type(1) | generation(4) | iv_struct])
        // 前 16 字节为 key，后 16 字节为 nonce
        for i in 0..PACKET_TYPE_KINDS {
            let mut input = Vec::with_capacity(2 + 4 + iv_struct.len());
            input.push(0x31); // direction: client->server
            input.push(i as u8);
            input.extend_from_slice(&0u32.to_be_bytes()); // generation 0
            input.extend_from_slice(iv_struct);

            let hash = ts_hash256(&input);
            self.cached_keys[i] = hash[..16].try_into().unwrap();
            self.cached_ivs[i] = hash[16..32].try_into().unwrap();
        }

        Ok(())
    }

    /// 生成 Init1 包数据
    pub fn process_init1(&self) -> Vec<u8> {
        // Init1 包包含版本信息和客户端公钥
        let mut data = Vec::new();

        // 版本标记 "TS3INIT1"
        data.extend_from_slice(b"TS3INIT1");

        // 协议版本
        data.extend_from_slice(&1566914096u32.to_be_bytes()); // 3.5.0

        // 客户端公钥
        let public_key_bytes = self.identity.public_key_bytes();
        data.push(public_key_bytes.len() as u8);
        data.extend_from_slice(&public_key_bytes);

        data
    }

    /// 加密包
    pub fn encrypt(&self, packet: &mut Packet) -> Result<()> {
        if !self.crypto_init_complete {
            return Ok(());
        }

        if packet.header.packet_type == PacketType::Init1
            || packet.header.packet_type == PacketType::Ping
            || packet.header.packet_type == PacketType::Pong
        {
            // 这些包类型不加密，使用 fake_signature
            packet.mac = FAKE_SIGNATURE;
            return Ok(());
        }

        let packet_type_idx = packet.header.packet_type as usize;
        if packet_type_idx >= PACKET_TYPE_KINDS {
            return Err(HeadlessError::InvalidPacketType(packet_type_idx as u8));
        }

        let key = &self.cached_keys[packet_type_idx];
        let iv = &self.cached_ivs[packet_type_idx];

        // 构造 nonce (14 bytes IV + 2 bytes packet_id)
        let mut nonce_bytes = [0u8; 16];
        nonce_bytes[..14].copy_from_slice(&iv[..14]);
        nonce_bytes[14..].copy_from_slice(&packet.header.packet_id.to_be_bytes());

        // AES-EAX 加密，使用包头作为 AAD
        let cipher =
            Eax::<Aes128>::new_from_slice(key).map_err(|_| HeadlessError::EncryptionFailed)?;
        let nonce = Nonce::from_slice(&nonce_bytes);
        let header_bytes = packet.header.to_bytes();

        let tag = cipher
            .encrypt_in_place_detached(nonce, &header_bytes, &mut packet.data)
            .map_err(|_| HeadlessError::EncryptionFailed)?;

        // EAX 返回 16 字节 tag，TeamSpeak 只用前 8 字节作为 MAC
        packet.mac = tag[..8].try_into().unwrap();

        Ok(())
    }

    /// 解密包
    pub fn decrypt(&self, packet: &mut Packet) -> Result<bool> {
        if !self.crypto_init_complete {
            return Ok(true);
        }

        if packet.header.packet_type == PacketType::Init1
            || packet.header.packet_type == PacketType::Ping
            || packet.header.packet_type == PacketType::Pong
        {
            // 这些包类型不加密，验证 fake_signature
            return Ok(packet.mac == FAKE_SIGNATURE);
        }

        let packet_type_idx = packet.header.packet_type as usize;
        if packet_type_idx >= PACKET_TYPE_KINDS {
            return Ok(false);
        }

        let key = &self.cached_keys[packet_type_idx];
        let iv = &self.cached_ivs[packet_type_idx];

        // 构造 nonce
        let mut nonce_bytes = [0u8; 16];
        nonce_bytes[..14].copy_from_slice(&iv[..14]);
        nonce_bytes[14..].copy_from_slice(&packet.header.packet_id.to_be_bytes());

        // 构造完整 16 字节 tag (前 8 字节来自 MAC，后 8 字节补零)
        let mut tag_bytes = [0u8; 16];
        tag_bytes[..8].copy_from_slice(&packet.mac);

        // AES-EAX 解密，使用包头作为 AAD
        let cipher =
            Eax::<Aes128>::new_from_slice(key).map_err(|_| HeadlessError::DecryptionFailed)?;
        let nonce = Nonce::from_slice(&nonce_bytes);
        let tag = Tag::from_slice(&tag_bytes);
        let header_bytes = packet.header.to_bytes();

        cipher
            .decrypt_in_place_detached(nonce, &header_bytes, &mut packet.data, tag)
            .map(|_| true)
            .map_err(|_| HeadlessError::DecryptionFailed)
    }
}

/// XOR 操作：dst = a ^ b (长度 len)
fn xor_bytes(dst: &mut [u8], a: &[u8], b: &[u8], len: usize) {
    for i in 0..len {
        if i < a.len() && i < b.len() && i < dst.len() {
            dst[i] = a[i] ^ b[i];
        }
    }
}

/// XOR 操作：dst[offset..] = a ^ b
fn xor_bytes_range(dst: &mut [u8], a: &[u8], b: &[u8], offset: usize) {
    for i in 0..b.len() {
        let dst_idx = offset + i;
        if dst_idx < dst.len() && i < a.len() {
            dst[dst_idx] = a[i] ^ b[i];
        }
    }
}

/// 生成随机字节
pub fn generate_random_bytes(len: usize) -> Vec<u8> {
    use p256::elliptic_curve::rand_core::{OsRng, RngCore};
    let mut rng = OsRng;
    let mut bytes = vec![0u8; len];
    rng.fill_bytes(&mut bytes);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crypto_new() {
        let identity = Identity::generate();
        let _crypto = TsCrypto::new(identity);
    }

    #[test]
    fn test_xor_bytes() {
        let mut dst = [0u8; 4];
        let a = [0xFF, 0x00, 0xFF, 0x00];
        let b = [0x00, 0xFF, 0x00, 0xFF];
        xor_bytes(&mut dst, &a, &b, 4);
        assert_eq!(dst, [0xFF, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn test_generate_random_bytes() {
        let bytes = generate_random_bytes(32);
        assert_eq!(bytes.len(), 32);
    }
}
