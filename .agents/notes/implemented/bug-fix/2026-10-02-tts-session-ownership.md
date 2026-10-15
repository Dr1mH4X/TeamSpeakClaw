# Agent Note: TTS 会话归 synth 任务所有，轮次收尾不再等待

Status: implemented

## Problem

流式 TTS 把回复切成 ≤28 字的句段逐段合成（`STREAM_TTS_MAX_CHARS`），synth 任务「合成一段 → `push_encoded`」受段通道（容量 8）背压，而段是由 actor 按 20ms 实时节奏播出去的，因此「等 synth 任务收尾」在时长上等于「等剩余音频播完」。

`TtsTurnRuntime::finish` 却把它当成 5s 级的收尾等待（`TTS_TEARDOWN_TIMEOUT`）：超时后 `drop(session)`，而 `TtsSession::Drop` 在未 `finish` 时置 `cancel`，消费者在段间轮询该标志（`play_encoded_stream`），于是丢掉队列里剩余段直接收尾。现象是长回复（积压超过 5 秒，约 9 段/250 字以上）说到一半静音，日志出现 `tts synth task teardown timed out`。超时分支还有一处隐藏等待：synth 在 `push_encoded().await` 期间持有会话槽这把 tokio mutex，`.lock().await` 要等消费者腾出槽位才拿得到，断点因此落在超时后一段音频处。

## Decision

- `TtsTurnRuntime::finish` 改为同步函数：调用 `TtsStreamState::close_stream` 置空当前流的句段发送端后随运行时一起放掉 `state`，不等播放、不取会话、不置 cancel；`TTS_TEARDOWN_TIMEOUT` 删除，`trace_id` 现存于 `TtsStreamState`（每条流一个，用于日志关联）。
- 会话生命周期归 synth 任务：任务在句段通道关闭后自行 `TtsSession::finish_drained`（关段通道并 await 本会话 job 收尾，不置 cancel），消费者播完已入队段后 `recv` 得到 `None`、job 正常收尾；await 期间任务持有的活跃回合句柄让「机器人正在产出」覆盖播放尾，插话因此能取消已交给消费者的音频。每条 LLM 流的会话由该流的任务独占持有（[TTS 会话按 LLM 流创建，工具轮与最终回复各占一条](../../implemented/bug-fix/2026-10-03-tts-session-per-llm-stream.md)）；`abort()` 先置回合持有的播放取消位（`ActiveTurn::playback_cancel`）再中止全部 synth 任务，未收尾的会话随任务 future 一起 drop，停声不依赖会话句柄。
- 补齐播放收尾 API：`TtsSession::finish_drained()` 关段通道后 await 本会话 job 的 `done_rx`（原本被 `finish` 丢弃），用于「确实要等播完」的调用方；`play_encoded_media_wait` 改由 `open_encoded_session` + `push_encoded` + `finish_drained` 组合，删掉一份重复的通道与 codec 探测管道。
- 回归测试：voice_router 侧断言 `finish` 关闭当前流且不置取消位、`abort` 置取消位并清空任务列表（`tts_turn_runtime_finish_closes_the_stream_without_cancelling`、`tts_turn_runtime_abort_cancels_playback_and_every_stream`）；audio_output 侧断言 `finish` 不置 cancel、`finish_drained` 在队列播完后返回且 `last_error` 为空，`tts_session_drop_aborts_job` 继续覆盖主动取消。

## Alternatives considered

- **只调大 `TTS_TEARDOWN_TIMEOUT`**：TTS 源没有总时长上限，等待时长必须覆盖整段剩余播放，且它是编译期常量、无环境变量开关；超时后仍是 drop→cancel，问题只是被推迟。
- **超时后 abort synth 再 finish 会话**：abort 会丢弃句段通道里最多 128 条未合成句段，已入队的 8 段能播完而尾巴仍被截断，等于把「说一半没声」换成另一种截断。
- **保留有界等待、超时仅记日志不 drop 会话**：每轮长回复白等 5 秒并占着同会话轮次锁，且超时分支不能去 lock 被背压任务持有的会话槽；收益只是常见路径能捕获 synth panic，不值得。
- **改 `TtsSession::Drop` 为不置 cancel**：Drop 置 cancel 是 gRPC `stream_tts_audio` 断连与错误路径主动停声的依赖，改动波及面大于本次缺陷。

## Consequences

- 所得：长回复整段播完；轮次收尾不再有 5 秒停顿，同会话轮次锁提前释放（下一轮音频靠 `AudioOutput` FIFO 自然排在前一轮之后）；`play_encoded_media_wait` 少一份重复管道，播放完毕有了显式 API。
- 代价：synth 任务可超出轮次存活整个剩余播放时长（准入登记与 `barge-in` 依赖这段存活期），且不在 router 的 `JoinSet` 里，`finish` 不再上报它的 panic；进程/运行时退出时随运行时一起终止。
- 验证：`cargo fmt --check`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo test --all-targets --locked`；实机用 >250 字的语音回复确认播完且日志不再出现 `tts synth task teardown timed out`。
