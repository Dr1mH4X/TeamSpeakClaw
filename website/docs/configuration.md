---
sidebar_position: 3
---

# 配置指南

下载 Release 后，解压 zip 文件，内含 `config/` 目录。直接修改其中的配置文件即可。

请根据您的需求修改配置文件。

## 1. 主配置 (settings.toml)

文件路径: `config/settings.toml`

包含连接 TeamSpeak 服务器、LLM 提供商设置和机器人行为的基本配置。

**查看完整配置示例**：[settings.toml](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/examples/config/settings.toml)

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

### Headless TTS 配置

`[headless.tts]` 区段中的 `always_tts` 配置项控制是否始终使用语音合成：

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `always_tts` | bool | `false` | 始终使用 TTS：当启用时，来自其他平台（如 NapCat/QQ）的消息也会通过 headless 播放语音回复 |

**使用场景**：
- 当您希望所有平台的回复都通过 TeamSpeak 语音播报时，设置 `always_tts = true`
- 需要同时启用 `headless.enabled = true` 和 `headless.tts.enabled = true`

## 2. 权限配置 (acl.toml)

文件路径: `config/acl.toml`

控制哪些用户组可以使用哪些功能。**所有匹配规则的允许技能会合并收集**（非首个匹配），取并集。

**查看完整配置示例**：[acl.toml](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/examples/config/acl.toml)

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

**查看完整配置示例**：[prompts.toml](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/examples/config/prompts.toml)

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
