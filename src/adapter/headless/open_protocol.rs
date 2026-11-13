//! TeamSpeakClaw PluginCommand 开放协议管理：
//! - `tsclient:identity` 身份互认（频道广播与单播回执）
//! - `tsclient:commands` 按需斜杠命令清单发现

use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use tracing::{debug, warn};
use tsclient_rs::{PluginCommandEvent, PluginCommandTarget};

use crate::config::AppConfig;

pub const PROTOCOL_VERSION: u32 = 1;
pub const IDENTITY_PLUGIN: &str = "tsclient:identity";
pub const COMMANDS_PLUGIN: &str = "tsclient:commands";

pub const ANNOUNCE_ACK_COOLDOWN: Duration = Duration::from_secs(10);
pub const QUERY_ACK_COOLDOWN: Duration = Duration::from_secs(5);
pub const COMMANDS_ACK_COOLDOWN: Duration = Duration::from_secs(5);
pub const SELF_ANNOUNCE_DEBOUNCE: Duration = Duration::from_secs(2);

pub const MAX_COMMANDS: usize = 12;
pub const MAX_COMMAND_NAME_CHARS: usize = 24;
pub const MAX_PAYLOAD_BYTES: usize = 1024;
#[cfg(test)]
const MAX_COMMAND_ARGS_CHARS: usize = 32;
#[cfg(test)]
const MAX_COMMAND_DESC_CHARS: usize = 64;

const ACK_GLOBAL_BURST: f64 = 4.0;
const ACK_GLOBAL_RATE_PER_SEC: f64 = 2.0;

const SELF_ANNOUNCE_RETRY_DELAY: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IdentityPayload {
    pub v: u32,
    pub action: String,
    pub client_type: String,
    pub app_name: String,
    pub app_version: String,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BotCommandItem {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
    pub desc: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommandsAckPayload {
    pub v: u32,
    pub action: String,
    pub commands: Vec<BotCommandItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
struct InboundRequest {
    v: u32,
    action: String,
}

fn parse_inbound_action(data: &str) -> Option<String> {
    let req = serde_json::from_str::<InboundRequest>(data).ok()?;
    if req.v != PROTOCOL_VERSION {
        debug!(
            version = req.v,
            expected = PROTOCOL_VERSION,
            "Ignore unsupported open protocol version"
        );
        return None;
    }
    Some(req.action)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CooldownAction {
    IdentityAnnounce,
    IdentityQuery,
    CommandsList,
}

impl CooldownAction {
    fn cooldown(self) -> Duration {
        match self {
            Self::IdentityAnnounce => ANNOUNCE_ACK_COOLDOWN,
            Self::IdentityQuery => QUERY_ACK_COOLDOWN,
            Self::CommandsList => COMMANDS_ACK_COOLDOWN,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CooldownKey {
    peer: String,
    action: CooldownAction,
}

/// 维护按 `(peer, action)` 隔离的冷却窗口，并用全局令牌桶上限约束出站 ACK 洪峰。
struct AckRateLimiter {
    cooldowns: HashMap<CooldownKey, Instant>,
    tokens: f64,
    last_refill: Instant,
}

impl AckRateLimiter {
    fn new(now: Instant) -> Self {
        Self {
            cooldowns: HashMap::new(),
            tokens: ACK_GLOBAL_BURST,
            last_refill: now,
        }
    }

    fn try_acquire(&mut self, peer: &str, action: CooldownAction, now: Instant) -> bool {
        self.cooldowns
            .retain(|key, &mut last| now.saturating_duration_since(last) < key.action.cooldown());

        let key = CooldownKey {
            peer: peer.to_string(),
            action,
        };
        if let Some(&last) = self.cooldowns.get(&key) {
            if now.saturating_duration_since(last) < action.cooldown() {
                return false;
            }
        }

        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        self.tokens = (self.tokens + elapsed * ACK_GLOBAL_RATE_PER_SEC).min(ACK_GLOBAL_BURST);
        self.last_refill = self.last_refill.max(now);
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;

        self.cooldowns.insert(key, now);
        true
    }

    fn rollback(&mut self, peer: &str, action: CooldownAction, acquired_at: Instant) {
        let key = CooldownKey {
            peer: peer.to_string(),
            action,
        };
        if self.cooldowns.get(&key) == Some(&acquired_at) {
            self.cooldowns.remove(&key);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnnounceDecision {
    SendNow {
        attempt_at: Instant,
        seq_at_start: u64,
    },
    ScheduleTrailing {
        wait: Duration,
    },
    AlreadyScheduled,
}

#[derive(Default)]
struct AnnounceState {
    last_attempt: Option<Instant>,
    trailing_scheduled: bool,
    success_seq: u64,
}

impl AnnounceState {
    fn try_begin(&mut self, now: Instant) -> AnnounceDecision {
        if let Some(last) = self.last_attempt {
            let elapsed = now.saturating_duration_since(last);
            if elapsed < SELF_ANNOUNCE_DEBOUNCE {
                if !self.trailing_scheduled {
                    self.trailing_scheduled = true;
                    return AnnounceDecision::ScheduleTrailing {
                        wait: SELF_ANNOUNCE_DEBOUNCE - elapsed,
                    };
                }
                return AnnounceDecision::AlreadyScheduled;
            }
        }

        self.last_attempt = Some(now);
        self.trailing_scheduled = false;
        AnnounceDecision::SendNow {
            attempt_at: now,
            seq_at_start: self.success_seq,
        }
    }

    fn begin_trailing(&mut self, now: Instant) -> AnnounceDecision {
        self.trailing_scheduled = false;
        self.try_begin(now)
    }

    fn record_success(&mut self) {
        self.success_seq = self.success_seq.wrapping_add(1);
    }

    fn rollback_failure(&mut self, attempt_at: Instant, seq_at_start: u64) -> bool {
        if self.last_attempt == Some(attempt_at) {
            self.last_attempt = None;
        }
        self.success_seq == seq_at_start
    }
}

#[derive(Clone, Copy)]
enum AttemptMode {
    Normal,
    Trailing,
}

struct DelayedAnnounce {
    delay: Duration,
    mode: AttemptMode,
    allow_failure_retry: bool,
    expected_seq: Option<u64>,
}

enum AnnounceStep {
    Done,
    Delayed(DelayedAnnounce),
}

pub struct OpenProtocolManager {
    client: Mutex<Option<Weak<tsclient_rs::Client>>>,
    self_client_id: AtomicI32,
    capabilities: Vec<String>,
    commands: Vec<BotCommandItem>,
    ack_limiter: Mutex<AckRateLimiter>,
    announce_state: Mutex<AnnounceState>,
}

impl OpenProtocolManager {
    pub fn new(config: &AppConfig) -> Self {
        let commands = Self::build_commands(config);
        let capabilities = Self::build_capabilities(!commands.is_empty());

        Self {
            client: Mutex::new(None),
            self_client_id: AtomicI32::new(-1),
            capabilities,
            commands,
            ack_limiter: Mutex::new(AckRateLimiter::new(Instant::now())),
            announce_state: Mutex::new(AnnounceState::default()),
        }
    }

    fn build_capabilities(has_commands: bool) -> Vec<String> {
        let mut caps = Vec::new();
        if has_commands {
            caps.push("chat:slash_commands".to_string());
        }
        caps
    }

    fn build_commands(config: &AppConfig) -> Vec<BotCommandItem> {
        let mut cmds = Vec::new();
        if !config.voice_replay.enabled {
            return cmds;
        }

        for raw in &config.voice_replay.direct_commands {
            let normalized = raw.trim().trim_start_matches(['!', '/']).trim();
            if normalized.is_empty() {
                continue;
            }
            if normalized.chars().count() > MAX_COMMAND_NAME_CHARS {
                warn!(
                    command = normalized,
                    max_chars = MAX_COMMAND_NAME_CHARS,
                    "Skip oversized voice_replay direct command for open protocol"
                );
                continue;
            }

            let candidate = BotCommandItem {
                name: normalized.to_string(),
                args: Some("[秒数] [@昵称]".into()),
                desc: "回放最近语音片段".into(),
            };
            let probe = CommandsAckPayload {
                v: PROTOCOL_VERSION,
                action: "list_ack".into(),
                commands: vec![candidate.clone()],
            };
            let Ok(encoded) = serde_json::to_string(&probe) else {
                continue;
            };
            if encoded.len() >= MAX_PAYLOAD_BYTES {
                warn!(
                    command = normalized,
                    bytes = encoded.len(),
                    max_bytes = MAX_PAYLOAD_BYTES,
                    "Skip voice_replay direct command exceeding open protocol payload limit"
                );
                continue;
            }

            cmds.push(candidate);
            break;
        }

        cmds.truncate(MAX_COMMANDS);
        cmds
    }

    pub fn bind(&self, client: Weak<tsclient_rs::Client>, clid: i32) {
        self.self_client_id.store(clid, Ordering::Relaxed);
        let mut guard = self.client.lock().unwrap();
        *guard = Some(client);
    }

    #[cfg(test)]
    pub fn capabilities(&self) -> &[String] {
        &self.capabilities
    }

    #[cfg(test)]
    pub fn commands(&self) -> &[BotCommandItem] {
        &self.commands
    }

    fn upgrade_client(&self) -> Option<Arc<tsclient_rs::Client>> {
        let guard = self.client.lock().unwrap();
        guard.as_ref().and_then(|w| w.upgrade())
    }

    fn try_begin_announce(&self, mode: AttemptMode, now: Instant) -> AnnounceDecision {
        let mut guard = self.announce_state.lock().unwrap();
        match mode {
            AttemptMode::Normal => guard.try_begin(now),
            AttemptMode::Trailing => guard.begin_trailing(now),
        }
    }

    fn record_announce_success(&self) {
        let mut guard = self.announce_state.lock().unwrap();
        guard.record_success();
    }

    fn rollback_announce_failure(&self, attempt_at: Instant, seq_at_start: u64) -> bool {
        let mut guard = self.announce_state.lock().unwrap();
        guard.rollback_failure(attempt_at, seq_at_start)
    }

    fn should_run_failure_retry(&self, seq_at_start: u64) -> bool {
        let guard = self.announce_state.lock().unwrap();
        guard.success_seq == seq_at_start
    }

    pub async fn announce_identity(self: &Arc<Self>) {
        if let AnnounceStep::Delayed(next) = self.announce_step(AttemptMode::Normal, true).await {
            Self::spawn_delayed_announce(Arc::downgrade(self), next);
        }
    }

    async fn announce_step(&self, mode: AttemptMode, allow_failure_retry: bool) -> AnnounceStep {
        let Some(client) = self.upgrade_client() else {
            return AnnounceStep::Done;
        };

        let now = Instant::now();
        let (attempt_at, seq_at_start) = match self.try_begin_announce(mode, now) {
            AnnounceDecision::SendNow {
                attempt_at,
                seq_at_start,
            } => (attempt_at, seq_at_start),
            AnnounceDecision::ScheduleTrailing { wait } => {
                return AnnounceStep::Delayed(DelayedAnnounce {
                    delay: wait,
                    mode: AttemptMode::Trailing,
                    allow_failure_retry: true,
                    expected_seq: None,
                });
            }
            AnnounceDecision::AlreadyScheduled => return AnnounceStep::Done,
        };

        let payload = IdentityPayload {
            v: PROTOCOL_VERSION,
            action: "announce".into(),
            client_type: "bot".into(),
            app_name: "TeamSpeakClaw".into(),
            app_version: env!("CARGO_PKG_VERSION").into(),
            capabilities: self.capabilities.clone(),
        };

        let Ok(data) = serde_json::to_string(&payload) else {
            return AnnounceStep::Done;
        };
        if data.len() >= MAX_PAYLOAD_BYTES {
            warn!(
                bytes = data.len(),
                max_bytes = MAX_PAYLOAD_BYTES,
                "Skip sending oversized open protocol announce"
            );
            return AnnounceStep::Done;
        }

        match tsclient_rs::send_plugin_command(
            &client,
            IDENTITY_PLUGIN,
            &data,
            PluginCommandTarget::CurrentChannel,
            None,
        )
        .await
        {
            Ok(()) => {
                self.record_announce_success();
                AnnounceStep::Done
            }
            Err(e) => {
                let should_retry = self.rollback_announce_failure(attempt_at, seq_at_start);
                debug!(error = %e, "Broadcast identity announce failed");
                if allow_failure_retry && should_retry {
                    AnnounceStep::Delayed(DelayedAnnounce {
                        delay: SELF_ANNOUNCE_RETRY_DELAY,
                        mode: AttemptMode::Normal,
                        allow_failure_retry: false,
                        expected_seq: Some(seq_at_start),
                    })
                } else {
                    AnnounceStep::Done
                }
            }
        }
    }

    fn spawn_delayed_announce(weak: Weak<Self>, mut next: DelayedAnnounce) {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(next.delay).await;
                let Some(mgr) = weak.upgrade() else {
                    return;
                };
                if let Some(seq) = next.expected_seq {
                    if !mgr.should_run_failure_retry(seq) {
                        return;
                    }
                }
                match mgr.announce_step(next.mode, next.allow_failure_retry).await {
                    AnnounceStep::Done => return,
                    AnnounceStep::Delayed(following) => next = following,
                }
            }
        });
    }

    pub async fn handle_client_moved(self: &Arc<Self>, clid: i32) {
        let self_id = self.self_client_id.load(Ordering::Relaxed);
        if self_id >= 0 && clid == self_id {
            self.announce_identity().await;
        }
    }

    pub(crate) fn try_acquire_ack(&self, peer: &str, action: CooldownAction, now: Instant) -> bool {
        let mut guard = self.ack_limiter.lock().unwrap();
        guard.try_acquire(peer, action, now)
    }

    fn rollback_ack(&self, peer: &str, action: CooldownAction, acquired_at: Instant) {
        let mut guard = self.ack_limiter.lock().unwrap();
        guard.rollback(peer, action, acquired_at);
    }

    async fn send_ack<T: serde::Serialize>(
        &self,
        plugin: &str,
        peer: &str,
        invoker_id: i32,
        action: CooldownAction,
        payload: &T,
    ) {
        let Ok(data) = serde_json::to_string(payload) else {
            return;
        };
        if data.len() >= MAX_PAYLOAD_BYTES {
            warn!(
                plugin,
                bytes = data.len(),
                max_bytes = MAX_PAYLOAD_BYTES,
                "Skip sending oversized open protocol ack"
            );
            return;
        }
        let Some(client) = self.upgrade_client() else {
            return;
        };
        let now = Instant::now();
        if !self.try_acquire_ack(peer, action, now) {
            return;
        }

        if let Err(e) = tsclient_rs::send_plugin_command(
            &client,
            plugin,
            &data,
            PluginCommandTarget::Client,
            Some(invoker_id),
        )
        .await
        {
            self.rollback_ack(peer, action, now);
            debug!(
                error = %e,
                plugin,
                invoker_id,
                "Send open protocol ack failed"
            );
        }
    }

    pub async fn handle_plugin_command(&self, event: PluginCommandEvent) {
        let self_id = self.self_client_id.load(Ordering::Relaxed);
        if self_id >= 0 && event.invoker_id == self_id {
            return;
        }

        let peer_fallback;
        let peer = if !event.invoker_uid.is_empty() {
            event.invoker_uid.as_str()
        } else {
            peer_fallback = event.invoker_id.to_string();
            peer_fallback.as_str()
        };

        match event.name.as_str() {
            IDENTITY_PLUGIN => {
                let Some(action_str) = parse_inbound_action(&event.data) else {
                    return;
                };
                let action = match action_str.as_str() {
                    "announce" => CooldownAction::IdentityAnnounce,
                    "query" => CooldownAction::IdentityQuery,
                    _ => return,
                };
                let ack = IdentityPayload {
                    v: PROTOCOL_VERSION,
                    action: "announce_ack".into(),
                    client_type: "bot".into(),
                    app_name: "TeamSpeakClaw".into(),
                    app_version: env!("CARGO_PKG_VERSION").into(),
                    capabilities: self.capabilities.clone(),
                };
                self.send_ack(IDENTITY_PLUGIN, peer, event.invoker_id, action, &ack)
                    .await;
            }
            COMMANDS_PLUGIN => {
                let Some(action_str) = parse_inbound_action(&event.data) else {
                    return;
                };
                if action_str != "list_req" {
                    return;
                }
                let ack = CommandsAckPayload {
                    v: PROTOCOL_VERSION,
                    action: "list_ack".into(),
                    commands: self.commands.clone(),
                };
                self.send_ack(
                    COMMANDS_PLUGIN,
                    peer,
                    event.invoker_id,
                    CooldownAction::CommandsList,
                    &ack,
                )
                .await;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_announce_payload_serializes_with_correct_structure() {
        let payload = IdentityPayload {
            v: 1,
            action: "announce".into(),
            client_type: "bot".into(),
            app_name: "TeamSpeakClaw".into(),
            app_version: "1.2.3".into(),
            capabilities: vec!["chat:slash_commands".into()],
        };

        let json = serde_json::to_string(&payload).expect("serialize identity");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse json");

        assert_eq!(parsed["v"], 1);
        assert_eq!(parsed["action"], "announce");
        assert_eq!(parsed["client_type"], "bot");
        assert_eq!(parsed["app_name"], "TeamSpeakClaw");
        assert_eq!(parsed["app_version"], "1.2.3");
        assert_eq!(
            parsed["capabilities"],
            serde_json::json!(["chat:slash_commands"])
        );
    }

    #[test]
    fn commands_ack_payload_omits_none_args_and_keeps_size_well_below_packet_limit() {
        let payload = CommandsAckPayload {
            v: 1,
            action: "list_ack".into(),
            commands: vec![
                BotCommandItem {
                    name: "replay".into(),
                    args: Some("[秒数] [@昵称]".into()),
                    desc: "回放最近语音片段".into(),
                },
                BotCommandItem {
                    name: "skip".into(),
                    args: None,
                    desc: "跳过当前播放曲目".into(),
                },
            ],
        };

        let json = serde_json::to_string(&payload).expect("serialize commands");
        assert!(json.len() < 1024);
        assert!(!json.contains("\"args\":null"));

        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse json");
        assert_eq!(parsed["v"], 1);
        assert_eq!(parsed["action"], "list_ack");
        assert_eq!(parsed["commands"][0]["name"], "replay");
        assert_eq!(parsed["commands"][0]["args"], "[秒数] [@昵称]");
        assert_eq!(parsed["commands"][1]["name"], "skip");
        assert!(parsed["commands"][1].get("args").is_none());
    }

    #[test]
    fn identity_and_commands_cooldowns_are_isolated_per_peer_and_action() {
        let mut config = AppConfig::default();
        config.voice_replay.enabled = false;
        let manager = OpenProtocolManager::new(&config);

        let t0 = Instant::now();
        // 同一 peer 的 announce 不阻塞紧随其后的 list_req 与 query
        assert!(manager.try_acquire_ack("uid_1", CooldownAction::IdentityAnnounce, t0));
        assert!(manager.try_acquire_ack("uid_1", CooldownAction::CommandsList, t0));
        assert!(manager.try_acquire_ack("uid_1", CooldownAction::IdentityQuery, t0));

        // 冷却期内同 action 拒绝，到期后恢复
        assert!(!manager.try_acquire_ack(
            "uid_1",
            CooldownAction::IdentityAnnounce,
            t0 + Duration::from_secs(9)
        ));
        assert!(manager.try_acquire_ack(
            "uid_1",
            CooldownAction::IdentityAnnounce,
            t0 + Duration::from_secs(11)
        ));

        let t1 = t0 + Duration::from_secs(20);
        assert!(manager.try_acquire_ack("uid_2", CooldownAction::IdentityQuery, t1));
        assert!(!manager.try_acquire_ack(
            "uid_2",
            CooldownAction::IdentityQuery,
            t1 + Duration::from_secs(4)
        ));
        assert!(manager.try_acquire_ack(
            "uid_2",
            CooldownAction::IdentityQuery,
            t1 + Duration::from_secs(6)
        ));

        let t2 = t1 + Duration::from_secs(10);
        assert!(manager.try_acquire_ack("uid_3", CooldownAction::CommandsList, t2));
        assert!(!manager.try_acquire_ack(
            "uid_3",
            CooldownAction::CommandsList,
            t2 + Duration::from_secs(3)
        ));
        assert!(manager.try_acquire_ack(
            "uid_3",
            CooldownAction::CommandsList,
            t2 + Duration::from_secs(6)
        ));
    }

    #[test]
    fn global_ack_budget_limits_burst_and_preserves_rejected_peer_cooldown() {
        let config = AppConfig::default();
        let manager = OpenProtocolManager::new(&config);

        let t0 = Instant::now();
        // 突发上限 4 次放行，第 5 个 peer 被全局限流拒绝
        assert!(manager.try_acquire_ack("uid_1", CooldownAction::IdentityAnnounce, t0));
        assert!(manager.try_acquire_ack("uid_2", CooldownAction::IdentityAnnounce, t0));
        assert!(manager.try_acquire_ack("uid_3", CooldownAction::IdentityAnnounce, t0));
        assert!(manager.try_acquire_ack("uid_4", CooldownAction::IdentityAnnounce, t0));
        assert!(!manager.try_acquire_ack("uid_5", CooldownAction::IdentityAnnounce, t0));

        // 0.6 秒补充约 1.2 个令牌：未被记入冷却的 uid_5 可立即补位通过，仍在冷却的 uid_1 仍被拒绝
        let t1 = t0 + Duration::from_millis(600);
        assert!(!manager.try_acquire_ack("uid_1", CooldownAction::IdentityAnnounce, t1));
        assert!(manager.try_acquire_ack("uid_5", CooldownAction::IdentityAnnounce, t1));
    }

    #[test]
    fn expired_cooldown_entries_are_pruned_and_failure_rollback_allows_retry() {
        let config = AppConfig::default();
        let manager = OpenProtocolManager::new(&config);

        let t0 = Instant::now();
        assert!(manager.try_acquire_ack("uid_1", CooldownAction::IdentityQuery, t0));
        // 发送失败回滚后无需等待 5 秒冷却
        manager.rollback_ack("uid_1", CooldownAction::IdentityQuery, t0);
        assert!(manager.try_acquire_ack(
            "uid_1",
            CooldownAction::IdentityQuery,
            t0 + Duration::from_millis(100)
        ));

        // 11 秒后旧记录被清理
        let t_later = t0 + Duration::from_secs(11);
        assert!(manager.try_acquire_ack("uid_fresh", CooldownAction::CommandsList, t_later));
        let limiter = manager.ack_limiter.lock().unwrap();
        assert_eq!(limiter.cooldowns.len(), 1);
        assert!(limiter.cooldowns.contains_key(&CooldownKey {
            peer: "uid_fresh".to_string(),
            action: CooldownAction::CommandsList,
        }));
    }

    #[test]
    fn parse_inbound_action_enforces_protocol_version() {
        assert_eq!(
            parse_inbound_action(r#"{"v":1,"action":"announce"}"#),
            Some("announce".to_string())
        );
        assert_eq!(parse_inbound_action(r#"{"v":2,"action":"announce"}"#), None);
        assert_eq!(parse_inbound_action(r#"{"v":0,"action":"list_req"}"#), None);
        assert_eq!(parse_inbound_action(r#"{"action":"announce"}"#), None);
    }

    #[test]
    fn announce_state_schedules_trailing_and_rolls_back_on_failure() {
        let config = AppConfig::default();
        let manager = OpenProtocolManager::new(&config);

        let t0 = Instant::now();
        let first = manager.try_begin_announce(AttemptMode::Normal, t0);
        assert_eq!(
            first,
            AnnounceDecision::SendNow {
                attempt_at: t0,
                seq_at_start: 0,
            }
        );

        // 去抖窗口内仅调度一次尾随广播
        let trailing =
            manager.try_begin_announce(AttemptMode::Normal, t0 + Duration::from_millis(500));
        assert_eq!(
            trailing,
            AnnounceDecision::ScheduleTrailing {
                wait: Duration::from_millis(1500),
            }
        );
        let dedup = manager.try_begin_announce(AttemptMode::Normal, t0 + Duration::from_secs(1));
        assert_eq!(dedup, AnnounceDecision::AlreadyScheduled);

        // 尾随触发时若已过 2 秒则立即发送
        let fire_trailing =
            manager.try_begin_announce(AttemptMode::Trailing, t0 + Duration::from_secs(2));
        assert_eq!(
            fire_trailing,
            AnnounceDecision::SendNow {
                attempt_at: t0 + Duration::from_secs(2),
                seq_at_start: 0,
            }
        );

        // 发送失败回滚 last_attempt，允许重试且未成功前可触发单次重试
        assert!(manager.rollback_announce_failure(t0 + Duration::from_secs(2), 0));
        assert!(manager.should_run_failure_retry(0));

        let retry =
            manager.try_begin_announce(AttemptMode::Normal, t0 + Duration::from_millis(2100));
        assert_eq!(
            retry,
            AnnounceDecision::SendNow {
                attempt_at: t0 + Duration::from_millis(2100),
                seq_at_start: 0,
            }
        );
        manager.record_announce_success();
        assert!(!manager.should_run_failure_retry(0));
    }

    #[test]
    fn build_commands_skips_oversized_entries_and_respects_payload_limit() {
        let mut config = AppConfig::default();
        config.voice_replay.enabled = true;
        config.voice_replay.direct_commands = vec![
            format!("!{}", "a".repeat(MAX_COMMAND_NAME_CHARS + 5)),
            "!replay".into(),
        ];

        let manager = OpenProtocolManager::new(&config);
        let names: Vec<_> = manager.commands().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["replay"]);

        let ack = CommandsAckPayload {
            v: PROTOCOL_VERSION,
            action: "list_ack".into(),
            commands: manager.commands().to_vec(),
        };
        let encoded = serde_json::to_string(&ack).expect("serialize ack");
        assert!(encoded.len() < MAX_PAYLOAD_BYTES);
    }

    #[test]
    fn capabilities_and_commands_reflect_config() {
        let mut config = AppConfig::default();
        config.headless.stt.enabled = false;
        config.headless.tts.enabled = false;
        config.llm.omni_model = false;
        config.voice_replay.enabled = false;
        config.music_backend = None;

        let manager = OpenProtocolManager::new(&config);
        // voice_replay 关闭时无直呼命令，不宣称 chat:slash_commands
        assert!(manager.capabilities().is_empty());
        assert!(manager.commands().is_empty());

        // voice_replay 开启，使用默认配置
        config.voice_replay.enabled = true;
        config.music_backend = Some(crate::config::MusicBackendConfig {
            backend: "ts3audiobot".to_string(),
            base_url: String::new(),
            musicbot_name: String::new(),
        });
        let manager = OpenProtocolManager::new(&config);
        assert!(manager
            .capabilities()
            .contains(&"chat:slash_commands".to_string()));
        assert!(!manager.capabilities().contains(&"audio:stereo".to_string()));
        let expected_primary = config
            .voice_replay
            .direct_commands
            .iter()
            .map(|raw| raw.trim().trim_start_matches(['!', '/']).trim())
            .find(|s| !s.is_empty())
            .expect("default config has a primary direct command");
        let names: Vec<_> = manager.commands().iter().map(|c| c.name.as_str()).collect();
        // 仅宣称 bot 自身首个有效直呼主命令，不重复宣称别名，也不包含第三方音乐机器人或通用 LLM 技能（如 play, skip, poke）
        assert_eq!(names, vec![expected_primary]);
        assert!(!names.contains(&"poke"));
        assert!(!names.contains(&"play"));
        assert!(!names.contains(&"skip"));
        assert!(!names.contains(&"stop"));
        assert!(!names.contains(&"volume"));

        for cmd in manager.commands() {
            assert!(cmd.name.chars().count() <= MAX_COMMAND_NAME_CHARS);
            if let Some(args) = &cmd.args {
                assert!(args.chars().count() <= MAX_COMMAND_ARGS_CHARS);
            }
            assert!(cmd.desc.chars().count() <= MAX_COMMAND_DESC_CHARS);
        }
    }

    #[test]
    fn commands_pick_first_non_empty_entry_from_config() {
        let mut config = AppConfig::default();
        config.voice_replay.enabled = true;
        config.voice_replay.direct_commands =
            vec!["!".into(), "  /  ".into(), "!回放".into(), "!replay".into()];

        let manager = OpenProtocolManager::new(&config);
        let names: Vec<_> = manager.commands().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["回放"]);
        assert!(manager
            .capabilities()
            .contains(&"chat:slash_commands".to_string()));

        config.voice_replay.direct_commands = vec!["!".into(), "/".into()];
        let empty_manager = OpenProtocolManager::new(&config);
        assert!(empty_manager.commands().is_empty());
        assert!(empty_manager.capabilities().is_empty());
    }
}
