//! TeamSpeak 无头客户端实现
//!
//! 基于 TS3AudioBot 的 TSLib 实现，提供完整的 TeamSpeak 客户端协议支持。

pub mod adapter;
pub mod connection;
pub mod crypto;
pub mod error;
pub mod identity;
pub mod packet;
pub mod packet_handler;
pub mod reconnect;

pub mod audio;

// 公共 API 重新导出 — 当启用无头特性时由 UnifiedAdapter 使用
pub use adapter::HeadlessAdapter;

pub use audio::AudioConfig;
