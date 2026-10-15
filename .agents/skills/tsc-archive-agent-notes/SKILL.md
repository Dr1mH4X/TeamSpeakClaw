---
name: tsc-archive-agent-notes
description: 新增、审计、清理、归档、恢复或评审 .agents/notes/ 下的 Agent Note 时使用；检查每篇新笔记是否取代了旧记录，按未来指导价值区分删除与归档，并把该冻结的记录移入 archived/。
---

# 维护 Agent Note 语料

目标是缩小活跃决策语料，同时不抹掉仍能指导工作的历史。每篇笔记都按语义判断；字数和存续时间只用来发现候选，永远不是归档判据。

## Sources of truth

先读 [笔记规则](../../notes/README.md)、[归档规则](../../notes/archived/AGENTS.md)、[已落地笔记规则](../../notes/implemented/AGENTS.md) 与 [docs/AGENTS.md](../../../docs/AGENTS.md)。用当前代码、配置、文档更新、更新的笔记与入站链接判断一条依据是否还在约束什么。

## 工作流步骤

1. **新增笔记时做取代性检查**：在活跃树里搜索覆盖同一决策、机制或被否决方案的笔记，逐条判定完全取代还是部分取代。完全取代的 implemented 笔记在同一个 PR 里归档或删除，部分取代的保留并互相链接，过时的 proposed 转 `rejected`，不再能阻止错误的 rejected 笔记删除。
2. **按未来价值分类已落地笔记**：
   - 直接删除：只描述局部文案调整或纯机械改动的笔记，删完修复或删掉入站链接。局部缺陷修复、性能改动、新能力与实质行为或归属决策不会仅因实现规模小就符合删除条件。
   - 保留：其依据、备选方案、否定性保证、持久化或协议语义、归属边界、安全规则、重新引入条件仍可能指导未来改动。判决策本身，不判实现的机械程度；长度不影响判定。
   - 归档：决策已完整落地、不太可能再指导未来工作，但历史依据仍值得留存。
3. **proposed 永不归档**：仍值得推进就留在活跃树；不再值得推进就写清原因转 `rejected`。
4. **rejected 只作为护栏保留**：仅当那个方案仍是一种诱人且代价明确的错误时保留，否则整篇删除并修复入站链接。
5. **归档一篇已落地笔记**：整篇从 `implemented/{class}/` 移到 `archived/{class}/`，正文一字不改，只在 `Status: implemented` 下方插入一行 `Archived: YYYY-MM-DD`；然后搜索入站链接，改指当前权威、或按有意引用历史改为归档路径、或删除。归档后不再核查或修复归档笔记向外的链接。
6. 不要为凑数量而归档。范围里的每篇都要看，同类按同一原则判，边界情形记录下来交回。

## 验证命令

```sh
# 活跃树里是否还有指向被删/被移动笔记的入站链接
grep -rn "2026-" --include="*.md" docs/ .agents/notes/ AGENTS.md CONTRIBUTING.md

# 归档笔记的路径、状态行与归档行是否合规
find .agents/notes/archived -name "*.md" | sort
grep -rn "^Status: implemented$" .agents/notes/archived
grep -rn "^Archived: " .agents/notes/archived
```

本仓库没有归档校验脚本，上述检查与[笔记规则](../../notes/README.md)末尾的核查清单是唯一门禁。

## 报告要求

列出：保留的 implemented 笔记、删除或归档的 implemented 笔记、保留或删除的 rejected 笔记、转为 rejected 的 proposed 笔记，以及每个边界情形及其判定结果。不要声称归档笔记向外的链接仍然有效。