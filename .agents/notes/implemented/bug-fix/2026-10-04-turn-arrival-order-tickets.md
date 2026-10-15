# Agent Note: 同会话回合按到达顺序取锁（到达顺序票据）

Status: implemented

## Problem

同会话的两条消息在同一批事件里被连续 spawn 时，谁先拿到会话串行锁取决于哪个任务先被 poll。tokio 多线程运行时受 LIFO 槽与任务窃取影响，后 spawn 的任务可能先执行，于是回复顺序与历史落库顺序相对到达顺序反序。实测（`#[tokio::test(flavor = "multi_thread")]`，2000 轮两两 pairing）反序率约 0.05%–0.15%，与 worker 数无关；概率低但真实，且只在负载下出现。PR #95 的用例在单任务里手动 poll 两个 future，构造不出调度器带来的反序。

## Decision

- `TurnCoordinator` 增加每会话「到达顺序链」：`chain: Mutex<HashMap<String, oneshot::Receiver<()>>>`，值是该会话最后一条票据的完成信号接收端。`enqueue_ticket(source)` 在同一把同步锁里取走上一条的接收端、放进本条的发送端，返回 `TurnTicket`。链的顺序即事件循环里的到达顺序。
- 票据在事件循环里同步生成（`LlmEngine::enqueue_turn_ticket`），紧邻 `TurnPermit::reserve`；两者都不 await，循环不被回合阻塞。`TurnPermit::acquire_session(self, llm, source, ticket)` 先 `ticket.wait_for_predecessor().await`，再取会话串行锁。
- 票据持有者是 `TurnSession`（字段顺序 `_permit` → `_session` → `_ticket`，即释放顺序：容量 → 串行锁 → 票据），`Drop` 时 `notify.send(())` 放行后继者；回合结束点因此是「回复落库完成」。
- 前一条回合的任务被丢弃时发送端随之消失，`wait_for_predecessor` 收到 `Err` 同样放行；后继者仍有串行锁兜底，不会与仍在跑的回合并发。
- 链上每个会话留一个条目，会话数无上限（NapCat 用户 id 可一直新增），`chain.len()` 达到 `SESSION_CHAIN_SWEEP_THRESHOLD = 1024` 时用 `try_recv()` 扫掉已完成的条目（`Empty` 表示前一条还没结束，保留）。

## Alternatives considered

- **靠 `spawn` 顺序 + 运行时公平性**：等价于「不修」，反序率虽低但可复现；且 PR 的排序保证只是概率性的。
- **在 `TurnCoordinator` 里维护显式等待队列（FIFO 队列 + 唤醒）**：要自己管「等待者的公平唤醒」与取消清理，票据链用 oneshot 的 Drop 语义表达同样的顺序，条目数与代码量都更小。
- **用 `tokio::sync::Semaphore`（每会话 1 个许可）的 FIFO 唤醒顺序**：`acquire` 派发的先后本来就依赖任务 poll 顺序，解决不了到达顺序问题。
- **把整条消息的准入判断搬进任务（保留 PR 的门禁位置）**：不需要回合的消息会在等同会话锁期间占着全局容量（容量 4，含等待中的回合），四条被忽略的消息就能让别的会话全线丢消息；因此同步门必须在占位与发票据之前，见本轮的 `precheck_text_turn`（`src/router/ts_router.rs`、`src/router/nc_router.rs`）。
- **给票据链加 TTL/定时清扫**：需要一个后台任务或时间源，阈值清扫已足够（链条目只在会话首次出现时增长）。

## Consequences

- 所得：取锁顺序 = 事件循环里的到达顺序，与任务调度无关；`arrival_order_outranks_task_scheduling_order` 故意让后到者先 spawn，去掉票据链时 100% 反序（`["second","first"]`），加上后稳定通过。回答顺序与历史顺序因此一致。
- 代价：容量占位与票据是两处登记（调用点各一行），漏掉 `enqueue_turn_ticket` 就退回旧行为；链是每会话一条的长期状态，需要阈值清扫（1024）。
- 验证：`cargo fmt --check`、`cargo clippy --all-targets --offline --locked -- -D warnings`、`cargo test --all-targets --offline --locked`（330 用例全绿）。新增用例：`llm::context` 的到达顺序/跨会话不等/前一条已结束不等/阈值清扫四条，`router::turn` 的 `arrival_order_outranks_task_scheduling_order`，以及 ts/nc 的准入同步门用例（`precheck_*`）。

相关：[LLM 回合骨架收敛为 `router/turn.rs` 单一归属](../../implemented/architecture/2026-10-03-llm-turn-module.md) 记录了会话锁与容量门禁的归属，本轮把取锁顺序补成确定性事实。