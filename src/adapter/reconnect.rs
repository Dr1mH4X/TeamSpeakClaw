use std::time::Duration;

use tokio_util::sync::CancellationToken;

/// Reconnect delay sequence (monotonically increasing).
pub(crate) const RECONNECT_DELAYS_MS: [u64; 5] = [10_000, 30_000, 60_000, 120_000, 300_000];

/// Maximum number of reconnect attempts, derived from the delay array.
pub(crate) const MAX_RECONNECT_ATTEMPTS: u32 = RECONNECT_DELAYS_MS.len() as u32;

/// Returns the delay for the n-th reconnect attempt (0-based).
pub(crate) fn reconnect_delay(attempt: u32) -> Duration {
    Duration::from_millis(
        RECONNECT_DELAYS_MS
            .get(attempt as usize)
            .copied()
            .unwrap_or(*RECONNECT_DELAYS_MS.last().unwrap()),
    )
}

/// Returns the delay for the n-th reconnect attempt (1-based).
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
