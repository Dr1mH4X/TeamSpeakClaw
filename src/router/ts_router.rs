use crate::adapter::napcat::NapCatAdapter;
use crate::adapter::{TextMessageEvent, TsAdapter, TsEvent};
use crate::config::{AppConfig, PromptsConfig};
use crate::llm::context::SessionSource;
use crate::llm::{LlmEngine, ToolCall, ToolExecutor};
use crate::permission::PermissionGate;
use crate::router::{ReplyPolicy, UnifiedInboundEvent};
use crate::skills::{ExecutionContext, SkillRegistry};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
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
}

impl EventRouter {
    pub fn new_with_clients(
        config: Arc<AppConfig>,
        prompts: Arc<PromptsConfig>,
        adapter: Arc<TsAdapter>,
        gate: Arc<PermissionGate>,
        llm: Arc<LlmEngine>,
        registry: Arc<SkillRegistry>,
        nc_adapter: Option<Arc<NapCatAdapter>>,
    ) -> Self {
        Self {
            config,
            prompts,
            adapter,
            gate,
            llm,
            registry,
            nc_adapter,
        }
    }

    pub async fn run(&self) -> Result<()> {
        let mut rx = self.adapter.subscribe();

        loop {
            match rx.recv().await {
                Ok(TsEvent::TextMessage(msg)) => {
                    let this = self.clone();
                    tokio::spawn(async move {
                        this.handle_message(msg).await;
                    });
                }
                Ok(TsEvent::Disconnected) => {
                    return Err(anyhow::anyhow!("TS connection lost"));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(skipped, "TS event router lagged; skipped buffered events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return Err(anyhow::anyhow!("TS event stream closed"));
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

        // 开启了语音桥接时，纯文本由 voice_router 处理
        if self.config.headless.stt.enabled
            || self.config.headless.tts.enabled
            || self.config.llm.omni_model
        {
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
