//! TeamSpeak 无头客户端实现
//!
//! 基于 TS3AudioBot 的 TSLib 实现，提供完整的 TeamSpeak 客户端协议支持。

// 模块尚未完全集成，pub use 导出和内部 API 暂时未被外部引用


pub mod connection;
pub mod crypto;
pub mod error;
pub mod identity;
pub mod packet;
pub mod packet_handler;
pub mod reconnect;

#[cfg(feature = "audio")]
pub mod audio;

// Public API re-exports — consumed by UnifiedAdapter when headless feature is enabled
#[allow(unused_imports)]
pub use connection::{Connection, ConnectionConfig, ConnectionEvent, ConnectionState};
#[allow(unused_imports)]
pub use crypto::TsCrypto;
#[allow(unused_imports)]
pub use error::{HeadlessError, Result};
#[allow(unused_imports)]
pub use identity::Identity;
#[allow(unused_imports)]
pub use packet::{Packet, PacketFlags, PacketType};
#[allow(unused_imports)]
pub use packet_handler::PacketHandler;
#[allow(unused_imports)]
pub use reconnect::{AutoReconnectConnection, ReconnectConfig, ReconnectEvent, ReconnectManager};

#[cfg(feature = "audio")]
#[allow(unused_imports)]
pub use audio::{
    AudioConfig, AudioError, AudioFrame, AudioProcessor, AudioReceiver, AudioSender,
    FfmpegDecoder, FfmpegEncoder,
};
