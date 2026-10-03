//! LLM 回合的唯一归属地：容量占位、会话串行锁、消息装配、工具循环、体积自愈与回复策略。
//!
//! 调用点只提供回合请求（会话、提示、输入、回调、取消）与回复落点 `TurnSink`：
//! 容量占位在事件循环里同步获取（`TurnPermit::reserve`），会话串行锁在执行任务内获取。

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::llm::context::{SessionSource, TurnCapacityPermit, TurnQueueFull};
use crate::llm::provider::PayloadTooLarge;
use crate::llm::tool_loop::ToolLoopError;
use crate::llm::{LlmEngine, StreamCallbacks, ToolCall, ToolExecutor};
use crate::skills::{SkillRegistry, UnifiedExecutionContext};

/// LLM 后端不可用时的固定回复文案（ts/nc/voice 调用点共用）
pub(crate) const LLM_ERROR_REPLY: &str = "AI backend unavailable. Please try again later.";

/// 体积类失败后最多重试的次数：只防放大，不保证保留内容
const MAX_PAYLOAD_RETRIES: usize = 1;

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
    /// 后端失败：固定文案已尽力回复，未落上下文
    Failed(anyhow::Error),
    /// 回复投递失败：内容未送达用户，未落上下文
    ReplyFailed(anyhow::Error),
}

/// 事件循环里的同步容量占位：满时立即返回 Err，不 spawn 回合任务。
/// 字段只为把占位持有到回合结束，容量释放靠 Drop
pub(crate) struct TurnPermit {
    _capacity: TurnCapacityPermit,
}

impl TurnPermit {
    pub(crate) fn reserve(llm: &LlmEngine) -> Result<Self, TurnQueueFull> {
        llm.try_reserve_turn_capacity()
            .map(|_capacity| Self { _capacity })
    }
}

/// 一回合 LLM 请求：提示与输入由调用点准备，门禁、循环与回复策略由本 module 承担
pub(crate) struct TurnRequest<'a> {
    pub(crate) llm: &'a LlmEngine,
    pub(crate) registry: &'a SkillRegistry,
    pub(crate) source: &'a SessionSource,
    pub(crate) system_prompt: &'a str,
    pub(crate) user_ctx: &'a str,
    pub(crate) allowed_skills: &'a [String],
    pub(crate) input: TurnInput<'a>,
    pub(crate) callbacks: Option<&'a StreamCallbacks>,
    pub(crate) cancel: &'a CancellationToken,
}

impl TurnRequest<'_> {
    /// 执行回合：持门禁跑完工具循环，把结果交给 `sink`，只在送达成功时落上下文
    pub(crate) async fn run<F, S>(
        self,
        permit: TurnPermit,
        build_exec_ctx: F,
        sink: &S,
    ) -> Result<(), TurnError>
    where
        F: Fn() -> UnifiedExecutionContext + Send + Sync,
        S: TurnSink,
    {
        // 容量占位与同会话串行锁持有到回复落库完成
        let _permit = permit;
        let _session = self.llm.acquire_turn_session(self.source).await;

        let tools = self.registry.to_tool_schemas(self.allowed_skills);
        let executor = TurnExecutor {
            registry: self.registry,
            allowed_skills: self.allowed_skills,
            build_exec_ctx,
        };
        let mut retries = 0usize;
        let result = loop {
            let mut messages = self.messages();
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
                Err(error) if retries < MAX_PAYLOAD_RETRIES && is_payload_too_large(&error) => {
                    // 没有可收缩的会话（NapCat）或已无音频可丢时原样返回，不做无意义的重试
                    if !self.llm.drop_oldest_audio_turn(self.source) {
                        break Err(error);
                    }
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
                self.deliver(sink, result.content).await
            }
            Ok(_) => Ok(()),
            Err(ToolLoopError::Cancelled) => Err(TurnError::Cancelled),
            Err(error) => {
                if let Err(send_error) = sink.send(LLM_ERROR_REPLY).await {
                    warn!(
                        session = %self.source,
                        error = %send_error,
                        "backend failure notice was not delivered"
                    );
                }
                Err(TurnError::Failed(error.into()))
            }
        }
    }

    /// 送达回复并落上下文：投递失败的内容不入上下文，避免下一轮回放用户没看到的回复
    async fn deliver<S: TurnSink>(&self, sink: &S, content: String) -> Result<(), TurnError> {
        if let Err(error) = sink.send(&content).await {
            warn!(
                session = %self.source,
                error = %error,
                "reply delivery failed; turn not saved"
            );
            return Err(TurnError::ReplyFailed(error));
        }
        match self.input {
            TurnInput::Text(text) => self.llm.save_turn(self.source, text.to_string(), content),
            TurnInput::Audio(wav) => self.llm.save_omni_turn(self.source, wav.to_vec(), content),
        }
        Ok(())
    }

    fn messages(&self) -> Vec<Value> {
        match self.input {
            TurnInput::Text(text) => {
                self.llm
                    .build_messages(self.source, self.system_prompt, self.user_ctx, text)
            }
            TurnInput::Audio(wav) => {
                self.llm
                    .build_omni_messages(self.source, self.system_prompt, self.user_ctx, wav)
            }
        }
    }
}

/// 工具循环执行器：调用点注入 UnifiedExecutionContext 构造闭包
struct TurnExecutor<'a, F> {
    registry: &'a SkillRegistry,
    allowed_skills: &'a [String],
    build_exec_ctx: F,
}

#[async_trait]
impl<F> ToolExecutor for TurnExecutor<'_, F>
where
    F: Fn() -> UnifiedExecutionContext + Send + Sync,
{
    async fn execute(&self, call: &ToolCall) -> String {
        self.registry
            .execute_skill(call, (self.build_exec_ctx)(), self.allowed_skills)
            .await
    }
}

/// 体积类失败判定：`PayloadTooLarge` 经 `anyhow::Error` → `ToolLoopError::Other` 原样透传，
/// 用 downcast 识别，避免每个调用点各写一份错误的文案比对
fn is_payload_too_large(error: &ToolLoopError) -> bool {
    matches!(error, ToolLoopError::Other(inner) if inner.downcast_ref::<PayloadTooLarge>().is_some())
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

    #[derive(Debug, Clone, Copy)]
    enum FakeOutcome {
        PayloadTooLarge,
        HttpError,
        Success,
        /// 有 `finish_reason` 但零 token：模型没产出可送达的文本
        EmptySuccess,
    }

    /// 脚本化的假 provider：记录调用次数与每次 messages 的序列化体积，
    /// 按脚本顺序返回结果，让回合策略不依赖真实网络
    struct FakeProvider {
        outcomes: Mutex<Vec<FakeOutcome>>,
        calls: AtomicUsize,
        body_lengths: Mutex<Vec<usize>>,
    }

    impl FakeProvider {
        fn new(outcomes: Vec<FakeOutcome>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes),
                calls: AtomicUsize::new(0),
                body_lengths: Mutex::new(Vec::new()),
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
                FakeOutcome::PayloadTooLarge => {
                    Err(PayloadTooLarge::for_test("oversized test payload").into())
                }
                FakeOutcome::HttpError => Err(anyhow::anyhow!("LLM API error: HTTP 500")),
                FakeOutcome::Success => Ok(Box::pin(stream::iter(vec![
                    Ok(LlmStreamEvent::Token("hello".to_string())),
                    Ok(LlmStreamEvent::Done {
                        finish_reason: "stop".to_string(),
                        tool_calls: Vec::new(),
                    }),
                ]))),
                FakeOutcome::EmptySuccess => {
                    Ok(Box::pin(stream::iter(vec![Ok(LlmStreamEvent::Done {
                        finish_reason: "stop".to_string(),
                        tool_calls: Vec::new(),
                    })])))
                }
            }
        }
    }

    /// 记录送达文本的落点；`fail` 时模拟投递失败
    /// 记录送达尝试（含失败的尝试）的落点；`fail` 时每次尝试都返回错误。
    /// 记全部尝试而不只记成功，才能断言「投递失败后不再补发兜底文案」
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
        let cancel = CancellationToken::new();
        TurnRequest {
            llm,
            registry: &SkillRegistry::default(),
            source,
            system_prompt: "system",
            user_ctx: "{}",
            allowed_skills: &[],
            input,
            callbacks: None,
            cancel: &cancel,
        }
        .run(
            TurnPermit::reserve(llm).expect("capacity is free in tests"),
            || panic!("no tool execution in these tests"),
            sink,
        )
        .await
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
        // 只尝试过正文那一次：投递失败不回退到兜底文案，也不落上下文
        assert_eq!(sink.sent(), vec!["hello".to_string()]);
        assert!(llm.stored_history(&source).is_empty());
    }

    #[tokio::test]
    async fn backend_failure_replies_the_fixed_text_and_saves_nothing() {
        let provider = Arc::new(FakeProvider::new(vec![FakeOutcome::HttpError]));
        let llm = engine_with(&provider, 8);
        let source = SessionSource::TeamSpeak {
            uid: "backend-failure".to_string(),
        };
        let sink = RecordingSink::new();

        let result = run_turn(&llm, &source, TurnInput::Text("question"), &sink).await;

        assert!(matches!(result, Err(TurnError::Failed(_))));
        assert_eq!(sink.sent(), vec![LLM_ERROR_REPLY.to_string()]);
        assert!(llm.stored_history(&source).is_empty());
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

        // 兜底文案送不出去也只记日志：回合仍以 Failed 交给调用点，且不落上下文
        assert!(matches!(result, Err(TurnError::Failed(_))));
        assert_eq!(sink.sent(), vec![LLM_ERROR_REPLY.to_string()]);
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
            registry: &SkillRegistry::default(),
            source: &source,
            system_prompt: "system",
            user_ctx: "{}",
            allowed_skills: &[],
            input: TurnInput::Text("question"),
            callbacks: None,
            cancel: &cancel,
        }
        .run(
            TurnPermit::reserve(&llm).expect("capacity is free in tests"),
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
        assert_eq!(sink.sent(), vec![LLM_ERROR_REPLY.to_string()]);
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
}
