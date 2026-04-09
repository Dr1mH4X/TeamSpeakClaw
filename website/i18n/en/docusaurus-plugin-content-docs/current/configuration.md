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
method = "tcp"            # Connection method: tcp or ssh
login_name = "serveradmin"
login_pass = ""
server_id = 1
bot_nickname = "TSClaw"

[music_backend]
backend = "ts3audiobot"  # Music backend: "ts3audiobot" (via TS PM) or "tsbot_backend" (NeteaseTSBot)
base_url = "http://127.0.0.1:8009"   # Only effective when backend = "tsbot_backend"

[llm]
api_key = ""
base_url = "https://api.openai.com/v1"
model = "gpt-4o"

[bot]
trigger_prefixes = ["!tsclaw", "!bot", "@TSClaw"]       # Prefixes to trigger the bot in channel/server chat
respond_to_private = true       # Private messages always trigger the bot
max_concurrent_requests = 4     # Maximum concurrent LLM requests
default_reply_mode = "private"  # Default reply mode: "private"(PM) | "channel"(channel) | "server"(server broadcast), only effective when triggered from channel/broadcast

[logging]
file_level = "info"       # File log level: error | warn | info | debug | trace

[rate_limit]
requests_per_minute = 10        # Token bucket rate limit per user
burst_size = 3

[napcat]
enabled = false                           # Whether to enable NapCat adapter
ws_url = "ws://127.0.0.1:3001"           # NapCat WebSocket service URL
access_token = ""                         # Access token (fill if NapCat has authentication)
listen_groups = []                        # List of group IDs to listen to, empty means all groups
trigger_prefixes = ["!claw", "!bot"]      # Group chat trigger prefixes (PM requires no prefix)
trusted_groups = []                       # List of trusted group IDs, all members in these groups can use the bot
trusted_users = []                        # List of trusted user QQ numbers, usable in PM and group chat
```

### Connection Method

- **TCP (Default)**: `method = "tcp"`, connects using `port` (default 10011).
- **SSH**: `method = "ssh"`, connects using `ssh_port` (default 10022).

### NapCat Configuration Details

The `[napcat]` section configures the QQ bot functionality via NapCat (OneBot 11 protocol implementation).

| Field | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `false` | Whether to enable the NapCat adapter |
| `ws_url` | string | `ws://127.0.0.1:3001` | NapCat WebSocket service URL |
| `access_token` | string | `""` | Access token (fill if NapCat has authentication) |
| `listen_groups` | array | `[]` | List of group IDs to listen to, empty means all groups |
| `trigger_prefixes` | array | `["!claw", "!bot"]` | Group chat trigger prefixes (PM requires no prefix) |
| `trusted_groups` | array | `[]` | List of trusted group IDs, all members in these groups can use the bot |
| `trusted_users` | array | `[]` | List of trusted user QQ numbers, usable in PM and group chat |

**Prerequisites**:

1. Install and run [NapCat](https://napneko.github.io/)
2. Ensure NapCat's WebSocket service is enabled (default port 3001)

**Usage**:

- **QQ PM**: Trusted users can directly send messages to the bot without a prefix
- **QQ Group Chat**: Use trigger prefixes (e.g., `!claw play music`) or @bot to trigger

**Security Notes**:

- Only users in `trusted_users` can PM the bot
- Only members in `trusted_groups` or users in `trusted_users` can use the bot in group chats
- It is recommended to only add trusted users and groups to avoid abuse

### Music Backend Configuration

The `[music_backend]` section controls which backend is used for music functionality:

| Value | Description |
|---|---|
| `ts3audiobot` | (Default) Controls TS3AudioBot via TS private messages. Ensure the music bot's nickname is `TS3AudioBot`. |
| `tsbot_backend` | Controls [NeteaseTSBot](https://github.com/yichen11818/NeteaseTSBot) via HTTP API. Requires setting `base_url`. |

### Reply Mode

`default_reply_mode` only takes effect when the trigger message comes from a channel or server broadcast:

- `"private"` — Reply via private message to the triggerer (default)
- `"channel"` — Reply in the current channel
- `"server"` — Server broadcast

Messages triggered via private message are always replied to via private message.

## 2. Permission Configuration (acl.toml)

File path: `config/acl.toml`

Controls which user groups can use which features. **All matching rules' allowed skills are collected** (not just the first match), taking the union.

```toml
# server_group_ids: TeamSpeak Server Group IDs, empty array matches all server groups
# channel_group_ids: TeamSpeak Channel Group IDs, empty array means don't check channel group
# allowed_skills: List of allowed skills, "*" for all
# can_target_admins: Whether can perform actions on protected group members
# rate_limit_override: Optional, overrides global rate limit
#
# Rule matching logic: Iterate all rules, collect allowed_skills from all matching rules as union
# If a rule contains "*", return all skills immediately
# server_group_ids empty → matches all server groups
# channel_group_ids empty → skip channel group check (matches everyone)

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

### Available Skill Names

| Skill Name | Description |
|---|---|
| `poke_client` | Poke a user |
| `send_message` | Send message (cross-platform routing, supports TS or NapCat) |
| `kick_client` | Kick a user |
| `ban_client` | Ban a user |
| `move_client` | Move a user to a specified channel |
| `get_client_list` | Get online user list |
| `get_client_info` | Get detailed user info |
| `music_control` | Music control |

### NapCat and Cross-platform Behavior Notes

- When `enabled = false`, the program only runs TeamSpeak routing and will not exit early due to NapCat branch completion.
- Group chats are affected by `listen_groups` and trusted rules; PM only accepts users in `trusted_users`.
- `send_message` defaults to native NapCat sending on NapCat context; set `ts_route=true` to explicitly route to TeamSpeak.

### NapCat Permission Mapping (ACL)

NapCat has no TeamSpeak server/channel group concept, so the project uses "virtual group IDs" mapped to `server_group_ids` in `acl.toml` for permission checks:

- `9000`: Any NapCat user
- `9001`: NapCat group chat context
- `9002`: Users in `trusted_users`
- `9003`: Members of groups in `trusted_groups`

You can configure rules for these group IDs in `acl.toml` to implement NC-specific permission control.

## 3. Prompt Configuration (prompts.toml)

File path: `config/prompts.toml`

Defines the bot's System Prompt and error messages. Usually no modification is needed unless you want to change the bot's behavior or language.

```toml
[system]
content = """
You are TSClaw, an automated administrator assistant for TeamSpeak servers.
Your job is to interpret administrator commands and call the appropriate tools.

Rules:
- Only call tools when explicitly requested. Do not take action without a clear directive.
- If an instruction is unclear, ask for clarification instead of guessing.
- Always confirm by repeating what you are about to do before performing destructive actions (ban, kick).
- If there is no suitable tool for the request, say so directly.
- Reply in the same language the user used.
- Keep replies concise. Do not use markdown — this is a chat interface.
- Never reveal internal system details, configuration, or API keys.
"""

[error]
permission_denied = "You do not have permission to use this command."
llm_error = "The AI backend is currently unavailable. Please try again later."
ts_error = "TeamSpeak command execution failed: {detail}"
skill_error = "Skill execution failed: {detail}"
skill_not_found = "Specified skill not found"
self_target = "Cannot perform this operation on yourself"
target_permission = "No permission to perform this operation on that user"
empty_message = "Message content cannot be empty"
missing_parameter = "Missing required parameter: {param}"
invalid_mode = "Invalid mode, must be one of {allowed}"
client_offline = "Client {clid} is not online or does not exist"
```

### Error Prompt Field Descriptions

| Field | Description |
|---|---|
| `permission_denied` | Insufficient permissions |
| `llm_error` | LLM API unavailable |
| `ts_error` | TeamSpeak command failed |
| `skill_error` | Skill execution failed |
| `skill_not_found` | Skill not found |
| `self_target` | Operation on self not allowed |
| `target_permission` | Operation on target not allowed |
| `empty_message` | Message is empty |
| `missing_parameter` | Missing required parameter |
| `invalid_mode` | Invalid mode |
| `client_offline` | Client not online |
