# Agent Notes — 决策记录与技能规范

## 用途与适用范围

Agent Note 是项目内已落地决策的「为什么」留痕：记下当初的选择、放弃的备选与付出的代价，供人/AI 事后回顾。它不是提案工具，不写迁移计划，只留结论与理由。

非平凡变更（涉及架构、生命周期、并发、音频/语音桥、重连、接口取舍等有明确 tradeoff 的改动）应与代码同 PR 附带 Agent Note；纯文案、格式改动不必写。判定含糊时倾向写：补一条比缺一条便宜，追忆比存档贵。

## 生命周期与路径

每条笔记一个状态，写进文件头；状态变更即移动文件。四态：

- `implemented` — 已落地。本篇即现状，后续改动须同步更新其中已变化的事实（路径、命名、结构），不改决策本身。
- `proposed` — 提案，尚未实施（或只落了一部分）。
- `rejected` — 否决。否决理由就是读者要找的事实，保留到该理由不再能防止重复犯错时删除。
- `archived` — 归档冻结。决策已完备、理由不再指导未来工作后封存，永久只读，不作现状权威。

文件系统按状态分目录：`.agents/notes/{implemented,proposed,rejected,archived}/`（本地、git 忽略，不入库）。类目（class）取 feature / bug-fix / simplification / architecture / process / testing 六类中最贴近者。命名 `yyyy-mm-dd-主题-标题.md`，日期为首提日，例如 `2026-08-15-voice-bridge-fallback.md`。笔记间用相对链接互参。

## 文件内格式

头两行固定为：

```
# Agent Note: <标题>
Status: implemented — <一句话理由>
```

proposed/rejected 的 Status 行同理，例如 `Status: rejected — <一句话原因>`。状态行不携带日期，日期在文件名，其余交 git。

implemented 骨架：

```
## Problem
## Decision
…自定义小节…
## Alternatives considered
## Consequences
```

`## Decision` 用现在时描述已落地现状；`## Alternatives considered` 必写已考虑并放弃的备选（每个备选一段加粗论点），无备选的决策迟早被复诉；`## Consequences` 记录取舍的代价与所得。禁用 `## Proposal`、`## Plan`、`## Acceptance criteria` 等 spec 词汇，原因见 [AGENTS.md](AGENTS.md) 的 slop checklist 中 spec 语气与 CoT 泄漏两条。

proposed 骨架：

```
## Problem
## Proposal
## Acceptance criteria
## Risks
```

`## Proposal` 是意图变更，可合理用未来时——计划、迁移步骤与未决问题归此处。全篇只写结论与理由，不写过程流水账。状态迁移：proposed 落地时把 `## Proposal` 改写成现在时 `## Decision`，把 `## Acceptance criteria`/`## Risks` 折入 `## Consequences`；proposed 被否决仅在 `Status:` 行补原因并冻结。

## 技能规范（SKILL.md）

此处「技能」指 agent 侧自有技能（`.agents/skills/`，本地、git 忽略），与 `src/skills.rs` 的代码级 Skill（`execute` / `execute_unified` 两方法）无关。每个技能一个目录 `<name>/SKILL.md`，YAML frontmatter 固定 `name`（ASCII）与 `description`（写触发条件，供 agent 检索）。

SKILL.md 正文结构固定四段，全中文：

- sources of truth（读哪些源、不重述）：先列要读的源文件与文档，只引用、不重述其内容。
- 工作流步骤：按序的实操步骤。
- 验证命令：可执行的验证方式（测试、构建命令等）。
- 报告要求：输出格式与内容约定。

特定 agent 前端（如 `.claude/skills`）如需引用，用符号链接或复制接入，本项目暂不硬性要求。
