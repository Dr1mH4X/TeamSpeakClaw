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

修改 `config/settings.toml`，填入您的 TeamSpeak 服务器连接信息以及 LLM API Key 等配置。

详细配置说明请参考 [配置指南](/docs/configuration)。

## 4. 唤醒词模型（可选） {#wakeword}

当 `config/settings.toml` 的 `[headless.wakeword]` 设为 `enabled = true` 时，需要准备唤醒词模型文件；未启用语音唤醒可跳过本节。

模型文件放在 `models/` 目录，该目录的位置随部署方式而定：

- 二进制部署：与 `teamspeakclaw` 可执行文件同级，解压归档后为 `<解压目录>/models/`
- Docker 部署：项目目录下的 `./models/`

需要放入三个 ONNX 文件：

1. 前端模型 `melspectrogram.onnx` 与 `embedding_model.onnx`：按[模型目录说明](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/models/README.md)下载。
2. 唤醒词分类器：从 [openWakeWord 社区模型库](https://openwakeword.com/library)下载一个模型（选 ONNX 导出），并把 `[headless.wakeword]` 的 `model` 设为该文件名（包含后缀名）。

语音唤醒需要语音输入：启用 `[headless.stt]` 或将 `[llm]` 的 `omni_model` 设为 `true`。STT 与多模态方案的选择见下一节。启用后模型文件缺失或非法时，程序会在启动时直接退出。

## 5. Docker 部署

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
- 多模态模型：在 `[llm]` 段将 `omni_model` 设为 `true`，语音以音频直接送入该模型（跳过 STT，无需配置 `[headless.stt]`；`model` 需支持音频输入）；模型回复仍是文本，需要语音回复时另行启用 `[headless.tts]`
- 在线 STT：在 `config/settings.toml` 的 `[headless.stt]` 中配置 OpenAI 兼容的在线 STT API

**方案二：本地 STT（FunASR）**

换用带 `funasr-server` 服务的 compose 文件：

```bash
curl -o docker-compose.yml https://raw.githubusercontent.com/Dr1mH4X/TeamSpeakClaw/main/examples/funasr/docker-compose.yml
```

- 从 [SenseVoiceSmall-GGUF](https://huggingface.co/FunAudioLLM/SenseVoiceSmall-GGUF) 下载模型到 `models/` 目录
- 在 `config/settings.toml` 的 `[headless.stt]` 中设 `base_url = "http://funasr-server:8000/v1"`，`api_key` 留空、`model` 可填任意值
- 实测 2C4G ECS 可运行 Q8 量化的 FunASR 模型，可做参考

**方案三：本地 STT（whisper.cpp）**

按显卡类型选择：

| 变体 | 硬件 | 配置文件 |
|---|---|---|
| CPU | 无独显 | [docker-compose-cpu.yml](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/examples/whisper/docker-compose-cpu.yml) |
| GPU（Vulkan） | Intel / AMD | [docker-compose-gpu.yml](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/examples/whisper/docker-compose-gpu.yml) |
| CUDA | NVIDIA | [docker-compose-cuda.yml](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/examples/whisper/docker-compose-cuda.yml) |

```bash
# 以 CUDA 为例：
curl -o docker-compose.yml https://raw.githubusercontent.com/Dr1mH4X/TeamSpeakClaw/main/examples/whisper/docker-compose-cuda.yml
```

- 下载 [whisper.cpp GGML 模型](https://huggingface.co/ggerganov/whisper.cpp/tree/main)到 `models/` 目录
- NVIDIA（CUDA）需在宿主机安装 [NVIDIA Container Toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/install-guide.html)

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
mkdir -p config logs models

# 复制示例配置并编辑
# 从 examples/config/ 目录复制配置文件并修改

# 编辑配置文件后运行容器
docker run -d \
  --name teamspeakclaw \
  --restart unless-stopped \
  -v ./config:/app/config:ro \
  -v ./logs:/app/logs \
  -v ./models:/app/models:ro \
  -e TZ=Asia/Shanghai \
  ghcr.io/dr1mh4x/teamspeakclaw:latest
```

## 6. 启动服务（二进制部署）

配置完成后，直接运行程序：

```bash
./teamspeakclaw
```

如果配置正确，机器人将连接到您的 TeamSpeak 服务器并开始监听事件。

## 7. 授予权限

机器人连接服务器后，请在 TeamSpeak 客户端中 **右键点击机器人 → 编辑服务器组**，为其添加 **Serveradmin** 服务器组权限，否则机器人无法执行管理操作（如踢人、封禁、移动用户等）。
