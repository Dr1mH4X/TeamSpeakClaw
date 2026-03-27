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
Version: v0.x.x
GitHub: https://github.com/Dr1mH4X/TeamSpeakClaw
INFO Starting TeamSpeakClaw v0.x.x
INFO Bot ready. Listening for events.
```

At this point, the bot should be connected to your TeamSpeak server.

## Command Line Options

- `--log-level <LEVEL>`: Set the console log level (error, warn, info, debug, trace). Default is `info`.
- `--config generate`: Generates default configuration files in the `config/` directory.
- `--config edit`: Starts the interactive configuration wizard.

## Interaction Methods

You can interact with the bot in the following ways:

1. **Channel Chat**: Send a message in the channel using a trigger prefix.
    - **Default Prefixes**: `!tsclaw`, `!bot`, `@TSClaw`
    - **Example**: `!bot Play Nocturne by Jay Chou`

2. **Private Chat (Recommended)**: Double-click the bot to start a private conversation.
    - Private messages usually do not require a prefix (depending on the `respond_to_private` setting).
    - **Example**: `Kick that person named User123`

## Available Skills

The bot currently supports the following skills (depending on your permission configuration):

### 🎵 Music Control (music_control)

TeamSpeakClaw supports two music backends:

**Mode 1: ts3audiobot (Default)**

Controls [TS3AudioBot](https://github.com/Splamy/TS3AudioBot) via TS private messages. Ensure the music bot's nickname is set to `TS3AudioBot`.

**Mode 2: tsbot_backend**

Controls [NeteaseTSBot](https://github.com/yichen11818/NeteaseTSBot) via HTTP API. Requires setting `backend = "tsbot_backend"` and `base_url` in `settings.toml`.

**Supported Actions:**
- **Request Songs**: "Play [Song Name]"
- **Skip Songs**: "Next song", "Skip"
- **Pause/Resume**: "Pause music", "Resume playback"
- **Search**: "Search for songs by Jay Chou"
- **Volume**: "Set volume to 50"
- **Audio Effects**: Adjust stereo, bass, reverb, etc.

### 🛡️ Administration

- **Kick Client** (kick_client): "Kick UserA from the server"
- **Ban Client** (ban_client): "Ban UserB for 10 minutes"

### 💬 Communication

- **Poke Client** (poke_client): "Poke UserA"
- **Send Message** (send_message): "Send a private message to UserA saying hello"

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
    - **ts3audiobot mode**: Ensure TS3AudioBot is online and its nickname is exactly `TS3AudioBot`.
    - **tsbot_backend mode**: Ensure the NeteaseTSBot backend service is running and the `base_url` is correct.
