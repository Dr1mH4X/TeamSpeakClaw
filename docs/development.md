# 开发流程

## 常用命令

- 构建发布版：`cargo build --release`
- 快速检查：`cargo check`
- 静态检查：`cargo clippy --all-targets --locked -- -D warnings`
- 测试：`cargo test --all-targets --locked`
- 格式化：`cargo fmt`（CI 另跑 `cargo fmt --check`）
- 清理：`cargo clean`

## 构建依赖

`build.rs` 用 `protoc-bin-vendored` 定位 protoc 并交给 `tonic_build` 编译 `proto/voice.proto`，本机无需预装 protoc。`.cargo/config.toml` 设置 `CMAKE_POLICY_VERSION_MINIMUM=3.5`，供受此策略约束的构建脚本正常工作，勿删除该配置。

各平台系统依赖：Linux 需 `cmake` 与 `libopus-dev`（运行镜像另需 opus/ffmpeg）；macOS 需 `autoconf automake libtool`；Docker 构建在 `rust:alpine` 内安装 `musl-dev cmake make gcc protoc`（显式 `ENV PROTOC=/usr/bin/protoc`），运行时镜像基于 `alpine:3.20`。具体装包步骤见 CI 各 workflow，见 [ci-cd.md](ci-cd.md)。

## 开发流程

动手前先读完相关文件与全部需求，不盲目编辑；只修改当前任务涉及的文件，不给未改动的代码补 docstring 或类型注解，避免无谓 diff。

注释与文档用中文（代码标识符除外），注释尽量少，只在逻辑不清晰处添加；不写行间散文。新增文件时注意目录结构合理，与 `src/` 现有模块组织一致；临时文件用后及时删除。

## git 约定

提交用 Conventional Commits 格式，类型取自 `feat / fix / docs / style / refactor / perf / test / chore / ci`，可选 scope。示例：`feat: add poke_client skill`、`docs: align AGENTS.md with current architecture`、`fix: serialize TTS turns in voice router`。变更日志由 `git-cliff` 按此格式生成，见 [ci-cd.md](ci-cd.md)。

## 子代理拆分

复杂问题（涉及多个独立子问题、需 Review/研究/并行分析）拆给子代理并行处理，保持主上下文干净；子代理禁止再拆子代理。临时任务产物不留在工作区。

## 输出规范

结论先行，短小精悍的中文大白话；代码任务直接给代码块，必要说明一两句放在后面；非代码任务不逐维度拆解简单问题，不复读背景。不要谄媚的开场白与空洞的结尾。结束前明确告知用到的 skill。代码逻辑本身（变量名、字符串字面量、命令）仅用 ASCII 字符，中文注释允许中文标点；禁止「稳稳接住」「打通」「你说得对」等填充词，不用「不是…而是…」式对比转折，直接陈述结论。

## CLI 工具偏好

用到对应能力时先读可用 skill。GitHub 操作（PR、Issue、Release、Actions）优先用 `gh`；查库的最新文档优先用 `ctx7`：`ctx7 library "<name>"` 后转 `ctx7 docs <id> <query>`，注意技术时效性，编码前以官方文档为准；网站搜索（需真实浏览器、绕过机器人限制）用 `fetch-any` skill；用 `opencli` 前设置 `OPENCLI_CDP_ENDPOINT="http://127.0.0.1:9333"`。涉及敏感数据文件的读取只用 `jq` 取结构，例如 `cat auth.json | jq 'keys'`，不整读内容。API key 等敏感配置放在 config 目录（`config_dir()`），不进 git、不写日志，见根 `AGENTS.md`。

编码准则（FAILFAST、YAGNI、DRY、禁警告压制、类型安全优先）见 [defensive-patterns.md](defensive-patterns.md)；测试写法见 [testing.md](testing.md)。
