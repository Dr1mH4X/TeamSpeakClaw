---
sidebar_position: 6
---

# 技能开发向导

本文档指导开发者从零开始创建一个新技能（Skill），包括实现 trait、跨平台支持和注册流程。

## 架构概览

技能系统由以下核心部分组成：

```
┌─────────────────────────────────────────────────┐
│                   SkillRegistry                  │
│  ┌─────────┐ ┌──────────┐ ┌───────────────────┐  │
│  │ poke_...│ │ send_... │ │ kick_client ...   │  │
│  └────┬────┘ └────┬─────┘ └────────┬──────────┘  │
│       │           │                │              │
│       └───────────┼────────────────┘              │
│                   ▼                               │
│            trait Skill                            │
│     execute() / execute_unified()                 │
└─────────────────────────────────────────────────┘
```

### 执行上下文

| 上文类型 | 用途 | 构建方式 |
|---|---|---|
| `ExecutionContext` | TeamSpeak 原生执行 | 路由器直接构建 |
| `NcExecutionContext` | NapCat 原生执行 | 路由器直接构建 |
| `UnifiedExecutionContext` | 跨平台统一执行 | `from_ts()` / `from_nc()` |

### Trait 定义

```rust
#[async_trait]
pub trait Skill: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn parameters(&self) -> Value;

    // 必须实现：TeamSpeak 原生执行
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value>;

    // 可选覆盖：NapCat 原生执行（默认返回不支持）
    async fn execute_nc(&self, args: Value, ctx: &NcExecutionContext) -> Result<Value>;

    // 可选覆盖：统一执行（默认返回不支持）
    async fn execute_unified(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value>;
}
```

**调用优先级**：`execute_unified` → 回退 `execute`（TS 端）或 `execute_nc`（NC 端）。

## 快速上手：创建一个新技能

### 第 1 步：定义结构体并实现 `execute`

```rust
// src/skills/example.rs
use crate::skills::{ExecutionContext, Skill, UnifiedExecutionContext};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::info;

pub struct EchoSkill;

#[async_trait]
impl Skill for EchoSkill {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn description(&self) -> &'static str {
        "回显传入的消息"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "msg": { "type": "string", "description": "要回显的消息" }
            },
            "required": ["msg"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value> {
        let msg = args["msg"].as_str().unwrap_or("");
        Ok(json!({ "status": "ok", "message": format!("回显: {}", msg) }))
    }
}
```

### 第 2 步：实现 `execute_unified`（跨平台支持）

所有需要跨平台调用的技能都应实现 `execute_unified`。使用 `ctx.to_ts_ctx()?` 可以直接从 `UnifiedExecutionContext` 还原出 `ExecutionContext`，避免手动拆箱：

```rust
    async fn execute_unified(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value> {
        info!("EchoSkill: unified execution, platform={:?}", ctx.platform);

        match ctx.platform {
            Platform::TeamSpeak => {
                // 通过 to_ts_ctx() 一行还原 TS 上下文
                let ts_ctx = ctx.to_ts_ctx()?;
                self.execute(args, &ts_ctx).await
            }
            Platform::NapCat => {
                // NC 端直接处理
                let msg = args["msg"].as_str().unwrap_or("");
                Ok(json!({
                    "status": "ok",
                    "message": format!("QQ回显: {}", msg),
                    "platform": "napcat"
                }))
            }
        }
    }
```

`to_ts_ctx()` 会自动检查 `ts_adapter` 和 `ts_clients` 是否可用，不可用时返回有意义的错误。

### 第 3 步：注册技能

在 `src/skills/mod.rs` 中：

```rust
pub mod example;  // 1. 添加模块声明

pub fn with_defaults() -> Self {
    // ...
    registry.register(Box::new(example::EchoSkill));  // 2. 注册
    // ...
}
```

### 第 4 步：配置 ACL

在 `config/acl.toml` 中为用户组授予该技能权限：

```toml
[[rules]]
name = "echo_rule"
server_group_ids = [8]       # 普通用户组
allowed_skills = ["echo"]
can_target_admins = false
```

## 常见模式

### 权限验证与自操作防护

对需要操作其他用户的功能（踢出、封禁、移动等），使用 `ctx.gate.can_target()` 检查目标权限，并防止操作自身：

```rust
async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value> {
    let clid = args["clid"].as_u64()
        .ok_or_else(|| anyhow::anyhow!(ctx.error_prompts.missing_parameter.replace("{param}", "clid")))? as u32;

    // 自操作防护
    if clid == ctx.caller_id {
        return Err(anyhow::anyhow!(ctx.error_prompts.self_target.clone()));
    }

    // 权限检查
    let target_groups = ctx.clients.get(&clid)
        .map(|c| c.server_groups.clone())
        .unwrap_or_default();
    if !ctx.gate.can_target(&ctx.caller_groups, ctx.caller_channel_group_id, &target_groups) {
        return Err(anyhow::anyhow!(ctx.error_prompts.target_permission.clone()));
    }

    // ... 执行操作
}
```

### 跨平台转发

在 NC 端调用时，如果实际操作需要在 TS 上执行（如播放音乐），直接使用 `ctx.to_ts_ctx()?` 获取 TS 上下文：

```rust
Platform::NapCat => {
    let ts_ctx = ctx.to_ts_ctx()?;  // 自动检查适配器可用性
    // 使用 ts_ctx.adapter 发送 TS 命令
    ts_ctx.adapter.send_raw(&some_command).await?;
    Ok(json!({ "status": "ok" }))
}
```

### 参数错误提示

优先使用 `ctx.error_prompts` 中的统一错误模板，保持用户体验一致：

```rust
let clid = args["clid"].as_u64().ok_or_else(|| {
    anyhow::anyhow!(ctx.error_prompts.missing_parameter.replace("{param}", "clid"))
})? as u32;
```

## 现有技能一览

| 技能名 | 文件 | 跨平台 | 说明 |
|---|---|---|---|
| `poke_client` | `communication.rs` | ✅ | 戳一戳用户 |
| `send_message` | `communication.rs` | ✅ | 发送消息（支持 TS↔NC 双向路由） |
| `kick_client` | `moderation.rs` | ✅ | 踢出用户 |
| `ban_client` | `moderation.rs` | ✅ | 封禁用户 |
| `move_client` | `moderation.rs` | ✅ | 移动用户到指定频道 |
| `get_client_list` | `information.rs` | ✅ | 获取在线用户列表 |
| `get_client_info` | `information.rs` | ✅ | 获取用户详细信息 |
| `music_control` | `music.rs` | ✅ | 音乐控制（双后端） |

## 文件组织

按功能分组放在 `src/skills/` 下：

```
src/skills/
├── mod.rs               # Skill trait + 上下文类型 + SkillRegistry
├── communication.rs     # 沟通类：poke_client, send_message
├── information.rs       # 查询类：get_client_list, get_client_info
├── moderation.rs        # 管理类：kick_client, ban_client, move_client
└── music.rs             # 音乐类：music_control
```

推荐新技能按相同分组原则放入对应文件，或创建新文件（如 `automation.rs`）。

## 最佳实践

1. **命名**：使用 `snake_case`，如 `kick_client`
2. **参数**：在 `parameters()` 中提供完整的 JSON Schema，LLM 依赖它来正确调用
3. **错误**：返回有意义的错误，使用 `ctx.error_prompts` 中的模板
4. **跨平台**：所有新技能都应实现 `execute_unified`，使用 `ctx.to_ts_ctx()?` 简化逻辑
5. **日志**：在 `execute_unified` 入口处添加 `info!` 日志，便于调试
6. **返回值**：返回 `{"status": "ok", ...}` 格式的 JSON 对象
