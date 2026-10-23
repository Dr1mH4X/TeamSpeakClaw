# 会话生命周期与重连

会话生命周期与重连负责 TeamSpeak 连接的建立、每轮会话的启动与收尾，以及会话失败后的记账与退避等待；它是 `main.rs` 装配完成后进入的会话主循环，位于入口与路由层之间。代码入口是 `src/adapter.rs` 的 `run()`，退避策略与失败决策归 `src/adapter/lifecycle.rs`。

## 职责

- `adapter::run()` 拥有会话主循环：发起单次连接尝试、按失败类别记账、等待退避并判定继续循环或退出进程。
- `adapter/lifecycle.rs` 是重连退避策略与失败决策的唯一归属地，提供 `ReconnectState`、`RetryDecision`、`retry_after_failure` 与 `run_retry_loop`。
- 语音桥就绪 `BridgeReadiness` 的定义归此模块：组件经 `BridgeComponent` 只上报 Up/Down，Service、Stream、Actor 三者全 Up 才算就绪。
- `adapter/reconnect.rs` 提供任务回收与限时等待工具：`drain_managed_tasks`、`abort_managed_tasks`、`wait_with_timeout`、`join_or_abort`。
- 会话收尾在此完成：结束 NapCat 适配器与 headless 运行时，再断开 TeamSpeak 适配器。
- `cli.rs` 提供 `--log-level`、`--ui` 参数与启动横幅；`ui.rs` 拥有启动阶段、连接状态、回合与工具 spinner、底部状态栏，非 TTY 或 `--ui plain` 时关闭动态绘制。

## 关键路径与序列

1. `main.rs` 用 `Args::parse()` 取 `--log-level` 与 `--ui`，建立 UI 后打印启动横幅，`AppConfig::load_all()` 加载配置，建 `PermissionGate`、`ToolRegistry` 与 `LlmEngine`，随后把 `adapter::run()` 与 Ctrl-C / SIGTERM 触发的 `CancellationToken` 取消放在同一个 `tokio::select!` 里等待。
2. `adapter::run()` 先调 `load_wakeword_models()`：`[headless.wakeword]` 启用时校验并解析各 onnx，任一失败即返回 `Err`，不建连、不进入重连；解析结果交给 `run_connected_session()` 在重连之间复用。
3. 主循环每轮调 `connect_ts()`：`TsAdapter::connect()` 建连、`take_main_subscriptions()` 取事件与断开订阅；两处失败分别以 `FailureKind::Connection` 与 `FailureKind::Subscription` 上报 `ConnectOutcome::Failed`。
4. 连接成功后进入 `run_connected_session()`：`napcat::ws::connect_if_enabled()` 按 `config.napcat.enabled` 启动 NapCat（未启用为 `None`），等待期间由 `wait_for_initialization()` 同时观察 TeamSpeak 断线与根取消；随后 `headless::Runtime::start()` 启动 headless 运行时，`EventRouter::new_with_clients()` 与 `router::run_routers()` 并发运行 `EventRouter` 与 `NcRouter`。
5. 会话在 TeamSpeak 断线、NapCat supervisor 异常退出或 headless 组件失败时结束；收尾依次调 `shutdown_napcat()`、`headless_runtime.shutdown()`、`disconnect_adapter()`，返回 `SessionCompletion`。
6. `adapter::run()` 以 `SessionCompletion::entered_running` 记录本轮是否进入过路由运行，把结果归为 `FailureKind::Running` 或 `FailureKind::Initialization`，交 `retry_after_failure()`；`RetryAction::ContinueLoop` 进入下一轮，`RetryAction::Shutdown` 正常返回，尝试耗尽则带最后一次失败原因返回 `Err`。

## 不变量

- 退避序列与尝试上限只由 `adapter/lifecycle.rs` 定义，其他模块只报告单次尝试的结局，否则重试语义分散、无法单测固定。
- 尝试次数上限只对未进入过运行会话的启动周期生效；调过 `record_session_started()` 后重连无界，否则运行期断线会被误判为启动失败而退出进程。
- 所有失败都经 `retry_after_failure()` 记账、选择日志措辞并等待退避，调用方只用 `FailureKind`（Connection/Subscription/Initialization/Running）声明失败来源。
- `BridgeReadiness` 只在 Service、Stream、Actor 三者全 Up 时 `is_ready()`，组件只上报 Up/Down、读方不改写标志位，否则文本会在语音桥未就绪时误交或漏交。
- 语音指示器只在 `BridgeReadiness::is_ready()` 为真时显示已连接；配置未启用语音时保持 Disabled，任一组件转 Down 后不再显示绿色。
- 每条会话结束路径都在返回 `SessionCompletion` 前释放本轮已建立的资源（NapCat 的取消令牌、headless 运行时、TeamSpeak 适配器），否则重连会累积未释放的连接与任务。
- 连接尝试与退避等待都以根取消令牌为优先分支（`biased`），取消触发时立即结束循环，不进入下一次尝试。

## 相关决策

- [会话生命周期与语音桥就绪的唯一归属](../../.agents/notes/implemented/architecture/2026-10-07-session-lifecycle-ownership.md) — 决定退避策略、失败决策点、重试驱动与语音桥就绪都归 `adapter/lifecycle.rs`，`reconnect.rs` 只留任务回收与限时等待工具。
- [唤醒模型在连接服务器前加载](../../.agents/notes/implemented/bug-fix/2026-10-02-wakeword-preload-before-connect.md) — 决定 `adapter::run()` 在进入连接循环前加载并校验唤醒模型，失败即以错误终止启动。
