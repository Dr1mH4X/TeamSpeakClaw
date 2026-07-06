use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::sync::broadcast;
use tracing::{error, info, warn};

use crate::config::AppConfig;

/// 身份升级最大安全等级
const IDENTITY_MAX_LEVEL: i32 = 29;
/// 每次重试提升的等级步长
const IDENTITY_UPGRADE_STEP: i32 = 5;

// 全局广播发送器，用于音频事件
static GLOBAL_AUDIO_TX: std::sync::OnceLock<broadcast::Sender<TsEvent>> =
    std::sync::OnceLock::new();

/// 检查 TS 错误，如果是权限问题则额外提示用户。
fn check_ts_error(err: tsclient_rs::Error, op: &str) -> anyhow::Error {
    let is_perm = matches!(&err,
        tsclient_rs::Error::ServerError { id, .. }
        if id == "2568" || id == "2569" || id.contains("permission") || id.contains("insufficient")
    );
    if is_perm {
        error!("{op} failed: insufficient permissions. Grant the bot Server Admin permissions");
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
    pub async fn connect(config: Arc<AppConfig>, identity_file: PathBuf) -> Result<Arc<Self>> {
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

        let mut current_level = identity.security_level();
        const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(8);

        loop {
            let opts = make_opts();
            let mut client =
                tsclient_rs::Client::new(identity.clone(), addr.clone(), nickname.clone(), opts);

            let (event_tx, _) = broadcast::channel::<TsEvent>(256);
            Self::register_event_handlers(&client, event_tx.clone());

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
                            if let Err(e) =
                                tsclient_rs::clientMove(&client, clid, cid_u64, pw).await
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

                    // 初始化全局广播发送器
                    let _ = GLOBAL_AUDIO_TX.set(event_tx.clone());

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
                    let _ = client.disconnect().await;
                    current_level = Self::upgrade_identity_and_save(
                        &mut identity,
                        current_level,
                        &identity_file,
                    )
                    .await?;
                }
            }
        }
    }

    fn register_event_handlers(client: &tsclient_rs::Client, tx: broadcast::Sender<TsEvent>) {
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

        // 监听客户端离开事件
        {
            let tx = tx.clone();
            client.on_client_leave(Arc::new(move |event: tsclient_rs::Event| {
                if let tsclient_rs::Event::ClientLeave(ref left) = event {
                    let _ = tx.send(TsEvent::ClientLeave(ClientLeaveEvent {
                        client_id: left.id as u32,
                    }));
                }
            }));
        }
    }

    async fn upgrade_identity_and_save(
        identity: &mut tsclient_rs::Identity,
        current_level: i32,
        identity_file: &std::path::Path,
    ) -> Result<i32> {
        let next_level = current_level + IDENTITY_UPGRADE_STEP;
        if next_level > IDENTITY_MAX_LEVEL {
            return Err(anyhow!(
                "Server rejected connection at identity level {current_level} (tried max {IDENTITY_MAX_LEVEL})"
            ));
        }

        info!("Upgrading identity to level {next_level} (this may take a few minutes)...");
        identity
            .upgrade_to_level(next_level, None)
            .await
            .map_err(|e| anyhow!("identity upgrade failed: {e}"))?;

        let s = identity.to_string();
        let _ = std::fs::write(identity_file, &s);
        info!("Identity upgraded to level {next_level}");
        Ok(next_level)
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

    /// 订阅全局音频事件（用于MeetingSummary等跨模块监听）
    pub fn subscribe_global() -> broadcast::Receiver<TsEvent> {
        GLOBAL_AUDIO_TX
            .get()
            .expect("全局广播发送器未初始化")
            .subscribe()
    }

    pub fn broadcast_audio_frame(&self, data: AudioFrameData) {
        if let Some(global_tx) = GLOBAL_AUDIO_TX.get() {
            let _ = global_tx.send(TsEvent::AudioFrame(data));
        }
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
    AudioFrame(AudioFrameData),
    ClientLeave(ClientLeaveEvent),
}

#[derive(Debug, Clone)]
pub struct AudioFrameData {
    pub from_client_id: u32,
    pub from_client_name: String,
    pub frame: Vec<u8>,
    pub codec: i32,
}

#[derive(Debug, Clone)]
pub struct ClientLeaveEvent {
    pub client_id: u32,
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
