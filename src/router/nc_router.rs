use crate::adapter::napcat::{
    event::{GroupMessageEvent, NcEvent, PrivateMessageEvent},
    types::{segments_to_text, Segment},
    NapCatAdapter,
};
use crate::adapter::TsAdapter;
use crate::config::{AppConfig, PromptsConfig};
use crate::llm::{provider::ToolCall, LlmEngine};
use crate::permission::PermissionGate;
use crate::router::{ChatHistory, ClientInfo, ReplyPolicy, UnifiedInboundEvent};
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
    history: ChatHistory,
}

impl NcRouter {
    fn resolve_nc_allowed_skills(&self, user_id: i64, group_id: Option<i64>) -> Vec<String> {
        // NC -> ACL 映射使用虚拟 server_group_ids：
        // 9000: 任意 NC 用户
        // 9001: 群消息上下文
        // 9002: trusted_users 中用户
        // 9003: trusted_groups 中群成员
        let mut pseudo_groups = vec![9000u32];
        if group_id.is_some() {
            pseudo_groups.push(9001);
        }
        if self.config.napcat.trusted_users.contains(&user_id) {
            pseudo_groups.push(9002);
        }
        if group_id
            .map(|gid| self.config.napcat.trusted_groups.contains(&gid))
            .unwrap_or(false)
        {
            pseudo_groups.push(9003);
        }
        self.gate.get_allowed_skills(&pseudo_groups, 0)
    }

    fn is_trusted(&self, user_id: i64, group_id: Option<i64>) -> bool {
        let nc = &self.config.napcat;
        if nc.trusted_users.contains(&user_id) {
            return true;
        }
        if let Some(gid) = group_id {
            if nc.trusted_groups.contains(&gid) {
                return true;
            }
        }
        false
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
            history: ChatHistory::new(),
        }
    }

    pub async fn run(&self) -> Result<()> {
        let mut rx = self.adapter.subscribe();
        info!("NcRouter: listening for NapCat events");

        while let Ok(event) = rx.recv().await {
            match event {
                NcEvent::PrivateMessage(msg) => {
                    if msg.user_id == self.adapter.get_self_id() {
                        continue;
                    }
                    if !self.is_trusted(msg.user_id, None) {
                        info!("NC: Ignored untrusted user {}", msg.user_id);
                        continue;
                    }
                    self.spawn_handle_private(msg);
                }
                NcEvent::GroupMessage(msg) => {
                    if msg.user_id == self.adapter.get_self_id() {
                        continue;
                    }
                    let nc = &self.config.napcat;
                    // 群组白名单过滤
                    if !nc.listen_groups.is_empty() && !nc.listen_groups.contains(&msg.group_id) {
                        continue;
                    }
                    // 信任检查
                    if !self.is_trusted(msg.user_id, Some(msg.group_id)) {
                        info!(
                            "NC: Ignored untrusted user {} in group {}",
                            msg.user_id, msg.group_id
                        );
                        continue;
                    }
                    self.spawn_handle_group(msg);
                }
                NcEvent::Heartbeat => {
                    debug!("NapCat heartbeat");
                }
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
        let history = self.history.clone();

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
                history,
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
        let history = self.history.clone();

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
                history,
            };
            router.handle_group(msg).await;
        });
    }

    async fn handle_private(&self, msg: PrivateMessageEvent) {
        let Some(unified_event) = UnifiedInboundEvent::from_nc_private(&msg) else {
            return;
        };
        debug!(
            source = ?unified_event.source,
            sender_id = %unified_event.sender_id,
            sender_name = %unified_event.sender_name,
            trace_id = %unified_event.trace_id,
            should_trigger_llm = unified_event.should_trigger_llm,
            "NC private unified inbound event"
        );
        if !unified_event.should_respond {
            return;
        }
        debug!("NC private event timestamp={}", msg.timestamp);

        let stripped = self.strip_prefix(&unified_event.text);

        info!("[NC Private] user={} msg={}", msg.sender.nickname, stripped);

        let allowed = self.resolve_nc_allowed_skills(msg.user_id, None);
        debug!("NC private allowed skills: {:?}", allowed);

        let reply_text = self
            .run_llm(stripped, &msg.sender.nickname, msg.user_id, None, &allowed)
            .await;

        if let ReplyPolicy::NapCatPrivate { user_id } = unified_event.reply_policy {
            let segs = vec![Segment::text(&reply_text)];
            if let Err(e) = self.adapter.send_private(user_id, &segs).await {
                error!("NC send_private failed: {e}");
            }
        }
    }

    async fn handle_group(&self, msg: GroupMessageEvent) {
        let triggered = self.is_triggered(segments_to_text(&msg.message).trim());
        let Some(unified_event) = UnifiedInboundEvent::from_nc_group(&msg, triggered) else {
            return;
        };
        debug!(
            source = ?unified_event.source,
            sender_id = %unified_event.sender_id,
            sender_name = %unified_event.sender_name,
            trace_id = %unified_event.trace_id,
            should_trigger_llm = unified_event.should_trigger_llm,
            "NC group unified inbound event"
        );
        if !unified_event.should_respond {
            return;
        }
        debug!("NC group event timestamp={}", msg.timestamp);

        let stripped = self.strip_prefix(&unified_event.text);

        info!(
            "[NC Group {}] user={} msg={}",
            msg.group_id, msg.sender.nickname, stripped
        );

        let allowed = self.resolve_nc_allowed_skills(msg.user_id, Some(msg.group_id));
        debug!("NC group allowed skills: {:?}", allowed);

        let reply_text = self
            .run_llm(
                stripped,
                &msg.sender.nickname,
                msg.user_id,
                Some(msg.group_id),
                &allowed,
            )
            .await;

        if let ReplyPolicy::NapCatGroup {
            group_id,
            at_user_id,
        } = unified_event.reply_policy
        {
            let mut segs = Vec::new();
            if let Some(uid) = at_user_id {
                segs.push(Segment::at(uid));
                segs.push(Segment::text(" "));
            }
            segs.push(Segment::text(&reply_text));
            if let Err(e) = self.adapter.send_group(group_id, &segs).await {
                error!("NC send_group failed: {e}");
            }
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

    /// 执行单个工具调用，返回结果字符串
    async fn execute_skill(
        &self,
        call: &ToolCall,
        user_id: i64,
        group_id: Option<i64>,
        sender_name: &str,
    ) -> String {
        if let Some(skill) = self.registry.get(&call.name) {
            // 构建统一执行上下文（包含 TS 和 NC 两个平台）
            let nc_ctx = NcExecutionContext {
                adapter: self.adapter.clone(),
                caller_id: user_id,
                caller_name: sender_name.to_string(),
                caller_group_id: group_id,
                gate: self.gate.clone(),
                config: self.config.clone(),
                error_prompts: &self.prompts.error,
            };
            let mut unified_ctx = UnifiedExecutionContext::from_nc(&nc_ctx).with_cross_adapters(
                self.ts_adapter.clone(),
                self.ts_clients.as_ref().map(|c| c.as_ref()),
                Some(self.adapter.clone()),
            );
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
                Err(_unified_err) => {
                    // 2. 回退到 NC 平台特定执行
                    let nc_ctx = NcExecutionContext {
                        adapter: self.adapter.clone(),
                        caller_id: user_id,
                        caller_name: sender_name.to_string(),
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
        }
    }

    /// 调用 LLM + Skill 系统，支持多轮工具调用，返回最终文本回复
    async fn run_llm(
        &self,
        user_msg: &str,
        sender_name: &str,
        user_id: i64,
        group_id: Option<i64>,
        allowed_skills: &[String],
    ) -> String {
        let error_msg = self.prompts.error.llm_error.clone();
        let max_turns = self.config.bot.max_tool_turns;

        let system_prompt = &self.prompts.system.content;
        let user_ctx = match group_id {
            Some(gid) => format!("User: {} (QQ: {}, Group: {})", sender_name, user_id, gid),
            None => format!("User: {} (QQ: {}, Private Chat)", sender_name, user_id),
        };

        let mut messages = vec![
            json!({"role": "system", "content": system_prompt}),
            json!({"role": "system", "content": user_ctx}),
        ];

        let session_key = user_id.unsigned_abs() as u32;
        let ctx_window = self.config.llm.context_window as usize;
        for msg in self.history.get_history(session_key, ctx_window) {
            messages.push(msg);
        }
        messages.push(json!({"role": "user", "content": user_msg}));

        let tools = self.registry.to_tool_schemas(allowed_skills);

        for turn in 0..max_turns {
            debug!("[NC] LLM turn {}/{}", turn + 1, max_turns);

            match self.llm.chat(messages.clone(), tools.clone()).await {
                Ok(response) => {
                    // 没有工具调用，返回最终内容
                    if response.tool_calls.is_empty() {
                        let content = response.content.clone().unwrap_or_default();
                        info!("[NC] LLM final reply (turn {}): {}", turn + 1, content);
                        self.history
                            .save_turn(session_key, user_msg, &content, ctx_window);
                        return content;
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

                    // 执行所有工具调用
                    for call in &response.tool_calls {
                        let tool_result = self
                            .execute_skill(call, user_id, group_id, sender_name)
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
                    error!("NC LLM error (turn {}): {}", turn + 1, e);
                    return error_msg;
                }
            }
        }

        // 达到最大轮数，尝试获取最后一个可用的回复
        warn!("[NC] Reached max tool turns ({})", max_turns);
        "操作超时，请稍后再试".to_string()
    }
}
