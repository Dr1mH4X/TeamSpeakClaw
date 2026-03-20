//! TeamSpeak 无头客户端实现
//! 
//! 基于 TS3AudioBot 的 TSLib 实现，提供完整的 TeamSpeak 客户端协议支持。

#![allow(dead_code)]
#![allow(unused_imports)]

pub mod connection;
pub mod crypto;
pub mod error;
pub mod identity;
pub mod packet;
pub mod packet_handler;
pub mod reconnect;

#[cfg(feature = "audio")]
pub mod audio;

pub use connection::{Connection, ConnectionConfig, ConnectionEvent, ConnectionState};
pub use crypto::TsCrypto;
pub use error::{HeadlessError, Result};
pub use identity::Identity;
pub use packet::{Packet, PacketFlags, PacketType};
pub use packet_handler::PacketHandler;
pub use reconnect::{AutoReconnectConnection, ReconnectConfig, ReconnectEvent, ReconnectManager};

#[cfg(feature = "audio")]
pub use audio::{AudioConfig, AudioError, AudioFrame, AudioProcessor, AudioSender, AudioReceiver, FfmpegEncoder, FfmpegDecoder};
