use crate::adapter::headless::TsAdapter;
use crate::adapter::napcat::{
    event::NcEvent,
    types::{segments_to_text, Segment},
    NapCatAdapter,
};
use crate::adapter::reconnect::drain_managed_tasks;
use crate::config::{AppConfig, NapCatConfig, PromptsConfig};
use crate::llm::context::SessionSource;
use crate::llm::LlmEngine;
use crate::permission::PermissionGate;
use crate::router::{
    strip_trigger_prefix, TurnError, TurnInput, TurnPermit, TurnRequest, TurnSession, TurnSink,
};
use crate::skills::{NcCaller, SkillRegistry, UnifiedExecutionContext};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

struct NcInboundText {
    user_id: i64,
    sender_name: String,
    group_id: Option<i64>,
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
                    let self_id = self.adapter.get_self_id();
                    // 同步门先过：不需要回合的消息不占容量、不排队等会话锁
                    let Some(stripped) = precheck_text_turn(
                        &self.config,
                        &self.llm,
                        self_id,
                        user_id,
                        &msg.sender.nickname,
                        None,
                        &msg.message,
                    ) else {
                        continue;
                    };
                    self.spawn_handle(
                        &mut tasks,
                        msg,
                        SessionSource::NapCatPrivate { user_id },
                        || warn!(user_id, "NC LLM turn queue full; dropping message"),
                        |router, msg, session| async move {
                            router
                                .handle_text(
                                    NcInboundText {
                                        user_id: msg.user_id,
                                        sender_name: msg.sender.nickname,
                                        group_id: None,
                                    },
                                    stripped,
                                    session,
                                )
                                .await
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
                    let self_id = self.adapter.get_self_id();
                    // 未命中触发条件的群闲聊不进队列：它们排在群里正在跑的回合后面时，
                    // 会一直占着全局容量，把别的会话挤掉
                    let Some(stripped) = precheck_text_turn(
                        &self.config,
                        &self.llm,
                        self_id,
                        msg.user_id,
                        &msg.sender.nickname,
                        Some(group_id),
                        &msg.message,
                    ) else {
                        continue;
                    };
                    self.spawn_handle(
                        &mut tasks,
                        msg,
                        SessionSource::NapCatGroup { group_id },
                        || warn!(group_id, "NC LLM turn queue full; dropping message"),
                        move |router, msg, session| async move {
                            router
                                .handle_text(
                                    NcInboundText {
                                        user_id: msg.user_id,
                                        sender_name: msg.sender.nickname,
                                        group_id: Some(msg.group_id),
                                    },
                                    stripped,
                                    session,
                                )
                                .await
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
        F: FnOnce(NcRouter, M, TurnSession) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let config = self.config.clone();
        let prompts = self.prompts.clone();
        let adapter = self.adapter.clone();
        let gate = self.gate.clone();
        let llm = self.llm.clone();
        let registry = self.registry.clone();
        let ts_adapter = self.ts_adapter.clone();

        let Ok(permit) = TurnPermit::reserve(&llm) else {
            on_queue_full();
            return;
        };
        let ticket = llm.enqueue_turn_ticket(&source);

        tasks.spawn(async move {
            let session = permit.acquire_session(&llm, &source, ticket).await;
            let router = NcRouter {
                config,
                prompts,
                adapter: adapter.clone(),
                gate,
                llm,
                registry,
                ts_adapter,
            };
            handler(router, msg, session).await;
        });
    }

    async fn handle_text(&self, inbound: NcInboundText, stripped: String, session: TurnSession) {
        let NcInboundText {
            user_id,
            sender_name,
            group_id,
        } = inbound;

        let caller_groups = nc_pseudo_groups(&self.config.napcat, user_id, group_id);
        let source = match group_id {
            Some(gid) => SessionSource::NapCatGroup { group_id: gid },
            None => SessionSource::NapCatPrivate { user_id },
        };
        let system_prompt = self.prompts.system.content.as_str();

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

        let allowed_skills = self.gate.get_allowed_skills(&caller_groups, 0);
        debug!("NC allowed skills: {:?}", allowed_skills);

        let sink = NcReplySink {
            adapter: self.adapter.clone(),
            user_id,
            group_id,
        };
        let cancel = CancellationToken::new();
        let request = TurnRequest {
            llm: &self.llm,
            registry: &self.registry,
            source: &source,
            system_prompt,
            user_ctx: user_ctx.as_str(),
            allowed_skills: &allowed_skills,
            input: TurnInput::Text(&stripped),
            callbacks: None,
            cancel: &cancel,
        };
        match request
            .run(
                session,
                || {
                    UnifiedExecutionContext::for_nc(
                        NcCaller {
                            adapter: self.adapter.clone(),
                            caller_id: user_id,
                            caller_name: sender_name.to_string(),
                            caller_groups: caller_groups.clone(),
                            group_id,
                            ts_adapter: self.ts_adapter.clone(),
                        },
                        self.gate.clone(),
                        self.config.clone(),
                    )
                },
                &sink,
            )
            .await
        {
            Err(TurnError::Failed(error)) => error!("NC LLM error: {error}"),
            Err(TurnError::ReplyFailed(error)) => {
                warn!(error = %error, "NC reply delivery failed")
            }
            _ => {}
        }
    }

    /// 群消息是否命中触发条件：@ 机器人、CQ 码 @，或文本以任一触发前缀开头。
    fn is_group_message_triggered(
        config: &NapCatConfig,
        self_id: i64,
        message: &[Segment],
    ) -> bool {
        let self_id = self_id.to_string();
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
        config
            .trigger_prefixes
            .iter()
            .any(|p| text.starts_with(p.as_str()))
    }
}

/// 回合准入前的同步门：未命中触发条件的群消息、空消息、超限消息都在这里返回 `None`。
///
/// 必须在容量占位与会话排队之前调用：不需要回合的消息若先进队列，会在等同会话锁期间
/// 占着全局容量（容量 4，含等待中的回合），把别的会话的有效消息挤成「queue full」。
/// 群里一条正在跑的回合下攒够四条闲聊就足以让所有会话开始丢消息。
fn precheck_text_turn(
    config: &AppConfig,
    llm: &LlmEngine,
    self_id: i64,
    user_id: i64,
    sender_name: &str,
    group_id: Option<i64>,
    segments: &[Segment],
) -> Option<String> {
    if group_id.is_some()
        && !NcRouter::is_group_message_triggered(&config.napcat, self_id, segments)
    {
        return None;
    }
    let raw = segments_to_text(segments);
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let stripped = strip_trigger_prefix(raw, &config.napcat.trigger_prefixes).unwrap_or(raw);

    if group_id.is_some() {
        info!(
            group_id = ?group_id,
            user_id,
            user = %sender_name,
            message_chars = stripped.chars().count(),
            "[NC Group] message received"
        );
    } else {
        info!(
            user_id,
            user = %sender_name,
            message_chars = stripped.chars().count(),
            "[NC Private] message received"
        );
    }

    if let Err(error) = llm.check_user_text_bounds(stripped) {
        warn!(error = %error, "NC message dropped for exceeding size limit");
        return None;
    }
    Some(stripped.to_string())
}

/// NapCat 回复落点：群聊先 @ 发送者再跟正文
struct NcReplySink {
    adapter: Arc<NapCatAdapter>,
    user_id: i64,
    group_id: Option<i64>,
}

#[async_trait]
impl TurnSink for NcReplySink {
    async fn send(&self, text: &str) -> Result<()> {
        match self.group_id {
            Some(gid) => {
                let segs = vec![
                    Segment::at(self.user_id),
                    Segment::text(" "),
                    Segment::text(text),
                ];
                self.adapter.send_group(gid, &segs).await
            }
            None => {
                let segs = vec![Segment::text(text)];
                self.adapter.send_private(self.user_id, &segs).await
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

    fn trigger_config() -> NapCatConfig {
        NapCatConfig {
            trigger_prefixes: vec!["!bot".to_string()],
            ..NapCatConfig::default()
        }
    }

    fn engine() -> LlmEngine {
        LlmEngine::new(Arc::new(AppConfig::default()))
            .expect("engine builds without a network call")
    }

    #[test]
    fn group_trigger_accepts_prefix_and_at_mentions() {
        let config = trigger_config();

        assert!(NcRouter::is_group_message_triggered(
            &config,
            99,
            &[Segment::text("!bot hello")]
        ));
        assert!(NcRouter::is_group_message_triggered(
            &config,
            99,
            &[Segment::at(99), Segment::text(" hello")]
        ));
        assert!(!NcRouter::is_group_message_triggered(
            &config,
            99,
            &[Segment::text("just chatting")]
        ));
        assert!(!NcRouter::is_group_message_triggered(
            &config,
            99,
            &[Segment::at(1), Segment::text(" hello")]
        ));
    }

    /// 不需要回合的消息必须在排队之前返回 `None`：进队列就会占着全局容量等会话锁
    #[test]
    fn precheck_rejects_messages_that_need_no_turn() {
        let config = AppConfig {
            napcat: trigger_config(),
            ..AppConfig::default()
        };
        let llm = engine();

        assert!(
            precheck_text_turn(
                &config,
                &llm,
                99,
                7,
                "someone",
                Some(5),
                &[Segment::text("just chatting")]
            )
            .is_none(),
            "an untriggered group message must not queue"
        );
        assert!(
            precheck_text_turn(
                &config,
                &llm,
                99,
                7,
                "someone",
                Some(5),
                &[Segment::text("   ")]
            )
            .is_none(),
            "an empty message must not queue"
        );

        let oversized = Segment::text(format!(
            "!bot {}",
            "x".repeat(crate::llm::engine::MAX_USER_TEXT_BYTES + 1)
        ));
        assert!(
            precheck_text_turn(&config, &llm, 99, 7, "someone", Some(5), &[oversized]).is_none(),
            "an oversized message must not queue"
        );
    }

    /// 触发文本在预检里就剥好前缀；私聊不看前缀但同样只走一次预检
    #[test]
    fn precheck_returns_the_stripped_text_for_a_turn() {
        let config = AppConfig {
            napcat: trigger_config(),
            ..AppConfig::default()
        };
        let llm = engine();

        assert_eq!(
            precheck_text_turn(
                &config,
                &llm,
                99,
                7,
                "someone",
                Some(5),
                &[Segment::text("!bot  hello")]
            )
            .as_deref(),
            Some("hello")
        );
        assert_eq!(
            precheck_text_turn(
                &config,
                &llm,
                99,
                7,
                "someone",
                None,
                &[Segment::text("no prefix needed")]
            )
            .as_deref(),
            Some("no prefix needed")
        );
    }
}
