//! TeamSpeak 连接状态机
//! 
//! 管理 TeamSpeak 客户端的连接状态

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, RwLock};
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
    /// 接收通道
    packet_rx: Arc<Mutex<mpsc::Receiver<Packet>>>,
    /// 加密处理器
    crypto: Arc<Mutex<TsCrypto>>,
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
            packet_rx: Arc::new(Mutex::new(packet_rx)),
            crypto: Arc::new(Mutex::new(crypto)),
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

        // 启动消息处理循环
        let connection = self.clone();
        tokio::spawn(async move {
            connection.message_loop().await;
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

    /// 消息处理循环
    async fn message_loop(&self) {
        let mut packet_rx = self.packet_rx.lock().await;

        while let Some(packet) = packet_rx.recv().await {
            if let Err(e) = self.handle_packet(packet).await {
                error!("Error handling packet: {e}");
                
                // 发送错误事件
                let _ = self.event_tx.send(ConnectionEvent::Error(e.to_string())).await;
                
                // 如果是严重错误，断开连接
                self.set_state(ConnectionState::Disconnected).await;
                break;
            }
        }

        debug!("Message loop ended");
    }

    /// 处理接收到的包
    async fn handle_packet(&self, packet: Packet) -> Result<()> {
        let state = *self.state.read().await;

        match state {
            ConnectionState::Connecting => {
                // 等待 Init1 响应或 initserver
                if packet.header.packet_type == PacketType::Command {
                    let data = String::from_utf8_lossy(&packet.data);
                    
                    if data.contains("initserver") {
                        // 服务器初始化响应
                        self.handle_init_server(&data).await?;
                    }
                }
            }
            ConnectionState::KeyExchange => {
                // 处理密钥交换
                if packet.header.packet_type == PacketType::Command {
                    let data = String::from_utf8_lossy(&packet.data);
                    
                    if data.contains("clientinitiv") {
                        self.handle_client_init_iv(&data).await?;
                    }
                }
            }
            ConnectionState::Initializing => {
                // 等待 clientinit 响应
                if packet.header.packet_type == PacketType::Command {
                    let data = String::from_utf8_lossy(&packet.data);
                    
                    if data.contains("notifycliententerview") {
                        // 成功进入服务器
                        self.handle_client_enter(&data).await?;
                    }
                }
            }
            ConnectionState::Connected => {
                // 处理正常消息
                let data = String::from_utf8_lossy(&packet.data);
                
                if data.starts_with("notify") {
                    let _ = self.event_tx.send(ConnectionEvent::Notification(data.to_string())).await;
                } else {
                    let _ = self.event_tx.send(ConnectionEvent::CommandResponse(data.to_string())).await;
                }
            }
            _ => {
                debug!("Ignoring packet in state {}: {}", state, packet);
            }
        }

        Ok(())
    }

    /// 处理 initserver 响应
    async fn handle_init_server(&self, data: &str) -> Result<()> {
        debug!("Handling initserver: {}", data);

        // 解析服务器公钥 (omega)
        let omega = self.extract_parameter(data, "omega")
            .ok_or_else(|| HeadlessError::ProtocolError("Missing omega".into()))?;

        // 生成 alpha 和 beta
        let alpha = BASE64.encode(crate::headless::crypto::generate_random_bytes(10));
        let beta = BASE64.encode(crate::headless::crypto::generate_random_bytes(10));

        // 初始化加密
        {
            let mut crypto = self.crypto.lock().await;
            crypto.crypto_init(
                &BASE64.decode(&alpha).map_err(|e| HeadlessError::CryptoError(e.to_string()))?,
                &BASE64.decode(&beta).map_err(|e| HeadlessError::CryptoError(e.to_string()))?,
                &BASE64.decode(&omega).map_err(|e| HeadlessError::CryptoError(e.to_string()))?,
            )?;
        }

        self.set_state(ConnectionState::KeyExchange).await;

        // 发送 clientinitiv
        let client_init_iv = format!(
            "clientinitiv alpha={} omega={} ip=",
            alpha,
            self.crypto.lock().await.identity().public_key_base64()
        );
        
        self.packet_handler.send(client_init_iv.as_bytes(), PacketType::Command).await?;

        Ok(())
    }

    /// 处理 clientinitiv 响应
    async fn handle_client_init_iv(&self, data: &str) -> Result<()> {
        debug!("Handling clientinitiv response: {}", data);

        self.set_state(ConnectionState::Initializing).await;

        // 发送 clientinit
        let client_init = format!(
            "clientinit client_nickname={} client_version=3.5.0 client_platform=Linux \
             client_input_hardware=1 client_output_hardware=1 client_default_channel \
             client_meta_data client_version_sign= client_key_offset=0 \
             client_nickname_phonetic client_default_token= client_badges",
            self.config.nickname
        );

        self.packet_handler.send(client_init.as_bytes(), PacketType::Command).await?;

        Ok(())
    }

    /// 处理客户端进入服务器
    async fn handle_client_enter(&self, data: &str) -> Result<()> {
        debug!("Handling client enter: {}", data);

        // 解析客户端 ID
        if let Some(clid_str) = self.extract_parameter(data, "clid") {
            if let Ok(clid) = clid_str.parse::<u16>() {
                *self.client_id.write().await = Some(clid);
                info!("Client ID: {}", clid);
            }
        }

        self.set_state(ConnectionState::Connected).await;

        // 发送状态变更事件
        let _ = self.event_tx.send(ConnectionEvent::StateChanged(ConnectionState::Connected)).await;

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
        let mut state = self.state.write().await;
        let old_state = *state;
        *state = new_state;

        if old_state != new_state {
            debug!("State changed: {} -> {}", old_state, new_state);
            let _ = self.event_tx.send(ConnectionEvent::StateChanged(new_state)).await;
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
    fn extract_parameter<'a>(&self, data: &'a str, param: &str) -> Option<&'a str> {
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
            packet_rx: self.packet_rx.clone(),
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
        
        // 直接测试参数提取逻辑
        fn extract<'a>(data: &'a str, param: &str) -> Option<&'a str> {
            let pattern = format!("{}=", param);
            data.split_whitespace()
                .find(|s| s.starts_with(&pattern))
                .map(|s| &s[pattern.len()..])
        }
        
        assert_eq!(extract(data, "server_name"), Some("Test\\sServer"));
        assert_eq!(extract(data, "server_welcome_message"), Some("Welcome"));
        assert_eq!(extract(data, "missing"), None);
    }
}
