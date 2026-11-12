//! TeamSpeakClaw PluginCommand 开放协议管理：
//! - `tsclient:identity` 身份互认（频道广播与单播回执）
//! - `tsclient:commands` 按需斜杠命令清单发现

use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use tracing::debug;
use tsclient_rs::{PluginCommandEvent, PluginCommandTarget};

use crate::config::AppConfig;

pub const IDENTITY_PLUGIN: &str = "tsclient:identity";
pub const COMMANDS_PLUGIN: &str = "tsclient:commands";

pub const ANNOUNCE_ACK_COOLDOWN: Duration = Duration::from_secs(10);
pub const QUERY_ACK_COOLDOWN: Duration = Duration::from_secs(5);
pub const COMMANDS_ACK_COOLDOWN: Duration = Duration::from_secs(5);
pub const SELF_ANNOUNCE_DEBOUNCE: Duration = Duration::from_secs(2);

pub const MAX_COMMANDS: usize = 12;
#[cfg(test)]
const MAX_COMMAND_NAME_CHARS: usize = 24;
#[cfg(test)]
const MAX_COMMAND_ARGS_CHARS: usize = 32;
#[cfg(test)]
const MAX_COMMAND_DESC_CHARS: usize = 64;

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
pub struct IdentityInboundReq {
    pub v: u32,
    pub action: String,
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommandsInboundReq {
    pub v: u32,
    pub action: String,
}

pub struct OpenProtocolManager {
    client: Mutex<Option<Weak<tsclient_rs::Client>>>,
    self_client_id: AtomicI32,
    capabilities: Vec<String>,
    commands: Vec<BotCommandItem>,
    ack_cooldowns: Mutex<HashMap<String, Instant>>,
    last_announce: Mutex<Option<Instant>>,
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
            ack_cooldowns: Mutex::new(HashMap::new()),
            last_announce: Mutex::new(None),
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
        if config.voice_replay.enabled {
            let mut seen = std::collections::HashSet::new();
            for raw in &config.voice_replay.direct_commands {
                let normalized = raw.trim_start_matches(['!', '/']).trim();
                if !normalized.is_empty() && seen.insert(normalized.to_string()) {
                    cmds.push(BotCommandItem {
                        name: normalized.to_string(),
                        args: Some("[秒数] [@昵称]".into()),
                        desc: "回放最近语音片段".into(),
                    });
                }
            }
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

    pub fn should_announce(&self, now: Instant) -> bool {
        let mut guard = self.last_announce.lock().unwrap();
        if let Some(last) = *guard {
            if now.duration_since(last) < SELF_ANNOUNCE_DEBOUNCE {
                return false;
            }
        }
        *guard = Some(now);
        true
    }

    pub async fn announce_identity(&self) {
        let Some(client) = self.upgrade_client() else {
            return;
        };
        if !self.should_announce(Instant::now()) {
            return;
        }

        let payload = IdentityPayload {
            v: 1,
            action: "announce".into(),
            client_type: "bot".into(),
            app_name: "TeamSpeakClaw".into(),
            app_version: env!("CARGO_PKG_VERSION").into(),
            capabilities: self.capabilities.clone(),
        };

        if let Ok(data) = serde_json::to_string(&payload) {
            if let Err(e) = tsclient_rs::send_plugin_command(
                &client,
                IDENTITY_PLUGIN,
                &data,
                PluginCommandTarget::CurrentChannel,
                None,
            )
            .await
            {
                debug!(error = %e, "Broadcast identity announce failed");
            }
        }
    }

    pub async fn handle_client_moved(&self, clid: i32) {
        let self_id = self.self_client_id.load(Ordering::Relaxed);
        if self_id >= 0 && clid == self_id {
            self.announce_identity().await;
        }
    }

    pub fn should_reply_identity_ack(&self, key: &str, action: &str, now: Instant) -> bool {
        let cooldown = match action {
            "announce" => ANNOUNCE_ACK_COOLDOWN,
            "query" => QUERY_ACK_COOLDOWN,
            _ => return false,
        };
        let mut guard = self.ack_cooldowns.lock().unwrap();
        if let Some(&last) = guard.get(key) {
            if now.duration_since(last) < cooldown {
                return false;
            }
        }
        guard.insert(key.to_string(), now);
        true
    }

    pub fn should_reply_commands_ack(&self, key: &str, now: Instant) -> bool {
        let mut guard = self.ack_cooldowns.lock().unwrap();
        if let Some(&last) = guard.get(key) {
            if now.duration_since(last) < COMMANDS_ACK_COOLDOWN {
                return false;
            }
        }
        guard.insert(key.to_string(), now);
        true
    }

    pub async fn handle_plugin_command(&self, event: PluginCommandEvent) {
        let self_id = self.self_client_id.load(Ordering::Relaxed);
        if self_id >= 0 && event.invoker_id == self_id {
            return;
        }

        match event.name.as_str() {
            IDENTITY_PLUGIN => {
                let Ok(req) = serde_json::from_str::<IdentityInboundReq>(&event.data) else {
                    return;
                };
                if req.action != "announce" && req.action != "query" {
                    return;
                }

                let cooldown_key = if !event.invoker_uid.is_empty() {
                    event.invoker_uid
                } else {
                    event.invoker_id.to_string()
                };

                if !self.should_reply_identity_ack(&cooldown_key, &req.action, Instant::now()) {
                    return;
                }

                let Some(client) = self.upgrade_client() else {
                    return;
                };

                let ack = IdentityPayload {
                    v: 1,
                    action: "announce_ack".into(),
                    client_type: "bot".into(),
                    app_name: "TeamSpeakClaw".into(),
                    app_version: env!("CARGO_PKG_VERSION").into(),
                    capabilities: self.capabilities.clone(),
                };

                if let Ok(data) = serde_json::to_string(&ack) {
                    if let Err(e) = tsclient_rs::send_plugin_command(
                        &client,
                        IDENTITY_PLUGIN,
                        &data,
                        PluginCommandTarget::Client,
                        Some(event.invoker_id),
                    )
                    .await
                    {
                        debug!(error = %e, invoker_id = event.invoker_id, "Send identity announce_ack failed");
                    }
                }
            }
            COMMANDS_PLUGIN => {
                let Ok(req) = serde_json::from_str::<CommandsInboundReq>(&event.data) else {
                    return;
                };
                if req.action != "list_req" {
                    return;
                }

                let cooldown_key = if !event.invoker_uid.is_empty() {
                    event.invoker_uid
                } else {
                    event.invoker_id.to_string()
                };

                if !self.should_reply_commands_ack(&cooldown_key, Instant::now()) {
                    return;
                }

                let Some(client) = self.upgrade_client() else {
                    return;
                };

                let ack = CommandsAckPayload {
                    v: 1,
                    action: "list_ack".into(),
                    commands: self.commands.clone(),
                };

                if let Ok(data) = serde_json::to_string(&ack) {
                    if let Err(e) = tsclient_rs::send_plugin_command(
                        &client,
                        COMMANDS_PLUGIN,
                        &data,
                        PluginCommandTarget::Client,
                        Some(event.invoker_id),
                    )
                    .await
                    {
                        debug!(error = %e, invoker_id = event.invoker_id, "Send commands list_ack failed");
                    }
                }
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
    fn identity_cooldown_distinguishes_announce_and_query() {
        let mut config = AppConfig::default();
        config.voice_replay.enabled = false;
        let manager = OpenProtocolManager::new(&config);

        let t0 = Instant::now();
        // 第一次 announce 回复允许
        assert!(manager.should_reply_identity_ack("uid_1", "announce", t0));
        // 10秒内再次 announce 回复拒绝
        assert!(!manager.should_reply_identity_ack(
            "uid_1",
            "announce",
            t0 + Duration::from_secs(9)
        ));
        // 10秒后 announce 回复允许
        assert!(manager.should_reply_identity_ack(
            "uid_1",
            "announce",
            t0 + Duration::from_secs(11)
        ));

        // query 5秒冷却测试
        let t1 = t0 + Duration::from_secs(20);
        assert!(manager.should_reply_identity_ack("uid_2", "query", t1));
        assert!(!manager.should_reply_identity_ack("uid_2", "query", t1 + Duration::from_secs(4)));
        assert!(manager.should_reply_identity_ack("uid_2", "query", t1 + Duration::from_secs(6)));

        // 未知 action 拒绝
        assert!(!manager.should_reply_identity_ack("uid_3", "unknown", t1));
    }

    #[test]
    fn commands_cooldown_enforces_five_seconds_rate_limit() {
        let config = AppConfig::default();
        let manager = OpenProtocolManager::new(&config);

        let t0 = Instant::now();
        assert!(manager.should_reply_commands_ack("uid_1", t0));
        assert!(!manager.should_reply_commands_ack("uid_1", t0 + Duration::from_secs(3)));
        assert!(manager.should_reply_commands_ack("uid_1", t0 + Duration::from_secs(6)));
    }

    #[test]
    fn self_announce_debounce_prevents_rapid_broadcasts() {
        let config = AppConfig::default();
        let manager = OpenProtocolManager::new(&config);

        let t0 = Instant::now();
        assert!(manager.should_announce(t0));
        assert!(!manager.should_announce(t0 + Duration::from_millis(500)));
        assert!(!manager.should_announce(t0 + Duration::from_secs(1)));
        assert!(manager.should_announce(t0 + Duration::from_millis(2100)));
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

        // voice_replay 开启，使用默认配置 vec!["!replay", "!回放"]
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
        let names: Vec<_> = manager.commands().iter().map(|c| c.name.as_str()).collect();
        // 仅宣称 bot 自身直呼命令，不包含第三方音乐机器人或通用 LLM 技能（如 play, skip, poke）
        assert_eq!(names, vec!["replay", "回放"]);
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
}
