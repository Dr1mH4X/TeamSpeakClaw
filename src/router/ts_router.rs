use crate::adapter::headless::{
    parse_server_groups, should_route_text_through_bridge, voice_features_enabled,
    MainSubscriptions, TextMessageEvent, TsAdapter, TsEvent,
};
use crate::adapter::lifecycle::BridgeReadiness;
use crate::adapter::napcat::NapCatAdapter;
use crate::adapter::reconnect::drain_managed_tasks;
use crate::config::{AppConfig, PromptsConfig};
use crate::llm::context::SessionSource;
use crate::llm::LlmEngine;
use crate::permission::PermissionGate;
use crate::router::{
    ReplyPolicy, RouterContext, TurnError, TurnInput, TurnPermit, TurnRequest, TurnSession,
    TurnSink, UnifiedInboundEvent,
};
use crate::skills::{SkillRegistry, TsCaller, UnifiedExecutionContext};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::{broadcast, watch, Mutex};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

#[derive(Clone)]
pub struct EventRouter {
    config: Arc<AppConfig>,
    prompts: Arc<PromptsConfig>,
    adapter: Arc<TsAdapter>,
    gate: Arc<PermissionGate>,
    llm: Arc<LlmEngine>,
    registry: Arc<SkillRegistry>,
    nc_adapter: Option<Arc<NapCatAdapter>>,
    voice_bridge_state: BridgeReadiness,
    voice_audio: crate::skills::VoiceAudioHandles,
    subscriptions: Arc<Mutex<Option<MainSubscriptions>>>,
}

impl EventRouter {
    pub(crate) fn new_with_clients(
        context: RouterContext,
        adapter: Arc<TsAdapter>,
        event_rx: broadcast::Receiver<TsEvent>,
        disconnect_rx: watch::Receiver<bool>,
        nc_adapter: Option<Arc<NapCatAdapter>>,
        voice_bridge_state: BridgeReadiness,
    ) -> Self {
        let RouterContext {
            config,
            prompts,
            gate,
            llm,
            registry,
            voice_audio,
        } = context;

        Self {
            config,
            prompts,
            adapter,
            gate,
            llm,
            registry,
            nc_adapter,
            voice_bridge_state,
            voice_audio,
            subscriptions: Arc::new(Mutex::new(Some(MainSubscriptions {
                events: event_rx,
                disconnected: disconnect_rx,
            }))),
        }
    }

    pub async fn run(&self) -> Result<()> {
        let mut subscriptions = self
            .subscriptions
            .lock()
            .await
            .take()
            .ok_or_else(|| anyhow::anyhow!("TS event router already started"))?;

        let mut tasks = JoinSet::new();
        loop {
            match receive_ts_event(&mut subscriptions.events, &mut subscriptions.disconnected)
                .await?
            {
                TsEvent::TextMessage(msg) => {
                    let this = self.clone();
                    // 同步门先过：不需要 LLM 回合的事件不占容量、不排队等会话锁
                    let Some(unified_event) = precheck_text_turn(
                        &this.config,
                        &this.llm,
                        this.adapter.get_bot_clid(),
                        this.voice_bridge_state.is_ready(),
                        &msg,
                    ) else {
                        continue;
                    };
                    let source = SessionSource::TeamSpeak {
                        uid: msg.invoker_uid.clone(),
                    };
                    let Ok(permit) = TurnPermit::reserve(&this.llm) else {
                        warn!(
                            invoker = %msg.invoker_name,
                            "TS LLM turn queue full; dropping message"
                        );
                        continue;
                    };
                    let ticket = this.llm.enqueue_turn_ticket(&source);
                    tasks.spawn(async move {
                        let session = permit.acquire_session(&this.llm, &source, ticket).await;
                        this.handle_message(msg, unified_event, session).await;
                    });
                }
                TsEvent::Disconnected => {
                    drain_managed_tasks(&mut tasks, "TS message").await;
                    return Ok(());
                }
            }
        }
    }

    fn run_voice_replay_direct(
        &self,
        command: crate::skills::voice_replay::DirectReplayCommand,
        groups: &[u32],
        channel_group_id: u32,
    ) -> String {
        if !crate::skills::voice_replay::direct_command_allowed(
            &self.gate,
            groups,
            channel_group_id,
        ) {
            return "voice_replay denied by ACL".to_string();
        }
        let Some(runtime) = self.voice_audio.get() else {
            return "voice replay runtime not ready".to_string();
        };
        match crate::skills::voice_replay::execute_direct_command(command, &runtime) {
            Ok(value) => match value.get("status").and_then(|s| s.as_str()) {
                Some("empty") => value
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("no speakers in the recording window")
                    .to_string(),
                Some(status) => {
                    let buffered = value
                        .get("buffered_ms")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let speakers = value
                        .get("speakers")
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "[]".into());
                    format!("voice_replay {status}; buffered_ms={buffered}; speakers={speakers}")
                }
                None => "voice_replay ok".to_string(),
            },
            Err(error) => format!("voice_replay failed: {error}"),
        }
    }

    async fn handle_message(
        &self,
        event: TextMessageEvent,
        unified_event: UnifiedInboundEvent,
        session: TurnSession,
    ) {
        let ReplyPolicy::TeamSpeak {
            target_mode: reply_mode,
            target: reply_target,
        } = unified_event.reply_policy;

        let msg_content = unified_event.text.as_str();
        info!(
            invoker = %event.invoker_name,
            clid = event.invoker_id,
            message_chars = msg_content.chars().count(),
            "Message received"
        );

        let groups = parse_server_groups(&event.invoker_groups);
        let channel_group_id = match self
            .adapter
            .get_client_channel_group_id(event.invoker_id)
            .await
        {
            Ok(channel_group_id) => channel_group_id,
            Err(error) => {
                error!(
                    clid = event.invoker_id,
                    error = %error,
                    "Failed to resolve caller channel group"
                );
                return;
            }
        };

        let source = SessionSource::TeamSpeak {
            uid: event.invoker_uid.clone(),
        };
        let system_prompt = self.prompts.system.content.as_str();

        let (online_clients, invoker_channel) =
            self.adapter.list_clients_json(event.invoker_id).await;

        let user_ctx = format!(
            r#"invoker: {{"name":"{}","clid":{},"channel_id":{}}}
Online: {}"#,
            event.invoker_name, event.invoker_id, invoker_channel, online_clients
        );

        let allowed_skills = self.gate.get_allowed_skills(&groups, channel_group_id);

        // 直呼与技能同一 ACL；bridge 未就绪时由 EventRouter 处理
        if self.config.voice_replay.enabled {
            if let Some(command) = crate::skills::voice_replay::parse_direct_command(
                msg_content,
                &self.config.voice_replay.direct_commands,
            ) {
                let ack = self.run_voice_replay_direct(command, &groups, channel_group_id);
                let _ = self
                    .adapter
                    .send_text_message(reply_mode, reply_target, &ack)
                    .await;
                return;
            }
        }

        // 文本回合不挂流式回调：等整体回复再送达
        let sink = TsTextSink {
            adapter: self.adapter.clone(),
            target_mode: reply_mode,
            target: reply_target,
        };
        let cancel = CancellationToken::new();
        let request = TurnRequest {
            llm: &self.llm,
            registry: &self.registry,
            source: &source,
            system_prompt,
            user_ctx: user_ctx.as_str(),
            allowed_skills: &allowed_skills,
            input: TurnInput::Text(msg_content),
            callbacks: None,
            cancel: &cancel,
        };
        match request
            .run(
                session,
                || {
                    UnifiedExecutionContext::for_ts(
                        TsCaller {
                            adapter: self.adapter.clone(),
                            caller_id: event.invoker_id,
                            caller_name: event.invoker_name.clone(),
                            caller_groups: groups.clone(),
                            caller_channel_group_id: channel_group_id,
                            nc_adapter: self.nc_adapter.clone(),
                        },
                        self.gate.clone(),
                        self.config.clone(),
                    )
                },
                &sink,
            )
            .await
        {
            Err(TurnError::Failed(error)) => error!("LLM error: {error}"),
            Err(TurnError::ReplyFailed(error)) => {
                warn!(error = %error, "TS reply delivery failed")
            }
            _ => {}
        }
    }
}

/// 回合准入前的同步门：bot 自身与音乐 bot 的消息、交给语音桥的文本、
/// 不触发 LLM 的文本、超限文本都在这里返回 `None`。
///
/// 必须在容量占位与会话排队之前调用：这些消息不需要回合，若先进队列，
/// 会在等同会话锁期间占着全局容量（容量 4，含等待中的回合），
/// 把别处会话的有效消息挤成「queue full」。
///
/// 返回的 `UnifiedInboundEvent` 直接交给回合使用，触发判定与文本剥离只算一次。
fn precheck_text_turn(
    config: &AppConfig,
    llm: &LlmEngine,
    bot_clid: u32,
    bridge_ready: bool,
    event: &TextMessageEvent,
) -> Option<UnifiedInboundEvent> {
    if event.invoker_id == bot_clid {
        return None;
    }
    if config.is_music_bot_name(&event.invoker_name) {
        return None;
    }

    // 订阅流健康时才由 voice_router 接管文本。
    if should_route_text_through_bridge(voice_features_enabled(config), bridge_ready) {
        return None;
    }

    let unified_event = UnifiedInboundEvent::from_ts(event, config)?;
    if !unified_event.should_trigger_llm {
        return None;
    }
    if let Err(error) = llm.check_user_text_bounds(&unified_event.text) {
        warn!(error = %error, "TS message dropped for exceeding size limit");
        return None;
    }
    Some(unified_event)
}

/// TS 文本回复落点：回复目标由事件触发策略解析后固定
struct TsTextSink {
    adapter: Arc<TsAdapter>,
    target_mode: u8,
    target: u32,
}

#[async_trait]
impl TurnSink for TsTextSink {
    async fn send(&self, text: &str) -> Result<()> {
        self.adapter
            .send_text_message(self.target_mode, self.target, text)
            .await
    }
}

async fn receive_ts_event(
    event_rx: &mut broadcast::Receiver<TsEvent>,
    disconnect_rx: &mut watch::Receiver<bool>,
) -> Result<TsEvent> {
    loop {
        if *disconnect_rx.borrow() {
            return Ok(TsEvent::Disconnected);
        }

        tokio::select! {
            biased;
            changed = disconnect_rx.changed() => {
                changed.map_err(|_| anyhow::anyhow!("TS connection state stream closed"))?;
                if *disconnect_rx.borrow_and_update() {
                    return Ok(TsEvent::Disconnected);
                }
            }
            event = event_rx.recv() => {
                match event {
                    Ok(event) => return Ok(event),
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!(skipped, "TS event router lagged; skipped buffered events");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(anyhow::anyhow!("TS event stream closed"));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{precheck_text_turn, receive_ts_event};
    use crate::adapter::headless::{TextMessageEvent, TextMessageTarget, TsEvent};
    use crate::config::AppConfig;
    use crate::llm::LlmEngine;
    use std::sync::Arc;
    use tokio::sync::{broadcast, watch};

    fn text_event(sequence: u32) -> TsEvent {
        TsEvent::TextMessage(TextMessageEvent {
            target_mode: TextMessageTarget::Private,
            invoker_name: format!("user-{sequence}"),
            invoker_uid: format!("uid-{sequence}"),
            invoker_id: sequence,
            invoker_groups: Vec::new(),
            message: "test".to_string(),
        })
    }

    /// 私聊触发、频道必须命中前缀的配置，便于构造「不需要回合」的输入
    fn review_config() -> AppConfig {
        let mut config = AppConfig::default();
        config.bot.trigger_prefixes = vec!["!bot".to_string()];
        config.bot.respond_to_private = true;
        // 语音特性显式关掉：`voice_replay.enabled` 默认是开的，否则 bridge 就绪就会接管文本
        config.voice_replay.enabled = false;
        config
    }

    fn channel_event(message: &str) -> TextMessageEvent {
        TextMessageEvent {
            target_mode: TextMessageTarget::Channel,
            invoker_name: "someone".to_string(),
            invoker_uid: "uid-someone".to_string(),
            invoker_id: 7,
            invoker_groups: Vec::new(),
            message: message.to_string(),
        }
    }

    fn engine() -> LlmEngine {
        LlmEngine::new(Arc::new(AppConfig::default()))
            .expect("engine builds without a network call")
    }

    /// 触发文本：预检把剥离后的文本交出去，回合不必再算一遍
    #[test]
    fn precheck_returns_the_stripped_text_for_a_triggering_message() {
        let config = review_config();
        let llm = engine();
        let event = channel_event("!bot  hello");

        let unified = precheck_text_turn(&config, &llm, 1, false, &event)
            .expect("a triggering message must be admitted");

        assert_eq!(unified.text, "hello");
        assert!(unified.should_trigger_llm);
    }

    /// 不需要回合的四类消息都在排队之前被丢掉
    #[test]
    fn precheck_rejects_messages_that_need_no_turn() {
        let config = review_config();
        let llm = engine();

        assert!(
            precheck_text_turn(&config, &llm, 7, false, &channel_event("!bot hello")).is_none(),
            "the bot's own client id must be ignored"
        );

        let mut music_config = review_config();
        music_config.music_backend = Some(crate::config::MusicBackendConfig {
            backend: "ts3audiobot".to_string(),
            base_url: String::new(),
            musicbot_name: "MusicBot".to_string(),
        });
        let music_event = TextMessageEvent {
            invoker_name: "MusicBot".to_string(),
            ..channel_event("!bot hello")
        };
        assert!(
            precheck_text_turn(&music_config, &llm, 1, false, &music_event).is_none(),
            "the music bot must be ignored"
        );

        let mut voice_config = review_config();
        voice_config.headless.stt.enabled = true;
        assert!(
            precheck_text_turn(&voice_config, &llm, 1, true, &channel_event("!bot hello"))
                .is_none(),
            "text must go to the voice bridge once it is ready"
        );

        assert!(
            precheck_text_turn(&config, &llm, 1, false, &channel_event("no prefix")).is_none(),
            "a channel message without the trigger prefix must not queue"
        );

        let oversized = channel_event(&format!(
            "!bot {}",
            "x".repeat(crate::llm::engine::MAX_USER_TEXT_BYTES + 1)
        ));
        assert!(
            precheck_text_turn(&config, &llm, 1, false, &oversized).is_none(),
            "an oversized message must be dropped before queueing"
        );
    }

    /// 语音特性未开启时 bridge 就绪也仍由 EventRouter 处理
    #[test]
    fn precheck_keeps_text_when_voice_features_are_off() {
        let config = review_config();
        let llm = engine();

        assert!(precheck_text_turn(&config, &llm, 1, true, &channel_event("!bot hello")).is_some());
    }

    #[tokio::test]
    async fn disconnect_state_survives_event_lag_before_first_poll() {
        let (event_tx, _) = broadcast::channel(2);
        let mut lag_probe = event_tx.subscribe();
        let mut event_rx = event_tx.subscribe();
        let (disconnect_tx, mut disconnect_rx) = watch::channel(false);

        event_tx.send(TsEvent::Disconnected).unwrap();
        disconnect_tx.send_replace(true);
        for sequence in 1..=4 {
            event_tx.send(text_event(sequence)).unwrap();
        }

        assert!(matches!(
            lag_probe.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
        assert!(matches!(
            receive_ts_event(&mut event_rx, &mut disconnect_rx)
                .await
                .unwrap(),
            TsEvent::Disconnected
        ));
    }
}
