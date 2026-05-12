<div align="center">
  <img alt="LOGO" src="https://tsclaw.dreamhax.cc/img/tsclaw-400.png" width="200" height="200" />
  <h1>TeamSpeakClaw</h1>

  <img src="https://count.getloli.com/get/@TeamSpeakClaw?theme=booru-lewd" alt=":name" /></p>
  
  <a href="https://github.com/Dr1mH4X/TeamSpeakClaw/actions">
    <img src="https://img.shields.io/github/actions/workflow/status/Dr1mH4X/TeamSpeakClaw/build.yml?style=for-the-badge&label=Build" alt="TeamSpeakClaw Build Status">
  </a>

  <a href="https://github.com/Dr1mH4X/TeamSpeakClaw/releases">
    <img src="https://img.shields.io/github/v/release/Dr1mH4X/TeamSpeakClaw?style=for-the-badge&color=blue&label=Latest%20Release" alt="TeamSpeakClaw Latest Release">
  </a>

  <a href="https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/LICENSE">
    <img src="https://img.shields.io/badge/AGPL--3.0-red?style=for-the-badge" alt="License">
  </a>

  <img src="https://img.shields.io/badge/TeamSpeak-2580C3?style=for-the-badge&logo=teamspeak&logoColor=white" alt="TeamSpeak Support">
</div>

TeamSpeakClaw 是一个基于 LLM (大语言模型) 的 TeamSpeak 智能助手。

它通过 ServerQuery 和 Headless 无头客户端连接到您的 TeamSpeak 服务器，允许用户使用自然语言与服务器进行交互。无论是播放音乐、管理成员还是查询信息，您只需在频道中"说"出来，TSClaw 就会理解您的意图并自动执行相应的操作。它不仅能直接管理服务器，还能与其他插件或机器人协同工作，为您提供无缝的语音服务器体验。

## ✨ 功能特性

- **🧠 自然语言交互**：告别繁琐的指令手册。直接说"播放周杰伦的歌"或"把那个捣乱的人踢出去"。
- **🛡️ 细粒度权限控制**：内置强大的权限系统。您可以为不同的 TeamSpeak 服务器组/用户组配置特定的技能权限。
- **🔌 灵活的技能系统**：
    - **音乐控制**：支持三种模式，包括一个内置API和两个外部机器人后端。
        - **内置（无需额外部署）**：`ncm_api` 内置网易云音乐API，并可选择配置 [UNM](https://github.com/UnblockNeteaseMusic/server) 来解锁变灰歌曲。
        - **外部机器人**：`ts3audiobot` 通过私信控制 [TS3AudioBot](https://github.com/Splamy/TS3AudioBot)。
        - **外部机器人**：`tsbot_backend` 通过 HTTP API 控制 [NeteaseTSBot](https://github.com/yichen11818/NeteaseTSBot) 播放音乐。
        - 支持点歌、切歌、搜索等功能。
    - **服务器管理**：支持踢出 (Kick)、戳一戳 (Poke)、移动用户等操作。
    - **信息查询**：获取在线用户列表、服务器状态等。
    - **etc.**
- **📱 NapCat(QQ) 支持**：
    - **跨平台交互**：通过 QQ 私聊或群聊控制 TeamSpeak 服务器。
    - **WebSocket 连接**：基于 OneBot 11 标准协议，支持断线自动重连。
    - **灵活的触发机制**：支持自定义触发前缀和 @机器人 触发。
    - **细粒度权限**：可配置信任用户和信任群组，确保安全。
- **🤖 广泛的模型支持**：兼容 OpenAI 接口格式，轻松接入 DeepSeek、Xiaomi Mimo 等多种大模型。

## 🚀 文档
 - [用户 & 开发者文档](http://tsclaw.dreamhax.cc/)

## 🙏 致谢

- [TS3AudioBot](https://github.com/Splamy/TS3AudioBot) - 一切的开始
- [TS3AudioBot-NetEaseCloudmusic-plugin](https://github.com/ZHANGTIANYAO1/TS3AudioBot-NetEaseCloudmusic-plugin) - 🙏
- [NeteaseTSBot](https://github.com/yichen11818/NeteaseTSBot) - `voice-service` 实现
- [ncm-api-rs](https://github.com/SPlayer-Dev/ncm-api-rs) - NCMAPI的Rust实现
- [UnblockNeteaseMusic](https://github.com/UnblockNeteaseMusic/server) - 解锁变灰歌曲
- [NapCatQQ](https://github.com/NapNeko/NapCatQQ) - 🐧

![Alt](https://repobeats.axiom.co/api/embed/e20c0a7a0fb24465f50fb3882dadf4416456dd24.svg "Repobeats analytics image")
