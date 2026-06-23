use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use tsclient_rs;

use crate::config::AppConfig;

/// 检查 TS 错误，如果是权限问题则额外提示用户。
fn check_ts_error(err: tsclient_rs::Error, op: &str) -> anyhow::Error {
    let is_perm = matches!(&err,
        tsclient_rs::Error::ServerError { id, .. }
        if id == "2568" || id == "2569" || id.contains("permission") || id.contains("insufficient")
    );
    if is_perm {
        error!(
            "{op} 失败：权限不足。请给机器人 Server Admin 权限（将机器人的服务器组加入 Admin 组）"
        );
    }
    anyhow!("{op} failed: {err}")
}

/// 封装 tsclient-rs::Client，提供管理命令和事件订阅。
/// 共享的 `Arc<Client>` 可通过 `get_client()` 给 voice 模块使用。
pub struct TsAdapter {
    client: Arc<tsclient_rs::Client>,
    event_tx: broadcast::Sender<TsEvent>,
    bot_clid: std::sync::atomic::AtomicU32,
}

impl TsAdapter {
    pub async fn connect(
        config: Arc<AppConfig>,
        identity_file: PathBuf,
    ) -> Result<Arc<Self>> {
        let hc = &config.headless;
        let host = &hc.server_address;
        let port = hc.server_port;
        let nickname = &config.bot.nickname;

        let mut identity = Self::load_or_create_identity(&identity_file, 8)?;
        let addr = format!("{host}:{port}");

        let make_opts = || tsclient_rs::ClientOptions {
            server_password: if hc.server_password.is_empty() {
                None
            } else {
                Some(hc.server_password.clone())
            },
            ..Default::default()
        };

        let register_handlers = |client: &tsclient_rs::Client, tx: broadcast::Sender<TsEvent>| {
            {
                let tx = tx.clone();
                client.on_text_message(Arc::new(move |event: tsclient_rs::Event| {
                    if let tsclient_rs::Event::TextMessage(ref msg) = event {
                        let _ = tx.send(TsEvent::TextMessage(TextMessageEvent {
                            target_mode: match msg.target_mode {
                                1 => TextMessageTarget::Private,
                                2 => TextMessageTarget::Channel,
                                _ => TextMessageTarget::Server,
                            },
                            invoker_name: msg.invoker_name.clone(),
                            invoker_uid: msg.invoker_uid.clone(),
                            invoker_id: msg.invoker_id as u32,
                            invoker_groups: msg.invoker_groups.clone(),
                            message: msg.message.clone(),
                        }));
                    }
                }));
            }

            {
                let tx = tx.clone();
                client.on_client_enter(Arc::new(move |event: tsclient_rs::Event| {
                    if let tsclient_rs::Event::ClientEnter(ref info) = event {
                        let groups: Vec<u32> = info
                            .server_groups
                            .iter()
                            .filter_map(|g| g.parse().ok())
                            .collect();
                        let _ = tx.send(TsEvent::ClientEnterView(ClientEnterEvent {
                            clid: info.id as u32,
                            cldbid: 0,
                            client_nickname: info.nickname.clone(),
                            client_server_groups: groups,
                            client_channel_group_id: 0,
                            channel_id: info.channel_id as u32,
                        }));
                        debug!(
                            "Client entered view: clid={}, nickname={}, channel_id={}",
                            info.id, info.nickname, info.channel_id
                        );
                    }
                }));
            }

            {
                let tx = tx.clone();
                client.on_client_leave(Arc::new(move |event: tsclient_rs::Event| {
                    if let tsclient_rs::Event::ClientLeave(ref ev) = event {
                        let _ = tx.send(TsEvent::ClientLeftView(ClientLeftEvent {
                            clid: ev.id as u32,
                        }));
                    }
                }));
            }
        };

        let mut current_level = identity.security_level();
        const MAX_LEVEL: i32 = 29;
        const STEP: i32 = 5;
        const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(8);

        loop {
            let opts = make_opts();
            let mut client =
                tsclient_rs::Client::new(identity.clone(), addr.clone(), nickname.clone(), opts);

            let (event_tx, _) = broadcast::channel::<TsEvent>(256);
            register_handlers(&client, event_tx.clone());

            client
                .connect()
                .await
                .map_err(|e| anyhow!("tsclient connect failed: {e}"))?;

            match tokio::time::timeout(HANDSHAKE_TIMEOUT, client.wait_connected(None)).await {
                Ok(Ok(())) => {
                    // 根据 STT/TTS/omni 配置设置 mute/硬件状态
                    {
                        let omni = config.llm.omni_model;
                        let stt = hc.stt.enabled;
                        let tts = hc.tts.enabled;
                        let speaker_on = tts || stt || omni;
                        let mic_on = tts;
                        let cmd = format!(
                            "clientupdate client_input_muted={} client_input_hardware={} client_output_muted={} client_output_hardware={}",
                            if mic_on { 0 } else { 1 },
                            if mic_on { 1 } else { 0 },
                            if speaker_on { 0 } else { 1 },
                            if speaker_on { 1 } else { 0 },
                        );
                        if let Err(e) = client.send_command_no_wait(&cmd).await {
                            warn!("set mute/hardware state failed: {e}");
                        }
                    }

                    // 加入指定频道
                    if !hc.channel_id.is_empty() {
                        let cid = hc.channel_id.trim();
                        if let Ok(cid_u64) = cid.parse::<u64>() {
                            let pw = &hc.channel_password;
                            let clid = client.client_id();
                            info!(cid = cid_u64, "joining channel on startup");
                            if let Err(e) = tsclient_rs::clientMove(&client, clid, cid_u64, pw).await
                            {
                                warn!("join channel failed: {e}");
                            }
                        } else {
                            warn!(
                                channel_id = %hc.channel_id,
                                "invalid channel_id, must be a numeric ID"
                            );
                        }
                    }

                    let clid = client.client_id();
                    let client = Arc::new(client);

                    return Ok(Arc::new(Self {
                        client,
                        event_tx,
                        bot_clid: std::sync::atomic::AtomicU32::new(clid as u32),
                    }));
                }
                Ok(Err(e)) => {
                    let _ = client.disconnect().await;
                    return Err(anyhow!("wait_connected failed: {e:?}"));
                }
                Err(_) => {
                    // handshake 超时，可能等级不够，升级后重试
                    let _ = client.disconnect().await;

                    let next_level = current_level + STEP;
                    if next_level > MAX_LEVEL {
                        return Err(anyhow!(
                            "Server rejected connection at identity level {current_level} (tried max {MAX_LEVEL})"
                        ));
                    }

                    info!(
                        "Handshake timed out at identity level {current_level}, upgrading to {next_level}..."
                    );
                    identity
                        .upgrade_to_level(next_level, None)
                        .await
                        .map_err(|e| anyhow!("identity upgrade failed: {e}"))?;

                    let s = identity.to_string();
                    let _ = std::fs::write(&identity_file, &s);
                    info!("Identity upgraded to level {next_level}");
                    current_level = next_level;
                    // 继续循环重试
                }
            }
        }
    }

    /// 获取共享的 Client（voice 模块使用）
    pub fn get_client(&self) -> &Arc<tsclient_rs::Client> {
        &self.client
    }

    fn load_or_create_identity(
        path: &std::path::Path,
        level: u32,
    ) -> Result<tsclient_rs::Identity> {
        if path.exists() {
            let s = std::fs::read_to_string(path)
                .map_err(|e| anyhow!("read identity file failed: {e}"))?;
            let s = s.trim();
            if !s.is_empty() {
                if let Ok(id) = tsclient_rs::identityFromString(s) {
                    info!("Loaded existing identity");
                    return Ok(id);
                }
            }
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let identity = tsclient_rs::generateIdentity(level as i32);
        let s = identity.to_string();
        if let Err(e) = std::fs::write(path, &s) {
            warn!("Failed to save identity to {}: {e}", path.display());
        }
        info!("Generated new identity at level {level}");
        Ok(identity)
    }

    pub fn get_bot_clid(&self) -> u32 {
        self.bot_clid.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TsEvent> {
        self.event_tx.subscribe()
    }

    pub async fn send_text_message(&self, target_mode: u8, target: u32, msg: &str) -> Result<()> {
        tsclient_rs::sendTextMessage(&self.client, target_mode as i32, target as u64, msg)
            .await
            .map_err(|e| anyhow!("sendTextMessage failed: {e}"))
    }

    pub async fn poke(&self, clid: u32, msg: &str) -> Result<()> {
        tsclient_rs::poke(&self.client, clid as i32, msg)
            .await
            .map_err(|e| anyhow!("poke failed: {e}"))
    }

    pub async fn kick(&self, clid: u32, reason: &str) -> Result<()> {
        tsclient_rs::clientKick(
            &self.client,
            clid as i32,
            tsclient_rs::KickReason::Server,
            reason,
        )
        .await
        .map_err(|e| anyhow!("clientKick failed: {e}"))
    }

    pub async fn ban(&self, clid: u32, time_secs: u64, reason: &str) -> Result<()> {
        tsclient_rs::banClient(&self.client, clid as i32, time_secs, reason)
            .await
            .map_err(|e| anyhow!("banClient failed: {e}"))
    }

    pub async fn move_client(&self, clid: u32, channel_id: u32) -> Result<()> {
        tsclient_rs::clientMove(&self.client, clid as i32, channel_id as u64, "")
            .await
            .map_err(|e| anyhow!("clientMove failed: {e}"))
    }

    pub async fn list_channels(&self) -> Result<Vec<tsclient_rs::ChannelInfo>> {
        tsclient_rs::listChannels(&self.client)
            .await
            .map_err(|e| check_ts_error(e, "listChannels"))
    }

    pub async fn list_clients(&self) -> Result<Vec<tsclient_rs::ClientInfo>> {
        tsclient_rs::listClients(&self.client)
            .await
            .map_err(|e| check_ts_error(e, "listClients"))
    }

    pub async fn get_client_info(
        &self,
        clid: u32,
    ) -> Result<std::collections::HashMap<String, String>> {
        tsclient_rs::getClientInfo(&self.client, clid as i32)
            .await
            .map_err(|e| anyhow!("getClientInfo failed: {e}"))
    }

    pub async fn fetch_client_snapshot(&self) -> Result<Vec<ClientEnterEvent>> {
        let clients = self.list_clients().await?;
        Ok(clients
            .into_iter()
            .map(|c| {
                let groups: Vec<u32> = c
                    .server_groups
                    .iter()
                    .filter_map(|g| g.parse().ok())
                    .collect();
                ClientEnterEvent {
                    clid: c.id as u32,
                    cldbid: 0,
                    client_nickname: c.nickname,
                    client_server_groups: groups,
                    client_channel_group_id: 0,
                    channel_id: c.channel_id as u32,
                }
            })
            .collect())
    }

    pub async fn quit(&self) -> Result<()> {
        self.client
            .disconnect()
            .await
            .map_err(|e| anyhow!("disconnect failed: {e}"))
    }
}

#[derive(Debug, Clone)]
pub enum TsEvent {
    TextMessage(TextMessageEvent),
    ClientEnterView(ClientEnterEvent),
    ClientLeftView(ClientLeftEvent),
}

#[derive(Debug, Clone)]
pub struct TextMessageEvent {
    pub target_mode: TextMessageTarget,
    pub invoker_name: String,
    pub invoker_uid: String,
    pub invoker_id: u32,
    pub invoker_groups: Vec<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TextMessageTarget {
    Private,
    Channel,
    Server,
}

#[derive(Debug, Clone)]
pub struct ClientEnterEvent {
    pub clid: u32,
    pub cldbid: u32,
    pub client_nickname: String,
    pub client_server_groups: Vec<u32>,
    pub client_channel_group_id: u32,
    pub channel_id: u32,
}

#[derive(Debug, Clone)]
pub struct ClientLeftEvent {
    pub clid: u32,
}
