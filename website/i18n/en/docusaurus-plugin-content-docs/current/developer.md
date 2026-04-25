---
sidebar_position: 5
---

# Developer Guide

This guide helps contributors quickly understand the code architecture of TeamSpeakClaw and start developing.

## Quick Start

### Prerequisites

- Rust 1.75+ (edition 2021)
- Git
- Optional: A TeamSpeak server (for connection testing)

### Clone & Build

```bash
# Clone the repository
git clone https://github.com/Dr1mH4X/TeamSpeakClaw.git
cd TeamSpeakClaw

# Build the project
cargo build

# Run (Development mode)
cargo run
```

### Setup Development Environment

Manually create the configuration file before first run. See [Configuration Guide](/docs/configuration) for details.

## Project Architecture

### Directory Structure

```
src/
├── main.rs                  # Entry: Initialize components, start event loop
├── cli.rs                   # CLI argument parsing
├── log.rs                   # Logging initialization
├── router.rs                # Module re-exports (→ router/)
├── router/
│   ├── sq_router.rs         # TeamSpeak event router
│   ├── nc_router.rs         # NapCat/QQ event router
│   ├── unified.rs           # Unified event model (cross-platform)
│   └── headless_bridge.rs   # Headless voice LLM bridge
├── adapter/
│   ├── mod.rs
│   ├── serverquery/         # TeamSpeak ServerQuery adapter
│   │   ├── mod.rs
│   │   ├── connection.rs    # TCP/SSH connection management
│   │   ├── command.rs       # Command building
│   │   └── event.rs         # Event parsing
│   ├── napcat/              # NapCat OneBot 11 adapter
│   │   ├── mod.rs
│   │   ├── ws.rs            # WebSocket connection and reconnection
│   │   ├── api.rs           # OneBot API action definitions
│   │   ├── event.rs         # Event parsing
│   │   └── types.rs         # Message segments and response types
│   └── headless/            # Headless voice service
│       ├── mod.rs
│       ├── service.rs        # Voice service main logic
│       ├── actor.rs          # Voice actor management
│       ├── playback.rs       # Playback control
│       ├── speech.rs         # STT/TTS processing
│       ├── serverquery.rs    # ServerQuery client for Headless
│       └── types.rs         # Type definitions
├── config/
│   ├── mod.rs               # AppConfig aggregation
│   ├── serverquery.rs       # ServerQuery config
│   ├── napcat.rs            # NapCat config
│   ├── headless.rs          # Headless voice service config
│   ├── bot.rs               # Bot behavior config
│   ├── llm.rs               # LLM config
│   ├── music_backend.rs     # Music backend config
│   ├── acl.rs               # ACL rules config
│   ├── logging.rs           # Logging config
│   ├── rate_limit.rs        # Rate limit config
│   └── prompts.rs           # Prompts and error messages
├── llm/
│   ├── mod.rs
│   ├── engine.rs            # LLM engine wrapper
│   ├── provider.rs          # Provider trait and OpenAI implementation
│   └── context.rs           # Context window management
├── permission/
│   ├── mod.rs
│   └── gate.rs              # Permission gating logic
└── skills/
    ├── mod.rs               # Skill trait, context types, and registry
    ├── communication.rs     # poke_client, send_message
    ├── information.rs       # get_client_list, get_client_info
    ├── moderation.rs        # kick_client, ban_client, move_client
    └── music.rs             # music_control (dual backend + cross-platform)
```

### Data Flow

**TeamSpeak Path**:

```
User Message → TsAdapter (TCP/SSH) → SqRouter → LlmEngine
                                                 ↓
                                            Tool Call Request
                                                 ↓
                                     PermissionGate (Auth Check)
                                                 ↓
                                     SkillRegistry → Skill.execute()
                                                 ↓
                                     Result → LlmEngine → TsAdapter → Reply to User
```

**NapCat / QQ Path**:

```
User Message → NapCatAdapter (WebSocket) → NcRouter → LlmEngine
                                                       ↓
                                                  Tool Call Request
                                                       ↓
                                           PermissionGate (Auth Check)
                                                       ↓
                                     SkillRegistry → Skill.execute_unified()
                                           ↓              ↓
                                     NC native exec    Forward to TS exec
                                           ↓              ↓
                                     Reply NC user     Reply NC user
```

**Headless Voice Path**:

```
Voice Input → STT (Speech-to-Text) → HeadlessService → HeadlessLlmBridge
                                                        ↓
                                                    Tool Call Request
                                                        ↓
                                            PermissionGate (Auth Check)
                                                        ↓
                                          SkillRegistry → Skill.execute()
                                                        ↓
                                             Result → LlmEngine → TTS → Voice Output
```

### Cross-platform Behavior Matrix

| Skill | TS entry | NC entry (default) | NC entry + `ts_route=true` | Headless entry |
|---|---|---|---|---|
| `poke_client` | ✅ TS execution | ❌ | ❌ | ❌ |
| `send_message` | ✅ `private/channel/server` | ✅ `private/group` (NapCat native) | ✅ routed to TS | ✅ via TS execution |
| `kick_client` | ✅ TS execution | ✅ forwarded to TS execution | n/a | ✅ TS execution |
| `ban_client` | ✅ TS execution | ✅ forwarded to TS execution | n/a | ✅ TS execution |
| `move_client` | ✅ TS execution | ✅ forwarded to TS execution | n/a | ✅ TS execution |
| `get_client_list` | ✅ TS execution | ✅ queries TS online cache and returns | n/a | ✅ TS execution |
| `get_client_info` | ✅ TS execution | ✅ queries TS online cache and returns | n/a | ✅ TS execution |
| `music_control` | ✅ TS execution | ✅ NC request forwarded to TS, waits for TS3AudioBot actual reply then returns | n/a | ✅ TS execution |

Notes:
- NC side unified execution follows "first `execute_unified`, fallback to `execute_nc` on failure".
- TS side unified execution follows "first `execute_unified`, fallback to `execute` on failure".
- Headless mode uses `HeadlessLlmBridge` as bridge, reusing TS execution context.
- NC permissions are enforced via ACL virtual group mapping (`9000~9003`), see configuration docs.

## Core Modules Detail

### adapter — Communication Adapter

#### TeamSpeak Adapter (`adapter/serverquery/`)

Handles low-level communication with the TeamSpeak server.

**Core Structure**: `src/adapter/serverquery/connection.rs`

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

**Connection Methods**:
- TCP (Default): `method = "tcp"`
- SSH: `method = "ssh"`

**Event Types**: `src/adapter/serverquery/event.rs`

| Event | Description |
|---|---|
| `TsEvent::TextMessage` | Text message received |
| `TsEvent::ClientEnterView` | Client entered view |
| `TsEvent::ClientLeftView` | Client left view |

#### NapCat Adapter (`adapter/napcat/`)

Connects to NapCat via WebSocket (OneBot 11 protocol), supports automatic reconnection on disconnect.

**Core Structure**: `src/adapter/napcat/ws.rs`

```rust
pub struct NapCatAdapter {
    writer: Mutex<Option<WsSink>>,   // None means disconnected
    event_tx: broadcast::Sender<NcEvent>,
    pending: Arc<DashMap<String, oneshot::Sender<NcApiResponse>>>,
    self_id: AtomicI64,
    reconnect_tx: mpsc::Sender<()>,
    config: NapCatConfig,
}
```

**Authentication**: Carries both `Authorization: Bearer` header and `access_token` query parameter during WebSocket handshake, compatible with OneBot 11 standard.

**Event Types**: `src/adapter/napcat/event.rs`

| Event | Description |
|---|---|
| `NcEvent::PrivateMessage` | QQ private message received |
| `NcEvent::GroupMessage` | QQ group message received |

#### Headless Voice Service (`adapter/headless/`)

Provides headless voice interaction capability, integrating STT (Speech-to-Text) and TTS (Text-to-Speech).

**Core Components**:
- `service.rs` — Voice service main logic, manages voice connections and audio streams
- `actor.rs` — Voice actor management, handles voice client state
- `playback.rs` — Playback control, manages audio playback queue
- `speech.rs` — STT/TTS processing, calls external voice service APIs
- `serverquery.rs` — ServerQuery client for Headless
- `types.rs` — Type definitions

**Configuration**: Configure via `[headless]`, `[headless.stt]`, `[headless.tts]` sections in `settings.toml`.

### llm — LLM Integration

Encapsulates interactions with LLM APIs, supporting OpenAI-compatible interfaces.

**Core Trait**: `src/llm/provider.rs:8-12`

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

**Response Structure**: `src/llm/provider.rs:14-25`

```rust
pub struct LlmResponse {
    pub content: Option<String>,    // Textual reply
    pub tool_calls: Vec<ToolCall>,  // Tool call requests
}

pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}
```

**Integration**: `LlmEngine::new()` in `src/llm/engine.rs` directly creates an `OpenAiProvider`. All OpenAI-compatible APIs (DeepSeek, ChatGPT, etc.) can be configured via `base_url` and `model`.

#### Context Window Management (`llm/context.rs`)

Manages multi-turn conversation context, supporting session isolation and automatic eviction.

**Session Sources**: `SessionSource` enum

| Source | Description |
| --- | --- |
| `TeamSpeak { clid }` | TeamSpeak user |
| `NapCatPrivate { user_id }` | NapCat private message |
| `NapCatGroup { group_id }` | NapCat group chat |
| `Headless { caller_id }` | Headless voice mode |

**Core Structure**: `ContextWindow`

```rust
pub struct ContextWindow {
    histories: Arc<DashMap<String, VecDeque<ContextTurn>>>,  // Session history
    session_order: Arc<Mutex<VecDeque<String>>>,            // Session order (for eviction)
    max_turns: usize,     // Maximum conversation turns
    max_sessions: usize,  // Maximum number of sessions
}
```

**Configuration**: Set in `[llm]` section of `settings.toml`:
- `max_context_turns` — Maximum context conversation turns (0 to disable)
- `max_context_sessions` — Maximum number of sessions (evicts oldest when exceeded)

### permission — Access Control

Access control based on TeamSpeak Server Groups and Channel Groups.

**Core Method**: `src/permission/gate.rs:12-43`

```rust
pub fn get_allowed_skills(&self, caller_groups: &[u32], caller_channel_group_id: u32) -> Vec<String>
```

**Target Protection**: `src/permission/gate.rs:45-79`

```rust
pub fn can_target(&self, caller_groups: &[u32], caller_channel_group_id: u32, target_groups: &[u32]) -> bool
```

Prevents regular users from performing actions (kick/ban) on administrative groups.

**Rule Matching Logic**: Iterate all rules, collect `allowed_skills` from all matching rules as a union. Empty array means "match all".

### router — Event Router

#### Unified Event Model (`router/unified.rs`)

Provides cross-platform event abstraction, unifying message handling from different sources.

**Event Sources**: `InboundSource` enum

| Source | Description |
| --- | --- |
| `TeamSpeakText` | TeamSpeak text message |
| `NapCatPrivate` | NapCat private message |
| `NapCatGroup` | NapCat group message |
| `HeadlessText` | Headless text input |
| `HeadlessVoiceStt` | Headless voice-to-text |

**Reply Policy**: `ReplyPolicy` enum

| Policy | Description |
| --- | --- |
| `TeamSpeak { target_mode, target }` | TS reply (PM/channel/server) |
| `NapCatPrivate { user_id }` | QQ private reply |
| `NapCatGroup { group_id, at_user_id }` | QQ group reply |
| `Headless { target_mode, target_client_id }` | Headless reply |

#### SqRouter (TeamSpeak)

`src/router/sq_router.rs` — Handles TeamSpeak text message events.

Message processing flow:
1. Filter out self-messages
2. Filter TS3AudioBot auto-replies
3. Determine response trigger (PM or prefix)
4. Get user server groups
5. First LLM call
6. Execute tool calls (if any, via `UnifiedExecutionContext`)
7. Second LLM call (containing tool results)
8. Send reply

#### NcRouter (NapCat / QQ)

`src/router/nc_router.rs` — Handles NapCat private and group message events.

Key differences from SqRouter:
- Has `ts_adapter` and `ts_clients`, can construct `UnifiedExecutionContext::from_nc()` for cross-platform tool calls
- NC user permissions are mapped via virtual group IDs (`9000-9003`)

## Skill System Development

### Skill Trait

All skills must implement the `Skill` trait: `src/skills/mod.rs:152-177`

```rust
#[async_trait]
pub trait Skill: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn parameters(&self) -> Value;

    /// TeamSpeak execution (must implement)
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value>;

    /// NapCat/QQ execution (defaults to "not supported", override as needed)
    async fn execute_nc(&self, args: Value, _ctx: &NcExecutionContext) -> Result<Value> {
        let _ = args;
        Err(anyhow::anyhow!(
            "Skill '{}' does not support the NapCat platform",
            self.name()
        ))
    }

    /// Unified execution (cross-platform, defaults to "not supported", override as needed)
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

Context during TeamSpeak skill execution: `src/skills/mod.rs:31-41`

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

Context during NapCat skill execution: `src/skills/mod.rs:47-55`

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

Cross-platform unified context, built via `from_ts()` or `from_nc()`, with cross-end adapters injected via `with_cross_adapters()`: `src/skills/mod.rs:61-75`

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

**Helper Methods**:

```rust
impl<'a> UnifiedExecutionContext<'a> {
    // Build from TS context
    pub fn from_ts(ctx: &ExecutionContext<'a>) -> Self { ... }

    // Build from NC context
    pub fn from_nc(ctx: &NcExecutionContext<'a>) -> Self { ... }

    // Inject cross-platform adapters
    pub fn with_cross_adapters(
        mut self,
        ts_adapter: Option<Arc<TsAdapter>>,
        ts_clients: Option<&'a DashMap<u32, ClientInfo>>,
        nc_adapter: Option<Arc<NapCatAdapter>>,
    ) -> Self { ... }

    // Restore to TS execution context (for cross-platform skill execution)
    pub fn to_ts_ctx(&self) -> Result<ExecutionContext<'a>> { ... }
}
```

### Adding a New Skill

**Step 1**: Create a new file or extend an existing one under `src/skills/`

```rust
// Example: src/skills/example.rs
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
        "Example skill: returns a greeting message"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "User name"
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

    // NapCat execution (override as needed)
    async fn execute_nc(&self, args: Value, ctx: &NcExecutionContext) -> Result<Value> {
        // Platform-specific implementation for NapCat
        let name = args["name"].as_str().unwrap_or("Unknown");
        Ok(json!({
            "message": format!("Hello from NC, {}!", name)
        }))
    }

    // Cross-platform support: use ctx.to_ts_ctx()? for simplified context restoration
    async fn execute_unified(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value> {
        info!("ExampleSkill: unified execution, platform={:?}", ctx.platform);
        match ctx.platform {
            Platform::TeamSpeak => {
                let ts_ctx = ctx.to_ts_ctx()?;
                self.execute(args, &ts_ctx).await
            }
            Platform::NapCat => {
                // If you have NC-specific implementation, call it here
                Err(anyhow::anyhow!("Use execute_nc for NC platform"))
            }
        }
    }
}
```

**Step 2**: Register in `src/skills/mod.rs`

```rust
pub mod example;  // Add module declaration

pub fn with_defaults() -> Self {
    // ... existing registration ...
    registry.register(Box::new(example::ExampleSkill));
    registry
}
```

**Step 3**: Configure permissions in `config/acl.toml`

```toml
[[rules]]
name = "example_rule"
server_group_ids = [8]
allowed_skills = ["example_skill"]
can_target_admins = false
```

### Skill Development Practices

1. **Naming**: Use `snake_case`, e.g., `kick_client`, `get_client_list`
2. **Parameter Validation**: Validate required parameters in `execute`
3. **Error Handling**: Return meaningful error messages, use `ctx.error_prompts` templates
4. **Permission Check**: Use `ctx.gate.can_target()` to check operation permissions
5. **Return Values**: Return JSON objects with `status: "ok"` and execution results
6. **Cross-Platform**: Implement `execute_unified()`, use `ctx.to_ts_ctx()?` for one-line TS context restoration


### Existing Skills

| Skill Name | File | Description |
|---|---|---|
| `poke_client` | `communication.rs` | Poke a user |
| `send_message` | `communication.rs` | Send message (cross-platform, supports TS/NC routing) |
| `kick_client` | `moderation.rs` | Kick a user |
| `ban_client` | `moderation.rs` | Ban a user |
| `move_client` | `moderation.rs` | Move a user to a specified channel |
| `get_client_list` | `information.rs` | Get online user list |
| `get_client_info` | `information.rs` | Get detailed user info |
| `music_control` | `music.rs` | Music control (dual backend + cross-platform + Headless support) |

## Permission System

### Configuration Structure

`config/acl.toml` iterates all rules, collecting the union of matched skills:

```toml
[[rules]]
name = "Rule Name"
server_group_ids = [6]          # Server group ID list, empty array matches everyone
channel_group_ids = [5]         # Channel group ID list, empty array means don't check channel group
allowed_skills = ["skill_name"] # Allowed skills, "*" means all
can_target_admins = true        # Whether can target protected group members

[acl]
protected_group_ids = [6, 8, 9] # Protected server groups
```

### Permission Evaluation Flow

1. Iterate through rule list
2. Check if caller's server group matches (`server_group_ids` empty array matches all server groups)
3. Check if caller's channel group matches (`channel_group_ids` empty array skips channel group check)
4. Both server group and channel group must match for the rule to match
5. Collect allowed skills from all matching rules, take the union
6. If any matching rule contains `"*"`, immediately return all skills
7. Before executing operation on target, call `can_target()` to check

## Code Standards

### General Guidelines

- **Global Chinese**: All visible output, comments, and documentation should prefer Chinese
- **Type Safety**: Prefer strong typed structs, avoid raw JSON manipulation
- **Error Handling**: Use `anyhow::Result`, provide meaningful error messages

### Compiler Warnings

- **No suppressing warnings**: Do not use `#[allow(dead_code)]` or `#[allow(unused)]`
- **Dead code handling**: Remove unused code, or refactor to use it properly
- **Unused imports**: Remove unused `use` statements

## Contribution Process

1. Fork the repository
2. Create a feature branch: `git checkout -b feature/my-feature`
3. Follow code standards during development
4. Ensure all tests pass: `cargo test`
5. Format code: `cargo fmt`
6. Submit a Pull Request

### Commit Message Format

```
type: short description

Detailed description (optional)
```

Types: `feat` | `fix` | `docs` | `refactor` | `test` | `chore`

## FAQ

### Q: How to debug connection issues?

Check connection settings in `config/settings.toml`, ensure:
- Host and port are correct
- Login credentials are valid
- Server ID exists

### Q: How to view detailed logs?

```bash
# Set log level
RUST_LOG=debug cargo run

# Or use CLI argument
cargo run -- --log-level debug
```

### Q: How to test a specific skill?

1. Add the target skill to your test user group in `config/acl.toml`
2. Start the bot and send a message using the corresponding TeamSpeak account
3. Check console logs to confirm execution results

## Related Resources

- [Rust Official Documentation](https://doc.rust-lang.org/book/)
- [TeamSpeak ServerQuery Manual](https://yat.qa/resources/)
- [OpenAI API Documentation](https://platform.openai.com/docs/api-reference)
- [OneBot 11 Standard](https://github.com/botuniverse/onebot-11)
- [NapCat Documentation](https://napneko.github.io/)
