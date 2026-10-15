# Agent Note: LLM 回合骨架收敛为 `router/turn.rs` 单一归属

Status: implemented

## Problem

四个调用点各写一遍回合骨架：`ts_router::handle_message`（`src/router/ts_router.rs:147`）、`nc_router::handle_text`（`src/router/nc_router.rs:223`）、`handle_omni_audio_chunk`（`src/router/voice_router.rs:1179`）与 `handle_user_input`（`src/router/voice_router.rs:1266`）。门禁顺序、消息装配分支、体积类失败自愈、`LLM_ERROR_REPLY`、`save_turn` 时机、错误上报各自复制一份。

旧结构把重复压成两个入口：`run_llm_turn`（`source` 为 `None`、不做自愈）与 `run_llm_turn_with_audio_recovery`，两者只差一个 `Option<&SessionSource>`；`TurnLoopSpec` 的存在只为让带自愈的入口参数个数落在 clippy 阈值内，而保留逐项签名的代价写在当时的注释里——nc 按原样调用，于是 NapCat 拿不到体积自愈。

后果之一是可见的不一致：nc 的 `run_llm` 把 `LLM_ERROR_REPLY` 当正常回复返回，`handle_text` 随后把它落进上下文；ts 与 voice 在失败回合都不落上下文。

## Decision

- 新增 `src/router/turn.rs`，模块拥有回合的全部策略：容量占位（`TurnPermit::reserve`，在事件循环里同步取，满即丢消息不 spawn）、同会话到达顺序票据（`LlmEngine::enqueue_turn_ticket`，同样在事件循环里同步取）与会话串行锁（`TurnPermit::acquire_session` 内先等前一条同会话回合结束再取，见[同会话回合按到达顺序取锁](../../implemented/bug-fix/2026-10-04-turn-arrival-order-tickets.md)）、工具白名单与工具循环、体积类失败收缩重试（`MAX_PAYLOAD_RETRIES = 1`，`src/router/turn.rs:21`）、`LLM_ERROR_REPLY`（`src/router/turn.rs:18`）的送达，以及「只有送达成功才落上下文」（`deliver`，`src/router/turn.rs:144`）。
- 兜底文案在工具循环失败时经 `TurnSink` 送达同一条回复落点（`src/router/turn.rs:132`），四条入站路径共用一份文案，不再由各调用点自己发。两种失败不发兜底：插话取消（`TurnError::Cancelled`）与正文投递失败（`TurnError::ReplyFailed`，此时回退补发只会把另一条消息推给同一个已经失败的落点）。兜底文案本身送不出去时只记一条 warn，回合仍以 `TurnError::Failed` 交回调用点。
- 调用点提供回合请求（`TurnRequest`，`src/router/turn.rs:60`：llm / registry / source / user_ctx / allowed_tools / `TurnInput` / callbacks / cancel）、一个 `UnifiedExecutionContext` 构造闭包，和一个回复落点 `TurnSink`（`src/router/turn.rs:32`）：TS 文本（`TsTextSink`，`src/router/ts_router.rs:284`）、NapCat 发送（`NcReplySink`，`src/router/nc_router.rs:364`）、语音桥通知（`VoiceNoticeSink`，`src/router/voice_router.rs:1556`）。
- `TurnInput::{Text, Audio}`（`src/router/turn.rs:25`）一个分支同时决定消息装配（`build_messages` / `build_omni_messages`，`messages` 在 `src/router/turn.rs:160`）与落库（`save_turn` / `save_omni_turn`），两处不可能再错配。
- 门禁句柄不再对外暴露：`src/llm.rs` 的 `TurnCapacityPermit` / `TurnSessionGuard` re-export 删除，只能经 `TurnPermit` 与 `TurnRequest::run` 使用。
- 失败分三类（`TurnError`，`src/router/turn.rs:37`）：`Cancelled`（插话，不回文案不落库）、`Failed`（后端失败，固定文案已尽力送达）、`ReplyFailed`（投递失败，未落库）。调用点按各自既有语义处理：ts/nc 记日志，voice 向上抛给外层 `voice turn failed` 日志。
- TTS 收尾留在 voice 调用点（`result.is_ok()` → `finish`，否则 `abort`），不给 module 加 settle 钩子。
- 输入体积预检（`check_user_text_bounds`）与其余同步准入判断（bot 自身与音乐 bot 的消息、桥接文本、未触发文本、空文本）在调用点、且都在容量占位与排队之前完成（ts/nc 的 `precheck_text_turn`）；voice 必须在预检之后才 `build_tts_callbacks` + `mark_producing`，否则超限消息会先登记成「正在产出」，可能顶掉同 clid 的后续语音。

## Alternatives considered

- **只抽回复与落库策略、保留两个入口**：两个入口的差别只是 `source`，留下它就得继续维护 `Option<&SessionSource>` 分支与 `TurnLoopSpec` 这层纯为参数计数的包装；合并成一处不需要额外实体。
- **`TurnSink` 改成交互式回调或通道枚举**：module 就要知道每家的目标语义（TS 的 `reply_mode`/`target`、NapCat 的 @ 段与群/私聊分支、语音桥的 `target_mode`/`client_id`），等于把三家 adapter 细节搬进 module；sink 让「怎么送达」留在 adapter 侧。
- **把执行上下文当值传入，或让调用点自己实现 `ToolExecutor`**：原 `TurnExecutor` 就是为消灭各家薄包装而存在；保留内部 executor + 一次闭包注入，调用点仍只提供一处上下文构造。
- **module 兼管体积预检**：预检与 TTS 副作用（`build_tts_callbacks` / `mark_producing`）的先后顺序是 voice 语义的一部分，见上；预检留在调用点（三处各一行），module 只管预检之后的事。
- **为 TTS 收尾加 settle 钩子（成功回调 / 失败清理）**：只有 voice 一家需要，播放取消位是回合级资源，钩子会为单一调用方扩大 module 接口。
- **`ReplyFailed` 并入 `Failed`**：投递失败不是后端失败；合并后 ts 的「静默跳过落库」、nc 的 `error!`、voice 的向上抛必须共用一种语义，且日志会把投递失败标成 LLM 错误。一个变体换三者语义不变更便宜。

## Consequences

- 所得：回合骨架一处实现，调用点只剩「准备提示 + 组装请求 + 定义 sink + 处理错误」；NapCat 拿到与 ts/voice 相同的自愈入口（其会话不存音频时 `drop_oldest_audio_turn` 返回 `false`、立即失败，不产生额外请求）；`src/llm.rs` 不再导出两个门禁句柄。
- 行为变化（有意，共四处）：① nc 后端失败不再把 `LLM_ERROR_REPLY` 落进上下文，与 ts/voice 对齐；② 投递失败一律不落上下文，ts/nc 各记一条 warn（ts 原先静默跳过）；③ 空回复（有 `finish_reason` 但零 token）不再发送也不再写入历史，ts/voice 原本就是跳过、nc 原先会发一条空消息并把空 assistant 轮落进上下文（`TurnRequest::run` 的 `!result.content.is_empty()` 分支）；④ voice 后端失败的顺序改为「先送错误文案、再 abort TTS 流」（原先 abort 在前），语音桥通知与 replay 回执共用 `send_notice`（`src/router/voice_router.rs:1529`）。
- 代价：`TurnRequest` 有 9 个字段，是一个请求对象而非 adapter，调用点仍需逐字段填一次；`TurnError` 要求调用点 match 一次；`TurnPermit` 的字段只为把占位持有到回合结束（容量释放靠 Drop）。
- 验证：`cargo fmt --check`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo test --all-targets --locked`（317 用例全绿）。`src/router/turn.rs` 的 12 个用例用 `RecordingSink` 与脚本化 `FakeProvider` 覆盖：文本轮落库、音频轮落库、投递失败不落库且不再补发兜底、后端失败回固定文案且不落库、兜底文案也送不出去时仍以 `Failed` 交回调用点、空回复不发不落、非体积类失败不收缩历史、取消不回不落、体积收缩丢最早音频轮并重试一次、重试后请求体变小、重试上限 1 次、无音频可丢不重试。实机需确认 voice 的 TTS 收尾时序与三条失败文案。

相关：[音频历史的线上体积预算](../../proposed/feature/2026-10-03-omni-audio-history-wire-budget.md) 记录的体积自愈在本轮换了归属（`src/router.rs` → `src/router/turn.rs`）。