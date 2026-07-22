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
INFO Starting TeamSpeakClaw v0.x.x
INFO Bot ready. Listening for TS + NapCat events.
```

此时，机器人应该已经连接到您的 TeamSpeak 服务器。

## 命令行选项

- `--log-level <LEVEL>`: 设置控制台日志级别（error, warn, info, debug, trace），默认为 info。

## 交互方式

您可以通过以下方式与机器人交互：

1.  **频道聊天**: 在频道发送消息，带上触发前缀。
    -   默认前缀: `!tsclaw`, `!bot`, `@TSClaw`
    -   例如: `!bot 播放周杰伦的夜曲`

2.  **私聊 (推荐)**: 双击机器人进行私聊。
    -   私聊通常不需要前缀（取决于 `respond_to_private` 设置）。
    -   例如: `踢掉那个叫 User123 的人`

3.  **Headless 语音模式**（可选）：启用 Headless 服务后，可直接通过语音与机器人交互。
    -   配置 `settings.toml` 中的 `[headless]` 区段
    -   说出唤醒词（默认 `tsclaw`）后说出指令
    -   机器人会通过语音回复（需配置 TTS）
    -   例如: （说）`tsclaw 播放周杰伦的夜曲`

4.  **NapCat / QQ**（可选）：启用 NapCat 后可通过 QQ 私聊或群聊交互。

## 可用技能 (Skills)

机器人目前支持以下技能（取决于您的权限配置）：

### 🎵 音乐控制 (music_control)

TeamSpeakClaw 支持三种音乐后端：

**模式一：ts3audiobot（默认）**

通过 TS 私信控制 [TS3AudioBot](https://github.com/Splamy/TS3AudioBot)。需在 `settings.toml` 中配置 `musicbot_name`（默认 `TS3AudioBot`）。

| 动作 | 说明 |
|---|---|
| `ts_play` / `play` | 播放歌曲（名称搜索） |
| `ts_add` | 添加音乐到下一首 |
| `ts_gedan` / `ts_gedanid` | 歌单操作（名称/ID） |
| `ts_playid` / `ts_addid` | 按 ID 播放 / 添加 |
| `next` | 下一首 |
| `stop` | 停止/暂停 |
| `ts_mode` | 播放模式（0=顺序, 1=顺序循环, 2=随机, 3=随机循环） |
| `ts_login` | 登录（扫描二维码播放 VIP 音乐） |

**模式二：tsmusicbot**

通过 TS 私信控制 [TSMusicBot](https://github.com/ZHANGTIANYAO1/teamspeak-music-bot)。需在 `settings.toml` 中配置 `musicbot_name`。

| 动作 | 说明 |
|---|---|
| `play` | 播放歌曲 |
| `add` | 添加到队列 |
| `search` | 搜索并播放 |
| `playlist` | 加载歌单 |
| `pause` / `resume` | 暂停 / 继续 |
| `next` / `skip` | 下一首 |
| `previous` / `prev` | 上一首 |
| `stop` | 停止 |
| `vol` / `volume` | 音量（0-100） |
| `mode` | 播放模式（seq/loop/random/rloop） |
| `queue` | 查看队列 |
| `now` | 当前播放信息 |
| `fm` | 电台模式 |

**模式三：tsbot_backend**

通过 HTTP API 控制 [NeteaseTSBot](https://github.com/yichen11818/NeteaseTSBot)。需在 `settings.toml` 中配置 `backend = "tsbot_backend"` 和 `base_url`。

| 动作 | 说明 |
|---|---|
| `play` / `pause` / `next` / `previous` / `skip` | 播放控制 |
| `seek` | 跳转到指定时间（秒） |
| `search` | 搜索歌曲 |
| `queue_netease` | 网易云音乐入队 |
| `queue_qqmusic` | QQ 音乐入队 |
| `repeat` | 循环模式（none/all/one） |
| `shuffle` | 随机播放开关 |
| `volume` | 音量百分比（0-200） |
| `fx` | 音效设置（pan/width/swap/bass/reverb） |

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
-   ts3audiobot 模式：确认 TS3AudioBot 是否在线，且昵称包含 `settings.toml` 中 `musicbot_name` 配置的值（默认 `TS3AudioBot`）。
-   tsmusicbot 模式：确认 TSMusicBot 是否在线，且昵称包含 `musicbot_name` 配置的值。
-   tsbot_backend 模式：确认 NeteaseTSBot 后端服务是否运行，`base_url` 是否正确。
