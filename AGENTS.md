# AGENTS.md

## Commands

- Build release: `cargo build --release`
- Check: `cargo check`
- Lint: `cargo clippy --all-targets --locked -- -D warnings`
- Test: `cargo test --all-targets --locked`
- Format: `cargo fmt`
- Clean: `cargo clean`

## Architecture

Single binary `teamspeakclaw`, two inbound adapter families (TeamSpeak headless client with an internal gRPC voice bridge, and NapCat OneBot 11):

```
src/
├── main.rs                  # Entrypoint: wires config, adapters, routers, shutdown
├── cli.rs                   # --log-level
├── log.rs                   # Daily-rotated file logs + tracing/slog bridge init
├── config.rs                # Loads config/settings.toml, acl.toml, prompts.toml
├── config/                  # Sub-modules (acl, bot, headless, llm, logging, music_backend, napcat, prompts) + .instructions.md
├── router.rs                # Event routing; also entry for combined router loop
├── router/                  # Sub-modules (ts_router, nc_router, voice_router, unified, trigger)
├── adapter.rs               # Reconnect loop, session lifecycle, cross-adapter coordination
├── adapter/
│   ├── reconnect.rs         # Reconnection backoff constants & helpers
│   ├── headless.rs          # Headless TS client + gRPC voice bridge; voice_features_enabled, should_route_text_through_bridge
│   ├── headless/            # (actor, event, speech, text_util, types, voice_service)
│   ├── napcat.rs            # OneBot 11 WebSocket root
│   └── napcat/              # (api, ws, event, types)
├── llm.rs                   # OpenAI-compatible LLM engine, context, tool loop
├── llm/                     # (context, engine, provider, tool_loop)
├── permission.rs            # ACL-based permission gate
├── permission/              # (gate)
├── skills.rs                # Skill trait + registry; Skill, ExecutionContext, UnifiedExecutionContext
├── skills/                  # (communication, information, moderation, music, web_search)
│   ├── music.rs             # Music skill root
│   └── music/               # (ts3audiobot, tsbot_http, tsmusicbot)

proto/voice.proto            # gRPC protobuf for voice bridge
examples/
├── config/                  # Reference config templates (settings.toml, acl.toml, prompts.toml)
└── docker-compose.yml       # Docker Compose example
website/                     # Docusaurus documentation (excluded from Rust CI paths)
```

### Entrypoint flow

1. `main.rs` loads config from `config/` subdirectory (relative to exe)
2. Creates `PermissionGate`, `SkillRegistry`, `LlmEngine`
3. `adapter::run()` loops: connect `TsAdapter` -> optionally connect `NapCatAdapter` -> start headless runtime (voice bridge + `VoiceRouter`) -> run routers
4. Router loop runs `EventRouter` (TeamSpeak) and optionally `NcRouter` (QQ) concurrently
5. On TS disconnect, the adapter reconnect loop restarts

## Critical Code Paths

- `adapter/headless.rs:voice_features_enabled()` + `should_route_text_through_bridge()`: when voice bridge is active, text messages are routed through `VoiceRouter` instead of `EventRouter`
- `ts_router.rs` `handle_message()`: uses `should_route_text_through_bridge` to skip text handling when the voice bridge is ready (STT/TTS/omni_model)
- `text_util.rs:split_message()` + `MAX_MESSAGE_BYTES`: splits at 8192-byte TS3 ServerQuery limit, UTF-8 safe, whitespace-preferred
- `event.rs:send_text_message()` / `actor.rs:notice_rx`: two send paths both go through `split_message`
- `voice_router.rs`: audio STT/TTS dual pipeline, music bot audio filter, gRPC voice service

## Build Dependencies

- `protoc-bin-vendored`: auto-downloaded by `build.rs` (generates gRPC code from `proto/voice.proto`)
- `.cargo/config.toml` sets `CMAKE_POLICY_VERSION_MINIMUM = "3.5"` (needed for building audiopus/opus-sys)
- Linux: `cmake libopus-dev`
- macOS: `brew install autoconf automake libtool`
- Docker: builder stage uses `rust:alpine` (unpinned), runtime stage is `alpine:3.20`; build deps `musl-dev cmake make gcc protoc`, runtime `opus ffmpeg`
- Docker build sets `ENV PROTOC=/usr/bin/protoc` to override protoc-bin-vendored

## LLM / Provider

- OpenAI-compatible (any API with `/v1/chat/completions`)
- Streamed response parsing: `reasoning_content` fields are **ignored** (not stored or relayed)
- Context: configurable max turns via `max_context_turns`; sessions capped by a fixed constant
- Concurrent request limiting via tokio `Semaphore`; fixed timeouts as constants (connect 10s, stream idle 30s, stream total 300s)
- `omni_model` flag (`config/llm.rs`): enables omni-modal mode; when set, text is routed through voice bridge

## Testing

- Unit tests live inline as `#[cfg(test)] mod tests { use super::*; }` blocks next to the code they cover
- Use `#[test]` for sync tests, `#[tokio::test]` for async tests; descriptive snake_case names; `assert!`/`assert_eq!` only
- Run with `cargo test --all-targets --locked`; CI also runs `cargo fmt --check` and clippy with `-D warnings`

## CI/CD

- `.github/workflows/ci.yml`: quality (fmt/clippy/test) + build (windows/linux/macos aarch64), uploads platform archives
- Triggers: push/PR to main/master, workflow_dispatch
- Artifacts: platform archives (uploaded by `ci.yml`, attached to GitHub Releases by `release.yml`); Docker image to `ghcr.io` only in `release.yml` / `docker-sha.yml`
- Changelog: `git-cliff` with `.github/cliff.toml`
- Other workflows: `release.yml` (tag-triggered release + GHCR), `deploy-website.yml` (GitHub Pages), `docker-sha.yml` (manual SHA tag), `cleanup-untagged.yml` (manual GHCR cleanup)

## Conventions

- `.github/copilot-instructions.md` defines strict coding rules: FAILFAST, YAGNI, DRY, Chinese comments, no defensive code, Conventional Commits, type safety, no compiler warning suppression
- Comments in Chinese (except code identifiers); pre-existing English comments in legacy files are left untouched, new/changed comments use Chinese
- No docstrings on untouched code
- Skills implement `Skill` trait with `execute` (TS), `execute_nc` (QQ), and `execute_unified` (cross-platform) — new skills should implement `execute_unified` when supporting both platforms
- Trigger prefixes are defined in config; `trigger.rs:strip_trigger_prefix()` strips them from incoming messages
- Config files live in `config/` beside the binary (loaded via `config_dir()` = `exe_dir().join("config")`)
