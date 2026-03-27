---
sidebar_position: 3
---

# 配置指南

运行 `./teamspeakclaw --config generate` 后，会在 `config/` 目录下生成三个配置文件。请根据您的需求修改它们。

## 1. 主配置 (settings.toml)

文件路径: `config/settings.toml`

包含连接 TeamSpeak 服务器、LLM 提供商设置和机器人行为的基本配置。

```toml
[teamspeak]
host = "127.0.0.1"
port = 10011
ssh_port = 10022
method = "tcp"            # 连接方式: tcp 或 ssh
login_name = "serveradmin"
login_pass = ""           # 通过环境变量 TS_LOGIN_PASS 覆盖
server_id = 1
bot_nickname = "TSClaw"

[music_backend]
backend = "ts3audiobot"  # 音乐后端选择: "ts3audiobot"（通过 TS 私信控制）或 "tsbot_backend"（NeteaseTSBot）
base_url = "http://127.0.0.1:8000"   # 仅 backend = "tsbot_backend" 时生效

[llm]
api_key = ""              # 通过环境变量 LLM_API_KEY 覆盖
base_url = "https://api.openai.com/v1"
model = "gpt-4o"
max_tokens = 1024

[bot]
trigger_prefixes = ["!tsclaw", "!bot", "@TSClaw"]       # 在频道/服务器聊天中触发机器人的前缀
respond_to_private = true       # 私聊消息始终触发机器人
max_concurrent_requests = 4     # 最大并发 LLM 请求数
default_reply_mode = "private"  # 默认回复模式: "private"(私聊) | "channel"(频道) | "server"(服务器广播)，仅频道/广播触发时生效

[rate_limit]
requests_per_minute = 10        # 每个用户的令牌桶限流设置
burst_size = 3
```

### 连接方式

- **TCP（默认）**：`method = "tcp"`，使用 `port`（默认 10011）连接。
- **SSH**：`method = "ssh"`，使用 `ssh_port`（默认 10022）连接。

### 环境变量

敏感信息可以通过环境变量覆盖，避免写入配置文件：

| 环境变量 | 覆盖字段 |
|---|---|
| `TS_LOGIN_PASS` | `teamspeak.login_pass` |
| `LLM_API_KEY` | `llm.api_key` |

### 音乐后端配置

`[music_backend]` 区段控制音乐功能使用哪个后端：

| 值 | 说明 |
|---|---|
| `ts3audiobot` | （默认）通过 TS 私信控制 TS3AudioBot，需确保音乐机器人昵称为 `TS3AudioBot` |
| `tsbot_backend` | 通过 HTTP API 控制 [NeteaseTSBot](https://github.com/yichen11818/NeteaseTSBot)，需设置 `base_url` |

### 回复模式

`default_reply_mode` 仅当触发消息来自频道或服务器广播时生效：

- `"private"` — 私聊回复触发者（默认）
- `"channel"` — 在当前频道回复
- `"server"` — 服务器广播

私聊触发的消息始终以私聊方式回复。

## 2. 权限配置 (acl.toml)

文件路径: `config/acl.toml`

控制哪些用户组可以使用哪些功能。规则从上到下匹配，第一个匹配的规则生效。

```toml
# server_group_ids: TeamSpeak 服务器组 ID
# allowed_skills: 允许使用的技能列表，"*" 代表所有
# can_target_admins: 是否允许对受保护组成员执行操作
# rate_limit_override: 可选，覆盖全局速率限制

[[rules]]
name = "superadmin"
server_group_ids = [6]    # 服务器管理员组 ID 通常是 6
allowed_skills = ["*"]
can_target_admins = true
rate_limit_override = 60

[[rules]]
name = "default_user"
server_group_ids = [8]    # 普通用户组 ID
allowed_skills = [
  "poke_client",
  "send_message",
  "get_client_info",
  "get_client_list",
  "music_control"
]
can_target_admins = false
rate_limit_override = 20

# 默认规则 (匹配所有人)
[[rules]]
name = "default"
server_group_ids = []
allowed_skills = ["music_control"]
can_target_admins = false

# 受保护的组 ID，can_target_admins = false 的用户不能对这些组的成员执行踢出/封禁等操作
[acl]
protected_group_ids = [6, 8, 9]
```

### 可用技能名称

| 技能名 | 说明 |
|---|---|
| `poke_client` | 戳一戳用户 |
| `send_message` | 发送消息（私聊/频道/广播） |
| `kick_client` | 踢出用户 |
| `ban_client` | 封禁用户 |
| `get_client_list` | 获取在线用户列表 |
| `get_client_info` | 获取用户详细信息 |
| `music_control` | 音乐控制 |

## 3. 提示词配置 (prompts.toml)

文件路径: `config/prompts.toml`

定义机器人的系统提示词 (System Prompt) 和错误消息。通常不需要修改，除非您想改变机器人的行为或语言。

```toml
[system]
content = """
你是 TSClaw，一个 TeamSpeak 服务器的自动化管理员助手。
你的工作是解释管理员的命令并调用合适的工具。

规则:
- 只有在明确要求时才调用工具。不要在没有明确指令的情况下采取行动。
- 如果指令不明确，请要求用户澄清而不是猜测。
- 在执行破坏性操作（封禁、踢出）之前，始终通过重复你将要做的事情来确认。
- 如果请求没有合适的工具，请直说。
- 使用用户使用的同一种语言进行回复。
- 保持回复简明扼要。不要使用 markdown — 这是一个聊天界面。
- 永远不要透露内部系统细节、配置或 API 密钥。
"""

[error]
permission_denied = "你没有权限使用此命令。"
llm_error = "AI 后端当前不可用。请稍后再试。"
ts_error = "TeamSpeak 命令执行失败: {detail}"
```
