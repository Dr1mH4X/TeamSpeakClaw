<div align="center">
<h1 align="center" style="margin-top: 0">TeamSpeakClaw</h1>
<p align="center">
<strong>LLM驱动的TeamSpeak机器人</strong>
</p>

[快速开始](#How-to-use)
|
[开发指南](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/docs/Development_CN.md)
|
[快速开始](https://tsclaw.dreamhax.cc)
|
[问题反馈](https://github.com/Dr1mH4X/TeamSpeakClaw/issues)

[![GitHub](https://img.shields.io/badge/-GitHub-181717?logo=github)](github.com/Dr1mH4X/TeamSpeakClaw)
![GitHub License](https://img.shields.io/github/license/Dr1mH4X/TeamSpeakClaw)
[![GitHub release](https://img.shields.io/github/v/release/Dr1mH4X/TeamSpeakClaw?color=blue&label=download&sort=semver)](https://github.com/Dr1mH4X/TeamSpeakClaw/releases/latest)

</div>

TeamSpeakClaw 是一个基于 LLM (大语言模型) 的 TeamSpeak 助手。

它通过 ServerQuery/~~无头TS客户端~~ 连接到您的 TeamSpeak 服务器，允许用户使用自然语言与服务器进行交互。无论是播放音乐、管理成员还是查询信息，您只需在频道中“说”出来，TSClaw 就会理解您的意图并自动执行相应的指令。

## ✨ 功能特性 (Features)

- **🧠 自然语言交互**：告别繁琐的指令手册。直接说“播放周杰伦的歌”或“把那个捣乱的人踢出去”，Claw 就能听懂并执行。
- **🛡️ 细粒度权限控制 (RBAC)**：内置强大的权限系统。您可以为不同的 TeamSpeak 用户组配置特定的技能权限（例如：仅允许管理员使用踢人功能，普通用户仅限点歌）。
- **🔌 灵活的技能系统 (Skills)**：
    - **音乐控制**：无缝对接 TS3AudioBot 网易云插件，支持点歌、切歌、播放列表管理。
    - **服务器管理**：支持踢出 (Kick)、封禁 (Ban)、移动用户等操作。
    - **信息查询**：获取在线用户列表、服务器状态等。
    - **可扩展性**：轻松编写自定义 Skill，扩展机器人的能力边界。
- **🤖 广泛的模型支持**：兼容 OpenAI 接口格式，轻松接入 DeepSeek、Ollama、LocalAI 等多种大模型。

## 🙏 致谢 (Credits)

- [TS3AudioBot](https://github.com/Splamy/TS3AudioBot)
- [TS3AudioBot-NetEaseCloudmusic-plugin](https://github.com/ZHANGTIANYAO1/TS3AudioBot-NetEaseCloudmusic-plugin)
