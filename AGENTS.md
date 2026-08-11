# Repository Guidelines

TeamSpeakClaw is a Rust binary for an LLM-powered TeamSpeak assistant, plus a Docusaurus docs site. This guide explains how the repository is organized and what contributors must follow.

## Project Structure & Module Organization

- `src/main.rs` — entrypoint; wires configuration, adapters, routers, and shutdown.
- `src/adapter/` — connection lifecycle and reconnect handling for the TeamSpeak headless (gRPC voice bridge) and NapCat (OneBot 11) adapters.
- `src/router/` — event routing only (`ts_router`, `nc_router`, `voice_router`, `unified`); no connection-state awareness.
- `src/llm/` — OpenAI-compatible engine, context, provider, and tool loop.
- `src/skills/` — skill implementations behind the `Skill` trait; music backends in `src/skills/music/`.
- `src/config/`, `src/permission/` — TOML config loading and ACL permission gates.
- `proto/voice.proto` — gRPC contract; `build.rs` generates bindings automatically.
- `examples/config/` — reference config templates; `website/` — Docusaurus docs.

Unit tests are embedded in source files; there is no top-level `tests/` directory.

## Build, Test, and Development Commands

- `cargo build` / `cargo build --release` — debug or optimized build.
- `cargo check` — type-check without codegen.
- `cargo fmt` / `cargo fmt --check` — format or verify formatting.
- `cargo clippy --all-targets --locked -- -D warnings` — lint; warnings are errors.
- `cargo test --all-targets --locked` — run all unit tests.
- `cd website && npm start` — local docs server; `npm run build`, `npm run typecheck` — production checks.

Linux builds require `cmake` and `libopus-dev`; protoc is vendored.

## Coding Style & Naming Conventions

- Standard `rustfmt`: 4-space indentation, `snake_case` functions/items, `CamelCase` types.
- Follow `.github/copilot-instructions.md`: fail fast, YAGNI, DRY, strong types over raw JSON/strings, no warning suppression.
- Sparse comments; Chinese per project convention (ASCII only in code identifiers).

## Testing Guidelines

- Unit tests live in `#[cfg(test)] mod tests { use super::*; }` blocks beside the code.
- Use `#[test]` (sync) or `#[tokio::test]` (async), descriptive `snake_case` names, and only `assert!` / `assert_eq!`.

## Commit & Pull Request Guidelines

- Use Conventional Commits: `feat`, `fix`, `refactor`, `docs`, `style`, `test`, `chore`, `ci`, `perf`, `revert`, with optional scopes like `refactor(adapter):`.
- Add `[skip changelog]` to exclude a commit from generated changelogs.
- PRs must pass all CI gates (fmt, tests, clippy with `-D warnings`, build), describe what and why, and link related issues.

## Security & Configuration Tips

- Never commit `config/`, `.env`, or other secrets; use `examples/config/` as a template.
- Report security issues through `SECURITY.md`.
