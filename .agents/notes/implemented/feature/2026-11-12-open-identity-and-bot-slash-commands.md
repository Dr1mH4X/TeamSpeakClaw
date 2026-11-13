# Agent Note: PluginCommand 开放身份互认与 Bot 斜杠命令协议

Status: implemented

## Problem

TeamSpeak 服务端与客户端标准协议不区分第三方客户端实现与机器人节点，也不提供结构化的私信命令发现机制。如果机器人节点在进出频道时广播完整命令清单，会触发 TeamSpeak 单条 `plugincmd` 约 1024 字节载荷限制以及服务端 Anti-Flood 积分桶踢出机制。

此外，用户在 FlSpeak 等富客户端私信输入框键入 `/` 或点击命令补全列表时发送的斜杠格式指令（如 `/replay 10`），如果机器人仅识别 `!replay` 前缀且全部回落给 LLM 处理，会引入不必要的大模型推理时延与调用成本。

## Decision

通过 `tsclient-rs` 升级至 `0.2.7`，在 `src/adapter/headless/open_protocol.rs` 中引入两阶段 `PluginCommand` 开放协议管理模块 `OpenProtocolManager`，并在 `src/adapter/headless/event.rs` 的生命周期与事件流中接线：

1. 身份互认子协议 `tsclient:identity`：仅处理 `v = 1` 报文；进入或切换频道时向当前频道（`TargetMode::CurrentChannel`）广播 `announce`；收到对端广播 `announce` 或定向探测 `query` 时，经防刷频限流后单播回执 `announce_ack`。声明客户端类型为 `"bot"`，软件名为 `"TeamSpeakClaw"`，版本号读取 `CARGO_PKG_VERSION`，并在存在可用直呼命令时上报 `"chat:slash_commands"` 能力标签（不宣称无协商必要的音频扩展能力）。
2. 斜杠命令拉取子协议 `tsclient:commands`：仅处理 `v = 1` 的单播查询 `list_req`，单播回执 `list_ack` 与精简命令清单。命令严格限制为机器人原生直呼主命令——从 `config.voice_replay.direct_commands` 依次去除 `!` / `/` 前缀并跳过空值、超过 24 字符或序列化后达到 1024 字节的条目，选取首个有效配置项作为主命令（默认配置下为 `replay`），避免别名重复，也不宣称外部音乐机器人或大模型通用技能（如 `poke`、`play`、`skip`）。未开启语音回放或无合法直呼命令时清单为空，能力集动态排除 `chat:slash_commands`。单条回执上限 12 条，省略空 `args` 字段，并强制校验出站 JSON 载荷严格小于 1024 字节。
3. 防刷频与限流约束：
   - 过滤自身回显：丢弃 `invoker_id == self_id` 的插件命令广播。
   - 进房广播防抖与恢复：通过 `SELF_ANNOUNCE_DEBOUNCE`（2 秒）合并频繁切换频道时的全频道广播，若窗内再次切房则调度一次尾随广播（使用 `Weak<OpenProtocolManager>` 避免延长生命周期）；广播发送失败时回滚去抖时间戳，并针对未成功完成的初始广播调度最多一次短延迟重试。
   - 单播回执冷却与全局配额：按 `(peer, action)` 复合键（对端优先取非空 UID，缺失时回退至 clid 字符串）独立隔离冷却窗口，对 `announce` 施加 10 秒冷却、对 `query` 与 `list_req` 各施加 5 秒冷却，使同一用户的 `announce` 与 `list_req` 互不阻塞；每次请求清理过期冷却条目，并叠加全局出站令牌桶上限（突发 4 条、速率 2 条/秒）以保护共享 `tsclient-rs` 指令配额。先校验协议版本、载荷上限与活跃连接，再扣减令牌并记录冷却；触发全局限流时不写入对端冷却，单播发送失败时仅回滚对端冷却而保留全局令牌消耗。
4. 文本路由与直达兼容：在 `src/tools/voice_replay.rs` 的 `parse_direct_command` 中支持对称前缀识别，使 `/replay` 与 `!replay`、`/回放` 与 `!回放` 享有相同的本地直通判定，保持零模型时延回放响应；其余由补全触发的业务斜杠命令作为标准文本进入 LLM 工具调用闭环。
5. 领域模型状态与边界约束：基于 `tsclient_rs::ClientInfo` 的 `away`、`input_muted`、`output_muted`、`channel_commander` 状态字段，更新 `src/adapter/headless/event.rs` 的 `list_clients_json` 在线快照与 `src/tools/information.rs` 的 `GetClientInfo` 工具。两处均保留三态 `Option<bool>` 语义（`null` / `"unknown"` 表示未上报状态，仅全部显式为 `false` 时返回 `"active"`）；全局不读取也不输出 `away_message`，并在 `GetClientInfo` 中主动剔除原始回报里的 `client_away_message` 键。`GetClientInfo` 从 `list_clients` 快照合入状态前，必须校验 `u32` client ID 与非空 `client_unique_identifier` 同时匹配，防止 `clid` 复用窗口错配他人状态。

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
    { "name": "replay", "args": "[秒数] [@昵称]", "desc": "回放最近语音片段" }
  ]
}
```

## Alternatives considered

**暴露客户端自定义 `away_message` 文本。** 远端客户端可任意设置 `away_message`，将其写入在线快照或 `GetClientInfo` 返回值会把不可信自由文本送入 LLM 上下文或跨平台消息，带来提示词注入与成员状态文本越权暴露风险；仅暴露 `away` 布尔状态与静音状态即可满足在线感知需求，并从 `clientinfo` 原文结果中主动移除 `client_away_message`。

**在斜杠命令清单中包含别名或音乐机器人工具。** 同时下发多个别名字面量或外部音乐机器人、通用大模型技能（如 `play`、`skip`、`poke`）既浪费插件命令有限载荷空间，也会误导富客户端 UI；只宣告首个符合字节与字符限制的原生直呼主命令，其余指令保留在标准文本与工具调用链路中。

**在身份广播中直接携带完整命令数组。** 每个支持多条命令的 Bot 在进出频道或回应广播时都会发送接近 1 KB 的载荷，容易触碰 TeamSpeak 服务端单个插件命令长度上限并消耗 Anti-Flood 积分，造成踢出风险；分层拆为第一阶段轻量互认与第二阶段私信按需拉取。

**将所有斜杠命令全部硬编码独立分发器并绕过 LLM。** 音乐点播等交互需要复杂的歌名模糊匹配、URL 解析以及跨后端协议适配，且需要结合 PermissionGate 实施 ACL 判定与友好的自然语言反馈；硬编码解析器会导致协议与业务逻辑重复，破坏 DRY 与类型安全；仅针对时间敏感的本地语音回放开放 `/replay` 直呼，其余斜杠命令统一交由模型解析并调用对应工具。

**在 TsAdapter 结构体中持久保存公开协议管理器的公共引用。** 开放协议收发完全收敛在事件回调与连接就绪内部，外部模块与上层路由器无直接调用需求；在连接建立时绑定弱引用，避免无意义的暴露与死代码警告。

## Consequences

TeamSpeakClaw 接入 FlSpeak 客户端时，在频道树成员列表中能点亮机器人标识徽标，在成员详情弹窗中展示软件名、版本号与能力 Chip。用户在私信窗口键入 `/` 时能够拉取并内联渲染对应的斜杠命令列表。

所有插件命令的交互均处于严格的频率限制与大小截断保护下，不影响现有语音桥与文本路由的吞吐与稳定性。

## Testing

- 单测覆盖 `IdentityPayload` 与 `CommandsAckPayload` 强类型序列化结构、空字段省略及 `v = 1` 版本号校验，验证单包体积严格低于 1024 字节。
- 单测覆盖 `OpenProtocolManager` 按 `(peer, action)` 隔离的 `announce`（10 秒）、`query`（5 秒）、`list_req`（5 秒）冷却窗口、过期记录清理、全局突发令牌上限、受限对端冷却保护以及发送失败回滚。
- 单测覆盖进房广播去抖、窗内尾随调度与失败回滚，以及基于运行期配置动态构建能力集并跳过超长直呼命令条目。
- 单测覆盖 `parse_direct_command` 针对 `/replay`、`/回放` 以及带参变体的直通解析与非法子命令拒绝。
- 单测覆盖 `format_client_summary` 与 `format_client_status_summary` 对 `Option<bool>` 三态（`null` / `"unknown"` / `"active"`）的保留，以及 `merge_client_status_from_snapshot` 对非空 UID 匹配与 `client_away_message` 剔除的约束。
