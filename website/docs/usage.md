---
sidebar_position: 4
---

# 使用指南

## 启动机器人

在配置好 `settings.toml` 和 `acl.toml` 后，直接运行程序：

```bash
./teamspeakclaw
```

如果配置正确，您应该会看到类似以下的日志：

```
版本: v0.x.x
GitHub: https://github.com/Dr1mH4X/TeamSpeakClaw
INFO Starting TeamSpeakClaw v0.x.x
INFO Bot ready. Listening for events.
```

此时，机器人应该已经连接到您的 TeamSpeak 服务器。

## 命令行选项

- `--log-level <LEVEL>`: 设置控制台日志级别（error, warn, info, debug, trace），默认为 info。
- `--config generate`: 在 `config/` 目录下生成默认配置文件。
- `--config edit`: 启动交互式配置向导。

## 交互方式

您可以通过以下方式与机器人交互：

1.  **频道聊天**: 在频道发送消息，带上触发前缀。
    -   默认前缀: `!tsclaw`, `!bot`, `@TSClaw`
    -   例如: `!bot 播放周杰伦的夜曲`

2.  **私聊 (推荐)**: 双击机器人进行私聊。
    -   私聊通常不需要前缀（取决于 `respond_to_private` 设置）。
    -   例如: `踢掉那个叫 User123 的人`

## 可用技能 (Skills)

机器人目前支持以下技能（取决于您的权限配置）：

### 🎵 音乐控制 (music_control)

TeamSpeakClaw 支持两种音乐后端：

**模式一：ts3audiobot（默认）**

通过 TS 私信控制 [TS3AudioBot](https://github.com/Splamy/TS3AudioBot)。需确保音乐机器人昵称为 `TS3AudioBot`。

**模式二：tsbot_backend**

通过 HTTP API 控制 [NeteaseTSBot](https://github.com/yichen11818/NeteaseTSBot)。需在 `settings.toml` 中配置 `backend = "tsbot_backend"` 和 `base_url`。

支持的操作：
- **点歌**: "播放 [歌名]"
- **切歌**: "下一首", "切歌"
- **暂停/继续**: "暂停音乐", "继续播放"
- **搜索**: "搜索周杰伦的歌"
- **音量**: "音量调到 50"
- **音效**: 调节立体声、低音、混响等

### 🛡️ 管理功能

- **踢出用户** (kick_client): "把 UserA 踢出服务器"
- **封禁用户** (ban_client): "封禁 UserB 10 分钟"
- **移动用户** (move_client): "把 UserA 移动到频道 12"

### 💬 通讯功能

- **戳一戳** (poke_client): "戳一下 UserA"
- **发送消息** (send_message): "给 UserA 发私信说你好"

#### `send_message` 跨平台路由说明

- TeamSpeak 场景：支持 `mode=private|channel|server`。
- NapCat 场景：默认走 NapCat 原生发送，支持 `mode=private|group`。
- 若希望从 NapCat 显式转发到 TeamSpeak，请传入 `ts_route=true`，此时支持 `mode=private|channel|server`（`private` 需 `clid`）。

### ℹ️ 信息查询

- **查询在线用户** (get_client_list): "现在谁在线？"
- **查询用户信息** (get_client_info): "UserA 的详细信息"

## 常见问题

- **机器人没反应？**
    -   检查是否使用了正确的前缀。
    -   检查后台日志是否有报错。
    -   确认 LLM API Key 是否正确且有余额。

- **提示"没有权限"？**
    -   检查 `acl.toml` 中的配置，确认您的用户组 ID 是否在允许的规则中。

- **音乐功能不工作？**
    -   ts3audiobot 模式：确认 TS3AudioBot 是否在线且昵称是否为 `TS3AudioBot`。
    -   tsbot_backend 模式：确认 NeteaseTSBot 后端服务是否运行，`base_url` 是否正确。
