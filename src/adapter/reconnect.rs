//! 任务回收与限时等待工具：路由退出前限时 join 或 abort 在途任务，
//! 以及按超时停止组件的 join 工具。重连退避策略与会话阶段见 `lifecycle.rs`。

use std::future::Future;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{error, warn};

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
