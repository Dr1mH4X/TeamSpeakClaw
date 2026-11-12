# TeamSpeak 无头客户端

TeamSpeak 无头客户端是两族入站适配器中的 TeamSpeak 一侧：它封装 `tsclient-rs::Client`，把连接上的事件送进路由层，把文本与音频送出连接。代码入口是 `src/adapter/headless.rs`，子模块在 `src/adapter/headless/`；gRPC 语音桥与唤醒门另有专页。

## 职责

- 封装 `tsclient-rs::Client`：建连、身份文件（`identity.json`）读写与等级升级、文本与断开事件回调、管理命令，接入参数由 `config/headless.rs` 的 `HeadlessConfig` 控制。
- 独占上行音频发送：`actor` 任务循环是连接上唯一的音频 pacer，出站音频统一经它的节奏发出。
- 独占客户端目录写入：目录更新经命令通道交给 `actor` 串行执行，其他模块不直写目录状态。
- 提供唯一的文本发送实现：`text_util::send_text_message()` 用 `split_message()` 分片后逐片发送。
- 提供 `event`（事件适配器与 `TsAdapter` 本身）、`types`（事件类型）、`speech`、`speaker_ring`、`audio_codec`（编解码与说话人音频环形缓冲）与 `open_protocol`（开放身份互认与斜杠命令发现协议）。

## 关键路径与序列

建连后由 `take_main_subscriptions()` 取事件与断开订阅，`actor` 任务循环负责收发；入站事件经 `event` 适配成路由层使用的事件，`types` 定义其字段。

进房或移动频道触发 `open_protocol::OpenProtocolManager::announce_identity()` 广播自身客户端类型（`bot`）、软件名、版本号与能力标签；对端入站插件命令在 `open_protocol::OpenProtocolManager::handle_plugin_command()` 统一进行防刷频冷却检查后单播回复 `announce_ack` 或 `list_ack`。

出站方向有两条：文本走 `send_text_message()`——先按字节长度分片，再逐片调用 `sendTextMessage`；音频经出站 FIFO 交给 `actor` 的 pacer，由它按连接节奏写出。`speaker_ring` 保存最近说话人的音频，`speech` 与 `audio_codec` 负责 OPUS 编解码与 STT/TTS 的音频封装。

## 不变量

- 分片不得超过 TS3 ServerQuery 的单行上限 `MAX_MESSAGE_BYTES = 8192`：`split_message()` 按 UTF-8 字节切分、在字符边界截断、优先在空白符处切分，单字符超限时强制成片，片边界的空白符丢弃。
- 文本发送只有一个实现：TS 文本路由、工具回复与语音桥 `send_notice` 都复用它；`send_notice` 先把目标模式归一为私聊/频道/服务器再复用，不另起发送路径。
- 插件命令防刷频限流：进房广播去抖至少 2 秒；忽略自身回显；对同一 UID 的 `announce` 在 10 秒内最多单播回复一次 `announce_ack`；对同一 UID 的 `query` 与 `list_req` 在 5 秒内最多单播回复一次；命令清单仅宣称机器人原生直呼命令（语音回放）且最多 12 条，单条 JSON 载荷严格小于 1024 字节。未开启直呼命令时排除 `chat:slash_commands` 能力宣称。
- 音频只能经出站 FIFO 与 `actor` 的 pacer 进入连接，其他模块不得直写连接的音频通道，否则 pacer 的节奏失效。
- 身份文件与等级升级由本模块独占，多处同时登录同一身份会互相挤下线。
- 断线只上报事件、不在适配器内部自行重连：退避与重试归 `adapter/lifecycle.rs`。

## 相关决策

- [PluginCommand 开放身份互认与 Bot 斜杠命令协议](../../.agents/notes/implemented/feature/2026-11-12-open-identity-and-bot-slash-commands.md)
