# Agent Note: omni 音频历史的 wire 体积预算与会话自愈

Status: proposed

行号口径：正文 file:line 已按当前工作树逐条核对（`src/llm/context.rs` 的音频预算与装配视图、`src/router/turn.rs` 的体积自愈入口、`src/router/voice_router.rs` 的回合注册表重构）；`## Problem` 里已被实现推翻的量按「修复前」标注，现值见 `## 落地状态`。这些函数或调用点一旦再改，引用行号需重新核对。（2026-10-03 回合归属重构把第 2 条的实现从 `src/router.rs` 移到 `src/router/turn.rs` 并合并了两个入口，见 [回合骨架收敛为 `router/turn.rs` 单一归属](../../implemented/architecture/2026-10-03-llm-turn-module.md)。）

## Problem

修复前的存储形态是 `ContextUser::Audio(Vec<u8>)`，存 16 kHz 单声道 WAV 字节（当时的 `src/llm/context.rs:120`；现为 `ContextUser::Audio(AudioBytes)`，`src/llm/context.rs:190-195`），唯一的体积上限是 `MAX_AUDIO_HISTORY_BYTES = 50 * 1024 * 1024`（修复前值，当时的 `src/llm/context.rs:112`；现为 8 MiB，`src/llm/context.rs:115`），由 `push` 内的 `drop_excess_audio` 执行（现调用点 `src/llm/context.rs:337`、定义 `src/llm/context.rs:417-430`），预算经 `audio_budget` 字段传入、生产默认即该常量（现 `src/llm/context.rs:239`、`258`），`audio_bytes()` 量的是 WAV 原始字节（现 `src/llm/context.rs:208-213`）；修复前装配后的请求体（base64 + JSON）没有任何检查，`LlmProvider` 侧也没有（当时的 `src/llm/provider.rs:303`、`318`；现发送前预检 `src/llm/provider.rs:396`、网关归类 `416-423`）。

单条 utterance 的体积上限可以按常量算出：16 kHz 单声道 16 bit 是 32,000 B/s；`MAX_CHUNK_MS = 12000`（`src/adapter/headless/speech.rs:52`）、`VAD_SILENCE_MS = 1200`（`50`）、`MIN_CHUNK_MS = 400`（`51`），静音尾在 `silence_ms <= VAD_SILENCE_MS` 时继续进缓冲（`src/adapter/headless/speech.rs:167`），冲刷条件是语音满 12 s 或静音满 1.2 s（`src/adapter/headless/speech.rs:173-174`），因此单条 utterance 最多 12 s 语音 + 1.2 s 静音尾 = 422,400 B PCM，加 44 B WAV 头（`src/adapter/headless/speech.rs:461`）得 422,444 B（412.5 KiB）。

base64 按 4/3 膨胀为 563,260 B，加 `data:audio/wav;base64,` 前缀 22 B 得 563,282 B，再加固定 JSON 外壳 98 B（现由 `AUDIO_WIRE_ENVELOPE_BYTES` 精确给出，`src/llm/context.rs:139-140`；计划时按 76 B 框架估算，偏差见 `## 落地状态`），一个音频轮在请求体里约 563,380 B ≈ 550 KiB（修复前编码点为当时的 `src/llm/engine.rs:176`，`format!` 加 `BASE64.encode`；现历史轮走 `omni_audio_part`（`src/llm/engine.rs:202-204`，命中 `AudioBytes` 缓存）、当前轮走 `current_audio_part`（`207-209`），wire 拼装只在 `audio_content`（`212-215`）一处）。

请求体随轮数线性增长：`build_context_base`（`src/llm/engine.rs:95-113`）对每个历史音频轮回放（`:101-105`；修复前每轮重新编码，现命中缓存），而历史只在回合成功后才写入（`src/router/voice_router.rs:1246`，文本轮 `:1370`、`src/router/ts_router.rs:286`），所以第 N 轮语音请求带 N−1 个历史音频轮加当前轮；示例配置 `max_context_turns = 3`（`examples/config/settings.toml:17`；代码默认 0，`src/config/llm.rs:25`）的稳态是 4 个音频 part = 2,253,432 B ≈ 2.15 MiB（2.25 MB），而第 2 轮语音就已经是 2 个 part = 1,126,716 B，超过 nginx 默认 `client_max_body_size 1m`（1,048,576 B）。

修复前 `MAX_AUDIO_HISTORY_BYTES` 允许的单请求上限并不小：50 MiB 存储折 base64 约 66.7 MiB，加当前轮约 67 MiB；但 50 MiB ÷ 422,444 B = 124.1 轮，字节预算只在 `max_context_turns ≥ 124` 时先于轮数生效，示例配置下真正起作用的是轮数而非字节。

内存整体是 per-session 预算乘会话数：`MAX_CONTEXT_SESSIONS = 1000`（`src/llm/engine.rs:22`）下修复前理论上限 50 GiB（示例配置下每会话约 3 × 422,444 B = 1.21 MiB，合计约 1.2 GiB）；装配一次请求还会整份复制历史（修复前为当时的 `src/llm/context.rs:232` 的 `cloned()`；现 `history_for_wire` 只 clone 保留窗口，`src/llm/context.rs:391`，且音频是 `Arc` 浅拷贝）、生成 1.33 倍 base64 字符串（现首次编码后缓存）、在 `messages.to_vec()` 处再复制一份（`src/llm/tool_loop.rs:72`）、在请求体序列化时再要一份缓冲（修复前为当时的 `src/llm/provider.rs:155` 的 `.json(body)`；现 `394-396` 先 `serde_json::to_vec` 预检、再由 `build_request` 以 `.body(body)` 发送，`src/llm/provider.rs:224-237`），瞬时约为存储 WAV 的 5.3 倍：示例配置约 6.4 MB/轮，饱和时约 265 MB/轮，乘 `MAX_CONCURRENT_TURNS = 4`（`src/llm/engine.rs:25`）约 1.06 GB。以上瞬时倍数是修复前口径，常驻部分现由单会话 8 MiB 与全局 256 MiB 两个预算兜住。

不可自愈是这次要重点解决的失效模式：历史只在成功时写入，`drop_excess_audio` 只被 `push` 调用，网关按体积拒绝（413）时 `run_llm_turn`（当时的入口；现为 `TurnRequest::run`，`src/router/turn.rs:74`）返回错误、不落上下文，历史不变，下一轮装配出同样大的请求体；同一个 uid 的文本轮也回放同一份音频历史（桥接文本 `handle_user_input`（`src/router/voice_router.rs:1276-1375`，装配点 `:1313`）与非桥接文本（`src/router/ts_router.rs:246`）都走 `build_context_base`），于是语音轮与文本轮一起被永久拒到进程重启，每轮只回一条 `LLM_ERROR_REPLY`（`src/router/voice_router.rs:1356`）；修复前 provider 的非 2xx 路径只保留状态码、丢弃响应体（当时的 `src/llm/provider.rs:335`；现 `416-423` 读响应体做体积类归类）。

## Proposal

按优先级排列。第 1、2 条修的是本次缺陷本身（体积失控与卡死），第 3 条修内存上限，第 4、5 条是可选优化，4 与 5 互斥。本轮落地情况逐条见 `## 落地状态`。

### 1. 装配请求时按 wire 体积设硬上限，逐轮丢弃最早的音频轮

改动：在 `src/llm/context.rs` 增常量 `MAX_WIRE_AUDIO_BYTES`（建议默认 2 MiB，足以容纳 3 个音频 part = 1.61 MiB，即 2 个历史音频轮加当前轮，并在 4 MiB 级网关上留余量；1 MiB 网关需把它降到约 1 MiB 以下，此时只剩当前轮），并让 `ContextWindow::get` 之外新增 `history_for_wire(&self, source, budget) -> Vec<ContextTurn>`；`src/llm/engine.rs` 的 `build_context_base`（`95-113`）改用它，`build_messages`（`116-131`）与 `build_omni_messages`（`134-149`）自动继承。

代码形状：`fn audio_wire_bytes(wav_len: usize) -> usize { wav_len.div_ceil(3) * 4 + 98 }`（落地为 `AudioBytes::wire_bytes`，`src/llm/context.rs:183-185`；外壳常量 `AUDIO_WIRE_ENVELOPE_BYTES = 98`，`src/llm/context.rs:139-140`，由 `engine` 单测 `wire_estimate_matches_the_serialized_audio_part`（`src/llm/engine.rs:375`）对真实 `serde_json::to_vec` 逐字节断言）；在 `ContextWindow` 锁内先按该估算从最早的 `ContextUser::Audio` 逐轮往前裁，只 clone 保留的那一段（`history_for_wire` 顺带把整份 clone 缩成保留窗口），直到总量 ≤ `MAX_WIRE_AUDIO_BYTES`；`ContextUser::Text` 轮不参与该裁剪（其体积已由 `MAX_USER_TEXT_BYTES = 1 MiB` 约束，`src/llm/engine.rs:16`）；预算做成 `history_for_wire` 的入参，测试可注入小值。

影响：请求体上限由常量决定，与 `max_context_turns` 解耦；示例配置下稳态由 2.15 MiB 降到 ≤ 2 MiB（按建议值），配 1 MiB 网关时降到 ~550 KiB（只带当前轮）；内存同比例下降。

风险：这是视图级裁剪，存储里仍保留音频轮（`push` 的轮数上限不变），因此模型看到的历史可能短于存储的历史——但「历史里有什么等同模型看到什么」指的是模型实际收到的 messages，裁剪后仍然成立；`max_context_turns` 与 wire 上限两个概念重叠，需在注释与 `docs/architecture.md:33` 说清。

测试：`wire_budget_drops_earliest_audio_turns_until_the_body_fits`（`src/llm/context.rs:642`）、`wire_budget_keeps_text_turns_and_the_latest_audio_turn`（`src/llm/context.rs:661`）、`assembled_omni_request_stays_within_the_wire_budget`（对 `build_omni_messages(...)` 调 `serde_json::to_vec` 断言长度 ≤ 上限 + 一个当前轮，`src/llm/engine.rs:343`）。

### 2. 体积类失败后收缩历史并允许重试一次，使会话自愈

改动：`src/llm/context.rs` 增 `pub fn drop_oldest_audio_turn(&self, source) -> bool`（返回是否丢掉一轮，只丢最早的 `ContextUser::Audio`，不丢文本轮、不丢最新音频轮；现 `src/llm/context.rs:396-412`）；`src/llm/provider.rs` 在发送前加体积预检（`serde_json::to_vec(&body)` 长度 > 常量即返回可识别的 `PayloadTooLarge`，不发请求；现 `src/llm/provider.rs:394-396`），并把非 2xx 归类：413 或 400 且响应体含 `too large` / `entity too large` / `请求体过大` 归同一类，其余维持现状（现 `classify_payload_too_large`，`src/llm/provider.rs:80-90`、调用点 `416-423`）；调用点 `handle_omni_audio_chunk`（`src/router/voice_router.rs:1179-1272`，调用在 `:1209`、错误分支在 `:1260-1265`）与 `handle_user_input`（`src/router/voice_router.rs:1276-1375`，调用在 `:1313`、错误分支 `:1352-1357`），以及非桥接文本轮（`src/router/ts_router.rs:246` 调用、`:294` 错误分支）：捕获该类错误后收缩历史、重建 messages、重试一次。`src/router/nc_router.rs:367` 是第四个调用点，仍用原 `run_llm_turn`，因为 NapCat 会话不存音频、没有可丢的轮（后续[回合骨架收敛为 `router/turn.rs` 单一归属](../../implemented/architecture/2026-10-03-llm-turn-module.md) 把四个调用点合并到同一入口，nc 因此也走这条路径，实际结果不变：无音频可丢即原样失败）。

代码形状：`const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024 * 1024;`（现 `src/llm/provider.rs:29`；没有复用第 1 条的 wire 常量，两者的分工写在常量注释里）；`const MAX_PAYLOAD_RETRIES: usize = 1;`（现 `src/router/turn.rs:21`）用循环包住工具循环，`PayloadTooLarge` 且 `retries < MAX_PAYLOAD_RETRIES` 时 `drop_oldest_audio_turn` 后继续（现 `TurnRequest::run`，`src/router/turn.rs:74-142`；回合归属重构后两个入口合并为一个，`run_llm_turn` 与 `run_llm_turn_with_audio_recovery` 不复存在）；重试仍在既有容量占位与同会话串行锁内。错误传递靠 provider 侧定义 `struct PayloadTooLarge`（现 `src/llm/provider.rs:36-68`），调用判定收敛到共用助手 `is_payload_too_large`（`src/router/turn.rs:195`）——`LlmProvider` 返回 `anyhow::Result`、`ToolLoopError::Other(#[from] anyhow::Error)`（`src/llm/tool_loop.rs:45`）会原样保留该类型，不必改 trait 签名。

影响：网关按体积拒绝不再让会话死锁，被拒的历史尾部逐轮收缩直到放得下；代价是被丢音频从后续上下文消失，且最坏两次完整回合的超时预算（连接 10 s、流总 300 s，`src/llm/provider.rs:19`、`21`）。

风险：收缩是破坏性副作用，连续失败会逐步清空音频历史（以重试上限 1 次/轮兜住）；错误分类依赖网关文案，预检才是主防线；分类逻辑不要进 `ToolLoopError` 之外的层级，避免每个调用点各写一份判断。

测试：`payload_too_large_drops_the_earliest_audio_turn_and_retries_once`（`src/router/turn.rs:509`）、`retry_after_shrink_assembles_a_smaller_body`（`src/router/turn.rs:537`）、`retry_is_attempted_only_once`（`src/router/turn.rs:560`）、`non_payload_errors_do_not_shrink_the_history`（`src/router/turn.rs:456`）、`payload_too_large_without_audio_does_not_retry`（`src/router/turn.rs:586`）；预检用例 `omni_request_body_over_the_limit_is_rejected_before_send`（`src/llm/provider.rs:645-667`）与归类用例（`src/llm/provider.rs:668-695`）覆盖其余边界。

### 3. 参数化单会话音频预算并给「会话数 × 单会话预算」加全局上限

改动：单会话预算已经是 `ContextWindow` 的可注入字段（现 `src/llm/context.rs:239`、`258`，测试助手 `with_audio_budgets` 在 `276-289`、`with_audio_budget` 在 `265-272`），生产路径仍固定为 `MAX_AUDIO_HISTORY_BYTES`（`src/llm/context.rs:115`）；本项剩下的是把常量下调（落地取值 `8 * 1024 * 1024`）并在 `push`（`src/llm/context.rs:297-346`）里加全局账：新增 `MAX_AUDIO_HISTORY_BYTES_TOTAL`（落地取值 256 MiB，`src/llm/context.rs:120`），逐出后若全局超限，按 `session_order`（字段 `src/llm/context.rs:247`、push 内使用 `310-323`）从最早会话丢音频轮（`drop_global_excess_audio`，`src/llm/context.rs:450-468`）；「用户可配」形态按 `src/config/AGENTS.md` 的少即是多倾向不做（本轮确认不做，`src/llm/engine.rs:37` 仍只传 `max_context_turns`）。

影响：最坏常驻音频从 50 GiB（修复前 50 MiB × 1000 会话）降到 256 MiB；代价是长会话音频历史更早被丢。

风险：全局逐出会让不活跃会话的历史被别的会话挤掉，产生跨会话耦合；`session_order` 与 `histories` 的一致性是既有不变式（`concurrent_writes_respect_the_session_limit`，`src/llm/context.rs:786`），扩展时需保持；把总预算平均分到 1000 会话只有 256 KiB，小于一条 utterance 的 412 KiB，说明 `MAX_CONTEXT_SESSIONS = 1000` 与音频历史本身不相容，二者之一必须让步（下文 Alternatives 讨论）。

测试：`push_applies_the_injected_audio_byte_budget`（`src/llm/context.rs:542`）、`global_audio_budget_evicts_the_oldest_session_first`（`src/llm/context.rs:597`）、`global_audio_budget_never_clears_the_latest_audio_turn`（`src/llm/context.rs:626`）；计划里的 `per_session_audio_budget_is_injectable` 与 `audio_budget_never_clears_the_latest_audio_turn` 由首条与 `single_turn_over_budget_still_keeps_the_latest_turn`（`src/llm/context.rs:587`）覆盖。

### 4. 缓存 base64，省掉每轮重新编码与整份 clone（与第 5 条互斥）

改动：`src/llm/context.rs` 的 `ContextUser::Audio(Vec<u8>)` 改为 `Audio(AudioBytes)`（现 `src/llm/context.rs:194`），`AudioBytes { wav: Arc<[u8]>, encoded: OnceLock<Arc<str>> }`（现 `src/llm/context.rs:153-156`）；当时的 `src/llm/engine.rs:175-178` 的 `omni_audio_part` 改为接收 `&AudioBytes`，首次编码后写入 `OnceLock`，之后直接 `Arc` 共享（现 `omni_audio_part` 在 `src/llm/engine.rs:202-204`、编码缓存在 `AudioBytes::base64`（`src/llm/context.rs:172-180`））；`ContextTurn` 的 `Clone` 由深拷贝 WAV 变为共享指针（同时消掉整份历史 clone，现 `history_for_wire` 只 clone 保留窗口，`src/llm/context.rs:391`）。

影响：每轮省下一次 4/3 编码与一次整份历史拷贝（示例配置省约 1.2 MB 分配/轮，修复前饱和时省 50 MiB 拷贝/轮）；代价是常驻内存变 1.33 倍（示例配置 1.21 MiB → 1.61 MiB/会话；修复前饱和时 50 MiB → 66.7 MiB/会话，现单会话上限 8 MiB 折 base64 约 10.7 MiB）。

与 2026-10-02 的取舍：当时的理由见 [../implemented/2026-10-02-omni-audio-context-history.md](../../implemented/feature/2026-10-02-omni-audio-context-history.md) 的 `Alternatives considered` 第 2 条（`../implemented/2026-10-02-omni-audio-context-history.md:20`）——base64 是协议编码、不是数据本身，长期驻留不划算；该理由成立的前提是「同一段音频只会被编码一次」，而修复前是每轮都重编码一次（第 N 轮编码 N−1 段），因此把一次性编码换成常驻 1.33 倍在本缺陷下更划算；若采纳第 5 条只留 1 轮音频，回放次数降到 1，本条的收益随之消失，因此 4 与 5 择一。本轮选了第 4 条。

风险：`ContextUser` 的 `Clone` 语义从深拷贝变共享，现有测试助手按值构造的写法需调整（现 `audio_turn` 在 `src/llm/context.rs:482-487`）；`OnceLock` 与 `Arc` 的组合要在并发 `push`/`get` 下保持无 `unsafe`（`Mutex` 已串行化写入）。

测试：`audio_turn_encodes_base64_once`（`src/llm/context.rs:727`）、`cached_encoding_matches_a_fresh_encode`（`src/llm/context.rs:741`）、`cloned_audio_turns_share_the_wav_allocation`（`src/llm/context.rs:751`）。

### 5. 只保留最近 N 轮音频，更早的轮整轮丢弃（或退化为文本占位）

**未采纳（本轮明确不做）**，当初列出的取舍理由保留如下。

改动：`src/llm/context.rs` 的 `push`/`drop_excess_audio`（现调用点 `src/llm/context.rs:337`、定义 `src/llm/context.rs:417-430`）增 `MAX_AUDIO_TURNS_IN_HISTORY: usize = 1`，超出的 `ContextUser::Audio` 轮在存储层处理；两种形态：整轮丢弃（保持 user/assistant 成对与真假一致）或退化 `ContextUser::Text("[Audio message]")` 占位（保留轮次骨架）。

影响：请求体上限约 (N+1) × 550 KiB（N=1 为 1.07 MiB），内存同比例下降；与既定原则的冲突在于文本占位会让模型读到一句并非用户原话的文本，而整轮丢弃会让更早的语音指代失败——后者正是 2026-10-02 要修的问题（同笔记 `Problem`（`../implemented/2026-10-02-omni-audio-context-history.md:7`）与 `Alternatives considered` 第 1、4 条（`../implemented/2026-10-02-omni-audio-context-history.md:19`、`22`））。

建议的取舍（保留，本轮不做）：N=1 时用整轮丢弃，不用文本占位；占位变体只有在做成显式标注来源的固定标记（如 `[voice message omitted]`）并同步修订上述原则时才可接受，且需要单独立项。

风险：多轮语音指代能力回退；改动「历史里有什么」的语义，实现时需同步 `docs/architecture.md:33` 与该 implemented note。

测试：`only_the_latest_n_audio_turns_remain`、`audio_retention_never_drops_the_latest_turn`、`degraded_turns_use_an_explicit_omission_marker`（若采纳占位变体）。

## 落地状态

第 1、2、3、4 条已落地，第 5 条未采纳（见上）。以下逐条映射，只记与计划的偏差与当前引用；行号按当前工作树核对。

**第 1 条 — 已落地。** `MAX_WIRE_AUDIO_BYTES` 是 `pub const`（`src/llm/context.rs:133`），取值 2 MiB 与计划一致。偏差一：外壳常量。计划按「22 B 前缀 + 76 B 框架」估算，落地改为精确常量 `AUDIO_WIRE_ENVELOPE_BYTES`（`src/llm/context.rs:139-140`，字面量与 `engine::audio_content` 的构造逐字对应），`wire_bytes` 用 `div_ceil` 而非 `/ 3 * 4`（`src/llm/context.rs:183-185`），并由 `wire_estimate_matches_the_serialized_audio_part`（`src/llm/engine.rs:375`）对真实 `serde_json::to_vec` 逐字节断言。偏差二：`ContextWindow::get` 变为 `#[cfg(test)]` 的测试入口（`src/llm/context.rs:348-359`），生产装配只走 `history_for_wire`（`src/llm/context.rs:366-392`），引擎侧的 `stored_history` 也只在测试可见（`src/llm/engine.rs:178-182`）。

**第 2 条 — 已落地。** 重试只有一处实现：`TurnRequest::run`（`src/router/turn.rs:74-142`）里 `MAX_PAYLOAD_RETRIES = 1`（`src/router/turn.rs:21`）的循环包住工具循环，`PayloadTooLarge` 且 `retries < MAX_PAYLOAD_RETRIES` 时 `drop_oldest_audio_turn` 后继续；错误识别收敛为共用助手 `is_payload_too_large`（`src/router/turn.rs:195`），预检与归类拆成纯函数 `ensure_payload_within_limit`（`src/llm/provider.rs:71-76`）与 `classify_payload_too_large`（`src/llm/provider.rs:80-90`）；`MAX_REQUEST_BODY_BYTES = 64 MiB`（`src/llm/provider.rs:29`）与计划值一致。`drop_oldest_audio_turn` 的语义与计划一致：只丢最早的音频轮，文本轮与最新音频轮保留（`src/llm/context.rs:396-412`、`drop_earliest_audio_turn`（`src/llm/context.rs:434-446`））。计划里的 `omni_request_body_over_the_limit_is_rejected_before_send` 落在 `src/llm/provider.rs:645-667`，归类用例在 `src/llm/provider.rs:668-695`。后续的[回合骨架收敛为 `router/turn.rs` 单一归属](../../implemented/architecture/2026-10-03-llm-turn-module.md) 改了两处与本条相关的形状：① 两个入口合并为 `TurnRequest::run` 一个，`source` 的 `Option` 分支消失；② 第四个调用点（`nc_router::handle_text`，`src/router/nc_router.rs:223`）改用同一入口，于是 NapCat 也走这条重试路径；其会话不存音频，`drop_oldest_audio_turn` 返回 `false` 即原样失败，不产生额外请求（`non_payload_errors_do_not_shrink_the_history`（`src/router/turn.rs:456`）与 `payload_too_large_without_audio_does_not_retry`（`src/router/turn.rs:586`）覆盖无音频可丢时不收缩）。同一次重构也把兜底文案收进 `TurnRequest::run` 经 `TurnSink` 送达（`src/router/turn.rs:132`），四条入站路径共用一份 `LLM_ERROR_REPLY`，只有投递失败与插话取消两种情况不发兜底（`backend_failure_replies_the_fixed_text_and_saves_nothing`（`src/router/turn.rs:407`）、`undelivered_reply_is_not_saved`（`src/router/turn.rs:390`）、`cancelled_turn_neither_replies_nor_saves`（`src/router/turn.rs:475`））。

**第 3 条 — 已落地（用户可配形态按计划不做）。** 单会话预算 50 MiB → 8 MiB（`src/llm/context.rs:115`），新增全局 `MAX_AUDIO_HISTORY_BYTES_TOTAL = 256 MiB`（`src/llm/context.rs:120`）与 `ContextState::audio_bytes_total` 记账（`src/llm/context.rs:249`），`push` 内先做单会话逐出再做全局逐出（`drop_global_excess_audio`，`src/llm/context.rs:450-468`）。没有引入用户可见设置项，`examples/config/settings.toml` 与 `website/docs/configuration.md` 因此未改。

**第 4 条 — 已落地（4 与 5 择一，本轮选第 4 条）。** 形态与计划一致：`ContextUser::Audio(AudioBytes)`（`src/llm/context.rs:194`）、`AudioBytes { wav: Arc<[u8]>, encoded: OnceLock<Arc<str>> }`（`src/llm/context.rs:153-156`）、`base64()` 首次编码后缓存（`src/llm/context.rs:172-180`）；`ContextTurn::clone` 对音频是浅拷贝，装配只 clone 保留窗口（`src/llm/context.rs:391`）。测试名与计划一致（`src/llm/context.rs:727`、`741`、`751`）。

**第 5 条 — 未采纳（本轮明确不做）。** 取舍理由见上文第 5 条：整轮丢弃会让更早的语音指代失败，文本占位会让模型读到并非用户原话的内容；占位变体要成立需显式来源标记并单独立项。

**未随本轮同步的地方。** `docs/architecture.md:33` 已随常量更新（写出 8 MiB / 256 MiB / 2 MiB / 64 MiB 与 NapCat 不接入重试）；但 `website/docs/configuration.md:28` 与英文镜像 `website/i18n/en/docusaurus-plugin-content-docs/current/configuration.md:28` 仍只有 `omni_model` 一行、没有音频历史的体积代价，`examples/config/settings.toml:17` 的 `max_context_turns` 注释也没有补体积约束；`.agents/notes/implemented/2026-10-02-omni-audio-context-history.md` 的 `Status`、`Decision`（`:15`）与 `Consequences`（`:27`）仍写 50 MiB。

## Acceptance criteria

- 任一调用点装配出的请求体（`serde_json::to_vec` 长度）都不超过第 1 条常量加上一个最新轮的余量，并有不依赖真实网络的单测覆盖。
- 体积类失败后同一会话的下一轮请求体严格小于失败的那一轮；同一 uid 的文本轮不再被音频历史阻塞。
- 单会话与全局音频内存各有常量上限，逐出顺序有单测；最坏常驻从修复前的 50 GiB 降到全局上限。
- 事件循环与 `TurnCoordinator` 的并发门禁语义不变：重试仍持有原 capacity permit 与 session guard，插话取消仍不落上下文。
- 若引入用户可见设置项，则 `examples/config/settings.toml` 与 `website/docs/configuration.md`、`website/i18n/en/docusaurus-plugin-content-docs/current/configuration.md` 三处同步；本轮未引入用户可见项，因此只更新 `docs/architecture.md`（已更新）。
- `cargo test --all-targets --locked` 与 `cargo clippy --all-targets --locked -- -D warnings` 通过。

## Risks

- 第 1 条的 wire 上限与 `max_context_turns` 语义重叠，容易被读成同一种上限，需要在常量注释与架构文档里写清两者关系（轮数上限在存储层，体积上限在装配层）。
- 第 2 条收缩历史不可逆，网关持续拒绝时会逐轮清空音频历史；重试上限 1 次只防放大，不保证保留内容。
- 413 归类依赖状态码与网关文案，自建反代可能返回 400/502/自定义页；以序列化前预检为主防线，归类只作兜底。
- 第 3 条下调预算会让长会话更早丢音频，属可感知的行为回退，需要公告式文档说明而不是悄悄改常量。
- 第 4 条引入共享分配后 `ContextUser` 的 `Clone` 变浅拷贝，任何依赖深拷贝的调用点都会改变语义。
- 第 5 条与既定「历史里有什么等同模型看到什么」原则冲突，占位变体会让模型看到非原话内容；本轮未采纳，该风险不生效。

## Alternatives considered

- **只修错误处理、不收缩历史**：把 413 变成可读错误并提示用户，但历史不变、请求体不变，会话仍永久卡死到重启，等于没修失效模式。
- **靠调低 `max_context_turns` 规避**：音频轮最大值 412 KiB，1 MiB 网关下 `max_context_turns = 1` 时已有 2 个 part（1.07 MiB）被拒，`0` 则等于关掉本特性，治不了根因。
- **压缩音频（16 kHz 降到 8 kHz，或改送 Opus/mp3）**：单轮体积可减半到约 275 KiB，但降低采样率会损失识别质量，换编码格式要重新验证 `input_audio` 的数据 URL 与 mime，属于独立于本缺陷的改动。
- **要求运维调大 `client_max_body_size`**：可作文档建议（见 `## Consequences` 的文档同步），但网关不由本项目控制，不能作为唯一防线。
- **缩短单条 utterance（`MAX_CHUNK_MS` 12000 → 3000）**：单轮降到约 141 KiB，但改变断句与 STT 行为，是另一个产品取舍，不与本方案捆绑。
- **把音频历史放磁盘或按需重取**：常驻内存下降，但引入 I/O 与生命周期管理，且装配时仍需整份读入，收益主要在空闲内存而非请求体，与本缺陷无关。

## Consequences

- 所得（已落地）：示例配置下的稳态请求体从 2.15 MiB 降到 wire 常量以内；最坏常驻音频内存从修复前的 50 GiB 降到全局上限 256 MiB；体积类拒绝之后会话能在一次重试内自愈，文本轮不再被音频历史一起拖死；每轮省掉一次 base64 编码与整份历史 clone。
- 代价（已落地）：多轮音频历史比修复前更早被丢（装配层按 wire 预算先丢，存储层按 8 MiB 单会话与 256 MiB 全局再丢），模型的早期语音指代能力下降；失败路径上的收缩不可逆；第 5 条未采纳，历史仍保留多轮音频，不需要修订「历史里有什么等同模型看到什么」原则。
- 实现时需同步的文档：`docs/architecture.md:33` 已随常量改动同步（含 8 MiB、256 MiB、2 MiB、64 MiB 与 NapCat 不接入重试）；`website/docs/configuration.md:28`（中文源文只有 `omni_model` 一行，没有音频历史及其体积代价）与英文镜像 `website/i18n/en/docusaurus-plugin-content-docs/current/configuration.md:28` 仍未同步；`examples/config/settings.toml:17` 的 `max_context_turns` 注释也还没补音频历史受体积预算约束这一句。
- 英文镜像 `website/i18n/en/docusaurus-plugin-content-docs/current/configuration.md:28` 与中文源文结构一致（源文没有的 "Audio turns stay in context within `max_context_turns`" 一句已由本批另一任务删去），本方案不重复安排该差异，两条配置文档行按中英同步一起补。