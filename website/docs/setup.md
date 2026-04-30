---
sidebar_position: 2
---

# 下载与安装

## 1. 下载

请前往 GitHub Releases 页面下载最新版本的 TeamSpeakClaw：

[**下载最新版本**](https://github.com/Dr1mH4X/TeamSpeakClaw/releases/latest)

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

使用文本编辑器修改 `config/settings.toml`，填入您的 TeamSpeak ServerQuery 账号密码以及 LLM API Key 等信息。

**快速配置检查清单**：
- `[serverquery]` — 填写 TeamSpeak 服务器地址、端口和登录凭据
- `[llm]` — 填写 API Key、Base URL 和模型名称
- `[headless]` — 如需语音服务，启用并配置 STT/TTS（可选）
- `[napcat]` — 如需 QQ 机器人，启用并配置 WebSocket 地址（可选）

详细配置说明请参考 [配置指南](/docs/configuration)。

## 4. Docker 部署（推荐）

使用 Docker 部署是最简单的方式，无需手动安装依赖。

### 使用 Docker Compose（推荐）

1. 创建项目目录并下载配置文件：

```bash
mkdir teamspeakclaw && cd teamspeakclaw
curl -O https://raw.githubusercontent.com/Dr1mH4X/TeamSpeakClaw/main/docker-compose.yml
```

2. 准备配置文件和模型（可选）：

- 从 `examples/config/` 目录复制配置文件到 `config/` 目录并修改
- 如需本地 STT 服务，下载 moonshine v2 模型到 `models/` 目录：

```bash
mkdir -p models
cd models

# 中文模型（二选一）
# base 模型：精度更高，速度较慢
wget https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-moonshine-base-zh-int8.tar.bz2
tar xvf sherpa-onnx-moonshine-base-zh-int8.tar.bz2

# tiny 模型：精度稍低，速度更快
wget https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-moonshine-tiny-zh-int8.tar.bz2
tar xvf sherpa-onnx-moonshine-tiny-zh-int8.tar.bz2

# 其他语言模型也支持：en（英语）、ja（日语）、ko（韩语）、es（西班牙语）、uk（乌克兰语）、vi（越南语）、ar（阿拉伯语）
# 模型命名格式：sherpa-onnx-moonshine-{base|tiny}-{语言代码}-int8.tar.bz2
```

更多模型请访问：https://github.com/k2-fsa/sherpa-onnx/releases/tag/asr-models

3. 选择 STT 方案：

**方案一：本地 STT（默认，推荐）**

使用 docker-compose.yml 中已配置的 `stt-api` 服务，提供本地语音识别：
- 无需外部 API Key
- 离线运行，延迟更低
- 需下载模型文件到 `./models` 目录

**方案二：在线 STT 服务**

如果不使用本地 STT，可以：
- 删除或注释 docker-compose.yml 中的 `stt-api` 服务
- 移除 `teamspeakclaw` 服务中的 `depends_on: stt-api`
- 在 `config/settings.toml` 的 `[headless]` 中配置在线 STT API（如 Groq、Azure 等）

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
