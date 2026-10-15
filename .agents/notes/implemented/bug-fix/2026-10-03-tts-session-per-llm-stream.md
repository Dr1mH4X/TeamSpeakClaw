# Agent Note: TTS 会话按 LLM 流创建，工具轮与最终回复各占一条

Status: implemented

## Problem

`should_close_tts_turn` 恒真后，每个 `finish_reason` 都会关掉本轮唯一的 TTS 会话，而 `on_turn_end` 同时把句段发送端置为 `None`，此后 `on_text_token` 再也推不进句段。工具轮（`finish_reason == "tool_calls"`）因此把本轮会话与发送端一起终结，工具执行完、模型继续流式输出最终回复时，文本 token 全部被丢弃：工具之后的正式回复不发声，只有提示音能排进 FIFO。

## Decision

- 会话生命周期 = 单条 LLM 流：`TtsStreamState::ensure_stream` 在首个可播句段到达时（`on_text_token` 推段，或 `on_turn_end` 冲刷出结尾段）创建会话与它的 synth 任务；该流 `finish_reason` 到达时 `close_stream` 只关当前流。工具轮与最终回复因此各占一条会话，工具轮结束时它的音频 job 随即收尾，提示音与新会话（最终回复）依次排在它之后。
- 没有可播内容的流不开会话：段过滤仍留在 synth 任务里（`is_speakable`，记 `skipping unspeakable tts segment`），但 `ensure_stream` 只被「有段可推」的路径调用，避免空会话在 FIFO 上占一个 job 并触发 `encoded stream closed before first segment`。
- 每条流的会话由其 synth 任务独占持有（`run_synth(session, rx, keepalive)`）：任务被中止（插话）时会话随 future 一起 drop，段通道随之关闭；任务持一份回合句柄直到 `finish_drained` 返回，「正在产出」因此覆盖本流播放尾。
- 播放取消位归回合所有：`ActiveTurn::playback_cancel`（`Arc<AtomicBool>`，回合创建即持有）经 `AudioOutput::open_tts_session_with_cancel` 注入每条流的音频 job，一轮里的多条 TTS 流与提示音片段共用它，插话一次全停。`TtsSession::cancel_handle` 与 `TtsTurnRuntime` 的会话槽、`shared_tx` 一并删除。
- `TtsTurnRuntime` 只留 `callbacks`、`state`、`playback_cancel`：`finish` 关当前流，`state` 交还给存活的 synth 任务；`abort` 先置回合播放取消位，再中止全部流的 synth 任务。

## Alternatives considered

- **`should_close_tts_turn` 只对 `stop` 返回真**：工具轮不关会话，提示音与最终回复推进同一条会话，不需要第二条流；代价是会话生命周期跨工具执行，而 `EncodedStream` 的段间空闲预算只有 `ENCODED_INTER_SEGMENT_IDLE_SECS`（30s）、总预算 `ENCODED_EXTERNAL_MAX_JOB_SECS`（120s），长工具会撞上超时，还要额外保活段。
- **工具提示音改到工具返回后与最终回复一起入队**：提示音的意义是「工具还在跑」，放到工具返回之后就没有反馈价值。
- **把最终回复的 token 缓存起来，等下一轮再补播**：要引入按流的文本缓存与重放语义；会话在流结束时本就自然收尾，重开一条的成本只是一个 FIFO job。

## Consequences

- 所得：工具调用之后模型给出的正式回复照常播报；首次工具调用的提示音排在回复之前；一轮对话的音频顺序是「工具轮前导语 → 提示音 → 最终回复」，与 FIFO 顺序直觉一致。
- 代价：一轮对话可能开多条 TTS 会话，每条各占一次 FIFO 入队与一次编码器初始化；工具很多的一轮（上限 16 轮 / 32 次调用）会有多条会话。
- 代价：`TtsSession::Drop` 未收尾时置位它持有的取消位，现在是回合级取消位；会话只在 synth 任务被中止（插话）或任务 panic 时未收尾，两者都应当停本回合音频。
- 验证：`cargo fmt --check`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo test --all-targets --locked`；voice_router 覆盖「工具轮之后再开一条新会话」（`tts_stream_state_opens_a_separate_session_per_stream`）、`finish` 关流但不置取消位、`abort` 置取消位并清空任务列表。实机需确认工具轮前导语、提示音与最终回复的先后，以及插话对各条流的取消。