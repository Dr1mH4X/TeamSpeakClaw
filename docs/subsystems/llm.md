# LLM 引擎与上下文

`src/llm.rs` 聚合 `llm/{engine,provider,context,tool_loop}.rs`，是 OpenAI 兼容的模型访问、上下文预算与工具循环的唯一归属地：它把回合骨架装配好的 system 提示词与历史发成流式请求，驱动模型与工具往返，并维持每会话的对话与音频历史。配置入口是 `config/llm.rs` 的 `LlmConfig`，`max_context_turns` 控制历史轮数上限，`omni_model` 开启时音频以 `input_audio` content 直送模型。TS 文本、NapCat 与语音三条入站路径都经 `router/turn.rs` 的回合骨架进入这里，回合准入、门禁与提示词装配另有归属。

## 职责

- 对任意 `base_url/chat/completions` 发起 OpenAI 兼容的流式对话请求，解析 SSE 增量、工具调用与结束原因；`reasoning_content` 不保存也不转发。
- 装配请求 messages：system 角色、历史轮（文本轮回放原文、音频轮回放 `input_audio` content）与当前轮内容。
- 驱动工具循环：把模型的工具调用交给 `ToolExecutor` 执行，结果以 tool 角色回填后继续请求，直到模型给出不带工具调用的回复。
- 维持每会话的对话历史与音频字节预算，提供按 wire 体积裁剪的装配视图，以及请求被拒时的收缩与还原。
- 归类一次模型调用的可识别失败：发送前预检与网关的体积类、上下文窗口类拒绝，以及零输出的完成。分类经 `anyhow::Error` 原样透传，回合骨架用 `downcast_ref::<LlmFailure>()` 取变体决定动作，不解析错误文案。

## 关键路径与序列

1. 回合骨架调用 `LlmEngine::build_messages` 或 `build_omni_messages`：先放 system 提示词，再按 `ContextWindow::history_for_wire` 取 wire 预算内的历史后缀，最后拼当前轮。
2. `LlmEngine::run_tool_loop` 进入 `llm/tool_loop.rs` 的 `run_tool_loop`；`accumulate_stream` 经 `LlmProvider::chat_completion_stream`（实现为 `OpenAiProvider`）取流，Token 累积文本并触发 `StreamCallbacks::on_text_token`，Done 带回 `finish_reason` 与工具调用；终止事件既没有可见文本也没有工具调用时归类为 `LlmFailure::EmptyResponse`，不产出 Done。
3. 无工具调用即返回 `ToolLoopResult`；有工具调用则校验每轮与累计的工具调用数、实参字节上限，把 assistant 工具调用消息与每个 tool 结果追加进 messages 后再请求下一轮，工具执行前触发 `on_tool_call_start`，达到 `MAX_TOOL_LOOP_TURNS` 仍要调用工具即返回 `MaxTurnsExceeded`。
4. Provider 先 `serde_json::to_vec` 序列化请求体并按 `MAX_REQUEST_BODY_BYTES` 预检；非 2xx 走 `http_response_error`：413 直接归体积类，400 读最多 `MAX_ERROR_BODY_BYTES` 的响应体后先按上下文窗口文案、再按体积文案归类，其他状态不读正文。
5. 两类可收缩失败经 `anyhow::Error` → `ToolLoopError::Other` 原样透传，回合骨架用 `LlmFailure::is_shrinkable` 判定后调用 `LlmEngine::shrink_for_rejection_retry`，删除可见的最早音频轮与整个不可见前缀并重试一次；重试以取消结束时用 `restore_history` 放回。
6. `LlmFailure::EmptyResponse` 不参与收缩：回合骨架记一条 warn 日志后以 `Ok` 收尾，不送达回复、不落库，也不送兜底文案。工具执行后的空完成同样如此：它被包进 `ToolLoopError::AfterToolExecution`，分类不变，处置也不变。

## 不变量

- 同一会话的历史以「user+assistant 一轮」为单位成对增删；`max_context_turns`、单会话与全局音频预算、wire 预算都不能清空会话历史或丢到最新一轮音频之外。
- 存储历史与装配视图分离：`history_for_wire` 只决定模型实际收到的 messages，裁剪不改动存储；文本轮不占音频预算，落在可见后缀起点之前的文本与音频一并不可见，最新音频永不丢。
- 工具执行后的模型错误归 `ToolLoopError::AfterToolExecution` 并直接结束本轮，不得当作可收缩失败重试，否则已发生的工具副作用会被重放。
- 收缩与装配共用同一可见后缀起点，删除最早可删的可见音频轮加整个不可见前缀，保证重试请求严格变小；只剩一条可见音频时不动历史也不重试。
- 收缩只在重试被取消时还原（取消的回合不回复也不落库），其他失败保留收缩，否则下一回合会重复同样的体积错误。
- 音频轮的 wire 体积估算必须与 `engine` 侧 `input_audio` content 的实际序列化逐字节一致，估算漂移会让装配超出 wire 预算。
- 失败分类的每个变体都在回合骨架上有对应动作：两类可收缩拒绝走收缩重试，空完成走日志收尾，其余失败走固定兜底文案。
- 空完成是「无回复的完整回合」：不送达、不落库、不触发收缩，也不区分工具是否已执行；日志里的分类是它唯一的痕迹。
- 分类文案不携带网关响应体，正文只作归类判据：正文可能含凭据等敏感内容，而分类会进日志。

## 相关决策

- [omni 音频轮进上下文历史](../../.agents/notes/implemented/feature/2026-10-02-omni-audio-context-history.md) — 音频轮以 WAV 字节进历史、装配为 `input_audio` content，以及成对逐轮丢弃的存储形态。
- [omni 音频历史的 wire 体积预算与会话自愈](../../.agents/notes/implemented/feature/2026-10-03-omni-audio-history-wire-budget.md) — 装配层 wire 预算、单会话与全局音频上限、发送前预检与体积归类的取值与分工。
- [体积重试的安全收缩与取消还原](../../.agents/notes/implemented/bug-fix/2026-10-15-payload-retry-recovery.md) — 收缩与装配共用可见后缀起点、快照式还原，以及工具执行后错误不纳入重试。
- [模型调用失败按分类路由](../../.agents/notes/implemented/architecture/2026-10-15-llm-failure-taxonomy.md) — 归类在 provider、恢复在回合骨架的归属分工，上下文窗口类拒绝的归类口径，以及空完成的收尾方式。
- [LLM 回合骨架收敛为 `router/turn.rs` 单一归属](../../.agents/notes/implemented/architecture/2026-10-03-llm-turn-module.md) — 体积自愈的调用点归回合骨架，本子系统只提供引擎侧入口。