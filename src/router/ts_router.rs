use crate::adapter::headless::{
    parse_server_groups, should_route_text_through_bridge, voice_features_enabled,
    MainSubscriptions, TextMessageEvent, TsAdapter, TsEvent, VoiceBridgeState,
};
use crate::adapter::napcat::NapCatAdapter;
use crate::adapter::reconnect::drain_managed_tasks;
use crate::config::{AppConfig, PromptsConfig};
use crate::llm::context::SessionSource;
use crate::llm::{LlmEngine, TurnCapacityPermit, TurnSessionGuard};
use crate::permission::PermissionGate;
use crate::router::{
    run_llm_turn, ReplyPolicy, RouterContext, UnifiedInboundEvent, LLM_ERROR_REPLY,
};
use crate::skills::{ExecutionContext, SkillRegistry, UnifiedExecutionContext};
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::{broadcast, watch, Mutex};
use tokio::task::JoinSet;
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
    voice_bridge_state: VoiceBridgeState,
    subscriptions: Arc<Mutex<Option<MainSubscriptions>>>,
}

impl EventRouter {
    pub fn new_with_clients(
        context: RouterContext,
        adapter: Arc<TsAdapter>,
        event_rx: broadcast::Receiver<TsEvent>,
        disconnect_rx: watch::Receiver<bool>,
        nc_adapter: Option<Arc<NapCatAdapter>>,
        voice_bridge_state: VoiceBridgeState,
    ) -> Self {
        let RouterContext {
            config,
            prompts,
            gate,
            llm,
            registry,
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
                    let source = SessionSource::TeamSpeak {
                        uid: msg.invoker_uid.clone(),
                    };
                    let Ok(capacity) = this.llm.try_reserve_turn_capacity() else {
                        warn!(
                            invoker = %msg.invoker_name,
                            "TS LLM turn queue full; dropping message"
                        );
                        continue;
                    };
                    tasks.spawn(async move {
                        let session = this.llm.acquire_turn_session(&source).await;
                        this.handle_message(msg, capacity, session).await;
                    });
                }
                TsEvent::Disconnected => {
                    drain_managed_tasks(&mut tasks, "TS message").await;
                    return Ok(());
                }
            }
        }
    }

    async fn handle_message(
        &self,
        event: TextMessageEvent,
        // 持有容量占位与同会话串行锁直至回复发送与历史保存完成
        _capacity: TurnCapacityPermit,
        _session: TurnSessionGuard,
    ) {
        if event.invoker_id == self.adapter.get_bot_clid() {
            return;
        }
        if self.config.is_music_bot_name(&event.invoker_name) {
            return;
        }

        // 订阅流健康时才由 voice_router 接管文本。
        if should_route_text_through_bridge(
            voice_features_enabled(&self.config),
            self.voice_bridge_state.is_ready(),
        ) {
            return;
        }

        let Some(unified_event) = UnifiedInboundEvent::from_ts(&event, &self.config) else {
            return;
        };
        if !unified_event.should_trigger_llm {
            return;
        }

        let (reply_mode, reply_target) = match unified_event.reply_policy {
            ReplyPolicy::TeamSpeak {
                target_mode,
                target,
            } => (target_mode, target),
            _ => return,
        };

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
        if let Err(error) = self.llm.check_user_text_bounds(msg_content) {
            warn!(error = %error, "TS message dropped for exceeding size limit");
            return;
        }
        let system_prompt = &self.prompts.system.content;

        let (online_clients, invoker_channel) =
            self.adapter.list_clients_json(event.invoker_id).await;

        let user_ctx = format!(
            r#"invoker: {{"name":"{}","clid":{},"channel_id":{}}}
Online: {}"#,
            event.invoker_name, event.invoker_id, invoker_channel, online_clients
        );

        let allowed_skills = self.gate.get_allowed_skills(&groups, channel_group_id);

        // 注意这里传入了 None 作为 callbacks，意味着等待流式全部完成后拿整体回复
        match run_llm_turn(
            &self.llm,
            &self.registry,
            |llm| llm.build_messages(&source, system_prompt, &user_ctx, msg_content),
            &allowed_skills,
            None,
            || {
                let ctx = ExecutionContext {
                    adapter: self.adapter.clone(),
                    caller_id: event.invoker_id,
                    caller_name: event.invoker_name.clone(),
                    caller_groups: groups.clone(),
                    caller_channel_group_id: channel_group_id,
                    gate: self.gate.clone(),
                    config: self.config.clone(),
                };
                UnifiedExecutionContext::from_ts(&ctx)
                    .with_cross_adapters(Some(self.adapter.clone()), self.nc_adapter.clone())
            },
        )
        .await
        {
            Ok(result) => {
                if !result.content.is_empty() {
                    info!(
                        reply_chars = result.content.chars().count(),
                        "[TS] LLM final reply ready"
                    );
                    if self
                        .adapter
                        .send_text_message(reply_mode, reply_target, &result.content)
                        .await
                        .is_ok()
                    {
                        self.llm
                            .save_turn(&source, msg_content.to_string(), result.content);
                    }
                }
            }
            Err(e) => {
                error!("LLM error: {}", e);
                let _ = self
                    .adapter
                    .send_text_message(reply_mode, reply_target, LLM_ERROR_REPLY)
                    .await;
            }
        }
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
    use super::receive_ts_event;
    use crate::adapter::headless::{TextMessageEvent, TextMessageTarget, TsEvent};
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
