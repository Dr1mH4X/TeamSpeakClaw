# TeamSpeakClaw Implementation Plan

## Overview
TeamSpeakClaw 是一个基于 Rust 编写的 LLM 驱动的 TeamSpeak 3 ServerQuery 机器人。它使用 OpenAI（或兼容提供商）来解释用户指令并执行服务器操作（踢出、封禁、戳一戳、私信等）。

## Completed Tasks
- [x] 项目结构与配置 (`settings.toml`, `acl.toml`, `prompts.toml`)
- [x] TS3 适配器 (`src/adapter/`)
    - [x] TCP 连接与心跳保活
    - [x] 协议编码/解码
    - [x] 事件解析 (文本消息, 用户进入, 用户离开, 用户列表)
- [x] 核心基础设施
    - [x] 基于服务器组的权限系统 (`src/permission/`)
    - [x] 客户端缓存 (`src/cache/`) 与自动刷新
    - [x] 审计日志 (`src/audit/`)
- [x] LLM 集成 (`src/llm/`)
    - [x] OpenAI 提供商
    - [x] 带工具调用的对话补全
- [x] 技能系统 (`src/skills/`)
    - [x] 技能注册表与 Trait 定义
    - [x] 通讯技能: 戳一戳 (Poke), 发送私信
    - [x] 管理技能: 踢出 (Kick), 封禁 (Ban)
    - [x] 信息技能: 获取用户列表
- [x] 事件路由器 (`src/router.rs`)
    - [x] 消息处理循环
    - [x] 上下文构建
    - [x] LLM 工具执行循环 (双轮对话)

## Next Steps
- [ ] 添加更多技能 (频道管理, 服务器信息)
- [ ] 实现速率限制 (`RateLimitConfig` 已定义但未实装)
- [ ] 增强审计日志 (记录具体操作结果)
- [ ] 为关键组件添加测试
- [ ] 支持 SSH 连接 (配置项已存在但仅实现了 TCP)

## Usage
1. 配置 `config/settings.toml`, `config/acl.toml`, 和 `config/prompts.toml`。
2. 运行 `cargo run`。
