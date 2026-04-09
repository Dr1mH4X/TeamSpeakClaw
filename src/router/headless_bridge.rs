use std::sync::Arc;

use anyhow::{anyhow, Result};
use dashmap::DashMap;
use serde_json::json;
use tokio_stream::StreamExt;
use tonic::transport::Channel;
use tracing::{error, info, warn};

use crate::adapter::headless::tsbot::voice::v1 as voicev1;
use crate::adapter::headless::INTERNAL_GRPC_ADDR;
use crate::adapter::TsAdapter;
use crate::config::{AppConfig, PromptsConfig};
use crate::llm::provider::ToolCall;
use crate::llm::LlmEngine;
use crate::permission::PermissionGate;
use crate::router::ClientInfo;
use crate::skills::{ExecutionContext, SkillRegistry, UnifiedExecutionContext};
use voicev1::voice_service_client::VoiceServiceClient;

pub struct HeadlessLlmBridge {
    config: Arc<AppConfig>,
    prompts: Arc<PromptsConfig>,
    gate: Arc<PermissionGate>,
    llm: Arc<LlmEngine>,
    registry: Arc<SkillRegistry>,
    ts_adapter: Arc<TsAdapter>,
    ts_clients: Arc<DashMap<u32, ClientInfo>>,
}

impl HeadlessLlmBridge {
    const OBS_CHAT: bool = true;
    const OBS_LLM: bool = true;
    const OBS_TOOL: bool = true;
    const OBS_TEXT_MAX_LEN: usize = 240;

    pub fn new(
        config: Arc<AppConfig>,
        prompts: Arc<PromptsConfig>,
        gate: Arc<PermissionGate>,
        llm: Arc<LlmEngine>,
        registry: Arc<SkillRegistry>,
        ts_adapter: Arc<TsAdapter>,
        ts_clients: Arc<DashMap<u32, ClientInfo>>,
    ) -> Self {
        Self {
            config,
            prompts,
            gate,
            llm,
            registry,
            ts_adapter,
            ts_clients,
        }
    }

    pub async fn run(self) -> Result<()> {
        let endpoint = format!("http://{}", INTERNAL_GRPC_ADDR);
        let channel = Channel::from_shared(endpoint.clone())?.connect().await?;
        let mut client = VoiceServiceClient::new(channel);

        let req = tonic::Request::new(voicev1::SubscribeRequest {
            include_chat: true,
            include_playback: false,
            include_log: true,
            include_audio: false,
        });

        let mut stream = client.subscribe_events(req).await?.into_inner();
        info!("Headless LLM bridge subscribed: {}", endpoint);

        while let Some(item) = stream.next().await {
            match item {
                Ok(ev) => {
                    let Some(payload) = ev.payload else {
                        continue;
                    };
                    if let voicev1::event::Payload::Chat(chat) = payload {
                        let (caller_id, _, _) = self.resolve_caller(&chat);
                        if self.should_ignore_chat(&chat, caller_id) {
                            continue;
                        }
                        if !chat.should_trigger_llm {
                            continue;
                        }
                        if let Err(e) = self.handle_chat(&mut client, chat).await {
                            error!("headless bridge chat handling failed: {e}");
                        }
                    }
                }
                Err(e) => {
                    warn!("headless event stream error: {e}");
                    break;
                }
            }
        }

        Ok(())
    }

    fn truncate_for_log(&self, text: &str) -> String {
        let max_len = Self::OBS_TEXT_MAX_LEN.max(16);
        if text.len() <= max_len {
            return text.to_string();
        }
        let mut s = text.chars().take(max_len).collect::<String>();
        s.push_str("...");
        s
    }

    fn resolve_caller(&self, chat: &voicev1::ChatEvent) -> (u32, Vec<u32>, u32) {
        for item in self.ts_clients.iter() {
            let c = item.value();
            if c.nickname == chat.invoker_name {
                return (c.clid, c.server_groups.clone(), c.channel_group_id);
            }
        }
        (0, vec![], 0)
    }

    fn should_ignore_chat(&self, chat: &voicev1::ChatEvent, caller_id: u32) -> bool {
        if chat.invoker_name == self.config.bot.nickname {
            return true;
        }
        let bot_clid = self.ts_adapter.get_bot_clid();
        bot_clid != 0 && caller_id == bot_clid
    }

    async fn execute_skill(
        &self,
        call: &ToolCall,
        chat: &voicev1::ChatEvent,
        groups: &[u32],
        channel_group_id: u32,
    ) -> String {
        if let Some(skill) = self.registry.get(&call.name) {
            let (caller_id, _, _) = self.resolve_caller(chat);
            let ctx = ExecutionContext {
                adapter: self.ts_adapter.clone(),
                clients: self.ts_clients.as_ref(),
                caller_id,
                caller_name: chat.invoker_name.clone(),
                caller_groups: groups.to_vec(),
                caller_channel_group_id: channel_group_id,
                gate: self.gate.clone(),
                config: self.config.clone(),
                error_prompts: &self.prompts.error,
            };
            let unified_ctx = UnifiedExecutionContext::from_ts(&ctx).with_cross_adapters(
                Some(self.ts_adapter.clone()),
                Some(self.ts_clients.as_ref()),
                None,
            );

            let args = call.arguments.clone();
            let result = match skill.execute_unified(args.clone(), &unified_ctx).await {
                Ok(v) => Ok(v),
                Err(_) => skill.execute(args, &ctx).await,
            };

            match result {
                Ok(v) => v.to_string(),
                Err(e) => self
                    .prompts
                    .error
                    .skill_error
                    .replace("{detail}", &e.to_string()),
            }
        } else {
            self.prompts.error.skill_not_found.clone()
        }
    }

    async fn handle_chat(
        &self,
        client: &mut VoiceServiceClient<Channel>,
        chat: voicev1::ChatEvent,
    ) -> Result<()> {
        let system_prompt = &self.prompts.system.content;
        let (caller_id, groups, channel_group_id) = self.resolve_caller(&chat);
        let user_ctx = format!(
            "User: {} (uid: {}, clid: {}, groups: {:?}, channel_group: {})",
            chat.invoker_name, chat.invoker_unique_id, caller_id, groups, channel_group_id
        );
        let user_msg = chat.message.clone();

        if Self::OBS_CHAT {
            info!(
                event = "headless.chat.user_message",
                invoker = %chat.invoker_name,
                uid = %chat.invoker_unique_id,
                clid = caller_id,
                message = %self.truncate_for_log(&user_msg),
                "headless inbound chat"
            );
        }

        let allowed_skills = self.gate.get_allowed_skills(&groups, channel_group_id);
        let tools = self.registry.to_tool_schemas(&allowed_skills);
        let max_turns = self.config.bot.max_tool_turns;

        let mut messages = vec![
            json!({"role":"system","content":system_prompt}),
            json!({"role":"system","content":user_ctx}),
            json!({"role":"user","content":user_msg}),
        ];

        for turn in 0..max_turns {
            if Self::OBS_LLM {
                info!(
                    event = "headless.llm.request",
                    turn = turn + 1,
                    messages_count = messages.len(),
                    tools_count = tools.len(),
                    "headless llm request"
                );
            }

            let response = match self.llm.chat(messages.clone(), tools.clone()).await {
                Ok(r) => r,
                Err(e) => {
                    error!(
                        event = "headless.llm.error",
                        turn = turn + 1,
                        error = %e,
                        "headless llm request failed"
                    );
                    return Err(e);
                }
            };

            if Self::OBS_LLM {
                let preview = response
                    .content
                    .as_deref()
                    .map(|s| self.truncate_for_log(s))
                    .unwrap_or_default();
                info!(
                    event = "headless.llm.response",
                    turn = turn + 1,
                    tool_calls = response.tool_calls.len(),
                    content_preview = %preview,
                    "headless llm response"
                );
            }

            if response.tool_calls.is_empty() {
                if let Some(content) = response.content {
                    self.send_reply(client, &chat, &content).await?;
                }
                return Ok(());
            }

            let assistant_tool_calls: Vec<_> = response
                .tool_calls
                .iter()
                .map(|tc| {
                    json!({
                        "id": tc.id,
                        "type": "function",
                        "function": {
                            "name": tc.name,
                            "arguments": tc.arguments.to_string()
                        }
                    })
                })
                .collect();

            messages.push(json!({
                "role":"assistant",
                "content": response.content,
                "tool_calls": assistant_tool_calls
            }));

            for call in &response.tool_calls {
                if Self::OBS_TOOL {
                    info!(
                        event = "headless.tool.call",
                        turn = turn + 1,
                        tool_name = %call.name,
                        tool_call_id = %call.id,
                        args = %self.truncate_for_log(&call.arguments.to_string()),
                        "headless tool call"
                    );
                }

                let tool_result = self
                    .execute_skill(call, &chat, &groups, channel_group_id)
                    .await;

                if Self::OBS_TOOL {
                    info!(
                        event = "headless.tool.result",
                        turn = turn + 1,
                        tool_name = %call.name,
                        tool_call_id = %call.id,
                        result = %self.truncate_for_log(&tool_result),
                        "headless tool result"
                    );
                }

                messages.push(json!({
                    "role":"tool",
                    "tool_call_id": call.id,
                    "name": call.name,
                    "content": tool_result
                }));
            }
        }

        self.send_reply(client, &chat, "操作超时，请稍后再试")
            .await?;
        Err(anyhow!("headless tool loop reached max turns"))
    }

    async fn send_reply(
        &self,
        client: &mut VoiceServiceClient<Channel>,
        chat: &voicev1::ChatEvent,
        text: &str,
    ) -> Result<()> {
        let req = voicev1::NoticeRequest {
            message: text.to_string(),
            target_mode: chat.reply_target_mode,
            target_client_id: chat.reply_target_client_id,
        };
        let _ = client.send_notice(tonic::Request::new(req)).await?;
        Ok(())
    }
}
