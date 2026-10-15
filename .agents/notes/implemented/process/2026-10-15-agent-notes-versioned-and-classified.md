# Agent Note: 决策记录按生命周期与类目分树并入库

Status: implemented

## Problem

决策记录原先平铺在 `.agents/notes/{implemented,proposed}/` 两个目录下，状态只有目录这一层信息，改动的种类（新能力、缺陷修正、简化、结构、流程、测试）无从体现；文件多起来之后，按主题寻找旧记录只能靠全仓库搜索文件名。

同时 `.agents/` 整目录在 `.gitignore` 里，决策记录不随它描述的改动入库：评审看不到这些记录，事后也无法从某个提交追溯到当时的取舍，而「非平凡变更附带决策记录」这条约定正是要在评审时生效。

## Decision

笔记路径编码两个维度：`{lifecycle}/{class}/yyyy-mm-dd-主题-标题.md`。生命周期是 `proposed`、`implemented` 与冻结的 `archived`；类目是 `feature`、`bug-fix`、`simplification`、`architecture`、`process`、`testing`。文件名里的日期取主题首次提出日。

`.agents/notes/` 与 `.agents/skills/` 随仓库入库，只有 `.claude/`、`opencode.json`、`plan.md`、`skills-lock.json` 保持忽略。`Status:` 行只写生命周期本身，否决原因才附在状态行上。笔记之间的引用一律从笔记根写起，例如 `../../implemented/architecture/2026-10-03-llm-turn-module.md`，这样文件在生命周期之间移动后链接仍然有效。

规范不放在一篇大文档里，而是拆成 `.agents/notes/README.md`（布局、分类、归档、文件内格式、核查清单）与三个目录级 `AGENTS.md`（笔记树、`implemented/`、`archived/`），这样在 notes 下的任何会话都会自动加载对应规则。活跃目录树本身就是清单，不建集中的 `INDEX.md`。

`docs/agent-notes.md` 因此被 `.agents/notes/README.md` 取代；技能相关的规范移入 `.agents/skills/AGENTS.md`。

## Alternatives considered

- **继续忽略 `.agents/`**：决策记录无法在评审里被看到，也无法从代码改动追溯到当时的取舍，等于放弃「同一 PR 附带记录」这条约定；记录与它描述的代码还会各自漂移。
- **保持平铺、在正文里标注状态**：状态与种类仍然只能靠通读正文获得，重命名或移动文件时正文里的声明不会跟着变；用路径表达则移动与状态变更天然留下 git 记录。
- **保留集中的 `INDEX.md`**：它重复目录树里已有的路径信息，每次新增或移动笔记都会让互不相关的改动争抢同一个文件，也更容易失效；浏览与搜索已经够用。
- **只入库 `implemented/`，`proposed/` 留在本地**：被否决与在评审的提案恰好是最需要被看到、被链接的部分；分开存放还会让生命周期之间的移动跨出仓库。
- **给笔记加自动校验脚本**：仓库只有 Rust 工具链，为 Markdown 语料引入脚本与依赖的收益低于它带来的维护面；核查清单与评审仍然是门禁。

## Consequences

新增或移动一篇笔记时，状态变更体现为跨目录移动；引用必须写成从笔记根出发的相对路径，否则移动后失效。

`.agents/notes/README.md` 的核查清单是唯一的人工门禁，没有脚本兜底，评审时要按清单过一遍。`.agents/` 进入版本控制后，笔记的改动与代码改动在同一提交里出现。

`docs/agent-notes.md` 作为文档层级对外的入口消失，根 `AGENTS.md`、`docs/AGENTS.md`、`CONTRIBUTING.md` 与 `docs/architecture.md` 的指针改为指向 `.agents/notes/README.md`。