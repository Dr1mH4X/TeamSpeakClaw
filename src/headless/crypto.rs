//! TeamSpeak 加密模块
//!
//! 实现 ECDH 密钥交换和 AES-EAX 加密

use aes::Aes128;
use eax::{aead::Aead, AeadInPlace, Eax, KeyInit, Nonce, Tag};
use p256::PublicKey;

use crate::headless::{
    error::{HeadlessError, Result},
    identity::{ts_hash, Identity},
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

    /// 重置加密状态
    pub fn reset(&mut self) {
        self.crypto_init_complete = false;
        self.shared_secret = None;
        self.iv_struct = None;
        self.cached_keys = [[0u8; 16]; PACKET_TYPE_KINDS];
        self.cached_ivs = [[0u8; 16]; PACKET_TYPE_KINDS];
        self.cached_generations = [0u32; PACKET_TYPE_KINDS];
    }

    /// 获取客户端身份
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// 是否完成加密初始化
    pub fn is_initialized(&self) -> bool {
        self.crypto_init_complete
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

        // XOR 共享密钥和 alpha
        xor_bytes(&mut iv_struct, shared_secret, alpha, alpha.len());

        // XOR 共享密钥和 beta
        let beta_start = alpha.len();
        xor_bytes_range(&mut iv_struct, shared_secret, beta, beta_start);

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

        let half = iv_struct.len() / 2;

        for i in 0..PACKET_TYPE_KINDS {
            // 计算密钥
            let key_input = &iv_struct[..half];
            let key_hash = ts_hash(key_input);
            self.cached_keys[i] = key_hash[..16].try_into().unwrap();

            // 计算 IV
            let iv_input = &iv_struct[half..];
            let iv_hash = ts_hash(iv_input);
            self.cached_ivs[i] = iv_hash[..16].try_into().unwrap();

            // 通过改变输入来使每个包类型不同
            if i < PACKET_TYPE_KINDS - 1 {
                // 简单的差异化处理
                self.cached_keys[i][0] ^= i as u8;
                self.cached_ivs[i][0] ^= i as u8;
            }
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

    /// 生成 clientinitiv 命令
    pub fn create_client_init_iv(&self) -> Result<String> {
        let public_key_base64 = self.identity.public_key_base64();

        Ok(format!(
            "clientinitiv alpha={public_key_base64} omega={public_key_base64} ip="
        ))
    }

    /// 生成 clientinit 命令
    pub fn create_client_init(&self, nickname: &str) -> String {
        format!(
            "clientinit client_nickname={nickname} client_version=3.5.0 client_platform=Linux client_input_hardware=1 client_output_hardware=1 client_default_channel client_meta_data client_version_sign= client_key_offset=0 client_nickname_phonetic client_default_token= client_badges"
        )
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
            // 这些包类型不加密
            packet.mac = FAKE_SIGNATURE;
            return Ok(());
        }

        let packet_type_idx = packet.header.packet_type as usize;
        if packet_type_idx >= PACKET_TYPE_KINDS {
            return Err(HeadlessError::InvalidPacketType(packet_type_idx as u8));
        }

        let key = &self.cached_keys[packet_type_idx];
        let iv = &self.cached_ivs[packet_type_idx];

        // 构造 nonce
        let mut nonce_bytes = [0u8; 16];
        nonce_bytes[..14].copy_from_slice(&iv[..14]);
        nonce_bytes[14..].copy_from_slice(&packet.header.packet_id.to_be_bytes());

        // AES-EAX 加密
        let cipher =
            Eax::<Aes128>::new_from_slice(key).map_err(|_| HeadlessError::EncryptionFailed)?;
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = cipher
            .encrypt(nonce, packet.data.as_ref())
            .map_err(|_| HeadlessError::EncryptionFailed)?;

        // EAX 返回 Vec<u8>，直接使用
        packet.data = ciphertext;
        // MAC 从 tag 中提取（需要单独计算）
        packet.mac = [0u8; 8]; // 临时设置，后续需要正确处理 tag

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
            // 这些包类型不加密
            return Ok(true);
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

        // 构造 tag
        let mut tag_bytes = [0u8; 16];
        tag_bytes[..8].copy_from_slice(&packet.mac);

        // AES-EAX 解密
        let cipher =
            Eax::<Aes128>::new_from_slice(key).map_err(|_| HeadlessError::DecryptionFailed)?;
        let nonce = Nonce::from_slice(&nonce_bytes);
        let tag = Tag::from_slice(&tag_bytes);

        cipher
            .decrypt_in_place_detached(nonce, b"", &mut packet.data, tag)
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
        let crypto = TsCrypto::new(identity);
        assert!(!crypto.is_initialized());
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
