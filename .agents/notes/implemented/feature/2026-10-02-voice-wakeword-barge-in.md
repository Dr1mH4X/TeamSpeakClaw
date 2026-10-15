# Agent Note: 窗口内再次命中唤醒词即插话（打断当前回合）

Status: implemented

## Problem

唤醒门只输出 `GateVerdict { open, detected, probability }`，而 `VoiceRouter` 只读 `open`：`detected` 仅用于日志。于是窗口内第二次命中唤醒词与窗口内普通语音等价，唯一效果是把 `last_wake` 刷新一次，唤醒词失去「打断」这层语义。

更硬的阻碍在结构上：音频 drain 任务串行 `await handle_audio_chunk(...)`，而该函数一路 await 到 STT 与整个 LLM 流。机器人播报期间这个任务正卡在回合里，后续 utterance 只能堆在容量 8 的 `audio_chunk_tx` 里，唤醒门根本没机会看到第二次唤醒词；同时 `TtsTurnRuntime::finish` 只关句段通道、不等播放，drain 任务会立刻处理下一条 utterance，新回合的 TTS 排进 `AudioOutput` FIFO 跟在旧回放后面，形成「答非所问 + 陈旧回复」。

## Decision

- 准入与执行分离：`admit_audio_chunk` 只做按 `decide_wakeword_action` 裁决、登记回合、把执行派发到 `JoinSet` 里的任务；回合任务自己解析 caller、跑 STT/omni 与 LLM。喂门在音频收帧路径逐帧进行（[唤醒门逐帧喂入，命中即插话](../../implemented/feature/2026-10-03-wakeword-per-frame-gate.md)），准入路径既无 gRPC 与 STT 等待，也不做唤醒推理。
- 状态表由纯函数 `decide_wakeword_action(open, detected, busy)` 唯一决定：`open == false` 或（已产出且本条无唤醒词）丢弃；`detected && busy` 插话；其余正常对话。`open` 是门窗口与在飞命中状态合成的裁决（命中即开门），所以表里「关 + 是」进入函数时 `open == true`。
- `busy` 取回合注册表里是否有回合已开始产出。出站音频只有一路，说话人无法从听觉上分辨当前在播的是谁触发的回复，所以命中唤醒词一律夺回话语权，不分归属取消占用者。
- 插话展开是同步的：置 `CancellationToken`（LLM 流）与回合持有的 TTS 播放取消位（`ActiveTurn::playback_cancel`）。`ActiveTurn` 同时持两者，注册表只持 `Weak`，回合任务与每条流的 synth 任务各持一份 `Arc`，synth 任务在 `finish_drained` 返回后才放掉——条目因此覆盖播放尾，无需摘除钩子。
- LLM 取消贯穿 `run_tool_loop`：provider 连接、每次流读取、每轮模型调用、每次工具执行前都检查令牌，取消返回 `ToolLoopError::Cancelled`；语音回合对它不回 `LLM_ERROR_REPLY`、不落上下文，文本路由传一次性令牌保持原行为。
- 逐帧取消：`process_encoded_segment` 的读帧循环检查播放取消位，插话不必等当前整句播完；`TtsTurnRuntime::abort` 先置回合播放取消位再中止全部流的 synth 任务，各流的会话随任务 future 一起 drop。
- 出站通知改经共享 `VoiceChannel`（`Channel` clone 共享同一条连接）持有者，回合任务各自建 client；传输失败后台重连替换通道，本次通知不重试，避免重复发送。

## Alternatives considered

- **窗口内普通语音改为延后队列（busy_policy = defer）**：要新增 per-说话人单槽、TTL 与空闲泵；陈旧指令延迟执行通常比丢弃更糟，v1 只做丢弃。
- **插话只取消自己发起的回合**：能区分归属，但出站音频只有一路，被插话的一方听到的仍是「有人在喊唤醒词」，按归属限制只会让新指令排到正在播的音频后面；后续若要限制可再加配置。
- **插话时取消一切出站音频（含音乐/External）**：`AudioJob` 没有回合归属，无法只停被打断回合的音频；音乐后端独立于本回合 TTS 通道，本次不涉及。
- **文本触发的回复不登记回合**：文本回复同样经 TTS 出声，不登记会让「忙」漏判、语音回复排到它后面；`handle_chat_event` 因此也登记 per-clid 回合。

## Consequences

- 所得：窗口内喊唤醒词有了与普通语音不同的语义（打断并插话）；准入不再被回合阻塞，播报期间的裁决与丢弃是即时的；被取消的回合不产生错误文案、不污染上下文。
- 代价：插话延迟由门自身的命中滞后与帧长决定，不再有 VAD 断句延迟（[唤醒门逐帧喂入，命中即插话](../../implemented/feature/2026-10-03-wakeword-per-frame-gate.md)）；唤醒词在一句话里越靠后，命中越晚。
- 代价：同说话人两条 utterance 都在各自的 STT 阶段（都未开始产出）时才可能同时准入，二者靠 `TurnCoordinator` 会话锁串行，取得锁的顺序理论上可与到达顺序不同；实际需要第二条 utterance 的 VAD 冲刷先于第一条的 STT+取锁完成，属窄窗口竞态。
- 代价：忙判定只覆盖「已开始产出」的回合；解析与 STT 期间的后续语音照旧进队列，与改动前一致。
- 验证：`cargo fmt --check`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo test --all-targets --locked`（236 项通过，其中 `voice_turns` 覆盖状态表与注册表生命周期、`tool_loop` 覆盖流中途取消与取消前置短路、voice_router 覆盖 abort 的播放取消位）、`cargo build --release --locked`。实机插话延迟、误触发率与 `busy` 判定开销未测量：需要 TeamSpeak 服务端、唤醒模型与 LLM/TTS 凭据。
