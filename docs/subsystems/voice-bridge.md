# gRPC 语音桥

gRPC 语音桥是无头客户端进程内的语音服务：headless 运行时经它把音频与事件交给 `VoiceRouter`，并经它上报自己是否已经可以接管文本。代码入口是 `src/adapter/headless/voice_service.rs`，接口定义在 `proto/voice.proto`。

## 职责

- 实现 gRPC 服务端 `VoiceServiceImpl`，监听固定的回环地址 `INTERNAL_GRPC_ADDR = "127.0.0.1:50051"`。
- 生成服务代码：`build.rs` 用 `protoc-bin-vendored` 与 `tonic_build` 从 `proto/voice.proto` 生成，构建期完成，运行期只装配。
- 广播入站事件：`actor.rs` 的广播通道按控制与音频分离两类投递。
- 上报就绪：Service、Stream、Actor 三个 `BridgeComponent` 各自只上报 Up/Down，由 `BridgeReadiness` 合成 `is_ready()`。
- 提供 `send_notice` 的落地端，复用无头客户端的文本发送实现。
- 判定文本是否交给语音路由：`should_route_text_through_bridge(voice_configured, bridge_ready)`，其中 `voice_configured` 取自 `voice_features_enabled()`（STT/TTS/omni/voice replay 任一开启）。

## 关键路径与序列

headless 运行时启动后 gRPC 服务开始监听，三个组件依次上报 Up；`BridgeReadiness::is_ready()` 为真时，`ts_router::handle_message()` 直接跳过文本处理，TS 文本改由 `VoiceRouter` 接管。任一组件转为 Down（例如事件流订阅中断）即回退到文本路由。

## 不变量

- 就绪是三个组件全 Up 的合取，读方只调 `is_ready()`，不改写标志位；否则文本会在语音桥未就绪时误交或漏交。
- 文本接管的判定只有 `should_route_text_through_bridge` 一处，路由层不得自行推断语音配置或就绪状态。
- 服务只绑定回环地址，不对外暴露；改成可外部访问的地址会引入无鉴权的音频与文本入口。
- `send_notice` 复用唯一的文本发送实现，不另起一条发送路径。

## 相关决策

- [会话生命周期与语音桥就绪的唯一归属](../../.agents/notes/implemented/architecture/2026-10-07-session-lifecycle-ownership.md) — 就绪改由生命周期注册表上报事件，headless 侧散落的布尔标记被移除。