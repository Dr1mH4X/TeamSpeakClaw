# Agent Notes — 决策记录

Agent Note 记录影响本代码库的决策或提案：代码与文档载不下的「为什么」，以及当初放弃了什么。本文件规定笔记放在哪里、何时需要写一篇、文件内怎么组织，以及人工核查清单。

## 布局与命名

每篇笔记的两个维度都编码在路径里：`{lifecycle}/{class}/yyyy-mm-dd-主题-标题.md`。

- **生命周期**（顶层目录）是笔记的状态，状态变化时文件在目录之间移动：
  - `proposed/` — 实施前评审的提案，尚未落地或只落了一部分。
  - `implemented/` — 决策已交付。记录决定了什么、否决了什么，并与实际交付的内容保持一致：代码后续移动文件、改常量或改默认值时，同一改动里同步更新这篇笔记的事实（路径、命名、结构），不改决策本身。见 [implemented/AGENTS.md](implemented/AGENTS.md)。
  - `rejected/` — 提案经讨论后被否决。只在它的否决理由仍能阻止一种诱人且代价明确的错误时保留。
- **类目**（嵌套目录）是决策的种类，见下方「分类」。

文件名里的日期是该主题**首次提出**的时间（以 git 历史为准）。笔记之间的交叉引用一律用相对 Markdown 链接，并且从笔记根写起：`[主题](../../implemented/architecture/2026-10-03-llm-turn-module.md)`，这样链接在目录之间移动后仍然有效，也能被机械核查。

活跃生命周期目录树本身就是工作清单：浏览目录或搜索仓库即可，不要建集中的 `INDEX.md`，它重复路径信息，还会让互不相关的改动争抢同一个文件。

本仓库的笔记只有中文：没有英文对侧文件与配对记录，也没有自动门禁。核查按本文件末尾的清单人工进行。

## 分类

| 类目 | 覆盖范围 |
|---|---|
| `feature` | 面向用户或模型的新能力。 |
| `bug-fix` | 修正缺陷，或弥补复盘发现的缺口。 |
| `simplification` | 在不增加能力的前提下移除代码、行为或对外范围。 |
| `architecture` | 关于**交付源码**的结构性决策：模块之间的关系、运行期词汇、归属边界。 |
| `process` | 代码**周边**的工具、策略或工作流，不涉及运行期行为。 |
| `testing` | 测试基础设施与策略。 |

`architecture` 与 `process` 的界线：architecture 关乎交付的源码，process 关乎围绕源码的工具与流程。`refactor` 有意不在集合内：它与 `simplification` 重叠，而「可观察行为是否改变」这条判别标准已经覆盖了它。

## 归档与删除

只描述局部文案调整或纯机械改动的 implemented 笔记直接删除，并修复或删掉指向它的入站链接；局部缺陷修复、性能改动、新能力或实质行为决策不会仅因实现规模小就符合删除条件。其他已落地笔记只有在交付决策完整、其依据不太可能再指导未来工作、但仍有历史决策价值时才归档；如果其中的备选方案、归属边界、否定性保证、持久化或协议语义、安全规则、重新引入条件仍有价值，就继续留在活跃树里。proposed 笔记永不归档：过时的提案转为 `rejected`。rejected 笔记只在仍能阻止一种可能发生的错误时保留。

归档路径为 `archived/{class}/yyyy-mm-dd-主题-标题.md`，其中有意省略 `implemented`，因为只有已落地的笔记可以进入归档。归档时整篇移动，保留 `Status: implemented`，在两种语言都不涉及的正文里仅于状态行下方插入一行 `Archived: YYYY-MM-DD`，并修复或删掉入站链接——这是归档允许的全部内容改动。归档后文件永久冻结：不再编辑、翻译、重新格式化、更新、移动或删除，也不作为当前行为的权威依据。见 [archived/AGENTS.md](archived/AGENTS.md)。

## 何时需要写一份

只有代码、测试与现有文档解释不了的决策理由具有长期价值时，才在同一个 PR 里新增或更新一篇笔记；对未来重大工作的提案从 `proposed/` 开始，已经做出的决策从 `implemented/` 开始。非平凡变更（架构、生命周期、并发、音频与语音桥、重连、接口取舍等有明确 tradeoff 的改动）应当附带笔记；纯文案与格式改动不必写。判定含糊时倾向写：补一条比缺一条便宜。

更新已经拥有该决策的笔记即可满足规则，不要创建重复记录。机械性或局部编辑可以豁免。笔记永远不会被改写成另一个决策：用新笔记取代旧记录，并让两篇互相链接；被完全取代的旧记录可以合并进当前持有该决策的笔记并删除，合并前当前笔记必须保存旧记录独有的决策依据、备选方案、代价与验证方式。

## 文件内格式

每篇笔记的前三行严格为：

```markdown
# Agent Note: <标题>

Status: <状态>
```

`Status:` 只有三种形式，且必须与文件所在的生命周期目录一致：

- `Status: proposed`
- `Status: implemented`
- `Status: rejected — <一句话说明否决原因>`

状态行不带日期、不带补充说明：日期在文件名里，其余交给 git。否决原因是唯一带内容的状态，因为读者查阅被否决的笔记时，要找的正是这个结论。

正文以 `## Problem` 开头，写法上不依赖解决方案即可独立成文。后续章节取决于生命周期，固定章节只用下面这些名称，真正独特的技术章节（拓扑、协议约定、schema 等）放在必需章节之间自由组织。

引用代码写模块路径与符号名（如 `src/router/turn.rs` 的 `TurnPermit::reserve`、`src/llm/context.rs` 的 `history_for_wire`），不写 `file:line` 行号：行号随任何一次编辑漂移，而漂移没有任何门禁能发现。

implemented 骨架：

```markdown
## Problem
## Decision
…自定义小节…
## Alternatives considered
## Consequences
```

`## Decision` 用现在时描述已交付的现实；提案阶段的标题在这里属于规格用语，会被当作 slop：`## Proposal`、`## Plan`、`## Acceptance criteria` 不得出现在 implemented 笔记里。`## Testing`、`## Deferred`、`## Related` 等小节在陈述现在时的事实时可以使用。

proposed 骨架：

```markdown
## Problem
## Proposal
…自定义小节…
## Alternatives considered
## Acceptance criteria
## Risks
```

`## Proposal` 描述拟议的变更，尚未完成时用将来时合理；`## Acceptance criteria` 说明什么可观察状态意味着完成；`## Risks` 涵盖可能出错的事以及这次变更有意放弃的东西。

每篇笔记都必须有 `## Alternatives considered`：每个真实的备选方案及其落选原因，每个方案一段，用加粗论点引导。只记录决策而不记录它击败了什么，等于邀请同一场争论重来。

生命周期之间移动文件，意味着在同一个改动里更新 `Status:` 行并满足目标目录的骨架要求：`proposed` → `implemented` 把 `## Proposal` 改写成现在时的 `## Decision`，把 `## Acceptance criteria` 与 `## Risks` 折入 `## Consequences`（或一个现在时的验证章节），并用实际交付的内容替换计划；`proposed` → `rejected` 只在 `Status:` 行补上原因并冻结正文。

## 核查清单

改完笔记后逐条自查：

- 路径是 `{lifecycle}/{class}/yyyy-mm-dd-主题-标题.md`，且 `Status:` 与所在目录一致。
- 正文以 `## Problem` 开头，implemented 用 `## Decision`，proposed 用 `## Proposal`；implemented 里没有 `## Plan`、`## Acceptance criteria` 这类 spec 语气。
- `## Alternatives considered` 存在，且写的是真实考虑过并放弃的方案。
- 每一条笔记间链接都能解析到仓库里的文件（`](../../implemented/<class>/<file>.md)` 形式）。
- 没有 `file:line` 行号引用：代码只写模块路径与符号名，文档只写文件名与小节名。
- 没有 emoji、没有历史叙述（「之前/现在/不再」）、没有过程流水账。