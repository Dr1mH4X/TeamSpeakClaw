# AGENTS.md

TeamSpeakClaw 是 Rust 编写的单二进制聊天机器人：集成 TeamSpeak 无头客户端与 NapCat OneBot 11（QQ）双入站适配器，通过 OpenAI 兼容接口驱动 LLM 技能。改动 src/ 代码前先读 [docs/architecture.md](docs/architecture.md)；文档维护遵循 [docs/AGENTS.md](docs/AGENTS.md)。

## Repository layout

```
src/
├── main.rs                  # 入口：装配 config、适配器、路由器、关闭流程
├── cli.rs                   # --log-level
├── log.rs                   # 按日轮转文件日志 + tracing 初始化
├── config.rs                # 加载 config/settings.toml、acl.toml、prompts.toml
├── config/                  # 子模块（acl, bot, headless, llm, logging, music_backend, napcat, prompts, voice_replay）+ .instructions.md
├── router.rs                # 事件路由；组合路由器循环入口
├── router/                  # 子模块（ts_router, nc_router, voice_router, unified, trigger）
├── adapter.rs               # 重连循环、会话生命周期、跨适配器协调
├── adapter/
│   ├── reconnect.rs         # 重连退避常量与工具
│   ├── headless.rs          # 无头 TS 客户端 + gRPC 语音桥；voice_features_enabled、should_route_text_through_bridge
│   ├── headless/            # (actor, event, speech, text_util, types, voice_service)
│   ├── napcat.rs            # OneBot 11 WebSocket 根
│   └── napcat/              # (api, ws, event, types)
├── llm.rs                   # OpenAI 兼容 LLM 引擎、上下文、工具循环
├── llm/                     # (context, engine, provider, tool_loop)
├── permission.rs            # 基于 ACL 的权限门
├── permission/              # (gate)
├── skills.rs                # Skill trait + 注册表；Skill、UnifiedExecutionContext
├── skills/                  # (communication, information, moderation, music, web_search)
│   ├── music.rs             # 音乐技能根
│   └── music/               # (ts3audiobot, tsbot_http, tsmusicbot)

proto/voice.proto            # 语音桥 gRPC protobuf
docs/
├── AGENTS.md                # 文档标准：分层归属、写作规则、字数预算
└── architecture.md 等        # 开发者文档，详见 docs/AGENTS.md 分层表
examples/
├── config/                  # 参考配置模板（settings.toml, acl.toml, prompts.toml；Release 打包含此三文件）
└── docker-compose.yml       # Docker Compose 示例
website/                     # Docusaurus 用户文档（排除在 Rust CI 路径外）
```

## Commands

- Build release: `cargo build --release`
- Check: `cargo check`
- Lint: `cargo clippy --all-targets --locked -- -D warnings`
- Test: `cargo test --all-targets --locked`
- Format: `cargo fmt`
- Clean: `cargo clean`

构建依赖（protoc-bin-vendored、CMAKE_POLICY_VERSION_MINIMUM=3.5、Linux/macOS/Docker 依赖）见 [docs/development.md](docs/development.md)。

## Architecture

单二进制 `teamspeakclaw`，两族入站适配器：TeamSpeak 无头客户端（内置 gRPC 语音桥）与 NapCat OneBot 11。voice bridge 激活时文本经 `VoiceRouter` 而非 `EventRouter` 路由（由 `should_route_text_through_bridge` 决定）。入口流：main 加载 config → 建 `PermissionGate`/`SkillRegistry`/`LlmEngine` → `adapter::run()` 循环连接适配器并并发运行 `EventRouter`/`NcRouter`，TS 断线走重连循环。详见 [docs/architecture.md](docs/architecture.md)。

## Critical Code Paths

每条仅列要点，细节见 [docs/architecture.md](docs/architecture.md) 对应小节。

- `split_message()` + `MAX_MESSAGE_BYTES`：8192 字节 TS3 ServerQuery 上限，UTF-8 安全、优先空白切分
- 双发送路径：`event.rs:send_text_message()` / `actor.rs:notice_rx`，均经 `split_message`
- `voice_router.rs`：音频 STT/TTS 双流水线、音乐 bot 音频过滤、gRPC 语音服务
- `should_route_text_through_bridge`：voice bridge 就绪时 `ts_router::handle_message()` 跳过文本处理

## Secrets / .env

API key 等敏感配置放在 config 目录（加载自 `config_dir()` = `exe_dir().join("config")`），不进 git，不写入日志。读敏感文件只取结构（如 `jq 'keys'`），不整读内容。

## Conventions

每条只留规则一句话，归属链接放在行尾，细节见对应文档。

- **FAILFAST**：不写兼容、防御性、补丁式代码，错误按原样暴露。[docs/defensive-patterns.md](docs/defensive-patterns.md)
- **YAGNI**：不保留产生警告或无人使用的「未来功能」代码，需要时从 Git 历史找回。
- **DRY**：三处相似代码胜过一次过早抽象；单次使用的操作不做抽象。[docs/development.md](docs/development.md)
- **禁编译器警告压制**：禁止属性、注释禁用或无效读取绕过警告，须从根因修设计。[docs/defensive-patterns.md](docs/defensive-patterns.md)
- **类型安全优先**：用强类型结构体，避免原始 JSON 与字符串类型数据。[docs/defensive-patterns.md](docs/defensive-patterns.md)
- **改文件前先读**：动手前读完相关文件与全部需求，不盲目编辑。[docs/development.md](docs/development.md)
- **不改未动代码**：不给未改动的代码补 docstring 或类型注解。[docs/development.md](docs/development.md)
- **中文注释**：注释与文档用中文（代码标识符除外）；注释尽量少，只在逻辑不清晰处添加。[docs/development.md](docs/development.md)
- **Conventional Commits**：git 提交用 conventional 格式。[docs/development.md](docs/development.md)
- **子代理拆分**：复杂问题拆子代理，主上下文保持干净；禁止子代理再拆子代理。[docs/development.md](docs/development.md)
- **输出规范**：结论先行、中文大白话；代码逻辑仅 ASCII；临时文件及时删除。[docs/development.md](docs/development.md)
- **技能开发**：新技能实现 `execute`（统一入口，按 `ctx.platform` 分支）；触发前缀来自配置，`trigger.rs:strip_trigger_prefix()` 剥离。[docs/development.md](docs/development.md)
- **Agent Note**：非平凡变更（架构、生命周期、并发、音频/语音桥、重连等）MUST 附带决策记录。[docs/agent-notes.md](docs/agent-notes.md)

## LLM / Provider

OpenAI 兼容（任意 `/v1/chat/completions` API）；流式解析忽略 `reasoning_content`（不存不转发）；上下文受 `max_context_turns` 与固定常量上限控制；并发门禁为 `TurnCoordinator`（容量 + 同会话串行锁，三入口共用），超时为常量（连接 10s、流空闲 30s、流总 300s）；`omni_model` 标志（`config/llm.rs`）开启时文本走语音桥。详见 [docs/architecture.md](docs/architecture.md)。

## Testing

单测与实现同文件，为 `#[cfg(test)] mod tests { use super::*; }` 块；`#[test]` 同步、`#[tokio::test]` 异步，snake_case 命名，仅用 `assert!`/`assert_eq!`。运行 `cargo test --all-targets --locked`；CI 另跑 `cargo fmt --check` 与 clippy `-D warnings`。详见 [docs/testing.md](docs/testing.md)。

## CI/CD

各 workflow 一句话职责，细节见 [docs/ci-cd.md](docs/ci-cd.md)。

- `ci.yml`（push/PR 到 main/master、workflow_dispatch）：质量门（fmt/clippy/test）+ windows/linux/macos aarch64 构建，上传平台归档
- `release.yml`（tag 触发）：发布 + 构建并推送 GHCR 镜像
- `deploy-website.yml`：GitHub Pages 部署
- `docker-sha.yml`（手动）：打 SHA tag 镜像
- `cleanup-untagged.yml`（手动）：清理 GHCR 未打标镜像
- 变更日志：`git-cliff`（`.github/cliff.toml`）

## Defensive patterns

改生命周期、并发、音频/语音桥、重连相关代码前，先读 [docs/defensive-patterns.md](docs/defensive-patterns.md)。

## Editing these instructions

根 `AGENTS.md` 只放全局 standing orders；具体事实移入 docs/ 各归属地，这里只留指针。新增内容须符合 [docs/AGENTS.md](docs/AGENTS.md) 的分层归属与字数预算：超限先 relocate 到归属地，再 condense。
