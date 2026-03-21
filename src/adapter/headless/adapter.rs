//! 无头客户端适配器
//!
//! 将无头客户端集成到现有架构

use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{debug, error, info};

use crate::{
    adapter::serverquery::event::{
        ClientEnterEvent, ClientLeftEvent, TextMessageEvent, TextMessageTarget, TsEvent,
    },
    config::AppConfig,
    error::{AppError, Result},
};

use super::{
    connection::{ConnectionConfig, ConnectionEvent},
    identity::Identity,
    reconnect::{AutoReconnectConnection, ReconnectConfig, ReconnectEvent},
    AudioConfig,
};

/// 无头客户端适配器
pub struct HeadlessAdapter {
    /// 连接
    connection: Arc<AutoReconnectConnection>,
    /// 事件发送器
    event_tx: broadcast::Sender<TsEvent>,
    /// 客户端 ID
    bot_clid: std::sync::atomic::AtomicU32,
}

impl Clone for HeadlessAdapter {
    fn clone(&self) -> Self {
        Self {
            connection: self.connection.clone(),
            event_tx: self.event_tx.clone(),
            bot_clid: std::sync::atomic::AtomicU32::new(
                self.bot_clid.load(std::sync::atomic::Ordering::Relaxed),
            ),
        }
    }
}

impl HeadlessAdapter {
    /// 创建并连接无头客户端适配器
    pub async fn connect(config: Arc<arc_swap::ArcSwap<AppConfig>>) -> Result<Arc<Self>> {
        let cfg = config.load();

        info!("Connecting to TeamSpeak using headless client mode");

        // 加载或生成身份
        let identity = Self::load_or_create_identity(&cfg.teamspeak.headless.identity_path)?;

        // 解析服务器地址
        let server_addr: std::net::SocketAddr = cfg
            .teamspeak
            .headless
            .server_address
            .parse()
            .map_err(|e| AppError::ConfigError(format!("Invalid server address: {e}")))?;

        // 创建连接配置
        let connection_config = ConnectionConfig {
            server_addr,
            nickname: cfg.teamspeak.bot_nickname.clone(),
            identity,
            connect_timeout: std::time::Duration::from_secs(
                cfg.teamspeak.headless.connect_timeout_secs,
            ),
            audio: Some(AudioConfig::default()), // TODO: Load from config
        };

        let reconnect_config = ReconnectConfig {
            max_retries: cfg.teamspeak.reconnect_max_retries,
            initial_delay_ms: cfg.teamspeak.reconnect_base_delay_ms,
            max_delay_ms: 30000,
            backoff_multiplier: 2.0,
            enabled: true,
        };

        // 创建连接
        let (connection, mut reconnect_rx) =
            AutoReconnectConnection::new(connection_config, reconnect_config)
                .await
                .map_err(|e| AppError::TsError {
                    code: 0,
                    message: e.to_string(),
                })?;

        let connection = Arc::new(connection);
        let (event_tx, _) = broadcast::channel::<TsEvent>(256);

        let adapter = Arc::new(Self {
            connection: connection.clone(),
            event_tx: event_tx.clone(),
            bot_clid: std::sync::atomic::AtomicU32::new(0),
        });

        // 启动事件处理任务
        // let event_tx_clone = event_tx.clone();
        tokio::spawn(async move {
            while let Some(event) = reconnect_rx.recv().await {
                match event {
                    ReconnectEvent::Connected => info!("Headless client connected"),
                    ReconnectEvent::Reconnected => info!("Headless client reconnected"),
                    ReconnectEvent::Disconnected(reason) => {
                        info!("Headless client disconnected: {:?}", reason)
                    }
                    ReconnectEvent::ReconnectFailed(e) => {
                        error!("Headless client reconnect failed: {}", e)
                    }
                    _ => {}
                }
            }
        });

        // 连接到服务器
        connection.connect().await.map_err(|e| AppError::TsError {
            code: 0,
            message: e.to_string(),
        })?;

        info!("Headless client initialized");

        Ok(adapter)
    }

    /// 加载或创建身份
    fn load_or_create_identity(path: &str) -> Result<Identity> {
        let path = std::path::Path::new(path);

        if path.exists() {
            // 加载现有身份
            let content = std::fs::read_to_string(path)?;
            let key: serde_json::Value = serde_json::from_str(&content)?;

            if let Some(key_str) = key.get("key").and_then(|v| v.as_str()) {
                return Identity::from_teamspeak_key(key_str)
                    .map_err(|e| AppError::ConfigError(format!("Failed to load identity: {e}")));
            }
        }

        // 生成新身份
        info!("Generating new identity");
        let identity = Identity::generate();

        // 保存身份
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let key_data = serde_json::json!({
            "key": identity.to_teamspeak_key(),
            "uid": identity.uid(),
        });

        std::fs::write(path, serde_json::to_string_pretty(&key_data)?)?;
        info!("Identity saved to {:?}", path);

        Ok(identity)
    }

    /// 处理连接事件
    #[allow(dead_code)]
    async fn handle_connection_event(
        event: ConnectionEvent,
        event_tx: &broadcast::Sender<TsEvent>,
    ) {
        match event {
            ConnectionEvent::StateChanged(state) => {
                debug!("Connection state changed: {:?}", state);
            }
            ConnectionEvent::Notification(data) => {
                // 解析通知并转换为 TsEvent
                let events = Self::parse_notification(&data);
                for ts_event in events {
                    if let Err(e) = event_tx.send(ts_event) {
                        debug!("No subscribers for event: {e}");
                    }
                }
            }
            ConnectionEvent::CommandResponse(data) => {
                debug!("Command response: {}", data);
            }
            ConnectionEvent::Error(msg) => {
                error!("Connection error: {}", msg);
            }
            ConnectionEvent::Disconnected(reason) => {
                info!("Disconnected: {:?}", reason);
            }
        }
    }

    /// 解析通知
    fn parse_notification(data: &str) -> Vec<TsEvent> {
        if data.starts_with("notifytextmessage") {
            vec![Self::parse_text_message(data)]
        } else if data.starts_with("notifycliententerview") {
            vec![Self::parse_client_enter(data)]
        } else if data.starts_with("notifyclientleftview") {
            vec![Self::parse_client_left(data)]
        } else {
            vec![]
        }
    }

    /// 解析文字消息
    fn parse_text_message(data: &str) -> TsEvent {
        let target_mode = match Self::extract_param(data, "targetmode") {
            Some("1") => TextMessageTarget::Private,
            Some("2") => TextMessageTarget::Channel,
            Some("3") => TextMessageTarget::Server,
            _ => return TsEvent::Unknown,
        };

        let invoker_name = Self::extract_param(data, "invokername")
            .unwrap_or_default()
            .to_string();
        let invoker_uid = Self::extract_param(data, "invokeruid")
            .unwrap_or_default()
            .to_string();
        let invoker_id = Self::extract_param(data, "invokerid")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let message = Self::extract_param(data, "msg")
            .unwrap_or_default()
            .to_string();

        TsEvent::TextMessage(TextMessageEvent {
            target_mode,
            invoker_name,
            invoker_uid,
            invoker_id,
            message,
        })
    }

    /// 解析客户端进入
    fn parse_client_enter(data: &str) -> TsEvent {
        let clid = Self::extract_param(data, "clid")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let cldbid = Self::extract_param(data, "client_database_id")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let client_nickname = Self::extract_param(data, "client_nickname")
            .unwrap_or_default()
            .to_string();
        let groups = Self::extract_param(data, "client_servergroups")
            .unwrap_or_default()
            .split(',')
            .filter_map(|s| s.parse().ok())
            .collect();

        TsEvent::ClientEnterView(ClientEnterEvent {
            clid,
            cldbid,
            client_nickname,
            client_server_groups: groups,
        })
    }

    /// 解析客户端离开
    fn parse_client_left(data: &str) -> TsEvent {
        let clid = Self::extract_param(data, "clid")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        TsEvent::ClientLeftView(ClientLeftEvent { clid })
    }

    /// 提取参数
    fn extract_param<'a>(data: &'a str, key: &str) -> Option<&'a str> {
        let pattern = format!("{}=", key);
        data.split_whitespace()
            .find(|s| s.starts_with(&pattern))
            .map(|s| &s[pattern.len()..])
    }

    /// 发送消息
    pub async fn send_message(&self, target_mode: u8, target: u32, message: &str) -> Result<()> {
        if let Some(conn) = self.connection.connection().await {
            conn.send_text_message(target_mode, target, message)
                .await
                .map_err(|e| AppError::TsError {
                    code: 0,
                    message: e.to_string(),
                })
        } else {
            Err(AppError::TsError {
                code: 0,
                message: "Not connected".into(),
            })
        }
    }

    /// 发送原始命令
    pub async fn send_raw(&self, cmd: &str) -> Result<()> {
        if let Some(conn) = self.connection.connection().await {
            conn.send_command(cmd).await.map_err(|e| AppError::TsError {
                code: 0,
                message: e.to_string(),
            })
        } else {
            Err(AppError::TsError {
                code: 0,
                message: "Not connected".into(),
            })
        }
    }

    /// 设置昵称
    pub async fn set_nickname(&self, nick: &str) -> Result<()> {
        let suffix = rand::random::<u16>();
        let nickname = format!("{}_{}", nick, suffix);
        info!("Setting nickname to {}", nickname);
        self.send_raw(&crate::adapter::serverquery::command::cmd_clientupdate_nick(&nickname))
            .await
    }

    /// 退出
    pub async fn quit(&self) -> Result<()> {
        info!("Sending quit command");
        self.connection.shutdown().await;
        Ok(())
    }

    /// 播放音频
    #[allow(dead_code)]
    pub async fn play_audio(&self, source: &str) -> Result<()> {
        if let Some(conn) = self.connection.connection().await {
            conn.play_audio(source)
                .await
                .map_err(|e| AppError::TsError {
                    code: 0,
                    message: e.to_string(),
                })
        } else {
            Err(AppError::TsError {
                code: 0,
                message: "Not connected".into(),
            })
        }
    }

    /// 停止音频
    #[allow(dead_code)]
    pub async fn stop_audio(&self) -> Result<()> {
        if let Some(conn) = self.connection.connection().await {
            conn.stop_audio().await.map_err(|e| AppError::TsError {
                code: 0,
                message: e.to_string(),
            })
        } else {
            Err(AppError::TsError {
                code: 0,
                message: "Not connected".into(),
            })
        }
    }

    /// 订阅事件
    pub fn subscribe(&self) -> broadcast::Receiver<TsEvent> {
        self.event_tx.subscribe()
    }

    /// 获取机器人客户端 ID
    pub fn get_bot_clid(&self) -> u32 {
        self.bot_clid.load(std::sync::atomic::Ordering::Relaxed)
    }
}
