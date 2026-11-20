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

进房或移动频道时由 `open_protocol::attach_open_protocol()` 挂载的 `tsclient_rs::open_protocol::OpenProtocolHandle` 自动向当前频道广播自身客户端类型（`bot`）、软件名、版本号与能力标签（建连首次就绪额外向全服发出单次带 `noreply: true` 的单向公告，便于跨频道客户端立即识别），并在防刷频冷却与全局令牌桶检查后单播回复 `announce_ack` 或 `list_ack`。

出站方向有两条：文本走 `send_text_message()`——先按字节长度分片，再逐片调用 `sendTextMessage`；音频经出站 FIFO 交给 `actor` 的 pacer，由它按连接节奏写出。入站方向 `actor` 从 `VoiceData` 提取 `seq` 同步喂给 `speaker_ring::SpeakerRings::push_opus_frame()` 与 gRPC `AudioFrameEvent`；`audio_output`、`speaker_ring` 与 `speech` 统一经由 `audio_codec` 的纯 Rust `rusty-opus` 封装执行 48kHz 立体声编解码。

## 不变量

- 分片不得超过 TS3 ServerQuery 的单行上限 `MAX_MESSAGE_BYTES = 8192`：`split_message()` 按 UTF-8 字节切分、在字符边界截断、优先在空白符处切分，单字符超限时强制成片，片边界的空白符丢弃。
- 文本发送只有一个实现：TS 文本路由、工具回复与语音桥 `send_notice` 都复用它；`send_notice` 先把目标模式归一为私聊/频道/服务器再复用，不另起发送路径。
- 开放协议进房广播：忽略自身回显；进房或切房广播去抖 2 秒，窗内触发切房会调度尾随广播，首次发送失败时允许单次重试并回滚去抖时间戳。
- 开放协议单播限流：仅处理 `v = 1` 报文；按 `(peer, action)` 维护独立冷却（`announce` 10 秒、`query` 与 `list_req` 各 5 秒）并清理过期记录，叠加全局出站令牌桶上限；先确认客户端存活再扣减配额，发送失败时回滚对端冷却。
- 开放协议载荷上限：命令名称上限 24 字符且不含空白、仅提取单个 ASCII 标点作为 `command_prefix`（命令名以字母、数字或中文开头），清单从 `config.voice_replay.direct_commands` 跳过裸命令、多前缀或格式不符项后选取首个有效语音回放直呼主命令并随 `list_ack` 下发，出站完整 `plugincmd` 线缆命令严格小于 1024 字节；未产出可用命令时排除 `chat:slash_commands` 能力宣称。
- 音频只能经出站 FIFO 与 `actor` 的 pacer 进入连接，其他模块不得直写连接的音频通道，否则 pacer 的节奏失效。
- 身份文件与等级升级由本模块独占，多处同时登录同一身份会互相挤下线。
- 断线只上报事件、不在适配器内部自行重连：退避与重试归 `adapter/lifecycle.rs`。
- Opus 编解码收口于 `audio_codec`：出站编码固定 48kHz 立体声 Audio 模式、128kbps CVBR，单包上限 `MAX_VOICE_PACKET_BYTES = 484`；入站由 `OpusJitterDecoder` 按 `u16` 回绕序差对 `1 <= diff <= 3` 做轻量重排缓冲，缺包优先调用 `decode_fec` 或上限 3 帧（60ms）的 `decode_plc`（严格按单帧 20ms / 960 样本/声道解码），1 字节边界包静默跳过，0 字节尾包清空待重排缓冲并重同步，大跳变（`diff > 32`）或长静默后直接重同步。

## 相关决策

- [PluginCommand 开放身份互认与 Bot 斜杠命令协议](../../.agents/notes/implemented/feature/2026-11-12-open-identity-and-bot-slash-commands.md)
