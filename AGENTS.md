# AGENTS.md

## 核心命令

- 构建: `cargo build --release`
- 运行: `cargo run`
- 检查: `cargo check`
- 格式化: `cargo fmt`
- 清理构建: `cargo clean`

## 项目架构

```
src/
├── main.rs             # 入口
├── cli.rs              # CLI 参数解析 (--log-level)
├── config/             # 配置加载
├── router/             # 事件路由
│   ├── sq_router.rs    # TeamSpeak ServerQuery 事件
│   ├── nc_router.rs    # NapCat/QQ 事件
│   ├── unified.rs      # 统一路由
│   └── headless_bridge.rs
├── adapter/
│   ├── serverquery/    # TeamSpeak ServerQuery (TCP/SSH)
│   ├── napcat/         # NapCat OneBot 11 (WebSocket)
│   └── headless/       # 语音桥接服务
├── llm/                # LLM 引擎 (OpenAI 兼容)
├── permission/         # 权限门控
└── skills/             # 技能系统 (music, moderation, information, communication)
```

## 构建依赖

- `protoc-bin-vendored`: `build.rs` 自动下载，无需手动安装
- Linux: `cmake libopus-dev`
- macOS: `brew install autoconf automake libtool`

## 配置文件

- 位置: 可执行文件同目录下的 `config/` 文件夹
- 模板: `examples/config/{settings.toml,acl.toml,prompts.toml}` → 复制为 `config/`

## 调试

- 日志级别: `RUST_LOG=debug cargo run` 或 `cargo run -- --log-level debug`

## CI/CD

- GitHub Actions: `.github/workflows/build.yml`
- 触发: main/master push, PR, tag (v*)
- 产物: Windows/macOS/Linux 多平台二进制 + Docker 镜像 (ghcr.io)

## 代码规范

- 项目使用 `.github/copilot-instructions.md` 定义开发规范

## 实现参考

- serverquery: https://yat.qa/resources/
- headless: https://github.com/yichen11818/NeteaseTSBot/raw/refs/heads/main/voice-service/src/main.rs
- napcatqq: https://napneko.github.io/
