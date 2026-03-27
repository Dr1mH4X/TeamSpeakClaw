---
sidebar_position: 3
---

# Configuration

After the first run, three configuration files will be generated in the `config/` directory. Please modify them according to your needs.

## 1. Main Configuration (settings.toml)

Path: `config/settings.toml`

Contains basic settings for connecting to the TeamSpeak server, LLM provider settings, and bot behavior.

```toml
[teamspeak]
host = "127.0.0.1"        # TeamSpeak server address
port = 10011              # ServerQuery port (default 10011)
ssh_port = 10022          # SSH port (default 10022)
use_ssh = false           # Whether to use SSH connection
login_name = "serveradmin" # ServerQuery login name
login_pass = ""           # ServerQuery password (can also be set via TS_LOGIN_PASS env var)
server_id = 1             # Virtual server ID (usually 1)
bot_nickname = "TSClaw"   # Bot nickname in TS
keepalive_interval_secs = 180
reconnect_max_retries = 10
reconnect_base_delay_ms = 1000

[llm]
provider = "openai"       # Options: openai | anthropic | ollama
api_key = ""              # LLM API Key (can also be set via LLM_API_KEY env var)
base_url = "https://api.openai.com/v1" # API Base URL (e.g. DeepSeek, OpenAI)
model = "gpt-4o"          # Model name
max_tokens = 1024
timeout_secs = 30
retry_max = 3
retry_delay_ms = 500

[bot]
# Prefixes to trigger the bot in channel/server chat
trigger_prefixes = ["!tsclaw", "!bot", "@TSClaw"]
# Whether private messages always trigger the bot
respond_to_private = true
# Max concurrent LLM requests
max_concurrent_requests = 4

[rate_limit]
# Token bucket rate limit per user
requests_per_minute = 10
burst_size = 3

[audit]
enabled = true
log_dir = "./logs"
log_file = "audit.jsonl"

[cache]
refresh_interval_secs = 30
entry_ttl_secs = 300
```

## 2. Permissions Configuration (acl.toml)

Path: `config/acl.toml`

Controls which user groups can use which features. Rules are matched from top to bottom; the first matching rule applies.

```toml
# server_group_ids: TeamSpeak Server Group IDs
# allowed_skills: List of allowed skills, "*" means all

[[rules]]
name = "superadmin"
server_group_ids = [6]    # Server Admin Group ID usually 6
allowed_skills = ["*"]
can_target_admins = true
rate_limit_override = 60

[[rules]]
name = "default_user"
server_group_ids = [8]    # Normal User Group ID
allowed_skills = [
  "poke_client",
  "send_private_msg",
  "music_control"
]
can_target_admins = false
rate_limit_override = 20

# Default rule (matches everyone else)
[[rules]]
name = "default"
server_group_ids = []
allowed_skills = ["music_control"]
can_target_admins = false

[acl]
# Protected Group IDs, normal users cannot kick/ban members of these groups
protected_group_ids = [6, 8, 9]
```

## 3. Prompts Configuration (prompts.toml)

Path: `config/prompts.toml`

Defines the bot's System Prompt and error messages. Usually no need to modify unless you want to change the bot's persona or language.

```toml
[system]
content = """
You are TSClaw, an automated admin assistant for a TeamSpeak server.
...
"""

[error]
permission_denied = "You do not have permission to use this command."
rate_limited = "Too many requests. Please try again later."
# ... other error messages
```
