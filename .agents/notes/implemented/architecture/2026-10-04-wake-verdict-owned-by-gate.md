# Agent Note: 唤醒裁决的累计与拼装归唤醒门

Status: implemented

## Problem

「这条 utterance 是唤醒词、是命令、还是噪音」的判定散在四个 module：检测与窗口在 `adapter/headless/wakeword.rs`，动作表在 `router/voice_turns.rs`，计量与 `WAKE_ONLY_TAIL_MS` 在 `router/voice_feedback.rs`，在飞状态与裁决拼装在 `router/voice_router.rs`。同一个 clid 在 router 的 `pending_wake` 与门的 `speakers` 里各有一条生命周期：router 在 `utterance_started` 时清条目、取裁决时 remove，门另按空闲驱逐，两条表互不知道对方的移除时机。收帧缝 `feed_wakeword_frame` 还把门推理与「帧长→采样→毫秒」记账交织在一起：它拿 `WakeEvent::fed_samples_before` 自己算帧起点、用 `mono_16k.len() * 1000 / 16_000` 自己算帧毫秒，再调 `WakeUtterance::note_frame`。这条缝没有测试覆盖——没有任何测试构造过 `VoiceRouter`，路由测试只直接调 `note_frame` / `combine_wake_verdict`，所以「喂帧路径算错毫秒或帧起点」不会被任何测试挡住；`voice_feedback.rs` 还从 `wakeword.rs` 再导出 `CHUNK_SIZE` 当 `WAKE_BLOCK_SAMPLES` 推算门的时间轴，`WAKE_ONLY_TAIL_MS` 的几何也只由一条重算块几何的测试钉住，而不是由喂帧的代码路径钉住。

## Decision

- 门模块持有 per-utterance 累计：`UtteranceState { detected, anchor_sample, active_ms_after, first_voiced_sample, recent_frames }` 挂在既有 `SpeakerSession` 上，`WAKE_ONLY_TAIL_MS`、`RecentFrame`、`has_command_tail` 与回补窗口一并落在 `wakeword.rs`。命中累计与开门窗口因此同一张 per-clid 表、同一条驱逐路径。
- 帧记账在门内完成：`feed(clid, WakeFrame { pcm16, voiced, utterance_started }, now)` 收帧长与 VAD 状态，内部算帧起点（`fed_samples`）与帧毫秒（`SAMPLE_RATE_16K_HZ`），并在推理后调用 `note_detection` / `note_frame`。路由层不再做时间轴换算。
- 裁决由门给出：`take_utterance_verdict(clid, now)` 一次取走命中、窗口与独占三项并返回 `WakeVerdict { open, detected, wakeword_only, anchor_sample, anchor_active_ms, utterance_start_sample }`，同时清空命中累计；取代原先的 `window_state` 只读查询 + router 侧 `combine_wake_verdict`。
- 块长不再对外：`CHUNK_SIZE` 收为门内私有，命中块序号由 `WakeEvent::detection_block_index` 与 `WakeVerdict::detection_block_index` 给出，路由层不需要知道 1280 这个数。
- 动作表接类型化的裁决：`decide_wakeword_action(verdict: &WakeVerdict, busy)` 取代 `(open, detected, busy)` 三个布尔位参，`detected && wakeword_only` 的重复判断收敛为 `wakeword_only`（该字段在未命中时恒为假）。
- 路由侧删掉 `VoiceRouter::pending_wake`、`VoiceRouter::WakeVerdict`、`combine_wake_verdict`；`feed_wakeword_frame` 只剩「喂帧、命中打点、命中即插话」。`voice_feedback.rs` 回到纯反馈音频：短语池与播放基建，不再持有唤醒计量。

## Alternatives considered

- **只把 `WakeUtterance` 从 `voice_feedback.rs` 挪到 `voice_router.rs`**：四个 module 变三个，但 per-clid 仍是两张表（`pending_wake` 与 `speakers`），帧记账仍留在收帧缝里，前两条症状都在。
- **保留 `pending_wake`，只把 `WAKE_ONLY_TAIL_MS` 常量搬到 `wakeword.rs`**：常量归属对了，但计量代码与阈值分居两地，判据仍然不由喂帧路径覆盖。
- **把动作表 `decide_wakeword_action` 与 `WakewordAction` 也移进 `wakeword.rs`**：判定彻底集中，但 `Drop`/`Talk`/`BargeIn` 是回合调度语义，依赖「机器人是否已在产出」这个回合注册表的量；适配器层不该定义回合并发动作。动作表留在 `voice_turns.rs`，改为接类型化裁决。
- **让 `take_utterance_verdict` 返回两个查询（窗口 + 命中累计）**：收尾路径会多拿一次门锁，且中间可能插入新的喂帧；一次取走并原子清零更贴近「这条 utterance 到此为止」的语义。

## Consequences

- 所得：判据集中一处，per-clid 状态回到一张表且与门同生命周期；帧→采样→毫秒的换算不再跨 module，收帧缝只传 `WakeFrame`；块长与阈值不再泄漏到路由层。
- 所得：裁决与阈值改由喂帧路径的测试覆盖——新增用例用生产帧长（320 采样）逐帧喂门再由 `take_utterance_verdict` 取裁决，钉住「命中块内 80ms 尾音判独占」「锚点后再攒 120ms 恰好到阈值判含命令」「锚点之前的活跃帧不计入」「新 utterance 不继承上一段命中」。原先重算块几何的常量断言删除，几何关系由这条路径本身钉住。
- 代价：`WakeEvent` 增两个打点字段（`anchor_active_ms`、`utterance_start_sample`），`fed_samples_before` 改名 `fed_samples` 并改语义为「喂完本帧后的累计采样数」——命中日志的 `input_samples` 语义不变，但门单测里对帧起点的断言改为对累计量的断言。
- 代价：`WakeVerdict` 由 `voice_router.rs` 的定义改为 `wakeword.rs` 的 `pub` 类型，`voice_turns.rs` 因此从 `adapter::headless::wakeword` 引入该类型；路由层内既有 adapter 依赖不变。
- 验证：`cargo fmt --check`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo test --all-targets --locked` 全绿。防回退点在于 `feed_wakeword_frame` 已不含任何采样/毫秒算术，`voice_feedback.rs` 已不含唤醒类型，`pending_wake` 表已不存在。