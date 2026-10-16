---
name: tsc-release-log
description: 发版前写 docs/release/<tag>.md、补写或改写某个版本的 Release 正文时使用；按 git 提交归纳这一版面向读者的变化，并把它作为 GitHub Release 正文发布。
---

# 发版日志

发版日志是仓库里的一个文件：`docs/release/<tag>.md`，文件名与本版 git tag 完全一致。它由 git 提交归纳而成，不依赖 git-cliff 之类的生成器；`.github/workflows/release.yml` 在打 tag 时直接以它为 Release 正文，文件缺失就失败。

## Sources of truth

- [docs/release/AGENTS.md](../../../docs/release/AGENTS.md)：文件格式、分类顺序、生命周期与写法规则的唯一归属地，动笔前先读。
- [docs/ci-cd.md](../../../docs/ci-cd.md)：发布流程与 workflow 职责。
- [docs/development.md](../../../docs/development.md)：Conventional Commits 约定与常用命令。
- git：上一个 tag、范围内的提交主题与正文、merge commit 里的 PR 号、提交作者。

## 工作流步骤

1. **定 tag 与范围**：tag 由用户指定；未指定时按 semver 从上一个 tag 推断——范围内有 `feat` 或破坏性变更进 minor/major，否则进 patch。`git fetch --tags` 后取 `git describe --tags --abbrev=0` 作为上一版，范围是它到 `HEAD`。
2. **读全部提交**：逐条看主题与正文，把每个 `Merge pull request #N` 与它带进的改动对应起来，这样条目能附上 `（#N）`；直接提交到主干的提交没有 PR 号，省略即可。不要只读主题就动笔。
3. **按分类写条目**：分类与顺序见 [docs/release/AGENTS.md](../../../docs/release/AGENTS.md)。每条一句话讲行为或归属上的变化，不抄提交标题原文；只写本版真实存在的内容，没内容的分类整节不写。判断不准某条改动的用户可见性时，用 `git show <sha>` 看它实际改了什么。
4. **写升级注意**：检查范围内是否动了 `config/settings.toml` 或 `config/acl.toml` 的键、环境变量、CLI 参数、消息路由与回复行为、上下文或数据格式。有人需要动手就写清受影响的人与确切动作；没有就删掉整节。
5. **写贡献者**：`git log <range> --pretty=%an` 取人类作者，机器人账号（如 `dependabot[bot]`）不计，写成 `@handle`。
6. **落盘并随改动提交**：写 `docs/release/<tag>.md`，第一行是标题行 `# <版本> (<发布日期>): <主要更新简述>`，例：`# 0.6.2 (2026-10-03): 唤醒词与语音反馈`；这一行同时是 GitHub Release 的标题，写法判据见 [docs/release/AGENTS.md](../../../docs/release/AGENTS.md)。与该版改动同一个 PR。tag 打完后冻结，只有事实写错才回头修改，并在提交信息里说明改的是哪一处。
7. **发布**：tag 推送后 CI 自动把该文件作为 Release 正文；需要手工补发或改写时用 `gh`（见下）。未知 tag 不要发明版本号：先与用户确认。

## 验证命令

```sh
git fetch --tags
git describe --tags --abbrev=0
git log --oneline "$(git describe --tags --abbrev=0)"..HEAD

# 文件与 tag 必须同名
ls docs/release/
test -f "docs/release/<tag>.md" && echo ok

# 格式自查：首行标题的版本、日期与简述，分类节名与顺序符合 AGENTS.md
head -n 1 "docs/release/<tag>.md"
grep -n '^## ' "docs/release/<tag>.md"

# 发布与复核：标题取该文件首行
gh release create <tag> --title "$(head -n 1 docs/release/<tag>.md | sed 's/^# //')" --notes-file "docs/release/<tag>.md"   # -beta 时加 --prerelease
gh release edit <tag> --title "$(head -n 1 docs/release/<tag>.md | sed 's/^# //')" --notes-file "docs/release/<tag>.md"
gh release view <tag> --json name,body
```

## 报告要求

给出：目标 tag、标题行（版本、发布日期、主要更新简述）、提交范围与提交数、每个分类的条目数、升级注意的条目（没有就说没有）、贡献者、写出的文件路径。拿不准用户可见性的改动单独列出并说明判断依据，不要为了凑齐分类而写条目。