use std::future::Future;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

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

/// 跟踪当前启动周期的连续失败；至少成功进入一次运行会话后不再耗尽重试次数。
#[derive(Debug, Default)]
pub(crate) struct ReconnectState {
    session_started: bool,
    consecutive_failures: u32,
}

impl ReconnectState {
    pub(crate) fn record_session_started(&mut self) {
        self.session_started = true;
        self.consecutive_failures = 0;
    }

    pub(crate) fn record_failure(&mut self) -> RetryDecision {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);

        if !self.session_started && self.consecutive_failures >= MAX_RECONNECT_ATTEMPTS {
            return RetryDecision::Exhausted;
        }

        RetryDecision::Retry {
            attempt: self.consecutive_failures,
            delay: reconnect_delay_for_attempt(self.consecutive_failures),
        }
    }

    pub(crate) fn has_started_session(&self) -> bool {
        self.session_started
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

/// 路由退出前回收在途任务：优先限时 join，超时后 abort 并收割。
pub(crate) const TASK_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// 限时 join JoinSet 中的全部任务；超时后 abort 并收割。
/// `label` 用于日志前缀（如 "TS message"）。
pub(crate) async fn drain_managed_tasks(tasks: &mut JoinSet<()>, label: &str) {
    loop {
        let result = tokio::time::timeout(TASK_DRAIN_TIMEOUT, tasks.join_next()).await;
        match result {
            Ok(Some(Ok(()))) => {}
            Ok(Some(Err(error))) => {
                error!("{label} task failed: {error}");
            }
            Ok(None) => break,
            Err(_) => {
                warn!(
                    timeout_secs = TASK_DRAIN_TIMEOUT.as_secs(),
                    "{label} tasks exceeded drain timeout; aborting"
                );
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
                break;
            }
        }
    }
}

/// 直接 abort 全部任务并收割；仅记录非取消导致的失败。
pub(crate) async fn abort_managed_tasks(tasks: &mut JoinSet<()>, label: &str) {
    tasks.abort_all();
    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result {
            if !error.is_cancelled() {
                error!("{label} task failed during shutdown: {error}");
            }
        }
    }
}

/// 限时等待 future 完成；超时返回 `None`（不中止任务，调用方自行决定后续）。
pub(crate) async fn wait_with_timeout<F, T>(future: F, timeout: Duration) -> Option<T>
where
    F: Future<Output = T>,
{
    tokio::time::timeout(timeout, future).await.ok()
}

/// 限时 join 任务句柄；超时则 abort 并返回错误。
pub(crate) async fn join_or_abort<T>(
    handle: &mut JoinHandle<T>,
    component: &str,
    timeout: Duration,
) -> Result<T> {
    match wait_with_timeout(&mut *handle, timeout).await {
        Some(result) => result.with_context(|| format!("failed to join {component}")),
        None => {
            handle.abort();
            let _ = handle.await;
            Err(anyhow!(
                "timed out after {} seconds while stopping {component}",
                timeout.as_secs()
            ))
        }
    }
}

/// 当前 Unix 毫秒时间戳
pub(crate) fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// 当前 Unix 秒时间戳
pub(crate) fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::{
        wait_for_retry, ReconnectState, RetryDecision, MAX_RECONNECT_ATTEMPTS, RECONNECT_DELAYS_MS,
    };
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
}
