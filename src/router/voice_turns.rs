//! 语音回合准入：唤醒门裁决后的动作表，以及 per-clid 活跃回合注册表。
//!
//! 注册表只持 `Weak<ActiveTurn>`：条目在最后一个持有者释放时自然失效，
//! 无需摘除钩子。回合任务持一份，synth 任务持一份直到本轮音频播放收尾，
//! 所以「机器人正在产出」覆盖 LLM 流式与 TTS 播放尾两段。
//!
//! 每个 clid 存的是一个活回合集合而非单槽：同一 clid 可以同时有多条活回合
//! （文本回合不夺取话语权，语音回合在解析/STT 期间与其后产出期间也可能并存）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

use tokio_util::sync::CancellationToken;

/// 唤醒门裁决后的准入动作
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WakewordAction {
    /// 丢弃本条 utterance
    Drop,
    /// 正常对话
    Talk,
    /// 插话：取消全部正在产出的回合（不分归属），再处理本条
    BargeIn,
}

/// 状态表：门是否放行 × 本条是否命中唤醒词 × 机器人是否已开始产出。
///
/// `open` 是喂门之后的裁决，已含「命中即开门」：`open == false` 时 `detected` 必为 false，
/// 所以表里「关 + 是」那一行进入本函数时是 `open == true`。
/// 唤醒词命中不区分产出回合的归属：出站音频只有一路（`AudioOutput` FIFO）是全局资源，
/// 说话人无法从听觉上分辨当前在播的是谁触发的回复，所以「正在播报时再喊一次唤醒词」
/// 一律夺回话语权；未命中唤醒词的补充语音在产出期间仍然丢弃。
pub(crate) fn decide_wakeword_action(open: bool, detected: bool, busy: bool) -> WakewordAction {
    if !open {
        return WakewordAction::Drop;
    }
    if !detected {
        return if busy {
            WakewordAction::Drop
        } else {
            WakewordAction::Talk
        };
    }
    if busy {
        WakewordAction::BargeIn
    } else {
        WakewordAction::Talk
    }
}

/// 一个活跃语音回合
pub(crate) struct ActiveTurn {
    /// LLM 流取消令牌：工具循环在每个 await 点检查
    pub(crate) cancel: CancellationToken,
    /// TTS 播放取消：置位后音频消费者在当前帧或段边界停止。
    ///
    /// 回合创建即持有，一轮对话里的多条 TTS 流（工具轮与最终回复各占一条会话）
    /// 与本回合的提示音片段共用它——注册成「最后一个会话的句柄」会让先前的标志
    /// 在插话时停止不了后来的流。
    playback_cancel: Arc<AtomicBool>,
    /// 是否已进入 LLM 回合。准入的「忙」只看它，避免解析与 STT 期间挡住同说话人的后续语音
    producing: AtomicBool,
    /// 准入时刻：`producing_turns` 据此升序返回，插话按同一顺序逐条取消。
    /// 出站 FIFO 的真实先后由入队时刻（`open_tts_session*` / `enqueue_clip_job`）决定，
    /// 与准入时刻未必一致，所以插话取消整条产出集合而不是只挑最早准入的一条
    pub(crate) started_at: Instant,
}

impl ActiveTurn {
    pub(crate) fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
            playback_cancel: Arc::new(AtomicBool::new(false)),
            producing: AtomicBool::new(false),
            started_at: Instant::now(),
        }
    }

    /// 本轮 TTS 播放取消位；回合内所有会话与片段共用同一个
    pub(crate) fn playback_cancel(&self) -> Arc<AtomicBool> {
        self.playback_cancel.clone()
    }

    pub(crate) fn mark_producing(&self) {
        self.producing.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_producing(&self) -> bool {
        self.producing.load(Ordering::SeqCst)
    }

    pub(crate) fn age_ms(&self) -> u128 {
        self.started_at.elapsed().as_millis()
    }
}

/// per-clid 活跃回合注册表；键为说话人 clid（同一连接内稳定，无需 gRPC 解析），
/// 值为该 clid 的活回合集合（同一 clid 可同时有多条：文本回合不夺取话语权，
/// 语音回合也可能与新准入的回合并存）
#[derive(Default)]
pub(crate) struct TurnRegistry {
    turns: Mutex<HashMap<u32, Vec<Weak<ActiveTurn>>>>,
}

impl TurnRegistry {
    /// 登记本条 utterance 的回合，返回该 clid 之前仍然存活的回合。
    ///
    /// 返回旧条目而非静默覆盖：旧回合可能还没进入 `mark_producing`（正在解析/STT），
    /// 此时它不在「忙」的视野里，却已持有回合状态；调用方需要顺手取消它，
    /// 否则会出现两条并发回合，其中一条的播放取消句柄已不可达。
    /// 旧条目仍留在集合里，直到调用方按身份摘除或它们的持有者全部释放——
    /// 文本回合不夺取话语权，忽略返回值时旧条目必须留得住，插话才找得到它。
    pub(crate) fn begin_turn(&self, clid: u32, turn: &Arc<ActiveTurn>) -> Vec<Arc<ActiveTurn>> {
        let mut turns = self.turns.lock().expect("turn registry poisoned");
        turns.retain(|_, entries| {
            entries.retain(|entry| entry.strong_count() > 0);
            !entries.is_empty()
        });
        let displaced = turns
            .get(&clid)
            .map(|entries| entries.iter().filter_map(Weak::upgrade).collect())
            .unwrap_or_default();
        turns.entry(clid).or_default().push(Arc::downgrade(turn));
        displaced
    }

    /// 全部已 `mark_producing` 的活回合及其归属 clid，按准入时刻升序。
    ///
    /// 「忙」与插话取消目标必须来自同一次快照：分两次查询会在两次加锁之间漂移，
    /// 出现「判定为忙但取不到取消目标」或反过来漏掉插话。
    ///
    /// 多条回合可以同时进入产出：各自在解析/STT 期间都不算忙，会被一起放行，
    /// 而出站音频只有一路 FIFO。入队时刻（`open_tts_session*` / `enqueue_clip_job`）
    /// 决定谁先出声，与准入顺序无关，所以插话要取消的是整个集合，只挑一条会留下仍在出声的回合。
    /// 升序只用于日志与定序，不代表播放先后。
    pub(crate) fn producing_turns(&self) -> Vec<(u32, Arc<ActiveTurn>)> {
        let mut turns = self.turns.lock().expect("turn registry poisoned");
        let mut producing = Vec::new();
        for (clid, entries) in turns.iter_mut() {
            entries.retain(|entry| entry.strong_count() > 0);
            for turn in entries.iter().filter_map(Weak::upgrade) {
                if turn.is_producing() {
                    producing.push((*clid, turn));
                }
            }
        }
        turns.retain(|_, entries| !entries.is_empty());
        producing.sort_by_key(|(_, turn)| turn.started_at);
        producing
    }

    /// 按身份摘除条目；只删 `Arc::ptr_eq` 命中的那一条，同 clid 的其他条目（含新回合）不动。
    /// 返回是否确实摘除。
    pub(crate) fn remove_turn(&self, turn: &Arc<ActiveTurn>) -> bool {
        let mut turns = self.turns.lock().expect("turn registry poisoned");
        let mut removed = false;
        turns.retain(|_, entries| {
            entries.retain(|entry| match entry.upgrade() {
                Some(live) => {
                    let matched = Arc::ptr_eq(&live, turn);
                    removed |= matched;
                    !matched
                }
                None => false,
            });
            !entries.is_empty()
        });
        removed
    }

    /// 取该 clid 最近登记且仍存活的回合；准入路径不用这个按 clid 的查询，
    /// 它留给单测断言集合内容
    #[cfg(test)]
    pub(crate) fn active(&self, clid: u32) -> Option<Arc<ActiveTurn>> {
        let mut turns = self.turns.lock().expect("turn registry poisoned");
        let entries = turns.get_mut(&clid)?;
        entries.retain(|entry| entry.strong_count() > 0);
        let latest = entries.iter().rev().find_map(Weak::upgrade);
        if entries.is_empty() {
            turns.remove(&clid);
        }
        latest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_gate_drops_utterances_without_a_wakeword() {
        assert_eq!(
            decide_wakeword_action(false, false, false),
            WakewordAction::Drop
        );
        assert_eq!(
            decide_wakeword_action(false, false, true),
            WakewordAction::Drop
        );
    }

    #[test]
    fn wakeword_passes_when_idle() {
        // 命中即开门：无论此前是否在窗口内，空闲时都放行（窗口顺带刷新）
        assert_eq!(
            decide_wakeword_action(true, true, false),
            WakewordAction::Talk
        );
    }

    #[test]
    fn a_wakeword_hit_always_arrives_with_an_open_gate() {
        // 防御性分支：命中即开门，所以 (open=false, detected=true) 不是真实输入
        assert_eq!(
            decide_wakeword_action(false, true, false),
            WakewordAction::Drop
        );
    }

    #[test]
    fn window_utterance_talks_only_while_idle() {
        assert_eq!(
            decide_wakeword_action(true, false, false),
            WakewordAction::Talk
        );
        assert_eq!(
            decide_wakeword_action(true, false, true),
            WakewordAction::Drop
        );
    }

    /// 插话不看归属：产出回合属于另一说话人时同样夺回话语权，
    /// 因为出站音频只有一路，说话人无法分辨当前在播的是谁触发的回复
    #[test]
    fn wakeword_barges_in_regardless_of_who_owns_the_floor() {
        assert_eq!(
            decide_wakeword_action(true, true, true),
            WakewordAction::BargeIn
        );
    }

    #[test]
    fn registry_tracks_only_live_turns() {
        let registry = TurnRegistry::default();
        let turn = Arc::new(ActiveTurn::new());
        assert!(registry.begin_turn(7, &turn).is_empty());
        assert!(registry.active(7).is_some());
        assert!(registry.producing_turns().is_empty());

        turn.mark_producing();
        let producing = registry.producing_turns();
        assert_eq!(producing.len(), 1);
        assert_eq!(producing[0].0, 7);
        assert!(Arc::ptr_eq(&producing[0].1, &turn));

        // 最后一个持有者释放后条目失效，无需摘除
        drop(producing);
        drop(turn);
        assert!(registry.active(7).is_none());
        assert!(registry.producing_turns().is_empty());
    }

    /// 同 clid 可以有多条活回合：`begin_turn` 报告全部存活旧条目，但不把它们挤出集合。
    /// 文本回合的调用方忽略返回值，旧条目必须留得住，插话才找得到它
    #[test]
    fn begin_turn_reports_previous_live_turns_without_evicting_them() {
        let registry = TurnRegistry::default();
        let first = Arc::new(ActiveTurn::new());
        let second = Arc::new(ActiveTurn::new());
        assert!(registry.begin_turn(3, &first).is_empty());

        let displaced = registry.begin_turn(3, &second);
        assert_eq!(displaced.len(), 1);
        assert!(Arc::ptr_eq(&displaced[0], &first));
        let registered = registry.active(3).expect("latest turn is registered");
        assert!(Arc::ptr_eq(&registered, &second));

        // 旧条目仍在集合里：它还没产出，仍在插话可达范围内
        first.mark_producing();
        let producing = registry.producing_turns();
        assert_eq!(producing.len(), 1);
        assert!(Arc::ptr_eq(&producing[0].1, &first));
    }

    /// 跨说话人插话：取消目标带产出方 clid，而不是喊唤醒词的说话人
    #[test]
    fn producing_turns_reports_the_owner_of_the_floor() {
        let registry = TurnRegistry::default();
        let speaker = Arc::new(ActiveTurn::new());
        let other = Arc::new(ActiveTurn::new());
        registry.begin_turn(3, &speaker);
        registry.begin_turn(9, &other);
        other.mark_producing();

        let producing = registry.producing_turns();
        assert_eq!(producing.len(), 1);
        assert_eq!(producing[0].0, 9);
        assert!(Arc::ptr_eq(&producing[0].1, &other));
        // 无产出的回合不算忙
        assert!(!speaker.is_producing());
    }

    /// 摘除按身份匹配：只删命中那一条，同 clid 的新条目与其他条目都不动
    #[test]
    fn remove_turn_matches_identity_and_keeps_newer_entries() {
        let registry = TurnRegistry::default();
        let old = Arc::new(ActiveTurn::new());
        registry.begin_turn(4, &old);

        assert!(registry.remove_turn(&old));
        assert!(registry.active(4).is_none());
        assert!(!registry.remove_turn(&old), "second removal is a no-op");

        let new = Arc::new(ActiveTurn::new());
        registry.begin_turn(4, &new);
        let stranger = Arc::new(ActiveTurn::new());
        assert!(!registry.remove_turn(&stranger));
        let registered = registry.active(4).expect("new entry stays registered");
        assert!(Arc::ptr_eq(&registered, &new));
        assert!(registry.remove_turn(&new));
        assert!(registry.active(4).is_none());
    }

    /// 同 clid 两条活条目：摘除一条不得连带删掉另一条
    #[test]
    fn remove_turn_drops_only_the_matching_entry_of_the_same_clid() {
        let registry = TurnRegistry::default();
        let voice = Arc::new(ActiveTurn::new());
        let text = Arc::new(ActiveTurn::new());
        registry.begin_turn(4, &voice);
        registry.begin_turn(4, &text);

        assert!(registry.remove_turn(&voice));
        assert!(!registry.remove_turn(&voice));
        let remaining = registry
            .active(4)
            .expect("the other entry stays registered");
        assert!(Arc::ptr_eq(&remaining, &text));
    }

    /// 两条回合各自在 STT/解析期间被放行，随后一起进入产出：集合按准入时刻升序返回全部，
    /// 插话据此逐条取消（定序只用于日志，取消不分先后）
    #[test]
    fn producing_turns_returns_every_producing_turn_in_admission_order() {
        let registry = TurnRegistry::default();
        let first = Arc::new(ActiveTurn::new());
        // 准入时刻必须可区分，否则本测试无法断言定序规则
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = Arc::new(ActiveTurn::new());
        registry.begin_turn(1, &first);
        registry.begin_turn(2, &second);
        first.mark_producing();
        second.mark_producing();

        let producing = registry.producing_turns();
        assert_eq!(producing.len(), 2);
        assert_eq!(producing[0].0, 1);
        assert_eq!(producing[1].0, 2);
    }

    /// 文本回合挤掉同 clid 正在产出的语音回合：`handle_chat_event` 不取消也不摘除旧条目
    /// （文本不夺取话语权），所以那条语音回合必须仍留在集合里、仍能被 `producing_turns` 找到；
    /// 否则它的播放取消句柄不可达，此后唤醒词插话也停不掉它
    #[test]
    fn displaced_producing_turn_stays_findable_for_a_later_barge_in() {
        let registry = TurnRegistry::default();
        let voice = Arc::new(ActiveTurn::new());
        registry.begin_turn(6, &voice);
        voice.mark_producing();

        // 文本回合：忽略 begin_turn 的返回值，不取消任何旧条目
        let text = Arc::new(ActiveTurn::new());
        registry.begin_turn(6, &text);

        let producing = registry.producing_turns();
        assert_eq!(producing.len(), 1);
        assert_eq!(producing[0].0, 6);
        assert!(Arc::ptr_eq(&producing[0].1, &voice));
        // 文本回合自己还没产出，不算「忙」
        assert!(!text.is_producing());
    }

    /// TOCTOU：插话裁决与回合收尾并发。取消已结束的回合是无害 no-op——任务已无 await 点，
    /// 令牌与播放取消位再置位也没人再看；摘除按身份匹配，所以要么清掉这条陈旧条目把话语权
    /// 交回空闲准入，要么发现该条目已不在集合里而不动同 clid 的新回合。
    #[test]
    fn barge_in_racing_turn_completion_only_clears_its_own_entry() {
        let registry = TurnRegistry::default();
        let finished = Arc::new(ActiveTurn::new());
        let playback = finished.playback_cancel();
        registry.begin_turn(5, &finished);
        finished.mark_producing();

        // 插话方在回合收尾之后才走到取消：目标来自早先的快照，这里模拟它仍持有 Arc
        let snapshot = registry.producing_turns();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].0, 5);
        let target = snapshot[0].1.clone();
        target.cancel.cancel();
        target.cancel.cancel();
        target.playback_cancel().store(true, Ordering::SeqCst);
        assert!(playback.load(Ordering::SeqCst));
        assert!(registry.remove_turn(&target));

        // 同一 clid 已被新回合接手：陈旧目标不得把新条目摘出表
        let successor = Arc::new(ActiveTurn::new());
        registry.begin_turn(5, &successor);
        assert!(!registry.remove_turn(&target));
        let registered = registry.active(5).expect("successor stays registered");
        assert!(Arc::ptr_eq(&registered, &successor));
    }

    /// 播放取消位在回合创建时就存在，且同一回合的多次取用共享同一个位：
    /// 一轮对话的多条 TTS 流靠它一起被插话停声
    #[test]
    fn playback_cancel_handle_is_turn_scoped_and_shared() {
        let turn = ActiveTurn::new();
        let first = turn.playback_cancel();
        let second = turn.playback_cancel();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(!first.load(Ordering::SeqCst));

        first.store(true, Ordering::SeqCst);
        assert!(second.load(Ordering::SeqCst));
    }
}
