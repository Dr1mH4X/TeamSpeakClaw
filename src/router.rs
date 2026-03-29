use crate::adapter::command::cmd_send_text;
use crate::adapter::{TextMessageEvent, TextMessageTarget, TsAdapter, TsEvent};
use crate::config::{AppConfig, PromptsConfig};
use crate::llm::LlmEngine;
use crate::permission::PermissionGate;
use crate::skills::{ExecutionContext, SkillRegistry};
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
}

impl EventRouter {
    pub fn new(
        config: Arc<AppConfig>,
        prompts: Arc<PromptsConfig>,
        adapter: Arc<TsAdapter>,
        gate: Arc<PermissionGate>,
        llm: Arc<LlmEngine>,
        registry: Arc<SkillRegistry>,
    ) -> Self {
        Self::new_with_clients(config, prompts, adapter, gate, llm, registry, Arc::new(DashMap::new()))
    }

    pub fn new_with_clients(
        config: Arc<AppConfig>,
        prompts: Arc<PromptsConfig>,
        adapter: Arc<TsAdapter>,
        gate: Arc<PermissionGate>,
        llm: Arc<LlmEngine>,
        registry: Arc<SkillRegistry>,
        clients: Arc<DashMap<u32, ClientInfo>>,
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

                    tokio::spawn(async move {
                        let _permit = match semaphore.acquire().await {
                            Ok(p) => p,
                            Err(e) => {
                                error!("Failed to acquire semaphore: {}", e);
                                return;
                            }
                        };

                        let router = Self::new_with_clients(config, prompts, adapter, gate, llm, registry, clients);
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

    async fn handle_message(&self, event: TextMessageEvent) {
        // 按客户端ID忽略自身
        if event.invoker_id == self.adapter.get_bot_clid() {
            return;
        }

        // 只响应私信或由前缀触发的消息
        let is_private = event.target_mode == TextMessageTarget::Private;
        let msg_content = event.message.trim();
        let triggers = &self.config.bot.trigger_prefixes;

        let should_respond = is_private && self.config.bot.respond_to_private
            || triggers
                .iter()
                .any(|prefix| msg_content.starts_with(prefix));

        if !should_respond {
            return;
        }

        // 确定回复目标 (targetmode, target)
        let (reply_mode, reply_target) = if is_private {
            // 私聊触发始终私聊回复
            (1u8, event.invoker_id)
        } else {
            match self.config.bot.default_reply_mode.as_str() {
                "channel" => (2, 0),
                "server" => (3, 0),
                _ => (1, event.invoker_id),
            }
        };

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
            json!({"role": "user", "content": msg_content}),
        ];

        // 2. 获取工具
        let allowed_skills = self.gate.get_allowed_skills(&groups, channel_group_id);
        let tools = self.registry.to_tool_schemas(&allowed_skills);

        // 3. 第一次LLM调用
        match self.llm.chat(messages.clone(), tools.clone()).await {
            Ok(response) => {
                // 4. 处理响应
                if response.tool_calls.is_empty() {
                    if let Some(content) = response.content {
                        let _ = self
                            .adapter
                            .send_raw(&cmd_send_text(reply_mode, reply_target, &content))
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
                            clients: &self.clients,
                            caller_id: event.invoker_id,
                            caller_groups: groups.clone(),
                            caller_channel_group_id: channel_group_id,
                            gate: self.gate.clone(),
                            config: self.config.clone(),
                            error_prompts: &self.prompts.error,
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
                            }
                            Err(e) => {
                                let err_msg = self.prompts.error.skill_error.replace("{detail}", &e.to_string());
                                error!(
                                    skill = %call.name,
                                    caller = %event.invoker_name,
                                    args = %call.arguments,
                                    error = %e,
                                    "技能执行失败"
                                );
                                err_msg
                            }
                        }
                    } else {
                        self.prompts.error.skill_not_found.clone()
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
                                .send_raw(&cmd_send_text(reply_mode, reply_target, &content))
                                .await;
                        }
                    }
                    Err(e) => {
                        error!("LLM error (2nd turn): {e}");
                        let _ = self
                            .adapter
                            .send_raw(&cmd_send_text(
                                reply_mode,
                                reply_target,
                                &self.prompts.error.llm_error,
                            ))
                            .await;
                    }
                }
            }
            Err(e) => {
                error!("LLM error: {e}");
                let _ = self
                    .adapter
                    .send_raw(&cmd_send_text(
                        reply_mode,
                        reply_target,
                        &self.prompts.error.llm_error,
                    ))
                    .await;
            }
        }
    }
}
