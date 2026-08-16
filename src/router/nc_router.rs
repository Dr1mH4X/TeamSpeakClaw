use crate::adapter::headless::TsAdapter;
use crate::adapter::napcat::{
    event::{GroupMessageEvent, NcEvent, PrivateMessageEvent},
    types::{segments_to_text, Segment},
    NapCatAdapter,
};
use crate::adapter::reconnect::drain_managed_tasks;
use crate::config::{AppConfig, NapCatConfig, PromptsConfig};
use crate::llm::context::SessionSource;
use crate::llm::{LlmEngine, TurnCapacityPermit, TurnSessionGuard};
use crate::permission::PermissionGate;
use crate::router::{
    run_llm_turn, strip_trigger_prefix, ReplyPolicy, UnifiedInboundEvent, LLM_ERROR_REPLY,
};
use crate::skills::{NcExecutionContext, SkillRegistry, UnifiedExecutionContext};
use anyhow::Result;
use std::sync::Arc;
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};

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
                    drain_managed_tasks(&mut tasks, "NC message").await;
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
                    let user_id = msg.user_id;
                    self.spawn_handle(
                        &mut tasks,
                        msg,
                        SessionSource::NapCatPrivate { user_id },
                        || warn!(user_id, "NC LLM turn queue full; dropping message"),
                        |router, msg, capacity, session| async move {
                            router.handle_private(msg, capacity, session).await
                        },
                    )
                    .await;
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
                    let group_id = msg.group_id;
                    self.spawn_handle(
                        &mut tasks,
                        msg,
                        SessionSource::NapCatGroup { group_id },
                        || warn!(group_id, "NC LLM turn queue full; dropping message"),
                        |router, msg, capacity, session| async move {
                            router.handle_group(msg, capacity, session).await
                        },
                    )
                    .await;
                }
                NcEvent::Heartbeat => {
                    debug!("NapCat heartbeat");
                }
            }
        }
    }

    // clone 依赖 → 容量检查 → spawn 任务，重建 NcRouter 后分派给对应 handler
    async fn spawn_handle<M, F, Fut>(
        &self,
        tasks: &mut JoinSet<()>,
        msg: M,
        source: SessionSource,
        on_queue_full: impl FnOnce(),
        handler: F,
    ) where
        M: Send + 'static,
        F: FnOnce(NcRouter, M, TurnCapacityPermit, TurnSessionGuard) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let config = self.config.clone();
        let prompts = self.prompts.clone();
        let adapter = self.adapter.clone();
        let gate = self.gate.clone();
        let llm = self.llm.clone();
        let registry = self.registry.clone();
        let ts_adapter = self.ts_adapter.clone();

        let Ok(capacity) = llm.try_reserve_turn_capacity() else {
            on_queue_full();
            return;
        };

        tasks.spawn(async move {
            let session = llm.acquire_turn_session(&source).await;
            let router = NcRouter {
                config,
                prompts,
                adapter: adapter.clone(),
                gate,
                llm,
                registry,
                ts_adapter,
            };
            handler(router, msg, capacity, session).await;
        });
    }

    // 持有容量占位与同会话串行锁直至回复发送与历史保存完成
    async fn handle_private(
        &self,
        msg: PrivateMessageEvent,
        _capacity: TurnCapacityPermit,
        _session: TurnSessionGuard,
    ) {
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
        if !unified_event.should_trigger_llm {
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

    // 持有容量占位与同会话串行锁直至回复发送与历史保存完成
    async fn handle_group(
        &self,
        msg: GroupMessageEvent,
        _capacity: TurnCapacityPermit,
        _session: TurnSessionGuard,
    ) {
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
        if !unified_event.should_trigger_llm {
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

    fn is_triggered(&self, message: &[Segment]) -> bool {
        let nc = &self.config.napcat;
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

    /// 调用 LLM + Skill 系统，支持多轮工具调用，返回最终文本回复
    async fn run_llm(
        &self,
        user_msg: &str,
        sender_name: &str,
        user_id: i64,
        group_id: Option<i64>,
        caller_groups: &[u32],
    ) -> String {
        let source = match group_id {
            Some(gid) => SessionSource::NapCatGroup { group_id: gid },
            None => SessionSource::NapCatPrivate { user_id },
        };

        let system_prompt = &self.prompts.system.content;

        let online_suffix = if let Some(ref adapter) = self.ts_adapter {
            let (online_clients, _) = adapter.list_clients_json(0).await;
            if online_clients.is_empty() {
                String::new()
            } else {
                format!("\nOnline: {}", online_clients)
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

        let allowed_skills = self.gate.get_allowed_skills(caller_groups, 0);
        debug!("NC allowed skills: {:?}", allowed_skills);

        match run_llm_turn(
            &self.llm,
            &self.registry,
            |llm| llm.build_messages(&source, system_prompt, &user_ctx, user_msg),
            &allowed_skills,
            None,
            || {
                let nc_ctx = NcExecutionContext {
                    adapter: self.adapter.clone(),
                    caller_id: user_id,
                    caller_name: sender_name.to_string(),
                    caller_groups: caller_groups.to_vec(),
                    caller_group_id: group_id,
                    gate: self.gate.clone(),
                    config: self.config.clone(),
                };
                UnifiedExecutionContext::from_nc(&nc_ctx)
                    .with_cross_adapters(self.ts_adapter.clone(), Some(self.adapter.clone()))
            },
        )
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
                LLM_ERROR_REPLY.to_string()
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
