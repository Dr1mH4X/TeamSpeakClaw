# TeamSpeakClaw

TeamSpeakClaw 是一个基于 LLM (大语言模型) 的 TeamSpeak 智能助手。

它通过 ServerQuery 连接到您的 TeamSpeak 服务器，允许用户使用自然语言与服务器进行交互。无论是播放音乐、管理成员还是查询信息，您只需在频道中“说”出来，TSClaw 就会理解您的意图并自动执行相应的操作。它不仅能直接管理服务器，还能与其他插件（如 TS3AudioBot + 网易云插件）协同工作，为您提供无缝的语音服务器体验。

## ✨ 功能特性 (Features)

- **🧠 自然语言交互**：告别繁琐的指令手册。直接说“播放周杰伦的歌”或“把那个捣乱的人踢出去”，Claw 就能听懂并执行。
- **🛡️ 细粒度权限控制 (RBAC)**：内置强大的权限系统。您可以为不同的 TeamSpeak 用户组配置特定的技能权限（例如：仅允许管理员使用踢人功能，普通用户仅限点歌）。
- **🔌 灵活的技能系统 (Skills)**：
    - **音乐控制**：支持两种模式 - ts3audiobot 后端通过私信控制 [TS3AudioBot](https://github.com/Splamy/TS3AudioBot)，或 tsbot_backend [NeteaseTSBot](https://github.com/yichen11818/NeteaseTSBot) 后端通过 HTTP API 控制音乐播放。支持点歌、切歌、搜索、音量调节、音效设置等功能。
    - **服务器管理**：支持踢出 (Kick)、封禁 (Ban)、移动用户等操作。
    - **信息查询**：获取在线用户列表、服务器状态等。
    - **可扩展性**：轻松编写自定义 Skill，扩展机器人的能力边界。
- **🤖 广泛的模型支持**：兼容 OpenAI 接口格式，轻松接入 DeepSeek、Ollama、LocalAI 等多种大模型。

## 🗺️ 未来功能(Roadmap)
 - 客户端实现
 - TS3AudioBot插件实现

## 🚀 快速开始 (Quick Start)

[快速开始向导](http://tsclaw.dreamhax.cc/)

## 🛠️ 开发与构建 (Development)

如果您想自行编译或贡献代码：

1.  **环境准备**：安装 Rust (最新 Stable 版本)。
2.  **克隆仓库**：
    ```bash
    git clone https://github.com/Dr1mH4X/TeamSpeakClaw.git
    cd TeamSpeakClaw
    ```
3.  **编译**：
    ```bash
    cargo build --release
    ```
    编译产物位于 `target/release/teamspeakclaw` (或 `.exe`)。

## 🙏 致谢 (Credits)

- [TS3AudioBot](https://github.com/Splamy/TS3AudioBot)
- [TS3AudioBot-NetEaseCloudmusic-plugin](https://github.com/ZHANGTIANYAO1/TS3AudioBot-NetEaseCloudmusic-plugin)
- [NeteaseTSBot](https://github.com/yichen11818/NeteaseTSBot)

![Alt](https://repobeats.axiom.co/api/embed/e20c0a7a0fb24465f50fb3882dadf4416456dd24.svg "Repobeats analytics image")
