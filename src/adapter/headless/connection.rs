//! TeamSpeak 连接状态机（握手链路版）

use base64::Engine;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex as AsyncMutex, RwLock};
use tokio::time::{timeout, Duration};
use tracing::{debug, error, info, warn};

use super::audio::{AudioConfig, AudioPlayer};
use super::crypto::TsCrypto;
use super::error::{HeadlessError, Result};
use super::identity::Identity;
use super::packet::{Packet, PacketType};
use super::packet_handler::{PacketHandler, PacketHandlerConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    KeyExchange,
    Initializing,
    Connected,
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

#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    pub server_addr: SocketAddr,
    pub nickname: String,
    pub identity: Identity,
    pub connect_timeout: Duration,
    pub audio: Option<AudioConfig>,
}

#[derive(Debug, Clone)]
pub enum ConnectionEvent {
    StateChanged(ConnectionState),
    CommandResponse(String),
    Notification(String),
    Error(String),
    Disconnected(Option<String>),
}

pub struct Connection {
    state: Arc<RwLock<ConnectionState>>,
    packet_handler: Arc<PacketHandler>,
    packet_rx: std::sync::Mutex<Option<mpsc::Receiver<Packet>>>,
    crypto: Arc<AsyncMutex<TsCrypto>>,
    client_id: Arc<RwLock<Option<u16>>>,
    event_tx: mpsc::Sender<ConnectionEvent>,
    config: ConnectionConfig,
    audio_player: Option<Arc<AudioPlayer>>,
}

impl Connection {
    pub async fn new(config: ConnectionConfig) -> Result<(Self, mpsc::Receiver<ConnectionEvent>)> {
        let local_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let crypto = Arc::new(AsyncMutex::new(TsCrypto::new(config.identity.clone())));
        let (handler, packet_rx) = PacketHandler::new(
            PacketHandlerConfig {
                local_addr,
                remote_addr: config.server_addr,
            },
            crypto.clone(),
        )
        .await?;
        let handler = Arc::new(handler);

        let (event_tx, event_rx) = mpsc::channel(1024);

        let audio_player = if let Some(audio_config) = config.audio.clone() {
            let (frame_tx, mut frame_rx) = mpsc::channel(1024);
            let player = Arc::new(AudioPlayer::new(audio_config, frame_tx));
            let handler_clone = handler.clone();
            tokio::spawn(async move {
                let mut sequence: u16 = 0;
                while let Some(opus_data) = frame_rx.recv().await {
                    let mut data = Vec::with_capacity(2 + opus_data.len());
                    data.extend_from_slice(&sequence.to_be_bytes());
                    data.extend_from_slice(&opus_data);
                    let _ = handler_clone.send(&data, PacketType::Voice).await;
                    sequence = sequence.wrapping_add(1);
                }
            });
            Some(player)
        } else {
            None
        };

        Ok((
            Self {
                state: Arc::new(RwLock::new(ConnectionState::Disconnected)),
                packet_handler: handler,
                packet_rx: std::sync::Mutex::new(Some(packet_rx)),
                crypto,
                client_id: Arc::new(RwLock::new(None)),
                event_tx,
                config,
                audio_player,
            },
            event_rx,
        ))
    }

    pub async fn connect(&self) -> Result<()> {
        {
            let state = self.state.read().await;
            if *state != ConnectionState::Disconnected {
                return Err(HeadlessError::ConnectionError(format!(
                    "Cannot connect in state: {}",
                    state
                )));
            }
        }

        self.packet_handler.start().await?;
        self.set_state(ConnectionState::Connecting).await;

        // TS3AudioBot: command 计数在握手开始前先 +1
        self.packet_handler.bump_counter(PacketType::Command).await;

        let init_data = self.crypto.lock().await.process_init1_start();
        self.packet_handler
            .send(&init_data, PacketType::Init1)
            .await?;

        let state = self.state.clone();
        let packet_handler = self.packet_handler.clone();
        let crypto = self.crypto.clone();
        let client_id = self.client_id.clone();
        let event_tx = self.event_tx.clone();
        let config = self.config.clone();
        let mut packet_rx = self
            .packet_rx
            .lock()
            .unwrap()
            .take()
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
            )
            .await;
        });

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

    async fn wait_for_connected(&self) -> Result<()> {
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            match *self.state.read().await {
                ConnectionState::Connected => return Ok(()),
                ConnectionState::Disconnected => {
                    return Err(HeadlessError::ConnectionError("Connection failed".into()))
                }
                _ => {}
            }
        }
    }

    async fn message_loop(
        state: &Arc<RwLock<ConnectionState>>,
        packet_handler: &Arc<PacketHandler>,
        crypto: &Arc<AsyncMutex<TsCrypto>>,
        client_id: &Arc<RwLock<Option<u16>>>,
        event_tx: &mpsc::Sender<ConnectionEvent>,
        config: &ConnectionConfig,
        packet_rx: &mut mpsc::Receiver<Packet>,
    ) {
        loop {
            match timeout(Duration::from_secs(35), packet_rx.recv()).await {
                Ok(Some(packet)) => {
                    if let Err(e) = Self::handle_packet(
                        packet,
                        state,
                        packet_handler,
                        crypto,
                        client_id,
                        event_tx,
                        config,
                    )
                    .await
                    {
                        error!("Error handling packet: {e}");
                        let _ = event_tx.send(ConnectionEvent::Error(e.to_string())).await;
                        Self::set_state_static(state, event_tx, ConnectionState::Disconnected)
                            .await;
                        break;
                    }
                }
                Ok(None) => {
                    debug!("Message loop ended (channel closed)");
                    Self::set_state_static(state, event_tx, ConnectionState::Disconnected).await;
                    let _ = event_tx
                        .send(ConnectionEvent::Disconnected(Some(
                            "Connection closed".into(),
                        )))
                        .await;
                    break;
                }
                Err(_) => {
                    warn!("Message loop timed out (35s)");
                    Self::set_state_static(state, event_tx, ConnectionState::Disconnected).await;
                    let _ = event_tx
                        .send(ConnectionEvent::Disconnected(Some("Timeout".into())))
                        .await;
                    break;
                }
            }
        }
    }

    async fn handle_packet(
        packet: Packet,
        state: &Arc<RwLock<ConnectionState>>,
        packet_handler: &Arc<PacketHandler>,
        crypto: &Arc<AsyncMutex<TsCrypto>>,
        client_id: &Arc<RwLock<Option<u16>>>,
        event_tx: &mpsc::Sender<ConnectionEvent>,
        config: &ConnectionConfig,
    ) -> Result<()> {
        if packet.header.packet_type == PacketType::Init1 {
            let reply = {
                let mut guard = crypto.lock().await;
                guard.process_init1_reply(&packet.data)?
            };

            if let Some(reply) = reply {
                if reply.starts_with(b"clientinitiv ") {
                    debug!("Sending clientinitiv after Init1 step 3");
                    packet_handler.send(&reply, PacketType::Command).await?;
                    Self::set_state_static(state, event_tx, ConnectionState::KeyExchange).await;
                } else {
                    debug!("Forwarding Init1 reply packet");
                    packet_handler.send(&reply, PacketType::Init1).await?;
                }
            }
            return Ok(());
        }

        let current_state = *state.read().await;
        match current_state {
            ConnectionState::Connecting
            | ConnectionState::KeyExchange
            | ConnectionState::Initializing => {
                if packet.header.packet_type == PacketType::Command
                    || packet.header.packet_type == PacketType::CommandLow
                {
                    let data = String::from_utf8_lossy(&packet.data).to_string();
                    let data = Self::ts_unescape_line(&data);
                    debug!("Handshake command payload: {}", data);

                    if data.contains("initivexpand ") {
                        debug!("Received initivexpand");
                        Self::handle_initivexpand(&data, crypto).await?;
                        Self::send_client_init(packet_handler, config).await?;
                        Self::set_state_static(state, event_tx, ConnectionState::Initializing)
                            .await;
                    } else if data.contains("initivexpand2 ") {
                        debug!("Received initivexpand2");
                        Self::handle_initivexpand2(&data, crypto, packet_handler, config).await?;
                        Self::send_client_init(packet_handler, config).await?;
                        Self::set_state_static(state, event_tx, ConnectionState::Initializing)
                            .await;
                    } else if data.contains("initserver ") {
                        debug!("Received initserver");
                        Self::handle_init_server(
                            &data,
                            packet_handler,
                            client_id,
                            state,
                            event_tx,
                            config,
                        )
                        .await?;
                    } else {
                        if let Some(code) = Self::extract_error_code(&data) {
                            if code != 0 {
                                return Err(HeadlessError::ConnectionError(format!(
                                    "Handshake failed: {data}"
                                )));
                            }
                        }

                        // 某些服务器在握手后不会先发 initserver，而是先推业务通知；
                        // 在已完成 initivexpand2/clientinit 的前提下，将其视为连接成功信号。
                        if current_state != ConnectionState::Connected
                            && Self::is_connected_signal(&data)
                        {
                            Self::set_state_static(state, event_tx, ConnectionState::Connected)
                                .await;
                        }

                        if data.starts_with("notify") {
                            let _ = event_tx
                                .send(ConnectionEvent::Notification(data.to_string()))
                                .await;
                        } else {
                            let _ = event_tx
                                .send(ConnectionEvent::CommandResponse(data.to_string()))
                                .await;
                        }
                    }
                }
            }
            ConnectionState::Connected | ConnectionState::Disconnecting => {
                if packet.header.packet_type != PacketType::Command
                    && packet.header.packet_type != PacketType::CommandLow
                {
                    return Ok(());
                }

                let data = Self::ts_unescape_line(&String::from_utf8_lossy(&packet.data));
                let self_clid = *client_id.read().await;

                // 对齐 TS3AudioBot：收到自己的 leftview 通知后，立即进入 Disconnected。
                if Self::is_self_disconnect_notification(&data, self_clid) {
                    Self::set_state_static(state, event_tx, ConnectionState::Disconnected).await;
                    let _ = event_tx
                        .send(ConnectionEvent::Disconnected(Some(data.to_string())))
                        .await;
                    return Ok(());
                }

                if data.starts_with("notify") {
                    let _ = event_tx
                        .send(ConnectionEvent::Notification(data.to_string()))
                        .await;
                } else {
                    let _ = event_tx
                        .send(ConnectionEvent::CommandResponse(data.to_string()))
                        .await;
                }
            }
            _ => {
                debug!("Ignoring packet in state {}: {}", current_state, packet);
            }
        }
        Ok(())
    }

    async fn handle_initivexpand(data: &str, crypto: &Arc<AsyncMutex<TsCrypto>>) -> Result<()> {
        let alpha = Self::extract_param(data, "alpha")
            .ok_or_else(|| HeadlessError::ProtocolError("Missing alpha".into()))?;
        let beta = Self::extract_param(data, "beta")
            .ok_or_else(|| HeadlessError::ProtocolError("Missing beta".into()))?;
        let omega = Self::extract_param(data, "omega")
            .ok_or_else(|| HeadlessError::ProtocolError("Missing omega".into()))?;

        let alpha = base64::engine::general_purpose::STANDARD
            .decode(alpha)
            .map_err(|e| HeadlessError::CryptoError(e.to_string()))?;
        let beta = base64::engine::general_purpose::STANDARD
            .decode(beta)
            .map_err(|e| HeadlessError::CryptoError(e.to_string()))?;
        let omega = base64::engine::general_purpose::STANDARD
            .decode(omega)
            .map_err(|e| HeadlessError::CryptoError(e.to_string()))?;

        crypto.lock().await.crypto_init(&alpha, &beta, &omega)
    }

    async fn handle_initivexpand2(
        data: &str,
        crypto: &Arc<AsyncMutex<TsCrypto>>,
        packet_handler: &Arc<PacketHandler>,
        config: &ConnectionConfig,
    ) -> Result<()> {
        let beta_b64 = Self::extract_param(data, "beta")
            .ok_or_else(|| HeadlessError::ProtocolError("Missing beta".into()))?;
        let license_b64 = Self::extract_param(data, "l")
            .ok_or_else(|| HeadlessError::ProtocolError("Missing l".into()))?;
        let omega_b64 = Self::extract_param(data, "omega")
            .ok_or_else(|| HeadlessError::ProtocolError("Missing omega".into()))?;
        let proof_b64 = Self::extract_param(data, "proof")
            .ok_or_else(|| HeadlessError::ProtocolError("Missing proof".into()))?;

        let beta = base64::engine::general_purpose::STANDARD
            .decode(beta_b64)
            .map_err(|e| HeadlessError::CryptoError(format!("beta decode failed: {e}")))?;
        if beta.len() != 54 {
            return Err(HeadlessError::ProtocolError(format!(
                "initivexpand2 beta must be 54 bytes, got {}",
                beta.len()
            )));
        }

        let (temporary_public_key, temporary_private_key) = TsCrypto::generate_temporary_keypair();
        let mut to_sign = [0u8; 86];
        to_sign[..32].copy_from_slice(&temporary_public_key);
        to_sign[32..].copy_from_slice(&beta);

        let proof_der = config.identity.sign_ecdsa_sha256_der(&to_sign)?;
        let ek_b64 = base64::engine::general_purpose::STANDARD.encode(temporary_public_key);
        let proof_b64_client = base64::engine::general_purpose::STANDARD.encode(proof_der);
        let client_ek = format!("clientek ek={} proof={}", ek_b64, proof_b64_client);
        packet_handler
            .send(client_ek.as_bytes(), PacketType::Command)
            .await?;

        {
            let mut guard = crypto.lock().await;
            guard.crypto_init2(
                license_b64,
                omega_b64,
                proof_b64,
                beta_b64,
                &temporary_private_key,
            )?;
        }

        debug!("initivexpand2: sent clientek and completed CryptoInit2");

        Ok(())
    }

    async fn send_client_init(
        packet_handler: &Arc<PacketHandler>,
        config: &ConnectionConfig,
    ) -> Result<()> {
        // 使用 TS3AudioBot 常用签名参数（Linux 3.5.0）
        let client_init = format!(
            "clientinit client_nickname={} client_version=114.514.0\\s[Build:\\s1584610661] \
             client_platform=Linux client_input_hardware=1 client_output_hardware=1 \
             client_default_channel= client_default_channel_password= client_server_password= \
             client_meta_data= client_version_sign=kHfR/JyZ6Ah06rW/t+dFIHkOgLGFth5CCbRr9T3xfPd2gqL5CeYei47LGBjA9K9GrVVRivF0L5eo5MrxGh/QDA== \
             client_key_offset={} client_nickname_phonetic= client_default_token= hwid=",
            config.nickname,
            config.identity.key_offset
        );
        packet_handler
            .send(client_init.as_bytes(), PacketType::Command)
            .await
    }

    async fn handle_init_server(
        data: &str,
        packet_handler: &Arc<PacketHandler>,
        client_id: &Arc<RwLock<Option<u16>>>,
        state: &Arc<RwLock<ConnectionState>>,
        event_tx: &mpsc::Sender<ConnectionEvent>,
        _config: &ConnectionConfig,
    ) -> Result<()> {
        if let Some(clid_str) = Self::extract_param(data, "clid") {
            if let Ok(clid) = clid_str.parse::<u16>() {
                *client_id.write().await = Some(clid);
                packet_handler.set_client_id(clid).await;
                info!("Client ID: {}", clid);
            }
        }

        Self::set_state_static(state, event_tx, ConnectionState::Connected).await;
        let _ = event_tx
            .send(ConnectionEvent::StateChanged(ConnectionState::Connected))
            .await;
        Ok(())
    }

    pub async fn send_command(&self, command: &str) -> Result<()> {
        if *self.state.read().await != ConnectionState::Connected {
            return Err(HeadlessError::NotConnected);
        }
        self.packet_handler
            .send(command.as_bytes(), PacketType::Command)
            .await
    }

    pub async fn send_text_message(
        &self,
        target_mode: u8,
        target: u32,
        message: &str,
    ) -> Result<()> {
        let command = format!(
            "sendtextmessage targetmode={} target={} msg={}",
            target_mode,
            target,
            crate::adapter::serverquery::command::ts_escape(message)
        );
        self.send_command(&command).await
    }

    pub async fn disconnect(&self) -> Result<()> {
        let current_state = *self.state.read().await;
        if current_state == ConnectionState::Disconnected {
            return Ok(());
        }

        if current_state == ConnectionState::Connected {
            let reason_msg = crate::adapter::serverquery::command::ts_escape("Disconnected");
            let command = format!("clientdisconnect reasonid=8 reasonmsg={reason_msg}");
            if let Err(e) = self
                .packet_handler
                .send(command.as_bytes(), PacketType::Command)
                .await
            {
                warn!("Failed to send clientdisconnect: {}", e);
            } else {
                info!("Sent clientdisconnect, waiting for leave notification");
            }
            self.set_state(ConnectionState::Disconnecting).await;
            let _ = timeout(Duration::from_secs(2), self.wait_for_disconnected()).await;
        } else if current_state != ConnectionState::Disconnecting {
            self.set_state(ConnectionState::Disconnecting).await;
        }

        self.packet_handler.shutdown().await;
        if *self.state.read().await != ConnectionState::Disconnected {
            self.set_state(ConnectionState::Disconnected).await;
            let _ = self
                .event_tx
                .send(ConnectionEvent::Disconnected(Some("local shutdown".into())))
                .await;
        }

        Ok(())
    }

    #[allow(dead_code)]
    pub async fn play_audio(&self, source: &str) -> Result<()> {
        if *self.state.read().await != ConnectionState::Connected {
            return Err(HeadlessError::NotConnected);
        }
        if let Some(player) = &self.audio_player {
            player
                .play(source.to_string())
                .await
                .map_err(|e| HeadlessError::AudioError(e.to_string()))
        } else {
            Err(HeadlessError::AudioError("Audio not enabled".into()))
        }
    }

    #[allow(dead_code)]
    pub async fn stop_audio(&self) -> Result<()> {
        if let Some(player) = &self.audio_player {
            player
                .stop()
                .await
                .map_err(|e| HeadlessError::AudioError(e.to_string()))
        } else {
            Err(HeadlessError::AudioError("Audio not enabled".into()))
        }
    }

    async fn set_state(&self, new_state: ConnectionState) {
        Self::set_state_static(&self.state, &self.event_tx, new_state).await;
    }

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
            let _ = event_tx
                .send(ConnectionEvent::StateChanged(new_state))
                .await;
        }
    }

    pub async fn state(&self) -> ConnectionState {
        *self.state.read().await
    }

    async fn wait_for_disconnected(&self) {
        loop {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if *self.state.read().await == ConnectionState::Disconnected {
                return;
            }
        }
    }

    fn extract_param<'a>(data: &'a str, key: &str) -> Option<&'a str> {
        let pattern = format!("{}=", key);
        data.split_whitespace()
            .find(|s| s.starts_with(&pattern))
            .map(|s| &s[pattern.len()..])
    }

    fn ts_unescape_line(s: &str) -> String {
        s.replace("\\s", " ")
            .replace("\\p", "|")
            .replace("\\/", "/")
            .replace("\\\\", "\\")
    }

    fn is_connected_signal(data: &str) -> bool {
        data.starts_with("notify")
            || data.contains("channellistfinished")
            || data.contains("servergrouplistfinished")
            || data.contains("channelgrouplistfinished")
            || data.contains("clientneededpermissions")
            || data.contains("notifyclientneededpermissions")
    }

    fn extract_error_code(data: &str) -> Option<u32> {
        if !data.starts_with("error ") {
            return None;
        }
        data.split_whitespace()
            .find(|part| part.starts_with("id="))
            .and_then(|part| part.strip_prefix("id="))
            .and_then(|v| v.parse::<u32>().ok())
    }

    fn is_self_disconnect_notification(data: &str, self_clid: Option<u16>) -> bool {
        if !data.starts_with("notifyclientleftview") {
            return false;
        }
        let Some(self_clid) = self_clid else {
            return false;
        };
        Self::extract_param(data, "clid")
            .and_then(|v| v.parse::<u16>().ok())
            .map(|clid| clid == self_clid)
            .unwrap_or(false)
    }
}

impl Clone for Connection {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            packet_handler: self.packet_handler.clone(),
            packet_rx: std::sync::Mutex::new(None),
            crypto: self.crypto.clone(),
            client_id: self.client_id.clone(),
            event_tx: self.event_tx.clone(),
            config: self.config.clone(),
            audio_player: self.audio_player.clone(),
        }
    }
}
