//! TeamSpeakClaw OpenProtocol 配置与挂载入口（传输、限流与防抖由 `tsclient-rs::open_protocol` 托管）。

use tracing::{debug, warn};
use tsclient_rs::open_protocol::{
    fits_wire_budget, BotCommand, BotCommandList, ClientKind, OpenProtocolConfig,
    OpenProtocolEvent, OpenProtocolHandle, PeerIdentity, COMMANDS_PLUGIN, MAX_COMMAND_NAME_CHARS,
    MAX_PAYLOAD_BYTES,
};
use tsclient_rs::PluginCommandTarget;

use crate::config::AppConfig;

const REPLAY_COMMAND_ARGS: &str = "[秒数] [@昵称]";
const REPLAY_COMMAND_DESC: &str = "回放最近语音片段";

fn split_single_prefix_command(raw: &str) -> Option<(&str, &str)> {
    let trimmed = raw.trim();
    let first = trimmed.chars().next()?;
    if !first.is_ascii_punctuation() {
        return None;
    }
    let (prefix, name) = trimmed.split_at(first.len_utf8());
    let first_name_char = name.chars().next()?;
    if !first_name_char.is_alphanumeric() || name.chars().any(char::is_whitespace) {
        return None;
    }
    Some((prefix, name))
}

fn build_direct_command_list(config: &AppConfig) -> BotCommandList {
    if !config.voice_replay.enabled {
        return BotCommandList::default();
    }

    for raw in &config.voice_replay.direct_commands {
        let trimmed = raw.trim();
        let Some(first) = trimmed.chars().next() else {
            continue;
        };
        if first.is_ascii_punctuation() && trimmed.len() == first.len_utf8() {
            continue;
        }
        let Some((prefix, normalized)) = split_single_prefix_command(trimmed) else {
            warn!(
                command = trimmed,
                "Skip unsupported voice_replay direct command for open protocol"
            );
            continue;
        };
        if normalized.chars().count() > MAX_COMMAND_NAME_CHARS {
            warn!(
                command = normalized,
                max_chars = MAX_COMMAND_NAME_CHARS,
                "Skip oversized voice_replay direct command for open protocol"
            );
            continue;
        }

        let candidate =
            BotCommand::new(normalized, REPLAY_COMMAND_DESC).with_args(REPLAY_COMMAND_ARGS);
        let list = BotCommandList::new(vec![candidate]).with_prefix(prefix);
        let probe_json = serde_json::json!({
            "v": 1,
            "action": "list_ack",
            "command_prefix": prefix,
            "commands": [{
                "name": normalized,
                "args": REPLAY_COMMAND_ARGS,
                "desc": REPLAY_COMMAND_DESC,
            }],
        })
        .to_string();
        if !fits_wire_budget(
            COMMANDS_PLUGIN,
            &probe_json,
            PluginCommandTarget::Client,
            Some(u16::MAX as i32),
        ) {
            warn!(
                command = normalized,
                bytes = probe_json.len(),
                max_bytes = MAX_PAYLOAD_BYTES,
                "Skip voice_replay direct command exceeding open protocol wire budget"
            );
            continue;
        }

        return list;
    }

    BotCommandList::default()
}

pub fn build_open_protocol_config(config: &AppConfig) -> OpenProtocolConfig {
    let identity = PeerIdentity::new(ClientKind::Bot, "TeamSpeakClaw", env!("CARGO_PKG_VERSION"));
    OpenProtocolConfig::new(identity)
        .with_commands(build_direct_command_list(config))
        .with_cache_peers(false)
        .with_server_announce_on_join(true)
}

pub fn attach_open_protocol(client: &tsclient_rs::Client, config: &AppConfig) {
    let handle = OpenProtocolHandle::attach(client, build_open_protocol_config(config));
    let mut rx = handle.subscribe();
    tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            if let OpenProtocolEvent::SendFailed {
                plugin,
                target_id,
                error,
                ..
            } = event
            {
                debug!(plugin, target_id, error = %error, "OpenProtocol send failed");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use tsclient_rs::open_protocol::{
        OpenProtocolEngine, CAP_CHAT_SLASH_COMMANDS, COMMANDS_PLUGIN, MAX_COMMAND_ARGS_CHARS,
        MAX_COMMAND_DESC_CHARS,
    };
    use tsclient_rs::PluginCommandEvent;

    use super::*;

    #[test]
    fn capabilities_and_commands_reflect_config() {
        let mut config = AppConfig::default();
        config.headless.stt.enabled = false;
        config.headless.tts.enabled = false;
        config.llm.omni_model = false;
        config.voice_replay.enabled = false;
        config.music_backend = None;

        let t0 = Instant::now();
        let disabled = OpenProtocolEngine::new(build_open_protocol_config(&config), t0);
        assert!(disabled.effective_identity().capabilities.is_empty());
        assert!(disabled
            .local_commands()
            .is_some_and(|l| l.commands.is_empty()));

        config.voice_replay.enabled = true;
        config.music_backend = Some(crate::config::MusicBackendConfig {
            backend: "ts3audiobot".to_string(),
            base_url: String::new(),
            musicbot_name: String::new(),
        });
        let enabled = OpenProtocolEngine::new(build_open_protocol_config(&config), t0);
        assert!(enabled
            .effective_identity()
            .has_capability(CAP_CHAT_SLASH_COMMANDS));
        assert!(!enabled.effective_identity().has_capability("audio:stereo"));

        let (expected_prefix, expected_primary) = config
            .voice_replay
            .direct_commands
            .iter()
            .filter_map(|raw| split_single_prefix_command(raw))
            .next()
            .expect("default config has a primary direct command");
        let list = enabled.local_commands().expect("commands configured");
        let names: Vec<_> = list.commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec![expected_primary]);
        assert_eq!(list.command_prefix.as_deref(), Some(expected_prefix));
        for cmd in &list.commands {
            assert!(cmd.name.chars().count() <= MAX_COMMAND_NAME_CHARS);
            if let Some(args) = &cmd.args {
                assert!(args.chars().count() <= MAX_COMMAND_ARGS_CHARS);
            }
            assert!(cmd.desc.chars().count() <= MAX_COMMAND_DESC_CHARS);
        }
    }

    #[test]
    fn commands_pick_first_non_empty_and_skip_invalid_entries() {
        let mut config = AppConfig::default();
        config.voice_replay.enabled = true;
        config.voice_replay.direct_commands =
            vec!["!".into(), "  /  ".into(), "!回放".into(), "!replay".into()];
        let list = build_direct_command_list(&config);
        assert_eq!(list.command_prefix.as_deref(), Some("!"));
        assert_eq!(
            list.commands
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["回放"]
        );

        config.voice_replay.direct_commands = vec![
            "replay".into(),
            "! replay".into(),
            "!!replay".into(),
            "!!!!!!!!!replay".into(),
            "!replay extra".into(),
            format!("!{}", "a".repeat(MAX_COMMAND_NAME_CHARS + 5)),
        ];
        let rejected = build_direct_command_list(&config);
        assert!(rejected.commands.is_empty());
        assert_eq!(rejected.command_prefix, None);

        config.voice_replay.direct_commands =
            vec!["replay".into(), "! replay".into(), "?回放".into()];
        let custom = build_direct_command_list(&config);
        let prefix = custom.command_prefix.as_deref().expect("prefix");
        let name = custom.commands[0].name.as_str();
        assert_eq!(prefix, "?");
        assert_eq!(name, "回放");
        assert!(crate::tools::voice_replay::parse_direct_command(
            &format!("{prefix}{name}"),
            &config.voice_replay.direct_commands,
        )
        .is_some());
    }

    #[test]
    fn list_req_emits_prefix_when_enabled_and_empty_when_disabled() {
        let mut config = AppConfig::default();
        config.voice_replay.enabled = true;
        config.voice_replay.direct_commands = vec!["!replay".into()];

        let t0 = Instant::now();
        let mut engine = OpenProtocolEngine::new(build_open_protocol_config(&config), t0);
        let connect_eff = engine.on_connected(1, 1, t0);
        assert_eq!(connect_eff.outbound.len(), 2);
        assert_eq!(
            connect_eff.outbound[1].target,
            tsclient_rs::PluginCommandTarget::Server
        );
        assert!(connect_eff.outbound[1].data.contains(r#""noreply":true"#));

        let req = PluginCommandEvent {
            name: COMMANDS_PLUGIN.to_string(),
            data: r#"{"v":1,"action":"list_req"}"#.to_string(),
            invoker_id: 42,
            invoker_name: "Alice".to_string(),
            invoker_uid: "uid_alice".to_string(),
        };
        let eff = engine.on_plugin_command(&req, None, t0);
        assert_eq!(eff.outbound.len(), 1);
        let parsed: serde_json::Value =
            serde_json::from_str(&eff.outbound[0].data).expect("parse list_ack");
        assert_eq!(parsed["v"], 1);
        assert_eq!(parsed["action"], "list_ack");
        assert_eq!(parsed["command_prefix"], "!");
        assert_eq!(parsed["commands"][0]["name"], "replay");

        config.voice_replay.enabled = false;
        let mut disabled = OpenProtocolEngine::new(build_open_protocol_config(&config), t0);
        disabled.on_connected(1, 1, t0);
        let disabled_eff = disabled.on_plugin_command(&req, None, t0);
        assert_eq!(disabled_eff.outbound.len(), 1);
        let disabled_parsed: serde_json::Value =
            serde_json::from_str(&disabled_eff.outbound[0].data).expect("parse disabled list_ack");
        assert!(disabled_parsed.get("command_prefix").is_none());
        assert_eq!(disabled_parsed["commands"], serde_json::json!([]));
    }
}
