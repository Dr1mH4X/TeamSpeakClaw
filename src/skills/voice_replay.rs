//! voice_replay：分说话人环形窗回放 / status / stop。

use crate::adapter::headless::audio_output::PcmClipPayload;
use crate::adapter::headless::speaker_ring::{NameResolve, ReplayFilter};
use crate::config::AppConfig;
use crate::skills::voice_audio::{VoiceAudioHandles, VoiceAudioRuntime};
use crate::skills::{Platform, Skill, UnifiedExecutionContext};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

const SAMPLE_RATE_HZ: u64 = 48_000;
const CHANNELS: u64 = 2;

pub struct VoiceReplay {
    config: Arc<AppConfig>,
    handles: VoiceAudioHandles,
}

impl VoiceReplay {
    pub fn new(config: Arc<AppConfig>, handles: VoiceAudioHandles) -> Self {
        Self { config, handles }
    }

    fn runtime(&self) -> Result<VoiceAudioRuntime> {
        self.handles
            .get()
            .ok_or_else(|| anyhow!("voice replay runtime not ready"))
    }
}

fn speakers_json(runtime: &VoiceAudioRuntime) -> Value {
    let stats = runtime.speaker_rings.stats();
    Value::Array(
        stats
            .iter()
            .map(|s| {
                json!({
                    "clid": s.clid,
                    "name": s.name,
                    "active_ms": s.active_ms,
                })
            })
            .collect(),
    )
}

fn playback_duration_ms(sample_count: usize) -> u64 {
    sample_count as u64 * 1000 / (SAMPLE_RATE_HZ * CHANNELS)
}

fn resolve_speaker_filter(runtime: &VoiceAudioRuntime, name: &str) -> Result<Option<ReplayFilter>> {
    if name.is_empty() {
        return Ok(None);
    }
    match runtime.speaker_rings.resolve_name(name) {
        NameResolve::Unique(clid) => Ok(Some(ReplayFilter::Speaker { clid })),
        NameResolve::Ambiguous(candidates) => {
            let names: Vec<&str> = candidates.iter().map(|s| s.as_str()).collect();
            Err(anyhow!(
                "speaker name '{}' is ambiguous; candidates: {}",
                name,
                names.join(", ")
            ))
        }
        NameResolve::None => {
            let stats = runtime.speaker_rings.stats();
            let names: Vec<&str> = stats.iter().map(|s| s.name.as_str()).collect();
            Err(anyhow!(
                "speaker '{}' not found in recording window; candidates: {}",
                name,
                names.join(", ")
            ))
        }
    }
}

fn execute_replay_with_filter(
    runtime: &VoiceAudioRuntime,
    seconds: Option<u32>,
    filter: ReplayFilter,
) -> Result<Value> {
    let seconds = seconds.map(|s| s.min(runtime.window_secs));
    let snapshot = runtime.speaker_rings.snapshot(filter, seconds);
    let duration_ms = playback_duration_ms(snapshot.samples.len());
    runtime
        .audio_output
        .enqueue_pcm_clip(PcmClipPayload {
            samples: snapshot.samples.clone(),
            sample_rate: snapshot.sample_rate,
            channels: snapshot.channels,
        })
        .map_err(|error| anyhow!("enqueue replay clip failed: {error}"))?;

    Ok(json!({
        "status": "queued",
        "buffered_ms": snapshot.buffered_ms,
        "playback_duration_ms": duration_ms,
        "speakers": Value::Array(
            snapshot.speakers.iter().map(|s| json!({
                "clid": s.clid,
                "name": s.name,
                "active_ms": s.active_ms,
            })).collect()
        ),
    }))
}

fn execute_replay(runtime: &VoiceAudioRuntime, args: &Value) -> Result<Value> {
    let requested = args
        .get("seconds")
        .and_then(Value::as_u64)
        .map(|s| u32::try_from(s).unwrap_or(u32::MAX));
    let speaker = args
        .get("speaker")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let filter = resolve_speaker_filter(runtime, speaker)?.unwrap_or(ReplayFilter::All);
    execute_replay_with_filter(runtime, requested, filter)
}

fn execute_status(runtime: &VoiceAudioRuntime) -> Result<Value> {
    let status = runtime.audio_output.status();
    let snapshot = runtime.speaker_rings.snapshot(ReplayFilter::All, None);
    let playing = status.current.as_ref().map(|info| {
        let kind = match info.kind {
            crate::adapter::headless::audio_output::JobKind::PcmClip => "PcmClip",
            crate::adapter::headless::audio_output::JobKind::EncodedStream => "EncodedStream",
        };
        let source = match info.source {
            crate::adapter::headless::audio_output::JobSource::SkillClip => "SkillClip",
            crate::adapter::headless::audio_output::JobSource::Tts => "Tts",
            crate::adapter::headless::audio_output::JobSource::External => "External",
        };
        json!({ "kind": kind, "source": source })
    });
    Ok(json!({
        "recording": true,
        "buffered_ms": snapshot.buffered_ms,
        "playback_duration_ms": playback_duration_ms(snapshot.samples.len()),
        "speakers": speakers_json(runtime),
        "playing": playing,
        "queued_jobs": status.queued_jobs,
        "last_error": status.last_error,
    }))
}

#[async_trait]
impl Skill for VoiceReplay {
    fn name(&self) -> &'static str {
        "voice_replay"
    }

    fn description(&self) -> &'static str {
        "Replay recent TeamSpeak voice audio from the recording window. \
         actions: status (list speakers/active_ms), replay (optional seconds/speaker), stop (cancel clips). \
         Prefer status first, then replay with speaker when needed."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["status", "replay", "stop"],
                    "description": "status | replay | stop"
                },
                "seconds": {
                    "type": "integer",
                    "description": "Replay length in seconds; clamped to the recording window."
                },
                "speaker": {
                    "type": "string",
                    "description": "Exact speaker nickname in the window; ambiguous/missing returns candidates."
                }
            },
            "required": ["action"]
        })
    }

    fn should_register(&self) -> bool {
        self.config.voice_replay.enabled
    }

    async fn execute(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value> {
        if ctx.platform != Platform::TeamSpeak {
            anyhow::bail!("voice_replay is TeamSpeak-only");
        }
        let runtime = self.runtime()?;
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("missing required parameter: action"))?;
        match action {
            "replay" => execute_replay(&runtime, &args),
            "status" => execute_status(&runtime),
            "stop" => {
                let stopped = runtime.audio_output.stop_clips();
                Ok(json!({ "status": "ok", "stopped_clips": stopped }))
            }
            other => Err(anyhow!("unknown voice_replay action '{other}'")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectReplayCommand {
    Replay {
        seconds: Option<u32>,
        speaker: Option<String>,
    },
    Stop,
    Status,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpeakerMatch {
    Unique { clid: u32, name: String },
    Ambiguous(Vec<String>),
    None { candidates: Vec<String> },
}

/// 直呼解析：`!replay [N] [@name with spaces]` / `!replay stop` / `!replay status`
pub fn parse_direct_command(text: &str, commands: &[String]) -> Option<DirectReplayCommand> {
    let text = text.trim();
    for cmd in commands {
        let cmd = cmd.trim();
        if cmd.is_empty() {
            continue;
        }
        if text == cmd {
            return Some(DirectReplayCommand::Replay {
                seconds: None,
                speaker: None,
            });
        }
        let prefix = format!("{cmd} ");
        if let Some(rest) = text.strip_prefix(&prefix) {
            return parse_direct_args(rest.trim());
        }
    }
    None
}

fn parse_direct_args(rest: &str) -> Option<DirectReplayCommand> {
    if rest.is_empty() {
        return Some(DirectReplayCommand::Replay {
            seconds: None,
            speaker: None,
        });
    }
    if rest.eq_ignore_ascii_case("stop") {
        return Some(DirectReplayCommand::Stop);
    }
    if rest.eq_ignore_ascii_case("status") {
        return Some(DirectReplayCommand::Status);
    }
    let mut seconds = None;
    let mut parts: Vec<&str> = rest.split_whitespace().collect();
    // N 可在 @Name 前或后；昵称含空格时取最长名，末位纯数字视为 seconds
    if let Some(first) = parts.first().copied() {
        if let Ok(n) = first.parse::<u32>() {
            seconds = Some(n);
            parts.remove(0);
        } else if let Some(last) = parts.last().copied() {
            if parts.len() > 1 {
                if let Ok(n) = last.parse::<u32>() {
                    seconds = Some(n);
                    parts.pop();
                }
            }
        }
    }
    let speaker = if parts.is_empty() {
        None
    } else {
        let joined = parts.join(" ");
        let name = joined.strip_prefix('@').unwrap_or(&joined);
        if name.is_empty() {
            None
        } else {
            Some(name.to_string())
        }
    };
    Some(DirectReplayCommand::Replay { seconds, speaker })
}

/// 已知 speakers 最长匹配；精确优先，多匹配/无匹配返回候选
pub fn match_speaker_longest(candidates: &[(u32, String)], input: &str) -> SpeakerMatch {
    let input = input.trim();
    let all_names = || -> Vec<String> {
        candidates
            .iter()
            .map(|(_, name)| name.clone())
            .collect::<Vec<_>>()
    };
    if input.is_empty() {
        return SpeakerMatch::None {
            candidates: all_names(),
        };
    }

    let exact: Vec<&(u32, String)> = candidates
        .iter()
        .filter(|(_, name)| name == input || name.eq_ignore_ascii_case(input))
        .collect();
    if exact.len() == 1 {
        return SpeakerMatch::Unique {
            clid: exact[0].0,
            name: exact[0].1.clone(),
        };
    }
    if exact.len() > 1 {
        return SpeakerMatch::Ambiguous(exact.iter().map(|(_, n)| (*n).clone()).collect());
    }

    let input_l = input.to_ascii_lowercase();
    let mut hits: Vec<&(u32, String)> = candidates
        .iter()
        .filter(|(_, name)| {
            let name_l = name.to_ascii_lowercase();
            name_l == input_l
                || name_l.starts_with(&input_l)
                || input_l.starts_with(&name_l)
                || name_l.contains(&input_l)
        })
        .collect();
    if hits.is_empty() {
        return SpeakerMatch::None {
            candidates: all_names(),
        };
    }
    let max_len = hits
        .iter()
        .map(|(_, name)| name.chars().count())
        .max()
        .unwrap_or(0);
    hits.retain(|(_, name)| name.chars().count() == max_len);
    if hits.len() == 1 {
        return SpeakerMatch::Unique {
            clid: hits[0].0,
            name: hits[0].1.clone(),
        };
    }
    SpeakerMatch::Ambiguous(hits.iter().map(|(_, n)| (*n).clone()).collect())
}

/// 直呼执行：与技能同一 JSON 契约；speaker 走最长匹配
pub fn execute_direct_command(
    command: DirectReplayCommand,
    runtime: &VoiceAudioRuntime,
) -> Result<Value> {
    match command {
        DirectReplayCommand::Stop => {
            let stopped = runtime.audio_output.stop_clips();
            Ok(json!({ "status": "ok", "stopped_clips": stopped }))
        }
        DirectReplayCommand::Status => execute_status(runtime),
        DirectReplayCommand::Replay { seconds, speaker } => {
            let filter = match speaker {
                None => ReplayFilter::All,
                Some(name) => {
                    let stats = runtime.speaker_rings.stats();
                    let candidates: Vec<(u32, String)> =
                        stats.iter().map(|s| (s.clid, s.name.clone())).collect();
                    match match_speaker_longest(&candidates, &name) {
                        SpeakerMatch::Unique { clid, .. } => ReplayFilter::Speaker { clid },
                        SpeakerMatch::Ambiguous(names) => {
                            return Err(anyhow!(
                                "speaker name '{}' is ambiguous; candidates: {}",
                                name,
                                names.join(", ")
                            ));
                        }
                        SpeakerMatch::None { candidates } => {
                            return Err(anyhow!(
                                "speaker '{}' not found in recording window; candidates: {}",
                                name,
                                candidates.join(", ")
                            ));
                        }
                    }
                }
            };
            execute_replay_with_filter(runtime, seconds, filter)
        }
    }
}

/// 直呼与技能共用的 ACL：同一 `voice_replay` 白名单
pub fn direct_command_allowed(
    gate: &crate::permission::PermissionGate,
    groups: &[u32],
    channel_group_id: u32,
) -> bool {
    let allowed = gate.get_allowed_skills(groups, channel_group_id);
    crate::skills::is_skill_allowed("voice_replay", &allowed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::headless::audio_output::AudioBus;
    use crate::adapter::headless::speaker_ring::SpeakerRings;
    use crate::config::{AclConfig, VoiceReplayConfig};
    use crate::permission::PermissionGate;
    use std::time::Duration;

    fn handles_with_rings(window_secs: u32) -> VoiceAudioHandles {
        let bus = AudioBus::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(Vec<u8>, i32)>(64);
        tokio::spawn(bus.consumer.run(tx));
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let handles = VoiceAudioHandles::default();
        handles.install(VoiceAudioRuntime {
            audio_output: bus.output,
            speaker_rings: Arc::new(SpeakerRings::new(Duration::from_secs(u64::from(
                window_secs,
            )))),
            window_secs,
        });
        handles
    }

    fn ts_ctx(config: Arc<AppConfig>) -> UnifiedExecutionContext {
        UnifiedExecutionContext {
            platform: Platform::TeamSpeak,
            ts_adapter: None,
            nc_adapter: None,
            caller_id: 1,
            caller_id_nc: 0,
            caller_name: "tester".into(),
            caller_groups: vec![],
            caller_channel_group_id: 0,
            nc_group_id: None,
            gate: Arc::new(PermissionGate::new(AclConfig::default())),
            config,
        }
    }

    fn enabled_config(window_secs: u32) -> Arc<AppConfig> {
        Arc::new(AppConfig {
            voice_replay: VoiceReplayConfig {
                enabled: true,
                window_secs,
                direct_commands: vec!["!replay".into()],
            },
            ..AppConfig::default()
        })
    }

    #[test]
    fn should_register_follows_config_enabled() {
        let mut disabled = AppConfig::default();
        disabled.voice_replay.enabled = false;
        let skill = VoiceReplay::new(Arc::new(disabled), VoiceAudioHandles::default());
        assert!(!skill.should_register());

        let skill = VoiceReplay::new(enabled_config(30), VoiceAudioHandles::default());
        assert!(skill.should_register());
    }

    #[tokio::test]
    async fn status_json_contract_fields() {
        let config = enabled_config(30);
        let handles = handles_with_rings(30);
        let skill = VoiceReplay::new(config.clone(), handles);
        let ctx = ts_ctx(config);
        let value = skill
            .execute(json!({"action": "status"}), &ctx)
            .await
            .unwrap();
        assert_eq!(value["recording"], json!(true));
        assert_eq!(
            value["buffered_ms"].as_u64(),
            value["playback_duration_ms"].as_u64()
        );
        assert!(value["speakers"].is_array());
        assert!(value["queued_jobs"].as_u64().is_some());
    }

    #[tokio::test]
    async fn replay_clamps_seconds_over_window() {
        let config = enabled_config(30);
        let handles = handles_with_rings(30);
        let skill = VoiceReplay::new(config.clone(), handles);
        let ctx = ts_ctx(config);
        let value = skill
            .execute(json!({"action": "replay", "seconds": 120}), &ctx)
            .await
            .unwrap();
        assert_eq!(value["status"], json!("queued"));
        // 新建环 age≈0：axis=min(window, age, seconds) 由 age 决定，且 seconds 已 clamp 到 window
        let buffered = value["buffered_ms"].as_u64().expect("buffered_ms");
        assert!(buffered <= 30_000);
        assert_eq!(Some(buffered), value["playback_duration_ms"].as_u64());
    }

    #[tokio::test]
    async fn replay_unknown_speaker_lists_candidates() {
        let config = enabled_config(30);
        let handles = handles_with_rings(30);
        let skill = VoiceReplay::new(config.clone(), handles);
        let ctx = ts_ctx(config);
        let err = skill
            .execute(json!({"action": "replay", "speaker": "nobody"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
        assert!(err.to_string().contains("candidates"));
    }

    #[tokio::test]
    async fn stop_returns_stopped_clips() {
        let config = enabled_config(30);
        let handles = handles_with_rings(30);
        let skill = VoiceReplay::new(config.clone(), handles);
        let ctx = ts_ctx(config);
        let value = skill
            .execute(json!({"action": "stop"}), &ctx)
            .await
            .unwrap();
        assert_eq!(value["status"], json!("ok"));
        assert!(value["stopped_clips"].as_u64().is_some());
    }

    #[tokio::test]
    async fn napcat_platform_is_rejected() {
        let config = enabled_config(30);
        let handles = handles_with_rings(30);
        let skill = VoiceReplay::new(config.clone(), handles);
        let mut ctx = ts_ctx(config);
        ctx.platform = Platform::NapCat;
        assert!(skill
            .execute(json!({"action": "status"}), &ctx)
            .await
            .is_err());
    }

    #[test]
    fn playback_duration_matches_sample_geometry() {
        assert_eq!(playback_duration_ms(96_000), 1000);
        assert_eq!(playback_duration_ms(0), 0);
    }

    fn cmds() -> Vec<String> {
        vec!["!replay".to_string(), "!回放".to_string()]
    }

    #[test]
    fn parse_direct_command_variants() {
        assert_eq!(
            parse_direct_command("!replay", &cmds()),
            Some(DirectReplayCommand::Replay {
                seconds: None,
                speaker: None
            })
        );
        assert_eq!(
            parse_direct_command("!replay 10", &cmds()),
            Some(DirectReplayCommand::Replay {
                seconds: Some(10),
                speaker: None
            })
        );
        assert_eq!(
            parse_direct_command("!replay @Alice", &cmds()),
            Some(DirectReplayCommand::Replay {
                seconds: None,
                speaker: Some("Alice".into())
            })
        );
        assert_eq!(
            parse_direct_command("!replay @Alice Smith 10", &cmds()),
            Some(DirectReplayCommand::Replay {
                seconds: Some(10),
                speaker: Some("Alice Smith".into())
            })
        );
        // 数字在昵称后时整体作为 speaker（最长匹配在 execute 阶段）
        assert_eq!(
            parse_direct_command("!replay @Bob 3", &cmds()),
            Some(DirectReplayCommand::Replay {
                seconds: Some(3),
                speaker: Some("Bob".into())
            })
        );
        assert_eq!(
            parse_direct_command("!replay stop", &cmds()),
            Some(DirectReplayCommand::Stop)
        );
        assert_eq!(
            parse_direct_command("!回放 status", &cmds()),
            Some(DirectReplayCommand::Status)
        );
        assert_eq!(parse_direct_command("hello", &cmds()), None);
        assert_eq!(parse_direct_command("!replay", &[]), None);
    }

    #[test]
    fn match_speaker_longest_prefers_longer_nickname() {
        let candidates = vec![
            (1u32, "Alice".to_string()),
            (2u32, "Alice Smith".to_string()),
            (3u32, "Bob".to_string()),
        ];
        assert_eq!(
            match_speaker_longest(&candidates, "Alice Smith"),
            SpeakerMatch::Unique {
                clid: 2,
                name: "Alice Smith".into()
            }
        );
        assert_eq!(
            match_speaker_longest(&candidates, "Alice"),
            SpeakerMatch::Unique {
                clid: 1,
                name: "Alice".into()
            }
        );
        // 仅给 "Alice" 且无精确独占时：唯一最长命中 "Alice" 本身
        let only_long = vec![(2u32, "Alice Smith".to_string())];
        assert_eq!(
            match_speaker_longest(&only_long, "Alice"),
            SpeakerMatch::Unique {
                clid: 2,
                name: "Alice Smith".into()
            }
        );
        assert!(matches!(
            match_speaker_longest(&candidates, "Carol"),
            SpeakerMatch::None { .. }
        ));
        let dups = vec![(1u32, "Alice".to_string()), (4u32, "Alice".to_string())];
        assert!(matches!(
            match_speaker_longest(&dups, "Alice"),
            SpeakerMatch::Ambiguous(names) if names.len() == 2
        ));
    }

    #[test]
    fn direct_command_acl_denies_without_skill() {
        let mut acl = AclConfig::default();
        acl.rules.push(crate::config::acl::AclRule {
            server_group_ids: vec![8],
            channel_group_ids: vec![],
            allowed_skills: vec!["poke_client".into()],
            can_target_admins: false,
        });
        let gate = PermissionGate::new(acl);
        assert!(!direct_command_allowed(&gate, &[8], 0));
        let allow_acl = AclConfig::default();
        let gate = PermissionGate::new(crate::config::AclConfig {
            rules: vec![crate::config::acl::AclRule {
                server_group_ids: vec![8],
                channel_group_ids: vec![],
                allowed_skills: vec!["voice_replay".into()],
                can_target_admins: false,
            }],
            acl: Default::default(),
        });
        assert!(direct_command_allowed(&gate, &[8], 0));
        let _ = allow_acl;
    }

    #[tokio::test]
    async fn execute_direct_unknown_speaker_returns_error_json_path() {
        let config = enabled_config(30);
        let handles = handles_with_rings(30);
        let runtime = handles.get().unwrap();
        let err = execute_direct_command(
            DirectReplayCommand::Replay {
                seconds: None,
                speaker: Some("nobody".into()),
            },
            &runtime,
        )
        .unwrap_err();
        assert!(err.to_string().contains("candidates"));
        let _ = config;
    }
}
