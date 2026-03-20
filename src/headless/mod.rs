//! TeamSpeak 无头客户端实现
//!
//! 基于 TS3AudioBot 的 TSLib 实现，提供完整的 TeamSpeak 客户端协议支持。

pub mod connection;
pub mod crypto;
pub mod error;
pub mod identity;
pub mod packet;
pub mod packet_handler;
pub mod reconnect;

#[cfg(feature = "audio")]
pub mod audio;

// 公共 API 重新导出 — 当启用无头特性时由 UnifiedAdapter 使用
pub use connection::{Connection, ConnectionConfig, ConnectionEvent, ConnectionState};
pub use crypto::TsCrypto;
pub use error::{HeadlessError, Result};
pub use identity::Identity;
pub use packet::{Packet, PacketFlags, PacketType};
pub use packet_handler::PacketHandler;
pub use reconnect::{AutoReconnectConnection, ReconnectConfig, ReconnectEvent, ReconnectManager};

#[cfg(feature = "audio")]
pub use audio::{
    AudioConfig, AudioError, AudioFrame, AudioProcessor, AudioReceiver, AudioSender,
    FfmpegDecoder, FfmpegEncoder,
};
