//! LLM 回合的唯一归属地：容量占位、会话串行锁、消息装配、工具循环、请求被拒后的收缩自愈与回复策略。
//!
//! 调用点只提供回合请求（会话、提示、输入、回调、取消）与回复落点 `TurnSink`：
//! 容量占位（`TurnPermit::reserve`）与到达顺序票据（`LlmEngine::enqueue_turn_ticket`）
//! 都在单一到达点（事件循环）同步获取，会话串行锁由 `TurnPermit::acquire_session`
//! 在任务里、请求准备之前获取。

use async_trait::async_trait;
use serde_json::Value;
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::llm::context::{
    ContextTurn, SessionSource, TurnCapacityPermit, TurnQueueFull, TurnSessionGuard, TurnTicket,
};
use crate::llm::provider::LlmFailure;
use crate::llm::tool_loop::ToolLoopError;
use crate::llm::{LlmEngine, StreamCallbacks, ToolCall, ToolExecutor};
use crate::tools::{ToolRegistry, UnifiedExecutionContext};
use crate::ui::{Activity, ActivityGuard, ActivityLabel, Ui};

/// clid 回退键：clid 会被复用，只在拿不到真实 UID 时使用
pub(crate) fn ts_caller_clid_uid(clid: u32) -> String {
    format!("clid:{clid}")
}

/// TS 调用者的准入键：容量票据与会话锁必须在 caller 解析（要查服务器）之前就定下来，
/// 事件不带 UID 时退到 clid。clid 只配当准入键——它会被复用，复用后是另一个人
pub(crate) fn ts_caller_admission_uid(event_uid: &str, clid: u32) -> String {
    if event_uid.is_empty() {
        ts_caller_clid_uid(clid)
    } else {
        event_uid.to_string()
    }
}

/// TS 调用者的会话/历史键：解析出的真实 UID 优先（`clid` 复用不能把两任占用者写进同一份
/// 历史），解析不到真实 UID 时退回准入键。绝不落到空串——空串会让所有拿不到 UID 的调用者
/// 共用一份历史与一把会话锁
pub(crate) fn ts_caller_session_uid(resolved_uid: &str, admission_uid: &str) -> String {
    if resolved_uid.is_empty() {
        admission_uid.to_string()
    } else {
        resolved_uid.to_string()
    }
}

/// 可收缩失败后最多重试的次数：只防放大，不保证保留内容
const MAX_SHRINK_RETRIES: usize = 1;

/// 回合输入：文本走 `build_messages`，音频走 `build_omni_messages`；
/// 成功落上下文时按同一分支选择 `save_turn` / `save_omni_turn`
pub(crate) enum TurnInput<'a> {
    Text(&'a str),
    Audio(&'a [u8]),
}

/// 回复落点：TS 文本、语音桥通知、NapCat 发送各实现一份
#[async_trait]
pub(crate) trait TurnSink: Send + Sync {
    async fn send(&self, text: &str) -> anyhow::Result<()>;
}

/// 回合失败分类：调用点只处理自己关心的分支
pub(crate) enum TurnError {
    /// 插话取消：未回复、未落上下文
    Cancelled,
    /// 后端失败：失败说明已尽力送达，未落上下文
    Failed(anyhow::Error),
    /// 回复投递失败：内容未送达用户，未落上下文
    ReplyFailed(anyhow::Error),
}

/// 事件循环里的同步容量占位：满时立即返回 Err，不 spawn 回合任务。
/// 字段只为把占位持有到回合结束，容量释放靠 Drop
pub(crate) struct TurnPermit {
    _capacity: TurnCapacityPermit,
    activity: Option<ActivityGuard>,
}

impl TurnPermit {
    pub(crate) fn reserve(llm: &LlmEngine) -> Result<Self, TurnQueueFull> {
        llm.try_reserve_turn_capacity().map(|_capacity| Self {
            _capacity,
            activity: None,
        })
    }

    pub(crate) fn reserve_with_ui(
        llm: &LlmEngine,
        ui: &Ui,
        label: ActivityLabel,
    ) -> Result<Self, TurnQueueFull> {
        let _capacity = llm.try_reserve_turn_capacity()?;
        Ok(Self {
            _capacity,
            activity: Some(ActivityGuard::new(ui.clone(), label)),
        })
    }

    /// 按到达顺序取会话锁：先等同会话前一条回合结束（票据在到达点生成），再取串行锁。
    /// 票据与容量占位一起放进 `TurnSession`，持有到回复落库完成才放行后继者
    pub(crate) async fn acquire_session(
        mut self,
        llm: &LlmEngine,
        source: &SessionSource,
        ticket: TurnTicket,
    ) -> TurnSession {
        let ticket = ticket.wait_for_predecessor().await;
        let session = llm.acquire_turn_session(source).await;
        if let Some(activity) = &self.activity {
            activity.activate();
        }
        let activity = self.activity.take();
        TurnSession {
            _permit: self,
            _session: session,
            _ticket: ticket,
            activity,
        }
    }
}

/// 容量占位、会话串行锁与到达顺序票据一起持有，覆盖请求准备、回复投递与历史保存。
/// 字段顺序即释放顺序：先放容量、再放锁、最后放行同会话的下一条
pub(crate) struct TurnSession {
    _permit: TurnPermit,
    _session: TurnSessionGuard,
    _ticket: TurnTicket,
    activity: Option<ActivityGuard>,
}

impl TurnSession {
    pub(crate) fn activity_handle(&self) -> Option<Activity> {
        self.activity.as_ref().map(ActivityGuard::handle)
    }
}

/// 一回合 LLM 请求：提示与输入由调用点准备，门禁、循环与回复策略由本 module 承担
pub(crate) struct TurnRequest<'a> {
    pub(crate) llm: &'a LlmEngine,
    pub(crate) registry: &'a ToolRegistry,
    pub(crate) source: &'a SessionSource,
    pub(crate) user_ctx: &'a str,
    pub(crate) allowed_tools: &'a [String],
    pub(crate) input: TurnInput<'a>,
    pub(crate) callbacks: Option<&'a StreamCallbacks>,
    pub(crate) cancel: &'a CancellationToken,
}

impl TurnRequest<'_> {
    /// 执行回合：持门禁跑完工具循环，把结果交给 `sink`，只在送达成功时落上下文
    pub(crate) async fn run<F, S>(
        self,
        session: TurnSession,
        build_exec_ctx: F,
        sink: &S,
    ) -> Result<(), TurnError>
    where
        F: Fn() -> UnifiedExecutionContext + Send + Sync,
        S: TurnSink,
    {
        // 容量占位与同会话串行锁持有到回复落库完成
        let activity = session.activity_handle();
        if let Some(activity) = &activity {
            activity.thinking();
        }
        let _session = session;

        // system 提示词由回合骨架装配：所有入站路径共用同一份有序段落与变量解析。
        // 装配失败与后端失败同一处理：先尽力送出失败说明，再以 `Failed` 交回调用点
        let system_prompt = match crate::prompt::system_prompt() {
            Ok(system_prompt) => system_prompt,
            Err(error) => return Err(self.fail_with_notice(sink, error, activity.as_ref()).await),
        };
        let tools = self.registry.to_tool_schemas(self.allowed_tools);
        let executor = TurnExecutor {
            registry: self.registry,
            allowed_tools: self.allowed_tools,
            build_exec_ctx,
            activity: activity.clone(),
        };
        let mut retries = 0usize;
        // 收缩在重试前落到存储历史上；重试被取消时用这份快照放回
        let mut undo: Option<Vec<ContextTurn>> = None;
        let result = loop {
            let mut messages = self.messages(&system_prompt);
            match self
                .llm
                .run_tool_loop(
                    &mut messages,
                    &tools,
                    &executor,
                    self.callbacks,
                    self.cancel,
                )
                .await
            {
                Err(error)
                    if retries < MAX_SHRINK_RETRIES && shrinkable_failure(&error).is_some() =>
                {
                    // 没有可收缩的会话（NapCat）或已无音频可丢时原样返回，不做无意义的重试
                    let Some(history) = self.llm.shrink_for_rejection_retry(self.source) else {
                        break Err(error);
                    };
                    warn!(
                        session = %self.source,
                        error = %error,
                        "LLM rejected the request; retrying with a smaller history"
                    );
                    undo = Some(history);
                    retries += 1;
                }
                result => break result,
            }
        };

        match result {
            Ok(result) if !result.content.is_empty() => {
                info!(
                    session = %self.source,
                    reply_chars = result.content.chars().count(),
                    "LLM reply ready"
                );
                if let Some(activity) = &activity {
                    activity.replying();
                }
                self.deliver(sink, result.content, activity.as_ref()).await
            }
            // provider 未归类的空回复：防御分支，既不送达也不落库
            Ok(_) => {
                if let Some(activity) = &activity {
                    activity.empty();
                }
                Ok(())
            }
            Err(ToolLoopError::Cancelled) => {
                // 取消的回合不回复也不落库：把收缩掉的历史放回，别让一次插话永久丢上下文
                if let Some(history) = undo {
                    self.llm.restore_history(self.source, history);
                }
                if let Some(activity) = &activity {
                    activity.cancelled();
                }
                Err(TurnError::Cancelled)
            }
            Err(error) => Err(self
                .fail_with_notice(sink, error.into(), activity.as_ref())
                .await),
        }
    }

    /// 后端失败：先尽力把失败原因作为文本送达用户，再以 `Failed` 交回调用点
    async fn fail_with_notice<S: TurnSink>(
        &self,
        sink: &S,
        error: anyhow::Error,
        activity: Option<&Activity>,
    ) -> TurnError {
        if let Some(activity) = activity {
            activity.failed("backend");
        }
        if let Err(send_error) = sink.send(&failure_text(&error)).await {
            warn!(
                session = %self.source,
                error = %send_error,
                "backend failure notice was not delivered"
            );
        }
        TurnError::Failed(error)
    }

    /// 送达回复并落上下文：投递失败的内容不入上下文，避免下一轮回放用户没看到的回复
    async fn deliver<S: TurnSink>(
        &self,
        sink: &S,
        content: String,
        activity: Option<&Activity>,
    ) -> Result<(), TurnError> {
        let chars = content.chars().count();
        if let Err(error) = sink.send(&content).await {
            warn!(
                session = %self.source,
                error = %error,
                "reply delivery failed; turn not saved"
            );
            if let Some(activity) = activity {
                activity.failed("reply delivery");
            }
            return Err(TurnError::ReplyFailed(error));
        }
        match self.input {
            TurnInput::Text(text) => self.llm.save_turn(self.source, text.to_string(), content),
            TurnInput::Audio(wav) => self.llm.save_omni_turn(self.source, wav.to_vec(), content),
        }
        if let Some(activity) = activity {
            activity.replied(chars);
        }
        Ok(())
    }

    fn messages(&self, system_prompt: &str) -> Vec<Value> {
        match self.input {
            TurnInput::Text(text) => {
                self.llm
                    .build_messages(self.source, system_prompt, self.user_ctx, text)
            }
            TurnInput::Audio(wav) => {
                self.llm
                    .build_omni_messages(self.source, system_prompt, self.user_ctx, wav)
            }
        }
    }
}

/// 工具循环执行器：调用点注入 UnifiedExecutionContext 构造闭包
struct TurnExecutor<'a, F> {
    registry: &'a ToolRegistry,
    allowed_tools: &'a [String],
    build_exec_ctx: F,
    activity: Option<Activity>,
}

#[async_trait]
impl<F> ToolExecutor for TurnExecutor<'_, F>
where
    F: Fn() -> UnifiedExecutionContext + Send + Sync,
{
    async fn execute(&self, call: &ToolCall) -> String {
        let started = Instant::now();
        if let Some(activity) = &self.activity {
            activity.tool_start(&call.name);
        }
        let result = self
            .registry
            .execute_tool(call, (self.build_exec_ctx)(), self.allowed_tools)
            .await;
        if let Some(activity) = &self.activity {
            activity.tool_end(&call.name, started.elapsed());
        }
        result
    }
}

/// 用户可见的失败文案：分类给出稳定的类别名，未分类的失败退回原始错误链。
/// 两类文案都不含网关响应体正文——正文只作归类判据，可能携带凭据
fn failure_text(error: &anyhow::Error) -> String {
    match classified_failure(error) {
        Some(failure) => format!("AI backend failure: {}", failure.user_text()),
        None => format!("AI backend failure: {error:#}"),
    }
}

/// 分类提取：模型调用错误可能被工具循环包进 `Other` 或 `AfterToolExecution`，两种包装下类别都不变
fn classified_failure(error: &anyhow::Error) -> Option<&LlmFailure> {
    if let Some(failure) = error.downcast_ref::<LlmFailure>() {
        return Some(failure);
    }
    match error.downcast_ref::<ToolLoopError>() {
        Some(ToolLoopError::Other(inner) | ToolLoopError::AfterToolExecution(inner)) => {
            inner.downcast_ref::<LlmFailure>()
        }
        _ => None,
    }
}

/// 可收缩失败判定：`LlmFailure` 经 `anyhow::Error` → `ToolLoopError::Other` 原样透传，
/// 只允许尚未执行工具的 `Other` 触发收缩重试，避免重放已经发生的操作。
fn shrinkable_failure(error: &ToolLoopError) -> Option<&LlmFailure> {
    match error {
        ToolLoopError::Other(inner) => inner
            .downcast_ref::<LlmFailure>()
            .filter(|failure| failure.is_shrinkable()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::context::ContextUser;
    use crate::llm::provider::{LlmProvider, LlmStreamEvent};
    use async_trait::async_trait;
    use futures_util::stream::{self, BoxStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    #[test]
    fn ts_caller_admission_key_falls_back_to_clid() {
        assert_eq!(ts_caller_admission_uid("alice-uid", 7), "alice-uid");
        assert_eq!(ts_caller_admission_uid("", 7), "clid:7");
    }

    /// 会话键用解析出的真实 UID：同一个 clid 复用后是另一个人，不能共享历史
    #[test]
    fn ts_caller_session_key_prefers_the_resolved_uid() {
        assert_eq!(ts_caller_session_uid("alice-uid", "clid:7"), "alice-uid");
        assert_eq!(ts_caller_session_uid("", "clid:7"), "clid:7");
    }

    #[derive(Debug, Clone, Copy)]
    enum FakeOutcome {
        PayloadTooLarge,
        /// 网关按模型上下文窗口拒绝：与体积类共用收缩恢复
        ContextWindowExceeded,
        HttpError,
        Success,
        /// 固定文本回复：让「哪条回复先送达」可断言，顺序类用例用它
        Reply(&'static str),
        /// 有 `finish_reason` 但零 token：模型没产出可送达的文本
        EmptySuccess,
        /// 真实 provider 对空完成的分类形态：以 `LlmFailure` 交回回合骨架
        EmptyResponse,
        Action,
        /// 重试期间插话取消：置位令牌并挂起流，让取消分支胜出
        CancelRetry,
    }

    /// 脚本化的假 provider：记录调用次数与每次 messages 的序列化体积，
    /// 按脚本顺序返回结果，让回合策略不依赖真实网络
    /// 未分类失败的原始错误链文案（假 provider 的 `HttpError`）
    const BACKEND_FAILURE_TEXT: &str = "AI backend failure: LLM API error: HTTP 500";
    /// 体积类失败的用户文案
    const PAYLOAD_FAILURE_TEXT: &str =
        "AI backend failure: payload too large: oversized test payload";
    /// 分类空完成的用户文案
    const EMPTY_COMPLETION_TEXT: &str =
        "AI backend failure: no content in the model completion (finish reason 'stop')";

    struct FakeProvider {
        outcomes: Mutex<Vec<FakeOutcome>>,
        calls: AtomicUsize,
        body_lengths: Mutex<Vec<usize>>,
        /// 需要模拟插话取消时由用例注入，与 `TurnRequest::run` 用的是同一个令牌
        cancel: Option<CancellationToken>,
    }

    impl FakeProvider {
        fn new(outcomes: Vec<FakeOutcome>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes),
                calls: AtomicUsize::new(0),
                body_lengths: Mutex::new(Vec::new()),
                cancel: None,
            }
        }

        fn with_cancel(outcomes: Vec<FakeOutcome>, cancel: CancellationToken) -> Self {
            Self {
                cancel: Some(cancel),
                ..Self::new(outcomes)
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn body_lengths(&self) -> Vec<usize> {
            self.body_lengths.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl LlmProvider for Arc<FakeProvider> {
        async fn chat_completion_stream(
            &self,
            messages: Vec<Value>,
            _tools: Vec<Value>,
        ) -> anyhow::Result<BoxStream<'static, anyhow::Result<LlmStreamEvent>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.body_lengths
                .lock()
                .unwrap()
                .push(serde_json::to_vec(&messages).unwrap().len());
            match self.outcomes.lock().unwrap().remove(0) {
                FakeOutcome::PayloadTooLarge => Err(LlmFailure::PayloadTooLarge {
                    message: "oversized test payload".to_string(),
                }
                .into()),
                FakeOutcome::ContextWindowExceeded => Err(LlmFailure::ContextWindowExceeded {
                    message: "context window exceeded".to_string(),
                }
                .into()),
                FakeOutcome::EmptyResponse => Err(LlmFailure::EmptyResponse {
                    finish_reason: "stop".to_string(),
                }
                .into()),
                FakeOutcome::HttpError => Err(anyhow::anyhow!("LLM API error: HTTP 500")),
                FakeOutcome::Success => Ok(Box::pin(stream::iter(vec![
                    Ok(LlmStreamEvent::Token("hello".to_string())),
                    Ok(LlmStreamEvent::Done {
                        finish_reason: "stop".to_string(),
                        tool_calls: Vec::new(),
                    }),
                ]))),
                FakeOutcome::Reply(text) => Ok(Box::pin(stream::iter(vec![
                    Ok(LlmStreamEvent::Token(text.to_string())),
                    Ok(LlmStreamEvent::Done {
                        finish_reason: "stop".to_string(),
                        tool_calls: Vec::new(),
                    }),
                ]))),
                FakeOutcome::Action => Ok(Box::pin(stream::iter(vec![Ok(LlmStreamEvent::Done {
                    finish_reason: "tool_calls".to_string(),
                    tool_calls: vec![ToolCall {
                        id: "action-call".to_string(),
                        name: "record_action".to_string(),
                        arguments: serde_json::json!({}),
                    }],
                })]))),
                FakeOutcome::EmptySuccess => {
                    Ok(Box::pin(stream::iter(vec![Ok(LlmStreamEvent::Done {
                        finish_reason: "stop".to_string(),
                        tool_calls: Vec::new(),
                    })])))
                }
                FakeOutcome::CancelRetry => {
                    if let Some(cancel) = &self.cancel {
                        cancel.cancel();
                    }
                    Ok(Box::pin(stream::pending()))
                }
            }
        }
    }

    /// 记录送达文本的落点；`fail` 时模拟投递失败
    /// 记录送达尝试（含失败的尝试）的落点；`fail` 时每次尝试都返回错误。
    /// 记全部尝试而不只记成功，才能断言「投递失败后没有第二次发送」
    struct RecordingSink {
        sent: Mutex<Vec<String>>,
        fail: bool,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                fail: false,
            }
        }

        fn failing() -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                fail: true,
            }
        }

        fn sent(&self) -> Vec<String> {
            self.sent.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl TurnSink for RecordingSink {
        async fn send(&self, text: &str) -> anyhow::Result<()> {
            self.sent.lock().unwrap().push(text.to_string());
            if self.fail {
                anyhow::bail!("sink delivery failed");
            }
            Ok(())
        }
    }

    fn engine_with(provider: &Arc<FakeProvider>, max_context_turns: usize) -> LlmEngine {
        LlmEngine::with_provider(Box::new(provider.clone()), max_context_turns)
    }

    async fn run_turn(
        llm: &LlmEngine,
        source: &SessionSource,
        input: TurnInput<'_>,
        sink: &RecordingSink,
    ) -> Result<(), TurnError> {
        let permit = TurnPermit::reserve(llm).expect("capacity is free in tests");
        let ticket = llm.enqueue_turn_ticket(source);
        let session = permit.acquire_session(llm, source, ticket).await;
        run_session(llm, source, input, sink, session).await
    }

    async fn run_session(
        llm: &LlmEngine,
        source: &SessionSource,
        input: TurnInput<'_>,
        sink: &RecordingSink,
        session: TurnSession,
    ) -> Result<(), TurnError> {
        let cancel = CancellationToken::new();
        TurnRequest {
            llm,
            registry: &ToolRegistry::default(),
            source,
            user_ctx: "{}",
            allowed_tools: &[],
            input,
            callbacks: None,
            cancel: &cancel,
        }
        .run(session, || panic!("no tool execution in these tests"), sink)
        .await
    }

    #[tokio::test]
    async fn slow_preparation_preserves_same_session_reply_and_history_order() {
        for source in [
            SessionSource::TeamSpeak {
                uid: "ordered-text".to_string(),
            },
            SessionSource::NapCatPrivate { user_id: 42 },
            SessionSource::NapCatGroup { group_id: 42 },
        ] {
            let provider = Arc::new(FakeProvider::new(vec![
                FakeOutcome::Reply("r-first"),
                FakeOutcome::Reply("r-second"),
            ]));
            let llm = engine_with(&provider, 8);
            let sink = RecordingSink::new();
            let (finish_preparing, prepared) = tokio::sync::oneshot::channel();
            let first_permit = TurnPermit::reserve(&llm).unwrap();
            let first_ticket = llm.enqueue_turn_ticket(&source);
            let second_permit = TurnPermit::reserve(&llm).unwrap();
            let second_ticket = llm.enqueue_turn_ticket(&source);

            let first = async {
                let session = first_permit
                    .acquire_session(&llm, &source, first_ticket)
                    .await;
                prepared.await.unwrap();
                run_session(&llm, &source, TurnInput::Text("first"), &sink, session).await
            };
            let second = async {
                let session = second_permit
                    .acquire_session(&llm, &source, second_ticket)
                    .await;
                run_session(&llm, &source, TurnInput::Text("second"), &sink, session).await
            };
            tokio::pin!(first, second);

            assert!(futures_util::poll!(&mut first).is_pending());
            assert!(futures_util::poll!(&mut second).is_pending());
            assert_eq!(provider.calls(), 0);
            assert!(sink.sent().is_empty());

            finish_preparing.send(()).unwrap();
            assert!(first.await.is_ok());
            assert!(second.await.is_ok());
            assert_eq!(provider.calls(), 2);
            assert_eq!(sink.sent(), vec!["r-first", "r-second"]);
            let history = llm.stored_history(&source);
            assert_eq!(history.len(), 2);
            for (turn, expected) in history.iter().zip(["first", "second"]) {
                let ContextUser::Text(text) = &turn.user else {
                    panic!("text input must be saved as a text turn");
                };
                assert_eq!(text, expected);
            }
        }
    }

    /// 取会话锁的先后由到达顺序票据决定，不由任务被调度执行的顺序决定。
    ///
    /// 这里故意让「后到」那条消息的任务先被 spawn：没有票据时它会先拿到会话锁、
    /// 先回复先落库（顺序反转），有票据时它必须等同会话前一条结束。
    /// 事件循环在一批事件里连续 spawn 时正是这个形状。
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn arrival_order_outranks_task_scheduling_order() {
        let provider = Arc::new(FakeProvider::new(vec![
            FakeOutcome::Reply("r-first"),
            FakeOutcome::Reply("r-second"),
        ]));
        let llm = Arc::new(engine_with(&provider, 8));
        let source = SessionSource::TeamSpeak {
            uid: "arrival-order".to_string(),
        };
        let sink = Arc::new(RecordingSink::new());

        // 事件循环：按到达顺序给两条消息发票据
        let first_permit = TurnPermit::reserve(&llm).unwrap();
        let first_ticket = llm.enqueue_turn_ticket(&source);
        let second_permit = TurnPermit::reserve(&llm).unwrap();
        let second_ticket = llm.enqueue_turn_ticket(&source);

        let mut tasks = tokio::task::JoinSet::new();
        // 后到者先 spawn，让调度顺序与到达顺序相反
        for (permit, ticket, input) in [
            (second_permit, second_ticket, "second"),
            (first_permit, first_ticket, "first"),
        ] {
            let llm = llm.clone();
            let sink = sink.clone();
            let source = source.clone();
            tasks.spawn(async move {
                let session = permit.acquire_session(&llm, &source, ticket).await;
                let _ = run_session(&llm, &source, TurnInput::Text(input), &sink, session).await;
            });
        }

        tokio::task::yield_now().await;
        while tasks.join_next().await.is_some() {}

        assert_eq!(sink.sent(), vec!["r-first", "r-second"]);
        let history = llm.stored_history(&source);
        let texts: Vec<&str> = history
            .iter()
            .map(|turn| match &turn.user {
                ContextUser::Text(text) => text.as_str(),
                _ => panic!("text input must be saved as a text turn"),
            })
            .collect();
        assert_eq!(texts, vec!["first", "second"]);
    }

    #[tokio::test]
    async fn delivered_reply_is_saved_as_a_text_turn() {
        let provider = Arc::new(FakeProvider::new(vec![FakeOutcome::Success]));
        let llm = engine_with(&provider, 8);
        let source = SessionSource::TeamSpeak {
            uid: "text-turn".to_string(),
        };
        let sink = RecordingSink::new();

        let result = run_turn(&llm, &source, TurnInput::Text("question"), &sink).await;

        assert!(result.is_ok());
        assert_eq!(sink.sent(), vec!["hello".to_string()]);
        let history = llm.stored_history(&source);
        assert_eq!(history.len(), 1);
        let ContextUser::Text(question) = &history[0].user else {
            panic!("text input must be saved as a text turn");
        };
        assert_eq!(question, "question");
        assert_eq!(history[0].assistant, "hello");
    }

    #[tokio::test]
    async fn delivered_reply_is_saved_as_an_omni_turn() {
        let provider = Arc::new(FakeProvider::new(vec![FakeOutcome::Success]));
        let llm = engine_with(&provider, 8);
        let source = SessionSource::TeamSpeak {
            uid: "audio-turn".to_string(),
        };
        let sink = RecordingSink::new();

        let result = run_turn(&llm, &source, TurnInput::Audio(&[1, 2, 3, 4]), &sink).await;

        assert!(result.is_ok());
        let history = llm.stored_history(&source);
        assert_eq!(history.len(), 1);
        let ContextUser::Audio(audio) = &history[0].user else {
            panic!("audio input must be saved as an audio turn");
        };
        assert_eq!(audio.len(), 4);
    }

    #[tokio::test]
    async fn undelivered_reply_is_not_saved() {
        let provider = Arc::new(FakeProvider::new(vec![FakeOutcome::Success]));
        let llm = engine_with(&provider, 8);
        let source = SessionSource::TeamSpeak {
            uid: "undelivered".to_string(),
        };
        let sink = RecordingSink::failing();

        let result = run_turn(&llm, &source, TurnInput::Text("question"), &sink).await;

        assert!(matches!(result, Err(TurnError::ReplyFailed(_))));
        // 只尝试过正文那一次：投递失败不回退到失败说明，也不落上下文
        assert_eq!(sink.sent(), vec!["hello".to_string()]);
        assert!(llm.stored_history(&source).is_empty());
    }

    #[tokio::test]
    async fn backend_failure_sends_the_reason_as_text() {
        // 失败原因作为文本送达用户：可识别的失败给稳定类别，未分类的失败给原始错误链
        for (outcome, expected) in [
            (
                FakeOutcome::HttpError,
                "AI backend failure: LLM API error: HTTP 500",
            ),
            (
                FakeOutcome::PayloadTooLarge,
                "AI backend failure: payload too large: oversized test payload",
            ),
            (
                FakeOutcome::ContextWindowExceeded,
                "AI backend failure: context window exceeded: context window exceeded",
            ),
        ] {
            let provider = Arc::new(FakeProvider::new(vec![outcome]));
            let llm = engine_with(&provider, 8);
            let source = SessionSource::TeamSpeak {
                uid: format!("reason-{}", expected.len()),
            };
            let sink = RecordingSink::new();

            let result = run_turn(&llm, &source, TurnInput::Text("question"), &sink).await;

            assert!(matches!(result, Err(TurnError::Failed(_))));
            assert_eq!(sink.sent(), vec![expected.to_string()]);
            assert!(llm.stored_history(&source).is_empty());
        }
    }

    #[tokio::test]
    async fn undeliverable_backend_notice_still_reports_failure() {
        let provider = Arc::new(FakeProvider::new(vec![FakeOutcome::HttpError]));
        let llm = engine_with(&provider, 8);
        let source = SessionSource::TeamSpeak {
            uid: "backend-notice-failed".to_string(),
        };
        let sink = RecordingSink::failing();

        let result = run_turn(&llm, &source, TurnInput::Text("question"), &sink).await;

        // 失败文案送不出去也只记日志：回合仍以 Failed 交给调用点，且不落上下文
        assert!(matches!(result, Err(TurnError::Failed(_))));
        assert_eq!(sink.sent(), vec![BACKEND_FAILURE_TEXT.to_string()]);
        assert!(llm.stored_history(&source).is_empty());
    }

    #[tokio::test]
    async fn empty_reply_is_neither_sent_nor_saved() {
        let provider = Arc::new(FakeProvider::new(vec![FakeOutcome::EmptySuccess]));
        let llm = engine_with(&provider, 8);
        let source = SessionSource::TeamSpeak {
            uid: "empty-reply".to_string(),
        };
        let sink = RecordingSink::new();

        let result = run_turn(&llm, &source, TurnInput::Text("question"), &sink).await;

        assert!(result.is_ok());
        assert!(sink.sent().is_empty());
        assert!(llm.stored_history(&source).is_empty());
    }

    #[tokio::test]
    async fn classified_empty_completion_reports_the_reason() {
        // 真实 provider 把空完成交回 `LlmFailure::EmptyResponse`：用户拿到零输出的原因，
        // 回合以 Failed 收尾且不落库
        let provider = Arc::new(FakeProvider::new(vec![FakeOutcome::EmptyResponse]));
        let llm = engine_with(&provider, 8);
        let source = SessionSource::TeamSpeak {
            uid: "classified-empty".to_string(),
        };
        let sink = RecordingSink::new();

        let result = run_turn(&llm, &source, TurnInput::Text("question"), &sink).await;

        assert!(matches!(result, Err(TurnError::Failed(_))));
        assert_eq!(sink.sent(), vec![EMPTY_COMPLETION_TEXT.to_string()]);
        assert!(llm.stored_history(&source).is_empty());
    }

    #[tokio::test]
    async fn context_window_rejection_shrinks_and_retries_like_a_payload_rejection() {
        let provider = Arc::new(FakeProvider::new(vec![
            FakeOutcome::ContextWindowExceeded,
            FakeOutcome::Success,
        ]));
        let llm = engine_with(&provider, 8);
        let source = SessionSource::TeamSpeak {
            uid: "context-window".to_string(),
        };
        for _ in 0..3 {
            llm.save_omni_turn(&source, vec![0u8; 2048], "reply".to_string());
        }
        let sink = RecordingSink::new();

        let result = run_turn(&llm, &source, TurnInput::Audio(&[0u8; 64]), &sink).await;

        assert!(result.is_ok());
        let bodies = provider.body_lengths();
        assert_eq!(bodies.len(), 2);
        assert!(bodies[1] < bodies[0]);
        assert_eq!(sink.sent(), vec!["hello".to_string()]);
    }

    #[tokio::test]
    async fn empty_completion_does_not_shrink_the_history() {
        // 空完成不是可收缩拒绝：不得因为一次零输出就丢掉会话历史
        let provider = Arc::new(FakeProvider::new(vec![FakeOutcome::EmptyResponse]));
        let llm = engine_with(&provider, 8);
        let source = SessionSource::TeamSpeak {
            uid: "empty-no-shrink".to_string(),
        };
        for _ in 0..3 {
            llm.save_omni_turn(&source, vec![0u8; 1024], "reply".to_string());
        }
        let sink = RecordingSink::new();

        let result = run_turn(&llm, &source, TurnInput::Audio(&[0u8; 64]), &sink).await;

        assert!(matches!(result, Err(TurnError::Failed(_))));
        assert_eq!(provider.calls(), 1);
        assert_eq!(sink.sent(), vec![EMPTY_COMPLETION_TEXT.to_string()]);
        assert_eq!(llm.stored_history(&source).len(), 3);
    }

    #[tokio::test]
    async fn empty_completion_after_a_tool_call_reports_the_reason() {
        // 工具执行后的空完成被包进 `AfterToolExecution`，但类别不变：用户拿到同样的原因
        let provider = Arc::new(FakeProvider::new(vec![
            FakeOutcome::Action,
            FakeOutcome::EmptyResponse,
        ]));
        let llm = engine_with(&provider, 8);
        let source = SessionSource::TeamSpeak {
            uid: "empty-after-action".to_string(),
        };
        let actions = Arc::new(AtomicUsize::new(0));
        let registry = ToolRegistry::default();
        registry.register(Box::new(RecordedAction(actions.clone())));
        let sink = RecordingSink::new();
        let cancel = CancellationToken::new();
        let permit = TurnPermit::reserve(&llm).unwrap();
        let ticket = llm.enqueue_turn_ticket(&source);
        let session = permit.acquire_session(&llm, &source, ticket).await;
        let allowed = vec!["record_action".to_string()];
        let request = TurnRequest {
            llm: &llm,
            registry: &registry,
            source: &source,
            user_ctx: "{}",
            allowed_tools: &allowed,
            input: TurnInput::Text("perform the action"),
            callbacks: None,
            cancel: &cancel,
        };
        let result = request
            .run(
                session,
                || UnifiedExecutionContext {
                    platform: crate::tools::Platform::TeamSpeak,
                    ts_adapter: None,
                    nc_adapter: None,
                    caller_id: 1,
                    caller_id_nc: 0,
                    caller_name: "test".to_string(),
                    caller_groups: Vec::new(),
                    caller_channel_group_id: 0,
                    nc_group_id: None,
                    gate: Arc::new(crate::permission::PermissionGate::new(
                        crate::config::AclConfig::default(),
                    )),
                    config: Arc::new(crate::config::AppConfig::default()),
                },
                &sink,
            )
            .await;

        assert!(matches!(result, Err(TurnError::Failed(_))));
        assert_eq!(actions.load(Ordering::SeqCst), 1);
        assert_eq!(provider.calls(), 2);
        assert_eq!(sink.sent(), vec![EMPTY_COMPLETION_TEXT.to_string()]);
        assert!(llm.stored_history(&source).is_empty());
    }

    #[tokio::test]
    async fn non_payload_errors_do_not_shrink_the_history() {
        let provider = Arc::new(FakeProvider::new(vec![FakeOutcome::HttpError]));
        let llm = engine_with(&provider, 8);
        let source = SessionSource::TeamSpeak {
            uid: "no-shrink".to_string(),
        };
        llm.save_omni_turn(&source, vec![0u8; 1024], "first-reply".to_string());
        llm.save_omni_turn(&source, vec![0u8; 1024], "second-reply".to_string());
        let sink = RecordingSink::new();

        let result = run_turn(&llm, &source, TurnInput::Audio(&[0u8; 64]), &sink).await;

        // 非体积类失败不触发收缩：只调一次，历史原样保留
        assert!(matches!(result, Err(TurnError::Failed(_))));
        assert_eq!(provider.calls(), 1);
        assert_eq!(llm.stored_history(&source).len(), 2);
    }

    #[tokio::test]
    async fn cancelled_turn_neither_replies_nor_saves() {
        let provider = Arc::new(FakeProvider::new(vec![FakeOutcome::Success]));
        let llm = engine_with(&provider, 8);
        let source = SessionSource::TeamSpeak {
            uid: "cancelled".to_string(),
        };
        let sink = RecordingSink::new();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = TurnRequest {
            llm: &llm,
            registry: &ToolRegistry::default(),
            source: &source,
            user_ctx: "{}",
            allowed_tools: &[],
            input: TurnInput::Text("question"),
            callbacks: None,
            cancel: &cancel,
        }
        .run(
            TurnPermit::reserve(&llm)
                .expect("capacity is free in tests")
                .acquire_session(&llm, &source, llm.enqueue_turn_ticket(&source))
                .await,
            || panic!("no tool execution in these tests"),
            &sink,
        )
        .await;

        assert!(matches!(result, Err(TurnError::Cancelled)));
        assert!(sink.sent().is_empty());
        assert!(llm.stored_history(&source).is_empty());
    }

    #[tokio::test]
    async fn payload_too_large_drops_the_earliest_audio_turn_and_retries_once() {
        let provider = Arc::new(FakeProvider::new(vec![
            FakeOutcome::PayloadTooLarge,
            FakeOutcome::Success,
        ]));
        let llm = engine_with(&provider, 8);
        let source = SessionSource::TeamSpeak {
            uid: "retry-shrink".to_string(),
        };
        llm.save_omni_turn(&source, vec![0u8; 2048], "oldest-reply".to_string());
        llm.save_omni_turn(&source, vec![0u8; 1024], "latest-reply".to_string());
        let sink = RecordingSink::new();

        let result = run_turn(&llm, &source, TurnInput::Audio(&[0u8; 64]), &sink).await;

        assert!(result.is_ok());
        assert_eq!(provider.calls(), 2);
        assert_eq!(sink.sent(), vec!["hello".to_string()]);
        // 最早一轮被丢掉、被丢弃的旧回复不入历史，最新音频轮与当前轮留下
        let history = llm.stored_history(&source);
        assert_eq!(history.len(), 2);
        let ContextUser::Audio(audio) = &history[0].user else {
            panic!("the latest audio turn must survive");
        };
        assert_eq!(audio.len(), 1024);
    }

    #[tokio::test]
    async fn cancelled_retry_restores_the_history_the_shrink_removed() {
        let cancel = CancellationToken::new();
        let provider = Arc::new(FakeProvider::with_cancel(
            vec![FakeOutcome::PayloadTooLarge, FakeOutcome::CancelRetry],
            cancel.clone(),
        ));
        let llm = engine_with(&provider, 8);
        let source = SessionSource::TeamSpeak {
            uid: "retry-cancel".to_string(),
        };
        for _ in 0..8 {
            llm.save_omni_turn(&source, vec![0u8; 400 * 1024], "reply".to_string());
        }
        let before = llm.stored_history(&source);
        let sink = RecordingSink::new();

        let result = TurnRequest {
            llm: &llm,
            registry: &ToolRegistry::default(),
            source: &source,
            user_ctx: "{}",
            allowed_tools: &[],
            input: TurnInput::Audio(&[0u8; 64]),
            callbacks: None,
            cancel: &cancel,
        }
        .run(
            TurnPermit::reserve(&llm)
                .expect("capacity is free in tests")
                .acquire_session(&llm, &source, llm.enqueue_turn_ticket(&source))
                .await,
            || panic!("no tool execution in these tests"),
            &sink,
        )
        .await;

        assert!(matches!(result, Err(TurnError::Cancelled)));
        assert!(sink.sent().is_empty());
        // 重试确实在收缩后的更小请求上发出，随后被插话取消
        assert_eq!(provider.calls(), 2);
        let bodies = provider.body_lengths();
        assert!(
            bodies[1] < bodies[0],
            "the retry was not assembled from the shrunk history: {} -> {} bytes",
            bodies[0],
            bodies[1]
        );
        // 取消的回合不落库，收缩也要放回：一次插话不该永久丢掉上下文
        let after = llm.stored_history(&source);
        assert_eq!(after.len(), before.len());
        assert_eq!(
            after
                .iter()
                .map(|turn| turn.user.as_text())
                .collect::<Vec<_>>(),
            before
                .iter()
                .map(|turn| turn.user.as_text())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn retry_after_shrink_assembles_a_smaller_body() {
        let provider = Arc::new(FakeProvider::new(vec![
            FakeOutcome::PayloadTooLarge,
            FakeOutcome::Success,
        ]));
        let llm = engine_with(&provider, 8);
        let source = SessionSource::TeamSpeak {
            uid: "retry-body".to_string(),
        };
        for _ in 0..3 {
            llm.save_omni_turn(&source, vec![0u8; 2048], "reply".to_string());
        }
        let sink = RecordingSink::new();

        let result = run_turn(&llm, &source, TurnInput::Audio(&[0u8; 64]), &sink).await;

        assert!(result.is_ok());
        let bodies = provider.body_lengths();
        assert_eq!(bodies.len(), 2);
        assert!(bodies[1] < bodies[0]);
    }

    #[tokio::test]
    async fn regression_retry_shrinks_already_clipped_wire_view() {
        let provider = Arc::new(FakeProvider::new(vec![
            FakeOutcome::PayloadTooLarge,
            FakeOutcome::Success,
        ]));
        let llm = engine_with(&provider, 8);
        let source = SessionSource::TeamSpeak {
            uid: "retry-body".to_string(),
        };
        for _ in 0..8 {
            llm.save_omni_turn(&source, vec![0u8; 400 * 1024], "reply".to_string());
        }
        let sink = RecordingSink::new();

        let result = run_turn(&llm, &source, TurnInput::Audio(&[0u8; 64]), &sink).await;

        assert!(result.is_ok());
        let bodies = provider.body_lengths();
        assert_eq!(bodies.len(), 2);
        assert!(
            bodies[1] < bodies[0],
            "retry did not shrink request: {} -> {} bytes",
            bodies[0],
            bodies[1]
        );
    }

    #[tokio::test]
    async fn retry_is_attempted_only_once() {
        // 第二次仍是体积类错误：重试上限 1 次，不得出现第三次调用
        let provider = Arc::new(FakeProvider::new(vec![
            FakeOutcome::PayloadTooLarge,
            FakeOutcome::PayloadTooLarge,
            FakeOutcome::Success,
        ]));
        let llm = engine_with(&provider, 8);
        let source = SessionSource::TeamSpeak {
            uid: "retry-once".to_string(),
        };
        for _ in 0..3 {
            llm.save_omni_turn(&source, vec![0u8; 1024], "reply".to_string());
        }
        let sink = RecordingSink::new();

        let result = run_turn(&llm, &source, TurnInput::Audio(&[0u8; 64]), &sink).await;

        assert!(matches!(result, Err(TurnError::Failed(_))));
        assert_eq!(provider.calls(), 2);
        assert_eq!(sink.sent(), vec![PAYLOAD_FAILURE_TEXT.to_string()]);
        // 收缩已丢掉最早一轮，失败回合本身不落上下文
        assert_eq!(llm.stored_history(&source).len(), 2);
    }

    #[tokio::test]
    async fn payload_too_large_without_audio_does_not_retry() {
        let provider = Arc::new(FakeProvider::new(vec![FakeOutcome::PayloadTooLarge]));
        let llm = engine_with(&provider, 8);
        let source = SessionSource::TeamSpeak {
            uid: "text-only".to_string(),
        };
        llm.save_turn(&source, "text-question".to_string(), "reply".to_string());
        let sink = RecordingSink::new();

        let result = run_turn(&llm, &source, TurnInput::Text("question"), &sink).await;

        // 文本轮不参与收缩：无音频可丢就不重试，历史原样保留
        assert!(matches!(result, Err(TurnError::Failed(_))));
        assert_eq!(provider.calls(), 1);
        assert_eq!(llm.stored_history(&source).len(), 1);
    }
    struct RecordedAction(Arc<AtomicUsize>);
    #[async_trait]
    impl crate::tools::Tool for RecordedAction {
        fn name(&self) -> &'static str {
            "record_action"
        }
        fn description(&self) -> &'static str {
            "Record a local test action"
        }
        fn parameters(&self) -> Value {
            serde_json::json!({"type":"object"})
        }
        async fn execute(
            &self,
            _args: Value,
            _ctx: &UnifiedExecutionContext,
        ) -> anyhow::Result<Value> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({"done":true}))
        }
    }

    #[tokio::test]
    async fn regression_payload_retry_must_not_repeat_completed_action() {
        let provider = Arc::new(FakeProvider::new(vec![
            FakeOutcome::Action,
            FakeOutcome::PayloadTooLarge,
            FakeOutcome::Action,
            FakeOutcome::Success,
        ]));
        let llm = engine_with(&provider, 8);
        let source = SessionSource::TeamSpeak {
            uid: "side-effect-retry".to_string(),
        };
        llm.save_omni_turn(&source, vec![0; 1024], "old".to_string());
        llm.save_omni_turn(&source, vec![0; 1024], "new".to_string());
        let actions = Arc::new(AtomicUsize::new(0));
        let registry = ToolRegistry::default();
        registry.register(Box::new(RecordedAction(actions.clone())));
        let sink = RecordingSink::new();
        let cancel = CancellationToken::new();
        let permit = TurnPermit::reserve(&llm).unwrap();
        let ticket = llm.enqueue_turn_ticket(&source);
        let session = permit.acquire_session(&llm, &source, ticket).await;
        let allowed = vec!["record_action".to_string()];
        let request = TurnRequest {
            llm: &llm,
            registry: &registry,
            source: &source,
            user_ctx: "{}",
            allowed_tools: &allowed,
            input: TurnInput::Text("perform the action once"),
            callbacks: None,
            cancel: &cancel,
        };
        let result = request
            .run(
                session,
                || UnifiedExecutionContext {
                    platform: crate::tools::Platform::TeamSpeak,
                    ts_adapter: None,
                    nc_adapter: None,
                    caller_id: 1,
                    caller_id_nc: 0,
                    caller_name: "test".to_string(),
                    caller_groups: Vec::new(),
                    caller_channel_group_id: 0,
                    nc_group_id: None,
                    gate: Arc::new(crate::permission::PermissionGate::new(
                        crate::config::AclConfig::default(),
                    )),
                    config: Arc::new(crate::config::AppConfig::default()),
                },
                &sink,
            )
            .await;
        let Err(TurnError::Failed(error)) = result else {
            panic!("a model failure after tool execution must fail without retrying");
        };
        assert!(matches!(
            error.downcast_ref::<ToolLoopError>(),
            Some(ToolLoopError::AfterToolExecution(_))
        ));
        assert_eq!(
            actions.load(Ordering::SeqCst),
            1,
            "completed action was replayed by the context retry"
        );
        assert_eq!(provider.calls(), 2);
        assert_eq!(sink.sent(), vec![PAYLOAD_FAILURE_TEXT.to_string()]);
        assert_eq!(llm.stored_history(&source).len(), 2);
    }
}
