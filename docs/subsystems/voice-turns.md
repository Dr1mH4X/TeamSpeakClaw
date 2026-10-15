# 语音回合

语音回合是语音桥入站音频与桥接文本转成 LLM 回复、并把回复以语音送回频道的处理路径，运行在语音桥就绪后的 `VoiceRouter` 里。入口是 `src/router/voice_router.rs` 的 `VoiceRouter::run`；回合准入与取消状态在 `src/router/voice_turns.rs`，短反馈音频在 `src/router/voice_feedback.rs`，出站与录音回放句柄在 `src/tools/voice_audio.rs` 与 `src/tools/voice_replay.rs`。

## 职责

- 承载 STT 与 omni 两条语音回合：`OpusSttPipeline` 分段后由 `OpenAiSpeechProvider` 转写，或音频以 `input_audio` 直送模型；模型回复始终是文本，语音 API 响应受体积上限（STT 响应 64 KiB、TTS 音频 8 MiB）。
- 承载桥接文本回合：`prepare_chat` 在事件循环里同步完成过滤、触发判定、文本预处理与体积预检，取得 LLM 容量与到达顺序票据，并登记活跃回合。
- 拥有回合准入与取消：`TurnRegistry` 按 clid 保存活回合集合，`decide_wakeword_action` 把每条 utterance 的裁决映射为丢弃、正常对话或插话，取消时同时置 LLM 取消令牌与播放取消位。
- 拥有出站音频：TTS 流、提示音片段与回放片段统一排在 `AudioOutput` 的单一 FIFO 上，文本通知经 `VoiceNoticeSink` 落到语音桥。
- 拥有短反馈音频：`FeedbackPlayer` 预生成工具提示音与唤醒确认音短语并按触发点分组轮换取用；提示音在 STT 与 omni 回合的第一次工具调用开始执行时入队，桥接文本回合不挂，只喊唤醒词时播确认音并就地登记一个播报回合。
- 拥有录音回放：`voice_replay` 工具与 `!replay` 直呼从分说话人录音环取片段入队出站 FIFO，两者共用同一 ACL。

## 关键路径与序列

事件流分两路进入 `VoiceRouter::run`：`ChatEvent` 经 `prepare_chat` 同步准入（bot 自身与音乐 bot 按其名字过滤）后交给 `handle_chat_event`，`AudioFrameEvent` 经 `process_audio_frame` 过滤与分段，收帧路径喂唤醒门取裁决，成段后带 `SpeechChunk` 进 `admit_audio_chunk`。`admit_audio_chunk` 先占 `audio_turn_slots` 名额，再按 `decide_wakeword_action` 的结果丢弃、插话或放行，登记回合并把 `run_admitted_utterance` 派发到 `JoinSet`，准入路径因此不被 caller 解析与 STT 阻塞。回合任务取 caller 快照、跑 STT 或 omni、取得会话锁，随后 `handle_user_input` 以 `TurnRequest::run` 驱动 LLM 流。TTS 回调把模型 token 切成句段送进当前 `TtsStreamState`，首个可播句段到达时开出该流的会话与 synth 任务，任务合成、编码并把音频推进 `AudioOutput` 的 FIFO；回合正常结束时 `TtsTurnRuntime::finish` 只关当前流，失败或取消时 `abort` 置播放取消位并中止全部 synth 任务。

## 不变量

- 在飞名额先于任何取消与顶替取得：满额只丢弃新段、不动已有回合，先取消再占名额会让旧回合与新段一起丢。
- 取消一条回合必须置 LLM 取消令牌、置该回合的播放取消位，并按 `Arc::ptr_eq` 摘除条目；漏摘会让取消句柄不可达，回复播到自然结束。
- 「正在产出」在可失败的 TTS 初始化之后才置位，由每条流的 synth 任务持到本流音频播完；注册表只持 `Weak`，最后一个持有者释放即失效。
- 同一回合的多条 TTS 流与提示音片段共用同一个播放取消位；出站只有一路 FIFO，插话按产出集合整批取消而不分归属。
- 只有必须立刻放下的等待（caller 查询、STT、等 LLM 容量与会话锁、请求装配）用可取消等待，其余保持协作式取消，使工具调用与「送达成功才落库」保持原子。
- `TtsTurnRuntime` 正常收尾走 `finish`、失败走 `abort`；future 未走到 `finish`/`abort` 就被丢弃时由 `Drop` 兜底置播放取消位并中止 synth 任务，不留游离任务持有会话与回合句柄。

## 相关决策

- [窗口内再次命中唤醒词即插话（打断当前回合）](../../.agents/notes/implemented/feature/2026-10-02-voice-wakeword-barge-in.md) — 决定准入与执行分离、裁决表，以及插话取消的产出集合范围。
- [TTS 会话按 LLM 流创建，工具轮与最终回复各占一条](../../.agents/notes/implemented/bug-fix/2026-10-03-tts-session-per-llm-stream.md) — 决定 TTS 会话的创建与关闭时机、播放取消位的归属。
- [TTS 会话归 synth 任务所有，轮次收尾不再等待](../../.agents/notes/implemented/bug-fix/2026-10-02-tts-session-ownership.md) — 决定 `finish`/`abort` 与 `finish_drained` 的收尾语义。
- [短反馈音频走预生成 PCM 缓存与播报回合](../../.agents/notes/implemented/feature/2026-10-03-voice-feedback-phrases.md) — 决定提示音与确认音的短语池、入队时机与播报回合。