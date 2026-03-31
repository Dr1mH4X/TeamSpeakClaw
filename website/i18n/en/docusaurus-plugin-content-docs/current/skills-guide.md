---
sidebar_position: 6
---

# Skills Development Guide

This guide walks you through creating a new Skill from scratch, including trait implementation, cross-platform support, and registration.

## Architecture Overview

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

### Execution Contexts

| Context Type | Purpose | Built By |
|---|---|---|
| `ExecutionContext` | TeamSpeak native execution | Router directly |
| `NcExecutionContext` | NapCat native execution | Router directly |
| `UnifiedExecutionContext` | Cross-platform execution | `from_ts()` / `from_nc()` |

### Trait Definition

```rust
#[async_trait]
pub trait Skill: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn parameters(&self) -> Value;

    // Required: TeamSpeak native execution
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value>;

    // Optional: NapCat native execution (defaults to "not supported")
    async fn execute_nc(&self, args: Value, ctx: &NcExecutionContext) -> Result<Value>;

    // Optional: Unified execution (defaults to "not supported")
    async fn execute_unified(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value>;
}
```

**Call priority**: `execute_unified` → fallback to `execute` (TS side) or `execute_nc` (NC side).

## Step-by-Step: Create a New Skill

### Step 1: Define the Struct and Implement `execute`

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
        "Echo the input message"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "msg": { "type": "string", "description": "The message to echo" }
            },
            "required": ["msg"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value> {
        let msg = args["msg"].as_str().unwrap_or("");
        Ok(json!({ "status": "ok", "message": format!("Echo: {}", msg) }))
    }
}
```

### Step 2: Implement `execute_unified` (Cross-Platform)

All skills that need cross-platform invocation should implement `execute_unified`. Use `ctx.to_ts_ctx()?` to convert `UnifiedExecutionContext` back to `ExecutionContext` in one line:

```rust
    async fn execute_unified(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value> {
        info!("EchoSkill: unified execution, platform={:?}", ctx.platform);

        match ctx.platform {
            Platform::TeamSpeak => {
                // Restore TS context via to_ts_ctx()
                let ts_ctx = ctx.to_ts_ctx()?;
                self.execute(args, &ts_ctx).await
            }
            Platform::NapCat => {
                // Handle NC side directly
                let msg = args["msg"].as_str().unwrap_or("");
                Ok(json!({
                    "status": "ok",
                    "message": format!("QQ echo: {}", msg),
                    "platform": "napcat"
                }))
            }
        }
    }
```

`to_ts_ctx()` automatically checks that `ts_adapter` and `ts_clients` are available, returning a meaningful error if not.

### Step 3: Register the Skill

In `src/skills/mod.rs`:

```rust
pub mod example;  // 1. Add module declaration

pub fn with_defaults() -> Self {
    // ...
    registry.register(Box::new(example::EchoSkill));  // 2. Register
    // ...
}
```

### Step 4: Configure ACL

Grant the skill to a user group in `config/acl.toml`:

```toml
[[rules]]
name = "echo_rule"
server_group_ids = [8]       # Regular user group
allowed_skills = ["echo"]
can_target_admins = false
```

## Common Patterns

### Permission Check & Self-Target Protection

For skills that operate on other users (kick, ban, move), use `ctx.gate.can_target()` and prevent self-targeting:

```rust
async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value> {
    let clid = args["clid"].as_u64()
        .ok_or_else(|| anyhow::anyhow!(ctx.error_prompts.missing_parameter.replace("{param}", "clid")))? as u32;

    // Self-target protection
    if clid == ctx.caller_id {
        return Err(anyhow::anyhow!(ctx.error_prompts.self_target.clone()));
    }

    // Permission check
    let target_groups = ctx.clients.get(&clid)
        .map(|c| c.server_groups.clone())
        .unwrap_or_default();
    if !ctx.gate.can_target(&ctx.caller_groups, ctx.caller_channel_group_id, &target_groups) {
        return Err(anyhow::anyhow!(ctx.error_prompts.target_permission.clone()));
    }

    // ... execute operation
}
```

### Cross-Platform Forwarding

When called from NC but the operation needs to execute on TS (e.g., music control), use `ctx.to_ts_ctx()?` directly:

```rust
Platform::NapCat => {
    let ts_ctx = ctx.to_ts_ctx()?;  // Auto-validates adapter availability
    ts_ctx.adapter.send_raw(&some_command).await?;
    Ok(json!({ "status": "ok" }))
}
```

### Parameter Error Prompts

Use `ctx.error_prompts` templates for consistent user experience:

```rust
let clid = args["clid"].as_u64().ok_or_else(|| {
    anyhow::anyhow!(ctx.error_prompts.missing_parameter.replace("{param}", "clid"))
})? as u32;
```

## Existing Skills

| Skill Name | File | Cross-Platform | Description |
|---|---|---|---|
| `poke_client` | `communication.rs` | ✅ | Poke a user |
| `send_message` | `communication.rs` | ✅ | Send message (supports TS↔NC routing) |
| `kick_client` | `moderation.rs` | ✅ | Kick a user from the server |
| `ban_client` | `moderation.rs` | ✅ | Ban a user from the server |
| `move_client` | `moderation.rs` | ✅ | Move a user to another channel |
| `get_client_list` | `information.rs` | ✅ | Get list of online users |
| `get_client_info` | `information.rs` | ✅ | Get detailed user info |
| `music_control` | `music.rs` | ✅ | Music player control (dual backend) |

## File Organization

Skills are grouped by function in `src/skills/`:

```
src/skills/
├── mod.rs               # Skill trait + context types + SkillRegistry
├── communication.rs     # Communication: poke_client, send_message
├── information.rs       # Information: get_client_list, get_client_info
├── moderation.rs        # Moderation: kick_client, ban_client, move_client
└── music.rs             # Music: music_control
```

Place new skills in the appropriate existing file, or create a new file (e.g., `automation.rs`).

## Best Practices

1. **Naming**: Use `snake_case`, e.g., `kick_client`
2. **Parameters**: Provide complete JSON Schema in `parameters()` — the LLM relies on it
3. **Errors**: Return meaningful errors using `ctx.error_prompts` templates
4. **Cross-platform**: All new skills should implement `execute_unified` using `ctx.to_ts_ctx()?`
5. **Logging**: Add `info!` at the `execute_unified` entry point for debugging
6. **Return values**: Return JSON in `{"status": "ok", ...}` format
