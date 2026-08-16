use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// 按超时参数化的共享 HTTP 客户端：同一超时值只构建一次。
/// 返回克隆（`reqwest::Client` 内部引用计数，克隆开销很小）。
pub(crate) fn shared_client(timeout_secs: u64) -> reqwest::Client {
    static CLIENTS: OnceLock<Mutex<HashMap<u64, reqwest::Client>>> = OnceLock::new();
    let clients = CLIENTS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = clients.lock().expect("http client registry poisoned");
    guard
        .entry(timeout_secs)
        .or_insert_with(|| {
            reqwest::Client::builder()
                .timeout(Duration::from_secs(timeout_secs))
                .build()
                .expect("failed to build reqwest::Client")
        })
        .clone()
}
