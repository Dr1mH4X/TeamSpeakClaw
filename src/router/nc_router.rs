use async_trait::async_trait;

use crate::adapter::napcat::{
    event::{GroupMessageEvent, NcEvent, PrivateMessageEvent},
    types::{segments_to_text, Segment},
    NapCatAdapter,
};
use crate::adapter::TsAdapter;
use crate::config::{AppConfig, NapCatConfig, PromptsConfig};
use crate::llm::context::SessionSource;
use crate::llm::{LlmEngine, ToolCall, ToolExecutor, TurnPermit};
use crate::permission::PermissionGate;
use crate::router::{strip_trigger_prefix, ReplyPolicy, UnifiedInboundEvent};
use crate::skills::{is_skill_allowed, NcExecutionContext, SkillRegistry, UnifiedExecutionContext};
use anyhow::Result;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};

struct NcExecutor<'a> {
    router: &'a NcRouter,
    user_id: i64,
    group_id: Option<i64>,
    sender_name: &'a str,
    caller_groups: &'a [u32],
    allowed_skills: &'a [String],
}

#[async_trait]
impl ToolExecutor for NcExecutor<'_> {
    async fn execute(&self, call: &ToolCall) -> String {
        self.router
            .execute_skill(
                call,
                self.user_id,
                self.group_id,
                self.sender_name,
                self.caller_groups,
                self.allowed_skills,
            )
            .await
    }
}

pub struct NcRouter {
    config: Arc<AppConfig>,
    prompts: Arc<PromptsConfig>,
    adapter: Arc<NapCatAdapter>,
    gate: Arc<PermissionGate>,
    llm: Arc<LlmEngine>,
    registry: Arc<SkillRegistry>,
    ts_adapter: Option<Arc<TsAdapter>>,
}

fn nc_pseudo_groups(config: &NapCatConfig, user_id: i64, group_id: Option<i64>) -> Vec<u32> {
    let mut groups = vec![9000];
    if group_id.is_some() {
        groups.push(9001);
    }
    if config.trusted_users.contains(&user_id) {
        groups.push(9002);
    }
    if group_id.is_some_and(|gid| config.trusted_groups.contains(&gid)) {
        groups.push(9003);
    }
    groups
}

impl NcRouter {
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
    ) -> Self {
        Self {
            config,
            prompts,
            adapter,
            gate,
            llm,
            registry,
            ts_adapter,
        }
    }

    pub async fn run(&self) -> Result<()> {
        let mut rx = self.adapter.subscribe();
        info!("NcRouter: listening for NapCat events");

        let mut tasks = JoinSet::new();
        loop {
            let event = match rx.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(
                        skipped,
                        "NapCat event router lagged; skipped buffered events"
                    );
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    drain_nc_tasks(tasks).await;
                    return Err(anyhow::anyhow!("NcRouter event stream closed"));
                }
            };
            match event {
                NcEvent::PrivateMessage(msg) => {
                    if msg.user_id == self.adapter.get_self_id() {
                        continue;
                    }
                    if !self.is_trusted(msg.user_id, None) {
                        info!("NC: Ignored untrusted user {}", msg.user_id);
                        continue;
                    }
                    self.spawn_handle_private(&mut tasks, msg).await;
                }
                NcEvent::GroupMessage(msg) => {
                    if msg.user_id == self.adapter.get_self_id() {
                        continue;
                    }
                    let nc = &self.config.napcat;
                    if !nc.listen_groups.is_empty() && !nc.listen_groups.contains(&msg.group_id) {
                        continue;
                    }
                    if !self.is_trusted(msg.user_id, Some(msg.group_id)) {
                        info!(
                            "NC: Ignored untrusted user {} in group {}",
                            msg.user_id, msg.group_id
                        );
                        continue;
                    }
                    self.spawn_handle_group(&mut tasks, msg).await;
                }
                NcEvent::Heartbeat => {
                    debug!("NapCat heartbeat");
                }
            }
        }
    }

    async fn spawn_handle_private(&self, tasks: &mut JoinSet<()>, msg: PrivateMessageEvent) {
        let config = self.config.clone();
        let prompts = self.prompts.clone();
        let adapter = self.adapter.clone();
        let gate = self.gate.clone();
        let llm = self.llm.clone();
        let registry = self.registry.clone();
        let ts_adapter = self.ts_adapter.clone();

        let source = SessionSource::NapCatPrivate {
            user_id: msg.user_id,
        };
        let Ok(permit) = llm.try_reserve_turn(&source).await else {
            warn!(user_id = msg.user_id, "NC LLM turn queue full; dropping message");
            return;
        };

        tasks.spawn(async move {
            let router = NcRouter {
                config,
                prompts,
                adapter: adapter.clone(),
                gate,
                llm,
                registry,
                ts_adapter,
            };
            router.handle_private(msg, permit).await;
        });
    }

    async fn spawn_handle_group(&self, tasks: &mut JoinSet<()>, msg: GroupMessageEvent) {
        let config = self.config.clone();
        let prompts = self.prompts.clone();
        let adapter = self.adapter.clone();
        let gate = self.gate.clone();
        let llm = self.llm.clone();
        let registry = self.registry.clone();
        let ts_adapter = self.ts_adapter.clone();

        let source = SessionSource::NapCatGroup {
            group_id: msg.group_id,
        };
        let Ok(permit) = llm.try_reserve_turn(&source).await else {
            warn!(group_id = msg.group_id, "NC LLM turn queue full; dropping message");
            return;
        };

        tasks.spawn(async move {
            let router = NcRouter {
                config,
                prompts,
                adapter: adapter.clone(),
                gate,
                llm,
                registry,
                ts_adapter,
            };
            router.handle_group(msg, permit).await;
        });
    }

    async fn handle_private(&self, msg: PrivateMessageEvent, _turn: TurnPermit) {
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

        info!(
            user_id = msg.user_id,
            user = %msg.sender.nickname,
            message_chars = stripped.chars().count(),
            "[NC Private] message received"
        );

        if let Err(error) = self.llm.check_user_text_bounds(stripped) {
            warn!(error = %error, "NC message dropped for exceeding size limit");
            return;
        }

        let caller_groups = nc_pseudo_groups(&self.config.napcat, msg.user_id, None);

        let reply_text = self
            .run_llm(
                stripped,
                &msg.sender.nickname,
                msg.user_id,
                None,
                &caller_groups,
            )
            .await;

        if let ReplyPolicy::NapCatPrivate { user_id } = unified_event.reply_policy {
            let segs = vec![Segment::text(&reply_text)];
            if let Err(e) = self.adapter.send_private(user_id, &segs).await {
                error!("NC send_private failed: {e}");
                return;
            }
        }
        let source = SessionSource::NapCatPrivate {
            user_id: msg.user_id,
        };
        self.llm
            .save_turn(&source, stripped.to_string(), reply_text);
    }

    async fn handle_group(&self, msg: GroupMessageEvent, _turn: TurnPermit) {
        let triggered = self.is_triggered(&msg.message);
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
            group_id = msg.group_id,
            user_id = msg.user_id,
            user = %msg.sender.nickname,
            message_chars = stripped.chars().count(),
            "[NC Group] message received"
        );

        if let Err(error) = self.llm.check_user_text_bounds(stripped) {
            warn!(error = %error, "NC message dropped for exceeding size limit");
            return;
        }

        let caller_groups = nc_pseudo_groups(&self.config.napcat, msg.user_id, Some(msg.group_id));

        let reply_text = self
            .run_llm(
                stripped,
                &msg.sender.nickname,
                msg.user_id,
                Some(msg.group_id),
                &caller_groups,
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
                return;
            }
        }
        let source = SessionSource::NapCatGroup {
            group_id: msg.group_id,
        };
        self.llm
            .save_turn(&source, stripped.to_string(), reply_text);
    }

    fn is_triggered(&self, message: &[Segment]) -> bool {        let nc = &self.config.napcat;
        let self_id = self.adapter.get_self_id().to_string();
        if message
            .iter()
            .any(|segment| matches!(segment, Segment::At { qq } if qq == &self_id))
        {
            return true;
        }
        let text = segments_to_text(message);
        let text = text.trim();
        if text.contains(&format!("[CQ:at,qq={self_id}]")) {
            return true;
        }
        nc.trigger_prefixes
            .iter()
            .any(|p| text.starts_with(p.as_str()))
    }

    fn strip_prefix<'a>(&self, text: &'a str) -> &'a str {
        strip_trigger_prefix(text, &self.config.napcat.trigger_prefixes).unwrap_or(text)
    }

    /// 执行单个工具调用，返回结果字符串
    async fn execute_skill(
        &self,
        call: &ToolCall,
        user_id: i64,
        group_id: Option<i64>,
        sender_name: &str,
        caller_groups: &[u32],
        allowed_skills: &[String],
    ) -> String {
        if !is_skill_allowed(&call.name, allowed_skills) {
            warn!(skill = %call.name, "NC Skill execution denied by ACL");
            return "Skill execution denied".to_string();
        }

        if let Some(skill) = self.registry.get(&call.name) {
            let nc_ctx = NcExecutionContext {
                adapter: self.adapter.clone(),
                caller_id: user_id,
                caller_name: sender_name.to_string(),
                caller_groups: caller_groups.to_vec(),
                caller_group_id: group_id,
                gate: self.gate.clone(),
                config: self.config.clone(),
            };
            let unified_ctx = UnifiedExecutionContext::from_nc(&nc_ctx)
                .with_cross_adapters(self.ts_adapter.clone(), Some(self.adapter.clone()));

            match skill
                .execute_unified(call.arguments.clone(), &unified_ctx)
                .await
            {
                Ok(val) => {
                    info!(
                        skill = %call.name,
                        caller = %sender_name,
                        "NC Unified Skill executed"
                    );
                    val.to_string()
                }
                Err(e) => {
                    let msg = format!("Skill execution failed: {}", e);
                    error!(skill = %call.name, error = %e, "NC Skill failed");
                    msg
                }
            }
        } else {
            warn!(skill = %call.name, "NC Skill not found");
            "Skill not found".to_string()
        }
    }

    /// 调用 LLM + Skill 系统，支持多轮工具调用，返回最终文本回复
    async fn run_llm(
        &self,
        user_msg: &str,
        sender_name: &str,
        user_id: i64,
        group_id: Option<i64>,
        caller_groups: &[u32],
    ) -> String {
        let error_msg = "AI backend unavailable. Please try again later.".to_string();

        let source = match group_id {
            Some(gid) => SessionSource::NapCatGroup { group_id: gid },
            None => SessionSource::NapCatPrivate { user_id },
        };

        let system_prompt = &self.prompts.system.content;

        let online_suffix = if let Some(ref adapter) = self.ts_adapter {
            match adapter.list_clients().await {
                Ok(clients) => {
                    let arr: Vec<serde_json::Value> = clients
                        .iter()
                        .map(|c| {
                            json!({"name": c.nickname, "clid": c.id, "channel_id": c.channel_id})
                        })
                        .collect();
                    debug!("Fetched {} online clients for LLM context", clients.len());
                    format!(
                        "\nOnline: {}",
                        serde_json::to_string(&arr).unwrap_or_default()
                    )
                }
                Err(_) => String::new(),
            }
        } else {
            String::new()
        };

        let user_ctx = match group_id {
            Some(gid) => format!(
                "User: {} (QQ: {}, Group: {}){}",
                sender_name, user_id, gid, online_suffix
            ),
            None => format!(
                "User: {} (QQ: {}, Private Chat){}",
                sender_name, user_id, online_suffix
            ),
        };

        let mut messages = self
            .llm
            .build_messages(&source, system_prompt, &user_ctx, user_msg);

        let allowed_skills = self.gate.get_allowed_skills(caller_groups, 0);
        debug!("NC allowed skills: {:?}", allowed_skills);
        let tools = self.registry.to_tool_schemas(&allowed_skills);

        let executor = NcExecutor {
            router: self,
            user_id,
            group_id,
            sender_name,
            caller_groups,
            allowed_skills: &allowed_skills,
        };

        match self
            .llm
            .run_tool_loop(&mut messages, &tools, &executor, None)
            .await
        {
            Ok(result) => {
                let content = result.content;
                info!(
                    reply_chars = content.chars().count(),
                    "[NC] LLM final reply ready"
                );
                content
            }
            Err(e) => {
                error!("NC LLM error: {}", e);
                error_msg
            }
        }
    }
}

/// 路由退出前回收在途任务：优先限时 join，超时后 abort 并收割。
const NC_TASK_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

async fn drain_nc_tasks(mut tasks: JoinSet<()>) {
    loop {
        let result = tokio::time::timeout(NC_TASK_DRAIN_TIMEOUT, tasks.join_next()).await;
        match result {
            Ok(Some(Ok(()))) => {}
            Ok(Some(Err(error))) => {
                error!("NC message task failed: {error}");
            }
            Ok(None) => break,
            Err(_) => {
                warn!(
                    timeout_secs = NC_TASK_DRAIN_TIMEOUT.as_secs(),
                    "NC message tasks exceeded drain timeout; aborting"
                );
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_chat_uses_base_and_trusted_user_groups() {
        let config = NapCatConfig {
            trusted_users: vec![42],
            ..NapCatConfig::default()
        };

        assert_eq!(nc_pseudo_groups(&config, 42, None), vec![9000, 9002]);
    }

    #[test]
    fn group_chat_uses_all_matching_pseudo_groups() {
        let config = NapCatConfig {
            trusted_users: vec![42],
            trusted_groups: vec![7],
            ..NapCatConfig::default()
        };

        assert_eq!(
            nc_pseudo_groups(&config, 42, Some(7)),
            vec![9000, 9001, 9002, 9003]
        );
    }
}
