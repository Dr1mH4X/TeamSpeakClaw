---
sidebar_position: 4
---

# User Guide

## Starting the Bot

After configuring `settings.toml` and `acl.toml`, run the program directly:

```bash
./teamspeakclaw
```

If configured correctly, you should see logs similar to the following:

```
INFO Starting TeamSpeakClaw v0.x.x
INFO Bot ready. Listening for TS + NapCat events.
```

At this point, the bot should be connected to your TeamSpeak server.

## Command Line Options

- `--log-level <LEVEL>`: Set the console log level (error, warn, info, debug, trace). Default is `info`.

## Interaction Methods

You can interact with the bot in the following ways:

1.  **Channel Chat**: Send a message in the channel using a trigger prefix.
    -   Default Prefixes: `!tsclaw`, `!bot`, `@TSClaw`
    -   Example: `!bot Play Nocturne by Jay Chou`

2.  **Private Chat (Recommended)**: Double-click the bot to start a private conversation.
    -   Private messages usually do not require a prefix (depending on the `respond_to_private` setting).
    -   Example: `Kick that person named User123`

3.  **Headless Voice Mode** (Optional): After enabling Headless service, you can interact with the bot directly via voice.
    -   Configure the `[headless]` section in `settings.toml`
    -   Say the wake word (default `tsclaw`) followed by your command
    -   The bot will reply via voice (requires TTS configuration)
    -   Example: (Say) `tsclaw play Nocturne by Jay Chou`

4.  **NapCat / QQ** (Optional): Enable NapCat to interact via QQ private messages or group chats.

## Available Skills

The bot currently supports the following skills (depending on your permission configuration):

### 🎵 Music Control (music_control)

TeamSpeakClaw supports three music backends:

**Mode 1: ts3audiobot (Default)**

Controls [TS3AudioBot](https://github.com/Splamy/TS3AudioBot) via TS private messages. Set `musicbot_name` in `settings.toml` (default `TS3AudioBot`).

| Action | Description |
|---|---|
| `ts_play` / `play` | Play a song (search by name) |
| `ts_add` | Add song to next |
| `ts_gedan` / `ts_gedanid` | Playlist by name / ID |
| `ts_playid` / `ts_addid` | Play / add by ID |
| `next` | Next track |
| `stop` | Stop / pause |
| `ts_mode` | Playback mode (0=seq, 1=seq loop, 2=shuffle, 3=shuffle loop) |
| `ts_login` | Login for VIP music (scan QR code) |

**Mode 2: tsmusicbot**

Controls [TSMusicBot](https://github.com/ZHANGTIANYAO1/teamspeak-music-bot) via TS private messages. Set `musicbot_name` in `settings.toml`.

| Action | Description |
|---|---|
| `play` | Play a song |
| `add` | Add to queue |
| `search` | Search and play |
| `playlist` | Load playlist |
| `pause` / `resume` | Pause / resume |
| `next` / `skip` | Next track |
| `previous` / `prev` | Previous track |
| `stop` | Stop |
| `vol` / `volume` | Volume (0-100) |
| `mode` | Playback mode (seq/loop/random/rloop) |
| `queue` | View queue |
| `now` | Current track info |
| `fm` | Radio mode |

**Mode 3: tsbot_backend**

Controls [NeteaseTSBot](https://github.com/yichen11818/NeteaseTSBot) via HTTP API. Requires `backend = "tsbot_backend"` and `base_url` in `settings.toml`.

| Action | Description |
|---|---|
| `play` / `pause` / `next` / `previous` / `skip` | Playback control |
| `seek` | Seek to time (seconds) |
| `search` | Search songs |
| `queue_netease` | Enqueue from NetEase Music |
| `queue_qqmusic` | Enqueue from QQ Music |
| `repeat` | Repeat mode (none/all/one) |
| `shuffle` | Toggle shuffle |
| `volume` | Volume percentage (0-200) |
| `fx` | Sound effects (pan/width/swap/bass/reverb) |

### 🛡️ Administration

- **Kick Client** (kick_client): "Kick UserA from the server"
- **Ban Client** (ban_client): "Ban UserB for 10 minutes"
- **Move Client** (move_client): "Move UserA to channel 12"

### 💬 Communication

- **Poke Client** (poke_client): "Poke UserA"
- **Send Message** (send_message): "Send a private message to UserA saying hello"

#### `send_message` Cross-Platform Routing Notes

- TeamSpeak context: supports `mode=private|channel|server`.
- NapCat context: defaults to native NapCat sending, supports `mode=private|group`.
- To explicitly route from NapCat to TeamSpeak, pass `ts_route=true`; then `mode=private|channel|server` is used (`private` requires `clid`).

### ℹ️ Information Query

- **List Online Users** (get_client_list): "Who is online right now?"
- **Client Info** (get_client_info): "Show detailed info for UserA"

## FAQ

- **The bot is not responding?**
    - Check if you are using the correct prefix.
    - Check the background logs for any error messages.
    - Verify that your LLM API Key is correct and has sufficient balance.

- **Message: "Permission Denied"?**
    - Check the configuration in `acl.toml` to ensure your User Group ID is included in an allowed rule.

- **Music features aren't working?**
-   ts3audiobot mode: Ensure TS3AudioBot is online and its nickname contains the `musicbot_name` value from `settings.toml` (default `TS3AudioBot`).
-   tsmusicbot mode: Ensure TSMusicBot is online and its nickname contains the `musicbot_name` config value.
-   tsbot_backend mode: Ensure the NeteaseTSBot backend service is running and the `base_url` is correct.
