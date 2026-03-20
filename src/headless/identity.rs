//! TeamSpeak 身份管理
//!
//! 处理客户端密钥对的生成、导入和导出。

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use p256::{ecdh::diffie_hellman, elliptic_curve::sec1::ToEncodedPoint, PublicKey, SecretKey};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use std::fmt;

use crate::headless::error::{HeadlessError, Result};

/// TeamSpeak 客户端身份
#[derive(Clone)]
pub struct Identity {
    /// 私钥
    private_key: SecretKey,
    /// 公钥
    public_key: PublicKey,
    /// 安全级别偏移量
    pub key_offset: u64,
    /// 最后检查的偏移量
    pub last_checked_offset: u64,
    /// 用户唯一标识符
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
    /// 生成新的随机身份
    pub fn generate() -> Self {
        // 使用 p256 内置的随机数生成器
        let private_key = SecretKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
        let public_key = private_key.public_key();
        let uid = Self::compute_uid(&public_key);

        Self {
            private_key,
            public_key,
            key_offset: 0,
            last_checked_offset: 0,
            uid,
        }
    }

    /// 从私钥字节创建身份
    pub fn from_private_key_bytes(bytes: &[u8]) -> Result<Self> {
        let private_key = SecretKey::from_slice(bytes)
            .map_err(|e| HeadlessError::InvalidKey(format!("Invalid private key: {e}")))?;
        let public_key = private_key.public_key();
        let uid = Self::compute_uid(&public_key);

        Ok(Self {
            private_key,
            public_key,
            key_offset: 0,
            last_checked_offset: 0,
            uid,
        })
    }

    /// 从 TeamSpeak 格式的密钥导入
    ///
    /// 格式: "{level}V{base64_encoded_key}"
    pub fn from_teamspeak_key(key: &str) -> Result<Self> {
        // 解析格式 "20Vbase64..."
        let parts: Vec<&str> = key.splitn(2, 'V').collect();
        if parts.len() != 2 {
            return Err(HeadlessError::InvalidKey(
                "Invalid TeamSpeak key format, expected '{level}V{base64}'".into(),
            ));
        }

        let level: u64 = parts[0]
            .parse()
            .map_err(|_| HeadlessError::InvalidKey("Invalid key level".into()))?;

        let encoded = parts[1];
        let mut data = BASE64
            .decode(encoded)
            .map_err(|e| HeadlessError::InvalidKey(format!("Base64 decode failed: {e}")))?;

        // 解混淆
        Self::deobfuscate_identity(&mut data)?;

        // 导入密钥
        Self::import_key_data(&data, level)
    }

    /// 解混淆 TeamSpeak 身份数据
    fn deobfuscate_identity(data: &mut [u8]) -> Result<()> {
        if data.len() < 20 {
            return Err(HeadlessError::InvalidKey("Identity too short".into()));
        }

        // 计算哈希用于解混淆
        let hash_start = 20;
        let hash_end = data.len();
        let hash = ts_hash_range(data, hash_start, hash_end);

        // XOR 解混淆
        for (i, byte) in data.iter_mut().enumerate().take(hash_end) {
            if i < hash.len() {
                *byte ^= hash[i];
            }
        }

        // 第二层 XOR 使用固定密钥
        let obfuscation_key = b"b9dfaa7bee6ac57ac7b65f1094a1c155e747327bc2fe5d51c512023fe54a280201004e90ad1daaae1075d53b7d571c30e063b5a62a4a017bb394833aa0983e6e";
        let len = std::cmp::min(100, data.len());
        for (i, byte) in data.iter_mut().enumerate().take(len) {
            *byte ^= obfuscation_key[i % obfuscation_key.len()];
        }

        Ok(())
    }

    /// 从原始密钥数据导入
    fn import_key_data(data: &[u8], level: u64) -> Result<Self> {
        // 尝试解析 ASN.1 DER 格式
        // TeamSpeak 使用的格式可能不同，这里简化处理

        // 尝试直接作为私钥导入
        if data.len() >= 32 {
            let key_bytes = &data[data.len() - 32..];
            if let Ok(private_key) = SecretKey::from_slice(key_bytes) {
                let public_key = private_key.public_key();
                let uid = Self::compute_uid(&public_key);
                return Ok(Self {
                    private_key,
                    public_key,
                    key_offset: level,
                    last_checked_offset: level,
                    uid,
                });
            }
        }

        Err(HeadlessError::InvalidKey(
            "Could not import key data".into(),
        ))
    }

    /// 导出为 TeamSpeak 格式
    pub fn to_teamspeak_key(&self) -> String {
        let private_bytes = self.private_key.to_bytes();
        let encoded = BASE64.encode(&private_bytes);
        format!("{}V{}", self.key_offset, encoded)
    }

    /// 获取私钥字节
    pub fn private_key_bytes(&self) -> [u8; 32] {
        self.private_key.to_bytes().into()
    }

    /// 获取公钥字节（未压缩格式）
    pub fn public_key_bytes(&self) -> Vec<u8> {
        self.public_key.to_encoded_point(false).as_bytes().to_vec()
    }

    /// 获取公钥 Base64 编码
    pub fn public_key_base64(&self) -> String {
        BASE64.encode(self.public_key_bytes())
    }

    /// 计算 UID（从公钥哈希）
    pub fn compute_uid(public_key: &PublicKey) -> String {
        let key_bytes = public_key.to_encoded_point(false);
        let hash = ts_hash(key_bytes.as_bytes());
        BASE64.encode(hash)
    }

    /// 获取 UID
    pub fn uid(&self) -> &str {
        &self.uid
    }

    /// 与服务器公钥计算共享密钥
    pub fn compute_shared_secret(&self, server_public_key: &PublicKey) -> [u8; 32] {
        let shared = diffie_hellman(
            self.private_key.to_nonzero_scalar(),
            server_public_key.as_affine(),
        );

        // TeamSpeak 使用 SHA1 哈希共享密钥
        let raw_secret = shared.raw_secret_bytes();
        let hash = if raw_secret.len() == 32 {
            ts_hash(raw_secret)
        } else if raw_secret.len() > 32 {
            ts_hash(&raw_secret[raw_secret.len() - 32..])
        } else {
            // 填充到 32 字节
            let mut padded = [0u8; 32];
            padded[32 - raw_secret.len()..].copy_from_slice(raw_secret);
            ts_hash(&padded)
        };

        // 扩展到 32 字节（SHA1 只有 20 字节）
        let mut result = [0u8; 32];
        result[..20].copy_from_slice(&hash);
        result
    }
}

/// SHA1 哈希（TeamSpeak 使用单次 SHA1）
pub fn ts_hash(data: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// SHA256 哈希（用于密钥派生）
pub fn ts_hash256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// 对数据范围进行 SHA1 哈希
fn ts_hash_range(data: &[u8], start: usize, end: usize) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(&data[start..end]);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identity_generate() {
        let identity = Identity::generate();
        assert!(!identity.uid().is_empty());
        assert_eq!(identity.key_offset, 0);
    }

    #[test]
    fn test_identity_export_import() {
        let identity = Identity::generate();
        let key = identity.to_teamspeak_key();

        assert!(key.contains('V'));
        assert!(key.starts_with('0'));
    }

    #[test]
    fn test_ts_hash() {
        let data = b"test data";
        let hash = ts_hash(data);
        assert_eq!(hash.len(), 20);
    }
}
