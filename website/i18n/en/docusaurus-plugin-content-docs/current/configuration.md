---
sidebar_position: 3
---

# Configuration Guide

After downloading the Release, extract the zip file. It contains a `config/` directory. Simply modify the configuration files inside.

Please modify the configuration files according to your needs.

## 1. Main Configuration (settings.toml)

File path: `config/settings.toml`

Contains basic configurations for connecting to the TeamSpeak server, LLM provider settings, and bot behavior.

**View full configuration example**: [settings.toml](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/examples/config/settings.toml)

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
Replies triggered from voice STT follow this mode as well.

## 2. Permission Configuration (acl.toml)

File path: `config/acl.toml`

Controls which user groups can use which features. **All matching rules' allowed skills are collected** (not just the first match), taking the union.

**View full configuration example**: [acl.toml](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/examples/config/acl.toml)

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

**View full configuration example**: [prompts.toml](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/examples/config/prompts.toml)

### TTS Style Prompt Configuration

| Field | Description |
|---|---|
| `style_prompt` | MiMo TTS style prompt, controls voice tone, pace, and emotional expression |

When using MiMo TTS provider, `style_prompt` is sent as the `user` message to the TTS API to guide the voice generation style. Examples:
- `"Natural, friendly tone, moderate pace."` — Natural and friendly tone
- `"Bright, bouncy, slightly sing-song tone — like you are bursting with good news."` — Cheerful and excited tone
- `"Calm, soothing tone, slow pace."` — Calm and soothing tone

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
