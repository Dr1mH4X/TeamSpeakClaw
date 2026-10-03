mod nc_router;
mod trigger;
mod ts_router;
mod unified;
mod voice_feedback;
mod voice_router;
mod voice_turns;

pub use nc_router::NcRouter;
pub use trigger::{resolve_ts_inbound, strip_trigger_prefix};
pub use ts_router::EventRouter;
pub use unified::{ReplyPolicy, UnifiedInboundEvent};
pub use voice_router::{VoiceRouter, VoiceRouterHandles};

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::adapter::headless::TsAdapter;
use crate::adapter::napcat::NapCatAdapter;
use crate::config::{AppConfig, PromptsConfig};
use crate::llm::context::SessionSource;
use crate::llm::provider::PayloadTooLarge;
use crate::llm::tool_loop::{ToolLoopError, ToolLoopResult};
use crate::llm::{LlmEngine, StreamCallbacks, ToolCall, ToolExecutor};
use crate::permission::PermissionGate;
use crate::skills::{SkillRegistry, UnifiedExecutionContext, VoiceAudioHandles};

/// LLM 后端不可用时的固定回复文案（ts/nc/voice 四调用点共用）
pub(crate) const LLM_ERROR_REPLY: &str = "AI backend unavailable. Please try again later.";

/// 体积类失败后最多重试的次数：只防放大，不保证保留内容
const MAX_PAYLOAD_RETRIES: usize = 1;

/// 工具循环执行器：调用点注入 UnifiedExecutionContext 构造闭包，替代原各 router 的薄包装 Executor
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

/// 工具循环的公共运行参数（工具白名单、流式回调、取消令牌）。
/// 聚成参数对象是为了让带体积自愈的入口保持在 clippy 的参数个数阈值内；
/// `run_llm_turn` 保留原有的逐项签名，nc 路径按原样调用。
pub(crate) struct TurnLoopSpec<'a> {
    pub(crate) allowed_skills: &'a [String],
    pub(crate) callbacks: Option<&'a StreamCallbacks>,
    pub(crate) cancel: &'a CancellationToken,
}

/// 共享的单回合 LLM 执行骨架：构建消息 → 筛选工具 schema → 构造执行器 → 运行工具循环。
/// 差异点（user_ctx 构建、发送通道、save_turn 时机、错误处理）由调用点各自实现。
/// 不做体积自愈；可能回放音频历史的调用点走 `run_llm_turn_with_audio_recovery`。
pub(crate) async fn run_llm_turn<F, M>(
    llm: &LlmEngine,
    registry: &SkillRegistry,
    build_messages: M,
    allowed_skills: &[String],
    callbacks: Option<&StreamCallbacks>,
    cancel: &CancellationToken,
    build_exec_ctx: F,
) -> Result<ToolLoopResult, ToolLoopError>
where
    F: Fn() -> UnifiedExecutionContext + Send + Sync,
    M: FnMut(&LlmEngine) -> Vec<Value>,
{
    let spec = TurnLoopSpec {
        allowed_skills,
        callbacks,
        cancel,
    };
    run_llm_turn_inner(llm, registry, None, build_messages, spec, build_exec_ctx).await
}

/// 与 `run_llm_turn` 相同，但在拿到 `PayloadTooLarge` 且 `source` 还有音频可丢时，
/// 丢弃最早的音频轮、按收缩后的历史重建 messages 并重试一次（`MAX_PAYLOAD_RETRIES`）。
///
/// 重试发生在调用方已持有的 capacity permit 与 session guard 内，不重入门禁；
/// 历史只在回合成功时写入，因此收缩只作用于后续请求，取消或失败的回合不落上下文。
pub(crate) async fn run_llm_turn_with_audio_recovery<F, M>(
    llm: &LlmEngine,
    registry: &SkillRegistry,
    source: &SessionSource,
    build_messages: M,
    spec: TurnLoopSpec<'_>,
    build_exec_ctx: F,
) -> Result<ToolLoopResult, ToolLoopError>
where
    F: Fn() -> UnifiedExecutionContext + Send + Sync,
    M: FnMut(&LlmEngine) -> Vec<Value>,
{
    run_llm_turn_inner(
        llm,
        registry,
        Some(source),
        build_messages,
        spec,
        build_exec_ctx,
    )
    .await
}

async fn run_llm_turn_inner<F, M>(
    llm: &LlmEngine,
    registry: &SkillRegistry,
    source: Option<&SessionSource>,
    mut build_messages: M,
    spec: TurnLoopSpec<'_>,
    build_exec_ctx: F,
) -> Result<ToolLoopResult, ToolLoopError>
where
    F: Fn() -> UnifiedExecutionContext + Send + Sync,
    M: FnMut(&LlmEngine) -> Vec<Value>,
{
    let TurnLoopSpec {
        allowed_skills,
        callbacks,
        cancel,
    } = spec;
    let tools = registry.to_tool_schemas(allowed_skills);
    let executor = TurnExecutor {
        registry,
        allowed_skills,
        build_exec_ctx,
    };
    let mut retries = 0usize;

    loop {
        let mut messages = build_messages(llm);
        match llm
            .run_tool_loop(&mut messages, &tools, &executor, callbacks, cancel)
            .await
        {
            Err(error) if retries < MAX_PAYLOAD_RETRIES && is_payload_too_large(&error) => {
                // 没有可收缩的会话（NapCat）或已无音频可丢时原样返回，不做无意义的重试
                let Some(source) = source else {
                    return Err(error);
                };
                if !llm.drop_oldest_audio_turn(source) {
                    return Err(error);
                }
                retries += 1;
            }
            result => return result,
        }
    }
}

/// 体积类失败判定：`PayloadTooLarge` 经 `anyhow::Error` → `ToolLoopError::Other` 原样透传，
/// 用 downcast 识别，避免每个调用点各写一份错误的文案比对
fn is_payload_too_large(error: &ToolLoopError) -> bool {
    matches!(error, ToolLoopError::Other(inner) if inner.downcast_ref::<PayloadTooLarge>().is_some())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouterExit {
    Shutdown,
    TeamSpeakDisconnected,
}

#[derive(Clone)]
pub(crate) struct RouterContext {
    pub(crate) config: Arc<AppConfig>,
    pub(crate) prompts: Arc<PromptsConfig>,
    pub(crate) gate: Arc<PermissionGate>,
    pub(crate) llm: Arc<LlmEngine>,
    pub(crate) registry: Arc<SkillRegistry>,
    pub(crate) voice_audio: VoiceAudioHandles,
}

impl RouterContext {
    pub(crate) fn new(
        config: Arc<AppConfig>,
        prompts: Arc<PromptsConfig>,
        gate: Arc<PermissionGate>,
        llm: Arc<LlmEngine>,
        registry: Arc<SkillRegistry>,
        voice_audio: VoiceAudioHandles,
    ) -> Self {
        Self {
            config,
            prompts,
            gate,
            llm,
            registry,
            voice_audio,
        }
    }
}

pub(crate) async fn run_routers(
    context: RouterContext,
    adapter: Arc<TsAdapter>,
    ts_router: EventRouter,
    nc_adapter: Option<Arc<NapCatAdapter>>,
    shutdown: CancellationToken,
) -> Result<RouterExit> {
    let RouterContext {
        config,
        prompts,
        gate,
        llm,
        registry,
        voice_audio: _,
    } = context;
    let napcat_enabled = nc_adapter.is_some();
    if !napcat_enabled {
        info!("NapCat adapter disabled, running in TeamSpeak-only mode");
    }

    let ts_router_run = ts_router.run();
    tokio::pin!(ts_router_run);
    let bot_clid = adapter.get_bot_clid();
    let bot_ctx = match wait_for_ready_or_router(
        describe_bot(&adapter, bot_clid, napcat_enabled),
        ts_router_run.as_mut(),
        &shutdown,
    )
    .await
    {
        ReadyWait::Ready(context) => context,
        ReadyWait::Router(result) => return map_ts_router_result(result),
        ReadyWait::Shutdown => return Ok(RouterExit::Shutdown),
    };
    info!("{bot_ctx}");

    if let Some(nc_adapter) = nc_adapter {
        let nc_router = NcRouter::new_with_ts(
            config,
            prompts,
            nc_adapter,
            gate,
            llm,
            registry,
            Some(adapter),
        );

        tokio::select! {
            biased;
            _ = shutdown.cancelled() => Ok(RouterExit::Shutdown),
            res = &mut ts_router_run => map_ts_router_result(res),
            res = nc_router.run() => map_nc_router_result(res),
        }
    } else {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => Ok(RouterExit::Shutdown),
            res = &mut ts_router_run => map_ts_router_result(res),
        }
    }
}

enum ReadyWait<T, R> {
    Ready(T),
    Router(R),
    Shutdown,
}

async fn wait_for_ready_or_router<ReadyFuture, RouterFuture>(
    ready: ReadyFuture,
    mut router: Pin<&mut RouterFuture>,
    shutdown: &CancellationToken,
) -> ReadyWait<ReadyFuture::Output, RouterFuture::Output>
where
    ReadyFuture: Future,
    RouterFuture: Future,
{
    tokio::pin!(ready);
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => ReadyWait::Shutdown,
        result = router.as_mut() => ReadyWait::Router(result),
        result = &mut ready => ReadyWait::Ready(result),
    }
}

async fn describe_bot(adapter: &TsAdapter, bot_clid: u32, napcat_enabled: bool) -> String {
    let event_sources = if napcat_enabled {
        "TS + NapCat"
    } else {
        "TeamSpeak"
    };

    match adapter.list_clients().await {
        Ok(clients) => {
            if let Some(bot) = clients.iter().find(|client| client.id as u32 == bot_clid) {
                format!(
                    "Bot ready: {}({})[{}]. Listening for {event_sources} events.",
                    bot.nickname, bot.id, bot.channel_id
                )
            } else {
                format!("Bot ready (clid={bot_clid}). Listening for {event_sources} events.")
            }
        }
        Err(_) => format!("Bot ready (clid={bot_clid}). Listening for {event_sources} events."),
    }
}

fn map_ts_router_result(res: Result<()>) -> Result<RouterExit> {
    match res {
        Ok(()) => {
            warn!("TeamSpeak connection lost");
            Ok(RouterExit::TeamSpeakDisconnected)
        }
        Err(e) => {
            error!("TS Event router exited with error: {}", e);
            Err(e)
        }
    }
}

fn map_nc_router_result(res: Result<()>) -> Result<RouterExit> {
    match res {
        Ok(()) => {
            warn!("NC router exited unexpectedly");
            Err(anyhow::anyhow!("NC router exited unexpectedly"))
        }
        Err(e) => {
            error!("NC router error: {e}");
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::context::ContextUser;
    use crate::llm::provider::{LlmProvider, LlmStreamEvent};
    use async_trait::async_trait;
    use futures_util::stream::{self, BoxStream};
    use std::future::pending;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[test]
    fn disconnected_router_has_explicit_exit_reason() {
        assert_eq!(
            map_ts_router_result(Ok(())).unwrap(),
            RouterExit::TeamSpeakDisconnected
        );
    }

    #[tokio::test]
    async fn router_exit_interrupts_stalled_ready_query() {
        let mut router = Box::pin(async {});

        let outcome =
            wait_for_ready_or_router(pending::<()>(), router.as_mut(), &CancellationToken::new())
                .await;

        assert!(matches!(outcome, ReadyWait::Router(())));
    }

    #[tokio::test]
    async fn shutdown_interrupts_stalled_ready_query() {
        let mut router = Box::pin(pending::<()>());
        let shutdown = CancellationToken::new();
        shutdown.cancel();

        let outcome = wait_for_ready_or_router(pending::<()>(), router.as_mut(), &shutdown).await;

        assert!(matches!(outcome, ReadyWait::Shutdown));
    }

    #[derive(Debug, Clone, Copy)]
    enum FakeOutcome {
        PayloadTooLarge,
        HttpError,
        Success,
    }

    /// 脚本化的假 provider：记录调用次数与每次 messages 的序列化体积，
    /// 按脚本顺序返回结果，让体积类重试路径不依赖真实网络
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
                FakeOutcome::Success => {
                    Ok(Box::pin(stream::iter(vec![Ok(LlmStreamEvent::Done {
                        finish_reason: "stop".to_string(),
                        tool_calls: Vec::new(),
                    })])))
                }
            }
        }
    }

    fn engine_with(provider: &Arc<FakeProvider>, max_context_turns: usize) -> LlmEngine {
        LlmEngine::with_provider(Box::new(provider.clone()), max_context_turns)
    }

    async fn run_recovery(
        llm: &LlmEngine,
        source: &SessionSource,
    ) -> Result<ToolLoopResult, ToolLoopError> {
        run_llm_turn_with_audio_recovery(
            llm,
            &SkillRegistry::default(),
            source,
            |llm| llm.build_messages(source, "system", "{}", "hello"),
            TurnLoopSpec {
                allowed_skills: &[],
                callbacks: None,
                cancel: &CancellationToken::new(),
            },
            || panic!("no tool execution in these tests"),
        )
        .await
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

        let result = run_recovery(&llm, &source).await;

        assert!(result.is_ok());
        assert_eq!(provider.calls(), 2);
        let history = llm.stored_history(&source);
        assert_eq!(history.len(), 1);
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

        let result = run_recovery(&llm, &source).await;

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

        let error = run_recovery(&llm, &source).await.unwrap_err();

        assert_eq!(provider.calls(), 2);
        assert!(is_payload_too_large(&error));
        assert_eq!(llm.stored_history(&source).len(), 2);
    }

    #[tokio::test]
    async fn non_payload_errors_keep_the_history() {
        let provider = Arc::new(FakeProvider::new(vec![FakeOutcome::HttpError]));
        let llm = engine_with(&provider, 8);
        let source = SessionSource::TeamSpeak {
            uid: "no-shrink".to_string(),
        };
        llm.save_omni_turn(&source, vec![0u8; 1024], "first-reply".to_string());
        llm.save_omni_turn(&source, vec![0u8; 1024], "second-reply".to_string());

        let error = run_recovery(&llm, &source).await.unwrap_err();

        assert_eq!(provider.calls(), 1);
        assert!(error.to_string().contains("HTTP 500"));
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

        let error = run_recovery(&llm, &source).await.unwrap_err();

        // 文本轮不参与收缩：无音频可丢就不重试，历史原样保留
        assert_eq!(provider.calls(), 1);
        assert!(is_payload_too_large(&error));
        assert_eq!(llm.stored_history(&source).len(), 1);
    }
}
