# 唤醒门

唤醒门是语音桥入站的第一道裁决：`[headless.wakeword]` 启用时，语音桥把解码后的每一帧音频喂给 `WakewordGate`，由它判定是否命中唤醒词、是否仍在该 clid 的开门窗口内、以及这条 utterance 是否只喊了唤醒词。门模块在 `src/adapter/headless/wakeword.rs`，推理核心与命中打点分居 `wakeword/model.rs`、`wakeword/calibration.rs`，喂帧与裁决调度在 `src/router/voice_router.rs`。语音回合的其余部分（STT/TTS、LLM、出站音频 FIFO）不属于本文。

## 职责

- 逐帧喂入 16k 单声道 PCM，门内按 80ms 块做 OpenWakeWord 推理（melspectrogram 与 embedding 前端 + 分类器），跨帧余量由门自己携带。
- 维护每个 clid 的开门窗口：命中即开门并刷新 `window_secs`，窗口内继续喂模型允许续命中，确认音播报按确认时刻续窗。
- 维护每条 utterance 的「只喊了唤醒词」判据：以命中块的绝对起始采样位置为锚点，累计锚点及其之后的 VAD 活跃毫秒。
- 每帧产出 `WakeEvent`（命中块锚点、原始分数序列、窗口标志、锚点后活跃毫秒、utterance 起点），utterance 收尾产出 `WakeVerdict` 的 `open`、`detected`、`wakeword_only` 三项裁决。
- 维护 per-clid 说话人状态表与推理模型实例池，并按空闲时间、容量上限与最久未见驱逐。
- 命中时取消占用出站音频的回合（插话触发侧），准入时按裁决调度丢弃、插话或放行。

## 关键路径与序列

1. 唤醒模型由会话生命周期在连接前加载（见[会话生命周期与重连](lifecycle.md)），`VoiceRouter` 只消费已加载的 `WakewordModels` 与 `[headless.wakeword].window_secs` 建 `WakewordGate`；未启用时门不存在，喂帧与裁决都走旁路。
2. 收帧：`Payload::Audio` 分支先经 `process_audio_frame`，每帧的 PCM 与 VAD 状态再交给 `feed_wakeword_frame` → `WakewordGate::feed`；门内排空足块余量逐块推理，命中时记锚点并回补锚点之后已喂入的帧。
3. 命中即插话：`WakeEvent` 带出命中块锚点时记 `voice.wakeword` 与 `voice.wakeword.calibration` 日志，再取 `turns.producing_turns()` 交给 `apply_barge_in`；出站音频不等 utterance 收尾即停声。
4. 收尾：断句端点处（收帧返回到完整段，或静默冲刷）调 `take_wake_verdict` → `WakewordGate::take_utterance_verdict`，裁决随完整语音段入 `audio_chunk_tx`。
5. 准入：`admit_audio_chunk` 先占音频名额，再按 `decide_wakeword_action(&verdict, busy)` 调度 `Talk`、`Drop` 或 `BargeIn`；`wakeword_only` 时记 `voice.wakeword.confirm` 并播唤醒确认音（同时登记一个播报回合）后返回，否则 `begin_turn` 派发 `run_admitted_utterance`。

## 不变量

- 命中与开门窗口相互独立：窗口内重命中仍报 `detected` 并把窗口起点顺延到最近一次命中，`open` 只在未命中时表达放行，否则插话语义会被窗口吞掉。
- 命中累计只在门内完成，只计锚点及其之后的 VAD 活跃帧，更早的活跃语音不计入。
- 收尾裁决只读会话状态、不喂音频，重复喂会打乱推理窗口。
- 不足一块的余量与累计喂入采样数跨帧、跨 utterance 与模型驱逐保留，丢失会让命中锚点与阈值累计错位。
- 说话人状态表上限 262 项、推理模型实例池上限 6 个；容量清理按已收段、在飞未命中、在飞已命中的次序取，同档取最久未见，逐出命中未收段的状态会让本条裁决再也取不回来。
- 裁决调度先于 gRPC 解析与 STT，且插话取消不分回合归属，命中即取消全部正在产出的回合。

## 相关决策

- [唤醒裁决的累计与拼装归唤醒门](../../.agents/notes/implemented/architecture/2026-10-04-wake-verdict-owned-by-gate.md) — 裁决、在飞语句与帧记账落在门内，路由层只按字段调度。
- [唤醒门逐帧喂入，命中即插话](../../.agents/notes/implemented/feature/2026-10-03-wakeword-per-frame-gate.md) — 收帧路径逐帧喂门，命中在帧内返回而不等 utterance 收尾。
- [窗口内再次命中唤醒词即插话（打断当前回合）](../../.agents/notes/implemented/feature/2026-10-02-voice-wakeword-barge-in.md) — `detected` 承担插话语义，命中取消占用出站音频的回合。
- [唤醒阈值固定为常量](../../.agents/notes/implemented/simplification/2026-10-02-wakeword-threshold-constant.md) — 检测阈值不进用户配置，固定为源码常量。
- [「只喊唤醒词」判据的命中锚点与阈值](../../.agents/notes/proposed/bug-fix/2026-10-03-wakeword-only-command-tail.md) — 独占判据的锚点口径、阈值算术与尚未执行的语料标定。