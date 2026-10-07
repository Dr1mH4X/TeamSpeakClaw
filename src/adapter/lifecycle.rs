//! 会话生命周期的唯一归属地：连接尝试（`Connecting`）、会话建立、组件就绪
//! （`Initializing -> Running`）与关闭（`Closed`）的阶段迁移，以及重试退避策略，
//! 都在这里定义。其余模块只报告事件或只读状态，不自行推进阶段。
//!
//! 组件就绪事件模型尚未接入：当前由适配器在进入路由运行后直接报告 `Running`，
//! 逐组件 ready 上报驱动 `Initializing -> Running` 的模型待后续接入。
//! 语音桥组件就绪（`BridgeReadiness`）的写入归属也在此模块：其余模块只报告
//! `BridgeComponent` 的 Up/Down 事件，不自行持有或改写具体标志位。
//! 组件就绪写入已全部经 `BridgeReadiness` 上报：适配器、actor 与路由层都直接调用
//! `set_up` / `set_down` / `take_stream_established`，不再有中间的薄门面结构。
//!
//! 重试驱动 `run_retry_loop` 也由本模块拥有：适配器只执行单次尝试并用自身措辞记录失败，
//! 记账、会话建立后的退避重置与等待关闭都在驱动内完成。

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

use crate::router::RouterExit;

/// 重连延迟序列（单调递增）
pub(crate) const RECONNECT_DELAYS_MS: [u64; 5] = [10_000, 30_000, 60_000, 120_000, 300_000];

/// 最大重连尝试次数，由延迟数组推导
pub(crate) const MAX_RECONNECT_ATTEMPTS: u32 = RECONNECT_DELAYS_MS.len() as u32;

/// 返回第 n 次重连尝试的延迟（从 0 开始）
pub(crate) fn reconnect_delay(attempt: u32) -> Duration {
    Duration::from_millis(
        RECONNECT_DELAYS_MS
            .get(attempt as usize)
            .copied()
            .unwrap_or(*RECONNECT_DELAYS_MS.last().unwrap()),
    )
}

/// 返回第 n 次重连尝试的延迟（从 1 开始）
pub(crate) fn reconnect_delay_for_attempt(attempt_1_based: u32) -> Duration {
    reconnect_delay(attempt_1_based.saturating_sub(1))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryDecision {
    Retry { attempt: u32, delay: Duration },
    Exhausted,
}

/// 会话所处的生命周期阶段，按 `Connecting -> Initializing -> Running -> Closed` 推进。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionPhase {
    /// 尚未建立连接，正在尝试。
    Connecting,
    /// 连接已建立，组件尚未就绪。
    Initializing,
    /// 已进入路由运行。
    Running,
    /// 已进入路由运行的会话收尾完毕。
    Closed,
}

/// 一次会话结束时的阶段与结果。
#[derive(Debug)]
pub(crate) struct SessionCompletion {
    phase: SessionPhase,
    pub(crate) result: Result<RouterExit>,
}

impl SessionCompletion {
    /// 连接已建立、组件或路由未就绪即结束的会话。
    pub(crate) fn initialization(result: Result<RouterExit>) -> Self {
        Self {
            phase: SessionPhase::Initializing,
            result,
        }
    }

    /// 已进入路由运行的会话。
    pub(crate) fn running(result: Result<RouterExit>) -> Self {
        Self {
            phase: SessionPhase::Running,
            result,
        }
    }

    /// 会话收尾：已进入路由运行的会话转入终态 `Closed`，未进入运行的会话保持
    /// `Initializing`。`entered_running()` 因此在收尾后仍反映真实的启动历史。
    pub(crate) fn close(mut self) -> Self {
        if matches!(self.phase, SessionPhase::Running) {
            self.phase = SessionPhase::Closed;
        }
        self
    }

    /// 派生读法：会话是否已进入路由运行阶段。
    pub(crate) fn entered_running(&self) -> bool {
        matches!(self.phase, SessionPhase::Running | SessionPhase::Closed)
    }
}

/// 跟踪当前启动周期的连续失败；至少成功进入一次运行会话后不再耗尽重试次数。
#[derive(Debug)]
pub(crate) struct ReconnectState {
    /// 本轮启动周期到达的最远阶段：成功进入运行会话后为 `Running`。
    phase: SessionPhase,
    consecutive_failures: u32,
}

impl Default for ReconnectState {
    fn default() -> Self {
        Self {
            phase: SessionPhase::Connecting,
            consecutive_failures: 0,
        }
    }
}

impl ReconnectState {
    pub(crate) fn record_session_started(&mut self) {
        self.phase = SessionPhase::Running;
        self.consecutive_failures = 0;
    }

    pub(crate) fn record_failure(&mut self) -> RetryDecision {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);

        if !self.has_started_session() && self.consecutive_failures >= MAX_RECONNECT_ATTEMPTS {
            return RetryDecision::Exhausted;
        }

        RetryDecision::Retry {
            attempt: self.consecutive_failures,
            delay: reconnect_delay_for_attempt(self.consecutive_failures),
        }
    }

    pub(crate) fn has_started_session(&self) -> bool {
        matches!(self.phase, SessionPhase::Running)
    }
}

/// 等待下一次重试；返回 `false` 表示根取消令牌先触发。
pub(crate) async fn wait_for_retry(delay: Duration, shutdown: &CancellationToken) -> bool {
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => false,
        _ = tokio::time::sleep(delay) => true,
    }
}

/// 一次连接或会话失败所属的类别，决定重试日志的措辞与耗尽时的归属。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureKind {
    /// TeamSpeak 连接建立失败。
    Connection,
    /// 连接已建立但主订阅取出失败。
    Subscription,
    /// 未进入路由运行的会话初始化失败。
    Initialization,
    /// 已进入路由运行的会话失败或断开。
    Running,
}

/// 唯一重试决策点的结果：继续循环，或等待期间关闭而正常退出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryAction {
    /// 失败已记账、已等待完毕，调用方继续下一轮尝试。
    ContinueLoop,
    /// 等待期间根取消令牌触发，调用方直接返回 `Ok(())`。
    Shutdown,
}

/// 连接失败在尝试次数耗尽时的日志模板。
const CONNECTION_ATTEMPTS_EXHAUSTED_LOG: &str =
    "All {MAX_RECONNECT_ATTEMPTS} initial connection attempts exhausted. Last error: {error}";

/// 会话启动失败在尝试次数耗尽时的日志模板。
const STARTUP_ATTEMPTS_EXHAUSTED_LOG: &str =
    "All {MAX_RECONNECT_ATTEMPTS} initial startup attempts exhausted. Last error: {failure}";

/// 尝试次数耗尽时该失败类别对应的日志模板；`None` 表示保持静默。
///
/// 重构前连接失败会打印耗尽日志，而主订阅取出失败是静默 `return Err`，两者不可合并，
/// 故由本映射唯一决定「耗尽时打哪条日志」。
pub(crate) fn exhaustion_log(kind: FailureKind) -> Option<&'static str> {
    match kind {
        FailureKind::Connection => Some(CONNECTION_ATTEMPTS_EXHAUSTED_LOG),
        FailureKind::Subscription => None,
        FailureKind::Initialization | FailureKind::Running => Some(STARTUP_ATTEMPTS_EXHAUSTED_LOG),
    }
}

/// 唯一的失败决策点：记账一次失败，按类别选择日志，等待退避，并把结果归类为
/// 继续循环、正常退出或带错误退出。未运行过会话即耗尽尝试次数时返回 `Err`，
/// 携带最后一次失败原因。
pub(crate) async fn retry_after_failure(
    reconnect: &mut ReconnectState,
    kind: FailureKind,
    failure: anyhow::Error,
    shutdown: &CancellationToken,
) -> Result<RetryAction> {
    // 已进入运行会话后本轮启动周期不再受尝试次数限制，读法与记账前后一致。
    let had_running_session = reconnect.has_started_session();
    // 连接与订阅路径的既有日志以 error 命名失败原因，会话路径以 failure 命名；
    // 统一决策点只保留一个失败值，此处别名仅为保留原有的日志模板。
    let error = &failure;

    let (attempt, delay) = match reconnect.record_failure() {
        RetryDecision::Retry { attempt, delay } => (attempt, delay),
        RetryDecision::Exhausted => {
            // 主订阅取出失败在重构前耗尽即静默返回，不得并入连接路径的耗尽日志。
            match exhaustion_log(kind) {
                None => {}
                Some(CONNECTION_ATTEMPTS_EXHAUSTED_LOG) => error!(
                    "All {MAX_RECONNECT_ATTEMPTS} initial connection attempts exhausted. Last error: {error}"
                ),
                Some(_) => error!(
                    "All {MAX_RECONNECT_ATTEMPTS} initial startup attempts exhausted. Last error: {failure}"
                ),
            }
            return Err(failure);
        }
    };

    match kind {
        FailureKind::Subscription => {
            if !had_running_session {
                warn!(
                    "Initial TeamSpeak connection attempt {attempt}/{MAX_RECONNECT_ATTEMPTS} failed; retrying after {delay:.0?}"
                );
            }
        }
        FailureKind::Connection => {
            if had_running_session {
                warn!(
                    "TeamSpeak reconnect attempt {attempt} failed: {error}; retrying after {delay:.0?}"
                );
            } else {
                warn!(
                    "Initial TeamSpeak connection attempt {attempt}/{MAX_RECONNECT_ATTEMPTS} failed: {error}; retrying after {delay:.0?}"
                );
            }
        }
        FailureKind::Initialization | FailureKind::Running => {
            if had_running_session {
                warn!("TeamSpeak reconnect attempt {attempt} scheduled after {delay:.0?}");
            } else {
                warn!(
                    "Initial startup attempt {attempt}/{MAX_RECONNECT_ATTEMPTS} failed: {failure}; retrying after {delay:.0?}"
                );
            }
        }
    }

    if wait_for_retry(delay, shutdown).await {
        Ok(RetryAction::ContinueLoop)
    } else {
        Ok(RetryAction::Shutdown)
    }
}

/// 一次尝试的结局：失败并按退避策略重试，或因根取消令牌触发而不计账地结束。
pub(crate) enum AttemptOutcome {
    /// 本轮尝试以失败告终；`session_started` 为真时先重置退避再记账。
    Failed {
        /// 失败原因，有界用法耗尽尝试次数时由驱动交回调用方。
        error: anyhow::Error,
        /// 本轮尝试内是否成功建立过会话。
        session_started: bool,
    },
    /// 根取消令牌触发，本轮不计账，驱动随即正常返回。
    Cancelled,
}

/// 可测的重试驱动：反复执行一次会话尝试，失败后记账并按共享退避策略等待。
///
/// `attempt` 每轮执行一次尝试并报告结局，`on_failure` 在记账后、等待前收到尝试序号与
/// 延迟，由调用方用自身措辞记录日志。有界用法（未标记过会话建立）耗尽尝试次数时返回
/// `Err`，携带最后一次失败原因；`Ok(())` 一律表示根取消令牌触发而正常结束。
pub(crate) async fn run_retry_loop<F, Fut, R>(
    reconnect: &mut ReconnectState,
    shutdown: &CancellationToken,
    mut attempt: F,
    mut on_failure: R,
) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = AttemptOutcome>,
    R: FnMut(u32, Duration),
{
    loop {
        let AttemptOutcome::Failed {
            error,
            session_started,
        } = attempt().await
        else {
            return Ok(());
        };
        if session_started {
            reconnect.record_session_started();
        }
        let (attempt_number, delay) = match reconnect.record_failure() {
            RetryDecision::Retry { attempt, delay } => (attempt, delay),
            RetryDecision::Exhausted => return Err(error),
        };
        on_failure(attempt_number, delay);
        if !wait_for_retry(delay, shutdown).await {
            return Ok(());
        }
    }
}

/// 语音桥的组件身份：gRPC 服务在跑 / VoiceRouter 已建立事件订阅流 /
/// TS3 actor 事件 handler 已注册。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BridgeComponent {
    Service,
    Stream,
    Actor,
}

#[derive(Default)]
struct BridgeReadinessInner {
    service_up: AtomicBool,
    stream_up: AtomicBool,
    actor_up: AtomicBool,
    stream_established_since_retry: AtomicBool,
}

/// 语音桥组件就绪的唯一归属：三者全 Up 才算就绪；订阅流转入 Up 时顺带置位
/// 「本轮尝试内建立过订阅流」闩，供桥接重试循环取走（取走即清零）。
#[derive(Clone, Default)]
pub(crate) struct BridgeReadiness {
    inner: Arc<BridgeReadinessInner>,
}

impl BridgeReadiness {
    pub(crate) fn is_ready(&self) -> bool {
        self.inner.service_up.load(Ordering::Acquire)
            && self.inner.stream_up.load(Ordering::Acquire)
            && self.inner.actor_up.load(Ordering::Acquire)
    }

    pub(crate) fn set_up(&self, component: BridgeComponent) {
        self.flag(component).store(true, Ordering::Release);
        if component == BridgeComponent::Stream {
            self.inner
                .stream_established_since_retry
                .store(true, Ordering::Release);
        }
    }

    pub(crate) fn set_down(&self, component: BridgeComponent) {
        self.flag(component).store(false, Ordering::Release);
    }

    /// 取走「本轮尝试内建立过订阅流」闩：Stream 每次转入 Up 都会重新置位。
    pub(crate) fn take_stream_established(&self) -> bool {
        self.inner
            .stream_established_since_retry
            .swap(false, Ordering::AcqRel)
    }

    fn flag(&self, component: BridgeComponent) -> &AtomicBool {
        match component {
            BridgeComponent::Service => &self.inner.service_up,
            BridgeComponent::Stream => &self.inner.stream_up,
            BridgeComponent::Actor => &self.inner.actor_up,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        exhaustion_log, retry_after_failure, run_retry_loop, wait_for_retry, AttemptOutcome,
        BridgeComponent, BridgeReadiness, FailureKind, ReconnectState, RetryAction, RetryDecision,
        SessionCompletion, SessionPhase, MAX_RECONNECT_ATTEMPTS, RECONNECT_DELAYS_MS,
    };
    use crate::router::RouterExit;
    use std::cell::Cell;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn bridge_readiness_requires_every_component_up() {
        let readiness = BridgeReadiness::default();
        assert!(!readiness.is_ready());

        readiness.set_up(BridgeComponent::Service);
        readiness.set_up(BridgeComponent::Stream);
        assert!(!readiness.is_ready());

        readiness.set_up(BridgeComponent::Actor);
        assert!(readiness.is_ready());
    }

    #[test]
    fn bridge_readiness_is_unready_before_actor_handler_registration() {
        let readiness = BridgeReadiness::default();
        readiness.set_up(BridgeComponent::Service);
        readiness.set_up(BridgeComponent::Stream);

        // actor handler 注册前即使服务与订阅流都正常也不就绪
        assert!(!readiness.is_ready());

        readiness.set_up(BridgeComponent::Actor);
        assert!(readiness.is_ready());
    }

    #[test]
    fn bridge_stream_latch_is_consumed_once() {
        let readiness = BridgeReadiness::default();
        readiness.set_up(BridgeComponent::Stream);

        assert!(readiness.take_stream_established());
        assert!(!readiness.take_stream_established());
    }

    #[test]
    fn bridge_stream_latch_rearms_when_stream_returns_up() {
        let readiness = BridgeReadiness::default();
        readiness.set_up(BridgeComponent::Stream);
        assert!(readiness.take_stream_established());

        readiness.set_down(BridgeComponent::Stream);
        readiness.set_up(BridgeComponent::Stream);

        assert!(readiness.take_stream_established());
        assert!(!readiness.take_stream_established());
    }

    #[test]
    fn bridge_readiness_flips_when_any_component_goes_down() {
        let readiness = BridgeReadiness::default();
        for component in [
            BridgeComponent::Service,
            BridgeComponent::Stream,
            BridgeComponent::Actor,
        ] {
            readiness.set_up(component);
        }
        assert!(readiness.is_ready());

        for component in [
            BridgeComponent::Service,
            BridgeComponent::Stream,
            BridgeComponent::Actor,
        ] {
            readiness.set_down(component);
            assert!(!readiness.is_ready());
            readiness.set_up(component);
            assert!(readiness.is_ready());
        }
    }

    #[test]
    fn subscription_exhaustion_stays_silent() {
        // 主订阅取出失败在重构前耗尽时静默 `return Err`，不得与连接失败共用耗尽日志；
        // 此断言固定该差异，防止后续又被并回连接路径。
        assert_eq!(exhaustion_log(FailureKind::Subscription), None);
    }

    #[test]
    fn connection_exhaustion_uses_the_connection_template() {
        assert_eq!(
            exhaustion_log(FailureKind::Connection),
            Some(
                "All {MAX_RECONNECT_ATTEMPTS} initial connection attempts exhausted. Last error: {error}"
            )
        );
    }

    #[test]
    fn session_exhaustion_uses_the_startup_template() {
        let startup = "All {MAX_RECONNECT_ATTEMPTS} initial startup attempts exhausted. Last error: {failure}";
        assert_eq!(exhaustion_log(FailureKind::Initialization), Some(startup));
        assert_eq!(exhaustion_log(FailureKind::Running), Some(startup));
    }

    /// 只报告失败、未建立会话的单次尝试。
    fn failing_attempt() -> AttemptOutcome {
        AttemptOutcome::Failed {
            error: anyhow::anyhow!("attempt failed"),
            session_started: false,
        }
    }

    #[test]
    fn initial_failures_are_bounded() {
        let mut state = ReconnectState::default();

        for (index, delay_ms) in RECONNECT_DELAYS_MS
            .iter()
            .take((MAX_RECONNECT_ATTEMPTS - 1) as usize)
            .enumerate()
        {
            assert_eq!(
                state.record_failure(),
                RetryDecision::Retry {
                    attempt: index as u32 + 1,
                    delay: Duration::from_millis(*delay_ms),
                }
            );
        }

        assert_eq!(state.record_failure(), RetryDecision::Exhausted);
    }

    #[test]
    fn successful_session_start_resets_backoff_and_removes_retry_limit() {
        let mut state = ReconnectState::default();
        assert_eq!(
            state.record_failure(),
            RetryDecision::Retry {
                attempt: 1,
                delay: Duration::from_millis(10_000)
            }
        );

        state.record_session_started();
        assert!(state.has_started_session());

        // 会话断开后先等待 10 秒；首次实际重连失败后退避到 30 秒。
        assert_eq!(
            state.record_failure(),
            RetryDecision::Retry {
                attempt: 1,
                delay: Duration::from_millis(10_000),
            }
        );
        assert_eq!(
            state.record_failure(),
            RetryDecision::Retry {
                attempt: 2,
                delay: Duration::from_millis(30_000),
            }
        );
        for _ in 0..MAX_RECONNECT_ATTEMPTS + 2 {
            assert!(matches!(
                state.record_failure(),
                RetryDecision::Retry { .. }
            ));
        }

        state.record_session_started();
        assert_eq!(
            state.record_failure(),
            RetryDecision::Retry {
                attempt: 1,
                delay: Duration::from_millis(10_000),
            }
        );
    }

    #[tokio::test]
    async fn retry_wait_stops_when_shutdown_is_cancelled() {
        let shutdown = CancellationToken::new();
        shutdown.cancel();

        let completed = tokio::time::timeout(
            Duration::from_millis(100),
            wait_for_retry(Duration::from_secs(300), &shutdown),
        )
        .await
        .expect("cancelled retry wait must complete promptly");

        assert!(!completed);
    }

    #[test]
    fn reconnect_state_starts_in_the_connecting_phase() {
        let state = ReconnectState::default();

        assert_eq!(state.phase, SessionPhase::Connecting);
        assert!(!state.has_started_session());
    }

    #[test]
    fn initialization_completion_is_initializing_and_not_running() {
        let initialization = SessionCompletion::initialization(Err(anyhow::anyhow!("failed")));

        assert_eq!(initialization.phase, SessionPhase::Initializing);
        assert!(!initialization.entered_running());
    }

    #[test]
    fn running_completion_is_running() {
        let running = SessionCompletion::running(Ok(RouterExit::TeamSpeakDisconnected));

        assert_eq!(running.phase, SessionPhase::Running);
        assert!(running.entered_running());
    }

    #[test]
    fn only_running_completion_marks_session_started() {
        let initialization = SessionCompletion::initialization(Err(anyhow::anyhow!("failed")));
        let running = SessionCompletion::running(Ok(RouterExit::TeamSpeakDisconnected));

        assert!(!initialization.entered_running());
        assert!(running.entered_running());
    }

    #[test]
    fn running_completion_closes_into_closed_phase() {
        let closed = SessionCompletion::running(Ok(RouterExit::TeamSpeakDisconnected)).close();

        assert_eq!(closed.phase, SessionPhase::Closed);
        assert!(closed.entered_running());
    }

    #[test]
    fn initialization_completion_stays_initializing_after_close() {
        let closed = SessionCompletion::initialization(Err(anyhow::anyhow!("failed"))).close();

        assert_eq!(closed.phase, SessionPhase::Initializing);
        assert!(!closed.entered_running());
    }

    #[tokio::test(start_paused = true)]
    async fn first_failure_without_a_running_session_retries_after_the_first_delay() {
        let mut state = ReconnectState::default();
        let shutdown = CancellationToken::new();
        let started = tokio::time::Instant::now();

        let action = retry_after_failure(
            &mut state,
            FailureKind::Connection,
            anyhow::anyhow!("connect failed"),
            &shutdown,
        )
        .await
        .expect("未运行过的首次失败必须重试");

        assert_eq!(action, RetryAction::ContinueLoop);
        assert_eq!(state.consecutive_failures, 1);
        assert_eq!(
            tokio::time::Instant::now() - started,
            Duration::from_millis(RECONNECT_DELAYS_MS[0])
        );
    }

    #[tokio::test(start_paused = true)]
    async fn failure_after_a_running_session_restarts_attempt_numbering() {
        let mut state = ReconnectState::default();
        let shutdown = CancellationToken::new();

        retry_after_failure(
            &mut state,
            FailureKind::Connection,
            anyhow::anyhow!("initial connect failed"),
            &shutdown,
        )
        .await
        .expect("未运行过的首次失败必须重试");
        state.record_session_started();
        let started = tokio::time::Instant::now();

        let action = retry_after_failure(
            &mut state,
            FailureKind::Running,
            anyhow::anyhow!("running session failed"),
            &shutdown,
        )
        .await
        .expect("运行过会话后的失败必须重试");

        assert_eq!(action, RetryAction::ContinueLoop);
        assert_eq!(state.consecutive_failures, 1);
        assert_eq!(
            tokio::time::Instant::now() - started,
            Duration::from_millis(RECONNECT_DELAYS_MS[0])
        );
    }

    #[tokio::test(start_paused = true)]
    async fn initial_failures_exhaust_the_budget_with_the_last_error() {
        let mut state = ReconnectState::default();
        let shutdown = CancellationToken::new();

        for attempt in 1..MAX_RECONNECT_ATTEMPTS {
            let action = retry_after_failure(
                &mut state,
                FailureKind::Initialization,
                anyhow::anyhow!("startup {attempt} failed"),
                &shutdown,
            )
            .await
            .expect("未运行过时的前几次失败必须重试");

            assert_eq!(action, RetryAction::ContinueLoop);
        }

        let error = retry_after_failure(
            &mut state,
            FailureKind::Initialization,
            anyhow::anyhow!("last startup error"),
            &shutdown,
        )
        .await
        .expect_err("未运行过时耗尽尝试次数必须带错误退出");

        assert_eq!(error.to_string(), "last startup error");
    }

    #[tokio::test(start_paused = true)]
    async fn failures_after_a_running_session_never_exhaust_the_budget() {
        let mut state = ReconnectState::default();
        let shutdown = CancellationToken::new();
        state.record_session_started();

        for attempt in 1..=MAX_RECONNECT_ATTEMPTS + 2 {
            let action = retry_after_failure(
                &mut state,
                FailureKind::Running,
                anyhow::anyhow!("reconnect {attempt} failed"),
                &shutdown,
            )
            .await
            .expect("运行过会话后不再耗尽尝试次数");

            assert_eq!(action, RetryAction::ContinueLoop);
        }
    }

    #[tokio::test]
    async fn shutdown_during_the_retry_wait_reports_shutdown() {
        let mut state = ReconnectState::default();
        let shutdown = CancellationToken::new();
        shutdown.cancel();

        let action = retry_after_failure(
            &mut state,
            FailureKind::Running,
            anyhow::anyhow!("session disconnected"),
            &shutdown,
        )
        .await
        .expect("关闭不是失败");

        assert_eq!(action, RetryAction::Shutdown);
    }

    #[tokio::test(start_paused = true)]
    async fn unbounded_retry_loop_walks_the_shared_backoff_and_caps_it() {
        let mut state = ReconnectState::default();
        state.record_session_started();
        let shutdown = CancellationToken::new();
        let stop = shutdown.clone();
        let mut recorded = Vec::new();

        let result = run_retry_loop(
            &mut state,
            &shutdown,
            || async { failing_attempt() },
            |attempt, delay| {
                recorded.push((attempt, delay));
                if recorded.len() == 6 {
                    stop.cancel();
                }
            },
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(
            recorded,
            vec![
                (1, Duration::from_secs(10)),
                (2, Duration::from_secs(30)),
                (3, Duration::from_secs(60)),
                (4, Duration::from_secs(120)),
                (5, Duration::from_secs(300)),
                (6, Duration::from_secs(300)),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_attempt_that_started_a_session_resets_the_backoff() {
        let mut state = ReconnectState::default();
        state.record_session_started();
        let shutdown = CancellationToken::new();
        let stop = shutdown.clone();
        let calls = Cell::new(0u32);
        let mut recorded = Vec::new();

        let result = run_retry_loop(
            &mut state,
            &shutdown,
            || {
                let session_started = calls.replace(calls.get() + 1) == 2;
                async move {
                    AttemptOutcome::Failed {
                        error: anyhow::anyhow!("attempt failed"),
                        session_started,
                    }
                }
            },
            |attempt, delay| {
                recorded.push((attempt, delay));
                if recorded.len() == 4 {
                    stop.cancel();
                }
            },
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(
            recorded,
            vec![
                (1, Duration::from_secs(10)),
                (2, Duration::from_secs(30)),
                (1, Duration::from_secs(10)),
                (2, Duration::from_secs(30)),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_during_the_wait_stops_the_loop_without_another_attempt() {
        let mut state = ReconnectState::default();
        state.record_session_started();
        let shutdown = CancellationToken::new();
        let stop = shutdown.clone();
        let attempts = Cell::new(0u32);
        let started = tokio::time::Instant::now();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            stop.cancel();
        });

        let result = run_retry_loop(
            &mut state,
            &shutdown,
            || {
                attempts.set(attempts.get() + 1);
                async { failing_attempt() }
            },
            |_, _| {},
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(attempts.get(), 1);
        assert_eq!(
            tokio::time::Instant::now() - started,
            Duration::from_secs(5)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unbounded_retry_loop_continues_past_the_attempt_budget() {
        let mut state = ReconnectState::default();
        state.record_session_started();
        let shutdown = CancellationToken::new();
        let stop = shutdown.clone();
        let attempts = Cell::new(0u32);

        let result = run_retry_loop(
            &mut state,
            &shutdown,
            || {
                attempts.set(attempts.get() + 1);
                async { failing_attempt() }
            },
            |_, _| {
                if attempts.get() == MAX_RECONNECT_ATTEMPTS + 3 {
                    stop.cancel();
                }
            },
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(attempts.get(), MAX_RECONNECT_ATTEMPTS + 3);
        assert_eq!(state.consecutive_failures, MAX_RECONNECT_ATTEMPTS + 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_attempt_ends_the_loop_without_recording_a_failure() {
        let mut state = ReconnectState::default();
        let shutdown = CancellationToken::new();
        let attempts = Cell::new(0u32);
        let failures = Cell::new(0u32);
        let started = tokio::time::Instant::now();

        let result = run_retry_loop(
            &mut state,
            &shutdown,
            || {
                attempts.set(attempts.get() + 1);
                async { AttemptOutcome::Cancelled }
            },
            |_, _| failures.set(failures.get() + 1),
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(attempts.get(), 1);
        assert_eq!(failures.get(), 0);
        assert_eq!(state.consecutive_failures, 0);
        assert_eq!(tokio::time::Instant::now() - started, Duration::ZERO);
    }
}
