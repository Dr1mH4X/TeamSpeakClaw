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

- `settings.toml` — Core settings (Connection, LLM, bot behavior, Headless voice service, voice_replay)
- `acl.toml` — Permission control rules
- `prompts.toml` — System prompt and error messages

Use a text editor to modify `config/settings.toml`, filling in your TeamSpeak server connection details, LLM API Key, and other configuration.

**Quick configuration checklist**:
- `[headless]` — TeamSpeak server address, port, password, etc.
- `[llm]` — API Key, Base URL, model name
- `[headless.stt]` / `[headless.tts]` — Enable if you need voice services (optional)
- `[napcat]` — Enable and set WebSocket URL for QQ bot (optional)
- `[voice_replay]` — Leave `enabled = false` unless you need replay; if enabled, grant access in `acl.toml` by group (optional; restart after changes). Direct commands share the skill ACL. See [usage.md](usage.md)
- `prompts.toml` — System prompts and error messages

Use a text editor to modify `config/settings.toml`, filling in your TeamSpeak server connection details, LLM API Key, and other configuration.

**Quick Configuration Checklist**:
- `[headless]` — Fill in TeamSpeak server address (`server_address`), port (`server_port`), password, etc.
- `[llm]` — Fill in API Key, Base URL, and model name
- `[headless.stt]` / `[headless.tts]` — Enable and configure if you need voice service (optional)
- `[voice_replay]` — Keep `enabled = false` unless you need replay; grant `voice_replay` in ACL if enabled
- `[napcat]` — Enable and configure WebSocket URL if you need QQ bot (optional)

For detailed configuration instructions, please refer to the [Configuration Guide](/docs/configuration).

## 4. Docker Deployment (Recommended)

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
- Online STT: configure an OpenAI-compatible online STT API under `[headless.stt]` in `config/settings.toml`
- Multimodal model: set `omni_model = true` under `[llm]` — TTS/STT are disabled automatically and voice goes in/out directly, no STT config needed

**Option 2: Local STT (whisper.cpp, offline)**

Switch to one of the compose variants that include the `stt-api` service, based on your GPU:

| Variant | Hardware | File |
|---|---|---|
| CPU | No dedicated GPU | [docker-compose-cpu.yml](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/examples/docker-compose-cpu.yml) |
| GPU (Vulkan) | Intel / AMD | [docker-compose-gpu.yml](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/examples/docker-compose-gpu.yml) |
| CUDA | NVIDIA | [docker-compose-cuda.yml](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/examples/docker-compose-cuda.yml) |

```bash
# Example (CUDA): download the variant directly as docker-compose.yml
curl -o docker-compose.yml https://raw.githubusercontent.com/Dr1mH4X/TeamSpeakClaw/main/examples/docker-compose-cuda.yml
```

- Download [whisper.cpp GGML models](https://huggingface.co/ggerganov/whisper.cpp/tree/main) into the `models/` directory: the CPU variant defaults to `ggml-small.bin`, GPU/CUDA variants default to `ggml-large-v3-turbo.bin`
- NVIDIA (CUDA) requires [NVIDIA Container Toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/install-guide.html) on the host; Intel/AMD needs `/dev/dri` device mapping (already configured in the compose file)

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
mkdir -p config logs

# Copy example configuration and edit
# Copy configuration files from the examples/config/ directory and modify them

# After editing the configuration, run the container
docker run -d \
  --name teamspeakclaw \
  --restart unless-stopped \
  -v ./config:/app/config:ro \
  -v ./logs:/app/logs \
  -e TZ=Asia/Shanghai \
  ghcr.io/dr1mh4x/teamspeakclaw:latest
```

## 5. Start Service (Traditional Method)

Once the configuration is complete, simply run the program:

```bash
./teamspeakclaw
```

If configured correctly, the bot will connect to your TeamSpeak server and begin listening for events.

## 6. Grant Permissions

After the bot connects, **right-click the bot in the TeamSpeak client → Edit Server Groups** and assign it the **Serveradmin** server group. Otherwise, the bot will not be able to perform administrative actions (such as kick, ban, move users, etc.).
