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
    /// 插话：取消同说话人正在产出的回合，再处理本条
    BargeIn,
}

/// 状态表：门是否放行 × 本条是否命中唤醒词 × 机器人是否已开始产出 × 本条说话人是否有可取消的回合。
///
/// `open` 是喂门之后的裁决，已含「命中即开门」：`open == false` 时 `detected` 必为 false，
/// 所以表里「关 + 是」那一行进入本函数时是 `open == true`。窗口内命中唤醒词的语义与
/// 窗口起始状态无关，只由 `detected` 与 `cancellable` 决定是否插话。
pub(crate) fn decide_wakeword_action(
    open: bool,
    detected: bool,
    busy: bool,
    cancellable: bool,
) -> WakewordAction {
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
    if cancellable {
        return WakewordAction::BargeIn;
    }
    // 有产出但地板不属于本条说话人：让路，避免回复排到正在播的音频后面
    if busy {
        WakewordAction::Drop
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
    started_at: Instant,
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
    /// 登记本条 utterance 的回合：同 clid 覆盖旧条目（唤醒词即夺回话语权）
    pub(crate) fn insert(&self, clid: u32, turn: &Arc<ActiveTurn>) {
        let mut turns = self.turns.lock().expect("turn registry poisoned");
        turns.retain(|_, entry| entry.strong_count() > 0);
        turns.insert(clid, Arc::downgrade(turn));
    }

    /// 取该 clid 仍存活的回合
    pub(crate) fn active(&self, clid: u32) -> Option<Arc<ActiveTurn>> {
        self.turns
            .lock()
            .expect("turn registry poisoned")
            .get(&clid)
            .and_then(Weak::upgrade)
    }

    /// 取走并移除该 clid 的回合（插话用）
    pub(crate) fn take(&self, clid: u32) -> Option<Arc<ActiveTurn>> {
        self.turns
            .lock()
            .expect("turn registry poisoned")
            .remove(&clid)
            .and_then(|entry| entry.upgrade())
    }

    /// 是否有回合已开始产出（准入的「忙」）
    pub(crate) fn any_producing(&self) -> bool {
        self.turns
            .lock()
            .expect("turn registry poisoned")
            .values()
            .filter_map(Weak::upgrade)
            .any(|turn| turn.is_producing())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_gate_drops_utterances_without_a_wakeword() {
        assert_eq!(
            decide_wakeword_action(false, false, false, false),
            WakewordAction::Drop
        );
        assert_eq!(
            decide_wakeword_action(false, false, true, false),
            WakewordAction::Drop
        );
    }

    #[test]
    fn wakeword_passes_when_idle() {
        // 命中即开门：无论此前是否在窗口内，空闲时都放行（窗口顺带刷新）
        assert_eq!(
            decide_wakeword_action(true, true, false, false),
            WakewordAction::Talk
        );
    }

    #[test]
    fn a_wakeword_hit_always_arrives_with_an_open_gate() {
        // 防御性分支：命中即开门，所以 (open=false, detected=true) 不是真实输入
        assert_eq!(
            decide_wakeword_action(false, true, false, false),
            WakewordAction::Drop
        );
    }

    #[test]
    fn window_utterance_talks_only_while_idle() {
        assert_eq!(
            decide_wakeword_action(true, false, false, false),
            WakewordAction::Talk
        );
        assert_eq!(
            decide_wakeword_action(true, false, true, false),
            WakewordAction::Drop
        );
    }

    #[test]
    fn wakeword_barges_in_only_when_the_turn_is_cancellable() {
        assert_eq!(
            decide_wakeword_action(true, true, true, true),
            WakewordAction::BargeIn
        );
    }

    #[test]
    fn wakeword_makes_way_when_the_floor_belongs_to_another_speaker() {
        // 有产出但无可取消回合：丢弃，避免新回复排到正在播的音频后面
        assert_eq!(
            decide_wakeword_action(true, true, true, false),
            WakewordAction::Drop
        );
    }

    #[test]
    fn registry_tracks_only_live_turns() {
        let registry = TurnRegistry::default();
        let turn = Arc::new(ActiveTurn::new());
        registry.insert(7, &turn);
        assert!(registry.active(7).is_some());
        assert!(!registry.any_producing());

        turn.mark_producing();
        assert!(registry.any_producing());

        // 最后一个持有者释放后条目失效，无需摘除
        drop(turn);
        assert!(registry.active(7).is_none());
        assert!(!registry.any_producing());
    }

    #[test]
    fn registry_replaces_and_takes_the_same_clid_entry() {
        let registry = TurnRegistry::default();
        let first = Arc::new(ActiveTurn::new());
        let second = Arc::new(ActiveTurn::new());
        registry.insert(3, &first);
        registry.insert(3, &second);
        let registered = registry.active(3).expect("latest turn is registered");
        assert!(Arc::ptr_eq(&registered, &second));

        let taken = registry.take(3).expect("second turn is registered");
        assert!(Arc::ptr_eq(&taken, &second));
        assert!(registry.active(3).is_none());
        // 被替换的回合仍由原持有者维持存活，但不属于注册表
        assert!(!first.is_producing());
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
