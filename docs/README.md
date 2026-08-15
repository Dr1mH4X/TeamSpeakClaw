# 文档索引

`docs/` 是 TeamSpeakClaw 的开发者文档仓库：一部分是项目自身的文档标准（`AGENTS.md`），其余是按分层归属各自承载一类事实的开发者文档。规则归属遵循「一事实一归属」，先用 [AGENTS.md](AGENTS.md) 判断某条事实放哪个文档。

- [AGENTS.md](AGENTS.md) — 文档标准：分层归属、写作规则、字数预算与精简版 slop checklist，写或改本目录任何文档前先读。
- [architecture.md](architecture.md) — 架构：系统拓扑、入口流、双入站适配器、文本路由与语音桥、关键代码路径、LLM 引擎、权限与技能体系、层级规范。
- [development.md](development.md) — 开发流程：常用命令、构建依赖、git 约定、子代理拆分、输出规范与 CLI 工具偏好。
- [testing.md](testing.md) — 测试约定：单测摆放与标注、断言规范、测试范围选择、CI 测试门。
- [defensive-patterns.md](defensive-patterns.md) — 编码反面准则：最高原则、FAILFAST、YAGNI、DRY、警告压制处置、类型安全优先、审查与调试。
- [ci-cd.md](ci-cd.md) — CI/CD：各 GitHub Actions workflow 的触发条件、职责与产物，以及 git-cliff 变更日志。
- [agent-notes.md](agent-notes.md) — 决策记录：为什么这么做、放弃了什么、如何验证。

仓库级 standing orders 见根 `AGENTS.md`，源码内的模块级规则见各模块 `.instructions.md`，面向最终用户的文档在 `website/`。
