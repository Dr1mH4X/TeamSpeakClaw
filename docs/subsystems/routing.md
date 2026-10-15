# 事件路由与回合准入

事件路由与回合准入是入站事件到 LLM 回合之间的唯一入口层：三条入站路径（TeamSpeak 文本、NapCat OneBot 11、语音桥文本与音频）在这里完成触发判定、丢弃与回合门禁，随后把回合交给 `LlmEngine`。组合循环在 `router.rs` 的 `run_routers()`，触发策略在 `router/trigger.rs`，回合骨架与门禁在 `router/turn.rs`。

## 职责

- 组合路由循环：`run_routers()` 并发运行 `EventRouter`（TS 文本）与 `NcRouter`（QQ），并按关闭、TS 断线、NC 异常退出裁决会话结束原因。
- TS 触发策略：`resolve_ts_inbound()` 计算是否触发 LLM、剥离前缀后的文本与回复目标，`EventRouter`、`UnifiedInboundEvent` 与语音桥文本共用这一份判定。
- NapCat 准入：私聊按可信用户、群聊按 `listen_groups`、可信群与 @ 或触发前缀命中决定是否进入回合。
- 回合门禁：事件循环里同步占全局容量（`TurnPermit::reserve()`）并生成到达顺序票据（`LlmEngine::enqueue_turn_ticket()`），任务内先等前驱再取同会话串行锁（`TurnPermit::acquire_session()`）。
- 回合骨架：`TurnRequest` 提供输入与执行，`TurnSink` 提供回复落点；门禁之后的提示词装配、工具循环、失败分类与「送达成功才落上下文」由 `router/turn.rs` 承担。
- 调用者身份键：`ts_caller_admission_uid()` 与 `ts_caller_session_uid()` 区分准入键与会话历史键，三条入口共用。

## 关键路径与序列

1. `EventRouter::run()` 从 TS 事件流取事件，`precheck_text_turn()` 同步丢弃 bot 自身与音乐 bot 的消息、bridge 就绪且语音特性开启时交给语音桥的文本、未触发文本、空文本与超出用户文本上限的文本；其余消息先 `TurnPermit::reserve()` 占容量（满则丢弃且不派发任务），再 `enqueue_turn_ticket()` 发票据，任务内取到 `TurnSession` 后进入 `handle_message()`。
2. `NcRouter::run()` 先按可信用户、`listen_groups` 与群触发条件过滤，再做同一套 `precheck_text_turn()` 预检，经 `spawn_handle()` 占容量、发票据、取锁后进入 `handle_text()`。
3. 语音桥文本在事件循环里由 `prepare_chat()` 完成同类预检，随后占容量、发票据并在派发任务前登记回合；任务内 `ChatTurnAdmission::prepare()` 先取会话锁，再查询 caller 与在线名单，两次快照里的非空 UID 都必须与事件一致，最后进入文本回合。
4. 三条路径都以 `TurnRequest`（`source`、`user_ctx`、`allowed_tools`、`TurnInput::{Text, Audio}`、`callbacks`、`cancel`）加各自的 `TurnSink` 调用 `TurnRequest::run()`：文本经 `build_messages` / `save_turn`，音频经 `build_omni_messages` / `save_omni_turn`，回复由 sink 送达，只有送达成功才落上下文。

## 不变量

- 准入判定与容量占位都在事件循环里同步完成，不需要回合的消息不得先占容量再排队等锁，否则等待中的回合会耗尽全局容量，把其他会话的有效消息挤成丢弃。
- 同会话的回合不得并发超过一条，且取锁顺序等于事件循环里的到达顺序；票据必须与容量占位在同一处取得，漏发即退化为任务调度顺序。
- 容量占位、会话锁与票据由 `TurnSession` 一起持有到回复落库完成，释放顺序为容量、锁、票据。
- 准入键只能取自事件本身（事件 UID，缺失时退到 `clid:<id>`），语音桥文本的准入键不等待 caller 解析结果；会话与历史键用解析出的真实 UID，因为 `clid` 会被复用，准入键不得当历史键，两者也都不得落空串。
- 回复未送达的回合不落上下文，被取消的回合既不回复也不落库。

## 相关决策

- [LLM 回合骨架收敛为 `router/turn.rs` 单一归属](../../.agents/notes/implemented/architecture/2026-10-03-llm-turn-module.md) — 回合策略、失败分类与回复落点在三处的分工。
- [同会话回合按到达顺序取锁（到达顺序票据）](../../.agents/notes/implemented/bug-fix/2026-10-04-turn-arrival-order-tickets.md) — 票据链与准入同步门在事件循环里的位置。
- [TS 调用者身份键：准入键与会话键分离](../../.agents/notes/implemented/bug-fix/2026-10-15-ts-caller-identity-keys.md) — 三枚身份键函数与各入口的用法。