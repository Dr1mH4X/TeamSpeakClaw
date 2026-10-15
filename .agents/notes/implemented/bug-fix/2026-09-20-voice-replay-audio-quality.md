# Agent Note: voice_replay 音质双时钟与单 pacer
Status: implemented

## Problem

`!replay` 听感碎裂与失真：墙钟段起点 + 未登记的 `MERGE_GAP=2ms` 导致抖动即拆段/重叠；出站双节拍抽干 FIFO；`listClients` 与发送同 `select!`；多人混音 `saturating_add` 削顶；Opus 未设码率。

## Decision

- **双时钟**：墙钟管留存/驱逐/LRU/`active_ms`/断 run；播放时钟管 run 内样本链式续写（`start + PCM 时长`，不用到达时刻重定位）。
- **RUN_BREAK=200ms**：正式取代从未入决策记录的 `MERGE_GAP=2ms`（实现常量上户口）。
- **轴尾扩展 = P1-2 受控放宽**：回溯长度仍 `min(window, ring_age, N)`；窗内 run 播放终点越过 `now` 时轴尾扩展，`!replay N` 偶尔回放可略长于 N。无未来播放时与旧轴一致。
- **同 run 重叠**：`dest < write_end` 时 debug 断言；release 相加 + soft-clip（`|x|>i16::MAX` 用 tanh）。
- **目录**：actor 唯一写者（含 bootstrap：启动快照也经 `DirCmd::Snapshot` 入队，仅 `select!` 应用）；enter/leave/快照经 `DirCmd`；Snapshot merge（`entry.seq > started_seq` 的本地事件保留）。**enqueue**：`Full` 仍 warn；`Closed`（shutdown/actor 已退出）静默——避免关闭竞态误报。
- **单 pacer**：生产者不 sleep，靠 `ts3_audio_tx` 背压；actor `VoicePacer` 空闲复位、稳态 +20ms、落后 >200ms 重定位。
- **出站码率**：`Bitrate::BitsPerSecond(128_000)`。

## Alternatives considered

- **严格轴止于 now**：不放宽 P1-2，但突发/漂移会裁掉刚录到的语音。
- **包络裁剪快照**：听感最短，正式推翻 P1-2，本轮不做。
- **Opus passthrough**：单说话人比特级无损，要改 PCM 存储格式，future。
- **listClients spawn 直接 replace 目录**：与 enter/leave 并发会 clobber，故改 actor 单写者 + merge。
- **生产者 sleep + 消费者 tick 双节拍**：稳态生产 < 消费，FIFO 抽干。

## Consequences

- 所得：run 内连续无微间隙；突发不重叠；发送节奏单一权威；目录刷新不堵发送；多人混合不硬削波；重编码质量可预期。
- 代价：`buffered_ms`/`!replay N` 在轴尾触发时可能略大于 N；`MAX_SEGMENT_MS` 强制切段依赖播放链式；usage 文档需补轴尾说明（下一轮文档窗口）。
- 测试：speaker_ring 抖动/突发/199·201ms/标记/驱逐链式/soft-clip/轴尾；actor pacer 三态 + snapshot merge；codec 码率。
