use dashmap::DashMap;
use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

/// 会话来源
#[derive(Debug, Clone)]
pub enum SessionSource {
    /// TeamSpeak ServerQuery
    TeamSpeak { clid: u32 },
    /// NapCat 私聊
    NapCatPrivate { user_id: i64 },
    /// NapCat 群聊
    NapCatGroup { group_id: i64 },
    /// Headless 模式
    Headless { caller_id: u32 },
}

impl fmt::Display for SessionSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionSource::TeamSpeak { clid } => write!(f, "sq:{}", clid),
            SessionSource::NapCatPrivate { user_id } => write!(f, "nc:private:{}", user_id),
            SessionSource::NapCatGroup { group_id } => write!(f, "nc:group:{}", group_id),
            SessionSource::Headless { caller_id } => write!(f, "headless:{}", caller_id),
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
    /// 会话历史，key = session_id
    histories: Arc<DashMap<String, VecDeque<ContextTurn>>>,
    /// 会话创建顺序（用于淘汰最旧会话）
    session_order: Arc<std::sync::Mutex<VecDeque<String>>>,
    /// 最大对话轮数
    max_turns: usize,
    /// 最大会话数
    max_sessions: usize,
}

impl ContextWindow {
    pub fn new(max_turns: usize, max_sessions: usize) -> Self {
        Self {
            histories: Arc::new(DashMap::new()),
            session_order: Arc::new(std::sync::Mutex::new(VecDeque::new())),
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

        // 检查是否需要淘汰旧会话
        if self.max_sessions > 0 {
            let mut order = self.session_order.lock().unwrap();
            if !order.contains(&session_id) {
                // 新会话，检查是否超过限制
                while order.len() >= self.max_sessions {
                    if let Some(old_id) = order.pop_front() {
                        self.histories.remove(&old_id);
                    }
                }
                order.push_back(session_id.clone());
            }
        }

        let mut entry = self.histories.entry(session_id).or_default();
        entry.push_back(turn);

        // 裁剪超出限制的旧对话
        while entry.len() > self.max_turns {
            entry.pop_front();
        }
    }

    /// 获取会话历史
    pub fn get(&self, source: &SessionSource) -> Vec<ContextTurn> {
        let session_id = source.to_string();
        self.histories
            .get(&session_id)
            .map(|v| v.iter().cloned().collect())
            .unwrap_or_default()
    }
}
