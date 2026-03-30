use crate::adapter::napcat::{
    event::{GroupMessageEvent, NcEvent, PrivateMessageEvent},
    types::{segments_to_text, Segment},
    NapCatAdapter,
};
use crate::adapter::TsAdapter;
use crate::config::{AppConfig, PromptsConfig};
use crate::llm::LlmEngine;
use crate::permission::PermissionGate;
use crate::router::ClientInfo;
use crate::skills::{NcExecutionContext, SkillRegistry, UnifiedExecutionContext};
use anyhow::Result;
use dashmap::DashMap;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};

pub struct NcRouter {
    config: Arc<AppConfig>,
    prompts: Arc<PromptsConfig>,
    adapter: Arc<NapCatAdapter>,
    gate: Arc<PermissionGate>,
    llm: Arc<LlmEngine>,
    registry: Arc<SkillRegistry>,
    semaphore: Arc<Semaphore>,
    ts_adapter: Option<Arc<TsAdapter>>,
    ts_clients: Option<Arc<DashMap<u32, ClientInfo>>>,
}

impl NcRouter {
    pub fn new(
        config: Arc<AppConfig>,
        prompts: Arc<PromptsConfig>,
        adapter: Arc<NapCatAdapter>,
        gate: Arc<PermissionGate>,
        llm: Arc<LlmEngine>,
        registry: Arc<SkillRegistry>,
    ) -> Self {
        Self::new_with_ts(config, prompts, adapter, gate, llm, registry, None, None)
    }

    pub fn new_with_ts(
        config: Arc<AppConfig>,
        prompts: Arc<PromptsConfig>,
        adapter: Arc<NapCatAdapter>,
        gate: Arc<PermissionGate>,
        llm: Arc<LlmEngine>,
        registry: Arc<SkillRegistry>,
        ts_adapter: Option<Arc<TsAdapter>>,
        ts_clients: Option<Arc<DashMap<u32, ClientInfo>>>,
    ) -> Self {
        let max_concurrent = config.bot.max_concurrent_requests;
        Self {
            config,
            prompts,
            adapter,
            gate,
            llm,
            registry,
            semaphore: Arc::new(Semaphore::new(max_concurrent as usize)),
            ts_adapter,
            ts_clients,
        }
    }

    pub async fn run(&self) -> Result<()> {
        let mut rx = self.adapter.subscribe();
        info!("NcRouter: listening for NapCat events");

        while let Ok(event) = rx.recv().await {
            match event {
                NcEvent::PrivateMessage(msg) => {
                    if msg.user_id == self.adapter.get_self_id() {
                        continue; // 忽略自身消息
                    }
                    if !self.config.napcat.respond_to_private {
                        continue;
                    }
                    self.spawn_handle_private(msg);
                }
                NcEvent::GroupMessage(msg) => {
                    if msg.user_id == self.adapter.get_self_id() {
                        continue;
                    }
                    // 群组白名单过滤
                    let nc = &self.config.napcat;
                    if !nc.listen_groups.is_empty() && !nc.listen_groups.contains(&msg.group_id) {
                        continue;
                    }
                    self.spawn_handle_group(msg);
                }
                NcEvent::Heartbeat => {
                    debug!("NapCat heartbeat");
                }
                NcEvent::Lifecycle(lc) => {
                    info!("NapCat lifecycle: {:?}", lc.sub_type);
                }
                _ => {}
            }
        }
        Err(anyhow::anyhow!("NcRouter event stream ended"))
    }

    fn spawn_handle_private(&self, msg: PrivateMessageEvent) {
        let config = self.config.clone();
        let prompts = self.prompts.clone();
        let adapter = self.adapter.clone();
        let gate = self.gate.clone();
        let llm = self.llm.clone();
        let registry = self.registry.clone();
        let semaphore = self.semaphore.clone();
        let ts_adapter = self.ts_adapter.clone();
        let ts_clients = self.ts_clients.clone();

        tokio::spawn(async move {
            let _permit = match semaphore.acquire().await {
                Ok(p) => p,
                Err(e) => {
                    error!("NcRouter semaphore error: {e}");
                    return;
                }
            };
            let router = NcRouter {
                config,
                prompts,
                adapter: adapter.clone(),
                gate,
                llm,
                registry,
                semaphore: Arc::new(Semaphore::new(1)),
                ts_adapter,
                ts_clients,
            };
            router.handle_private(msg).await;
        });
    }

    fn spawn_handle_group(&self, msg: GroupMessageEvent) {
        let config = self.config.clone();
        let prompts = self.prompts.clone();
        let adapter = self.adapter.clone();
        let gate = self.gate.clone();
        let llm = self.llm.clone();
        let registry = self.registry.clone();
        let semaphore = self.semaphore.clone();
        let ts_adapter = self.ts_adapter.clone();
        let ts_clients = self.ts_clients.clone();

        tokio::spawn(async move {
            let _permit = match semaphore.acquire().await {
                Ok(p) => p,
                Err(e) => {
                    error!("NcRouter semaphore error: {e}");
                    return;
                }
            };
            let router = NcRouter {
                config,
                prompts,
                adapter: adapter.clone(),
                gate,
                llm,
                registry,
                semaphore: Arc::new(Semaphore::new(1)),
                ts_adapter,
                ts_clients,
            };
            router.handle_group(msg).await;
        });
    }

    async fn handle_private(&self, msg: PrivateMessageEvent) {
        let text = segments_to_text(&msg.message);
        let text = text.trim();
        if text.is_empty() {
            return;
        }

        // 触发词检查（私聊可以不检查触发词，直接响应）
        let nc = &self.config.napcat;
        let triggered = if nc.respond_to_private {
            true
        } else {
            self.is_triggered(text)
        };
        if !triggered {
            return;
        }

        let stripped = self.strip_prefix(text);

        info!("[NC Private] user={} msg={}", msg.sender.nickname, stripped);

        // QQ 用户在权限层没有分组概念，使用空组列表（由 acl default 规则兜底）
        let allowed = self.gate.get_allowed_skills(&[], 0);

        let reply_text = self
            .run_llm(stripped, &msg.sender.nickname, msg.user_id, None, &allowed)
            .await;

        let segs = vec![Segment::text(&reply_text)];
        if let Err(e) = self.adapter.send_private(msg.user_id, &segs).await {
            error!("NC send_private failed: {e}");
        }
    }

    async fn handle_group(&self, msg: GroupMessageEvent) {
        let text = segments_to_text(&msg.message);
        let text = text.trim();
        if text.is_empty() {
            return;
        }

        if !self.is_triggered(text) {
            return;
        }

        let stripped = self.strip_prefix(text);

        info!(
            "[NC Group {}] user={} msg={}",
            msg.group_id, msg.sender.nickname, stripped
        );

        let allowed = self.gate.get_allowed_skills(&[], 0);

        let reply_text = self
            .run_llm(
                stripped,
                &msg.sender.nickname,
                msg.user_id,
                Some(msg.group_id),
                &allowed,
            )
            .await;

        // 群消息回复带 @
        let segs = vec![
            Segment::at(msg.user_id),
            Segment::text(" "),
            Segment::text(&reply_text),
        ];
        if let Err(e) = self.adapter.send_group(msg.group_id, &segs).await {
            error!("NC send_group failed: {e}");
        }
    }

    /// 是否匹配触发词
    fn is_triggered(&self, text: &str) -> bool {
        let nc = &self.config.napcat;
        let self_id = self.adapter.get_self_id().to_string();
        // @bot 触发
        if text.contains(&format!("[CQ:at,qq={self_id}]")) {
            return true;
        }
        nc.trigger_prefixes
            .iter()
            .any(|p| text.starts_with(p.as_str()))
    }

    /// 去除触发词前缀
    fn strip_prefix<'a>(&self, text: &'a str) -> &'a str {
        let nc = &self.config.napcat;
        for p in &nc.trigger_prefixes {
            if let Some(rest) = text.strip_prefix(p.as_str()) {
                return rest.trim();
            }
        }
        text
    }

    /// 调用 LLM + Skill 系统，返回最终文本回复
    async fn run_llm(
        &self,
        user_msg: &str,
        sender_name: &str,
        user_id: i64,
        group_id: Option<i64>,
        allowed_skills: &[String],
    ) -> String {
        let error_msg = self.prompts.error.llm_error.clone();

        let system_prompt = &self.prompts.system.content;
        let user_ctx = match group_id {
            Some(gid) => format!("User: {} (QQ: {}, Group: {})", sender_name, user_id, gid),
            None => format!("User: {} (QQ: {}, Private Chat)", sender_name, user_id),
        };

        let mut messages = vec![
            json!({"role": "system", "content": system_prompt}),
            json!({"role": "system", "content": user_ctx}),
            json!({"role": "user", "content": user_msg}),
        ];

        // 注意：NapCat Skill 没有 TS 客户端列表，使用专用上下文
        let tools = self.registry.to_tool_schemas(allowed_skills);

        match self.llm.chat(messages.clone(), tools.clone()).await {
            Ok(response) => {
                if response.tool_calls.is_empty() {
                    return response.content.unwrap_or_default();
                }

                // 准备工具调用历史
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
                        // 构建统一执行上下文（包含 TS 和 NC 两个平台）
                        let mut unified_ctx =
                            UnifiedExecutionContext::from_nc(&NcExecutionContext {
                                adapter: self.adapter.clone(),
                                caller_id: user_id,
                                caller_group_id: group_id,
                                gate: self.gate.clone(),
                                config: self.config.clone(),
                                error_prompts: &self.prompts.error,
                            });
                        // 添加跨平台 adapter
                        if let Some(ref ts_adapter) = self.ts_adapter {
                            unified_ctx.ts_adapter = Some(ts_adapter.clone());
                        }
                        if let Some(ref ts_clients) = self.ts_clients {
                            unified_ctx.ts_clients = Some(ts_clients.as_ref());
                        }

                        // 1. 优先尝试统一执行（跨平台支持）
                        let args = call.arguments.clone();
                        match skill.execute_unified(args, &unified_ctx).await {
                            Ok(val) => {
                                info!(
                                    skill = %call.name,
                                    caller = %sender_name,
                                    result = %val,
                                    "NC Unified Skill executed"
                                );
                                val.to_string()
                            }
                            Err(unified_err) => {
                                // 2. 回退到 NC 平台特定执行
                                let nc_ctx = NcExecutionContext {
                                    adapter: self.adapter.clone(),
                                    caller_id: user_id,
                                    caller_group_id: group_id,
                                    gate: self.gate.clone(),
                                    config: self.config.clone(),
                                    error_prompts: &self.prompts.error,
                                };
                                match skill.execute_nc(call.arguments.clone(), &nc_ctx).await {
                                    Ok(val) => {
                                        info!(
                                            skill = %call.name,
                                            caller = %sender_name,
                                            result = %val,
                                            "NC Skill executed"
                                        );
                                        val.to_string()
                                    }
                                    Err(e) => {
                                        let msg = self
                                            .prompts
                                            .error
                                            .skill_error
                                            .replace("{detail}", &e.to_string());
                                        error!(skill = %call.name, error = %e, "NC Skill failed");
                                        msg
                                    }
                                }
                            }
                        }
                    } else {
                        warn!(skill = %call.name, "NC Skill not found");
                        self.prompts.error.skill_not_found.clone()
                    };

                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": call.id,
                        "name": call.name,
                        "content": tool_result
                    }));
                }

                // 二轮 LLM
                match self.llm.chat(messages, tools).await {
                    Ok(final_resp) => final_resp.content.unwrap_or_default(),
                    Err(e) => {
                        error!("NC LLM 2nd turn error: {e}");
                        error_msg
                    }
                }
            }
            Err(e) => {
                error!("NC LLM error: {e}");
                error_msg
            }
        }
    }
}
