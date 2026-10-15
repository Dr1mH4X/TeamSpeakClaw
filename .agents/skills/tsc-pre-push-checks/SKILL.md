---
name: tsc-pre-push-checks
description: 推送、强推、把 PR 标为可评审或声称检查通过之前使用：按即将推出的 diff 选出覆盖它的最小检查集，而不是反射式地跑全量套件。
---

# 推送前的检查

本技能只决定「跑哪些命令」。判据与命令来自 [docs/testing.md](../../../docs/testing.md)、[docs/development.md](../../../docs/development.md) 与 [docs/defensive-patterns.md](../../../docs/defensive-patterns.md)，这里不重述。

## Sources of truth

- [docs/testing.md](../../../docs/testing.md)：单测摆放、断言规范、CI 测试门。
- [docs/development.md](../../../docs/development.md)：常用命令与 git 约定。
- [docs/defensive-patterns.md](../../../docs/defensive-patterns.md)：生命周期、并发、音频与语音桥、重连相关改动的前置阅读要求。
- [.github/workflows/ci.yml](../../../.github/workflows/ci.yml)：push 的路径过滤与 PR 的实际门禁。

## 工作流步骤

1. **确认分支与范围**：

```sh
git status --short --branch
git diff --stat origin/main...HEAD
```

2. **按改动面选最小证据**（每个行为改动都要有「回归时会失败」的那条测试或检查）：
   - `src/` 中某个模块的行为：定向跑拥有它的测试，例如 `cargo test --locked <测试名或模块前缀>`；改动共享契约时再补相邻模块的测试。
   - 上下文、回合、并发门禁、生命周期、音频与语音桥、重连：先按 [docs/defensive-patterns.md](../../../docs/defensive-patterns.md) 的清单逐条自查，再跑这些模块的定向测试，最后跑一次 `cargo test --all-targets --locked`。
   - `docs/`、`.agents/notes/`、`AGENTS.md`、`CONTRIBUTING.md`：仓库内相对链接与路径是否解析（见下面验证命令），笔记还要按[核查清单](../../notes/README.md#核查清单)过一遍。
   - `.github/workflows/`、`Dockerfile`、`.cargo/`：本机无法复现的，推到 PR 后看 `gh pr checks`，不要在本地假装验证过。
   - `examples/config/`、`website/docs/` 与配置键：确认三处同步（示例配置、中文文档、英文镜像）。
3. **只在必要时做全量排练**：用户明确要求、正在诊断 CI 失败、或改动跨到没有更窄集合可信时才跑 `cargo fmt --check`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo test --all-targets --locked`。不要因为「反正要提交」重复一遍刚通过的检查。
4. **保护重写历史的推送**：重写前先取远端分支的确切 OID，用 `git push --force-with-lease=<branch>:<observed-oid>` 发布，绝不用裸 `--force`。重写后重新取远端 head，重新审查评审线程、批准与检查状态。
5. **失败处置**：相关检查失败就停下修掉，不要先推再看 CI。怀疑是环境差异时，记录确切命令、失败的测试与平台差异，先确认与平台无关的证据全部通过，再决定是否绕过。
6. **推送后**：`git rev-parse HEAD origin/$(git branch --show-current)` 确认远端 ref 与本地一致，然后看 `gh pr checks`；pending 就报 pending，失败先看日志再归因。

## 验证命令

```sh
# 改动面
git status --short --branch
git diff --stat origin/main...HEAD

# 定向测试（例子）
cargo test --locked voice_turns
cargo test --locked tool_loop

# 文档与笔记的链接核查
grep -rn "](.*\.md" docs/ .agents/ AGENTS.md CONTRIBUTING.md | grep -v "http" 
find .agents/notes -name "*.md" | sort

# 全量门禁（按需）
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked

# 推送与远端状态
git rev-parse HEAD origin/$(git branch --show-current)
gh pr checks
```

## 报告要求

报告：改动范围（哪些路径）、选了哪些检查及理由、每条命令的结果、跳过了哪些检查以及为什么、远端 ref 与本地是否一致、`gh pr checks` 的当前状态。pending 一律报 pending，不要写成通过。