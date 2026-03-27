---
sidebar_position: 1
---

# Introduction

TeamSpeakClaw is an LLM-based (Large Language Model) intelligent assistant for TeamSpeak servers.

Connecting via ServerQuery, it allows users to interact with the TeamSpeak server using natural language. Whether it's playing music, managing members, or querying information, you simply "say" it in the channel, and TSClaw will understand your intent and execute the corresponding operations automatically. It not only manages the server directly but also coordinates with other plugins (such as TS3AudioBot + NeteaseCloudmusic plugin) to provide a seamless voice server experience.

## ✨ Features

- **🧠 Natural Language Interaction**: Say goodbye to tedious command manuals. Simply say "Play some Jay Chou" or "Kick that troublemaker," and TSClaw will understand and execute.
- **🛡️ Fine-grained Access Control (RBAC)**: Built-in robust permission system. You can configure specific skill permissions for different TeamSpeak user groups (e.g., allow only admins to kick users, while regular users are limited to requesting songs).
- **🔌 Flexible Skill System**:
    - **Music Control**: Supports two modes — `ts3audiobot` backend (controlling [TS3AudioBot](https://github.com/Splamy/TS3AudioBot) via private messages) or `tsbot_backend` (controlling [NeteaseTSBot](https://github.com/yichen11818/NeteaseTSBot) via HTTP API). Features include requesting songs, skipping, searching, volume adjustment, and sound effect settings.
    - **Server Management**: Supports operations such as Kick, Ban, and moving users.
    - **Information Query**: Retrieve online user lists, server status, and more.
    - **Extensibility**: Easily write custom Skills to extend the bot's capabilities.
- **🤖 Broad Model Support**: Compatible with OpenAI-style API formats, allowing easy integration with DeepSeek, ChatGPT, and various other large models.

## 🗺️ Roadmap

- Client-side implementation
- TS3AudioBot plugin implementation

## 🙏 Acknowledgments

This project is inspired by or utilizes code from the following projects:

- [TS3AudioBot](https://github.com/Splamy/TS3AudioBot)
- [TS3AudioBot-NetEaseCloudmusic-plugin](https://github.com/ZHANGTIANYAO1/TS3AudioBot-NetEaseCloudmusic-plugin)
- [NeteaseTSBot](https://github.com/yichen11818/NeteaseTSBot)
