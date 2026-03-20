# TeamSpeakClaw 开发指南_中文

[English](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/docs/Development.md)|[Chinese](https://github.com/Dr1mH4X/TeamSpeakClaw/blob/main/docs/Development_CN.md)

欢迎来到 TeamSpeakClaw 开发文档！本指南将帮助你搭建开发环境、理解项目结构并为新功能的开发做出贡献。

## 1. 项目概述

**TeamSpeakClaw** 是一款基于 Rust 编写的 TeamSpeak ServerQuery 机器人。它利用大语言模型（LLM）为服务器管理和实用工具提供自然语言交互能力。

### 技术栈
- **编程语言**: Rust (2021 Edition)
- **异步运行时**: `tokio`
- **HTTP 客户端**: `reqwest`
- **CLI 框架**: `clap`
- **配置管理**: `toml` 配合 `serde`
- **日志系统**: `tracing` 生态

---

## 2. 环境准备

在开始之前，请确保已安装以下工具：
- **Rust 工具链**: 最新稳定版（建议通过brew或scoop安装）。
- **Git**: 用于版本控制。
- **TeamSpeak 3 Server**（可选）: 用于测试机器人交互的本地或远程服务器。

---

## 3. 快速入门

### 克隆仓库
```bash
git clone https://github.com/Dr1mH4X/TeamSpeakClaw.git
cd TeamSpeakClaw
```

### 配置文件
应用运行需要配置文件。你可以使用命令行工具生成默认文件：

```bash
cargo run -- --config generate
```

这将在 `config/` 目录下创建以下文件：
- `settings.toml`: 主应用设置（TeamSpeak 凭据、LLM API 密钥）。
- `acl.toml`: 访问控制列表（定义哪些 TS 用户组可以使用哪些技能）。
- `prompts.toml`: 自定义系统提示词和错误消息。

**重要提示**：在运行机器人前，必须编辑 `config/settings.toml` 并添加你的 TeamSpeak 服务器详情和 LLM API 密钥。

### 编译与运行
以 Release 模式编译项目：
```bash
cargo build --release
```
二进制文件将位于 `target/release/teamspeakclaw`。

在开发期间本地运行：
```bash
cargo run
# 或者开启调试级别日志
cargo run -- --log-level debug
```

---

## 4. 项目架构

源代码组织在 `src/` 目录下，结构如下：

| 目录/文件 | 描述 |
|---|---|
| `main.rs` | **应用入口**。初始化配置、日志、LLM 引擎和主事件循环。 |
| `router.rs` | **事件路由**。核心逻辑：接收 TS 事件、咨询 LLM、检查权限并分发技能。 |
| `adapter/` | **TeamSpeak 适配器**。处理原始 ServerQuery 连接、命令发送和事件解析。 |
| `config/` | **配置管理**。定义配置结构体并处理加载/保存逻辑。 |
| `llm/` | **LLM 集成**。`LlmEngine` 管理上下文/对话历史；`provider.rs` 实现 API 调用。 |
| `skills/` | **功能技能**。包含具体机器人行为的逻辑（如 `music.rs`, `moderation.rs`）。 |
| `permission/` | **鉴权系统**。`PermissionGate` 检查用户（基于 TS 组 ID）是否有权执行特定技能。 |
| `cache/` | **状态缓存**。维护服务器状态的本地视图（如在线客户端列表）。 |
| `audit/` | **审计日志**。将管理操作记录至 `logs/audit.jsonl`。 |

---

## 5. 开发工作流

### 添加新技能 (Skill)
若要为机器人添加新能力（例如“天气”查询）：

1. 在 `src/skills/` 中**创建一个新模块**（如 `weather.rs`）。
2. **实现 `Skill` 特性 (Trait)**。该 Trait 定义了技能如何被触发和执行。
3. 在 `main.rs` 中**注册该技能**，以便路由系统识别。
4. 在 `config/acl.toml` 中**添加权限规则**，控制使用权限。

### 测试
运行测试套件以确保更改未破坏现有功能：
```bash
cargo test
```
项目使用 `tokio-test`、`mockall` 和 `wiremock` 来测试异步组件并模拟外部服务。

### 代码风格
我们遵循标准的 Rust 社区准则。请在提交 PR 前确保代码已格式化且无 Lint 错误：

```bash
cargo fmt
cargo clippy
```

---

## 6. 贡献指南

我们欢迎任何形式的贡献！请遵循以下步骤：
1. Fork 本仓库。
2. 为你的功能或修复创建新分支。
3. 提交记录清晰的 Commit。
4. 确保所有测试通过且代码格式正确。
5. 提交 Pull Request 并详细描述你的更改。

---
