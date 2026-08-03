use crate::adapter::napcat::NapCatAdapter;
use crate::adapter::headless::{
    should_route_text_through_bridge, voice_features_enabled, VoiceBridgeState,
};
use crate::adapter::{TextMessageEvent, TsAdapter, TsEvent};
use crate::config::{AppConfig, PromptsConfig};
use crate::llm::context::SessionSource;
use crate::llm::{LlmEngine, ToolCall, ToolExecutor};
use crate::permission::PermissionGate;
use crate::router::{ReplyPolicy, RouterContext, UnifiedInboundEvent};
use crate::skills::{ExecutionContext, SkillRegistry};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::{broadcast, watch, Mutex};
use tracing::{debug, error, info, warn};

struct SqExecutor<'a> {
    router: &'a EventRouter,
    event: &'a TextMessageEvent,
    groups: &'a [u32],
    channel_group_id: u32,
    allowed_skills: &'a [String],
}

#[async_trait]
impl ToolExecutor for SqExecutor<'_> {
    async fn execute(&self, call: &ToolCall) -> String {
        self.router
            .execute_skill(
                call,
                self.event,
                self.groups,
                self.channel_group_id,
                self.allowed_skills,
            )
            .await
    }
}

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
    subscriptions: Arc<Mutex<Option<TsSubscriptions>>>,
}

struct TsSubscriptions {
    events: broadcast::Receiver<TsEvent>,
    disconnected: watch::Receiver<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TsRouterExit {
    Disconnected,
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
            subscriptions: Arc::new(Mutex::new(Some(TsSubscriptions {
                events: event_rx,
                disconnected: disconnect_rx,
            }))),
        }
    }

    pub async fn run(&self) -> Result<TsRouterExit> {
        let mut subscriptions = self
            .subscriptions
            .lock()
            .await
            .take()
            .ok_or_else(|| anyhow::anyhow!("TS event router already started"))?;

        loop {
            match receive_ts_event(&mut subscriptions.events, &mut subscriptions.disconnected)
                .await?
            {
                TsEvent::TextMessage(msg) => {
                    let this = self.clone();
                    tokio::spawn(async move {
                        this.handle_message(msg).await;
                    });
                }
                TsEvent::Disconnected => {
                    return Ok(TsRouterExit::Disconnected);
                }
            }
        }
    }

    async fn execute_skill(
        &self,
        call: &ToolCall,
        event: &TextMessageEvent,
        groups: &[u32],
        channel_group_id: u32,
        allowed_skills: &[String],
    ) -> String {
        let ctx = ExecutionContext {
            adapter: self.adapter.clone(),
            caller_id: event.invoker_id,
            caller_name: event.invoker_name.clone(),
            caller_groups: groups.to_vec(),
            caller_channel_group_id: channel_group_id,
            gate: self.gate.clone(),
            config: self.config.clone(),
        };
        self.registry
            .execute_skill(call, ctx, allowed_skills, self.nc_adapter.clone())
            .await
    }

    async fn handle_message(&self, event: TextMessageEvent) {
        if event.invoker_id == self.adapter.get_bot_clid() {
            return;
        }
        let musicbot_name = self
            .config
            .music_backend
            .as_ref()
            .map_or("", |c| c.musicbot_name.as_str());
        if !musicbot_name.is_empty()
            && event
                .invoker_name
                .to_ascii_lowercase()
                .contains(&musicbot_name.to_ascii_lowercase())
        {
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
        if !unified_event.should_respond {
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

        let groups: Vec<u32> = event
            .invoker_groups
            .iter()
            .filter_map(|g| g.parse().ok())
            .collect();
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
        let system_prompt = &self.prompts.system.content;

        let (online_clients, invoker_channel) = match self.adapter.list_clients().await {
            Ok(clients) => {
                let arr: Vec<serde_json::Value> = clients
                    .iter()
                    .map(|c| json!({"name": c.nickname, "clid": c.id, "channel_id": c.channel_id}))
                    .collect();
                let invoker_chan = clients
                    .iter()
                    .find(|c| c.id as u32 == event.invoker_id)
                    .map(|c| c.channel_id)
                    .unwrap_or(0);
                debug!("Fetched {} online clients for LLM context", clients.len());
                (
                    serde_json::to_string(&arr).unwrap_or_default(),
                    invoker_chan,
                )
            }
            Err(e) => {
                warn!("Failed to fetch online clients: {e}");
                (String::new(), 0)
            }
        };

        let user_ctx = format!(
            r#"invoker: {{"name":"{}","clid":{},"channel_id":{}}}
Online: {}"#,
            event.invoker_name, event.invoker_id, invoker_channel, online_clients
        );

        let mut messages = self
            .llm
            .build_messages(&source, system_prompt, &user_ctx, msg_content);
        let allowed_skills = self.gate.get_allowed_skills(&groups, channel_group_id);
        let tools = self.registry.to_tool_schemas(&allowed_skills);

        let executor = SqExecutor {
            router: self,
            event: &event,
            groups: &groups,
            channel_group_id,
            allowed_skills: &allowed_skills,
        };

        // 注意这里传入了 None 作为 callbacks，意味着等待流式全部完成后拿整体回复
        match self
            .llm
            .run_tool_loop(&mut messages, &tools, &executor, None)
            .await
        {
            Ok(result) => {
                if !result.content.is_empty() {
                    info!(
                        reply_chars = result.content.chars().count(),
                        "[TS] LLM final reply ready"
                    );
                    self.llm
                        .save_turn(&source, msg_content.to_string(), result.content.clone());
                    let _ = self
                        .adapter
                        .send_text_message(reply_mode, reply_target, &result.content)
                        .await;
                }
            }
            Err(e) => {
                error!("LLM error: {}", e);
                let _ = self
                    .adapter
                    .send_text_message(
                        reply_mode,
                        reply_target,
                        "AI backend unavailable. Please try again later.",
                    )
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
    use crate::adapter::{TextMessageEvent, TextMessageTarget, TsEvent};
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
