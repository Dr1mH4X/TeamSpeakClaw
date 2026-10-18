# 工具系统与权限门

工具系统把模型可调用的函数统一为 `Tool` 并在 `ToolRegistry` 注册，权限门 `PermissionGate` 按 `acl.toml` 的规则判定调用者能拿到哪些工具、能否对受保护目标动手。它处在 LLM 工具循环与工具实现之间：回合据白名单生成请求体的 `tools` 字段，模型回传的调用再经同一白名单校验后派发到具体工具。代码入口是 `src/tools.rs`、`src/tools/` 与 `src/permission.rs`。

## 职责

- `Tool` trait 与 `ToolRegistry`：工具以 `name`、`description`、`parameters` 暴露给模型，注册表按 ACL 白名单过滤出 schema 并执行调用。
- `UnifiedExecutionContext` 与 `ToolContext`：前者是单次调用的上下文（平台、调用者身份与组、TS/NapCat 适配器句柄、gate、config），后者是装配期工具依赖的统一来源。
- `PermissionGate`：把 `acl.toml` 的规则匹配成工具白名单，并对受保护组给出目标授权判定。
- 工具分组：`communication`、`information`、`moderation`、`music`（后端 `ts3audiobot`、`tsbot_http`、`tsmusicbot`）、`web_search`、`voice_audio`、`voice_replay`。
- 工具间共享设施：按超时参数化复用的 HTTP 客户端，以及聊天类音乐后端的串行锁。
- `WebSearch` 在注册时读取搜索服务选择，并按所选服务生成参数 schema；执行仍经过相同的 ACL 与结果包装。配置用法见[用户配置指南](../../website/docs/configuration.md#联网搜索)。

## 关键路径与序列

1. `main.rs` 经 `ToolRegistry::with_defaults` 遍历 `DEFAULT_TOOLS` 工厂表建注册表；每个工具先过 `should_register()`，`music_control` 只在配置的后端有效时注册，`voice_replay` 只在 `voice_replay.enabled` 时注册。
2. `ts_router`、`nc_router`、`voice_router` 各自用调用者的服务器组与频道组调 `PermissionGate::get_allowed_tools` 取白名单（NapCat 调用者的频道组按 0 传入）。
3. `router/turn.rs` 的 `TurnRequest` 用 `ToolRegistry::to_tool_schemas` 生成按名排序、已过滤的 schema 交给引擎；模型回传 `ToolCall` 后由 `TurnExecutor` 调 `ToolRegistry::execute_tool`。
4. `execute_tool` 先复检 `is_tool_allowed`，再取工具并 `execute`；成功值序列化为字符串，拒绝、工具缺失与执行失败各自返回固定文案给模型。
5. 工具内部再各自判定自身约束：`moderation` 用 `ctx.gate.can_target` 判定受保护目标，`voice_replay` 的直呼命令经 `direct_command_allowed` 复用同一白名单。

## 不变量

- 模型看到的工具定义来自 `Tool` 的 `name`、`description` 与 `parameters`，不来自 system 段落，工具的用法指引写在工具自己的描述与参数 schema 里。
- 生成 schema 与执行调用共用 `is_tool_allowed`，白名单中的 `*` 表示放行全部工具。
- 执行期必须重新校验白名单；只按 schema 过滤会让模型能调到未暴露的工具。
- 白名单只由 `acl.toml` 规则叠加得出，没有规则匹配时为空，不回落为默认放行。
- 目标组含 `protected_group_ids` 时，`can_target` 只认显式 `can_target_admins` 规则，否则拒绝该交互。
- 工具以 `Result` 表达失败，`execute_tool` 把失败转成文案回给模型而不中断工具循环；TeamSpeak 专属工具在 NapCat 平台显式报错，不做平台降级。

## 相关决策

- [代码侧 skill 词汇统一为 tool](../../.agents/notes/implemented/architecture/2026-10-12-tool-vocabulary-rename.md) — 统一类型名、日志文案与 ACL 配置键的工具词汇，并保留旧键别名。
- [提示词改为代码内有序段落装配](../../.agents/notes/implemented/architecture/2026-10-12-code-owned-prompt-composition.md) — 确定工具用法指引由工具自身承载，system 段落不重复。
- [联网搜索服务显式选择](../../.agents/notes/implemented/feature/2026-10-17-optional-parallel-search.md) — 搜索服务由配置选择，参数 schema 与服务能力一致。
