use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::sync::{broadcast, watch};
use tracing::{error, info, warn};

use crate::config::{config_dir, AppConfig};

/// 身份升级最大安全等级
const IDENTITY_MAX_LEVEL: i32 = 29;
/// 每次重试提升的等级步长
const IDENTITY_UPGRADE_STEP: i32 = 5;

fn next_identity_level(current_level: i32) -> Option<i32> {
    (current_level < IDENTITY_MAX_LEVEL)
        .then(|| (current_level + IDENTITY_UPGRADE_STEP).min(IDENTITY_MAX_LEVEL))
}

fn write_identity_file(path: &std::path::Path, serialized: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("identity path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|error| {
        anyhow!(
            "create identity directory {} failed: {error}",
            parent.display()
        )
    })?;
    std::fs::write(path, serialized)
        .map_err(|error| anyhow!("write identity file {} failed: {error}", path.display()))
}

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

fn checked_client_id(clid: u32) -> Result<i32> {
    i32::try_from(clid).map_err(|_| anyhow!("TeamSpeak client ID out of range: {clid}"))
}

fn parse_client_channel_group_id(info: &std::collections::HashMap<String, String>) -> Result<u32> {
    let value = info
        .get("client_channel_group_id")
        .ok_or_else(|| anyhow!("clientinfo missing client_channel_group_id"))?;
    value
        .parse()
        .map_err(|error| anyhow!("invalid client_channel_group_id '{value}': {error}"))
}

/// 封装 tsclient-rs::Client，提供管理命令和事件订阅。
/// 共享的 `Arc<Client>` 可通过 `get_client()` 给 voice 模块使用。
pub struct TsAdapter {
    client: Arc<tsclient_rs::Client>,
    event_tx: broadcast::Sender<TsEvent>,
    bot_clid: std::sync::atomic::AtomicU32,
    main_subscriptions: Mutex<Option<MainSubscriptions>>,
}

struct MainSubscriptions {
    events: broadcast::Receiver<TsEvent>,
    disconnected: watch::Receiver<bool>,
}

impl TsAdapter {
    pub async fn connect(config: Arc<AppConfig>) -> Result<Arc<Self>> {
        let hc = &config.headless;
        let host = &hc.server_address;
        let port = hc.server_port;
        let nickname = &config.bot.nickname;

        let identity_file = config_dir().join("identity.json");
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

            let (event_tx, event_rx) = broadcast::channel::<TsEvent>(256);
            let (disconnect_tx, disconnect_rx) = watch::channel(false);
            Self::register_event_handlers(&client, event_tx.clone(), disconnect_tx);

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
                    let bot_clid = u32::try_from(clid)
                        .map_err(|_| anyhow!("invalid TeamSpeak bot client ID: {clid}"))?;
                    let client = Arc::new(client);

                    let adapter = Arc::new(Self {
                        client,
                        event_tx,
                        bot_clid: std::sync::atomic::AtomicU32::new(bot_clid),
                        main_subscriptions: Mutex::new(Some(MainSubscriptions {
                            events: event_rx,
                            disconnected: disconnect_rx,
                        })),
                    });

                    return Ok(adapter);
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

    fn register_event_handlers(
        client: &tsclient_rs::Client,
        tx: broadcast::Sender<TsEvent>,
        disconnect_tx: watch::Sender<bool>,
    ) {
        {
            let tx = tx.clone();
            client.on_text_message(Arc::new(move |event: tsclient_rs::Event| {
                if let tsclient_rs::Event::TextMessage(ref msg) = event {
                    let target_mode = match msg.target_mode {
                        1 => TextMessageTarget::Private,
                        2 => TextMessageTarget::Channel,
                        3 => TextMessageTarget::Server,
                        mode => {
                            warn!(target_mode = mode, "忽略未知类型的 TeamSpeak 文本消息");
                            return;
                        }
                    };
                    let Ok(invoker_id) = u32::try_from(msg.invoker_id) else {
                        warn!(
                            invoker_id = msg.invoker_id,
                            "忽略调用者 ID 无效的 TeamSpeak 文本消息"
                        );
                        return;
                    };
                    let _ = tx.send(TsEvent::TextMessage(TextMessageEvent {
                        target_mode,
                        invoker_name: msg.invoker_name.clone(),
                        invoker_uid: msg.invoker_uid.clone(),
                        invoker_id,
                        invoker_groups: msg.invoker_groups.clone(),
                        message: msg.message.clone(),
                    }));
                }
            }));
        }

        {
            let tx_dc = tx.clone();
            client.on_disconnected(Arc::new(move |_: tsclient_rs::Event| {
                let _ = tx_dc.send(TsEvent::Disconnected);
                disconnect_tx.send_replace(true);
            }));
        }
    }

    async fn upgrade_identity_and_save(
        identity: &mut tsclient_rs::Identity,
        current_level: i32,
        identity_file: &std::path::Path,
    ) -> Result<i32> {
        let next_level = next_identity_level(current_level).ok_or_else(|| {
            anyhow!(
                "Server rejected connection at identity level {current_level} (tried max {IDENTITY_MAX_LEVEL})"
            )
        })?;

        info!("Upgrading identity to level {next_level} (this may take a few minutes)...");
        identity
            .upgrade_to_level(next_level, None)
            .await
            .map_err(|e| anyhow!("identity upgrade failed: {e}"))?;

        let s = identity.to_string();
        write_identity_file(identity_file, &s)?;
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
        let identity = tsclient_rs::generateIdentity(level as i32);
        let s = identity.to_string();
        write_identity_file(path, &s)?;
        info!("Generated new identity at level {level}");
        Ok(identity)
    }

    pub fn get_bot_clid(&self) -> u32 {
        self.bot_clid.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TsEvent> {
        self.event_tx.subscribe()
    }

    /// 取出连接前创建的主订阅；每个适配器连接只能交给一个主路由。
    pub(crate) fn take_main_subscriptions(
        &self,
    ) -> Result<(broadcast::Receiver<TsEvent>, watch::Receiver<bool>)> {
        let subscriptions = self
            .main_subscriptions
            .lock()
            .map_err(|_| anyhow!("TS main subscription lock poisoned"))?
            .take()
            .ok_or_else(|| anyhow!("TS main subscriptions already taken"))?;
        Ok((subscriptions.events, subscriptions.disconnected))
    }

    pub async fn send_text_message(&self, target_mode: u8, target: u32, msg: &str) -> Result<()> {
        for chunk in super::text_util::split_message(msg, super::text_util::MAX_MESSAGE_BYTES) {
            tsclient_rs::sendTextMessage(&self.client, target_mode as i32, target as u64, &chunk)
                .await
                .map_err(|e| anyhow!("sendTextMessage failed: {e}"))?;
        }
        Ok(())
    }

    pub async fn poke(&self, clid: u32, msg: &str) -> Result<()> {
        tsclient_rs::poke(&self.client, checked_client_id(clid)?, msg)
            .await
            .map_err(|e| anyhow!("poke failed: {e}"))
    }

    pub async fn kick(&self, clid: u32, reason: &str) -> Result<()> {
        tsclient_rs::clientKick(
            &self.client,
            checked_client_id(clid)?,
            tsclient_rs::KickReason::Server,
            reason,
        )
        .await
        .map_err(|e| anyhow!("clientKick failed: {e}"))
    }

    pub async fn ban(&self, clid: u32, time_secs: u64, reason: &str) -> Result<()> {
        tsclient_rs::banClient(&self.client, checked_client_id(clid)?, time_secs, reason)
            .await
            .map_err(|e| anyhow!("banClient failed: {e}"))
    }

    pub async fn move_client(&self, clid: u32, channel_id: u32) -> Result<()> {
        tsclient_rs::clientMove(
            &self.client,
            checked_client_id(clid)?,
            u64::from(channel_id),
            "",
        )
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
        tsclient_rs::getClientInfo(&self.client, checked_client_id(clid)?)
            .await
            .map_err(|e| anyhow!("getClientInfo failed: {e}"))
    }

    pub async fn get_client_channel_group_id(&self, clid: u32) -> Result<u32> {
        let info = self.get_client_info(clid).await?;
        parse_client_channel_group_id(&info)
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
    Disconnected,
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

#[cfg(test)]
mod tests {
    use super::{next_identity_level, parse_client_channel_group_id, write_identity_file};
    use std::collections::HashMap;
    use uuid::Uuid;

    #[test]
    fn parses_nonzero_client_channel_group_id() {
        let info = HashMap::from([("client_channel_group_id".to_string(), "42".to_string())]);

        assert_eq!(parse_client_channel_group_id(&info).unwrap(), 42);
    }

    #[test]
    fn rejects_missing_client_channel_group_id() {
        assert!(parse_client_channel_group_id(&HashMap::new()).is_err());
    }

    #[test]
    fn rejects_invalid_client_channel_group_id() {
        let info = HashMap::from([("client_channel_group_id".to_string(), "invalid".to_string())]);

        assert!(parse_client_channel_group_id(&info).is_err());
    }

    #[test]
    fn identity_upgrade_uses_regular_step() {
        assert_eq!(next_identity_level(8), Some(13));
    }

    #[test]
    fn identity_upgrade_clamps_to_maximum() {
        assert_eq!(next_identity_level(28), Some(29));
    }

    #[test]
    fn identity_upgrade_stops_at_maximum() {
        assert_eq!(next_identity_level(29), None);
    }

    #[test]
    fn identity_write_creates_parent_and_persists_contents() {
        let root = std::env::temp_dir().join(format!("tsclaw-identity-{}", Uuid::new_v4()));
        let path = root.join("nested").join("identity.json");

        write_identity_file(&path, "identity-data").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "identity-data");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn identity_write_reports_directory_creation_error_with_path() {
        let root = std::env::temp_dir().join(format!("tsclaw-identity-{}", Uuid::new_v4()));
        std::fs::write(&root, "not-a-directory").unwrap();
        let path = root.join("identity.json");

        let error = write_identity_file(&path, "identity-data").unwrap_err();

        assert!(error.to_string().contains(&root.display().to_string()));
        std::fs::remove_file(&root).unwrap();
    }

    #[test]
    fn identity_write_reports_file_error_with_path() {
        let root = std::env::temp_dir().join(format!("tsclaw-identity-{}", Uuid::new_v4()));
        let path = root.join("identity.json");
        std::fs::create_dir_all(&path).unwrap();

        let error = write_identity_file(&path, "identity-data").unwrap_err();

        assert!(error.to_string().contains(&path.display().to_string()));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
