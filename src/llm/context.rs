use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

/// 会话来源
#[derive(Debug, Clone)]
pub enum SessionSource {
    /// TeamSpeak 客户端
    TeamSpeak { uid: String },
    /// NapCat 私聊
    NapCatPrivate { user_id: i64 },
    /// NapCat 群聊
    NapCatGroup { group_id: i64 },
    /// Headless 模式
    Headless { uid: String },
}

impl SessionSource {
    /// 返回跨适配器唯一且稳定的会话键。
    pub(crate) fn canonical_key(&self) -> String {
        match self {
            SessionSource::TeamSpeak { uid } => format!("sq:{uid}"),
            SessionSource::NapCatPrivate { user_id } => format!("nc:private:{user_id}"),
            SessionSource::NapCatGroup { group_id } => format!("nc:group:{group_id}"),
            SessionSource::Headless { uid } => format!("headless:{uid}"),
        }
    }
}

impl fmt::Display for SessionSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical_key())
    }
}

/// 轮次预留失败：容量（并发 + 排队）已满
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnQueueFull;

/// 已预留的轮次许可：持有期间同会话后续轮次被阻塞，容量占位
pub(crate) struct TurnPermit {
    _capacity: tokio::sync::OwnedSemaphorePermit,
    _session: OwnedMutexGuard<()>,
}

impl std::fmt::Debug for TurnPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TurnPermit")
    }
}

/// 为每个规范会话键提供独立的串行锁，并限制总排队/执行容量
pub(crate) struct TurnCoordinator {
    locks: AsyncMutex<HashMap<String, Weak<AsyncMutex<()>>>>,
    capacity: Arc<tokio::sync::Semaphore>,
}

impl TurnCoordinator {
    /// total_capacity = max_concurrent_requests + max_queued_requests
    pub(crate) fn new(total_capacity: usize) -> Self {
        Self {
            locks: AsyncMutex::new(HashMap::new()),
            capacity: Arc::new(tokio::sync::Semaphore::new(total_capacity)),
        }
    }

    /// 预留当前会话的轮次：容量满立即返回 busy；同会话严格按预留顺序排队。
    pub(crate) async fn try_reserve(
        &self,
        source: &SessionSource,
    ) -> Result<TurnPermit, TurnQueueFull> {
        let capacity = self
            .capacity
            .clone()
            .try_acquire_owned()
            .map_err(|_| TurnQueueFull)?;

        let session_lock = {
            let mut locks = self.locks.lock().await;
            locks.retain(|_, lock| lock.strong_count() > 0);

            let key = source.canonical_key();
            if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(AsyncMutex::new(()));
                locks.insert(key, Arc::downgrade(&lock));
                lock
            }
        };

        let session = session_lock.lock_owned().await;
        Ok(TurnPermit {
            _capacity: capacity,
            _session: session,
        })
    }
}

/// 单轮对话
#[derive(Debug, Clone)]
pub struct ContextTurn {
    pub user: String,
    pub assistant: String,
}

/// 上下文窗口管理器
pub struct ContextWindow {
    state: Mutex<ContextState>,
    /// 最大对话轮数
    max_turns: usize,
    /// 最大会话数
    max_sessions: usize,
}

#[derive(Default)]
struct ContextState {
    histories: HashMap<String, VecDeque<ContextTurn>>,
    session_order: VecDeque<String>,
}

impl ContextWindow {
    pub fn new(max_turns: usize, max_sessions: usize) -> Self {
        Self {
            state: Mutex::new(ContextState::default()),
            max_turns,
            max_sessions,
        }
    }

    /// 是否启用上下文
    pub fn is_enabled(&self) -> bool {
        self.max_turns > 0
    }

    /// 保存一轮对话
    pub fn push(&self, source: &SessionSource, turn: ContextTurn) {
        if self.max_turns == 0 {
            return;
        }

        let session_id = source.to_string();
        let mut state = self.state.lock().expect("context window lock poisoned");

        if self.max_sessions > 0 && !state.histories.contains_key(&session_id) {
            while state.histories.len() >= self.max_sessions {
                let old_id = state
                    .session_order
                    .pop_front()
                    .expect("context session order is inconsistent");
                state
                    .histories
                    .remove(&old_id)
                    .expect("context session history is inconsistent");
            }
            state.session_order.push_back(session_id.clone());
        }

        let entry = state.histories.entry(session_id).or_default();
        entry.push_back(turn);

        while entry.len() > self.max_turns {
            entry.pop_front();
        }
    }

    /// 获取会话历史
    pub fn get(&self, source: &SessionSource) -> Vec<ContextTurn> {
        let session_id = source.to_string();
        self.state
            .lock()
            .expect("context window lock poisoned")
            .histories
            .get(&session_id)
            .map(|v| v.iter().cloned().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn turn(value: usize) -> ContextTurn {
        ContextTurn {
            user: format!("user-{value}"),
            assistant: format!("assistant-{value}"),
        }
    }

    #[test]
    fn keeps_only_the_configured_turn_count() {
        let context = ContextWindow::new(2, 10);
        let source = SessionSource::TeamSpeak {
            uid: "uid-1".to_string(),
        };

        context.push(&source, turn(1));
        context.push(&source, turn(2));
        context.push(&source, turn(3));

        let history = context.get(&source);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].user, "user-2");
        assert_eq!(history[1].user, "user-3");
    }

    #[test]
    fn canonical_keys_separate_adapter_namespaces() {
        let teamspeak = SessionSource::TeamSpeak {
            uid: "42".to_string(),
        };
        let headless = SessionSource::Headless {
            uid: "42".to_string(),
        };
        let private = SessionSource::NapCatPrivate { user_id: 42 };
        let group = SessionSource::NapCatGroup { group_id: 42 };

        let keys = [
            teamspeak.canonical_key(),
            headless.canonical_key(),
            private.canonical_key(),
            group.canonical_key(),
        ];
        let unique: std::collections::HashSet<_> = keys.iter().collect();

        assert_eq!(unique.len(), keys.len());
    }

    #[test]
    fn concurrent_writes_respect_the_session_limit() {
        let context = Arc::new(ContextWindow::new(1, 4));
        let mut threads = Vec::new();

        for caller_id in 0..64 {
            let context = context.clone();
            threads.push(std::thread::spawn(move || {
                context.push(
                    &SessionSource::Headless {
                        uid: format!("uid-{caller_id}"),
                    },
                    turn(caller_id as usize),
                );
            }));
        }

        for thread in threads {
            thread.join().unwrap();
        }

        let state = context.state.lock().unwrap();
        assert_eq!(state.histories.len(), 4);
        assert_eq!(state.histories.len(), state.session_order.len());
    }

    #[tokio::test]
    async fn same_session_turns_are_serialized() {
        let coordinator = Arc::new(TurnCoordinator::new(8));
        let source = SessionSource::TeamSpeak {
            uid: "same-user".to_string(),
        };
        let first_guard = coordinator.try_reserve(&source).await.unwrap();

        let waiting_coordinator = coordinator.clone();
        let waiting_source = source.clone();
        let mut waiter =
            tokio::spawn(async move { waiting_coordinator.try_reserve(&waiting_source).await });

        assert!(tokio::time::timeout(Duration::from_millis(20), &mut waiter)
            .await
            .is_err());

        drop(first_guard);
        let second_guard = tokio::time::timeout(Duration::from_millis(200), &mut waiter)
            .await
            .expect("same-session waiter must continue after the first turn")
            .expect("same-session waiter task must succeed");
        drop(second_guard);
    }

    #[tokio::test]
    async fn different_sessions_can_run_concurrently() {
        let coordinator = TurnCoordinator::new(8);
        let first_source = SessionSource::TeamSpeak {
            uid: "first-user".to_string(),
        };
        let second_source = SessionSource::TeamSpeak {
            uid: "second-user".to_string(),
        };
        let _first_guard = coordinator.try_reserve(&first_source).await.unwrap();

        let second_guard = tokio::time::timeout(
            Duration::from_millis(200),
            coordinator.try_reserve(&second_source),
        )
        .await
        .expect("different sessions must not block each other")
        .expect("capacity must be available");
        drop(second_guard);
    }

    #[tokio::test]
    async fn stale_session_locks_are_removed_on_next_acquire() {
        let coordinator = TurnCoordinator::new(8);
        let first_source = SessionSource::Headless {
            uid: "expired".to_string(),
        };
        let second_source = SessionSource::Headless {
            uid: "active".to_string(),
        };

        let first_guard = coordinator.try_reserve(&first_source).await.unwrap();
        assert_eq!(coordinator.locks.lock().await.len(), 1);
        drop(first_guard);

        let _second_guard = coordinator.try_reserve(&second_source).await.unwrap();
        assert_eq!(coordinator.locks.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn full_capacity_returns_queue_full_without_waiting() {
        let coordinator = TurnCoordinator::new(2);
        let source = SessionSource::NapCatPrivate { user_id: 1 };
        let other = SessionSource::NapCatPrivate { user_id: 2 };

        let first = coordinator.try_reserve(&source).await.unwrap();
        let second = coordinator.try_reserve(&other).await.unwrap();

        assert_eq!(
            coordinator.try_reserve(&source).await.unwrap_err(),
            TurnQueueFull
        );
        drop(first);
        drop(second);
    }

    #[tokio::test]
    async fn cancelled_waiter_releases_capacity_for_successors() {
        let coordinator = Arc::new(TurnCoordinator::new(1));
        let source = SessionSource::NapCatPrivate { user_id: 1 };
        let holder = coordinator.try_reserve(&source).await.unwrap();

        let waiting = tokio::spawn({
            let coordinator = coordinator.clone();
            let source = source.clone();
            async move { coordinator.try_reserve(&source).await }
        });
        waiting.abort();
        drop(holder);

        let successor =
            tokio::time::timeout(Duration::from_millis(200), coordinator.try_reserve(&source))
                .await
                .expect("cancelled waiter must not block successors")
                .expect("capacity must be released");
        drop(successor);
    }
}
