---
sidebar_position: 3
---

# Configuration Guide

After downloading the Release, extract the zip file. It contains a `config/` directory. Simply modify the configuration files inside.

Please modify the configuration files according to your needs.

## 1. Main Configuration (settings.toml)

File path: `config/settings.toml`

Contains basic configurations for connecting to the TeamSpeak server, LLM provider settings, and bot behavior.

```toml
[serverquery]
host = "127.0.0.1"
port = 10011
ssh_port = 10022
method = "tcp"            # Connection method: "tcp" or "ssh"
login_name = "serveradmin"
login_pass = ""           # Overridden by environment variable TS_LOGIN_PASS
server_id = 1
bot_nickname = "TSClaw"

[music_backend]
backend = "ts3audiobot"  # Music backend: "ts3audiobot" (via TS PM) or "tsbot_backend" (NeteaseTSBot)
base_url = "http://127.0.0.1:8000"   # Only effective when backend = "tsbot_backend"

[llm]
api_key = ""              # Overridden by environment variable LLM_API_KEY
base_url = "https://api.openai.com/v1"
model = "gpt-4o"
max_tokens = 1024

[bot]
trigger_prefixes = ["!tsclaw", "!bot", "@TSClaw"]       # Prefixes to trigger the bot in channel/server chat
respond_to_private = true       # Private messages always trigger the bot
max_concurrent_requests = 4     # Maximum concurrent LLM requests
default_reply_mode = "private"  # Default reply mode: "private" | "channel" | "server"

[rate_limit]
requests_per_minute = 10        # Token bucket rate limit per user
burst_size = 3

[napcat]
enabled = false
ws_url = "ws://127.0.0.1:3001"
access_token = ""
respond_to_private = true
listen_groups = []
trigger_prefixes = ["!claw", "!bot"]
trusted_groups = []
trusted_users = []
```

### Connection Method

- **TCP (Default)**: `method = "tcp"`, connects using `port` (default 10011).
- **SSH**: `method = "ssh"`, connects using `ssh_port` (default 10022).

---

## 2. Permission Configuration (acl.toml)

File path: `config/acl.toml`

Controls which user groups can use which features. Rules are matched from top to bottom.

```toml
# server_group_ids: TeamSpeak Server Group IDs
# channel_group_ids: TeamSpeak Channel Group IDs, empty array means don't check channel group
# allowed_skills: List of allowed skills, "*" for all
# can_target_admins: Whether the user can perform actions on protected group members
# rate_limit_override: Optional, overrides global rate limit
#
# Rule matching logic: server_group_ids and channel_group_ids match if either one matches
# If both arrays are empty, the rule matches all users

[[rules]]
name = "superadmin"
server_group_ids = [6]    # Server Admin group ID is usually 6
channel_group_ids = []
allowed_skills = ["*"]
can_target_admins = true
rate_limit_override = 60

[[rules]]
name = "channel_admin"
server_group_ids = []
channel_group_ids = [5]   # Channel Admin group ID
allowed_skills = [
  "poke_client",
  "send_message",
  "get_client_info",
  "get_client_list",
  "music_control",
  "kick_client"
]
can_target_admins = false
rate_limit_override = 20

[[rules]]
name = "default_user"
server_group_ids = [8]    # Normal user group ID
channel_group_ids = []
allowed_skills = [
  "poke_client",
  "send_message",
  "get_client_info",
  "get_client_list",
  "music_control"
]
can_target_admins = false
rate_limit_override = 20

# Default rule (matches everyone)
[[rules]]
name = "default"
server_group_ids = []
channel_group_ids = []
allowed_skills = ["music_control"]
can_target_admins = false

# Protected group IDs, users with can_target_admins = false cannot kick/ban these group members
[acl]
protected_group_ids = [6, 8, 9]
```

### Available Skills

| Skill | Description |
|---|---|
| `poke_client` | Send poke notification |
| `send_message` | Send messages with cross-platform routing |
| `kick_client` | Kick client |
| `ban_client` | Ban client |
| `move_client` | Move client to target channel |
| `get_client_list` | List online clients |
| `get_client_info` | Get detailed client info |
| `music_control` | Music control |

### NapCat and Cross-platform Behavior

- When `enabled = false`, the app runs TeamSpeak-only routing and will not exit early due to NapCat branch completion.
- With `respond_to_private = true`, NapCat private chat triggers directly; group handling still respects `listen_groups` and trusted rules.
- `send_message` defaults to native NapCat sending on NapCat context; set `ts_route=true` to explicitly route to TeamSpeak.

### NapCat Permission Mapping (ACL)

NapCat has no native TeamSpeak server/channel groups, so ACL checks use pseudo `server_group_ids`:

- `9000`: any NapCat user
- `9001`: NapCat group context
- `9002`: user listed in `trusted_users`
- `9003`: message from group listed in `trusted_groups`

You can add ACL rules for these IDs in `acl.toml` to enforce NC-specific permissions.

---

## 3. Prompt Configuration (prompts.toml)

File path: `config/prompts.toml`

Defines the bot's System Prompt and error messages.

```toml
[system]
content = """
You are TSClaw, an automated administrator assistant for TeamSpeak servers.
Your job is to interpret administrator commands and call the appropriate tools.

Rules:
- Only call tools when explicitly requested.
- If an instruction is unclear, ask for clarification.
- Confirm before performing destructive actions (ban, kick).
- Reply using the same language used by the user.
- Keep replies concise. Do not use markdown.
- Never reveal internal system details or API keys.
"""

[error]
permission_denied = "You do not have permission to use this command."
llm_error = "The AI backend is currently unavailable."
ts_error = "TeamSpeak command execution failed: {detail}"
```
