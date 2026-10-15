# Agent Note: 会话生命周期与语音桥就绪的唯一归属

Status: implemented

## Problem

会话阶段与重试决策曾散在三个位置：`adapter.rs` 的建连失败、订阅取出失败与会话结束三处各自记账，`reconnect.rs` 持有退避状态机，`headless.rs` 的语音桥循环另行手写 attempt 计数与闩重置，重试语义因此不可测。语音桥就绪由一组 setter 分散写入 headless、`voice_router` 与 `actor`，读取方只能看到拼接出的布尔值。

## Decision

`src/adapter/lifecycle.rs` 是重连退避策略（`ReconnectState`、`RetryDecision`、`reconnect_delay_for_attempt`、`wait_for_retry`）、失败决策点（`retry_after_failure`）、重试驱动（`run_retry_loop`）与语音桥就绪（`BridgeReadiness` 经 `BridgeComponent` 的 `set_up`/`set_down` 上报，`is_ready()` 要求 Service/Stream/Actor 三者全 Up）的唯一归属地；`reconnect.rs` 只保留任务回收与限时等待工具。`adapter.rs` 的失败处理收敛到 `retry_after_failure` 单一决策点，调用方用 `FailureKind`（Connection/Subscription/Initialization/Running）声明失败来源，日志措辞、失败归类与退避等待都在决策点内完成；会话是否进入过路由运行直接读 `SessionCompletion::entered_running` 与 `ReconnectState` 的布尔字段，不引入阶段类型。`headless.rs` 的桥接重试改用 `run_retry_loop`，循环预标记会话已建立因而无界，单次尝试内建立过订阅流时经 `take_stream_established()` 重置退避。退避序列是 10/30/60/120/300 秒，尝试上限 5 次且只对未进入运行会话的启动周期生效。

## Alternatives considered

**不保留 `SessionPhase` 四态枚举（Connecting/Initializing/Running/Closed）。** 决策只读「是否进入过运行会话」，`Connecting` 只是 `ReconnectState` 的默认值，`Closed` 只服务 `entered_running()` 自身的测试语义，逐组件 ready 驱动 `Initializing -> Running` 的模型没有接入点。真需要时从历史找回，胜过同时养阶段枚举与布尔两套语义。

**耗尽日志不再经 `exhaustion_log` 常量映射。** 模板常量、`error!` 分支与测试断言各写一份、靠字符串相等维系；耗尽分支改为直接 match `FailureKind`：Subscription 静默、Connection 与 Initialization/Running 各一条日志。「订阅耗尽静默」改由行为级测试固定，不再由 `Option<&'static str>` 映射固定。

**NapCat 的两处重试循环不复用共享驱动。** 它们的有界与无界语义、耗尽日志与控制流各不相同，复用驱动会改动其可观测行为，因此留在 `adapter/napcat/ws.rs`。

**headless 的三条重复就绪测试不做保留。** 三者只覆盖组件标志的置位与全 Up 判定，现由 `lifecycle.rs` 的 `BridgeReadiness` 测试覆盖，随旧门面一并删除。

**旧的 `VoiceBridgeState` 门面不保留为薄包装。** 四个 setter 与 `is_ready()` 的转发层没有额外语义，删除后写入点统一到 `BridgeReadiness`。

## Consequences

组件侧只剩「上报 Up/Down」与「读 `is_ready()`」两种用法，`ts_router` 只在三者全 Up 时把文本交给语音桥；桥接重试的记账、重置与等待不再由 headless 拥有，行为由 `lifecycle.rs` 单测覆盖。代价是 `headless.rs` 需按 `AttemptOutcome` 报告单次尝试的结局；桥接循环预标记会话已建立后 `run_retry_loop` 不会返回 `Err`，该分支写作 `unreachable!`（release 的 `panic = "abort"` 下直接终止进程），不再给读者「这里可能耗尽」的暗示。

验证：`cargo fmt --check`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo test --all-targets --locked`（341 通过）、`cargo build --release` 全部通过；订阅取出失败耗尽时保持静默，由 `subscription_exhaustion_stays_silent` 驱动至尝试预算用尽并断言无 error 级事件固定，`connection_exhaustion_logs_the_last_error` 作正向对照（把订阅分支改成打日志，前者即失败）。`grep -rn "VoiceBridgeState\|set_service_running\|set_stream_ready\|set_actor_ready\|take_connected_since_retry" src/` 与 `grep -rn "let mut attempt" src/` 无匹配，`record_failure` 只出现在 `lifecycle.rs` 与 `adapter/napcat/ws.rs`，`wait_for_retry(` 在 `lifecycle.rs` 之外只剩 NapCat 两处调用与一处测试。