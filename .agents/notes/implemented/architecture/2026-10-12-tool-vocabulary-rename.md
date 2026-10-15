# Agent Note: 代码侧 skill 词汇统一为 tool

Status: implemented

## Problem

同一个东西有两个名字。它在代码里叫 skill：`skills.rs` 定义 `Skill` trait 与 `SkillRegistry`，`SkillFactory`、`SkillContext`、`DEFAULT_SKILLS`、`get_allowed_skills`、`is_skill_allowed`、`SkillClip` 一路带下来，注释里也写「技能」。可它在线上的形态是标准的 function tool：`to_tool_schemas()` 生成 `{"type":"function","function":{name,description,parameters}}`，进请求体的 `tools` 字段，模型的 `tool_calls` 再按函数名派发回来。同一个概念，schema 生成与循环入口用 tool，注册表与权限用 skill，读代码要来回换算一次；`tool_loop`、`to_tool_schemas`、`ToolCall`、`ToolExecutor` 已经全是 tool 词汇，只剩注册表这半边没跟上。

## Decision

- 模块改名：`src/skills.rs` → `src/tools.rs`，`src/skills/` → `src/tools/`，`mod skills` → `mod tools`，`crate::skills::*` → `crate::tools::*`。
- 类型与函数改名：`Skill` → `Tool`，`SkillRegistry` → `ToolRegistry`，`SkillFactory` → `ToolFactory`，`SkillContext` → `ToolContext`，`DEFAULT_SKILLS` → `DEFAULT_TOOLS`，`get_allowed_skills` → `get_allowed_tools`，`is_skill_allowed` → `is_tool_allowed`，`list_skills` → `list_tools`，`execute_skill` → `execute_tool`；`ActiveTurn` 之外的音频片段类型 `SkillClip` → `ToolClip`，参数 `skill_scoped` → `tool_scoped`。`TurnRequest` 的 `allowed_skills` 字段与各调用点的同名局部变量一并改为 `allowed_tools`。
- 日志与模型可见文案跟上：`Tools registered`、`Tool '{}' disabled`、`Tool execution denied`、`Tool not found`、`Tool execution failed`，`NC allowed tools`。
- 技能名（`poke_client`、`send_message`、`music_control` …）、`website/` 文档与 issue 模板里的「技能」照旧。ACL 配置键从 `allowed_skills` 改为 `allowed_tools`（`examples/config/acl.toml` 一并更新），`AclRule` 字段写 `#[serde(rename = "allowed_tools", alias = "allowed_skills")]`，既有部署的旧键照常解析，`PermissionGate` 内部统一说工具。
- 内部文档同步：根 `AGENTS.md` 的目录树与 standing orders、`docs/architecture.md`（「技能系统」小节改为「工具系统」、mermaid 节点、`get_allowed_tools()`）、`.agents/notes/README.md` 里指向代码实体的那一句；顺带删掉 `docs/architecture.md` 与 `AGENTS.md` 中并不存在的 `permission/gate.rs`、`permission/` 条目。`website/` 未改（其中不出现该配置键，只讲技能名）。

## Alternatives considered

- **只改注释不改符号**：读代码仍要在 skill 与 tool 之间换算，注册表白叫 skill、线上字段叫 tools 的分裂照旧，等于没做标准化。
- **配置键只改一半：留旧名 `allowed_skills` 当正式键**：代码里最显眼的配置面继续用 skill，用户看不出工具化；新键配别名两边都能用，成本只是一个属性。
- **配置键改名且不给别名**：任何既有部署升级后 ACL 会静默失效（规则仍在，白名单读成空），属于安全相关的静默降级，必须留 `alias`。
- **`AclRule` 字段也叫 `allowed_skills`，只在文档里说明**：代码里最显眼的位置留着一个 skill，标准化的收益立刻打折；一个 serde 属性换字段名干净，成本比解释低。
- **保留 `skills.rs` 文件名只改类型名**：文件名是目录树、`mod` 声明、`use` 路径与文档里出现最多的一处，留着它等于每次都要解释。
- **引入 `Skill` 作为 `Tool` 的类型别名过渡**：YAGNI，仓库内没有外部使用者，别名只会成为下一个要清理的旧词汇。

## Consequences

- 所得：代码、日志、schema 生成、权限门、循环入口、ACL 配置键统一说工具；「模型可调用单元」这个概念在每层都是同一个词，唯一的 skill 残留是兼容旧键的 `alias`，由属性与注释显式标注。
- 代价：git 历史里 `src/tools.rs` 与 `src/tools/` 需要 `--follow` 才能追到改名前的提交；`acl.toml` 存在新旧两个可用键名，用户文档统一只讲 `allowed_tools`，读代码遇到旧键要认出它是别名。
- 模型可见文案变化：ACL 拒绝与工具缺失时返回 `Tool execution denied` / `Tool not found`（原为 `Skill …`），工具执行失败的补充文案同理；语义不变。
- 验证：`cargo fmt --check`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo test --all-targets --locked`（350 用例全绿）。`src/config/acl.rs` 两个用例钉住配置面：旧键 `allowed_skills` 仍填充 `allowed_tools` 字段，随包 `examples/config/acl.toml` 用新键可解析且含 `*` 规则。`AclConfig` 只被反序列化、从不写回磁盘，改正式键名不产生落盘差异。仓库内 `grep -rn "skill\|Skill\|技能" src/` 只剩 `AclRule` 的那条注释、serde 属性与兼容用例。

相关：[提示词改为代码内有序段落装配](../../implemented/architecture/2026-10-12-code-owned-prompt-composition.md) 里同步改了工具用法指引的归属措辞（`src/tools/communication.rs`）。