---
sidebar_position: 1
---

# Download & Installation

## 1. Download

Please visit the [GitHub Releases](https://github.com/Dr1mH4X/TeamSpeakClaw/releases/latest) page to download the latest version of TeamSpeakClaw:

Select the appropriate file for your operating system (Windows, Linux, macOS).

## 2. Installation

TeamSpeakClaw is a standalone binary application and does not require a complex installation process.

1. Extract the downloaded archive into a folder.
2. Ensure you have read and write permissions for that folder.

## 3. Configuration

The extracted archive contains a `config/` directory with the following configuration files:

- `settings.toml` — Core settings (Connection, LLM, bot behavior, Headless voice service)
- `acl.toml` — Permission control rules
- `prompts.toml` — System prompt and error messages

Modify `config/settings.toml`, filling in your TeamSpeak server connection details, LLM API Key, and other configuration.

For detailed configuration instructions, please refer to the [Configuration Guide](/docs/configuration).

## 4. Wake Word Models (Optional) {#wakeword}

When `[headless.wakeword]` is set to `enabled = true` in `config/settings.toml`, the wake word model files must be prepared; skip this section if voice wake is not enabled.

Models go in a `models/` directory whose location depends on the deployment method:

- Binary deployment: next to the `teamspeakclaw` executable, i.e. `<extracted-dir>/models/` after unpacking the release archive
- Docker deployment: `./models/` in the project directory

Three ONNX files are required:

1. Front-end models `melspectrogram.onnx` and `embedding_model.onnx`: download them as described in the [model directory notes](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/models/README.md).
2. A wake word classifier: download one model from the [openWakeWord community model library](https://openwakeword.com/library) (choose the ONNX export) and set `[headless.wakeword].model` to its file name (including the extension).

Voice wake needs a speech input path: enable `[headless.stt]` or set `omni_model = true` under `[llm]`. The STT and multimodal options are listed in the next section. With the gate enabled but model files missing or invalid, the program exits at startup.

## 5. Docker Deployment

Deploying with Docker is the easiest way, without manually installing dependencies.

### Using Docker Compose (Recommended)

1. Create a project directory and download the base compose file (recommended, for online/multimodal STT services):

```bash
mkdir teamspeakclaw && cd teamspeakclaw
curl -O https://raw.githubusercontent.com/Dr1mH4X/TeamSpeakClaw/main/examples/docker-compose.yml
```

2. Copy the configuration files from the `examples/config/` directory to `config/` and edit them.

3. Choose an STT solution:

**Option 1: Online STT / Multimodal Model (Recommended)**

Use the base `docker-compose.yml`:
- Multimodal model: set `omni_model = true` under `[llm]` — speech is sent to that model as audio (skips STT, so `[headless.stt]` is not needed; `model` must accept audio input). Replies are still text; enable `[headless.tts]` separately for spoken replies
- Online STT: configure an OpenAI-compatible online STT API under `[headless.stt]` in `config/settings.toml`

**Option 2: Local STT (FunASR)**

Switch to the compose file that includes the `funasr-server` service:

```bash
curl -o docker-compose.yml https://raw.githubusercontent.com/Dr1mH4X/TeamSpeakClaw/main/examples/funasr/docker-compose.yml
```

- Download a model from [SenseVoiceSmall-GGUF](https://huggingface.co/FunAudioLLM/SenseVoiceSmall-GGUF) into the `models/` directory
- Set `base_url = "http://funasr-server:8000/v1"` under `[headless.stt]` in `config/settings.toml`; leave `api_key` empty and `model` can be any value
- In practice, a 2 vCPU / 4 GB ECS can run the Q8-quantized FunASR model, for reference

**Option 3: Local STT (whisper.cpp)**

based on your GPU:

| Variant | Hardware | File |
|---|---|---|
| CPU | No dedicated GPU | [docker-compose-cpu.yml](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/examples/whisper/docker-compose-cpu.yml) |
| GPU (Vulkan) | Intel / AMD | [docker-compose-gpu.yml](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/examples/whisper/docker-compose-gpu.yml) |
| CUDA | NVIDIA | [docker-compose-cuda.yml](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/examples/whisper/docker-compose-cuda.yml) |

```bash
# Example (CUDA): download the variant directly as docker-compose.yml
curl -o docker-compose.yml https://raw.githubusercontent.com/Dr1mH4X/TeamSpeakClaw/main/examples/whisper/docker-compose-cuda.yml
```

- Download [whisper.cpp GGML models](https://huggingface.co/ggerganov/whisper.cpp/tree/main) into the `models/` directory
- NVIDIA (CUDA) requires [NVIDIA Container Toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/install-guide.html) on the host

4. Start the service:

```bash
docker compose up -d
```

5. View logs:

```bash
docker compose logs -f
```

### Using Docker Command

```bash
# Pull the latest image
docker pull ghcr.io/dr1mh4x/teamspeakclaw:latest

# Create directories
mkdir -p config logs models

# Copy example configuration and edit
# Copy configuration files from the examples/config/ directory and modify them

# After editing the configuration, run the container
docker run -d \
  --name teamspeakclaw \
  --restart unless-stopped \
  -v ./config:/app/config:ro \
  -v ./logs:/app/logs \
  -v ./models:/app/models:ro \
  -e TZ=Asia/Shanghai \
  ghcr.io/dr1mh4x/teamspeakclaw:latest
```

## 6. Start the Service (Binary Deployment)

Once the configuration is complete, simply run the program:

```bash
./teamspeakclaw
```

If configured correctly, the bot will connect to your TeamSpeak server and begin listening for events.

## 7. Grant Permissions

After the bot connects, **right-click the bot in the TeamSpeak client → Edit Server Groups** and assign it the **Serveradmin** server group. Otherwise, the bot will not be able to perform administrative actions (such as kick, ban, move users, etc.).
