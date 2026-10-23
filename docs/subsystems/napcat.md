# NapCat OneBot 11 适配器

NapCat OneBot 11 适配器是 QQ 侧的入站通道：它作为 OneBot 11 WebSocket 客户端连接 NapCat，把上行事件解析为 `NcEvent` 广播给 `NcRouter`，并把回复编成动作调用发出。连接只在 `config.napcat.enabled` 为真时建立，会话期间 `adapter.rs` 观察它的 supervisor 退出信号来决定是否结束本轮会话。代码入口是 `src/adapter/napcat.rs`，实现在 `src/adapter/napcat/{ws,api,event,types}.rs`，连接参数结构在 `src/config/napcat.rs`。

## 职责

- 建连与重连：`NapCatAdapter::connect()` 完成握手并在断开后自行重连，异常退出经 `supervisor_status()` 上报。
- 上行事件解析：`event::parse_event()` 把 OneBot JSON 映射为 `NcEvent`（私聊、群聊、心跳），消息段定义在 `types.rs`。
- 下行动作调用：`api.rs` 构造 `NcAction`，`ws.rs` 的 `call()` 按 `echo` 把响应交还调用方，`send_private()` 与 `send_group()` 对外提供发送。
- 请求-响应匹配：`pending` 表保存 `echo` 到 `oneshot::Sender` 的映射，连接失效时整表清空。
- 事件广播：`subscribe()` 返回广播通道接收端，订阅者各自消费 `NcEvent`。
- 连接参数：`config::NapCatConfig` 持有启用开关、WebSocket 地址与访问令牌等连接所需设置。

## 关键路径与序列

1. `adapter.rs` 经 `napcat::ws::connect_if_enabled()` 按需建连，未启用时返回 `None`；`NapCatAdapter::connect()` 以有界重试循环反复调用 `try_connect()`。
2. `try_connect()` 先 `handshake()` 建 WebSocket（访问令牌同时进入查询参数与 `Authorization` 头），再拆分流、装 writer、起 `connection_loop()`，最后用 `get_login_info` 取机器人自身 QQ 号写入 `self_id`。
3. `reader_loop()` 是唯一读点：文本帧交 `handle_text_message()`，`Ping` 回 `Pong`，`Close`、读错误与重连信号都返回断开。
4. 断开后 `connection_loop()` 清掉当前代次的 writer 与待决请求，再握手并 `install_connection()` 递增连接代次；会话已建立后重试无尝试上限，退避到上限值后按该值继续。握手期间 shutdown 取消即退出，状态安装失败则置 supervisor 失败信号。
5. `handle_text_message()` 先按 `retcode` 或 `status` 字段判定动作响应并按 `echo` 唤醒等待者，其余交 `parse_event()` 解析后广播。
6. `NcRouter` 由 `subscribe()` 取事件，以 `get_self_id()`、可信用户与监听群等条件过滤后跑回合，回复经 `send_private()` 或 `send_group()` 进入 `call()`：插入 `pending`、`send_action()` 写出、等响应。
7. `shutdown()` 取消令牌、清连接并等待 `runtime_task`；`Drop` 同样只取消令牌。

## 不变量

- 待决请求只按 `echo` 匹配，连接失效时整表清空，等待者立刻拿到通道关闭错误而不是等到超时。
- 写入与断连清理都校验连接代次：代次不符的写入被拒绝，清理只作用于当前代次，旧连接不会写坏或卸掉新连接。
- 每个代次只上报一次断开信号，`reader_loop` 只响应与自身代次相同的信号，一次断开不会触发重复重连。
- 动作成功以 `status == "ok"` 且 `retcode == 0` 同时成立为准，失败响应不会当成已送达。
- 上行 JSON 解析失败或缺必要字段时归为 `Heartbeat`，非文本消息段不进入文本内容，畸形帧不会变成可回复消息。
- 只有 `config.napcat.enabled` 为真才建立连接，未配置的部署不会尝试外连 NapCat。
- 连接状态随实际 WebSocket 生命周期变化：断开后为 Reconnecting，新连接安装后为 Connected，supervisor 退出后为 Down。

## 相关决策

- [会话生命周期与语音桥就绪的唯一归属](../../.agents/notes/implemented/architecture/2026-10-07-session-lifecycle-ownership.md) — 决定重连退避与失败决策点的归属，并让 NapCat 两处重试循环留在 `ws` 内。
- [LLM 回合骨架收敛为 `router/turn.rs` 单一归属](../../.agents/notes/implemented/architecture/2026-10-03-llm-turn-module.md) — 决定 NapCat 与 TS、语音共用同一条回合骨架与自愈入口。
