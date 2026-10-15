# Agent Note: 体积重试的安全收缩与取消还原

Status: implemented

## Problem

`TurnRequest::run` 把「体积类失败就丢弃最旧音频并重试一次」当成回合级自愈，这条路径同时踩了三个坑。工具循环已经执行过工具后，下一次模型请求被网关按体积拒绝，整回合重放会把发消息、封禁这类副作用做第二遍（隔离复现：工具执行两次、provider 调用四次，回合最终还报成功）。`drop_earliest_audio_turn` 丢的是**存储历史**里最早的音频，而请求装配另有 `MAX_WIRE_AUDIO_BYTES` 裁剪：要丢的那一轮常常已经在不可见前缀里，丢完 `keep_from` 重算又会把更旧的音频顶回来，重试请求逐字节不变（复现：1641402 → 1641402）。附加的「共享 base64 缓存」改动则让编码结果活到音频被逐出——音频预算只统计 WAV 字节，256 MiB 上限处会额外留住约 341 MiB。

## Decision

工具循环把「已执行工具后」的模型错误收进 `ToolLoopError::AfterToolExecution`，`shrinkable_failure` 只认尚未执行工具的 `Other`，因此只有执行前的可收缩失败才会收缩并重试；取消错误原样保留，不误分类。

收缩与装配共用 `wire_keep_from`（可见后缀起点），删除「最早可删的可见音频 + 整个不可见前缀」。前缀必须一起删：只丢一轮可见音频时，释放出来的音频预算会让更旧的音频重新进入可见后缀，重试请求不会变小。只剩一条可见音频时返回 `None`，不动历史、不重试。

收缩返回收缩前的完整历史快照（音频是 `Arc<[u8]>`，浅拷贝），`TurnRequest::run` 在重试以 `Cancelled` 结束时用 `restore_history` 整体放回；其他失败保留收缩。共享 `encoded` 被放弃，缓存回到按对象/按视图存在，编码串随请求释放。

## Alternatives considered

- **只有重试成功才提交收缩**（Sourcery 建议）：网关限额低于 `MAX_WIRE_AUDIO_BYTES`（文档明确提到 1 MiB 网关）时，每回合都会 413 → 收缩 → 重试 → 413 → 还原，永远收敛不了；保留收缩才能让下一回合从更小的历史起步。只在取消分支还原就够。
- **非破坏性 wire floor（只改装配视图、不动存储）**：装配要把 floor 传进 `build_messages`，而音频逐出会移动索引、floor 必须跟着位移，改动面远大于收益；快照式还原保持了「先落地、失败再放回」的同一语义。
- **保留前缀文本、只丢音频**：前缀文本会重新进入可见后缀，把释放的音频预算又抵消掉，「重试请求一定变小」不再成立。
- **给音频预算加 base64 计数**：预算按 WAV 定义，编码是派生量；共享缓存的收益只有每回合一次毫秒级编码，直接退掉更简单。

## Consequences

- 所得：工具副作用不再因体积重试重放；重试请求严格变小；取消时删掉的轮次完整回来；不再有预算外的长期 base64 占用。
- 代价：体积失败触发收缩后，不可见前缀里的旧文本会从存储历史中永久消失（[LLM 引擎与上下文](../../../../docs/subsystems/llm.md) 已明确）；非取消类失败保留收缩，会话上下文会比失败前小。
- 验证：反向验证 —— 退回 `tool_loop` 分类后 `regression_payload_retry_must_not_repeat_completed_action` 报 `action ran 2 times, provider calls 4`；退回收缩语义后 `regression_retry_shrinks_already_clipped_wire_view` 报 `1641402 -> 1641402 bytes`；删掉取消还原后 `cancelled_retry_restores_the_history_the_shrink_removed` 报 `left: 2, right: 8`。`cargo test --all-targets --offline --locked` 400 passed，`cargo fmt --check` 与 `cargo clippy --all-targets --offline --locked -- -D warnings` 全绿。

相关：[模型调用失败按分类路由](../../implemented/architecture/2026-10-15-llm-failure-taxonomy.md) 定义 `LlmFailure` 的分类与 `is_shrinkable` 判定；[LLM 回合模块](../../implemented/architecture/2026-10-03-llm-turn-module.md) 是回合骨架与请求被拒后收缩自愈的归属地；[同会话回合按到达顺序取锁](../../implemented/bug-fix/2026-10-04-turn-arrival-order-tickets.md) 记录了会话锁的位置，收缩与还原都在锁内发生。