//! 会话生命周期的唯一归属地：连接尝试（`Connecting`）、会话建立、组件就绪
//! （`Initializing -> Running`）与关闭（`Closed`）的阶段迁移，以及重试退避策略，
//! 都在这里定义。其余模块只报告事件或只读状态，不自行推进阶段。
//!
//! 组件就绪事件模型尚未接入：当前由适配器在进入路由运行后直接报告 `Running`，
//! 逐组件 ready 上报驱动 `Initializing -> Running` 的模型待后续接入。

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
            match kind {
                FailureKind::Connection | FailureKind::Subscription => error!(
                    "All {MAX_RECONNECT_ATTEMPTS} initial connection attempts exhausted. Last error: {error}"
                ),
                FailureKind::Initialization | FailureKind::Running => error!(
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

#[cfg(test)]
mod tests {
    use super::{
        retry_after_failure, wait_for_retry, FailureKind, ReconnectState, RetryAction,
        RetryDecision, SessionCompletion, SessionPhase, MAX_RECONNECT_ATTEMPTS,
        RECONNECT_DELAYS_MS,
    };
    use crate::router::RouterExit;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

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
}
