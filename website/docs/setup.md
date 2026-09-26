---
sidebar_position: 1
---

# 下载与安装

## 1. 下载

请前往 [GitHub Releases](https://github.com/Dr1mH4X/TeamSpeakClaw/releases/latest) 页面下载最新版本的 TeamSpeakClaw：

根据您的操作系统选择合适的文件（Windows, Linux, macOS）。

## 2. 安装

TeamSpeakClaw 是一个独立的二进制应用程序，无需复杂的安装过程。

1. 将下载的压缩包解压到一个文件夹中。
2. 确保您拥有该文件夹的读写权限。

## 3. 配置

解压后内含 `config/` 目录，包含以下配置文件：

- `settings.toml` — 核心设置（连接、LLM、机器人行为、Headless 语音服务）
- `acl.toml` — 权限控制规则
- `prompts.toml` — 系统提示词与错误消息

使用文本编辑器修改 `config/settings.toml`，填入您的 TeamSpeak 服务器连接信息以及 LLM API Key 等配置。

**快速配置检查清单**：
- `[headless]` — 填写 TeamSpeak 服务器地址（`server_address`）、端口（`server_port`）、密码等
- `[llm]` — 填写 API Key、Base URL 和模型名称
- `[headless.stt]` / `[headless.tts]` — 如需语音服务，启用并配置（可选）
- `[napcat]` — 如需 QQ 机器人，启用并配置 WebSocket 地址（可选）
- `[voice_replay]` — 如需语音回放，默认保持 `enabled = false`；开启后在 `acl.toml` 按组授权（可选，改配置需重启）。直呼与技能共用 ACL。用法见 [usage.md](usage.md)

详细配置说明请参考 [配置指南](/docs/configuration)。

## 4. Docker 部署（推荐）

使用 Docker 部署是最简单的方式，无需手动安装依赖。

### 使用 Docker Compose（推荐）

1. 创建项目目录并下载 `docker-compose.yml`（推荐，配合在线/多模态 STT 服务）：

```bash
mkdir teamspeakclaw && cd teamspeakclaw
curl -O https://raw.githubusercontent.com/Dr1mH4X/TeamSpeakClaw/main/examples/docker-compose.yml
```

2. 从 `examples/config/` 目录复制配置文件到 `config/` 目录并修改。

3. 选择 STT 方案：

**方案一：在线 STT / 多模态模型（推荐）**

直接使用 `docker-compose.yml`：
- 在线 STT：在 `config/settings.toml` 的 `[headless.stt]` 中配置 OpenAI 兼容的在线 STT API
- 多模态模型：在 `[llm]` 段将 `omni_model` 设为 `true`，自动禁用 TTS/STT、直接用语音输入输出，无需配置 STT

**方案二：本地 STT（whisper.cpp，离线）**

换用带 `stt-api` 服务的 compose 变体，按显卡类型选择：

| 变体 | 硬件 | 配置文件 |
|---|---|---|
| CPU | 无独显 | [docker-compose-cpu.yml](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/examples/docker-compose-cpu.yml) |
| GPU（Vulkan） | Intel / AMD | [docker-compose-gpu.yml](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/examples/docker-compose-gpu.yml) |
| CUDA | NVIDIA | [docker-compose-cuda.yml](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/examples/docker-compose-cuda.yml) |

```bash
# 以 CUDA 为例：
curl -o docker-compose.yml https://raw.githubusercontent.com/Dr1mH4X/TeamSpeakClaw/main/examples/docker-compose-cuda.yml
```

- 下载 [whisper.cpp GGML 模型](https://huggingface.co/ggerganov/whisper.cpp/tree/main)到 `models/` 目录：CPU 默认 `ggml-small.bin`，GPU/CUDA 默认 `ggml-large-v3-turbo.bin`
- NVIDIA（CUDA）需在宿主机安装 [NVIDIA Container Toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/install-guide.html)；Intel/AMD 需要 `/dev/dri` 设备映射（compose 中已配置）

4. 启动服务：

```bash
docker compose up -d
```

5. 查看日志：

```bash
docker compose logs -f
```

### 使用 Docker 命令

```bash
# 拉取最新镜像
docker pull ghcr.io/dr1mh4x/teamspeakclaw:latest

# 创建目录
mkdir -p config logs

# 复制示例配置并编辑
# 从 examples/config/ 目录复制配置文件并修改

# 编辑配置文件后运行容器
docker run -d \
  --name teamspeakclaw \
  --restart unless-stopped \
  -v ./config:/app/config:ro \
  -v ./logs:/app/logs \
  -e TZ=Asia/Shanghai \
  ghcr.io/dr1mh4x/teamspeakclaw:latest
```

## 5. 启动服务（传统方式）

配置完成后，直接运行程序：

```bash
./teamspeakclaw
```

如果配置正确，机器人将连接到您的 TeamSpeak 服务器并开始监听事件。

## 6. 授予权限

机器人连接服务器后，请在 TeamSpeak 客户端中 **右键点击机器人 → 编辑服务器组**，为其添加 **Serveradmin** 服务器组权限，否则机器人无法执行管理操作（如踢人、封禁、移动用户等）。
