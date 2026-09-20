# 贡献指南

欢迎贡献。本文件不重复项目约定：贡献前请阅读本仓库指导文件，动手前读完与本次改动相关的那几篇。文档索引见 [docs/README.md](docs/README.md)，分层归属与写作规则见 [docs/AGENTS.md](docs/AGENTS.md)。

## 起步

- 全局 standing orders：[AGENTS.md](AGENTS.md)
- 开发者文档索引：[docs/README.md](docs/README.md)
- 文档写法与归属：[docs/AGENTS.md](docs/AGENTS.md)

## 改什么先读什么

| 改动 | 指导文件 |
|---|---|
| `src/` 代码 | [docs/architecture.md](docs/architecture.md)、[docs/defensive-patterns.md](docs/defensive-patterns.md) |
| 构建、常用命令、git、输出规范 | [docs/development.md](docs/development.md) |
| 测试与提交前质量门 | [docs/testing.md](docs/testing.md)、[docs/ci-cd.md](docs/ci-cd.md) |
| 架构、生命周期、并发、音频/语音桥、重连等非平凡变更 | [docs/defensive-patterns.md](docs/defensive-patterns.md)、[docs/agent-notes.md](docs/agent-notes.md) |
| 新增或修改 `docs/` | [docs/AGENTS.md](docs/AGENTS.md) |
| 源码模块内规则 | 对应模块的 `.instructions.md` |
| 面向最终用户的文档 | `website/` |

## 提交

质量门、Conventional Commits、注释与编码准则均以指导文件为准，不在此另立标准：

- 命令与 git 约定：[docs/development.md](docs/development.md)
- 测试写法与 CI 质量门：[docs/testing.md](docs/testing.md)
- FAILFAST、YAGNI、DRY、禁警告压制、类型安全优先：[docs/defensive-patterns.md](docs/defensive-patterns.md)
- 非平凡变更须附决策记录：[docs/agent-notes.md](docs/agent-notes.md)
- 敏感配置不进 git、不写日志：[AGENTS.md](AGENTS.md)

## Issue 与安全

Bug 报告使用 [.github/ISSUE_TEMPLATE/bug_report.md](.github/ISSUE_TEMPLATE/bug_report.md)。安全漏洞按 [SECURITY.md](SECURITY.md) 提交，不在公开 Issue 贴出 API key、token 或配置文件中的密文。

## 写文档

新增或修改贡献相关说明时，先按 [docs/AGENTS.md](docs/AGENTS.md) 判断归属地；本文件与 `docs/` 中只保留链接，不重复规则。
