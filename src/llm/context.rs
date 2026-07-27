use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::Mutex;

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

impl fmt::Display for SessionSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionSource::TeamSpeak { uid } => write!(f, "sq:{}", uid),
            SessionSource::NapCatPrivate { user_id } => write!(f, "nc:private:{}", user_id),
            SessionSource::NapCatGroup { group_id } => write!(f, "nc:group:{}", group_id),
            SessionSource::Headless { uid } => write!(f, "headless:{}", uid),
        }
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
    use std::sync::Arc;

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
}
