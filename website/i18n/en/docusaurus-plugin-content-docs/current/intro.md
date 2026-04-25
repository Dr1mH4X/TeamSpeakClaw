---
sidebar_position: 1
---

# Introduction

TeamSpeakClaw is an LLM-based intelligent assistant for TeamSpeak servers.

It connects to your TeamSpeak server via ServerQuery, allowing users to interact with the server using natural language. Whether it's playing music, managing members, or querying information, you simply "say" it in the channel, and TSClaw will understand your intent and automatically execute the corresponding operations. It not only manages the server directly but also coordinates with other music bot plugins to provide a seamless voice server experience.

## ✨ Features

- **🧠 Natural Language Interaction**: Say goodbye to tedious command manuals. Simply say "Play some Jay Chou" or "Kick that troublemaker," and TSClaw will understand and execute.
- **🎙️ Headless Voice Service**: Supports headless voice mode with STT (Speech-to-Text) and TTS (Text-to-Speech) capabilities. Configurable wake words, supports OpenAI-compatible voice services.
- **🛡️ Fine-grained Access Control (RBAC)**: Built-in robust permission system. You can configure specific skill permissions for different TeamSpeak user groups (e.g., allow only admins to kick users, while regular users are limited to requesting songs).
- **🔌 Flexible Skill System (Skills)**:
    - **Music Control**: Supports two modes — `ts3audiobot` backend (controlling [TS3AudioBot](https://github.com/Splamy/TS3AudioBot) via private messages) or `tsbot_backend` (controlling [NeteaseTSBot](https://github.com/yichen11818/NeteaseTSBot) via HTTP API, supporting NetEase Cloud Music and QQ Music). Features include requesting songs, skipping, searching, volume adjustment, and sound effect settings.
    - **Server Management**: Supports operations such as Kick, Ban, and moving users.
    - **Information Query**: Retrieve online user lists, server status, and more.
    - **Extensibility**: Easily write custom Skills to extend the bot's capabilities.
- **🤖 Broad Model Support**: Compatible with OpenAI-style API formats, allowing easy integration with DeepSeek, ChatGPT, and various other large models.
- **📱 QQ Bot Integration**: Supports QQ private and group chat interactions via NapCat adapter and OneBot 11 protocol, enabling cross-platform message routing.

## 🙏 Acknowledgments

This project is inspired by or utilizes code from the following projects:

- [TS3AudioBot](https://github.com/Splamy/TS3AudioBot)
- [TS3AudioBot-NetEaseCloudmusic-plugin](https://github.com/ZHANGTIANYAO1/TS3AudioBot-NetEaseCloudmusic-plugin)
- [NeteaseTSBot](https://github.com/yichen11818/NeteaseTSBot)
