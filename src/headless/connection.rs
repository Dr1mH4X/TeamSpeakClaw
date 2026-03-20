//! TeamSpeak 连接状态机
//! 
//! 管理 TeamSpeak 客户端的连接状态

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex as AsyncMutex, RwLock};
use tokio::time::{timeout, Duration};
use tracing::{debug, error, info};

use crate::headless::{
    crypto::TsCrypto,
    error::{HeadlessError, Result},
    identity::Identity,
    packet::{Packet, PacketType},
    packet_handler::{PacketHandler, PacketHandlerConfig},
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};

/// 连接状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// 已断开
    Disconnected,
    /// 正在连接（发送 Init1）
    Connecting,
    /// 密钥交换中
    KeyExchange,
    /// 正在初始化（发送 clientinit）
    Initializing,
    /// 已连接
    Connected,
    /// 正在断开
    Disconnecting,
}

impl std::fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disconnected => write!(f, "Disconnected"),
            Self::Connecting => write!(f, "Connecting"),
            Self::KeyExchange => write!(f, "KeyExchange"),
            Self::Initializing => write!(f, "Initializing"),
            Self::Connected => write!(f, "Connected"),
            Self::Disconnecting => write!(f, "Disconnecting"),
        }
    }
}

/// 连接配置
#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    /// 服务器地址
    pub server_addr: SocketAddr,
    /// 客户端昵称
    pub nickname: String,
    /// 身份
    pub identity: Identity,
    /// 连接超时
    pub connect_timeout: Duration,
}

/// 连接事件
#[derive(Debug, Clone)]
pub enum ConnectionEvent {
    /// 状态变更
    StateChanged(ConnectionState),
    /// 收到命令响应
    CommandResponse(String),
    /// 收到通知
    Notification(String),
    /// 错误
    Error(String),
    /// 断开连接
    Disconnected(Option<String>),
}

/// TeamSpeak 连接
pub struct Connection {
    /// 当前状态
    state: Arc<RwLock<ConnectionState>>,
    /// 包处理器
    packet_handler: Arc<PacketHandler>,
    /// 接收通道（connect 时 move 进消息循环，使用 std::sync::Mutex 因为 take 操作很快）
    packet_rx: std::sync::Mutex<Option<mpsc::Receiver<Packet>>>,
    /// 加密处理器
    crypto: Arc<AsyncMutex<TsCrypto>>,
    /// 客户端 ID
    client_id: Arc<RwLock<Option<u16>>>,
    /// 事件发送器
    event_tx: mpsc::Sender<ConnectionEvent>,
    /// 配置
    config: ConnectionConfig,
}

impl Connection {
    /// 创建新连接
    pub async fn new(
        config: ConnectionConfig,
    ) -> Result<(Self, mpsc::Receiver<ConnectionEvent>)> {
        let local_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
        
        let crypto = TsCrypto::new(config.identity.clone());
        
        let handler_config = PacketHandlerConfig {
            local_addr,
            remote_addr: config.server_addr,
        };

        let (handler, packet_rx) = PacketHandler::new(handler_config, crypto.clone()).await?;
        
        let (event_tx, event_rx) = mpsc::channel(1024);

        let connection = Self {
            state: Arc::new(RwLock::new(ConnectionState::Disconnected)),
            packet_handler: Arc::new(handler),
            packet_rx: std::sync::Mutex::new(Some(packet_rx)),
            crypto: Arc::new(AsyncMutex::new(crypto)),
            client_id: Arc::new(RwLock::new(None)),
            event_tx,
            config,
        };

        Ok((connection, event_rx))
    }

    /// 连接到服务器
    pub async fn connect(&self) -> Result<()> {
        // 检查当前状态
        {
            let state = self.state.read().await;
            if *state != ConnectionState::Disconnected {
                return Err(HeadlessError::ConnectionError(
                    format!("Cannot connect in state: {}", state)
                ));
            }
        }

        // 启动包处理器
        self.packet_handler.start().await?;

        // 更新状态
        self.set_state(ConnectionState::Connecting).await;

        // 发送 Init1 包
        {
            let crypto = self.crypto.lock().await;
            let init_data = crypto.process_init1();
            drop(crypto);
            
            self.packet_handler.send(&init_data, PacketType::Init1).await?;
        }

        // 启动消息处理循环（packet_rx move 进任务，不再共享）
        let state = self.state.clone();
        let packet_handler = self.packet_handler.clone();
        let crypto = self.crypto.clone();
        let client_id = self.client_id.clone();
        let event_tx = self.event_tx.clone();
        let config = self.config.clone();
        let mut packet_rx = self.packet_rx.lock().unwrap().take()
            .ok_or_else(|| HeadlessError::ConnectionError("Already connected".into()))?;

        tokio::spawn(async move {
            Self::message_loop(
                &state,
                &packet_handler,
                &crypto,
                &client_id,
                &event_tx,
                &config,
                &mut packet_rx,
            ).await;
        });

        // 等待连接完成或超时
        match timeout(self.config.connect_timeout, self.wait_for_connected()).await {
            Ok(Ok(())) => {
                info!("Connected to server");
                Ok(())
            }
            Ok(Err(e)) => {
                error!("Connection failed: {e}");
                self.set_state(ConnectionState::Disconnected).await;
                Err(e)
            }
            Err(_) => {
                error!("Connection timeout");
                self.set_state(ConnectionState::Disconnected).await;
                Err(HeadlessError::Timeout)
            }
        }
    }

    /// 等待连接完成
    async fn wait_for_connected(&self) -> Result<()> {
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            
            let state = self.state.read().await;
            match *state {
                ConnectionState::Connected => return Ok(()),
                ConnectionState::Disconnected => {
                    return Err(HeadlessError::ConnectionError("Connection failed".into()));
                }
                _ => continue,
            }
        }
    }

    /// 消息处理循环（独立于 Connection 实例运行）
    async fn message_loop(
        state: &Arc<RwLock<ConnectionState>>,
        packet_handler: &Arc<PacketHandler>,
        crypto: &Arc<AsyncMutex<TsCrypto>>,
        client_id: &Arc<RwLock<Option<u16>>>,
        event_tx: &mpsc::Sender<ConnectionEvent>,
        config: &ConnectionConfig,
        packet_rx: &mut mpsc::Receiver<Packet>,
    ) {
        while let Some(packet) = packet_rx.recv().await {
            if let Err(e) = Self::handle_packet(
                packet, state, packet_handler, crypto, client_id, event_tx, config,
            ).await {
                error!("Error handling packet: {e}");
                
                // 发送错误事件
                let _ = event_tx.send(ConnectionEvent::Error(e.to_string())).await;
                
                // 如果是严重错误，断开连接
                Self::set_state_static(state, event_tx, ConnectionState::Disconnected).await;
                break;
            }
        }

        debug!("Message loop ended");
    }

    /// 处理接收到的包（独立版本）
    async fn handle_packet(
        packet: Packet,
        state: &Arc<RwLock<ConnectionState>>,
        packet_handler: &Arc<PacketHandler>,
        crypto: &Arc<AsyncMutex<TsCrypto>>,
        client_id: &Arc<RwLock<Option<u16>>>,
        event_tx: &mpsc::Sender<ConnectionEvent>,
        config: &ConnectionConfig,
    ) -> Result<()> {
        let current_state = *state.read().await;

        match current_state {
            ConnectionState::Connecting => {
                if packet.header.packet_type == PacketType::Command {
                    let data = String::from_utf8_lossy(&packet.data);
                    
                    if data.contains("initserver") {
                        Self::handle_init_server(&data, crypto, packet_handler, state, event_tx, config).await?;
                    }
                }
            }
            ConnectionState::KeyExchange => {
                if packet.header.packet_type == PacketType::Command {
                    let data = String::from_utf8_lossy(&packet.data);
                    
                    if data.contains("clientinitiv") {
                        Self::handle_client_init_iv(&data, crypto, packet_handler, state, event_tx, config).await?;
                    }
                }
            }
            ConnectionState::Initializing => {
                if packet.header.packet_type == PacketType::Command {
                    let data = String::from_utf8_lossy(&packet.data);
                    
                    if data.contains("notifycliententerview") {
                        Self::handle_client_enter(&data, client_id, state, event_tx).await?;
                    }
                }
            }
            ConnectionState::Connected => {
                let data = String::from_utf8_lossy(&packet.data);
                
                if data.starts_with("notify") {
                    let _ = event_tx.send(ConnectionEvent::Notification(data.to_string())).await;
                } else {
                    let _ = event_tx.send(ConnectionEvent::CommandResponse(data.to_string())).await;
                }
            }
            _ => {
                debug!("Ignoring packet in state {}: {}", current_state, packet);
            }
        }

        Ok(())
    }

    /// 处理 initserver 响应（独立版本）
    async fn handle_init_server(
        data: &str,
        crypto: &Arc<AsyncMutex<TsCrypto>>,
        packet_handler: &Arc<PacketHandler>,
        state: &Arc<RwLock<ConnectionState>>,
        _event_tx: &mpsc::Sender<ConnectionEvent>,
        _config: &ConnectionConfig,
    ) -> Result<()> {
        // 解析服务器公钥 (omega)
        let omega = Self::extract_param(data, "omega")
            .ok_or_else(|| HeadlessError::ProtocolError("Missing omega".into()))?;

        // 生成 alpha 和 beta
        let alpha = BASE64.encode(crate::headless::crypto::generate_random_bytes(10));
        let beta = BASE64.encode(crate::headless::crypto::generate_random_bytes(10));

        // 初始化加密
        {
            let mut crypto_guard = crypto.lock().await;
            crypto_guard.crypto_init(
                &BASE64.decode(&alpha).map_err(|e| HeadlessError::CryptoError(e.to_string()))?,
                &BASE64.decode(&beta).map_err(|e| HeadlessError::CryptoError(e.to_string()))?,
                &BASE64.decode(&omega).map_err(|e| HeadlessError::CryptoError(e.to_string()))?,
            )?;
        }

        Self::set_state_static(state, _event_tx, ConnectionState::KeyExchange).await;

        // 发送 clientinitiv
        let public_key_b64 = crypto.lock().await.identity().public_key_base64();
        let client_init_iv = format!(
            "clientinitiv alpha={} omega={} ip=",
            alpha,
            public_key_b64
        );
        
        packet_handler.send(client_init_iv.as_bytes(), PacketType::Command).await?;

        Ok(())
    }

    /// 处理 clientinitiv 响应（独立版本）
    async fn handle_client_init_iv(
        data: &str,
        _crypto: &Arc<AsyncMutex<TsCrypto>>,
        packet_handler: &Arc<PacketHandler>,
        state: &Arc<RwLock<ConnectionState>>,
        event_tx: &mpsc::Sender<ConnectionEvent>,
        config: &ConnectionConfig,
    ) -> Result<()> {
        debug!("Handling clientinitiv response: {}", data);

        Self::set_state_static(state, event_tx, ConnectionState::Initializing).await;

        // 发送 clientinit
        let client_init = format!(
            "clientinit client_nickname={} client_version=3.5.0 client_platform=Linux \
             client_input_hardware=1 client_output_hardware=1 client_default_channel \
             client_meta_data client_version_sign= client_key_offset=0 \
             client_nickname_phonetic client_default_token= client_badges",
            config.nickname
        );

        packet_handler.send(client_init.as_bytes(), PacketType::Command).await?;

        Ok(())
    }

    /// 处理客户端进入服务器（独立版本）
    async fn handle_client_enter(
        data: &str,
        client_id: &Arc<RwLock<Option<u16>>>,
        state: &Arc<RwLock<ConnectionState>>,
        event_tx: &mpsc::Sender<ConnectionEvent>,
    ) -> Result<()> {
        debug!("Handling client enter: {}", data);

        // 解析客户端 ID
        if let Some(clid_str) = Self::extract_param(data, "clid") {
            if let Ok(clid) = clid_str.parse::<u16>() {
                *client_id.write().await = Some(clid);
                info!("Client ID: {}", clid);
            }
        }

        Self::set_state_static(state, event_tx, ConnectionState::Connected).await;

        // 发送状态变更事件
        let _ = event_tx.send(ConnectionEvent::StateChanged(ConnectionState::Connected)).await;

        Ok(())
    }

    /// 发送命令
    pub async fn send_command(&self, command: &str) -> Result<()> {
        let state = self.state.read().await;
        if *state != ConnectionState::Connected {
            return Err(HeadlessError::NotConnected);
        }

        self.packet_handler.send(command.as_bytes(), PacketType::Command).await
    }

    /// 发送文字消息
    pub async fn send_text_message(&self, target_mode: u8, target: u32, message: &str) -> Result<()> {
        let command = format!(
            "sendtextmessage targetmode={} target={} msg={}",
            target_mode,
            target,
            crate::adapter::command::ts_escape(message)
        );
        self.send_command(&command).await
    }

    /// 断开连接
    pub async fn disconnect(&self) -> Result<()> {
        let state = self.state.read().await;
        if *state == ConnectionState::Disconnected {
            return Ok(());
        }
        drop(state);

        self.set_state(ConnectionState::Disconnecting).await;

        // 发送退出命令
        let _ = self.send_command("quit").await;

        // 关闭包处理器
        self.packet_handler.shutdown().await;

        self.set_state(ConnectionState::Disconnected).await;

        // 发送断开事件
        let _ = self.event_tx.send(ConnectionEvent::Disconnected(None)).await;

        Ok(())
    }

    /// 设置状态
    async fn set_state(&self, new_state: ConnectionState) {
        Self::set_state_static(&self.state, &self.event_tx, new_state).await;
    }

    /// 设置状态（静态版本，供消息循环使用）
    async fn set_state_static(
        state: &Arc<RwLock<ConnectionState>>,
        event_tx: &mpsc::Sender<ConnectionEvent>,
        new_state: ConnectionState,
    ) {
        let mut state_guard = state.write().await;
        let old_state = *state_guard;
        *state_guard = new_state;

        if old_state != new_state {
            debug!("State changed: {} -> {}", old_state, new_state);
            let _ = event_tx.send(ConnectionEvent::StateChanged(new_state)).await;
        }
    }

    /// 获取当前状态
    pub async fn state(&self) -> ConnectionState {
        *self.state.read().await
    }

    /// 获取客户端 ID
    pub async fn client_id(&self) -> Option<u16> {
        *self.client_id.read().await
    }

    /// 提取参数值
    fn extract_param<'a>(data: &'a str, param: &str) -> Option<&'a str> {
        let pattern = format!("{}=", param);
        data.split_whitespace()
            .find(|s| s.starts_with(&pattern))
            .map(|s| &s[pattern.len()..])
    }
}

impl Clone for Connection {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            packet_handler: self.packet_handler.clone(),
            packet_rx: std::sync::Mutex::new(None), // clone 不持有接收器，只在原始实例中使用
            crypto: self.crypto.clone(),
            client_id: self.client_id.clone(),
            event_tx: self.event_tx.clone(),
            config: self.config.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connection_state_display() {
        assert_eq!(ConnectionState::Disconnected.to_string(), "Disconnected");
        assert_eq!(ConnectionState::Connected.to_string(), "Connected");
    }

    #[test]
    fn test_extract_parameter() {
        let data = "initserver server_name=Test\\sServer server_welcome_message=Welcome";
        
        assert_eq!(Connection::extract_param(data, "server_name"), Some("Test\\sServer"));
        assert_eq!(Connection::extract_param(data, "server_welcome_message"), Some("Welcome"));
        assert_eq!(Connection::extract_param(data, "missing"), None);
    }
}
