use crate::adapter::{TextMessageEvent, TextMessageTarget, TsEvent, UnifiedAdapter};
use crate::audit::AuditLog;
use crate::cache::ClientCache;
use crate::config::{AppConfig, PromptsConfig};
use crate::error::AppError;
use crate::llm::LlmEngine;
use crate::permission::PermissionGate;
use crate::skills::{ExecutionContext, SkillRegistry};
use anyhow::Result;
use arc_swap::ArcSwap;
use serde_json::json;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

pub struct EventRouter {
    config: Arc<ArcSwap<AppConfig>>,
    prompts: Arc<PromptsConfig>,
    adapter: Arc<UnifiedAdapter>,
    cache: Arc<ClientCache>,
    gate: Arc<PermissionGate>,
    llm: Arc<LlmEngine>,
    registry: Arc<SkillRegistry>,
    audit: Arc<AuditLog>,
}

impl EventRouter {
    pub fn new(
        config: Arc<ArcSwap<AppConfig>>,
        prompts: Arc<PromptsConfig>,
        adapter: Arc<UnifiedAdapter>,
        cache: Arc<ClientCache>,
        gate: Arc<PermissionGate>,
        llm: Arc<LlmEngine>,
        registry: Arc<SkillRegistry>,
        audit: Arc<AuditLog>,
    ) -> Self {
        Self {
            config,
            prompts,
            adapter,
            cache,
            gate,
            llm,
            registry,
            audit,
        }
    }

    pub async fn run(&self) -> Result<()> {
        let mut rx = self.adapter.subscribe();

        while let Ok(event) = rx.recv().await {
            match event {
                TsEvent::TextMessage(msg) => {
                    self.handle_message(msg).await;
                }
                TsEvent::ClientEnterView(e) => {
                    debug!(
                        "Cache updated: Client {} ({}) entered view",
                        e.client_nickname, e.clid
                    );
                    self.cache.update_client(
                        e.clid,
                        e.cldbid,
                        e.client_nickname,
                        e.client_server_groups,
                    );
                }
                TsEvent::ClientLeftView(e) => {
                    self.cache.remove_client(e.clid);
                    debug!("Cache updated: Client {} left view", e.clid);
                }
                _ => {}
            }
        }
        Ok(())
    }

    async fn handle_message(&self, event: TextMessageEvent) {
        // Ignore self by client ID
        if event.invoker_id == self.adapter.get_bot_clid() {
            return;
        }

        // 审计日志前置: 记录所有接收到的消息，无论是否被过滤
        // Audit log moved before filter logic
        self.audit.log(
            "message_received",
            json!({
                "invoker": event.invoker_name,
                "clid": event.invoker_id,
                "content": event.message
            }),
        );

        // Only respond to private messages or if triggered by prefix
        let is_private = event.target_mode == TextMessageTarget::Private;
        let msg_content = event.message.trim();
        let triggers = &self.config.load().bot.trigger_prefixes;

        let should_respond = is_private && self.config.load().bot.respond_to_private
            || triggers
                .iter()
                .any(|prefix| msg_content.starts_with(prefix));

        if !should_respond {
            return;
        }

        info!(
            "Handling message from {}: {}",
            event.invoker_name, msg_content
        );

        let groups = if let Some(client) = self.cache.get_client(event.invoker_id) {
            client.server_groups
        } else {
            debug!(
                "Client {} not in cache, assuming default permissions",
                event.invoker_id
            );
            vec![]
        };

        // 1. Prepare context
        let system_prompt = &self.prompts.system.content;
        let user_ctx = format!(
            "User: {} (clid: {}, groups: {:?})",
            event.invoker_name, event.invoker_id, groups
        );

        let mut messages = vec![
            json!({"role": "system", "content": system_prompt}),
            json!({"role": "system", "content": user_ctx}),
            json!({"role": "user", "content": msg_content}),
        ];

        // 2. Get tools
        let allowed_skills = self.gate.get_allowed_skills(&groups);
        let tools = self.registry.to_tool_schemas(&allowed_skills);

        // 多轮对话循环: 允许LLM执行工具后根据结果决定是否继续调用其他工具
        // Multi-turn loop: allows LLM to execute tools and decide next steps based on results
        let max_turns = 10;
        let mut turn_count = 0;

        loop {
            if turn_count >= max_turns {
                warn!(
                    "Max conversation turns ({}) reached for user {}",
                    max_turns, event.invoker_name
                );
                let _ = self
                    .send_text(event.invoker_id, "System limit reached.")
                    .await;
                break;
            }
            turn_count += 1;

            // 3. LLM call
            match self.llm.chat(messages.clone(), tools.clone()).await {
                Ok(response) => {
                    // 如果没有工具调用，说明LLM已生成最终回复
                    if response.tool_calls.is_empty() {
                        if let Some(content) = response.content {
                            let _ = self.send_text(event.invoker_id, &content).await;
                        }
                        break;
                    }

                    // 准备工具调用历史记录 (Assistant Message)
                    let assistant_tool_calls: Vec<_> =
                        response.tool_calls.iter().map(|tc| json!(tc)).collect();

                    messages.push(json!({
                        "role": "assistant",
                        "content": response.content,
                        "tool_calls": assistant_tool_calls
                    }));

                    // 执行工具
                    // Execute tools
                    for call in response.tool_calls {
                        let tool_result =
                            if let Some(skill) = self.registry.get(&call.function.name) {
                                // 权限检查
                                // Permission check
                                if let Err(e) = self.gate.check(&groups, &call.function.name) {
                                    let err_msg = match &e {
                                        AppError::PermissionDenied { .. } => {
                                            self.prompts.error.permission_denied.clone()
                                        }
                                        _ => format!("Permission Error: {e}"),
                                    };
                                    self.audit.log(
                                        "skill_denied",
                                        json!({
                                            "skill": call.function.name,
                                            "caller": event.invoker_name,
                                            "error": format!("{e}")
                                        }),
                                    );
                                    err_msg
                                } else {
                                    let ctx = ExecutionContext {
                                        adapter: self.adapter.clone(),
                                        cache: self.cache.clone(),
                                        caller_id: event.invoker_id,
                                        gate: self.gate.clone(),
                                    };

                                    let args: serde_json::Value =
                                        serde_json::from_str(&call.function.arguments)
                                            .unwrap_or(json!({}));

                                    // 技能执行与错误捕获
                                    // Skill execution & error handling
                                    match skill.execute(args.clone(), &ctx).await {
                                        Ok(val) => {
                                            self.audit.log(
                                                "skill_executed",
                                                json!({
                                                    "skill": call.function.name,
                                                    "caller": event.invoker_name,
                                                    "args": args,
                                                    "result": val
                                                }),
                                            );
                                            val.to_string()
                                        }
                                        Err(e) => {
                                            let err_msg = match &e {
                                                AppError::TargetProtected => {
                                                    self.prompts.error.target_protected.clone()
                                                }
                                                _ => format!("Execution Error: {e}"),
                                            };
                                            self.audit.log(
                                                "skill_failed",
                                                json!({
                                                    "skill": call.function.name,
                                                    "caller": event.invoker_name,
                                                    "args": args,
                                                    "error": format!("{e}")
                                                }),
                                            );
                                            err_msg
                                        }
                                    }
                                }
                            } else {
                                "Error: Skill not found".to_string()
                            };

                        // 将工具执行结果作为 Tool Message 加入历史
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": call.id,
                            "name": call.function.name,
                            "content": tool_result
                        }));
                    }
                    // 循环继续，将工具结果传回LLM进行下一轮处理
                }
                Err(e) => {
                    error!("LLM error: {e}");
                    let _ = self
                        .send_text(
                            event.invoker_id,
                            "Sorry, I encountered an error processing your request.",
                        )
                        .await;
                    break;
                }
            }
        }
    }

    // Helper for sending private text messages
    async fn send_text(&self, target_id: u32, content: &str) -> Result<()> {
        self.adapter.send_message(1, target_id, content).await?;
        Ok(())
    }
}
