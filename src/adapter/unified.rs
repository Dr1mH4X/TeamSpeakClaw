//! 统一适配器接口
//!
//! 提供 ServerQuery 和无头客户端的统一接口

use crate::{adapter::TsEvent, error::Result};
use std::sync::Arc;
use tokio::sync::broadcast;

/// 统一适配器枚举
#[derive(Clone)]
pub enum UnifiedAdapter {
    /// ServerQuery 适配器
    ServerQuery(Arc<crate::adapter::TsAdapter>),
    /// 无头客户端适配器
    Headless(Arc<crate::adapter::HeadlessAdapter>),
}

impl UnifiedAdapter {
    /// 连接到 TeamSpeak 服务器
    pub async fn connect(config: Arc<arc_swap::ArcSwap<crate::config::AppConfig>>) -> Result<Self> {
        {
            let cfg = config.load();
            if cfg.teamspeak.connection_mode == "headless" {
                let adapter = crate::adapter::HeadlessAdapter::connect(config).await?;
                return Ok(Self::Headless(adapter));
            }
        }

        // 默认使用 ServerQuery
        let adapter = crate::adapter::TsAdapter::connect(config).await?;
        Ok(Self::ServerQuery(adapter))
    }

    /// 设置昵称
    pub async fn set_nickname(&self, nick: &str) -> Result<()> {
        match self {
            Self::ServerQuery(adapter) => adapter.set_nickname(nick).await,
            Self::Headless(adapter) => adapter.set_nickname(nick).await,
        }
    }

    /// 退出
    pub async fn quit(&self) -> Result<()> {
        match self {
            Self::ServerQuery(adapter) => adapter.quit().await,
            Self::Headless(adapter) => adapter.quit().await,
        }
    }

    /// 发送原始命令
    /// 发送消息
    pub async fn send_message(&self, target_mode: u8, target: u32, message: &str) -> Result<()> {
        match self {
            Self::ServerQuery(adapter) => adapter.send_message(target_mode, target, message).await,
            Self::Headless(adapter) => adapter.send_message(target_mode, target, message).await,
        }
    }

    /// 发送原始命令
    pub async fn send_raw(&self, cmd: &str) -> Result<()> {
        match self {
            Self::ServerQuery(adapter) => adapter.send_raw(cmd).await,
            Self::Headless(adapter) => adapter.send_raw(cmd).await,
        }
    }

    /// 播放音频
    #[allow(dead_code)]
    pub async fn play_audio(&self, source: &str) -> Result<()> {
        match self {
            Self::ServerQuery(_) => Err(crate::error::AppError::TsError {
                code: 0,
                message: "Audio not supported in ServerQuery mode".into(),
            }),
            Self::Headless(adapter) => adapter.play_audio(source).await,
        }
    }

    /// 停止音频
    #[allow(dead_code)]
    pub async fn stop_audio(&self) -> Result<()> {
        match self {
            Self::ServerQuery(_) => Err(crate::error::AppError::TsError {
                code: 0,
                message: "Audio not supported in ServerQuery mode".into(),
            }),
            Self::Headless(adapter) => adapter.stop_audio().await,
        }
    }

    /// 订阅事件
    pub fn subscribe(&self) -> broadcast::Receiver<TsEvent> {
        match self {
            Self::ServerQuery(adapter) => adapter.subscribe(),
            Self::Headless(adapter) => adapter.subscribe(),
        }
    }

    /// 获取机器人客户端 ID
    pub fn get_bot_clid(&self) -> u32 {
        match self {
            Self::ServerQuery(adapter) => adapter.get_bot_clid(),
            Self::Headless(adapter) => adapter.get_bot_clid(),
        }
    }
}
