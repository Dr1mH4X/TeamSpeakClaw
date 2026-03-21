//! TeamSpeak 身份管理
//!
//! 当前阶段先保证连接握手可用：
//! - 兼容 identity 存储格式（`{level}V{base64(private32)}`）
//! - 提供 TS 握手所需 DER 公钥导出
//! - 共享密钥派生与安全等级计算对齐 TS3AudioBot

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use p256::ecdsa::{signature::Signer, Signature, SigningKey};
use p256::{ecdh::diffie_hellman, elliptic_curve::sec1::ToEncodedPoint, PublicKey, SecretKey};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use std::fmt;

use super::error::{HeadlessError, Result};

#[derive(Clone)]
pub struct Identity {
    private_key: SecretKey,
    public_key: PublicKey,
    pub key_offset: u64,
    uid: String,
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity")
            .field("key_offset", &self.key_offset)
            .field("uid", &self.uid)
            .finish()
    }
}

impl Identity {
    pub fn generate() -> Self {
        let private_key = SecretKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
        let public_key = private_key.public_key();
        let public_key_b64 = Self::encode_public_key_ts_base64(&public_key);
        let uid = Self::compute_uid_from_public_key_string(&public_key_b64);
        Self {
            private_key,
            public_key,
            key_offset: 0,
            uid,
        }
    }

    /// 兼容已有 key 格式：`{level}V{base64(private32)}`
    pub fn from_teamspeak_key(key: &str) -> Result<Self> {
        let parts: Vec<&str> = key.splitn(2, 'V').collect();
        if parts.len() != 2 {
            return Err(HeadlessError::InvalidKey(
                "Invalid TeamSpeak key format, expected '{level}V{base64}'".into(),
            ));
        }

        let level: u64 = parts[0]
            .parse()
            .map_err(|_| HeadlessError::InvalidKey("Invalid key level".into()))?;

        let decoded = BASE64
            .decode(parts[1])
            .map_err(|e| HeadlessError::InvalidKey(format!("Base64 decode failed: {e}")))?;
        if decoded.len() < 32 {
            return Err(HeadlessError::InvalidKey(
                "Private key bytes too short".into(),
            ));
        }

        let mut sk = [0u8; 32];
        sk.copy_from_slice(&decoded[decoded.len() - 32..]);
        let private_key = SecretKey::from_slice(&sk)
            .map_err(|e| HeadlessError::InvalidKey(format!("Invalid private key: {e}")))?;
        let public_key = private_key.public_key();
        let public_key_b64 = Self::encode_public_key_ts_base64(&public_key);
        let uid = Self::compute_uid_from_public_key_string(&public_key_b64);

        Ok(Self {
            private_key,
            public_key,
            key_offset: level,
            uid,
        })
    }

    /// 与现有存储保持一致
    pub fn to_teamspeak_key(&self) -> String {
        let private_bytes = self.private_key.to_bytes();
        format!("{}V{}", self.key_offset, BASE64.encode(private_bytes))
    }

    /// 握手里 `omega` 字段：对齐 TS3AudioBot 的 DER 公钥导出格式
    pub fn public_key_ts_base64(&self) -> String {
        Self::encode_public_key_ts_base64(&self.public_key)
    }

    pub fn uid(&self) -> &str {
        &self.uid
    }

    pub fn security_level(&self) -> u8 {
        let public_key_b64 = self.public_key_ts_base64();
        security_level_for_offset(&public_key_b64, self.key_offset)
    }

    /// 将 key_offset 提升到至少指定安全等级（与 TS3AudioBot ImproveSecurity 等价）
    pub fn ensure_security_level(&mut self, required_level: u8) -> u8 {
        let public_key_b64 = self.public_key_ts_base64();

        let mut best_offset = self.key_offset;
        let mut best_level = security_level_for_offset(&public_key_b64, best_offset);
        if best_level >= required_level {
            return best_level;
        }

        let mut offset = self.key_offset;
        loop {
            let level = security_level_for_offset(&public_key_b64, offset);
            if level > best_level {
                best_level = level;
                best_offset = offset;
                if best_level >= required_level {
                    self.key_offset = best_offset;
                    return best_level;
                }
            }

            match offset.checked_add(1) {
                Some(next) => offset = next,
                None => {
                    self.key_offset = best_offset;
                    return best_level;
                }
            }
        }
    }

    /// 对齐 TS3AudioBot：shared_x -> SHA1(20)
    pub fn compute_shared_secret_sha1(&self, server_public_key: &PublicKey) -> [u8; 20] {
        let shared = diffie_hellman(
            self.private_key.to_nonzero_scalar(),
            server_public_key.as_affine(),
        );

        let raw_secret = shared.raw_secret_bytes();
        if raw_secret.len() == 32 {
            ts_hash(raw_secret)
        } else if raw_secret.len() > 32 {
            ts_hash(&raw_secret[raw_secret.len() - 32..])
        } else {
            let mut padded = [0u8; 32];
            padded[32 - raw_secret.len()..].copy_from_slice(raw_secret);
            ts_hash(&padded)
        }
    }

    pub fn sign_ecdsa_sha256_der(&self, data: &[u8]) -> Result<Vec<u8>> {
        let key_bytes = self.private_key.to_bytes();
        let signing_key = SigningKey::from_bytes(&key_bytes)
            .map_err(|e| HeadlessError::CryptoError(format!("Invalid ECDSA private key: {e}")))?;
        let sig: Signature = signing_key.sign(data);
        Ok(sig.to_der().as_bytes().to_vec())
    }

    fn compute_uid_from_public_key_string(public_key_b64: &str) -> String {
        BASE64.encode(ts_hash(public_key_b64.as_bytes()))
    }

    fn encode_public_key_ts_base64(public_key: &PublicKey) -> String {
        let point = public_key.to_encoded_point(false);
        let x = point
            .x()
            .expect("uncompressed P-256 point should contain x coordinate");
        let y = point
            .y()
            .expect("uncompressed P-256 point should contain y coordinate");

        // TS3AudioBot ExportPublicKey:
        // SEQUENCE {
        //   BIT STRING (flags=0x00, pad bits=7),
        //   INTEGER 32,
        //   INTEGER x,
        //   INTEGER y
        // }
        let bit_string = vec![0x03, 0x02, 0x07, 0x00];
        let key_size = vec![0x02, 0x01, 0x20];
        let x_int = der_encode_integer_positive(x);
        let y_int = der_encode_integer_positive(y);

        let mut seq_data =
            Vec::with_capacity(bit_string.len() + key_size.len() + x_int.len() + y_int.len());
        seq_data.extend_from_slice(&bit_string);
        seq_data.extend_from_slice(&key_size);
        seq_data.extend_from_slice(&x_int);
        seq_data.extend_from_slice(&y_int);

        let mut out = Vec::with_capacity(2 + seq_data.len());
        out.push(0x30);
        der_write_len(&mut out, seq_data.len());
        out.extend_from_slice(&seq_data);
        BASE64.encode(out)
    }
}

pub fn ts_hash(data: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(data);
    hasher.finalize().into()
}

pub fn ts_hash256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

fn der_write_len(out: &mut Vec<u8>, len: usize) {
    if len < 0x80 {
        out.push(len as u8);
        return;
    }

    let mut buf = [0u8; 8];
    let mut n = len;
    let mut idx = buf.len();
    while n > 0 {
        idx -= 1;
        buf[idx] = (n & 0xFF) as u8;
        n >>= 8;
    }
    let octets = buf.len() - idx;
    out.push(0x80 | (octets as u8));
    out.extend_from_slice(&buf[idx..]);
}

fn der_encode_integer_positive(raw_be: &[u8]) -> Vec<u8> {
    let first_non_zero = raw_be
        .iter()
        .position(|b| *b != 0)
        .unwrap_or(raw_be.len().saturating_sub(1));
    let mut val = raw_be[first_non_zero..].to_vec();
    if val.is_empty() {
        val.push(0);
    }
    if val[0] & 0x80 != 0 {
        val.insert(0, 0);
    }

    let mut out = Vec::with_capacity(2 + val.len());
    out.push(0x02);
    der_write_len(&mut out, val.len());
    out.extend_from_slice(&val);
    out
}

fn security_level_for_offset(public_key_b64: &str, offset: u64) -> u8 {
    let mut hash_buffer = Vec::with_capacity(public_key_b64.len() + 20);
    hash_buffer.extend_from_slice(public_key_b64.as_bytes());
    hash_buffer.extend_from_slice(offset.to_string().as_bytes());
    let hash = ts_hash(&hash_buffer);
    leading_zero_bits_ts(&hash)
}

fn leading_zero_bits_ts(hash: &[u8]) -> u8 {
    let mut count: u8 = 0;
    for &b in hash {
        if b == 0 {
            count = count.saturating_add(8);
            continue;
        }

        let mut mask = 1u8;
        while mask != 0 && (b & mask) == 0 {
            count = count.saturating_add(1);
            mask <<= 1;
        }
        break;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identity_export_import() {
        let identity = Identity::generate();
        let key = identity.to_teamspeak_key();
        let loaded = Identity::from_teamspeak_key(&key).unwrap();
        assert!(!loaded.uid().is_empty());
    }
}
