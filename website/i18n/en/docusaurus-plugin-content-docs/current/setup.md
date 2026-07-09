---
sidebar_position: 2
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
- `prompts.toml` — System prompts and error messages

Use a text editor to modify `config/settings.toml`, filling in your TeamSpeak server connection details, LLM API Key, and other configuration.

**Quick Configuration Checklist**:
- `[headless]` — Fill in TeamSpeak server address (`server_address`), port (`server_port`), password, etc.
- `[llm]` — Fill in API Key, Base URL, and model name
- `[headless.stt]` / `[headless.tts]` — Enable and configure if you need voice service (optional)
- `[napcat]` — Enable and configure WebSocket URL if you need QQ bot (optional)

For detailed configuration instructions, please refer to the [Configuration Guide](/docs/configuration).

## 4. Docker Deployment (Recommended)

Deploying with Docker is the easiest way, without manually installing dependencies.

### Using Docker Compose (Recommended)

1. Create a project directory and download the configuration file:

```bash
mkdir teamspeakclaw && cd teamspeakclaw
curl -O https://raw.githubusercontent.com/Dr1mH4X/TeamSpeakClaw/main/docker-compose.yml
```

2. Prepare configuration files and models (optional):

- Copy configuration files from `examples/config/` directory to `config/` and modify them
- If you need local STT service, download [whisper.cpp GGML models](https://huggingface.co/ggerganov/whisper.cpp/tree/main) to the `models/` directory:

```bash
mkdir -p models
cd models

# Download whisper model (recommended: ggml-large-v3-turbo)
```

3. Choose STT Solution:

**Option 1: Local STT (Default, Recommended)**

Use the `stt-api` service (whisper.cpp) already configured in docker-compose.yml to provide local speech recognition:
- No external API Key required
- Runs offline with lower latency
- Supports GPU acceleration (requires `/dev/dri` device mapping)
- Requires downloading GGML model files to the `./models` directory

**Option 2: Online STT Service**

If you don't want to use local STT, you can:
- Remove or comment out the `stt-api` service in docker-compose.yml
- Remove `depends_on: stt-api` from the `teamspeakclaw` service
- Configure OpenAI-compatible online STT API in `config/settings.toml` under `[headless.stt]`

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
