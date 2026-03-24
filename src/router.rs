use crate::adapter::command::cmd_send_text;
use crate::adapter::{TextMessageEvent, TextMessageTarget, TsAdapter, TsEvent};
use crate::cache::ClientCache;
use crate::config::{AppConfig, PromptsConfig};
use crate::llm::LlmEngine;
use crate::permission::PermissionGate;
use crate::skills::{ExecutionContext, SkillRegistry};
use anyhow::Result;
use arc_swap::ArcSwap;
use serde_json::json;
use std::sync::Arc;
use tracing::{debug, error, info};

pub struct EventRouter {
    config: Arc<ArcSwap<AppConfig>>,
    prompts: Arc<PromptsConfig>,
    adapter: Arc<TsAdapter>,
    cache: Arc<ClientCache>,
    gate: Arc<PermissionGate>,
    llm: Arc<LlmEngine>,
    registry: Arc<SkillRegistry>,
}

impl EventRouter {
    pub fn new(
        config: Arc<ArcSwap<AppConfig>>,
        prompts: Arc<PromptsConfig>,
        adapter: Arc<TsAdapter>,
        cache: Arc<ClientCache>,
        gate: Arc<PermissionGate>,
        llm: Arc<LlmEngine>,
        registry: Arc<SkillRegistry>,
    ) -> Self {
        Self {
            config,
            prompts,
            adapter,
            cache,
            gate,
            llm,
            registry,
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
                    self.cache.update_client(
                        e.clid,
                        e.cldbid,
                        e.client_nickname,
                        e.client_server_groups,
                    );
                }
                TsEvent::ClientLeftView(e) => {
                    self.cache.remove_client(e.clid);
                }
                _ => {}
            }
        }
        Ok(())
    }

    async fn handle_message(&self, event: TextMessageEvent) {
        // 按客户端ID忽略自身
        if event.invoker_id == self.adapter.get_bot_clid() {
            return;
        }

        // 只响应私信或由前缀触发的消息
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
            "消息接收: {} (clid: {}, content: {})",
            event.invoker_name, event.invoker_id, msg_content
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

        // 1. 准备上下文
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

        // 2. 获取工具
        let allowed_skills = self.gate.get_allowed_skills(&groups);
        let tools = self.registry.to_tool_schemas(&allowed_skills);

        // 3. 第一次LLM调用
        match self.llm.chat(messages.clone(), tools.clone()).await {
            Ok(response) => {
                // 4. 处理响应
                if response.tool_calls.is_empty() {
                    if let Some(content) = response.content {
                        let _ = self
                            .adapter
                            .send_raw(&cmd_send_text(1, event.invoker_id, &content))
                            .await;
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

                // 执行工具
                for call in response.tool_calls {
                    let tool_result = if let Some(skill) = self.registry.get(&call.name) {
                        let ctx = ExecutionContext {
                            adapter: self.adapter.clone(),
                            cache: self.cache.clone(),
                            caller_id: event.invoker_id,
                        };
                        match skill.execute(call.arguments.clone(), &ctx).await {
                            Ok(val) => {
                                info!(
                                    skill = %call.name,
                                    caller = %event.invoker_name,
                                    args = %call.arguments,
                                    result = %val,
                                    "技能执行成功"
                                );
                                val.to_string()
                            },
                            Err(e) => {
                                let err_msg = format!("Error: {e}");
                                error!(
                                    skill = %call.name,
                                    caller = %event.invoker_name,
                                    args = %call.arguments,
                                    error = %err_msg,
                                    "技能执行失败"
                                );
                                err_msg
                            },
                        }
                    } else {
                        "Error: Skill not found".to_string()
                    };

                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": call.id,
                        "name": call.name,
                        "content": tool_result
                    }));
                }

                // 5. 第二次LLM调用（包含工具结果）
                match self.llm.chat(messages, tools).await {
                    Ok(final_response) => {
                        if let Some(content) = final_response.content {
                            let _ = self
                                .adapter
                                .send_raw(&cmd_send_text(1, event.invoker_id, &content))
                                .await;
                        }
                    }
                    Err(e) => error!("LLM error (2nd turn): {e}"),
                }
            }
            Err(e) => {
                error!("LLM error: {e}");
                let _ = self
                    .adapter
                    .send_raw(&cmd_send_text(
                        1,
                        event.invoker_id,
                        "Sorry, I encountered an error processing your request.",
                    ))
                    .await;
            }
        }
    }
}
