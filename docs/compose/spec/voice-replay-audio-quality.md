---
feature: voice-replay-audio-quality
status: delivered
updated: 2026-09-20
branch: fix/voice-replay-audio-quality
commits: be562bf..<head>
---

# Voice Replay Audio Quality

## Report

**What was built** — `speaker_ring` 改为双时钟：墙钟管留存/驱逐/断 run（`RUN_BREAK=200ms`），run 内按解码时长链式续写消除抖动碎裂与时间轴重叠；快照轴长仍 `min(window, ring_age, N)`，窗内播放终点越过 `now` 时轴尾扩展（P1-2 受控放宽）。出站去掉生产者每帧 sleep，actor `VoicePacer` 单一节拍；`listClients` 移出发送 `select!`，目录仅 actor 写、Snapshot merge。多人混音 soft-clip，Opus 出站 128kbps。

**Verification** — `cargo test --all-targets --locked` PASS（198）；`cargo clippy --all-targets --locked -- -D warnings` PASS；`cargo fmt` 已应用。

**Journey log**
- `MERGE_GAP=2ms` 是未入决策记录的实现常量，修复时正式登记为 `RUN_BREAK=200ms`。
- 轴尾扩展不是「P1-2 无变化」，须在 Agent Note 写明受控放宽与用户可见副作用（`!replay N` 可能略长）。
- 评审 critical：混音门 `sum.abs() > i16::MAX` 会把 `i16::MIN` 误送入 tanh；改为 i16 全范围透传。
- 评审 critical：bootstrap 快照不得在 actor 循环外写目录；一律 `DirCmd` 入队由 `select!` 应用。
- compose 工作区偏好：main 检出 + `checkout -b`，不用 linked worktree。

## [S1] Problem

`!replay` 回放听感失真、卡顿。对照 main 源码核实后归因如下：

1. **卡顿主因（时间轴）**：入环用回调墙钟 `Instant::now()` 作段起点，合并阈值 `MERGE_GAP=2ms`（`speaker_ring.rs:16`）。TS 语音包稳态 20ms 一发、抖动 ±0~10ms 常态：晚到 >2ms → 拆段并在快照里插入静音；早到 → 间隙按 0 处理，段在时间轴上重叠。碎裂是设计必然。
2. **失真主因之一（同根因）**：段重叠后 `mix_track_into` 做 `saturating_add`，多人 `ReplayFilter::All` 相加亦会削顶。
3. **卡顿次因（出站双节拍）**：`play_pcm_clip` 每帧 encode 后 `sleep(20ms)`，actor 每 20ms tick 只发一包，稳态生产速率 < 消费速率，FIFO 被抽干产生周期性空档。
4. **卡顿次因（actor 阻塞）**：`listClients` 在与 `send_tick` 同一 `select!` 里 `await`，目录刷新可卡住发送。
5. **闷糊（双重 Opus）**：出站编码未设码率，codec 5 重编码质量偏低。

`MERGE_GAP=2ms` 从未进入决策记录；整窗静音轴是 A 期 P1-2 有意行为，本轮不推翻回溯上界，仅对「播放终点越过 now」做轴尾受控放宽（见 S2.2）。

## [S2] Design

### 已裁决决策

| 轴 | 裁决 |
|---|---|
| 本轮范围 | P1 连续播放时钟+合并阈值；P2 单 pacer+listClients 挪出；P3 混合软限幅+出站 128kbps |
| 时钟框架 | 双时钟：墙钟管留存/驱逐/LRU/`active_ms`/断 run；播放时钟管 run 内样本链式续写 |
| 轴语义 | 轴长仍 `min(window, wall_ring_age, request_N)`；窗内 run 播放终点超过 `now` 时轴尾扩展到该终点；`buffered_ms` = 拼出后的实际播放时长 |
| 轴尾扩展性质 | **对 P1-2 上界语义的受控放宽**（非无变化）：触发条件 = run 播放终点越过 `now`；用户可见：`!replay N` 偶尔回放可略长于 N。usage 文档下一轮补一句 |
| 断 run 阈值 | 墙钟无包 **200ms**（取代未登记的 `MERGE_GAP=2ms`） |
| 同人 run 重叠 | debug 断言 `dest >= 已写入终点`；release 回退为相加 + soft-clip（见 S2.3） |
| 目录写者 | **actor 唯一写者**；enter/leave/快照均经命令通道；快照 merge 而非 replace（保留 started_seq 之后的本地 upsert） |
| 混合限幅 | 多人相加后 soft-clip |
| 出站码率 | Opus `Bitrate::BitsPerSecond(128_000)` |

### [S2.1] 录制：连续播放时钟（speaker_ring）

每说话人 track 维护：

- `run`：一段连续 PCM + 段起点 `start: Instant`（墙钟）
- `last_frame_at: Option<Instant>`：上一帧**入环**墙钟时刻
- 入帧 `received_at` 与 `last_frame_at` 比较：
  - 距上一帧 ≤ `RUN_BREAK_MS=200` → **同一 run**：样本直接续写，播放长度 += 本帧解码时长；**不**用 `received_at` 重定位
  - 距上一帧 > 200ms 或首帧 → **新 run**：`start = received_at`
- `MAX_SEGMENT_MS=1000` 为 **内存上限**：同 run 内样本累计达到上限时强制切段，下一段 `start = 上一段 start + 上一段 PCM 时长`（播放链式），`last_frame_at` 不因切段触发断 run
- **驱逐只删段**，不清 `last_frame_at` / run 追加状态；同 run 续帧在段被逐空后以 `received_at` 起新段（无法再链式时的退化）
- 1 字节 TS 标记：跳过，不入 run、不推进播放长度、**不重置** `last_frame_at`
- `last_active` / `active_ms`：墙钟语义不变

### [S2.2] 快照轴（snapshot_at）

```
axis_len = min(window_ms, wall_ring_age_ms, request_N * 1000)   // P1-2 回溯长度保留
axis_start = now - axis_len
axis_end_playback = max over 进入混合且 run.start 在墙钟窗内的 (start + pcm_duration)
axis_end = max(now, axis_end_playback)
sample_count = stereo_48k_sample_count(axis_end - axis_start)
buffered_ms = sample_count 对应播放时长
```

- run 放置：`dest = stereo_48k_sample_count(run.start - axis_start)`；`run.start < axis_start` 时裁剪前缀
- run 内样本连续；run 间墙钟空隙仍为静音
- **无未来播放**（所有 run 终点 ≤ `now`）时 `buffered_ms` 与旧 `axis_len` 一致
- `active_ms`、`speakers` 不变；请求 `N` 仍是墙钟回看长度

### [S2.3] 混合 soft-clip 与同 run 重叠防御（mix_track_into）

- 同 track 内维护 `write_end`：若 `dest_idx < write_end`（重叠）：
  - `debug_assert!` 失败即 panic
  - release：落入相加 + soft-clip，不崩
- 相加：`i32` 累加后 soft-clip 写回：
  - `|sum| <= i16::MAX` → 原值
  - 超出 → `i16::MAX * tanh(sum / i16::MAX)`（符号保留），禁止仅靠 `saturating_add`
- 单 run 无重叠时透传，不经过 tanh
- 测试：多人/重叠峰值超限 → 无溢出、未全零

### [S2.4] 出站码率（audio_codec）

- `new_opus_stereo_encoder`：`set_bitrate(Bitrate::BitsPerSecond(128_000))?`，FAILFAST
- `play_pcm_clip` / `process_encoded_segment` 共用

### [S2.5] 单 pacer（audio_output + actor）

**生产者**：删除 `play_pcm_clip` 每帧 `sleep(PCM_FRAME_MS)`；只 encode + `send`，靠 `ts3_audio_tx` 背压；deadline 仍自 dequeue 起算。

**消费者（actor 唯一 pacer）**：

- `next_send_at: Option<Instant>`
- `out_buf` 空 → 清空 `next_send_at`（空闲复位）
- `out_buf` 非空且 `next_send_at == None` → 置为 `now`，立即可发
- `now >= next_send_at` → 发一包，`next_send_at += 20ms`
- `now - next_send_at > 200ms` → 重定位 `next_send_at = now`（不倾泻）
- 测试锁：空闲→非空复位；落后 >200ms 重定位；稳态单调 +20ms

### [S2.6] listClients：actor 唯一写者

启动/周期 `listClients` 均只 `DirCmd::Snapshot` 入队，**仅 actor `select!` 内 `apply_dir_cmd` 写目录**（含 bootstrap）
- enter/leave 回调只 `DirCmd::Upsert` / `Remove`，**不**直接写目录
- actor `select!` 内应用命令，是目录唯一写者；voice 回调只读
- Snapshot **merge**：按 id 更新名称；`retain` 保留 snapshot 内 id 或 `entry.seq > started_seq` 的本地新事件，避免 clobber 刚 enter 的客户端
- 失败 `warn!`，不中断 actor；音频发送分支不再 `await listClients`

### [S2.7] 决策记录

`.agents/notes/`（git 忽略）Agent Note：双时钟、RUN_BREAK=200ms（登记原 2ms 为未裁决实现常量）、**轴尾扩展 = P1-2 受控放宽**、同 run 重叠 debug/release 双路径、目录 actor 单写者+merge、单 pacer、soft-clip、128kbps、放弃备选。

## [S3] Out of Scope

- 快照按语音包络裁剪（正式推翻 P1-2 回溯长度）
- Opus passthrough；MusicBot 策略；命令面/ACL/配置变更
- website/README 文案（轴尾导致 `!replay N` 可能略长于 N 的说明，下一轮文档窗口）
- STT `speech.rs` 时间轴

## Tasks

- [x] T1: speaker_ring 连续播放时钟 + RUN_BREAK=200ms + 轴尾扩展 — acceptance: ①抖动 40ms 同 run 快照无微间隙；②突发不重叠；③199ms 同 run / 201ms 断 run；④1 字节标记夹在语音中不断 run；⑤跨驱逐长 run 剩余段链式无缝；⑥无未来播放时 buffered_ms 与旧 axis 一致；⑦有未来播放时 buffered_ms 可 > N (covers: S2.1, S2.2)
- [x] T2: mix soft-clip + 同 run 重叠防御 — acceptance: 峰值超限无 i16 溢出且未全零；i16::MIN/MAX 透传；debug 下重叠可 assert (covers: S2.3; depends: T1)
- [x] T3: audio_codec 128kbps — acceptance: 构造后 bitrate 为 128000 bps (covers: S2.4)
- [x] T4: play_pcm_clip 去 sleep — acceptance: 源码无每帧 sleep；既有 deadline/取消测试仍过 (covers: S2.5)
- [x] T5: actor 单 pacer + 目录命令通道 — acceptance: ①pacer 空闲复位；②落后 >200ms 重定位；③稳态 +20ms 单调；④Snapshot merge 不丢 started_seq 后 Upsert；⑤bootstrap 亦经 DirCmd，仅 actor 写目录 (covers: S2.5, S2.6; depends: T4)
- [x] T6: 测试齐套 — acceptance: `cargo test --all-targets --locked` 全绿（198） (covers: S2.1–S2.6; depends: T1–T5)
- [x] T7: Agent Note — acceptance: `.agents/notes/implemented/` 含决策记录 (covers: S2.7)
- [x] T8: 质量门 — acceptance: fmt + clippy `-D warnings` + test 通过 (depends: T1–T7)
