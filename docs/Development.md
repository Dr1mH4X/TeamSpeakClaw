# TeamSpeakClaw Development Guide

[English](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/docs/Development.md)|[Chinese](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/docs/Development_CN.md)

Welcome to the TeamSpeakClaw development documentation! This guide will help you set up your development environment, understand the project structure, and contribute new features.

## 1. Project Overview

**TeamSpeakClaw** is a Rust-based TeamSpeak ServerQuery bot that leverages Large Language Models (LLMs) to provide natural language interaction for server management and utilities.

### Tech Stack
- **Language**: Rust (2021 Edition)
- **Async Runtime**: `tokio`
- **HTTP Client**: `reqwest`
- **CLI Framework**: `clap`
- **Configuration**: `toml` with `serde`
- **Logging**: `tracing` ecosystem

## 2. Prerequisites

Before you begin, ensure you have the following installed:
- **Rust Toolchain**: Latest stable version (install via [rustup.rs](https://rustup.rs/)).
- **Git**: For version control.
- **TeamSpeak 3 Server** (Optional): A local or remote server for testing bot interactions.

## 3. Getting Started

### Clone the Repository
```bash
git clone https://github.com/Dr1mH4X/TeamSpeakClaw.git
cd TeamSpeakClaw
```

### Configuration
The application requires configuration files to run. You can generate default files using the CLI:

```bash
cargo run -- --config generate
```

This will create the following files in the `config/` directory:
- `settings.toml`: Main application settings (TeamSpeak credentials, LLM API keys).
- `acl.toml`: Access Control List (defines which TS groups can use which skills).
- `prompts.toml`: Custom system prompts and error messages.

**Important**: You must edit `config/settings.toml` to add your TeamSpeak server details and LLM API key before running the bot.

### Build and Run
To build the project in release mode:
```bash
cargo build --release
```
The binary will be located at `target/release/teamspeakclaw`.

To run the bot locally during development:
```bash
cargo run
# Or with debug logging enabled
cargo run -- --log-level debug
```

## 4. Project Architecture

The source code is organized in `src/` as follows:

| Directory/File | Description |
|---|---|
| `main.rs` | Application entry point. Initializes config, logging, LLM engine, and the main event loop. |
| `router.rs` | **Event Router**. The core logic that receives TS events, consults the LLM, checks permissions, and dispatches Skills. |
| `adapter/` | **TeamSpeak Adapter**. Handles raw ServerQuery connection, command sending, and event parsing. |
| `config/` | **Configuration**. Structs for `settings.toml`, `acl.toml`, and `prompts.toml`. Handles loading/saving. |
| `llm/` | **LLM Integration**. `LlmEngine` manages context/chat history. `provider.rs` implements API calls. |
| `skills/` | **Capabilities**. Contains the logic for specific bot actions (e.g., `music.rs`, `moderation.rs`). |
| `permission/` | **Auth System**. `PermissionGate` checks if a user (by TS Group ID) can execute a specific Skill. |
| `cache/` | **State Cache**. Maintains a local view of server state (e.g., connected clients). |
| `audit/` | **Logging**. Records administrative actions to `logs/audit.jsonl`. |

## 5. Development Workflow

### Adding a New Skill
To add a new capability to the bot (e.g., a "weather" command):

1.  **Create a new module** in `src/skills/` (e.g., `weather.rs`).
2.  **Implement the `Skill` trait**. This trait defines how the skill is triggered and executed.
3.  **Register the skill** in `main.rs` so the `router` knows about it.
4.  **Add permission rules** in `config/acl.toml` to control who can use it.

### Testing
Run the test suite to ensure your changes don't break existing functionality:
```bash
cargo test
```
The project uses `tokio-test`, `mockall`, and `wiremock` for testing async components and mocking external services.

### Code Style
We follow standard Rust community guidelines. Please ensure your code is formatted and lint-free before submitting a PR:

```bash
cargo fmt
cargo clippy
```

## 6. Contribution Guidelines

We welcome contributions! Please follow these steps:
1.  Fork the repository.
2.  Create a new branch for your feature or bugfix.
3.  Commit your changes with clear messages.
4.  Ensure all tests pass and code is formatted.
5.  Submit a Pull Request describing your changes.

---
