use crate::adapter::command::cmd_send_text;
use crate::adapter::napcat::NapCatAdapter;
use crate::adapter::{TextMessageEvent, TsAdapter, TsEvent};
use crate::config::{AppConfig, PromptsConfig};
use crate::llm::{provider::ToolCall, LlmEngine};
use crate::permission::PermissionGate;
use crate::router::{ChatHistory, ReplyPolicy, UnifiedInboundEvent};
use crate::skills::{ExecutionContext, SkillRegistry, UnifiedExecutionContext};
use anyhow::Result;
use dashmap::DashMap;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};

#[derive(Debug, Clone)]
pub struct ClientInfo {
    pub clid: u32,
    pub cldbid: u32,
    pub nickname: String,
    pub server_groups: Vec<u32>,
    pub channel_group_id: u32,
}

pub struct EventRouter {
    config: Arc<AppConfig>,
    prompts: Arc<PromptsConfig>,
    adapter: Arc<TsAdapter>,
    pub clients: Arc<DashMap<u32, ClientInfo>>,
    gate: Arc<PermissionGate>,
    llm: Arc<LlmEngine>,
    registry: Arc<SkillRegistry>,
    semaphore: Arc<Semaphore>,
    nc_adapter: Option<Arc<NapCatAdapter>>,
    history: ChatHistory,
}

impl EventRouter {
    pub fn new_with_clients(
        config: Arc<AppConfig>,
        prompts: Arc<PromptsConfig>,
        adapter: Arc<TsAdapter>,
        gate: Arc<PermissionGate>,
        llm: Arc<LlmEngine>,
        registry: Arc<SkillRegistry>,
        clients: Arc<DashMap<u32, ClientInfo>>,
        nc_adapter: Option<Arc<NapCatAdapter>>,
    ) -> Self {
        let max_concurrent = config.bot.max_concurrent_requests;
        Self {
            config,
            prompts,
            adapter,
            clients,
            gate,
            llm,
            registry,
            semaphore: Arc::new(Semaphore::new(max_concurrent as usize)),
            nc_adapter,
            history: ChatHistory::new(),
        }
    }

    pub async fn run(&self) -> Result<()> {
        let mut rx = self.adapter.subscribe();

        let snapshot_result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.adapter.fetch_client_snapshot(),
        )
        .await;

        match snapshot_result {
            Ok(Ok(snapshot)) => {
                let snapshot_count = snapshot.len();
                for client in snapshot {
                    self.cache_client(
                        client.clid,
                        client.cldbid,
                        client.client_nickname,
                        client.client_server_groups,
                        client.client_channel_group_id,
                    );
                }
                info!(
                    "Prewarmed client cache with {} online clients",
                    snapshot_count
                );
            }
            Ok(Err(err)) => {
                warn!(
                    "Failed to prewarm client cache from snapshot, continuing without cache prewarm: {err}"
                );
            }
            Err(_) => {
                warn!(
                    "Timed out while prewarming client cache from snapshot, continuing without cache prewarm"
                );
            }
        }

        while let Ok(event) = rx.recv().await {
            match event {
                TsEvent::TextMessage(msg) => {
                    let config = self.config.clone();
                    let prompts = self.prompts.clone();
                    let adapter = self.adapter.clone();
                    let clients = self.clients.clone();
                    let gate = self.gate.clone();
                    let llm = self.llm.clone();
                    let registry = self.registry.clone();
                    let semaphore = self.semaphore.clone();
                    let nc_adapter = self.nc_adapter.clone();

                    tokio::spawn(async move {
                        let _permit = match semaphore.acquire().await {
                            Ok(p) => p,
                            Err(e) => {
                                error!("Failed to acquire semaphore: {}", e);
                                return;
                            }
                        };

                        let router = Self::new_with_clients(
                            config, prompts, adapter, gate, llm, registry, clients, nc_adapter,
                        );
                        router.handle_message(msg).await;
                    });
                }
                TsEvent::ClientEnterView(e) => {
                    self.cache_client(
                        e.clid,
                        e.cldbid,
                        e.client_nickname,
                        e.client_server_groups,
                        e.client_channel_group_id,
                    );
                }
                TsEvent::ClientLeftView(e) => {
                    self.clients.remove(&e.clid);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn cache_client(
        &self,
        clid: u32,
        cldbid: u32,
        nickname: String,
        server_groups: Vec<u32>,
        channel_group_id: u32,
    ) {
        self.clients.insert(
            clid,
            ClientInfo {
                clid,
                cldbid,
                nickname,
                server_groups,
                channel_group_id,
            },
        );
    }

    /// 执行单个工具调用，返回结果字符串
    async fn execute_skill(
        &self,
        call: &ToolCall,
        event: &TextMessageEvent,
        groups: &[u32],
        channel_group_id: u32,
    ) -> String {
        if let Some(skill) = self.registry.get(&call.name) {
            let ctx = ExecutionContext {
                adapter: self.adapter.clone(),
                clients: self.clients.as_ref(),
                caller_id: event.invoker_id,
                caller_name: event.invoker_name.clone(),
                caller_groups: groups.to_vec(),
                caller_channel_group_id: channel_group_id,
                gate: self.gate.clone(),
                config: self.config.clone(),
                error_prompts: &self.prompts.error,
            };
            let unified_ctx = UnifiedExecutionContext::from_ts(&ctx).with_cross_adapters(
                Some(self.adapter.clone()),
                Some(self.clients.as_ref()),
                self.nc_adapter.clone(),
            );
            let args = call.arguments.clone();
            let result = match skill.execute_unified(args.clone(), &unified_ctx).await {
                Ok(val) => {
                    info!(
                        skill = %call.name,
                        caller = %event.invoker_name,
                        args = %call.arguments,
                        result = %val,
                        "Unified skill executed successfully"
                    );
                    Ok(val)
                }
                Err(unified_err) => {
                    debug!(
                        skill = %call.name,
                        caller = %event.invoker_name,
                        error = %unified_err,
                        "Unified execution unavailable, falling back to TS execution"
                    );
                    skill.execute(args, &ctx).await
                }
            };
            match result {
                Ok(val) => {
                    info!(
                        skill = %call.name,
                        caller = %event.invoker_name,
                        args = %call.arguments,
                        result = %val,
                        "Skill executed successfully"
                    );
                    val.to_string()
                }
                Err(e) => {
                    let err_msg = self
                        .prompts
                        .error
                        .skill_error
                        .replace("{detail}", &e.to_string());
                    error!(
                        skill = %call.name,
                        caller = %event.invoker_name,
                        args = %call.arguments,
                        error = %e,
                        "Skill execution failed"
                    );
                    err_msg
                }
            }
        } else {
            warn!(
                caller = %event.invoker_name,
                skill = %call.name,
                "Skill not found"
            );
            self.prompts.error.skill_not_found.clone()
        }
    }

    async fn handle_message(&self, event: TextMessageEvent) {
        // 按客户端ID忽略自身
        if event.invoker_id == self.adapter.get_bot_clid() {
            return;
        }

        // 忽略 TS3AudioBot 自动回复（由 music skill 专用，不应走 LLM 流程）
        if event.invoker_name == "TS3AudioBot" {
            return;
        }

        let Some(unified_event) = UnifiedInboundEvent::from_ts(&event, &self.config) else {
            return;
        };
        debug!(
            source = ?unified_event.source,
            sender_id = %unified_event.sender_id,
            sender_name = %unified_event.sender_name,
            trace_id = %unified_event.trace_id,
            should_trigger_llm = unified_event.should_trigger_llm,
            "SQ unified inbound event"
        );
        if !unified_event.should_respond {
            return;
        }

        let (reply_mode, reply_target) = match unified_event.reply_policy {
            ReplyPolicy::TeamSpeak {
                target_mode,
                target,
            } => (target_mode, target),
            _ => {
                debug!(
                    "Unexpected reply policy for TS event: {:?}",
                    unified_event.reply_policy
                );
                return;
            }
        };
        let msg_content = unified_event.text.as_str();

        info!(
            "消息接收: {} (clid: {}, uid: {}, content: {})",
            event.invoker_name, event.invoker_id, event.invoker_uid, msg_content
        );

        let (groups, channel_group_id) = if let Some(client) = self.clients.get(&event.invoker_id) {
            (client.server_groups.clone(), client.channel_group_id)
        } else {
            debug!(
                "Client {} not in store, assuming default permissions",
                event.invoker_id
            );
            (vec![], 0)
        };

        // 1. 准备上下文
        let system_prompt = &self.prompts.system.content;
        let user_ctx = format!(
            "User: {} (clid: {}, groups: {:?}, channel_group: {})",
            event.invoker_name, event.invoker_id, groups, channel_group_id
        );

        let mut messages = vec![
            json!({"role": "system", "content": system_prompt}),
            json!({"role": "system", "content": user_ctx}),
        ];

        // 注入对话历史（如果启用）
        let ctx_window = self.config.llm.context_window as usize;
        if ctx_window > 0 {
            for msg in self
                .history
                .get_history(event.invoker_id as i64, ctx_window)
            {
                messages.push(msg);
            }
        }

        messages.push(json!({"role": "user", "content": msg_content}));

        // 2. 获取工具
        let allowed_skills = self.gate.get_allowed_skills(&groups, channel_group_id);
        let tools = self.registry.to_tool_schemas(&allowed_skills);
        let max_turns = self.config.bot.max_tool_turns;

        // 3. 多轮 LLM 调用循环
        for turn in 0..max_turns {
            debug!("[SQ] LLM turn {}/{}", turn + 1, max_turns);

            match self.llm.chat(messages.clone(), tools.clone()).await {
                Ok(response) => {
                    // 没有工具调用，发送最终内容
                    if response.tool_calls.is_empty() {
                        if let Some(ref content) = response.content {
                            info!("[SQ] LLM final reply (turn {}): {}", turn + 1, content);
                            let _ = self
                                .adapter
                                .send_raw(&cmd_send_text(reply_mode, reply_target, content))
                                .await;
                            self.history.save_turn(
                                event.invoker_id as i64,
                                msg_content,
                                content,
                                ctx_window,
                            );
                        }
                        return;
                    }

                    // 准备历史记录中的工具调用
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
                        "role": "assistant",
                        "content": response.content,
                        "tool_calls": assistant_tool_calls
                    }));

                    // 执行所有工具调用
                    for call in &response.tool_calls {
                        let tool_result = self
                            .execute_skill(call, &event, &groups, channel_group_id)
                            .await;
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": call.id,
                            "name": call.name,
                            "content": tool_result
                        }));
                    }

                    // 继续下一轮
                }
                Err(e) => {
                    error!("LLM error (turn {}): {}", turn + 1, e);
                    let _ = self
                        .adapter
                        .send_raw(&cmd_send_text(
                            reply_mode,
                            reply_target,
                            &self.prompts.error.llm_error,
                        ))
                        .await;
                    return;
                }
            }
        }

        // 达到最大轮数
        warn!("[SQ] Reached max tool turns ({})", max_turns);
        let _ = self
            .adapter
            .send_raw(&cmd_send_text(
                reply_mode,
                reply_target,
                "操作超时，请稍后再试",
            ))
            .await;
    }
}
