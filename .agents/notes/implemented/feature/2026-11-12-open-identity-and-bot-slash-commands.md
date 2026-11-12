# Agent Note: PluginCommand 开放身份互认与 Bot 斜杠命令协议

Status: implemented

## Problem

TeamSpeak 服务端与客户端标准协议不区分第三方客户端实现与机器人节点，也不提供结构化的私信命令发现机制。如果机器人节点在进出频道时广播完整命令清单，会触发 TeamSpeak 单条 `plugincmd` 约 1024 字节载荷限制以及服务端 Anti-Flood 积分桶踢出机制。

此外，用户在 FlSpeak 等富客户端私信输入框键入 `/` 或点击命令补全列表时发送的斜杠格式指令（如 `/replay 10`），如果机器人仅识别 `!replay` 前缀且全部回落给 LLM 处理，会引入不必要的大模型推理时延与调用成本。

## Decision

通过 `tsclient-rs` 升级至 `0.2.7`，在 `src/adapter/headless/open_protocol.rs` 中引入两阶段 `PluginCommand` 开放协议管理模块 `OpenProtocolManager`，并在 `src/adapter/headless/event.rs` 的生命周期与事件流中接线：

1. 身份互认子协议 `tsclient:identity`：进入频道或切换频道时向当前频道（`TargetMode::CurrentChannel`）广播 `announce`；收到对端广播 `announce` 或定向探测 `query` 时，经防刷频限流后单播回执 `announce_ack`。声明客户端类型为 `"bot"`，软件名为 `"TeamSpeakClaw"`，版本号动态读取 `CARGO_PKG_VERSION`，并在存在直呼命令时上报 `"chat:slash_commands"` 能力标签（不主动宣称无协商必要的音频扩展能力）。
2. 斜杠命令拉取子协议 `tsclient:commands`：收到对端单播查询 `list_req` 时，单播回执 `list_ack` 与精简命令清单。命令严格限制为机器人原生直呼命令（从 `config.voice_replay.direct_commands` 动态去除前缀并去重衍生，默认提供 `replay` 与 `回放`），不宣称外部音乐机器人或大模型通用技能（如 `poke`、`play`、`skip`）。在未开启语音回放时命令清单为空，且能力集动态排除 `chat:slash_commands`。单条回执总数截断于 12 条，省略空 `args` 字段，确保报文远低于 1024 字节。
3. 防刷频与限流约束：
   - 过滤自身回显：丢弃 `invoker_id == self_id` 的插件命令广播。
   - 进房广播防抖：通过 `SELF_ANNOUNCE_DEBOUNCE`（2 秒）阻止短时间频繁切换频道的全频道广播。
   - 单播回执冷却：按对端 UID（无 UID 则回退至 clid 字符串）维护冷却表，对 `announce` 施加 10 秒冷却，对 `query` 施加 5 秒冷却，对 `list_req` 施加 5 秒冷却。
4. 文本路由与直达兼容：在 `src/tools/voice_replay.rs` 的 `parse_direct_command` 中增加对称前缀识别，使 `/replay` 与 `!replay`、`/回放` 与 `!回放` 享有完全相同的本地直通判定，保持零模型时延回放响应；其余由补全触发的业务斜杠命令作为标准文本进入 LLM 工具调用闭环。
5. 领域模型状态全量配合：配合 `tsclient-rs` 升级后 `ClientInfo` 暴露的暂离与静音字段（`away`、`away_message`、`input_muted`、`output_muted`、`channel_commander`），同步升级 `src/adapter/headless/event.rs` 的 `list_clients_json` 在线快照与 `src/tools/information.rs` 的 `GetClientInfo` 工具，使大模型与跨平台查询均能感知成员的暂离与设备静音状态，杜绝仅在测试固件中填入哑值的孤立绕过。

## 协议与报文规格

### 1. 身份互认报文 (`tsclient:identity`)

```json
{
  "v": 1,
  "action": "announce",
  "client_type": "bot",
  "app_name": "TeamSpeakClaw",
  "app_version": "0.0.0",
  "capabilities": ["chat:slash_commands"]
}
```

### 2. 命令发现报文 (`tsclient:commands`)

```json
{
  "v": 1,
  "action": "list_ack",
  "commands": [
    { "name": "replay", "args": "[秒数] [@昵称]", "desc": "回放最近语音片段" },
    { "name": "回放", "args": "[秒数] [@昵称]", "desc": "回放最近语音片段" }
  ]
}
```

## Alternatives considered

**在斜杠命令清单中包含音乐机器人工具或通用技能。** 音乐机器人如 TS3AudioBot 等通常作为独立实例或通过 LLM Function Calling 调度，并非 TeamSpeakClaw 自身的直达硬解析命令；通用工具（如戳一戳 `poke`）也属于大模型自主规划的技能范畴。在斜杠菜单中虚假宣称这些工具会误导客户端将其视作机器人的原生直呼命令，且破坏了斜杠命令与本地直接分发的语义一致性；因此仅暴露机器人自身支持的直呼指令。

**在身份广播中直接携带完整命令数组。** 每个支持多条命令的 Bot 在进出频道或回应广播时都会发送接近 1 KB 的载荷，容易触碰 TeamSpeak 服务端单个插件命令长度上限并消耗 Anti-Flood 积分，造成踢出风险；分层拆为第一阶段轻量互认与第二阶段私信按需拉取。

**将所有斜杠命令全部硬编码独立分发器并绕过 LLM。** 音乐点播等交互需要复杂的歌名模糊匹配、URL 解析以及跨后端协议适配，且需要结合 PermissionGate 实施 ACL 判定与友好的自然语言反馈；硬编码解析器会导致协议与业务逻辑重复，破坏 DRY 与类型安全；仅针对时间敏感的本地语音回放开放 `/replay` 直呼，其余斜杠命令统一交由模型解析并调用对应工具。

**在 TsAdapter 结构体中持久保存公开协议管理器的公共引用。** 开放协议收发完全收敛在事件回调与连接就绪内部，外部模块与上层路由器无直接调用需求；在连接建立时绑定弱引用，避免无意义的暴露与死代码警告。

## Consequences

TeamSpeakClaw 接入 FlSpeak 客户端时，在频道树成员列表中能点亮机器人标识徽标，在成员详情弹窗中展示软件名、版本号与能力 Chip。用户在私信窗口键入 `/` 时能够拉取并内联渲染对应的斜杠命令列表。

所有插件命令的交互均处于严格的频率限制与大小截断保护下，不影响现有语音桥与文本路由的吞吐与稳定性。

## Testing

- 单测覆盖 `IdentityPayload` 与 `CommandsAckPayload` 强类型序列化结构与空字段省略，验证单包体积满足防洪水约束。
- 单测覆盖 `OpenProtocolManager` 对同一 UID 施加的 `announce`（10 秒）、`query`（5 秒）、`list_req`（5 秒）独立冷却周期，以及未知动作拒绝。
- 单测覆盖 `OpenProtocolManager` 广播进房去抖与基于运行期配置动态构建的能力集与命令集字段上限。
- 单测覆盖 `parse_direct_command` 针对 `/replay`、`/回放` 以及带参变体的直通解析与非法子命令拒绝。
- 单测覆盖 `format_client_summary` 对 `ClientInfo` 完整状态字段的 JSON 序列化，以及 `format_client_status_summary` 对暂离与设备静音状态文本的格式化。
