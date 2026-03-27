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

1. **下载程序**：
   前往 [Releases 页面](https://github.com/Dr1mH4X/TeamSpeakClaw/releases) 下载适合您操作系统的最新版本。

2. **生成配置**：
   在命令行运行以下命令，自动生成默认配置文件：
   ```bash
   ./teamspeakclaw --config generate
   ```
   这将在 `config/` 目录下创建 `settings.toml`, `acl.toml`, 和 `prompts.toml`。

3. **修改配置**：
   您可以使用文本编辑器手动修改，或使用内置的交互式向导：
   ```bash
   ./teamspeakclaw --config edit
   ```
   按照提示输入您的 TeamSpeak ServerQuery 账号密码以及 LLM API Key。

4. **启动服务**：
   直接运行程序：
   ```bash
   ./teamspeakclaw
   ```

## ⚙️ 配置

### `settings.toml` (核心设置)

```toml
[teamspeak]
host = "127.0.0.1"      # TeamSpeak服务器地址
port = 10011            # ServerQuery端口
login_name = "serveradmin"
login_pass = "YOUR_PASSWORD" # ServerQuery密码
server_id = 1           # 虚拟服务器ID
bot_nickname = "TSClaw" # 机器人在TS中的昵称

[llm]
api_key = "sk-xxxxxx"   # API Key
base_url = "https://api.openai.com/v1" # API 地址
model = "gpt-4o"        # 使用的模型名称

# 音乐后端配置 (默认使用 ts3audiobot)
[music_backend]
backend = "ts3audiobot"  # 或 "tsbot_backend"
base_url = "http://127.0.0.1:8009"  # HTTP 后端地址 (仅 tsbot_backend 使用)
```

### `acl.toml` (权限控制)

TeamSpeakClaw 使用基于角色的访问控制。规则从上到下匹配，生效第一条匹配的规则。

```toml
[[rules]]
name = "admin"
server_group_ids = [6]  # TS 服务器组 ID (例如 6 是 Server Admin)
allowed_skills = ["*"]  # 允许所有技能
can_target_admins = true # 允许对其他管理员操作

[[rules]]
name = "default"
server_group_ids = []   # 空数组代表匹配所有人
allowed_skills = ["music_control", "list_clients"] # 仅允许点歌和看列表
```

### 🤖 LLM 接入指南 (LLM Setup)

本项目使用 OpenAI 兼容接口，理论上支持所有类 OpenAI 模型。

#### 接入 DeepSeek
```toml
[llm]
api_key = "sk-..."
base_url = "https://api.deepseek.com/v1"
model = "deepseek-chat"
```

#### 接入 Ollama (本地)
```toml
[llm]
api_key = "ollama" # 任意填写
base_url = "http://localhost:11434/v1"
model = "llama3"
```

### 🎵 音乐插件联动 (Music Integration)

TeamSpeakClaw 支持两种音乐控制模式：

#### 模式一：ts3audiobot 后端 (默认)

1.  **部署 TS3AudioBot**：安装并运行 [TS3AudioBot](https://github.com/Splamy/TS3AudioBot)。
2.  **安装网易云插件**：安装 [TS3AudioBot-NetEaseCloudmusic-plugin](https://github.com/ZHANGTIANYAO1/TS3AudioBot-NetEaseCloudmusic-plugin)。
3.  **关键设置**：**务必将您的音乐机器人昵称设置为 `TS3AudioBot`**。
     *   TSClaw 会在服务器中寻找昵称为 `TS3AudioBot` 的用户，并通过私聊发送指令（如 `!yun play ...`）。
     *   如果您的音乐机器人叫其他名字，TSClaw 将无法找到它。

#### 模式二：tsbot_backend 后端

使用 HTTP API 控制音乐，需要部署 [NeteaseTSBot-backend](https://github.com/yichen11818/NeteaseTSBot)。

配置 `backend = "tsbot_backend"` 并设置 `base_url` 即可使用。

**使用示例**：
- 用户：“播放周杰伦的稻香”
- Claw -> (识别意图) -> 查找 `TS3AudioBot` -> 发送私信 `!yun play 稻香`
- 音乐机器人 -> (接收指令) -> 开始播放

## 💬 命令与使用 (Usage)

### 命令行选项 (CLI Options)

*   `--log-level <LEVEL>`: 设置控制台日志级别 (error, warn, info, debug, trace)。默认为 info。
*   `--config generate`: 生成默认配置文件到 `config/` 目录。
*   `--config edit`: 启动交互式配置向导。

### 交互指令


1.  **频道聊天**：在频道中发送以 `!tsclaw` 或 `@TSClaw` 开头的消息。
    > `!tsclaw 播放一首轻音乐`
2.  **私聊 (Private Message)**：向机器人发送私信（无需前缀）。
    > `播放一首轻音乐`

**常用指令示例**：
*   **点歌**：“播放周杰伦的青花瓷”、“来首摇滚乐”、“下一首”
*   **管理**：“把那个叫 'BadGuy' 的人踢出去”、“禁止 'Troll' 说话 10 分钟”
*   **查询**：“现在谁在线？”、“服务器有多少人？”

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
