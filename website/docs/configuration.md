---
sidebar_position: 3
---

# 配置指南

下载 Release 后，解压 zip 文件，内含 `config/` 目录。直接修改其中的配置文件即可。

请根据您的需求修改配置文件。

## 1. 主配置 (settings.toml)

文件路径: `config/settings.toml`

包含连接 TeamSpeak 服务器、LLM 提供商设置和机器人行为的基本配置。

```toml
[serverquery]
host = "127.0.0.1"
port = 10011
ssh_port = 10022
method = "tcp"            # 连接方式: tcp 或 ssh
login_name = "serveradmin"
login_pass = ""
server_id = 1
bot_nickname = "TSClaw"

[music_backend]
backend = "ts3audiobot"  # 音乐后端选择: "ts3audiobot"（通过 TS 私信控制）或 "tsbot_backend"（NeteaseTSBot）
base_url = "http://127.0.0.1:8009"   # 仅 backend = "tsbot_backend" 时生效

[llm]
api_key = ""
base_url = "https://api.openai.com/v1"
model = "gpt-4o"
max_tokens = 1024

[bot]
trigger_prefixes = ["!tsclaw", "!bot", "@TSClaw"]       # 在频道/服务器聊天中触发机器人的前缀
respond_to_private = true       # 私聊消息始终触发机器人
max_concurrent_requests = 4     # 最大并发 LLM 请求数
default_reply_mode = "private"  # 默认回复模式: "private"(私聊) | "channel"(频道) | "server"(服务器广播)，仅频道/广播触发时生效

[logging]
file_level = "info"       # 文件日志级别: error | warn | info | debug | trace

[rate_limit]
requests_per_minute = 10        # 每个用户的令牌桶限流设置
burst_size = 3

[napcat]
enabled = false
ws_url = "ws://127.0.0.1:3001"
access_token = ""
listen_groups = []
trigger_prefixes = ["!claw", "!bot"]
trusted_groups = []
trusted_users = []
```

### 连接方式

- **TCP（默认）**：`method = "tcp"`，使用 `port`（默认 10011）连接。
- **SSH**：`method = "ssh"`，使用 `ssh_port`（默认 10022）连接。

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

控制哪些用户组可以使用哪些功能。**所有匹配规则的允许技能会合并收集**（非首个匹配），取并集。

```toml
# server_group_ids: TeamSpeak 服务器组 ID，空数组匹配所有服务器组
# channel_group_ids: TeamSpeak 频道组 ID，空数组表示不检查频道组
# allowed_skills: 允许使用的技能列表，"*" 代表所有
# can_target_admins: 是否允许对受保护组成员执行操作
# rate_limit_override: 可选，覆盖全局速率限制
#
# 规则匹配逻辑：遍历所有规则，收集所有匹配规则的 allowed_skills 取并集
# 如果规则包含 "*"，立即返回全部技能
# server_group_ids 为空 → 匹配所有服务器组
# channel_group_ids 为空 → 跳过频道组检查（匹配所有人）

[[rules]]
name = "superadmin"
server_group_ids = [6]    # 服务器管理员组 ID 通常是 6
channel_group_ids = []
allowed_skills = ["*"]
can_target_admins = true
rate_limit_override = 60

[[rules]]
name = "channel_admin"
server_group_ids = []
channel_group_ids = [5]   # 频道管理员组 ID
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
server_group_ids = [8]    # 普通用户组 ID
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

# 默认规则 (匹配所有人)
[[rules]]
name = "default"
server_group_ids = []
channel_group_ids = []
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
| `send_message` | 发送消息（跨平台路由，支持 TS 或 NapCat） |
| `kick_client` | 踢出用户 |
| `ban_client` | 封禁用户 |
| `move_client` | 移动用户到指定频道 |
| `get_client_list` | 获取在线用户列表 |
| `get_client_info` | 获取用户详细信息 |
| `music_control` | 音乐控制 |

### NapCat 与跨平台行为说明

- `enabled = false` 时，程序仅运行 TeamSpeak 路由，不会因 NapCat 分支提前退出。
- 群聊受 `listen_groups` 与 trusted 规则影响；私聊仅接受 `trusted_users` 列表中的用户。
- `send_message` 在 NapCat 上默认走 NapCat 发送；如需显式走 TeamSpeak，请在工具参数里传 `ts_route=true`。

### NapCat 权限映射（ACL）

NapCat 不存在 TeamSpeak 的服务器组/频道组概念，因此项目在权限检查时使用"虚拟组 ID"映射到 `acl.toml` 的 `server_group_ids`：

- `9000`：任意 NapCat 用户
- `9001`：NapCat 群聊上下文
- `9002`：`trusted_users` 中的用户
- `9003`：`trusted_groups` 中群成员

您可以在 `acl.toml` 中为这些组配置规则，实现 NC 专用权限控制。

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
skill_error = "技能执行失败: {detail}"
skill_not_found = "未找到指定的技能"
self_target = "不能对自己执行此操作"
target_permission = "无权对该用户执行此操作"
empty_message = "消息内容不能为空"
missing_parameter = "缺少必要参数: {param}"
invalid_mode = "无效的模式，必须是 {allowed}"
client_offline = "客户端 {clid} 在线或不存在"
```

### 错误提示字段说明

| 字段 | 说明 |
|---|---|
| `permission_denied` | 权限不足 |
| `llm_error` | LLM API 不可用 |
| `ts_error` | TeamSpeak 命令失败 |
| `skill_error` | 技能执行失败 |
| `skill_not_found` | 未找到技能 |
| `self_target` | 不允许对自己操作 |
| `target_permission` | 不允许对该目标操作 |
| `empty_message` | 消息为空 |
| `missing_parameter` | 缺少必要参数 |
| `invalid_mode` | 无效模式 |
| `client_offline` | 客户端不在线 |
