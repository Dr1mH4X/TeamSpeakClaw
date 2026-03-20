//! 无头客户端错误类型

use thiserror::Error;

pub type Result<T> = std::result::Result<T, HeadlessError>;

#[derive(Error, Debug)]
pub enum HeadlessError {
    #[error("Crypto error: {0}")]
    CryptoError(String),

    #[error("Invalid key format: {0}")]
    InvalidKey(String),

    #[error("Connection error: {0}")]
    ConnectionError(String),

    #[error("Protocol error: {0}")]
    ProtocolError(String),

    #[error("Packet error: {0}")]
    PacketError(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Base64 decode error: {0}")]
    Base64Error(#[from] base64::DecodeError),

    #[error("Invalid packet type: {0}")]
    InvalidPacketType(u8),

    #[error("Encryption failed")]
    EncryptionFailed,

    #[error("Decryption failed")]
    DecryptionFailed,

    #[error("Invalid MAC")]
    InvalidMac,

    #[error("Connection closed")]
    ConnectionClosed,

    #[error("Timeout")]
    Timeout,

    #[error("Not connected")]
    NotConnected,

    #[cfg(feature = "audio")]
    #[error("Audio error: {0}")]
    AudioError(String),

    #[error("Internal error: {0}")]
    InternalError(String),
}
