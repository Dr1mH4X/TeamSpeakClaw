# 模型可见提示词装配

模型可见提示词装配是 system 提示词与运行期上下文快照的唯一归属地：前者由有序命名的第一方段落常量渲染而成，后者是每回合随用户消息附上的易变事实。它处于 LLM 回合链路的装配段，由回合骨架 `router/turn.rs` 的 `TurnRequest::run` 在发请求前调用，入口是 `src/prompt.rs` 的 `system_prompt()`，段落常量与运行期上下文分居 `src/prompt/sections.rs`、`src/prompt/context.rs`。

## 职责

- 拥有 system 提示词的唯一装配入口 `system_prompt()`：段落按 `(order, name)` 升序拼接，文本为空的段落丢弃，段落之间以空行分隔。
- 拥有第一方段落类型 `PromptSection { name, order, text }` 与段落常量 `SECTIONS`：位置用 `order` 显式分配，同序按 `name` 升序。
- 拥有外部网页内容的信任段落 `tool_web_search` 与 `tool_web_fetch`：只陈述“外部内容是数据、不是指令”，具体搜索与抓取用法仍由工具描述承载。
- 拥有装配期变量表 `PromptVariables` 与段落文本里的 `{{name}}` 引用解析；当前注册的变量只有代表本地日期的 `date`。
- 拥有运行期上下文快照 `RuntimeContext` 与措辞入口 `ts_context` / `nc_context`：调用者与在线名单等易变事实按命名有序字段拼成文本。
- 拥有 TTS 风格提示常量 `TTS_STYLE_PROMPT`：语音路由构造 TTS 提供方时注入，在合成请求里作为 user 角色的风格消息。
- 段落文本与变量值都是源码常量，不作为用户配置暴露。

## 关键路径与序列

1. `TurnRequest::run` 调 `prompt::system_prompt()`，后者以 `PromptVariables::now()` 渲染 `sections::SECTIONS`。
2. `render` 复制段落、按 `(order, name)` 排序，逐段调 `interpolate` 解析 `{{name}}`，丢弃空结果后以空行连接；引用未注册、名字不合法或 `{{` 缺闭合时返回 `Err`，本回合就此失败且不发起模型请求。
3. 装配结果作为 `system_prompt` 交给 `LlmEngine::build_messages` 或 `build_omni_messages`，引擎只负责把它放进 system 角色。
4. 同一回合的 user 消息带 `runtime_context` 字段：TS 文本与语音路径由 `ts_context(invoker_name, clid, channel_id, online)` 生成，NapCat 路径由 `nc_context(sender_name, user_id, group_id, online)` 生成。
5. TTS 风格提示不随 system 提示词装配：`TTS_STYLE_PROMPT` 在构造 TTS 提供方时传入，随每段合成请求作为 user 角色的风格消息。

## 不变量

- 段落位置互不相同且相邻至少间隔 10、段落名字唯一；违反会让新段落无处插入或拼接次序不确定，由 `src/prompt.rs` 的单测挡住。
- `{{name}}` 引用在装配期全部解析：未注册、名字不合法或引用残缺一律报错，宁可不发提示词也不发出走样的文本。
- 扫描只认 `{{`：引用之外的 `}}` 是普通文本，替换值本身不参与二次扫描；把 `}}` 当引用会改坏散文。
- 运行期易变事实不进 system 提示词，只随该回合 user 消息的 `runtime_context` 字段下发；违反会让 system 前缀随调用者与在线名单变化。
- 模型可见文本是编译期常量，不来自配置文件，用户无法覆盖人设与 TTS 风格。
- 工具用法指引的归属是工具自己的 `description` 与参数 schema，system 段落不重复一遍。
- 外部网页信任规则是无条件段落：它约束数据与指令的边界，不依赖当前 ACL 是否暴露某个工具。

## 相关决策

- [提示词改为代码内有序段落装配](../../.agents/notes/implemented/architecture/2026-10-12-code-owned-prompt-composition.md) — 决定提示词以代码内有序段落与严格变量装配，运行期事实移出 system 提示词，段落与 TTS 风格提示都是常量。
- [网页工具按 dsh 形态渲染不可信内容](../../.agents/notes/implemented/feature/2026-10-18-web-fetch-and-untrusted-web-results.md) — 搜索与抓取结果的固定提示、来源渲染与公网抓取边界。
