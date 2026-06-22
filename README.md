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
  <br>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://img.shields.io/badge/%E2%9A%A0%EF%B8%8F%20%E5%8D%87%E7%BA%A7%E8%AD%A6%E5%91%8A-white?style=for-the-badge&labelColor=red&color=red">
    <img src="https://img.shields.io/badge/%E2%9A%A0%EF%B8%8F%20%E5%8D%87%E7%BA%A7%E8%AD%A6%E5%91%8A-red?style=for-the-badge&labelColor=red&color=red" alt="升级警告">
  </picture>
</div>

> ⚠️ **v0.5.0 以前版本的升级警告**：自 v0.5.0 起，配置文件与 `identity.json` 不再兼容旧版本。升级前请参考最新的配置模板重新编写配置文件，**切勿**直接使用旧版配置，否则可能导致程序无法启动或数据异常。



TeamSpeakClaw 是一个基于 LLM (大语言模型) 的 TeamSpeak 智能助手。

它通过 ServerQuery 和 Headless 无头客户端连接到您的 TeamSpeak 服务器，允许用户使用自然语言与服务器进行交互。无论是播放音乐、管理成员还是查询信息，您只需在频道中"说"出来，TSClaw 就会理解您的意图并自动执行相应的操作。它不仅能直接管理服务器，还能与其他插件或机器人协同工作，为您提供无缝的语音服务器体验。

## ✨ 功能特性

- **🧠 自然语言交互**：告别繁琐的指令手册。直接说"播放周杰伦的歌"或"把那个捣乱的人踢出去"。
- **🛡️ 细粒度权限控制**：内置强大的权限系统。您可以为不同的 TeamSpeak 服务器组/用户组配置特定的技能权限。
- **🔌 灵活的技能系统**：
    - **音乐控制**：支持三种外部音乐机器人后端。
        - `ts3audiobot`：通过私信控制 [TS3AudioBot](https://github.com/ZHANGTIANYAO1/TS3AudioBot-NetEaseCloudmusic-plugin)或`tsmusicbot`：[TSMusicBot](https://github.com/ZHANGTIANYAO1/teamspeak-music-bot)。
        - `tsbot_backend`：通过 HTTP API 控制 [NeteaseTSBot](https://github.com/yichen11818/NeteaseTSBot)。
        - 支持点歌、切歌、搜索等功能。
    - **服务器管理**：支持踢出 (Kick)、戳一戳 (Poke)、移动用户等操作。
    - **信息查询**：获取在线用户列表/信息、服务器状态等。
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

- [TS3AudioBot](https://github.com/Splamy/TS3AudioBot)
- [NetEaseCloudmusic-plugin](https://github.com/ZHANGTIANYAO1/TS3AudioBot-NetEaseCloudmusic-plugin)
- [TSMusicBot](https://github.com/ZHANGTIANYAO1/teamspeak-music-bot)
- [NeteaseTSBot](https://github.com/yichen11818/NeteaseTSBot)
- [ncm-api-rs](https://github.com/SPlayer-Dev/ncm-api-rs)
- [UnblockNeteaseMusic](https://github.com/UnblockNeteaseMusic/server)
- [NapCatQQ](https://github.com/NapNeko/NapCatQQ)

![Alt](https://repobeats.axiom.co/api/embed/e20c0a7a0fb24465f50fb3882dadf4416456dd24.svg "Repobeats analytics image")
