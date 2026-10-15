# Agent Note: 唤醒模型在连接服务器前加载

Status: implemented

## Problem

唤醒模型原先在 `headless::Runtime::start()` 内加载，而该函数在 TeamSpeak 连接建立、NapCat 初始化之后才被调用。模型缺失或非法时，程序已经连上服务器并加载身份文件，随后把错误当作会话初始化失败：报一次 ERROR，再按退避重试 5 轮（10s、30s…），期间反复建连、断开、读身份。缺一个模型文件要等两分钟才退出，日志也把「配置/文件问题」伪装成「会话启动失败」。

## Decision

- `adapter::run()` 在进入连接循环前调用 `load_wakeword_models(&config)`：`[headless.wakeword]` 启用时 `wakeword::load_models()` 校验并解析三个 onnx，任一缺失、为空或解析失败即返回 Err，`main` 立即以非零退出码结束，不建连、不重试。
- 加载结果经 `run_connected_session()` 传入 `HeadlessStartHandles.wakeword_models`（`Option<Arc<WakewordModels>>`），`Runtime::start()` 只消费、不再自行加载。
- 解析只做一次：模型 `Arc` 在重连之间复用，同一进程内不重复读盘与建 tract 图。
- `[headless.wakeword]` 未启用时不触碰文件系统（`None`）。

## Alternatives considered

- **仍在 `Runtime::start()` 加载，只是把整个 headless 启动提前到连接前**：headless 运行时要拿 `TsAdapter`（gRPC 服务与 actor 都依赖已连接会话），提前会撕裂生命周期。
- **只在连接前做存在性预检（`metadata`），解析仍留在 `Runtime::start()`**：缺文件能早失败，模型损坏仍要连上后才发现，且读两遍文件。
- **把错误从重连计数中豁免（识别 wakeword 错误不重试）**：仍需先建连，且要靠错误字符串分类，不如把加载点前移干净。
- **在 `AppConfig::validate()` 里检查**：配置校验保持纯函数，不引入文件系统副作用。

## Consequences

- 所得：模型问题在 1 秒内以明确路径信息终止启动；不再出现「连上又被断开」与无意义退避；重连不再重复解析模型。
- 代价：`adapter::run()` 承担一项启动期 I/O；模型加载失败时 NapCat 与 TeamSpeak 完全不启动（这正是目标行为）。运行中删除模型文件不影响已加载的会话。
- 验证：`load_wakeword_models` 在未启用时返回 `None` 的单测；`cargo fmt --check`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo test --all-targets --locked`；实机把 `target/release/models/Hey_Nexus.onnx` 移走后启动，日志在身份加载之前即报 `wakeword model not found` 并退出。
