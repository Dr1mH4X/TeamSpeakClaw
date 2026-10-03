use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

/// 会话来源
#[derive(Debug, Clone)]
pub enum SessionSource {
    /// TeamSpeak 客户端（文本路由与语音桥共用同一 uid 空间）
    TeamSpeak { uid: String },
    /// NapCat 私聊
    NapCatPrivate { user_id: i64 },
    /// NapCat 群聊
    NapCatGroup { group_id: i64 },
}

impl SessionSource {
    /// 返回跨适配器唯一且稳定的会话键。
    pub(crate) fn canonical_key(&self) -> String {
        match self {
            SessionSource::TeamSpeak { uid } => format!("sq:{uid}"),
            SessionSource::NapCatPrivate { user_id } => format!("nc:private:{user_id}"),
            SessionSource::NapCatGroup { group_id } => format!("nc:group:{group_id}"),
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

/// 容量占位许可：事件循环内同步获取，不等待
pub(crate) struct TurnCapacityPermit {
    _capacity: tokio::sync::OwnedSemaphorePermit,
}

impl std::fmt::Debug for TurnCapacityPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TurnCapacityPermit")
    }
}

/// 会话串行锁 guard：在任务内异步获取，持有到回合结束
pub(crate) struct TurnSessionGuard {
    _session: OwnedMutexGuard<()>,
}

impl std::fmt::Debug for TurnSessionGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TurnSessionGuard")
    }
}

/// 为每个规范会话键提供独立的串行锁，并限制总排队/执行容量
pub(crate) struct TurnCoordinator {
    locks: AsyncMutex<HashMap<String, Weak<AsyncMutex<()>>>>,
    capacity: Arc<tokio::sync::Semaphore>,
}

impl TurnCoordinator {
    /// 创建协调器。`total_capacity` 是同时可持有的许可上限（含执行中与等待会话锁的轮次）。
    pub(crate) fn new(total_capacity: usize) -> Self {
        Self {
            locks: AsyncMutex::new(HashMap::new()),
            capacity: Arc::new(tokio::sync::Semaphore::new(total_capacity)),
        }
    }

    /// 同步获取容量占位：满立即返回 busy，不等待。
    /// 供事件循环调用，保证循环不被轮次阻塞。
    pub(crate) fn try_acquire_capacity(&self) -> Result<TurnCapacityPermit, TurnQueueFull> {
        let capacity = self
            .capacity
            .clone()
            .try_acquire_owned()
            .map_err(|_| TurnQueueFull)?;
        Ok(TurnCapacityPermit {
            _capacity: capacity,
        })
    }

    /// 异步获取会话串行锁：同会话严格 FIFO，不同会话互不阻塞。
    /// 在任务内调用，等待期间事件循环照常运转。
    pub(crate) async fn acquire_session(&self, source: &SessionSource) -> TurnSessionGuard {
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

        TurnSessionGuard {
            _session: session_lock.lock_owned().await,
        }
    }
}

/// 单会话保留的 omni 音频历史字节上限（8 MiB）；超出时从最早的轮整轮丢弃。
/// 这是存储层预算，与装配层的 `MAX_WIRE_AUDIO_BYTES` 分工：这里限制常驻内存，
/// 那里限制单次请求体。
const MAX_AUDIO_HISTORY_BYTES: usize = 8 * 1024 * 1024;

/// 所有会话合计保留的音频历史字节上限（256 MiB）。单会话逐出后若仍超限，
/// 按 `session_order` 从最早会话丢音频轮，最坏常驻因此是该值，
/// 而不是 `MAX_AUDIO_HISTORY_BYTES × MAX_CONTEXT_SESSIONS`（8 MiB × 1000）。
const MAX_AUDIO_HISTORY_BYTES_TOTAL: usize = 256 * 1024 * 1024;

/// 装配请求时历史音频允许占用的 wire 体积上限（2 MiB）。
///
/// 依据单条 utterance 的实测上限：16 kHz 单声道 16 bit 是 32,000 B/s，语音满
/// `MAX_CHUNK_MS = 12000` 或静音满 `VAD_SILENCE_MS = 1200` 时冲刷，故单条最多
/// 12 s 语音加 1.2 s 静音尾 = 422,444 B WAV（含 44 B 头），base64 后 563,260 B，
/// 加固定 JSON 外壳 98 B，一条历史音频轮约 550 KiB。2 MiB 容得下 3 条
/// （2 个历史轮加当前轮，1.61 MiB），并在 4 MiB 级网关上留余量；网关限 1 MiB 时
/// 需调到 1 MiB 以下，此时装配只剩当前轮。
///
/// 与 `max_context_turns` 的分工：轮数上限在存储层（`push`），体积上限在装配层
/// （`history_for_wire`），装配时两者取更严的那个。
pub const MAX_WIRE_AUDIO_BYTES: usize = 2 * 1024 * 1024;

/// 一条音频轮除 base64 载荷外的固定 wire 开销：`data:audio/wav;base64,` 前缀（22 B）
/// 与 `{"role":"user","content":[{"type":"input_audio","input_audio":{"data":"…"}}]}`
/// JSON 外壳。该字面量与 `engine::audio_content` 的构造逐字对应，改 wire 格式时必须
/// 同步；`engine` 的单测 `wire_estimate_matches_the_serialized_audio_part` 会在漂移时失败。
const AUDIO_WIRE_ENVELOPE_BYTES: usize =
    "{\"role\":\"user\",\"content\":[{\"type\":\"input_audio\",\"input_audio\":{\"data\":\"data:audio/wav;base64,\"}}]}".len();

/// 音频轮的 WAV 字节与惰性 base64 编码缓存。
///
/// `wav` 用 `Arc<[u8]>`：`ContextTurn::clone` 因此是浅拷贝，装配不再整份复制历史。
/// `encoded` 缓存首次编码结果，同一段音频被后续轮次回放或轮次被克隆时共享同一份
/// base64，不再重复编码。
///
/// 并发安全：`OnceLock::get_or_init` 保证只写入一次，并发进入时只有一个初始化值对外
/// 可见，其余拿到同一个 `Arc<str>`；存储侧对同一会话的写入由 `ContextWindow::state`
/// 的 `Mutex` 串行化，不存在对同一 `AudioBytes` 的并发写。全程只用 `Arc`/`OnceLock`，
/// 没有 `unsafe`。
#[derive(Debug, Clone)]
pub struct AudioBytes {
    wav: Arc<[u8]>,
    encoded: OnceLock<Arc<str>>,
}

impl AudioBytes {
    pub fn new(wav: Vec<u8>) -> Self {
        Self {
            wav: Arc::from(wav),
            encoded: OnceLock::new(),
        }
    }

    /// WAV 原始字节数；存储层预算按它计
    pub(crate) fn len(&self) -> usize {
        self.wav.len()
    }

    /// base64 载荷，首次调用编码并缓存；命中缓存只克隆 `Arc` 指针
    pub(crate) fn base64(&self) -> Arc<str> {
        self.encoded
            .get_or_init(|| {
                let mut encoded = String::with_capacity(self.wav.len().div_ceil(3) * 4);
                BASE64.encode_string(&*self.wav, &mut encoded);
                Arc::from(encoded)
            })
            .clone()
    }

    /// 该轮在装配后请求体里的精确字节数（base64 长度加固定 JSON 外壳）
    pub(crate) fn wire_bytes(&self) -> usize {
        self.wav.len().div_ceil(3) * 4 + AUDIO_WIRE_ENVELOPE_BYTES
    }
}

/// 上下文里的用户侧内容
#[derive(Debug, Clone)]
pub enum ContextUser {
    /// 文本轮原文
    Text(String),
    /// omni 音频轮：16 kHz 单声道 WAV 字节与编码缓存，装配请求时转成 `input_audio` content
    Audio(AudioBytes),
}

impl ContextUser {
    /// 文本轮原文；音频轮返回 None。仅测试断言用
    #[cfg(test)]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContextUser::Text(text) => Some(text),
            ContextUser::Audio(_) => None,
        }
    }

    /// 存储层预算计 WAV 原始字节
    fn audio_bytes(&self) -> usize {
        match self {
            ContextUser::Text(_) => 0,
            ContextUser::Audio(audio) => audio.len(),
        }
    }

    /// 装配层预算计 base64 加 JSON 后的 wire 体积；文本轮不占音频预算
    fn audio_wire_bytes(&self) -> usize {
        match self {
            ContextUser::Text(_) => 0,
            ContextUser::Audio(audio) => audio.wire_bytes(),
        }
    }
}

/// 单轮对话
#[derive(Debug, Clone)]
pub struct ContextTurn {
    pub user: ContextUser,
    pub assistant: String,
}

/// 上下文窗口管理器
pub struct ContextWindow {
    state: Mutex<ContextState>,
    /// 最大对话轮数
    max_turns: usize,
    /// 最大会话数
    max_sessions: usize,
    /// 单会话保留的音频历史字节上限
    audio_budget: usize,
    /// 所有会话合计保留的音频历史字节上限
    audio_budget_total: usize,
}

#[derive(Default)]
struct ContextState {
    histories: HashMap<String, VecDeque<ContextTurn>>,
    session_order: VecDeque<String>,
    /// 所有会话的音频 WAV 字节合计；与 `histories` 同步增减，供全局预算使用
    audio_bytes_total: usize,
}

impl ContextWindow {
    pub fn new(max_turns: usize, max_sessions: usize) -> Self {
        Self {
            state: Mutex::new(ContextState::default()),
            max_turns,
            max_sessions,
            audio_budget: MAX_AUDIO_HISTORY_BYTES,
            audio_budget_total: MAX_AUDIO_HISTORY_BYTES_TOTAL,
        }
    }

    /// 测试可见：注入单会话音频字节预算，走 `push` 验证整条丢弃路径
    #[cfg(test)]
    fn with_audio_budget(max_turns: usize, max_sessions: usize, audio_budget: usize) -> Self {
        Self::with_audio_budgets(
            max_turns,
            max_sessions,
            audio_budget,
            MAX_AUDIO_HISTORY_BYTES_TOTAL,
        )
    }

    /// 测试可见：同时注入单会话与全局音频字节预算
    #[cfg(test)]
    fn with_audio_budgets(
        max_turns: usize,
        max_sessions: usize,
        audio_budget: usize,
        audio_budget_total: usize,
    ) -> Self {
        Self {
            state: Mutex::new(ContextState::default()),
            max_turns,
            max_sessions,
            audio_budget,
            audio_budget_total,
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
        let ContextState {
            histories,
            session_order,
            audio_bytes_total,
        } = &mut *state;

        if self.max_sessions > 0 && !histories.contains_key(&session_id) {
            while histories.len() >= self.max_sessions {
                let old_id = session_order
                    .pop_front()
                    .expect("context session order is inconsistent");
                let removed = histories
                    .remove(&old_id)
                    .expect("context session history is inconsistent");
                *audio_bytes_total -= removed
                    .iter()
                    .map(|turn| turn.user.audio_bytes())
                    .sum::<usize>();
            }
            session_order.push_back(session_id.clone());
        }

        let entry = histories.entry(session_id).or_default();
        *audio_bytes_total += turn.user.audio_bytes();
        entry.push_back(turn);

        // 两种上限都以「一轮对话」为单位从最早逐轮丢弃：先按 max_context_turns，再按单会话音频字节预算兜底
        while entry.len() > self.max_turns {
            let dropped = entry
                .pop_front()
                .expect("context entry is longer than the turn limit");
            *audio_bytes_total -= dropped.user.audio_bytes();
        }
        *audio_bytes_total -= drop_excess_audio(entry, self.audio_budget);

        // 单会话逐出后仍超全局预算：按 session_order 从最早会话丢音频轮
        drop_global_excess_audio(
            histories,
            session_order,
            audio_bytes_total,
            self.audio_budget_total,
        );
    }

    /// 获取会话的完整存储历史；仅测试断言用，装配走 `history_for_wire`
    #[cfg(test)]
    pub(crate) fn get(&self, source: &SessionSource) -> Vec<ContextTurn> {
        let session_id = source.to_string();
        self.state
            .lock()
            .expect("context window lock poisoned")
            .histories
            .get(&session_id)
            .map(|v| v.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// 供装配用的历史视图：按 `wire_budget` 从最早的音频轮逐轮裁掉，直到估算的
    /// base64 加 JSON 体积放得下。文本轮不参与该裁剪（不占预算），最新一轮音频永不丢。
    ///
    /// 裁剪在锁内完成，只 clone 保留的那一段；`ContextTurn` 的音频是 `Arc`，
    /// clone 因此是浅拷贝。存储历史不因此改变，模型实际收到的 messages 才反映裁剪结果。
    pub fn history_for_wire(&self, source: &SessionSource, wire_budget: usize) -> Vec<ContextTurn> {
        let session_id = source.to_string();
        let state = self.state.lock().expect("context window lock poisoned");
        let Some(entry) = state.histories.get(&session_id) else {
            return Vec::new();
        };

        // 从最新往回累计音频轮的 wire 体积，keep_from 落在保留窗口的起点
        let mut total = 0usize;
        let mut keep_from = entry.len();
        let mut kept_latest_audio = false;
        for index in (0..entry.len()).rev() {
            let bytes = entry[index].user.audio_wire_bytes();
            if bytes == 0 {
                keep_from = index;
                continue;
            }
            if kept_latest_audio && total + bytes > wire_budget {
                break;
            }
            total += bytes;
            kept_latest_audio = true;
            keep_from = index;
        }

        entry.range(keep_from..).cloned().collect()
    }

    /// 体积类失败自愈：丢掉该会话最早的音频轮，返回是否真的丢了一轮。
    /// 只丢音频轮，文本轮保留；最新一轮音频永不丢。下一个体积类失败会继续往前丢。
    pub fn drop_oldest_audio_turn(&self, source: &SessionSource) -> bool {
        let session_id = source.to_string();
        let mut state = self.state.lock().expect("context window lock poisoned");
        let dropped = {
            let Some(entry) = state.histories.get_mut(&session_id) else {
                return false;
            };
            drop_earliest_audio_turn(entry)
        };
        match dropped {
            Some(bytes) => {
                state.audio_bytes_total -= bytes;
                true
            }
            None => false,
        }
    }
}

/// 单会话音频历史超预算时从最早的一轮对话逐轮丢弃：user/assistant 成对移除，历史保持连续；
/// 至少保留最新一轮，预算不会把会话历史清空。返回被丢弃的 WAV 字节数。
fn drop_excess_audio(entry: &mut VecDeque<ContextTurn>, budget: usize) -> usize {
    let mut total: usize = entry.iter().map(|turn| turn.user.audio_bytes()).sum();
    let mut dropped = 0usize;
    while total > budget && entry.len() > 1 {
        let Some(front) = entry.front() else {
            break;
        };
        let bytes = front.user.audio_bytes();
        total -= bytes;
        dropped += bytes;
        entry.pop_front();
    }
    dropped
}

/// 丢掉最早的音频轮（连同该轮的 assistant 回复），保留文本轮与最新一轮音频；
/// 返回被丢弃的 WAV 字节数，没有可丢的音频轮时返回 None。
fn drop_earliest_audio_turn(entry: &mut VecDeque<ContextTurn>) -> Option<usize> {
    let mut audio_indices = entry
        .iter()
        .enumerate()
        .filter(|(_, turn)| matches!(&turn.user, ContextUser::Audio(_)))
        .map(|(index, _)| index);
    let index = audio_indices.next()?;
    // 只剩最新一轮音频：任何预算都不再清空它
    audio_indices.next()?;
    let bytes = entry[index].user.audio_bytes();
    entry.remove(index);
    Some(bytes)
}

/// 全局音频预算超限时按 `session_order` 从最早会话逐会话丢音频轮，直到回到预算内；
/// 每个会话至少保留最新一轮音频，文本轮不丢。
fn drop_global_excess_audio(
    histories: &mut HashMap<String, VecDeque<ContextTurn>>,
    session_order: &VecDeque<String>,
    audio_bytes_total: &mut usize,
    budget: usize,
) {
    let mut order_index = 0;
    while *audio_bytes_total > budget && order_index < session_order.len() {
        if let Some(entry) = histories.get_mut(&session_order[order_index]) {
            while *audio_bytes_total > budget {
                match drop_earliest_audio_turn(entry) {
                    Some(dropped) => *audio_bytes_total -= dropped,
                    None => break,
                }
            }
        }
        order_index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn turn(value: usize) -> ContextTurn {
        ContextTurn {
            user: ContextUser::Text(format!("user-{value}")),
            assistant: format!("assistant-{value}"),
        }
    }

    fn audio_turn(bytes: usize) -> ContextTurn {
        ContextTurn {
            user: ContextUser::Audio(AudioBytes::new(vec![0u8; bytes])),
            assistant: "assistant-audio".to_string(),
        }
    }

    fn audio_bytes(turn: &ContextTurn) -> Option<usize> {
        match &turn.user {
            ContextUser::Text(_) => None,
            ContextUser::Audio(audio) => Some(audio.len()),
        }
    }

    /// 装配层估算的一条音频轮 wire 体积，供预算断言使用
    fn audio_wire(bytes: usize) -> usize {
        AudioBytes::new(vec![0u8; bytes]).wire_bytes()
    }

    /// 存储层音频字节合计，用来校验 `push`/逐出的增量记账
    fn stored_audio_bytes(context: &ContextWindow) -> usize {
        context
            .state
            .lock()
            .unwrap()
            .histories
            .values()
            .flatten()
            .map(|turn| turn.user.audio_bytes())
            .sum()
    }

    /// 测试辅助：两段式预留（容量 + 会话锁）
    async fn reserve(
        coordinator: &TurnCoordinator,
        source: &SessionSource,
    ) -> (TurnCapacityPermit, TurnSessionGuard) {
        let capacity = coordinator.try_acquire_capacity().unwrap();
        let session = coordinator.acquire_session(source).await;
        (capacity, session)
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
        assert_eq!(history[0].user.as_text(), Some("user-2"));
        assert_eq!(history[1].user.as_text(), Some("user-3"));
    }

    #[test]
    fn push_applies_the_injected_audio_byte_budget() {
        // 预算 5 KiB：先验证预算内（含恰好用满）整轮保留
        let context = ContextWindow::with_audio_budget(8, 10, 5 * 1024);
        let source = SessionSource::TeamSpeak {
            uid: "uid-audio-budget".to_string(),
        };

        context.push(&source, audio_turn(1024));
        context.push(&source, audio_turn(2 * 1024));
        context.push(&source, audio_turn(2 * 1024));

        let history = context.get(&source);
        assert_eq!(history.len(), 3);
        assert_eq!(audio_bytes(&history[0]), Some(1024));
        assert_eq!(audio_bytes(&history[1]), Some(2 * 1024));
        assert_eq!(audio_bytes(&history[2]), Some(2 * 1024));

        // 再加一轮即超预算：从最早的整轮丢弃，直到回到预算内
        context.push(&source, audio_turn(2 * 1024));

        let history = context.get(&source);
        assert_eq!(history.len(), 2);
        assert_eq!(audio_bytes(&history[0]), Some(2 * 1024));
        assert_eq!(audio_bytes(&history[1]), Some(2 * 1024));

        // 单轮自身超预算：仍保留最新一轮，不清空会话历史
        context.push(&source, audio_turn(9 * 1024));

        let history = context.get(&source);
        assert_eq!(history.len(), 1);
        assert_eq!(audio_bytes(&history[0]), Some(9 * 1024));
    }

    #[test]
    fn excess_audio_drops_whole_earliest_turns() {
        let mut entry = VecDeque::from(vec![audio_turn(8 * 1024), turn(1), audio_turn(8 * 1024)]);

        drop_excess_audio(&mut entry, 10 * 1024);

        assert_eq!(entry.len(), 2);
        assert_eq!(entry[0].user.as_text(), Some("user-1"));
        assert_eq!(audio_bytes(&entry[1]), Some(8 * 1024));
    }

    #[test]
    fn single_turn_over_budget_still_keeps_the_latest_turn() {
        let mut entry = VecDeque::from(vec![audio_turn(8 * 1024), audio_turn(4 * 1024)]);

        drop_excess_audio(&mut entry, 2 * 1024);

        assert_eq!(entry.len(), 1);
        assert_eq!(audio_bytes(&entry[0]), Some(4 * 1024));
    }

    #[test]
    fn global_audio_budget_evicts_the_oldest_session_first() {
        // 单会话预算宽松，全局预算只容得下 3 KiB
        let context = ContextWindow::with_audio_budgets(8, 10, 64 * 1024, 3 * 1024);
        let oldest = SessionSource::TeamSpeak {
            uid: "oldest-session".to_string(),
        };
        let newer = SessionSource::TeamSpeak {
            uid: "newer-session".to_string(),
        };

        context.push(&oldest, audio_turn(1024));
        context.push(&oldest, audio_turn(1024));
        context.push(&newer, audio_turn(1024));
        assert_eq!(context.get(&oldest).len(), 2);
        assert_eq!(context.get(&newer).len(), 1);

        // 第 4 KiB 使全局超限：从最早的会话丢音频轮，不动较新的会话
        context.push(&newer, audio_turn(1024));

        assert_eq!(context.get(&oldest).len(), 1);
        assert_eq!(context.get(&newer).len(), 2);
        assert_eq!(audio_bytes(&context.get(&oldest)[0]), Some(1024));
        assert_eq!(
            stored_audio_bytes(&context),
            context.state.lock().unwrap().audio_bytes_total
        );
    }

    #[test]
    fn global_audio_budget_never_clears_the_latest_audio_turn() {
        // 全局预算 1 B：每次都必须逐出，但每个会话的最新一轮音频不受影响
        let context = ContextWindow::with_audio_budgets(8, 10, 1024 * 1024, 1);
        let source = SessionSource::TeamSpeak {
            uid: "global-floor".to_string(),
        };

        context.push(&source, audio_turn(1024));
        context.push(&source, audio_turn(1024));

        let history = context.get(&source);
        assert_eq!(history.len(), 1);
        assert_eq!(audio_bytes(&history[0]), Some(1024));
    }

    #[test]
    fn wire_budget_drops_earliest_audio_turns_until_the_body_fits() {
        let context = ContextWindow::new(8, 10);
        let source = SessionSource::TeamSpeak {
            uid: "wire-drop".to_string(),
        };
        context.push(&source, audio_turn(3000));
        context.push(&source, audio_turn(3000));
        context.push(&source, audio_turn(3000));

        let history = context.history_for_wire(&source, 2 * audio_wire(3000));

        assert_eq!(history.len(), 2);
        assert_eq!(audio_bytes(&history[0]), Some(3000));
        assert_eq!(audio_bytes(&history[1]), Some(3000));
        // 视图裁剪不改变存储历史
        assert_eq!(context.get(&source).len(), 3);
    }

    #[test]
    fn wire_budget_keeps_text_turns_and_the_latest_audio_turn() {
        let context = ContextWindow::new(8, 10);
        let source = SessionSource::TeamSpeak {
            uid: "wire-text".to_string(),
        };
        context.push(&source, audio_turn(3000));
        context.push(&source, turn(1));
        context.push(&source, audio_turn(3000));
        context.push(&source, turn(2));

        let history = context.history_for_wire(&source, audio_wire(3000));

        // 只容得下一条音频：最早的音频轮被丢，其后的文本轮与最新音频轮都保留
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].user.as_text(), Some("user-1"));
        assert_eq!(audio_bytes(&history[1]), Some(3000));
        assert_eq!(history[2].user.as_text(), Some("user-2"));
    }

    #[test]
    fn wire_budget_always_keeps_the_latest_audio_turn() {
        let context = ContextWindow::new(8, 10);
        let source = SessionSource::TeamSpeak {
            uid: "wire-floor".to_string(),
        };
        context.push(&source, audio_turn(3000));
        context.push(&source, audio_turn(3000));

        let history = context.history_for_wire(&source, 1);

        assert_eq!(history.len(), 1);
        assert_eq!(audio_bytes(&history[0]), Some(3000));
    }

    #[test]
    fn drop_oldest_audio_turn_keeps_text_and_the_latest_audio_turn() {
        let context = ContextWindow::new(8, 10);
        let source = SessionSource::TeamSpeak {
            uid: "heal-once".to_string(),
        };
        context.push(&source, audio_turn(1000));
        context.push(&source, turn(1));
        context.push(&source, audio_turn(1000));

        assert!(context.drop_oldest_audio_turn(&source));

        let history = context.get(&source);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].user.as_text(), Some("user-1"));
        assert_eq!(audio_bytes(&history[1]), Some(1000));
        assert_eq!(
            stored_audio_bytes(&context),
            context.state.lock().unwrap().audio_bytes_total
        );

        // 只剩最新一轮音频：拒绝再丢
        assert!(!context.drop_oldest_audio_turn(&source));
        assert_eq!(context.get(&source).len(), 2);

        // 未知会话没有可丢的音频轮
        assert!(!context.drop_oldest_audio_turn(&SessionSource::TeamSpeak {
            uid: "missing".to_string(),
        }));
    }

    #[test]
    fn audio_turn_encodes_base64_once() {
        let turn = audio_turn(64);
        let ContextUser::Audio(audio) = &turn.user else {
            panic!("audio_turn must build an audio turn");
        };

        let first = audio.base64();
        let second = audio.base64();

        // 命中缓存时返回同一份分配：没有第二次编码
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn cached_encoding_matches_a_fresh_encode() {
        let turn = audio_turn(64);
        let ContextUser::Audio(audio) = &turn.user else {
            panic!("audio_turn must build an audio turn");
        };

        assert_eq!(audio.base64().as_ref(), BASE64.encode(vec![0u8; 64]));
    }

    #[test]
    fn cloned_audio_turns_share_the_wav_allocation() {
        let turn = audio_turn(64);
        let ContextUser::Audio(original) = &turn.user else {
            panic!("audio_turn must build an audio turn");
        };
        let encoded = original.base64();
        let cloned = turn.clone();
        let ContextUser::Audio(copy) = &cloned.user else {
            panic!("clone must keep the audio variant");
        };

        assert!(Arc::ptr_eq(&original.wav, &copy.wav));
        // 已缓存的编码随浅拷贝共享，克隆不会触发重新编码
        assert!(Arc::ptr_eq(&encoded, &copy.base64()));
    }

    #[test]
    fn canonical_keys_separate_adapter_namespaces() {
        let teamspeak = SessionSource::TeamSpeak {
            uid: "42".to_string(),
        };
        let private = SessionSource::NapCatPrivate { user_id: 42 };
        let group = SessionSource::NapCatGroup { group_id: 42 };

        let keys = [
            teamspeak.canonical_key(),
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
                    &SessionSource::TeamSpeak {
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
        let first_guard = reserve(&coordinator, &source).await;

        let waiting_coordinator = coordinator.clone();
        let waiting_source = source.clone();
        let mut waiter =
            tokio::spawn(async move { reserve(&waiting_coordinator, &waiting_source).await });

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
        let _first_guard = reserve(&coordinator, &first_source).await;

        let second_guard = tokio::time::timeout(
            Duration::from_millis(200),
            reserve(&coordinator, &second_source),
        )
        .await
        .expect("different sessions must not block each other");
        drop(second_guard);
    }

    #[tokio::test]
    async fn stale_session_locks_are_removed_on_next_acquire() {
        let coordinator = TurnCoordinator::new(8);
        let first_source = SessionSource::TeamSpeak {
            uid: "expired".to_string(),
        };
        let second_source = SessionSource::TeamSpeak {
            uid: "active".to_string(),
        };

        let first_guard = reserve(&coordinator, &first_source).await;
        assert_eq!(coordinator.locks.lock().await.len(), 1);
        drop(first_guard);

        let _second_guard = reserve(&coordinator, &second_source).await;
        assert_eq!(coordinator.locks.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn full_capacity_returns_queue_full_without_waiting() {
        let coordinator = TurnCoordinator::new(2);
        let source = SessionSource::NapCatPrivate { user_id: 1 };
        let other = SessionSource::NapCatPrivate { user_id: 2 };

        let first = reserve(&coordinator, &source).await;
        let second = reserve(&coordinator, &other).await;

        assert_eq!(
            coordinator.try_acquire_capacity().unwrap_err(),
            TurnQueueFull
        );
        drop(first);
        drop(second);
    }

    #[tokio::test]
    async fn cancelled_waiter_releases_capacity_for_successors() {
        let coordinator = Arc::new(TurnCoordinator::new(1));
        let source = SessionSource::NapCatPrivate { user_id: 1 };
        let holder = reserve(&coordinator, &source).await;

        let waiting = tokio::spawn({
            let coordinator = coordinator.clone();
            let source = source.clone();
            async move { reserve(&coordinator, &source).await }
        });
        waiting.abort();
        drop(holder);

        let successor =
            tokio::time::timeout(Duration::from_millis(200), reserve(&coordinator, &source))
                .await
                .expect("cancelled waiter must not block successors");
        drop(successor);
    }
}
