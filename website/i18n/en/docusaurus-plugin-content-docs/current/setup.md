---
sidebar_position: 2
---

# Download & Installation

## 1. Download

Please visit the GitHub Releases page to download the latest version of TeamSpeakClaw:

[**Download Latest Version**](https://github.com/Dr1mH4X/TeamSpeakClaw/releases/latest)

Select the appropriate file for your operating system (Windows, Linux, macOS).

## 2. Installation

TeamSpeakClaw is a standalone binary application and does not require a complex installation process.

1. Extract the downloaded archive into a folder.
2. Ensure you have read and write permissions for that folder.

## 3. Generate Configuration

Run the following command in your terminal to automatically generate the default configuration files:

```bash
./teamspeakclaw --config generate
```

This will create three configuration files in the `config/` directory:

- `settings.toml` — Core settings (Connection, LLM, bot behavior)
- `acl.toml` — Permission control rules (ACL)
- `prompts.toml` — System prompts and error messages

## 4. Edit Configuration

You can manually modify the configuration files using a text editor, or use the built-in interactive wizard:

```bash
./teamspeakclaw --config edit
```

The wizard will guide you through entering information such as your TeamSpeak ServerQuery account credentials and LLM API Key.

For detailed configuration instructions, please refer to the [Configuration Guide](/docs/configuration).

## 5. Start Service

Once the configuration is complete, simply run the program:

```bash
./teamspeakclaw
```

If configured correctly, the bot will connect to your TeamSpeak server and begin listening for events.
