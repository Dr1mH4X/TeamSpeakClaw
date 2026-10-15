# AGENTS.md — Agent 技能

这里的「技能」指 agent 侧自有技能，与 `src/tools.rs` 的代码级 Tool（带 `execute` 方法）无关。技能目录随仓库入库，与它描述的改动一起提交。

## 目录与 frontmatter

每个技能一个目录 `.agents/skills/<name>/SKILL.md`，需要时可以带 `references/`、`templates/` 等从属文件。`SKILL.md` 的 frontmatter 固定两个字段：

```markdown
---
name: <ASCII 名>
description: <触发条件>
---
```

`name` 只用 ASCII；`description` 写「什么时候该用这个技能」，供 agent 检索，不写内容摘要。

## 正文结构

正文全中文，四段固定且按序出现：

- sources of truth：读哪些源文件与文档；只引用、不重述其内容，引用代码写模块路径与符号名而不是 `file:line` 行号。
- 工作流步骤：按序的实操步骤。
- 验证命令：可执行的验证方式。
- 报告要求：输出格式与内容约定。

技能正文只放流程与判据。规则本身留在它的归属地（`docs/` 或所在目录的 `AGENTS.md`），技能里链接过去，不重复一遍。

特定 agent 前端（如 `.claude/skills`）如需引用，用符号链接或复制接入，本项目暂不硬性要求。