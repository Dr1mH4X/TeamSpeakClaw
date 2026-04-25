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
cargo run
```

### 配置开发环境

首次运行前请手动创建配置文件，详见 [配置指南](/docs/configuration)

## 项目架构

### 目录结构

```
src/
├── main.rs                  # 入口：初始化组件，启动事件循环
├── cli.rs                   # 命令行参数解析
├── log.rs                   # 日志初始化
├── router.rs                # 模块重导出（→ router/）
├── router/
│   ├── sq_router.rs         # TeamSpeak 事件路由
│   ├── nc_router.rs         # NapCat/QQ 事件路由
│   ├── unified.rs           # 统一事件模型（跨平台）
│   └── headless_bridge.rs   # Headless 语音桥接 LLM 路由
├── adapter/
│   ├── mod.rs
│   ├── serverquery/         # TeamSpeak ServerQuery 适配器
│   │   ├── mod.rs
│   │   ├── connection.rs    # TCP/SSH 连接管理
│   │   ├── command.rs       # 命令构建
│   │   └── event.rs         # 事件解析
│   ├── napcat/              # NapCat OneBot 11 适配器
│   │   ├── mod.rs
│   │   ├── ws.rs            # WebSocket 连接与重连
│   │   ├── api.rs           # OneBot API action 定义
│   │   ├── event.rs         # 事件解析
│   │   └── types.rs         # 消息段与响应类型
│   └── headless/            # Headless 语音服务
│       ├── mod.rs
│       ├── service.rs        # 语音服务主逻辑
│       ├── actor.rs          # 语音角色管理
│       ├── playback.rs       # 播放控制
│       ├── speech.rs         # STT/TTS 处理
│       ├── serverquery.rs    # Headless 使用的 ServerQuery 客户端
│       └── types.rs         # 类型定义
├── config/
│   ├── mod.rs               # AppConfig 聚合
│   ├── serverquery.rs       # ServerQuery 配置
│   ├── napcat.rs            # NapCat 配置
│   ├── headless.rs          # Headless 语音服务配置
│   ├── bot.rs               # 机器人行为配置
│   ├── llm.rs               # LLM 配置
│   ├── music_backend.rs     # 音乐后端配置
│   ├── acl.rs               # 权限规则配置
│   ├── logging.rs           # 日志配置
│   ├── rate_limit.rs        # 限流配置
│   └── prompts.rs           # 提示词与错误消息
├── llm/
│   ├── mod.rs
│   ├── engine.rs            # LLM 引擎封装
│   ├── provider.rs          # 提供者 trait 与 OpenAI 实现
│   └── context.rs           # 上下文窗口管理
├── permission/
│   ├── mod.rs
│   └── gate.rs              # 权限门控逻辑
└── skills/
    ├── mod.rs               # Skill trait、上下文类型与注册表
    ├── communication.rs     # poke_client、send_message
    ├── information.rs       # get_client_list、get_client_info
    ├── moderation.rs        # kick_client、ban_client、move_client
    └── music.rs             # music_control（双后端 + 跨平台）
```

### 数据流

**TeamSpeak 路径**：

```
用户消息 → TsAdapter (TCP/SSH) → SqRouter → LlmEngine
                                                 ↓
                                            工具调用请求
                                                 ↓
                                     PermissionGate (权限检查)
                                                 ↓
                                     SkillRegistry → Skill.execute()
                                                 ↓
                                     执行结果 → LlmEngine → TsAdapter → 回复用户
```

**NapCat / QQ 路径**：

```
用户消息 → NapCatAdapter (WebSocket) → NcRouter → LlmEngine
                                                       ↓
                                                  工具调用请求
                                                       ↓
                                           PermissionGate (权限检查)
                                                       ↓
                                     SkillRegistry → Skill.execute_unified()
                                           ↓              ↓
                                     NC 原生执行    转发到 TS 执行
                                           ↓              ↓
                                     回复 NC 用户    回复 NC 用户
```

**Headless 语音路径**：

```
语音输入 → STT (语音转文字) → HeadlessService → HeadlessLlmBridge
                                                        ↓
                                                   工具调用请求
                                                        ↓
                                            PermissionGate (权限检查)
                                                        ↓
                                          SkillRegistry → Skill.execute()
                                                        ↓
                                            执行结果 → LlmEngine → TTS → 语音输出
```

### 跨平台行为矩阵

| Skill | TS 入口 | NC 入口（默认） | NC 入口 + `ts_route=true` | Headless 入口 |
|---|---|---|---|---|
| `poke_client` | ✅ TS 执行 | ❌ | ❌ | ❌ |
| `send_message` | ✅ `private/channel/server` | ✅ `private/group`（NapCat 原生） | ✅ 路由到 TS | ✅ 通过 TS 执行 |
| `kick_client` | ✅ TS 执行 | ✅ 转发到 TS 执行 | 不适用 | ✅ TS 执行 |
| `ban_client` | ✅ TS 执行 | ✅ 转发到 TS 执行 | 不适用 | ✅ TS 执行 |
| `move_client` | ✅ TS 执行 | ✅ 转发到 TS 执行 | 不适用 | ✅ TS 执行 |
| `get_client_list` | ✅ TS 执行 | ✅ 查询 TS 在线缓存并回传 | 不适用 | ✅ TS 执行 |
| `get_client_info` | ✅ TS 执行 | ✅ 查询 TS 在线缓存并回传 | 不适用 | ✅ TS 执行 |
| `music_control` | ✅ TS 执行 | ✅ NC 请求转发到 TS，等待 TS3AudioBot 实际回复后回传 | 不适用 | ✅ TS 执行 |

说明：
- NC 侧统一执行遵循"先 `execute_unified`，失败再回退 `execute_nc`"。
- TS 侧统一执行遵循"先 `execute_unified`，失败回退 `execute`"。
- Headless 模式通过 `HeadlessLlmBridge` 桥接，复用 TS 的执行上下文。
- NC 权限通过 ACL 虚拟组映射（`9000~9003`）实现，详见配置文档。

## 核心模块详解

### adapter — 通信适配器

#### TeamSpeak 适配器 (`adapter/serverquery/`)

负责与 TeamSpeak 服务器的底层通信。

**核心结构**：`src/adapter/serverquery/connection.rs`

```rust
pub struct TsAdapter {
    writer: Mutex<tokio::io::WriteHalf<TsStream>>,
    event_tx: broadcast::Sender<TsEvent>,
    bot_clid: AtomicU32,
    query_tx: mpsc::Sender<String>,
    query_active: AtomicBool,
    include_event_lines_active: AtomicBool,
    query_lock: Mutex<()>,
}
```

**连接方式**：
- TCP（默认）：`method = "tcp"`
- SSH：`method = "ssh"`

**事件类型**：`src/adapter/serverquery/event.rs`

| 事件 | 说明 |
| --- | --- |
| `TsEvent::TextMessage` | 收到文本消息 |
| `TsEvent::ClientEnterView` | 用户进入可视范围 |
| `TsEvent::ClientLeftView` | 用户离开可视范围 |

#### NapCat 适配器 (`adapter/napcat/`)

通过 WebSocket 连接 NapCat（OneBot 11 协议），支持断线自动重连。

**核心结构**：`src/adapter/napcat/ws.rs`

```rust
pub struct NapCatAdapter {
    writer: Mutex<Option<WsSink>>,   // None 表示断线中
    event_tx: broadcast::Sender<NcEvent>,
    pending: Arc<DashMap<String, oneshot::Sender<NcApiResponse>>>,
    self_id: AtomicI64,
    reconnect_tx: mpsc::Sender<()>,
    config: NapCatConfig,
}
```

**认证方式**：WebSocket 握手时同时携带 `Authorization: Bearer` header 和 `access_token` query parameter，兼容 OneBot 11 标准。

**事件类型**：`src/adapter/napcat/event.rs`

| 事件 | 说明 |
| --- | --- |
| `NcEvent::PrivateMessage` | 收到 QQ 私聊消息 |
| `NcEvent::GroupMessage` | 收到 QQ 群消息 |

#### Headless 语音服务 (`adapter/headless/`)

提供无界面语音交互能力，集成 STT（语音转文字）和 TTS（文字转语音）。

**核心组件**：
- `service.rs` — 语音服务主逻辑，管理语音连接和音频流
- `actor.rs` — 语音角色管理，处理语音客户端的状态
- `playback.rs` — 播放控制，管理音频播放队列
- `speech.rs` — STT/TTS 处理，调用外部语音服务 API
- `serverquery.rs` — Headless 专用的 ServerQuery 客户端
- `types.rs` — 类型定义

**配置**：通过 `settings.toml` 的 `[headless]`、`[headless.stt]`、`[headless.tts]` 区段配置。

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

**集成方式**：`src/llm/engine.rs` 中 `LlmEngine::new()` 直接创建 `OpenAiProvider`，所有 OpenAI 兼容接口（DeepSeek、ChatGPT 等）均可通过 `base_url` 和 `model` 配置。

#### Context 窗口管理 (`llm/context.rs`)

管理多轮对话上下文，支持会话隔离和自动淘汰。

**会话来源**：`SessionSource` 枚举

| 来源 | 说明 |
| --- | --- |
| `TeamSpeak { clid }` | TeamSpeak 用户 |
| `NapCatPrivate { user_id }` | NapCat 私聊 |
| `NapCatGroup { group_id }` | NapCat 群聊 |
| `Headless { caller_id }` | Headless 语音模式 |

**核心结构**：`ContextWindow`

```rust
pub struct ContextWindow {
    histories: Arc<DashMap<String, VecDeque<ContextTurn>>>,  // 会话历史
    session_order: Arc<Mutex<VecDeque<String>>>,            // 会话顺序（用于淘汰）
    max_turns: usize,     // 最大对话轮数
    max_sessions: usize,  // 最大会话数
}
```

**配置**：通过 `settings.toml` 的 `[llm]` 区段设置：
- `max_context_turns` — 最大上下文对话轮数（0 表示禁用）
- `max_context_sessions` — 最大会话数（超过时淘汰最旧会话）

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

**规则匹配逻辑**：遍历所有规则，收集所有匹配规则的 `allowed_skills` 取并集。空数组表示"匹配所有"。

### router — 事件路由

#### 统一事件模型 (`router/unified.rs`)

提供跨平台的事件抽象，统一不同来源的消息处理。

**事件来源**：`InboundSource` 枚举

| 来源 | 说明 |
| --- | --- |
| `TeamSpeakText` | TeamSpeak 文本消息 |
| `NapCatPrivate` | NapCat 私聊消息 |
| `NapCatGroup` | NapCat 群聊消息 |
| `HeadlessText` | Headless 文本输入 |
| `HeadlessVoiceStt` | Headless 语音转文字 |

**回复策略**：`ReplyPolicy` 枚举

| 策略 | 说明 |
| --- | --- |
| `TeamSpeak { target_mode, target }` | TS 回复（私聊/频道/服务器） |
| `NapCatPrivate { user_id }` | QQ 私聊回复 |
| `NapCatGroup { group_id, at_user_id }` | QQ 群聊回复 |
| `Headless { target_mode, target_client_id }` | Headless 回复 |

#### SqRouter（TeamSpeak）

`src/router/sq_router.rs` — 处理 TeamSpeak 文本消息事件。

消息处理流程：
1. 过滤自身消息
2. 过滤 TS3AudioBot 自动回复
3. 判断是否响应（私聊或前缀触发）
4. 获取用户服务器组
5. 第一次 LLM 调用
6. 执行工具调用（如有，通过 `UnifiedExecutionContext`）
7. 第二次 LLM 调用（包含工具结果）
8. 发送回复

#### NcRouter（NapCat / QQ）

`src/router/nc_router.rs` — 处理 NapCat 私聊和群消息事件。

与 SqRouter 的关键差异：
- 拥有 `ts_adapter` 和 `ts_clients`，可构造 `UnifiedExecutionContext::from_nc()` 实现跨平台工具调用
- NC 用户的权限通过虚拟组 ID（`9000-9003`）映射

#### HeadlessLlmBridge（`router/headless_bridge.rs`）

`src/router/headless_bridge.rs` — 连接 Headless 语音服务与 LLM 引擎的桥梁。

处理流程：
1. 从 Headless 服务接收 STT 转换的文本
2. 构造 `UnifiedInboundEvent::from_headless()`
3. 执行 LLM 调用和工具执行
4. 将回复文本通过 TTS 转换为语音输出

## 技能系统开发

### Skill trait

所有技能必须实现 `Skill` trait：`src/skills/mod.rs:152-177`

```rust
#[async_trait]
pub trait Skill: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn parameters(&self) -> Value;

    /// TeamSpeak 执行（必须实现）
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value>;

    /// NapCat/QQ 执行（默认返回"不支持"，按需覆盖）
    async fn execute_nc(&self, args: Value, _ctx: &NcExecutionContext) -> Result<Value> {
        let _ = args;
        Err(anyhow::anyhow!(
            "Skill '{}' does not support the NapCat platform",
            self.name()
        ))
    }

    /// 统一执行（支持跨平台，默认返回"不支持"，按需覆盖）
    async fn execute_unified(&self, args: Value, _ctx: &UnifiedExecutionContext) -> Result<Value> {
        let _ = args;
        Err(anyhow::anyhow!(
            "Skill '{}' does not support unified execution",
            self.name()
        ))
    }
}
```

### ExecutionContext

TeamSpeak 技能执行时的上下文：`src/skills/mod.rs:31-41`

```rust
pub struct ExecutionContext<'a> {
    pub adapter: Arc<TsAdapter>,
    pub clients: &'a DashMap<u32, ClientInfo>,
    pub caller_id: u32,
    pub caller_name: String,
    pub caller_groups: Vec<u32>,
    pub caller_channel_group_id: u32,
    pub gate: Arc<PermissionGate>,
    pub config: Arc<AppConfig>,
    pub error_prompts: &'a ErrorPrompts,
}
```

### NcExecutionContext

NapCat 技能执行时的上下文：`src/skills/mod.rs:47-55`

```rust
pub struct NcExecutionContext<'a> {
    pub adapter: Arc<NapCatAdapter>,
    pub caller_id: i64,
    pub caller_name: String,
    pub caller_group_id: Option<i64>,
    pub gate: Arc<PermissionGate>,
    pub config: Arc<AppConfig>,
    pub error_prompts: &'a ErrorPrompts,
}
```

### UnifiedExecutionContext

跨平台统一上下文，由 `from_ts()` 或 `from_nc()` 构建，通过 `with_cross_adapters()` 注入对端适配器：`src/skills/mod.rs:61-75`

```rust
pub struct UnifiedExecutionContext<'a> {
    pub platform: Platform,                      // TeamSpeak | NapCat
    pub ts_adapter: Option<Arc<TsAdapter>>,
    pub ts_clients: Option<&'a DashMap<u32, ClientInfo>>,
    pub nc_adapter: Option<Arc<NapCatAdapter>>,
    pub caller_id: u32,
    pub caller_id_nc: i64,
    pub caller_name: String,
    pub caller_groups: Vec<u32>,
    pub caller_channel_group_id: u32,
    pub nc_group_id: Option<i64>,
    pub gate: Arc<PermissionGate>,
    pub config: Arc<AppConfig>,
    pub error_prompts: &'a ErrorPrompts,
}
```

**辅助方法**：

```rust
impl<'a> UnifiedExecutionContext<'a> {
    // 从 TS 上下文构建
    pub fn from_ts(ctx: &ExecutionContext<'a>) -> Self { ... }

    // 从 NC 上下文构建
    pub fn from_nc(ctx: &NcExecutionContext<'a>) -> Self { ... }

    // 注入跨平台适配器
    pub fn with_cross_adapters(
        mut self,
        ts_adapter: Option<Arc<TsAdapter>>,
        ts_clients: Option<&'a DashMap<u32, ClientInfo>>,
        nc_adapter: Option<Arc<NapCatAdapter>>,
    ) -> Self { ... }

    // 还原为 TS 执行上下文（用于跨平台技能执行）
    pub fn to_ts_ctx(&self) -> Result<ExecutionContext<'a>> { ... }
}
```

### 添加新技能

**步骤 1**：在 `src/skills/` 下创建新文件或扩展现有文件

```rust
// 示例：src/skills/example.rs
use crate::skills::{ExecutionContext, Platform, Skill, UnifiedExecutionContext};
use async_trait::async_trait;
use serde_json::{json, Value};
use anyhow::Result;
use tracing::info;

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

    // NapCat 执行（按需覆盖）
    async fn execute_nc(&self, args: Value, ctx: &NcExecutionContext) -> Result<Value> {
        // 针对 NapCat 平台的特化实现
        let name = args["name"].as_str().unwrap_or("Unknown");
        Ok(json!({
            "message": format!("Hello from NC, {}!", name)
        }))
    }

    // 跨平台支持：使用 ctx.to_ts_ctx()? 简化上下文还原
    async fn execute_unified(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value> {
        info!("ExampleSkill: unified execution, platform={:?}", ctx.platform);
        match ctx.platform {
            Platform::TeamSpeak => {
                let ts_ctx = ctx.to_ts_ctx()?;
                self.execute(args, &ts_ctx).await
            }
            Platform::NapCat => {
                // 如果有 NC 特化实现，可以在这里调用
                Err(anyhow::anyhow!("Use execute_nc for NC platform"))
            }
        }
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

### 技能开发实践

1. **命名规范**：使用 `snake_case`，如 `kick_client`、`get_client_list`
2. **参数验证**：在 `execute` 中验证必填参数
3. **错误处理**：返回有意义的错误消息，使用 `ctx.error_prompts` 模板
4. **权限检查**：使用 `ctx.gate.can_target()` 检查操作权限
5. **返回值**：返回 JSON 对象，包含 `status: "ok"` 及执行结果
6. **跨平台**：实现 `execute_unified()`，使用 `ctx.to_ts_ctx()?` 一行还原 TS 上下文

### 现有技能列表

| 技能名 | 文件 | 说明 |
| --- | --- | --- |
| `poke_client` | `communication.rs` | 戳一戳用户 |
| `send_message` | `communication.rs` | 发送消息（跨平台，支持 TS/NC 路由） |
| `kick_client` | `moderation.rs` | 踢出用户 |
| `ban_client` | `moderation.rs` | 封禁用户 |
| `move_client` | `moderation.rs` | 移动用户到指定频道 |
| `get_client_list` | `information.rs` | 获取在线用户列表 |
| `get_client_info` | `information.rs` | 获取用户详细信息 |
| `music_control` | `music.rs` | 音乐控制（双后端 + 跨平台 + Headless 支持） |

## 权限系统

### 配置结构

`config/acl.toml` 遍历所有规则，收集匹配规则的技能取并集：

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
2. 检查调用者服务器组是否匹配（`server_group_ids` 为空数组时匹配所有服务器组）
3. 检查调用者频道组是否匹配（`channel_group_ids` 为空数组时跳过频道组检查）
4. 服务器组和频道组同时匹配才视为该规则匹配
5. 收集所有匹配规则允许的技能，取并集
6. 如果任一匹配规则包含 `"*"`，立即返回全部技能
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

## 贡献流程

1. Fork 仓库
2. 创建功能分支：`git checkout -b feature/my-feature`
3. 遵循代码规范进行开发
4. 确保所有测试通过：`cargo test`
5. 格式化代码: `cargo fmt`
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
- [OneBot 11 标准](https://github.com/botuniverse/onebot-11)
- [NapCat 文档](https://napneko.github.io/)
