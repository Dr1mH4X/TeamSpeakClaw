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

### LLM Configuration

The `[llm]` section configures an OpenAI-compatible Chat Completions endpoint:

| Field | Type | Default | Description |
|---|---|---|---|
| `api_key` | string | `""` | API key; may be empty for unauthenticated local services |
| `base_url` | string | `https://api.openai.com/v1` | API base URL; only HTTP and HTTPS are supported |
| `model` | string | `gpt-4o` | Model name |
| `omni_model` | bool | `false` | Send speech straight to a multimodal model (audio as `input_audio`, no STT; `model` must accept audio input). Replies are still text; `[headless.tts]` provides spoken replies. Audio turns take up context size and the earliest ones are dropped once the size budget is exceeded |
| `max_context_turns` | integer | `0` | Maximum conversation turns retained per session; `0` disables context |

### Web Search

The `[web_search]` section selects the search service for the `web_search` tool; restart the process after changing it.

| Field | Type | Default | Description |
|---|---|---|---|
| `provider` | string | `exa` | `exa` or `parallel`; other values cause configuration loading to fail |

To use [Parallel Search MCP](https://docs.parallel.ai/integrations/mcp/search-mcp), add or edit this section in `config/settings.toml`. No Parallel account or API key is required:

```toml
[web_search]
provider = "parallel"
```

Anonymous Parallel search uses Fast mode with server-managed search settings, so the tool accepts only `query`; `numResults` and `type` apply only to Exa. The free service has usage limits. Quota and network failures return a tool error to the model without switching providers automatically.

Grant `web_search` to the relevant user groups in `config/acl.toml`, then ask the bot to "Search for the latest stable Rust release and its release notes." In the example ACL, administrators already have this permission through `*`; normal users and guests need it added explicitly. Queries are sent to the selected external search service.

### Web Fetch

`web_fetch` retrieves the body of a specific HTTP(S) URL and converts HTML to Markdown. Grant `web_fetch` to the relevant user groups in `config/acl.toml`; administrators already have it through `*` in the example ACL. Fetching accepts public destinations only: if any DNS answer is loopback, private, link-local, reserved, or a translation address, the whole fetch fails. Only same-origin redirects are followed automatically, up to five hops.

One fetch reads at most 5 MiB of response bytes, decodes at most 100000 characters, and returns at most 20000 model-visible characters; truncation adds a notice. Scripts, styles, hidden nodes, and embedded objects are removed before Markdown conversion; HTML that cannot be converted safely becomes a fixed omission marker. Every result begins with a notice identifying the page content as external, untrusted data.

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
| `ts3audiobot` | (Example configuration) Controls TS3AudioBot via TS private messages. Ensure the music bot's nickname is `TS3AudioBot`. |
| `tsmusicbot` | Controls [TSMusicBot](https://github.com/ZHANGTIANYAO1/teamspeak-music-bot) via TS private messages. |
| `tsbot_backend` | Controls [NeteaseTSBot](https://github.com/yichen11818/NeteaseTSBot) via HTTP API. Requires setting `base_url`. |

`musicbot_name`: the music bot’s TeamSpeak nickname (substring match). That bot’s voice is **not** used for STT and is **not recorded** for voice replay; targeting it with `!replay` returns an explicit not-recorded message.

### Reply Mode

`default_reply_mode` only takes effect when the trigger message comes from a channel or server broadcast:

- `"private"` — Reply via private message to the triggerer (default)
- `"channel"` — Reply in the current channel
- `"server"` — Server broadcast

Messages triggered via private message are always replied to via private message.
Replies triggered from voice STT follow this mode as well.

### Voice Replay

The `[voice_replay]` config section controls TeamSpeak recording-window replay. Enabled by default, with access controlled by the `voice_replay` permission in `acl.toml`; **restart the process** after changing config (no hot reload).

| Field | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `true` | Enable recording replay, skill, and direct commands |
| `window_secs` | integer | `30` | Recording window in seconds; valid range **1–120**; `seconds` above the window is clamped |
| `direct_commands` | array | `["!replay", "!回放"]` | Channel direct-command prefixes |

- The voice replay skill and direct commands (`!replay [N] [@Name]`) share the **same ACL**.
- **Default** `!replay`: mix-replay speakers **already recorded** over `window_secs` (default 30s); the bot itself is excluded. Music bots: see `musicbot_name` above.
- **Empty window**: if nobody is available to replay, the bot says so and does not play blank audio.
- Nicknames accept `<@clid|Name>` / `@clid|Name` / `@Name` / bare nick.
- Per-speaker replay is more sensitive than channel mix; grant `voice_replay` by group in `acl.toml`.
- Replay works with only this switch on (listen-only); voice NL trigger still needs STT.
- Syntax and usage: [usage.md](usage.md).

### System Prompt

The system prompt and TTS style prompt are built into the program and are not exposed as configuration.

## 2. Permission Configuration (acl.toml)

File path: `config/acl.toml`

Controls which user groups can use which features. **All matching rules' allowed skills are collected** (not just the first match), taking the union.

**View full configuration example**: [acl.toml](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/examples/config/acl.toml)

### Server Group IDs

`server_group_ids` holds TeamSpeak server group IDs: the default layout is `6` Server Admin, `7` Normal and `8` Guest, and the example splits permissions along those three groups. Channel groups use a separate numbering ("channel admin" is `5`), so do not mix the two; `protected_group_ids` (default `[6, 7, 8]`) lists the groups whose members cannot be kicked or banned by users without that authority.

### Available Skill Names

| Skill Name | Description |
|---|---|
| `poke_client` | Poke a user |
| `send_message` | Send message (cross-platform routing, supports TS or NapCat) |
| `kick_client` | Kick a user |
| `ban_client` | Ban a user |
| `move_client` | Move a user to a specified channel |
| `get_client_info` | Get detailed user info |
| `music_control` | Music control |
| `web_search` | Search the web for current information |
| `voice_replay` | Voice window replay via `!replay [N] [@Name]`; per-speaker is more sensitive |

### NapCat and Cross-platform Behavior Notes

- When `enabled = false`, the program only runs TeamSpeak routing and will not exit early due to NapCat branch completion.
- Group chats are affected by `listen_groups` and trusted rules; PM only accepts users in `trusted_users`.

### NapCat Permission Mapping (ACL)

NapCat has no TeamSpeak server/channel group concept, so the project uses "virtual group IDs" mapped to `server_group_ids` in `acl.toml` for permission checks:

- `9000`: Any NapCat user
- `9001`: NapCat group chat context
- `9002`: Users in `trusted_users`
- `9003`: Members of groups in `trusted_groups`

You can configure rules for these group IDs in `acl.toml` to implement NC-specific permission control.
