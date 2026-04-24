use dashmap::DashMap;
use std::collections::VecDeque;
use std::sync::Arc;

/// 单轮对话
#[derive(Debug, Clone)]
pub struct ContextTurn {
    pub user: String,
    pub assistant: String,
}

/// 上下文窗口管理器
pub struct ContextWindow {
    /// 会话历史，key = session_id
    histories: Arc<DashMap<String, VecDeque<ContextTurn>>>,
    /// 最大对话轮数
    max_turns: usize,
}

impl ContextWindow {
    pub fn new(max_turns: usize) -> Self {
        Self {
            histories: Arc::new(DashMap::new()),
            max_turns,
        }
    }

    /// 是否启用上下文
    pub fn is_enabled(&self) -> bool {
        self.max_turns > 0
    }

    /// 保存一轮对话
    pub fn push(&self, session_id: &str, turn: ContextTurn) {
        if self.max_turns == 0 {
            return;
        }

        let mut entry = self.histories.entry(session_id.to_string()).or_default();
        entry.push_back(turn);

        // 裁剪超出限制的旧对话
        while entry.len() > self.max_turns {
            entry.pop_front();
        }
    }

    /// 获取会话历史
    pub fn get(&self, session_id: &str) -> Vec<ContextTurn> {
        self.histories
            .get(session_id)
            .map(|v| v.iter().cloned().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_and_get() {
        let ctx = ContextWindow::new(3);

        ctx.push(
            "test",
            ContextTurn {
                user: "hello".to_string(),
                assistant: "hi".to_string(),
            },
        );

        let turns = ctx.get("test");
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].user, "hello");
        assert_eq!(turns[0].assistant, "hi");
    }

    #[test]
    fn test_trim() {
        let ctx = ContextWindow::new(2);

        ctx.push(
            "test",
            ContextTurn {
                user: "1".to_string(),
                assistant: "a".to_string(),
            },
        );
        ctx.push(
            "test",
            ContextTurn {
                user: "2".to_string(),
                assistant: "b".to_string(),
            },
        );
        ctx.push(
            "test",
            ContextTurn {
                user: "3".to_string(),
                assistant: "c".to_string(),
            },
        );

        let turns = ctx.get("test");
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].user, "2");
        assert_eq!(turns[1].user, "3");
    }

    #[test]
    fn test_disabled() {
        let ctx = ContextWindow::new(0);

        ctx.push(
            "test",
            ContextTurn {
                user: "hello".to_string(),
                assistant: "hi".to_string(),
            },
        );

        let turns = ctx.get("test");
        assert_eq!(turns.len(), 0);
    }
}
