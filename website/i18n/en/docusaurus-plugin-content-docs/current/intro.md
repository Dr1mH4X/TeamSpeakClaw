---
sidebar_position: 1
---

# Introduction

TeamSpeakClaw is an LLM-powered TeamSpeak assistant.

It connects to your TeamSpeak server via ServerQuery, allowing users to interact with the server using natural language. Whether it's playing music, managing members, or querying information, you just need to "say" it in the channel, and TSClaw will understand your intent and execute the corresponding command automatically.

## ✨ Features

- **🧠 Natural Language Interaction**: Say goodbye to complex command manuals. Just say "Play Jay Chou's songs" or "Kick that troublemaker out", and Claw will understand and execute.
- **🛡️ Fine-grained Access Control (RBAC)**: Built-in powerful permission system. You can configure specific skill permissions for different TeamSpeak user groups (e.g., only admins can kick, regular users can only request songs).
- **🔌 Flexible Skill System**:
    - **Music Control**: Seamless integration with TS3AudioBot NetEase Cloud Music plugin, supporting song requests, skipping, and playlist management.
    - **Server Management**: Supports Kick, Ban, Move users, etc.
    - **Information Query**: Get online user list, server status, etc.
    - **Extensibility**: Easily write custom Skills to extend the bot's capabilities.
- **🤖 Broad Model Support**: Compatible with OpenAI interface format, easily connecting to DeepSeek, Ollama, LocalAI, and other large models.

## 🙏 Credits

This project is inspired by or uses code from the following projects:

- [TS3AudioBot](https://github.com/Splamy/TS3AudioBot)
- [TS3AudioBot-NetEaseCloudmusic-plugin](https://github.com/ZHANGTIANYAO1/TS3AudioBot-NetEaseCloudmusic-plugin)
