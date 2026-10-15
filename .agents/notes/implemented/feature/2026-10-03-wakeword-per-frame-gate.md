# Agent Note: 唤醒门逐帧喂入，命中即插话

Status: implemented

## Problem

唤醒门在 `admit_audio_chunk` 收到的是 VAD 切分后的整条 `SpeechChunk`，「命中即插话」因此要等用户把整句说完、再等 `VAD_SILENCE_MS`（1200ms）静音冲刷出 chunk 才可能发生。喊唤醒词打断播报时，实际听到的是机器人把当前这一句说完（一句可达十几秒），插话延迟以秒计。同一实现还有一处判据缺陷：它用 `WakeEvent::fire_offset` 之后那段音频的活跃度判「只喊了唤醒词」，而命中点之后恰好是 1200ms 静音尾音，短指令与纯唤醒词的尾长区间重叠。

## Decision

- 唤醒门改由音频收帧路径逐帧喂：`Payload::Audio` 分支里 `process_audio_frame` 之后立即调用 `VoiceRouter::feed_wakeword_frame`。`OpusSttPipeline::process_audio_frame` 返回 `SttFrameOutcome { mono_16k, voiced, utterance_started, chunk }`，解码成功的每一帧都带 PCM 与 VAD 状态出来，门只吃这一帧。
- 门内部按 80ms 块推理，`acc` 跨帧携带余量，逐帧喂与「一次性喂整条 utterance」在推理上等价，命中却能在帧内返回；`WakeEvent` 带 `fed_samples`（喂完本帧后的累计采样数），命中打点的 `fire_sample` 在门内由「喂帧前累计采样数 − carry + 本帧已排空的采样数」合成。
- 命中当场插话：`feed_wakeword_frame` 取 `TurnRegistry::producing_turn()` 的一次快照并取消（LLM 取消令牌 + 回合播放取消位），不等 utterance 收尾。
- 准入路径不再喂门：收尾时门的 `take_utterance_verdict` 合成 `WakeVerdict { open, detected, wakeword_only }`（命中累计与开门窗口同在 per-clid 会话上，命中累计在 `utterance_started` 与取裁决时清零），`admit_audio_chunk` 只按 `decide_wakeword_action` 调度。
- 「只喊了唤醒词」改流式判据：会话上的 `UtteranceState` 累计命中锚点之后的 VAD 活跃毫秒数，不足 `WAKE_ONLY_TAIL_MS`（200ms）判独占。纯静音不进 `active_ms_after`，命中帧自身不计入，所以 1200ms 静音尾音不再参与判据；旧的 `wakeword_only_utterance(pcm, fire_offset)` 能量判据删除。

## Alternatives considered

- **保持按 utterance 喂，只在 VAD 检出活跃语音时提前判定**：判定仍在帧内，但门只有完整 chunk 可喂，必须先攒够音频；等价于逐帧喂，只是把判断状态留在 pipeline 而不是门里。
- **收帧路径一次跑完准入（喂门后立即裁决并投递 `audio_chunk_tx`）**：收帧时已有裁决，但准入还要按状态表区分丢弃与正常对话、队列满要记日志；裁决留在既有准入任务，收帧只做「喂门 + 命中即插话」，两条路径各守一件事。
- **只为插话喂门，安静帧不喂**：省推理，但 `acc` 会跨静音段拼接，推理窗口与真实时间轴错位；门是 80ms 一块的流式模型，连续喂才是它的设计用法。

## Consequences

- 所得：唤醒词打断播报的延迟从「说完 + 1200ms 静音」降到一帧音频加模型自身的命中滞后；「只喊了唤醒词」的判据不再受 VAD 尾音长度干扰。
- 代价：唤醒推理从「只在有 utterance 时」变成「该说话人的每一帧」，CPU 占用随同时说话的人数上升（OpenWakeWord 三个 onnx 逐 80ms 块推理）；推理在事件循环内联执行，与 gRPC 音频解码、事件分发共用同一任务。
- 代价：短于 `MIN_CHUNK_MS` 的短促杂音现在也会喂门并可能插话，即「唤醒词必然能打断」，代价是误触发同样能打断。
- 验证：`cargo fmt --check`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo test --all-targets --locked`；`wakeword` 覆盖逐帧喂、收尾裁决只读会话与流式独占判据（命中块内 80ms 尾音判独占、锚点后攒够 200ms 判含命令）、`speech` 覆盖 `SttFrameOutcome` 的 VAD 状态与 PCM 携带、`voice_turns` 覆盖状态表与取消位生命周期。实机插话延迟与误触发率需 TeamSpeak 服务端、唤醒模型与 STT/TTS 凭据，未测量。