use dashmap::DashMap;
use serde_json::json;
use std::sync::Arc;

/// 单轮对话中允许保留的最大消息条数（硬上限）。
/// context_window 表示"轮数"，每轮 = 1 user + 1 assistant = 2 条消息。
/// 因此实际保留消息数 = min(ctx_window * 2, MAX_HISTORY_MSGS)。
const MAX_HISTORY_MSGS: usize = 20;

/// 最大追踪客户端数量，防止内存无限增长。
const MAX_CLIENTS: usize = 1000;

/// 共享的每客户端对话历史管理器。
///
/// 统一语义：
/// - `context_window` 代表保留的对话轮数（每轮包含 user + assistant 各一条消息）
/// - 每次保存完整轮次，避免重复追加
/// - 自动截断超出上限的旧消息
/// - 当客户端数量超过 MAX_CLIENTS 时，自动清理最早的客户端
#[derive(Clone)]
pub struct ChatHistory {
    store: Arc<DashMap<i64, Vec<serde_json::Value>>>,
}

impl ChatHistory {
    pub fn new() -> Self {
        Self {
            store: Arc::new(DashMap::new()),
        }
    }

    /// 获取指定客户端的历史消息，用于构建 LLM 上下文。
    ///
    /// 返回最近 `ctx_window * 2` 条消息（不超过 `MAX_HISTORY_MSGS`）。
    pub fn get_history(&self, client_id: i64, ctx_window: usize) -> Vec<serde_json::Value> {
        if ctx_window == 0 {
            return Vec::new();
        }
        let keep = usize::min(ctx_window * 2, MAX_HISTORY_MSGS);
        self.store
            .get(&client_id)
            .map(|hist| {
                let total = hist.len();
                let start = total.saturating_sub(keep);
                hist.iter().skip(start).cloned().collect()
            })
            .unwrap_or_default()
    }

    /// 保存一个完整的对话轮次（user 消息 + assistant 回复）。
    ///
    /// 每次调用会追加一条 user 消息和一条 assistant 消息，然后自动截断。
    /// 这避免了重复追加历史消息的问题。
    pub fn save_turn(
        &self,
        client_id: i64,
        user_msg: &str,
        assistant_msg: &str,
        ctx_window: usize,
    ) {
        if ctx_window == 0 {
            return;
        }

        // 当客户端数量超过上限时，清理部分最早的客户端
        if self.store.len() >= MAX_CLIENTS {
            // 移除 10% 的客户端（至少移除 1 个）
            let remove_count = (MAX_CLIENTS / 10).max(1);
            let keys: Vec<i64> = self
                .store
                .iter()
                .take(remove_count)
                .map(|e| *e.key())
                .collect();
            for key in keys {
                self.store.remove(&key);
            }
        }

        let mut hist = self.store.entry(client_id).or_default();

        // 追加本轮对话
        hist.push(json!({"role": "user", "content": user_msg}));
        hist.push(json!({"role": "assistant", "content": assistant_msg}));

        // 截断超出上限的旧消息
        let keep = usize::min(ctx_window * 2, MAX_HISTORY_MSGS);
        if hist.len() > keep {
            let drop_count = hist.len() - keep;
            hist.drain(..drop_count);
        }
    }
}
