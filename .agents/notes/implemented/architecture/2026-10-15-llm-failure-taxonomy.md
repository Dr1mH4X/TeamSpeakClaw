# Agent Note: 模型调用失败按分类路由

Status: implemented

## Problem

一次模型调用会以多种方式失败，而调用点真正需要的信息只有一件：这次失败该触发什么动作。失败以 `anyhow::Error` 的文案形式向上传时，回合骨架只能看出「失败了」，于是所有失败共享一条固定兜底文案——包括那些本可以自愈的失败（请求被网关按体积或上下文窗口拒绝，而会话里还有可以丢掉的音频轮），也包括那些不该被当成后端故障的完成（模型以 `stop` 结束却什么都没输出）。

判定一旦落在文案上就立刻失稳：网关的错误句子随厂商、语言、版本变化，任何一次改动都会让自愈悄悄失效，而失效没有任何门禁能发现。归类必须是编译期可检查的东西。

## Decision

`src/llm/provider.rs` 的 `LlmFailure` 是模型调用失败的分类闭集合，三个变体：`PayloadTooLarge { message }`（发送前预检拒绝，或网关以 413、或 400 带体积文案拒绝）、`ContextWindowExceeded { message }`（网关以 400 带窗口文案拒绝）、`EmptyResponse { finish_reason }`（终止事件既没有可见文本也没有工具调用）。

归类在 provider 侧完成，恢复在回合骨架完成。`OpenAiProvider` 把失败作为 `LlmFailure` 交回，经 `anyhow::Error` → `ToolLoopError::Other` 原样透传；`src/router/turn.rs` 用 `downcast_ref::<LlmFailure>()` 取变体，`LlmFailure::is_shrinkable` 认下前两类，第三类有自己的动作。Rust 的变体匹配就是「按稳定分类路由、不解析文案」的形态，而且更严：漏掉一个变体是编译错误，不是运行期意外。

三条归类口径：

- 体积类：413 一律算；400 需要响应体含 `too large` 或 `请求体过大` 佐证，避免把参数错误误判成体积类。
- 窗口类：必须点名模型上下文（`context length`、`context window`、`context_length`、`context_window`、`model context`、`model's context`、`maximum context`、`max context`、`上下文长度`、`上下文窗口`），再叠加越界或上限信号（`exceed`、`overflow`、`too long`、`too large`、`reduce the length`、`超出`、`超限`、`超过`）；上限式说法本身已点名上下文，无需越界动词。只认这些组合，是为了不让「message is too long」这类参数错误也走收缩。
- 空完成：只有终止事件既没有可见文本也没有工具调用时成立。判定集中在 `classify_terminal_completion`，只吃「是否送出过可见文本、工具调用、终止原因」三个事实；`reasoning_content` 不算可见文本。`tool_calls` 却零调用属于协议违约，仍由工具循环报错，不并入这一类。

两类可收缩拒绝共用一条恢复：`LlmEngine::shrink_for_rejection_retry` 删除可见的最早音频轮与整个不可见前缀，`TurnRequest::run` 在 `MAX_SHRINK_RETRIES = 1` 的循环里重建 messages 并重试一次。窗口类与体积类共用这条恢复是刻意的：两种请求都靠丢掉最早音频轮同时变小（字节与 token 一起降），没有必要为窗口类另造一套裁剪。

空完成不走恢复：回合骨架记一条 warn 日志后以 `Ok` 收尾，不送达、不落库，也不送兜底文案。一次没有回复的回合仍然是完整回合；分类的用途是让这个事件可诊断，而不是把它变成用户可见的故障。空完成不看工具是否执行过：工具循环把执行后的模型错误包进 `AfterToolExecution`，`failure_class` 同时认这两个变体，因为空完成既不可收缩、也不需要那样的重放保护。

分类文案只带状态码与分类事实，不带网关响应体：正文只作归类判据，因为它可能含凭据等敏感内容，而分类会进日志。

## Alternatives considered

- **失败只留文案，靠文案匹配决定动作**：网关句子随厂商、语言、版本变化，匹配失效时自愈会悄悄消失，且没有任何门禁能发现；枚举让归类在编译期可见。
- **把归类放在回合骨架**：只有 provider 知道 HTTP 状态、响应体与终止事件的形状；放到调用点意味着要么把 wire 细节再抄一遍，要么把原始响应体透传上去，后者会把敏感正文带进日志候选。
- **窗口类拒绝按普通 400 处理（只报错、不收缩）**：多模态与语音历史最容易撞窗口，而收缩恰好是唯一能一次同时降低字节与 token 的手段；放弃它就是把可自愈的失败变成用户可见的兜底文案。
- **给窗口类单独造一条只裁文本历史的恢复路径**：现有收缩已保证「删除最早可删的可见音频轮加整个不可见前缀」，再按 token 单独裁一遍会引入第二套可见性口径，两套口径漂移的风险高于收益。
- **空完成送兜底文案**：零输出不是网关故障，兜底文案会把用户误导成服务不可用；模型偶尔没有可补充的内容时，这个方案还会把正常沉默报成故障。
- **先把限流、5xx、传输超时、鉴权、额度都塞进分类**：这些类别当前没有各自的恢复动作，加入只会得到没有消费点的变体；分类新增变体必须同时落一个动作。

## Consequences

- 所得：被拒绝的请求在日志里有分类，收缩重试另记一条 warn，历史丢弃可追溯；体积类与窗口类拒绝共用一条收缩自愈路径，语音长会话不会因为网关报窗口就直接失败。
- 边界：除这两类拒绝以外的一切失败（限流、5xx、传输与超时、鉴权、额度）仍是单次尝试加固定兜底文案；空完成是唯一「失败但静默收尾」的分支。
- 归类是保守的：宁可漏判（退回兜底文案）也不误判成可收缩。漏判的代价是丢掉一次本可自愈的机会，误判的代价是丢历史并多做一次请求。
- 风险：窗口类依赖网关文案。自建反代返回自定义页时归类不到，退化为一次普通失败；发送前预检与 `max_context_turns` 才是主防线，归类只作兜底。
- 验证：`classifies_context_window_rejections_by_their_wording`、`keeps_context_lookalikes_out_of_the_context_window_class`、`context_wording_wins_over_the_size_marker_it_carries`、`http_400_body_preserves_rejection_classification_without_disclosure` 覆盖归类口径与「正文不进文案」；`classifies_an_empty_completion_as_a_failure`、`a_completion_with_text_or_tool_calls_is_not_empty` 覆盖空完成判定；`context_window_rejection_shrinks_and_retries_like_a_payload_rejection`、`classified_empty_completion_ends_the_turn_without_a_reply`、`empty_completion_after_a_tool_call_ends_the_turn_without_a_reply`、`empty_completion_does_not_shrink_the_history` 覆盖两条动作。

相关：[LLM 引擎与上下文](../../../../docs/subsystems/llm.md) 是分类与恢复序列的契约归属；[体积重试的安全收缩与取消还原](../../implemented/bug-fix/2026-10-15-payload-retry-recovery.md) 记录收缩与装配共用可见后缀起点、快照式还原；[omni 音频历史的 wire 体积预算与会话自愈](../../implemented/feature/2026-10-03-omni-audio-history-wire-budget.md) 记录两层预算与体积归类的取值。