# AGENTS.md

## Commands

- Build: `cargo build --release`
- Run: `cargo run`
- Check: `cargo check`
- Format: `cargo fmt`
- Clean: `cargo clean`

## Architecture

Single binary `teamspeakclaw`, three inbound adapters:

```
src/
├── main.rs                  # Entrypoint: wires up adapters, routers, shutdown
├── cli.rs                   # --log-level
├── config.rs                # settings.toml, acl.toml, prompts.toml
├── config/                  # Sub-modules (acl, bot, headless, llm, ...)
├── router.rs                # Event routing
├── router/                  # Sub-modules (ts_router, nc_router, voice_router, unified)
├── adapter.rs               # Re-exports TsAdapter, TsEvent (from headless)
├── adapter/
│   ├── headless.rs          # gRPC voice bridge root
│   ├── headless/            # gRPC voice bridge (actor, event, speech, voice_service)
│   ├── napcat.rs            # OneBot 11 WebSocket root
│   └── napcat/              # OneBot 11 WebSocket (api, ws, event, types)
├── llm.rs                   # OpenAI-compatible LLM engine, context, tool loop
├── llm/                     # Sub-modules (context, engine, provider, tool_loop)
├── permission.rs            # ACL-based permission gate
├── permission/              # Sub-modules (gate)
└── skills.rs                # Skill system root
└── skills/                  # Skill system (music, moderation, information, ...)
    ├── music.rs             # Music skill root
    └── music/               # Music backends (ts3audiobot, tsbot_http, tsmusicbot)
```

## Critical Code Paths

- **Audio/STT dual path**: `voice_router.rs` has `handle_audio_event` (separate STT → text LLM) and `handle_omni_audio_event` (raw audio to multimodal LLM). Controlled by `llm.omni_model` config. Both need changes when modifying audio/STT logic.
- **Music bot filter**: `voice_router.rs:271-275` skips audio frames from `music_backend.musicbot_name` so they never reach STT.
- **Voice vs text routing**: When headless STT or TTS is enabled, `ts_router.rs:236-238` skips handling text messages (they're handled by `voice_router.rs` instead).

## Build Dependencies

- `protoc-bin-vendored`: auto-downloaded by `build.rs`, no manual install
- `.cargo/config.toml` sets `CMAKE_POLICY_VERSION_MINIMUM = "3.5"` (needed for building audiopus/opus-sys)
- Linux: `cmake libopus-dev`
- macOS: `brew install autoconf automake libtool`
- Docker: `ubuntu:24.04` base with `libopus0 ffmpeg`

## LLM / Provider

- OpenAI-compatible (any API with `/v1/chat/completions`)
- Streamed response parsing: `reasoning_content` fields are **ignored** (not stored or relayed)
- Context: configurable max turns/sessions via `max_context_turns` / `max_context_sessions`
- Concurrent request limiting via tokio `Semaphore`


## CI/CD

- `.github/workflows/build.yml`: windows-amd64, linux-amd64, macos-aarch64
- Triggers: main/master push, PR, tag `v*`
- Artifacts: platform archive + Docker image to `ghcr.io`
- Changelog: `git-cliff` with `.github/cliff.toml` (not committed — exists during CI only)

## Conventions

- `.github/copilot-instructions.md` defines strict coding rules: FAILFAST, YAGNI, DRY, Chinese comments, no defensive code, Conventional Commits, type safety, no compiler warning suppression
- Comments in Chinese (except code identifiers)
- No docstrings on untouched code
- Skills implement `Skill` trait with `execute` (TS), `execute_nc` (QQ), and `execute_unified` (cross-platform) — new skills should implement `execute_unified` when supporting both platforms
