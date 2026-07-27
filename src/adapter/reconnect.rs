use std::time::Duration;

/// 最大重连尝试次数
pub(crate) const MAX_RECONNECT_ATTEMPTS: u32 = 5;

/// 重连等待时间序列（依次递增）
pub(crate) const RECONNECT_DELAYS_MS: [u64; 5] = [10_000, 30_000, 60_000, 120_000, 300_000];

/// 获取第 n 次重试的等待时间
pub(crate) fn reconnect_delay(attempt: u32) -> Duration {
    Duration::from_millis(
        RECONNECT_DELAYS_MS
            .get(attempt as usize)
            .copied()
            .unwrap_or(*RECONNECT_DELAYS_MS.last().unwrap()),
    )
}
