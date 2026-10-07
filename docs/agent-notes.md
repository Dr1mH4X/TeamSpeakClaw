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

## 决策记录

### Agent Note: 会话生命周期与语音桥就绪的唯一归属

Status: implemented — 会话阶段、重连退避策略与语音桥组件就绪都归 `src/adapter/lifecycle.rs`，其余模块只上报事件或只读状态。

#### Problem

会话阶段与重试决策曾散在三个位置：`adapter.rs` 的建连失败、订阅取出失败与会话结束三处各自记账，`reconnect.rs` 持有退避状态机，`headless.rs` 的语音桥循环另行手写 attempt 计数与闩重置，重试语义因此不可测。语音桥就绪由一组 setter 分散写入 headless、`voice_router` 与 `actor`，读取方只能看到拼接出的布尔值。

#### Decision

`src/adapter/lifecycle.rs` 是会话阶段（`SessionPhase`：Connecting/Initializing/Running/Closed）、重连退避策略（`ReconnectState`、`RetryDecision`、`reconnect_delay_for_attempt`、`wait_for_retry`）与语音桥就绪（`BridgeReadiness` 经 `BridgeComponent` 的 `set_up`/`set_down` 上报，`is_ready()` 要求 Service/Stream/Actor 三者全 Up）的唯一归属地；`reconnect.rs` 只保留任务回收与限时等待工具。`adapter.rs` 的失败处理收敛到 `retry_after_failure` 单一决策点，调用方用 `FailureKind`（Connection/Subscription/Initialization/Running）声明失败来源，日志措辞、失败归类与退避等待都在决策点内完成；`headless.rs` 的桥接重试改用 `run_retry_loop`，循环预标记会话已建立因而无界，单次尝试内建立过订阅流时经 `take_stream_established()` 重置退避。退避序列仍是 10/30/60/120/300 秒，尝试上限 5 次且只对未进入运行会话的启动周期生效。

#### Alternatives considered

**NapCat 的两处重试循环不复用共享驱动。** 它们的有界与无界语义、耗尽日志与控制流各不相同，复用驱动会改动其可观测行为，因此留在 `adapter/napcat/ws.rs`。

**headless 的三条重复就绪测试不做保留。** 三者只覆盖组件标志的置位与全 Up 判定，现由 `lifecycle.rs` 的 `BridgeReadiness` 测试覆盖，随旧门面一并删除。

**旧的 `VoiceBridgeState` 门面不保留为薄包装。** 四个 setter 与 `is_ready()` 的转发层没有额外语义，删除后写入点统一到 `BridgeReadiness`。

#### Consequences

组件侧只剩「上报 Up/Down」与「读 `is_ready()`」两种用法，`ts_router` 只在三者全 Up 时把文本交给语音桥；桥接重试的记账、重置与等待不再由 headless 拥有，行为由 `lifecycle.rs` 单测覆盖。代价是 `headless.rs` 需按 `AttemptOutcome` 报告单次尝试的结局，并在预标记会话已建立后保留一条实际不可达的耗尽日志分支。

验证：`cargo fmt --check`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo test --all-targets --locked`（345 通过）、`cargo build --release` 全部通过；订阅取出失败路径在尝试耗尽时保持与重构前一致的静默返回（不新增耗尽日志），该差异由 `lifecycle.rs` 的纯映射单测固定（`exhaustion_log(FailureKind::Subscription) == None`）。`grep -rn "VoiceBridgeState\|set_service_running\|set_stream_ready\|set_actor_ready\|take_connected_since_retry" src/` 与 `grep -rn "let mut attempt" src/` 无匹配，`record_failure` 只出现在 `lifecycle.rs` 与 `adapter/napcat/ws.rs`，`wait_for_retry(` 在 `lifecycle.rs` 之外只剩 NapCat 两处调用与一处测试。
