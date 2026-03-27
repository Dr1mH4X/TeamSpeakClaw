---
sidebar_position: 4
---

# Usage Guide

## Starting the Bot

After configuring `settings.toml` and `acl.toml`, run the program again.

If configured correctly, you should see logs similar to:

```
INFO Starting TeamSpeakClaw v0.1.0
INFO Bot ready. Listening for events.
```

At this point, the bot should be connected to your TeamSpeak server.

## Interaction

You can interact with the bot in the following ways:

1.  **Channel Chat**: Send a message in the channel where the bot is, with a trigger prefix.
    -   Default prefixes: `!tsclaw`, `!bot`, `@TSClaw`
    -   Example: `!bot Play Jay Chou's Nocturne`

2.  **Private Message (Recommended)**: Double-click the bot to send a private message.
    -   Private messages usually don't need a prefix (depends on `respond_to_private` setting).
    -   Example: `Kick the user named User123`

## Available Skills

The bot currently supports the following skills (depending on your permission configuration):

### 🎵 Music Control
- **Request Song**: "Play [Song Name]"
- **Skip Song**: "Next song", "Skip"
- **Pause/Resume**: "Pause music", "Resume playback"

*(Note: Requires TS3AudioBot integration)*

### 🛡️ Management
- **Kick User**: "Kick UserA out of the channel"
- **Ban User**: "Ban UserB"
- **Move User**: "Move UserC to [Channel Name]"

### ℹ️ Information
- **Query Online Users**: "Who is online now?"
- **Query Server Status**: "How long has the server been running?"

## FAQ

- **Bot not responding?**
    -   Check if you used the correct prefix.
    -   Check the background logs for errors.
    -   Confirm if the LLM API Key is correct and has balance.

- **"Permission denied"?**
    -   Check configuration in `acl.toml` and confirm your user group ID is in the allowed rules.
