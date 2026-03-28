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
cargo run -- --config generate   # Generate default configuration
cargo run                         # Start the bot
```

### Setup Development Environment

A configuration file must be generated upon the first run. For details, see the [Configuration Guide](/docs/configuration).

## Project Architecture

### Directory Structure

```
src/
├── main.rs           # Entry point: Initializes components, starts event loop
├── cli.rs            # CLI argument parsing and configuration wizard
├── router.rs         # Event Router: Coordinates modules to handle user messages
├── adapter/          # TeamSpeak Adapter Layer
│   ├── mod.rs
│   ├── connection.rs # Connection management (TCP/SSH)
│   ├── command.rs    # Command building
│   └── event.rs      # Event parsing
├── config/           # Configuration loading and validation
│   └── mod.rs
├── llm/              # LLM Integration Layer
│   ├── mod.rs
│   ├── engine.rs     # LLM Engine encapsulation
│   └── provider.rs   # Provider trait and OpenAI implementation
├── permission/       # Access Control (ACL)
│   ├── mod.rs
│   └── gate.rs       # Permission gating logic
└── skills/           # Skill System
    ├── mod.rs        # Skill trait and registry
    ├── communication.rs
    ├── information.rs
    ├── moderation.rs
    └── music.rs
```

### Data Flow

```
User Message → TsAdapter → EventRouter → LlmEngine
                                           ↓
                                     Tool Call Request
                                           ↓
                                 PermissionGate (Auth Check)
                                           ↓
                                  SkillRegistry → Skill.execute()
                                           ↓
                                  Result → LlmEngine → TsAdapter → Reply to User
```

## Core Modules Detail

### adapter — TeamSpeak Adapter

Handles low-level communication with the TeamSpeak server.

**Core Structure**: `src/adapter/connection.rs:153-158`

```rust
pub struct TsAdapter {
    writer: Mutex<tokio::io::WriteHalf<TsStream>>,
    event_tx: broadcast::Sender<TsEvent>,
    bot_clid: AtomicU32,
}
```

**Connection Methods**:
- TCP (Default): `method = "tcp"`
- SSH: `method = "ssh"`

**Event Types**: `src/adapter/event.rs`

| Event | Description |
|---|---|
| `TsEvent::TextMessage` | Text message received |
| `TsEvent::ClientEnterView` | Client entered view |
| `TsEvent::ClientLeftView` | Client left view |

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

### permission — Access Control

Access control based on TeamSpeak Server Groups and Channel Groups.

**Core Methods**:
- `get_allowed_skills(&self, caller_groups: &[u32], caller_channel_group_id: u32) -> Vec<String>`: Returns a list of skills permitted for the user.
- `can_target(&self, caller_groups: &[u32], caller_channel_group_id: u32, target_groups: &[u32]) -> bool`: Prevents regular users from performing actions (kick/ban) on administrative groups.

**Rule Matching Logic**: Server groups and channel groups match if either one matches. If both arrays are empty, the rule matches all users.

### router — Event Router

Coordinates all modules to handle user messages, implementing the complete dialogue workflow.

**Message Processing Flow**: `src/router.rs:115-286`

1. Filter out self-messages.
2. Determine response trigger (Private message or Prefix).
3. Retrieve user server groups and channel group.
4. Prepare LLM context.
5. First LLM call.
6. Execute tool calls (if any).
7. Second LLM call (containing tool results).
8. Send reply.

## Skill Development

### Skill Trait

All skills must implement the `Skill` trait: `src/skills/mod.rs:26-31`

```rust
#[async_trait]
pub trait Skill: Send + Sync {
    /// Skill name (Unique identifier)
    fn name(&self) -> &'static str;

    /// Skill description (For LLM understanding)
    fn description(&self) -> &'static str;

    /// Parameter JSON Schema
    fn parameters(&self) -> Value;

    /// Execution logic
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value>;
}
```

### ExecutionContext

Context available during skill execution: `src/skills/mod.rs:16-23`

```rust
pub struct ExecutionContext<'a> {
    pub adapter: Arc<TsAdapter>,                // TeamSpeak Adapter
    pub clients: &'a DashMap<u32, ClientInfo>,  // Online client list
    pub caller_id: u32,                         // Caller client ID
    pub caller_groups: Vec<u32>,                // Caller server groups
    pub caller_channel_group_id: u32,           // Caller channel group ID
    pub gate: Arc<PermissionGate>,              // Permission gate
    pub config: Arc<AppConfig>,                 // Application config
}
```

### Adding a New Skill

1. **Create the file**: Create a new file or extend an existing one in `src/skills/`.
2. **Implement Skill**: Define your struct and implement the `Skill` trait.
3. **Register**: Add the module declaration and register it in `src/skills/mod.rs`.
4. **Configure ACL**: Add the skill to `config/acl.toml`.

### Best Practices

1. **Naming**: Use `snake_case` (e.g., `kick_client`).
2. **Validation**: Validate required parameters within `execute`.
3. **Error Handling**: Return meaningful error messages.
4. **Target Check**: Use `ctx.gate.can_target()` to verify permissions for administrative actions.

## Permission System

### Configuration Structure

`config/acl.toml` uses top-down rule matching:

```toml
[[rules]]
name = "rule_name"
server_group_ids = [6]          # Server group ID list, empty array matches everyone
channel_group_ids = [5]         # Channel group ID list, empty array means don't check channel group
allowed_skills = ["skill_name"] # Allowed skills, "*" means all
can_target_admins = true        # Whether can target protected group members

[acl]
protected_group_ids = [6, 8, 9] # Protected server group IDs
```

### Permission Evaluation Flow

1. Iterate through rule list
2. Check if caller's server group matches (server_group_ids empty array matches everyone)
3. Check if caller's channel group matches (channel_group_ids empty array matches everyone)
4. Server group and channel group match if either one matches
5. Collect allowed skills from matching rules
6. If rule contains `"*"` immediately return all skills
7. Before executing operation on target, call `can_target()` to check

## Code Standards

- **Language**: Visible output and documentation should prefer Chinese (or follow existing project locale).
- **Type Safety**: Prefer strong types over raw JSON manipulation.
- **Error Handling**: Use `anyhow::Result` for readable error chains.
- **Compiler Warnings**: Do not suppress warnings with `allow(dead_code)`; remove or refactor unused code instead.

## Contribution Process

1. Fork the repository.
2. Create a feature branch: `git checkout -b feature/my-feature`.
3. Follow the code standards.
4. Ensure all tests pass: `cargo test`.
5. Format code: `cargo fmt`.
6. Submit a Pull Request.

**Commit Format**: `type: short description` (e.g., `feat: add mute skill`).
