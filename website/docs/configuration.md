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
# Bot 配置
[bot]
nickname = "TSClaw"                       # 机器人名称（ServerQuery 会自动追加随机后缀）
trigger_prefixes = ["!tsclaw", "!bot", "@TSClaw"]       # 在频道/服务器聊天中触发机器人的前缀
respond_to_private = true       # 私聊消息始终触发机器人
default_reply_mode = "channel"  # 默认回复模式: "private"(私聊) | "channel"(频道) | "server"(服务器广播)

# Headless 语音服务配置
[headless]
enabled = false
ts3_host = "127.0.0.1"
ts3_port = 9987
server_password = ""
channel_password = ""
channel_path = ""
channel_id = ""

# ServerQuery 配置
[serverquery]
host = "127.0.0.1"
port = 10011
ssh_port = 10022
method = "tcp"            # 连接方式: tcp 或 ssh
login_name = "serveradmin"
login_pass = ""
server_id = 1

# LLM 配置
[llm]
api_key = ""
base_url = "https://api.openai.com/v1"
model = "gpt-4o"
stream_output = false  # false: 等完整回复后再TTS；true: 边生成边TTS（文字仍完整后一次发送）
omni_model = false    # 全模态模型：true 时自动禁用 TTS/STT，直接使用语音输入输出
max_context_sessions = 3  # 最大会话数（超过时淘汰最旧会话）
max_tool_turns = 3  # 最大工具调用轮数
max_concurrent_requests = 4  # 最大并发 LLM 请求数

[headless.stt]
enabled = false
provider = "openai-compatibility"
base_url = ""          # OpenAI兼容可填 .../v1；whisper.cpp server 模式可填 .../inference
api_key = ""           # 可选；若目标服务要求 token，可在这里配置
model = "tiny"         # 可选；非空时会透传给 STT 接口
language = "zh"
wake_words = ["tsclaw"]
wake_word_required = false

[headless.tts]
enabled = false
provider = "openai-compatibility"   # OpenAI兼容或mimo
base_url = ""
api_key = ""
model = "gpt-4o-mini-tts"
voice = "alloy"

# 限流配置
[rate_limit]
requests_per_minute = 10        # 每个用户的令牌桶限流设置
burst_size = 3

# 日志配置
[logging]
file_level = "info"

# 联动项目配置
# 音乐后端配置
[music_backend]
backend = "ts3audiobot"  # "ts3audiobot"（通过 TS 私信控制）或 "tsbot_backend"（NeteaseTSBot）
base_url = "http://127.0.0.1:8009"   # backend = "tsbot_backend" 时生效

# NapCat 配置（可选，用于 QQ 机器人功能）
# 前置要求：安装并运行 NapCat（https://napneko.github.io/）
[napcat]
enabled = false                           # 是否启用 NapCat 适配器
ws_url = "ws://127.0.0.1:3001"           # NapCat WebSocket 服务地址
access_token = ""                         # 访问令牌（若 NapCat 配置了鉴权则填写）
listen_groups = []                        # 监听的群 ID 列表，空列表表示监听所有群
trigger_prefixes = ["!claw", "!bot"]      # 群聊触发前缀（私聊无需前缀）
trusted_groups = []                       # 信任的群 ID 列表，群内所有成员可使用机器人
trusted_users = []                        # 信任的用户 QQ 号列表，私聊和群聊均可使用
```

### 连接方式

- **TCP（默认）**：`method = "tcp"`，使用 `port`（默认 10011）连接。
- **SSH**：`method = "ssh"`，使用 `ssh_port`（默认 10022）连接。

### NapCat 配置详解

`[napcat]` 区段用于配置 QQ 机器人功能，通过 NapCat（OneBot 11 协议实现）连接 QQ。

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `enabled` | bool | `false` | 是否启用 NapCat 适配器 |
| `ws_url` | string | `ws://127.0.0.1:3001` | NapCat WebSocket 服务地址 |
| `access_token` | string | `""` | 访问令牌（若 NapCat 设置了鉴权则填写） |
| `listen_groups` | 数组 | `[]` | 监听的群 ID 列表，空列表表示监听所有群 |
| `trigger_prefixes` | 数组 | `["!claw", "!bot"]` | 群聊触发前缀（私聊无需前缀） |
| `trusted_groups` | 数组 | `[]` | 信任的群 ID 列表，群内所有成员可使用机器人 |
| `trusted_users` | 数组 | `[]` | 信任的用户 QQ 号列表，私聊和群聊均可使用 |

**前置要求**：

1. 安装并运行 [NapCat](https://napneko.github.io/)
2. 确保 NapCat 的 WebSocket 服务已启用（默认端口 3001）

**使用方式**：

- **QQ 私聊**：信任用户直接发送消息给机器人，无需前缀
- **QQ 群聊**：使用触发前缀（如 `!claw 播放音乐`）或 @机器人 触发

**安全说明**：

- 仅 `trusted_users` 中的用户可以私聊机器人
- 仅 `trusted_groups` 中的群成员或 `trusted_users` 中的用户可以在群聊中使用机器人
- 建议仅添加信任的用户和群组，避免滥用

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
语音 STT 触发后的回复也遵循该模式。

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

# TTS 风格提示配置 (用于 MiMo TTS API)
[tts]
style_prompt = "Natural, friendly tone, moderate pace."
```

### TTS 风格提示配置

| 字段 | 说明 |
|---|---|
| `style_prompt` | MiMo TTS 风格提示，控制语音的语调、语速、情感表达等 |

当使用 MiMo TTS provider 时，`style_prompt` 会作为 `user` 消息发送给 TTS API，用于指导语音生成的风格。例如：
- `"Natural, friendly tone, moderate pace."` — 自然友好的语调
- `"Bright, bouncy, slightly sing-song tone — like you are bursting with good news."` — 欢快激动的语调
- `"Calm, soothing tone, slow pace."` — 平静舒缓的语调

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
