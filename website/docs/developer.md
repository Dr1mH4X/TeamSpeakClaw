---
sidebar_position: 5
---

# 开发者指南

本指南帮助贡献者快速理解 TeamSpeakClaw 的代码架构并开始开发。

## 快速开始

### 环境要求

- Rust 1.75+ (edition 2021)
- Git
- 可选：TeamSpeak 服务器（用于连接测试）

### 克隆与构建

```bash
# 克隆仓库
git clone https://github.com/Dr1mH4X/TeamSpeakClaw.git
cd TeamSpeakClaw

# 构建项目
cargo build

# 运行（开发模式）
cargo run -- --config generate   # 生成默认配置
cargo run                        # 启动机器人
```

### 配置开发环境

首次运行需生成配置文件，详见 [配置指南](/docs/configuration)

## 项目架构

### 目录结构

```
src/
├── main.rs           # 入口：初始化组件，启动事件循环
├── cli.rs            # 命令行参数解析与配置向导
├── router.rs         # 事件路由：协调各模块处理用户消息
├── adapter/          # TeamSpeak 适配器层
│   ├── mod.rs
│   ├── connection.rs # 连接管理（TCP/SSH）
│   ├── command.rs    # 命令构建
│   └── event.rs      # 事件解析
├── config/           # 配置加载与验证
│   └── mod.rs
├── llm/              # LLM 集成层
│   ├── mod.rs
│   ├── engine.rs     # LLM 引擎封装
│   └── provider.rs   # 提供者 trait 与 OpenAI 实现
├── permission/       # 权限控制
│   ├── mod.rs
│   └── gate.rs       # 权限门控逻辑
└── skills/           # 技能系统
    ├── mod.rs        # 技能 trait 与注册表
    ├── communication.rs
    ├── information.rs
    ├── moderation.rs
    └── music.rs
```

### 数据流

```
用户消息 → TsAdapter → EventRouter → LlmEngine
                                        ↓
                                   工具调用请求
                                        ↓
                              PermissionGate (权限检查)
                                        ↓
                                  SkillRegistry → Skill.execute()
                                        ↓
                                   执行结果 → LlmEngine → TsAdapter → 回复用户
```

### 跨平台行为矩阵（当前实现）

| Skill | TS 入口 | NC 入口（默认） | NC 入口 + `ts_route=true` |
|---|---|---|---|
| `poke_client` | ✅ TS 执行 | ❌（未实现 NC execute_nc） | ❌ |
| `send_message` | ✅ `private/channel/server` | ✅ `private/group`（NapCat 原生发送） | ✅ 路由到 TS（`private/channel/server`） |
| `kick_client` | ✅ TS 执行 | ❌（未实现 unified/NC） | ❌ |
| `ban_client` | ✅ TS 执行 | ❌（未实现 unified/NC） | ❌ |
| `move_client` | ✅ TS 执行 | ❌（未实现 unified/NC） | ❌ |
| `get_client_list` | ✅ TS 执行 | ✅ 查询 TS 在线缓存并回传 | 不适用 |
| `get_client_info` | ✅ TS 执行 | ✅ 查询 TS 在线缓存并回传 | 不适用 |
| `music_control` | ✅ TS 执行 | ✅ NC 请求转发到 TS | 不适用 |

说明：
- NC 侧统一执行遵循“先 `execute_unified`，失败再回退 `execute_nc`”。
- TS 侧统一执行遵循“先 `execute_unified`，失败回退 `execute`”。
- NC 权限通过 ACL 虚拟组映射（`9000~9003`）实现，详见配置文档。

## 核心模块详解

### adapter — TeamSpeak 适配器

负责与 TeamSpeak 服务器的底层通信。

**核心结构**：`src/adapter/connection.rs:153-158`

```rust
pub struct TsAdapter {
    writer: Mutex<tokio::io::WriteHalf<TsStream>>,
    event_tx: broadcast::Sender<TsEvent>,
    bot_clid: AtomicU32,
}
```

**连接方式**：
- TCP（默认）：`method = "tcp"`
- SSH：`method = "ssh"`

**事件类型**：`src/adapter/event.rs`

| 事件 | 说明 |
|---|---|
| `TsEvent::TextMessage` | 收到文本消息 |
| `TsEvent::ClientEnterView` | 用户进入可视范围 |
| `TsEvent::ClientLeftView` | 用户离开可视范围 |

### llm — 大语言模型集成

封装与 LLM API 的交互，支持 OpenAI 兼容接口。

**核心 trait**：`src/llm/provider.rs:8-12`

```rust
#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn chat_completion(
        &self,
        messages: Vec<Value>,
        tools: Vec<Value>,
    ) -> Result<LlmResponse>;
}
```

**响应结构**：`src/llm/provider.rs:14-25`

```rust
pub struct LlmResponse {
    pub content: Option<String>,    // 文本回复
    pub tool_calls: Vec<ToolCall>,  // 工具调用请求
}

pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}
```

### permission — 权限控制

基于 TeamSpeak 服务器组和频道组的访问控制。

**核心方法**：`src/permission/gate.rs:12-43`

```rust
pub fn get_allowed_skills(&self, caller_groups: &[u32], caller_channel_group_id: u32) -> Vec<String>
```

**目标保护**：`src/permission/gate.rs:45-79`

```rust
pub fn can_target(&self, caller_groups: &[u32], caller_channel_group_id: u32, target_groups: &[u32]) -> bool
```

防止普通用户对管理员组执行操作（踢出、封禁等）。

**规则匹配逻辑**：服务器组和频道组只要有一个匹配即视为匹配。如果两者都为空数组，则该规则匹配所有用户。

### router — 事件路由

协调所有模块处理用户消息，实现完整的对话流程。

**消息处理流程**：`src/router.rs:115-286`

1. 过滤自身消息
2. 判断是否响应（私聊或前缀触发）
3. 获取用户服务器组
4. 准备 LLM 上下文
5. 第一次 LLM 调用
6. 执行工具调用（如有）
7. 第二次 LLM 调用（包含工具结果）
8. 发送回复

## 技能系统开发

### Skill trait

所有技能必须实现 `Skill` trait：`src/skills/mod.rs:26-31`

```rust
#[async_trait]
pub trait Skill: Send + Sync {
    /// 技能名称（唯一标识）
    fn name(&self) -> &'static str;

    /// 技能描述（供 LLM 理解用途）
    fn description(&self) -> &'static str;

    /// 参数 JSON Schema
    fn parameters(&self) -> Value;

    /// 执行逻辑
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value>;
}
```

### ExecutionContext

技能执行时可访问的上下文：`src/skills/mod.rs:16-24`

```rust
pub struct ExecutionContext<'a> {
    pub adapter: Arc<TsAdapter>,         // TeamSpeak 适配器
    pub clients: &'a DashMap<u32, ClientInfo>,  // 在线用户列表
    pub caller_id: u32,                  // 调用者客户端 ID
    pub caller_groups: Vec<u32>,         // 调用者服务器组
    pub caller_channel_group_id: u32,    // 调用者频道组 ID
    pub gate: Arc<PermissionGate>,       // 权限门控
    pub config: Arc<AppConfig>,          // 应用配置
}
```

### 添加新技能

**步骤 1**：在 `src/skills/` 下创建新文件或扩展现有文件

```rust
// 示例：src/skills/example.rs
use crate::skills::{ExecutionContext, Skill};
use async_trait::async_trait;
use serde_json::{json, Value};
use anyhow::Result;

pub struct ExampleSkill;

#[async_trait]
impl Skill for ExampleSkill {
    fn name(&self) -> &'static str {
        "example_skill"
    }

    fn description(&self) -> &'static str {
        "示例技能：返回问候消息"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "用户名"
                }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value> {
        let name = args["name"].as_str().unwrap_or("Unknown");
        Ok(json!({
            "message": format!("Hello, {}!", name)
        }))
    }
}
```

**步骤 2**：在 `src/skills/mod.rs` 中注册

```rust
pub mod example;  // 添加模块声明

pub fn with_defaults() -> Self {
    // ... 现有注册 ...
    registry.register(Box::new(example::ExampleSkill));
    registry
}
```

**步骤 3**：在 `config/acl.toml` 中配置权限

```toml
[[rules]]
name = "example_rule"
server_group_ids = [8]
allowed_skills = ["example_skill"]
can_target_admins = false
```

### 技能开发最佳实践

1. **命名规范**：使用 `snake_case`，如 `kick_client`、`get_client_list`
2. **参数验证**：在 `execute` 中验证必填参数
3. **错误处理**：返回有意义的错误消息
4. **权限检查**：使用 `ctx.gate.can_target()` 检查操作权限
5. **返回值**：返回 JSON 对象，包含执行结果

### 现有技能列表

| 技能名 | 文件 | 说明 |
|---|---|---|
| `poke_client` | `communication.rs` | 戳一戳用户 |
| `send_message` | `communication.rs` | 发送消息 |
| `kick_client` | `moderation.rs` | 踢出用户 |
| `ban_client` | `moderation.rs` | 封禁用户 |
| `get_client_list` | `information.rs` | 获取在线用户列表 |
| `get_client_info` | `information.rs` | 获取用户详细信息 |
| `music_control` | `music.rs` | 音乐控制 |

## 扩展 LLM 提供者

### 实现 LlmProvider trait

```rust
use crate::llm::{LlmProvider, LlmResponse};
use async_trait::async_trait;
use serde_json::Value;
use anyhow::Result;

pub struct CustomProvider {
    // 配置字段
}

#[async_trait]
impl LlmProvider for CustomProvider {
    async fn chat_completion(
        &self,
        messages: Vec<Value>,
        tools: Vec<Value>,
    ) -> Result<LlmResponse> {
        // 实现 API 调用逻辑
        // 返回 LlmResponse
    }
}
```

### 集成到引擎

修改 `src/llm/engine.rs`，根据配置选择提供者：

```rust
pub fn new(config: Arc<AppConfig>) -> Self {
    let provider: Box<dyn LlmProvider> = match config.llm.provider.as_str() {
        "custom" => Box::new(CustomProvider::new(config.llm.clone())),
        _ => Box::new(OpenAiProvider::new(config.llm.clone())),
    };
    Self { provider }
}
```

## 权限系统

### 配置结构

`config/acl.toml` 采用从上到下的规则匹配：

```toml
[[rules]]
name = "规则名称"
server_group_ids = [6]          # 服务器组 ID 列表，空数组匹配所有人
channel_group_ids = [5]         # 频道组 ID 列表，空数组表示不检查频道组
allowed_skills = ["skill_name"] # 允许的技能，"*" 代表全部
can_target_admins = true        # 是否可操作受保护组成员

[acl]
protected_group_ids = [6, 8, 9] # 受保护的服务器组
```

### 权限评估流程

1. 遍历规则列表
2. 检查调用者服务器组是否匹配（server_group_ids 为空数组时匹配所有人）
3. 检查调用者频道组是否匹配（channel_group_ids 为空数组时匹配所有人）
4. 服务器组和频道组只要有一个匹配即视为匹配
5. 收集匹配规则允许的技能
6. 如果规则包含 `"*"`，立即返回全部技能
7. 对目标执行操作前，调用 `can_target()` 检查

## 代码规范

### 通用准则

- **全局中文**：所有可见输出、注释、文档优先使用中文
- **类型安全**：优先使用强类型结构体，避免原始 JSON 操作
- **错误处理**：使用 `anyhow::Result`，提供有意义的错误消息

### 编译器警告

- **禁止抑制警告**：不使用 `#[allow(dead_code)]` 或 `#[allow(unused)]`
- **死代码处理**：删除未使用代码，或重构以正确使用
- **未使用导入**：移除未使用的 `use` 语句

### 编写单元测试

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn test_skill_execution() {
        let skill = ExampleSkill;
        let args = json!({"name": "Test"});

        // 创建 mock ExecutionContext
        // ...

        let result = skill.execute(args, &ctx).await.unwrap();
        assert_eq!(result["message"], "Hello, Test!");
    }
}
```

## 贡献流程

1. Fork 仓库
2. 创建功能分支：`git checkout -b feature/my-feature`
3. 遵循代码规范进行开发
4. 确保所有测试通过：`cargo test`
5. Format代码: `cargo fmt`
6. 提交 Pull Request

### 提交信息格式

```
类型: 简短描述

详细描述（可选）
```

类型：`feat` | `fix` | `docs` | `refactor` | `test` | `chore`

## 常见问题

### Q: 如何调试连接问题？

检查 `config/settings.toml` 中的连接配置，确保：
- 主机和端口正确
- 登录凭据有效
- 服务器 ID 存在

### Q: 如何查看详细日志？

```bash
# 设置日志级别
RUST_LOG=debug cargo run

# 或使用命令行参数
cargo run -- --log-level debug
```

### Q: 如何测试特定技能？

1. 在 `config/acl.toml` 中为测试用户组添加目标技能
2. 启动机器人并使用对应 TeamSpeak 账号发送消息
3. 查看控制台日志确认执行结果

## 相关资源

- [Rust 官方文档](https://doc.rust-lang.org/book/)
- [TeamSpeak ServerQuery 手册](https://yat.qa/resources/)
- [OpenAI API 文档](https://platform.openai.com/docs/api-reference)
