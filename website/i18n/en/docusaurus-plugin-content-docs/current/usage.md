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

3.  **NapCat / QQ** (Optional): Enable NapCat to interact via QQ private messages or group chats.

## Available Skills

The bot currently supports the following skills (depending on your permission configuration):

### 🎵 Music Control (music_control)

TeamSpeakClaw supports two music backends:

**Mode 1: ts3audiobot (Default)**

Controls [TS3AudioBot](https://github.com/Splamy/TS3AudioBot) via TS private messages. Ensure the music bot's nickname is set to `TS3AudioBot`.

**Mode 2: tsbot_backend**

Controls [NeteaseTSBot](https://github.com/yichen11818/NeteaseTSBot) via HTTP API. Requires setting `backend = "tsbot_backend"` and `base_url` in `settings.toml`.

Supported actions:

| Action | Description |
|---|---|
| `play` | Play a song (search by keyword) |
| `pause` | Pause / resume playback |
| `next` / `skip` | Skip song |
| `previous` | Previous song |
| `search` | Search and play |
| `repeat` | Repeat mode (none/one/all) |
| `volume` | Adjust volume |
| `fx` | Sound effect settings |
| `ts_play` | TS3AudioBot exclusive play |
| `ts_add` | TS3AudioBot exclusive add to queue |
| `ts_gedan` / `ts_gedanid` | TS3AudioBot playlist operations |
| `ts_playid` / `ts_addid` | TS3AudioBot operations by ID |
| `ts_mode` | TS3AudioBot playback mode |
| `ts_login` | TS3AudioBot login |
| `queue_netease` | tsbot_backend: NetEase playlist enqueue |
| `queue_qqmusic` | tsbot_backend: QQ Music playlist enqueue |

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
    - ts3audiobot mode: Ensure TS3AudioBot is online and its nickname is exactly `TS3AudioBot`.
    - tsbot_backend mode: Ensure the NeteaseTSBot backend service is running and the `base_url` is correct.
