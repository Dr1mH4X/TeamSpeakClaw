# Agent Note: omni 音频历史的 wire 体积预算与会话自愈

Status: implemented

## Problem

`omni_model` 开启时音频轮以 16 kHz 单声道 WAV 字节用 `Vec<u8>` 直接进上下文，唯一的体积上限是单会话 50 MiB，由 `push` 内的 `drop_excess_audio` 执行；装配后的请求体（base64 加 JSON）在 `LlmProvider` 侧没有任何检查，历史每多一轮音频就多约 550 KiB，请求体随轮数线性增长。

单条 utterance 的上限可按常量算出：16 kHz 单声道 16 bit 是 32,000 B/s，语音满 `MAX_CHUNK_MS = 12000` 或静音满 `VAD_SILENCE_MS = 1200` 时冲刷，故单条最多 12 s 语音加 1.2 s 静音尾 = 422,444 B WAV（含 44 B 头），base64 后 563,260 B，加 JSON 外壳约 550 KiB；示例配置 `max_context_turns = 3` 的稳态是四个音频 part 约 2.15 MiB，第二轮语音就已经超过 nginx 默认的 `client_max_body_size 1m`。

50 MiB 存储折 base64 约 66.7 MiB，但 50 MiB ÷ 422,444 B = 124 轮，字节预算只有 `max_context_turns ≥ 124` 时才先于轮数生效，示例配置下真正起作用的是轮数而非字节。

内存是单会话预算乘会话数：`MAX_CONTEXT_SESSIONS = 1000` 下 50 MiB 单会话最坏约 50 GiB；装配一次请求还要整份复制历史、生成 1.33 倍 base64 字符串、在 `messages.to_vec()` 与请求体序列化处各再要一份缓冲，瞬时约为存储 WAV 的 5.3 倍。

不可自愈是核心失效模式：历史只在回合成功后写入，网关按体积返回 413 时回合返回错误、不落上下文，历史不变，下一轮装配出同样大的请求体；同一个 uid 的文本轮也回放同一份音频历史，于是语音轮与文本轮一起被永久拒到进程重启，每轮只回一条固定错误文案。

provider 的非 2xx 路径只保留状态码、丢弃响应体，连「体积类」都无法识别，因此会话没有从失败中恢复的入口。

## Decision

两层预算分工固定：存储层是 `MAX_AUDIO_HISTORY_BYTES = 8 MiB`（单会话）与 `MAX_AUDIO_HISTORY_BYTES_TOTAL = 256 MiB`（所有会话合计，`src/llm/context.rs`），装配层是 `pub const MAX_WIRE_AUDIO_BYTES = 2 MiB`，发送前预检是 `MAX_REQUEST_BODY_BYTES = 64 MiB`（`src/llm/provider.rs`）。

轮数上限在存储层的 `ContextWindow::push`，体积上限在装配层的 `history_for_wire`，装配时两者取更严的那个；`MAX_REQUEST_BODY_BYTES` 只拦装配预算管不到的部分（超大文本历史、工具 schema）失控，正常装配出的 omni 请求体在其之下，预检不会误杀。

### 装配层按 wire 预算裁剪

`ContextWindow::history_for_wire` 在锁内从最早的音频轮往前逐轮丢弃，直到估算的 base64 加 JSON 体积放得下 `MAX_WIRE_AUDIO_BYTES`，只 clone 保留的那一段。

文本轮不占音频预算，但落在可见后缀起点之前的文本与音频一并不可见，最新音频永不丢；存储历史不因裁剪改变，模型实际收到的 messages 才反映裁剪结果。

`LlmEngine::build_context_base` 用这个视图装配历史，`build_messages` 与 `build_omni_messages` 自动继承；`ContextWindow::get` 与引擎侧的 `stored_history` 只在测试可见，生产装配只走 `history_for_wire`。

尺寸参照写在常量注释里：2 MiB 容得下 3 个音频 part（2 个历史音频轮加当前轮，1.61 MiB），并在 4 MiB 级网关上留余量；网关限 1 MiB 时需把它调到 1 MiB 以下，此时装配只剩当前轮。

一条音频轮的固定 wire 开销由 `AUDIO_WIRE_ENVELOPE_BYTES` 精确给出（`data:audio/wav;base64,` 前缀加 `input_audio` content 的 JSON 外壳），`AudioBytes::wire_bytes` 用 `div_ceil(3) * 4` 加该常量；`src/llm/engine.rs` 的单测 `wire_estimate_matches_the_serialized_audio_part` 对真实 `serde_json::to_vec` 逐字节断言，估算漂移会让它失败。

### 体积类失败后的会话自愈

`OpenAiProvider` 在序列化请求体后按 `MAX_REQUEST_BODY_BYTES` 预检，超限直接返回 `PayloadTooLarge`、不发请求；非 2xx 归类里 413 一律算体积类，400 需要响应体含 `too large` 或 `请求体过大` 佐证，其余状态维持原样。

`TurnRequest::run`（`src/router/turn.rs`）在 `MAX_PAYLOAD_RETRIES = 1` 的循环里接住 `PayloadTooLarge`：收缩历史、重建 messages、重试一次，重试仍在回合开始时取得的容量占位与同会话串行锁之内，插话取消依旧不落上下文。

收缩只删最早的音频轮，不删文本轮、不删最新音频轮，且与装配共用同一可见后缀起点 `wire_keep_from`（`src/llm/context.rs`），把最早可删的可见音频轮连同整个不可见前缀一起删掉，因此重试请求严格小于失败的那一轮。

收缩快照保存收缩前的完整历史，重试以 `Cancelled` 结束时由 `restore_history` 整份放回，其他失败保留收缩，下一回合从更小的历史起步。

工具执行后的模型错误归 `ToolLoopError::AfterToolExecution`，`is_payload_too_large` 只认尚未执行工具的 `Other`，所以体积类失败不会被误判为可重试、已发生的工具副作用不会被重放。

`LlmEngine::shrink_for_payload_retry` 在会话没有音频可丢或只剩一条可见音频时返回 `None`，`TurnRequest::run` 直接返回原始错误、不做无意义的重试；NapCat 会话不存音频，与其他入站路径共用同一入口后同样原样失败。

### 存储层预算与全局账

`push` 先按 `max_context_turns` 与单会话音频预算逐轮丢弃最早的一轮（`drop_excess_audio`，至少保留最新一轮），再用 `drop_global_excess_audio` 按 `session_order` 从最早会话丢音频轮，直到全局计数 `audio_bytes_total` 回到 `MAX_AUDIO_HISTORY_BYTES_TOTAL` 以内。

最坏常驻音频因此是 256 MiB 这个全局常量，而不是 8 MiB × 1000 会话；全局逐出只动音频轮，文本轮与会话至少保留的最新一轮音频不受影响。

单会话与全局预算都是 `ContextWindow` 的可注入字段，测试助手 `with_audio_budget` 与 `with_audio_budgets` 注入小值覆盖逐出路径；没有引入用户可见设置项，生产路径仍固定为常量。

### base64 缓存与浅拷贝

`ContextUser::Audio(AudioBytes)`，`AudioBytes { wav: Arc<[u8]>, encoded: OnceLock<Arc<str>> }`：同一段音频首次编码后写入缓存，之后的回放命中缓存。

`ContextTurn` 克隆对 WAV 是 `Arc` 浅拷贝，装配只复制保留窗口的那一段；编码串随装配视图与请求释放，不长期占常驻内存。

### 未采纳的形态

「只保留最近 N 轮音频」没有进入实现：整轮丢弃会让更早的语音指代失败，那正是前一篇笔记要修的问题；文本占位会让模型读到并非用户原话的内容，与「历史里有什么等同模型看到什么」冲突；占位变体要成立还需显式来源标记并单独立项。

历史因此仍保留多轮音频，不需要修订「历史里有什么等同模型看到什么」这条原则。

`docs/subsystems/llm.md` 拥有音频预算与体积自愈的陈述（两层预算、`history_for_wire` 的不变量、收缩与还原的序列）；`omni_model` 的用户文档行与 `examples/config/settings.toml` 的 `max_context_turns` 注释都写明音频轮受体积预算约束、超出时从最早的音频轮开始丢弃。

## Alternatives considered

- **只修错误处理、不收缩历史**：把 413 变成可读错误并提示用户，但历史不变、请求体不变，会话仍永久卡死到重启，等于没修失效模式。
- **靠调低 `max_context_turns` 规避**：音频轮最大值 422,444 B，1 MiB 网关下 `max_context_turns = 1` 时已有 2 个 part 约 1.07 MiB 被拒，设为 `0` 等于关掉本特性，治不了根因。
- **压缩音频（16 kHz 降到 8 kHz，或改送 Opus/mp3）**：单轮体积可减半到约 275 KiB，但降低采样率会损失识别质量，换编码格式要重新验证 `input_audio` 的 data URL 与 mime，属于独立于本缺陷的改动。
- **要求运维调大 `client_max_body_size`**：可作文档建议，但网关不由本项目控制，不能作为唯一防线。
- **缩短单条 utterance（`MAX_CHUNK_MS` 12000 → 3000）**：单轮降到约 141 KiB，但改变断句与 STT 行为，是另一个产品取舍，不与本方案捆绑。
- **把音频历史放磁盘或按需重取**：常驻内存下降，但引入 I/O 与生命周期管理，且装配时仍需整份读入，收益主要在空闲内存而非请求体，与本缺陷无关。
- **只保留最近 N 轮音频（整轮丢弃或退化为 `[Audio message]` 文本占位）**：请求体上限约 (N+1) × 550 KiB、内存同比例下降，但整轮丢弃会让更早的语音指代失败，文本占位会让模型读到并非用户原话的文本，占位变体要成立还需显式来源标记并单独立项，故未采纳。

## Consequences

- 所得：示例配置下装配出的稳态请求体在 wire 常量以内（2 个历史音频轮加当前轮 1.61 MiB）；常驻音频内存由单会话 8 MiB 与全局 256 MiB 兜住；体积类拒绝之后会话在同一个回合内收缩并自愈，文本轮与音频历史各自独立、不会因音频超预算一起失败；历史回放命中 base64 缓存，装配只浅拷贝保留窗口。
- 代价：音频历史按装配层 wire 预算与存储层 8 MiB、256 MiB 一致地更早被丢，模型对早期语音的指代能力下降。
- 代价：失败路径上的收缩不可逆，非取消类失败会保留收缩，会话上下文比失败前小；不可见前缀里的旧文本会随收缩从存储历史中永久消失。
- 风险：wire 上限与 `max_context_turns` 语义重叠，容易被读成同一种上限，常量注释与 `docs/subsystems/llm.md` 写明后者在存储层、前者在装配层。
- 风险：413 归类依赖状态码与网关文案，自建反代可能返回 400、502 或自定义页，序列化前预检才是主防线，归类只作兜底。
- 风险：下调单会话预算会让长会话更早丢音频，属可感知的行为回退，`omni_model` 的用户文档行与示例配置注释都写明了体积约束。
- 风险：`ContextUser` 的克隆变浅拷贝后，任何依赖深拷贝的调用点都会改变语义；`OnceLock` 与 `Arc` 的组合在并发 `push`/`get` 下不引入 `unsafe`，写入由存储侧 `Mutex` 串行化。
- 未采纳项不生效：「只保留最近 N 轮音频」没有进入实现，多轮语音指代能力不回退，也不需要同步修订「历史里有什么等同模型看到什么」。
- 验证：`wire_budget_drops_earliest_audio_turns_until_the_body_fits`、`wire_budget_keeps_text_turns_and_the_latest_audio_turn`、`wire_budget_always_keeps_the_latest_audio_turn`、`assembled_omni_request_stays_within_the_wire_budget`、`wire_estimate_matches_the_serialized_audio_part` 覆盖装配层预算与逐字节对账。
- 验证：`omni_request_body_over_the_limit_is_rejected_before_send`、`non_size_http_errors_are_not_payload_errors`、`payload_too_large_drops_the_earliest_audio_turn_and_retries_once`、`retry_after_shrink_assembles_a_smaller_body`、`retry_is_attempted_only_once`、`non_payload_errors_do_not_shrink_the_history`、`payload_too_large_without_audio_does_not_retry` 覆盖预检、归类与重试边界。
- 验证：`push_applies_the_injected_audio_byte_budget`、`global_audio_budget_evicts_the_oldest_session_first`、`global_audio_budget_never_clears_the_latest_audio_turn`、`audio_turn_encodes_base64_once`、`cached_encoding_matches_a_fresh_encode`、`cloned_audio_turns_share_the_wav_allocation` 覆盖存储层逐出与编码缓存。
- 验证：`regression_payload_retry_must_not_repeat_completed_action`、`regression_retry_shrinks_already_clipped_wire_view`、`cancelled_retry_restores_the_history_the_shrink_removed` 守住「工具副作用不重放」「重试请求严格变小」「取消时还原」三条安全边界。
- 相关：`src/router/turn.rs` 的回合骨架是体积自愈的调用点归属，[LLM 回合骨架收敛为 `router/turn.rs` 单一归属](../../implemented/architecture/2026-10-03-llm-turn-module.md) 记录该归属与四入口合并；[体积重试的安全收缩与取消还原](../../implemented/bug-fix/2026-10-15-payload-retry-recovery.md) 记录收缩与装配共用可见后缀起点、快照式还原，以及工具执行后错误不纳入重试。
