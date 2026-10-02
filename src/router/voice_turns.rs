//! 语音回合准入：唤醒门裁决后的动作表，以及 per-clid 活跃回合注册表。
//!
//! 注册表只持 `Weak<ActiveTurn>`：条目在最后一个持有者释放时自然失效，
//! 无需摘除钩子。回合任务持一份，synth 任务持一份直到本轮音频播放收尾，
//! 所以「机器人正在产出」覆盖 LLM 流式与 TTS 播放尾两段。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Instant;

use tokio_util::sync::CancellationToken;

/// 唤醒门裁决后的准入动作
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WakewordAction {
    /// 丢弃本条 utterance
    Drop,
    /// 正常对话
    Talk,
    /// 插话：取消正在产出的回合（不分归属），再处理本条
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
    /// TTS 播放取消：置位后音频消费者在当前帧或段边界停止
    playback_cancel: OnceLock<Arc<AtomicBool>>,
    /// 是否已进入 LLM 回合。准入的「忙」只看它，避免解析与 STT 期间挡住同说话人的后续语音
    producing: AtomicBool,
    /// 准入时刻：出站音频按 FIFO 播放，最早准入的回合先占通道，插话据此在并发产出中定序
    pub(crate) started_at: Instant,
}

impl ActiveTurn {
    pub(crate) fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
            playback_cancel: OnceLock::new(),
            producing: AtomicBool::new(false),
            started_at: Instant::now(),
        }
    }

    /// 登记本轮 TTS 播放取消句柄；每回合只登记一次
    pub(crate) fn set_playback_cancel(&self, cancel: Arc<AtomicBool>) {
        let _ = self.playback_cancel.set(cancel);
    }

    pub(crate) fn playback_cancel(&self) -> Option<Arc<AtomicBool>> {
        self.playback_cancel.get().cloned()
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

/// per-clid 活跃回合注册表；键为说话人 clid（同一连接内稳定，无需 gRPC 解析）
#[derive(Default)]
pub(crate) struct TurnRegistry {
    turns: Mutex<HashMap<u32, Weak<ActiveTurn>>>,
}

impl TurnRegistry {
    /// 登记本条 utterance 的回合，返回被它挤掉的旧条目（同 clid 只保留最新一条）。
    ///
    /// 返回旧条目而非静默覆盖：旧回合可能还没进入 `mark_producing`（正在解析/STT），
    /// 此时它不在「忙」的视野里，却已持有回合状态；调用方需要顺手取消它，
    /// 否则会出现两条并发回合，其中一条的播放取消句柄已不可达。
    pub(crate) fn begin_turn(&self, clid: u32, turn: &Arc<ActiveTurn>) -> Option<Arc<ActiveTurn>> {
        let mut turns = self.turns.lock().expect("turn registry poisoned");
        turns.retain(|_, entry| entry.strong_count() > 0);
        turns
            .insert(clid, Arc::downgrade(turn))
            .and_then(|entry| entry.upgrade())
    }

    /// 持有出站音频通道的回合及其归属 clid；无产出回合时为 None。
    ///
    /// 「忙」与插话取消目标必须来自同一次快照：分两次查询会在两次加锁之间漂移，
    /// 出现「判定为忙但取不到取消目标」或反过来漏掉插话。
    ///
    /// 多条回合可以同时进入产出：各自在解析/STT 期间都不算忙，会被一起放行。
    /// 此时取值必须确定——出站音频按 FIFO 播放，最早准入的那条先占通道，插话要停的正是它；
    /// 按 HashMap 迭代顺序任取一条会随机停掉还在排队的回合，让插话听上去没生效。
    pub(crate) fn producing_turn(&self) -> Option<(u32, Arc<ActiveTurn>)> {
        self.turns
            .lock()
            .expect("turn registry poisoned")
            .iter()
            .filter_map(|(clid, entry)| entry.upgrade().map(|turn| (*clid, turn)))
            .filter(|(_, turn)| turn.is_producing())
            .min_by_key(|(_, turn)| turn.started_at)
    }

    /// 按身份摘除条目；条目已被新回合替换时不动新条目。返回是否确实摘除。
    pub(crate) fn remove_turn(&self, turn: &Arc<ActiveTurn>) -> bool {
        let mut turns = self.turns.lock().expect("turn registry poisoned");
        let target = turns
            .iter()
            .find(|(_, entry)| entry.upgrade().is_some_and(|live| Arc::ptr_eq(&live, turn)))
            .map(|(clid, _)| *clid);
        match target {
            Some(clid) => turns.remove(&clid).is_some(),
            None => false,
        }
    }

    /// 取该 clid 仍存活的回合；准入路径只用 `producing_turn` 与 `begin_turn`，
    /// 这个按 clid 的查询留给单测断言表内条目
    #[cfg(test)]
    pub(crate) fn active(&self, clid: u32) -> Option<Arc<ActiveTurn>> {
        self.turns
            .lock()
            .expect("turn registry poisoned")
            .get(&clid)
            .and_then(Weak::upgrade)
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
        assert!(registry.begin_turn(7, &turn).is_none());
        assert!(registry.active(7).is_some());
        assert!(registry.producing_turn().is_none());

        turn.mark_producing();
        let (clid, producing) = registry.producing_turn().expect("turn is producing");
        assert_eq!(clid, 7);
        assert!(Arc::ptr_eq(&producing, &turn));

        // 最后一个持有者释放后条目失效，无需摘除
        drop(producing);
        drop(turn);
        assert!(registry.active(7).is_none());
        assert!(registry.producing_turn().is_none());
    }

    #[test]
    fn begin_turn_replaces_and_returns_the_same_clid_entry() {
        let registry = TurnRegistry::default();
        let first = Arc::new(ActiveTurn::new());
        let second = Arc::new(ActiveTurn::new());
        assert!(registry.begin_turn(3, &first).is_none());

        let displaced = registry.begin_turn(3, &second).expect("first is displaced");
        assert!(Arc::ptr_eq(&displaced, &first));
        let registered = registry.active(3).expect("latest turn is registered");
        assert!(Arc::ptr_eq(&registered, &second));
    }

    /// 跨说话人插话：取消目标是产出方 clid，而不是喊唤醒词的说话人
    #[test]
    fn producing_turn_reports_the_owner_of_the_floor() {
        let registry = TurnRegistry::default();
        let speaker = Arc::new(ActiveTurn::new());
        let other = Arc::new(ActiveTurn::new());
        registry.begin_turn(3, &speaker);
        registry.begin_turn(9, &other);
        other.mark_producing();

        let (owner, producing) = registry.producing_turn().expect("turn is producing");
        assert_eq!(owner, 9);
        assert!(Arc::ptr_eq(&producing, &other));
        // 无产出的回合不算忙
        assert!(!speaker.is_producing());
    }

    /// 摘除按身份匹配：条目已被新回合替换时不得动新条目
    #[test]
    fn remove_turn_matches_identity_and_keeps_newer_entries() {
        let registry = TurnRegistry::default();
        let old = Arc::new(ActiveTurn::new());
        let new = Arc::new(ActiveTurn::new());
        registry.begin_turn(4, &old);

        assert!(registry.remove_turn(&old));
        assert!(registry.active(4).is_none());
        assert!(!registry.remove_turn(&old), "second removal is a no-op");

        registry.begin_turn(4, &new);
        let stranger = Arc::new(ActiveTurn::new());
        assert!(!registry.remove_turn(&stranger));
        assert!(registry.remove_turn(&new));
    }

    /// 两条回合各自在 STT/解析期间被放行，随后一起进入产出；插话目标必须是先占通道的那条
    #[test]
    fn producing_turn_returns_the_earliest_owner_when_two_are_producing() {
        let registry = TurnRegistry::default();
        let first = Arc::new(ActiveTurn::new());
        // 准入时刻必须可区分，否则本测试无法断言定序规则
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = Arc::new(ActiveTurn::new());
        registry.begin_turn(1, &first);
        registry.begin_turn(2, &second);
        first.mark_producing();
        second.mark_producing();

        let (owner, _) = registry.producing_turn().expect("both turns produce");
        assert_eq!(owner, 1, "出站 FIFO 先播最早准入的回合");
    }

    /// TOCTOU：插话裁决与回合收尾并发。取消已结束的回合是无害 no-op——任务已无 await 点，
    /// 令牌与播放取消位再置位也没人再看；摘除按身份匹配，所以要么清掉这条陈旧条目把话语权
    /// 交回空闲准入，要么发现条目已被新回合顶替而不动它。
    #[test]
    fn barge_in_racing_turn_completion_only_clears_its_own_entry() {
        let registry = TurnRegistry::default();
        let finished = Arc::new(ActiveTurn::new());
        let playback = Arc::new(AtomicBool::new(false));
        finished.set_playback_cancel(playback.clone());
        registry.begin_turn(5, &finished);
        finished.mark_producing();

        // 插话方在回合收尾之后才走到取消：目标来自早先的快照，这里模拟它仍持有 Arc
        let (owner, target) = registry
            .producing_turn()
            .expect("snapshot taken while producing");
        assert_eq!(owner, 5);
        target.cancel.cancel();
        target.cancel.cancel();
        target
            .playback_cancel()
            .expect("handle registered")
            .store(true, Ordering::SeqCst);
        assert!(playback.load(Ordering::SeqCst));
        assert!(registry.remove_turn(&target));

        // 同一 clid 已被新回合接手：陈旧目标不得把新条目摘出表
        let successor = Arc::new(ActiveTurn::new());
        registry.begin_turn(5, &successor);
        assert!(!registry.remove_turn(&target));
        let registered = registry.active(5).expect("successor stays registered");
        assert!(Arc::ptr_eq(&registered, &successor));
    }

    #[test]
    fn playback_cancel_handle_survives_the_turn_registration() {
        let turn = ActiveTurn::new();
        assert!(turn.playback_cancel().is_none());

        let cancel = Arc::new(AtomicBool::new(false));
        turn.set_playback_cancel(cancel.clone());
        turn.set_playback_cancel(Arc::new(AtomicBool::new(false)));

        cancel.store(true, Ordering::SeqCst);
        assert!(turn
            .playback_cancel()
            .expect("handle registered")
            .load(Ordering::SeqCst));
    }
}
