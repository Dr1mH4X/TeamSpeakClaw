# Agent Note: 「只喊唤醒词」判据的命中锚点与阈值

Status: proposed

正文行号已按当前工作树逐条核对（`src/router/voice_router.rs` 经回合注册表重构：`producing_turns`、`cancel_displaced_turns`、自由函数 `apply_barge_in` 与准入路径的 `barge_in_available`）；`## Problem` 里已被实现推翻的口径按「修复前」标注，现值见 `## 落地状态`。若后续再改这些函数，行号需重新核对。

裁决归属复核（本批「唤醒裁决的累计与拼装归唤醒门」重构后）：判据涉及的类型与常量已不在 `voice_feedback.rs` / `voice_router.rs`，`WakeUtterance`、`RecentFrame`、`WAKE_ONLY_TAIL_MS`、回补窗与 `has_command_tail` 现为 `wakeword.rs` 内的 `UtteranceState` / `RecentFrame` / `WAKE_ONLY_TAIL_MS` / `UtteranceState::has_command_tail`，并挂在 `SpeakerSession` 上；`VoiceRouter::WakeVerdict` 与 `combine_wake_verdict` 删除，改由门的 `take_utterance_verdict` 直接给出 `WakeVerdict`；`pending_wake` 表删除，命中累计进门的 per-clid 会话；收帧路径的帧记账（帧起点、帧毫秒、`note_frame` / `note_detection` 调用）移入门内 `feed`，`WakeEvent::fed_samples_before` 改为 `fed_samples`（喂完本帧后的累计采样数）。本笔记的判定数学、阈值决策与未做的 O1 测量 / O3 / O4 结论不受影响；行号需按新位置重读，测试引用改为 `wakeword.rs` 的 `feed_320_frame` 驱动用例。

## Problem

修复前 `WakeUtterance::has_command_tail`（当时的 `src/router/voice_feedback.rs:115-117`；现为 `src/router/voice_feedback.rs:184-186`，判据改成「锚点之后活跃毫秒 ≥ 阈值」）只累计**命中帧之后**的 VAD 活跃毫秒：当时的 `note_frame`（`src/router/voice_feedback.rs:104-108`；现为 `138-161`，按绝对采样位置记帧）在 `detected` 为真且 `voiced` 时累加，阈值 `WAKE_ONLY_TAIL_MS = 200`（当时的 `src/router/voice_feedback.rs:36`；现为 200，`src/router/voice_feedback.rs:45`），而当时的 `note_detection`（`src/router/voice_feedback.rs:98-101`；现为 `126-135`，带锚点并回补块内已喂帧）把计数清零，命中帧自身不计入。裁决由 `combine_wake_verdict`（`src/router/voice_router.rs:350-359`）合成 `WakeVerdict.wakeword_only`（`src/router/voice_router.rs:317-331`），`admit_audio_chunk` 在 `verdict.detected && verdict.wakeword_only` 时播确认音并 `return`（`src/router/voice_router.rs:1048-1070`）：chunk 在进 STT/LLM 之前被丢弃。

命中报告相对于命中块起点滞后，而分类器自身的滞后量至今未被测量。门按 80ms 块推理（`CHUNK_SIZE = 1280`，`src/adapter/headless/wakeword/model.rs:18`），生产路径逐帧喂入，每帧 320 采样 = 20ms @16k（`src/adapter/headless/speech.rs:801` 的断言；48000Hz 立体声降采样到 16k 单声道，`src/adapter/headless/speech.rs:149`），`acc` 跨帧携带余量（`src/adapter/headless/wakeword.rs:195-223`）。修复前的几何缺陷是：由于每帧只增加 320 采样，一块只在 `carry = 960` 的那一帧凑满，而当时的 `chunk_start = consumed.saturating_sub(carry)` 里 `consumed` 每次调用从 0 起算、恒被 saturate 到 0，`fire_offset` 恒为 `Some(0)`，同一个 clamp 不可达。于是当时的 `fire_sample = fed_samples_before + fire_offset`（当时的 `src/router/voice_router.rs:914`）等于**报告帧起点**，而命中块的真实起点在它前面 960 采样（60ms）、报告位置在它后面 320 采样（20ms）：命中块内的 ≤80ms 音频在 `detected` 置位前已经被喂过门且未计入。修复后 `fire_sample` 由 `chunk_abs_start = fed_samples_before − carry + consumed` 给出命中块绝对起点（`src/adapter/headless/wakeword.rs:209-213`），`feed_wakeword_frame` 直接把它当锚点（`src/router/voice_router.rs:927-928`）。

更大部分的滞后来自分类器：12 块检测缓冲（约 0.96s）内需要 ≥2 个原始分数越过 `DETECTION_THRESHOLD = 0.3` 的块才触发（`DETECTION_BUFFER_SIZE`、`MIN_POSITIVE_DETECTIONS`、`src/adapter/headless/wakeword/model.rs:20-22`、`24` 与 `src/adapter/headless/wakeword/model.rs:209-227`），首个正块到命中相隔 80ms 到 880ms 不等；而原始分数相对唤醒词声学末端的越阈位置由模型决定，可能落在词中段。因此记 `T_end` 为唤醒词声学末端、`T_rep` 为命中报告时刻，`L = T_rep − T_end` 的符号与大小都未测量，判据可能双向失效：`L` 明显为正时，`(T_end, T_rep]` 这段命令开头被静默丢弃，命令要活下来必须在 `T_rep` 之后**再**产生一个阈值量的活跃语音（修复前阈值 200ms，因此有效最小命令有声时长约 `L + 200ms`，再加 ≤80ms 的块内损失；现锚点已回收那 ≤80ms，阈值经决策定为 200ms，此时环境噪声余量与命令本体最短有声时长同为 `200 − 80 = 120ms`，见 `## 落地状态` 的算术结论）；`L` 为负且绝对值超过阈值时，纯唤醒词自身的剩余音频就会把计数推过阈值，`wakeword_only` 永远为假、确认音路径变成死代码。

中招形态是「紧跟唤醒词、有声部分不足约 400ms」的指令，例如唤醒词 +「停」「下一个」「下一首」「跳过」「继续」「暂停」；也包括唤醒词后停顿不足 `VAD_SILENCE_MS = 1200ms`（`src/adapter/headless/speech.rs:50`）再说一条短命令——停顿帧不活跃、不计入，报告点落在静音里，命令仍要独自凑满阈值（决策后仍为 200ms，扣掉块内最多 80ms 后命令本体约 120ms）。长指令、以及停顿超过 1200ms 后另起一条 utterance 的指令不受影响（后者落在开门窗口内，正常回合）。

与用户文档的冲突是确定的：`website/docs/usage.md:42` 与 `website/i18n/en/docusaurus-plugin-content-docs/current/usage.md:42` 承诺播报中再喊唤醒词会「打断当前回复并接着处理这一句」。播报中说「唤醒词 下一首」时，`src/router/voice_router.rs:1014-1044` 先插话停声（承诺的前半成立），紧接着 `src/router/voice_router.rs:1048-1070` 以 `wakeword_only` 丢弃 chunk（后半不成立），用户只听到「嗯哼」/「在呢」；静默场景下则是指令被吞、只听到确认音。

同一判据的注释依据也是错的：修复前的 `src/adapter/headless/wakeword.rs:66-68` 说检测滞后含「16 帧 embedding + 约 1s 平滑均值」。`calculate_average`（`src/adapter/headless/wakeword/model.rs:209-227`）只对**已越过 `DETECTION_THRESHOLD` 的块**求平均，`positive_count >= MIN_POSITIVE_DETECTIONS` 时 `avg > DETECTION_THRESHOLD` 恒真，`else` 分支是死代码；12 块缓冲只是提供约 0.96s 的回看/保持，命中即清空（`src/adapter/headless/wakeword/model.rs:182-190`），`NO_DETECTION_MS = 2000`（`src/adapter/headless/wakeword/model.rs:26`）是不应期而非平滑延迟。注释现已按这个真实几何改写（`src/adapter/headless/wakeword.rs:65-74`、`src/adapter/headless/wakeword/model.rs:19-26`），判定数学未动。

埋点退化曾导致「先测量」缺前提：修复前的 `fire_sample` 系统性偏晚 60ms（生产路径固定误差 = `carry`，帧长非 320 时误差随帧长变），且当时的 calibration debug 只有命中时刻的 `input_samples` 与块级分数，没有 utterance 起点与 VAD 时间轴，无法把命中位置换算成「utterance 内命令开头在哪」。一个现成可用的锚点：`score_history` 的 `chunk_index` 是说话人会话内的绝对块序号（`src/adapter/headless/wakeword/model.rs:162-176`、`src/adapter/headless/wakeword/calibration.rs:14-26`），生产每次 `feed` 只可能推理一块，所以 `raw_scores` 末项就是命中块，`first_chunk_index + len − 1` 即绝对命中块序号、乘 1280 即绝对采样位置；今天就能从日志还原「命中块在哪」。埋点补齐后（`detection_block_index`、`utterance_start_sample`、`anchor_active_ms`，见 `## 落地状态`），缺的只剩实测数据本身。

## Proposal

### O1 先测量：把命中锚点修成真正的绝对位置，再据数据定阈值

行为变化：只改内部计数口径与打点，暂不动阈值时用户可见行为不变；重排 anchor 后 `wakeword_only` 的判定结果会变（见 O2a），因此「口径」与「阈值」必须分成两次提交。

改动点：`WakewordGate::feed`（`src/adapter/headless/wakeword.rs:188-248`）在循环内计算命中块绝对起点 `chunk_abs_start = session.fed_samples − carry + consumed`（`carry` 在 `src/adapter/headless/wakeword.rs:198` 取到，`consumed` 为该块之前的块数 × `CHUNK_SIZE`，计算在 `209-213`），并作为新增字段 `WakeEvent::fire_sample: Option<usize>` 返回（`src/adapter/headless/wakeword.rs:76-87`）；`SpeakerSession.fed_samples`（`src/adapter/headless/wakeword.rs:160`）继续累计。`VoiceRouter::feed_wakeword_frame`（`src/router/voice_router.rs:904-972`）用 `event.fire_sample` 取代旧合成值（`src/router/voice_router.rs:927-928`），并把每帧的绝对采样位置与 `voiced` 标志交给 `WakeUtterance`。`WakeUtterance`（`src/router/voice_feedback.rs:100-187`）的 `note_detection` 增锚点参数、`note_frame` 改带绝对位置（`src/router/voice_feedback.rs:126-161`），并保存最近一个命中块跨度的帧（`recent_frames`）以便命中时回补块内已喂帧；`has_command_tail` 改读「锚点之后的 VAD 活跃毫秒」（`src/router/voice_feedback.rs:184-186`）。打点：`src/router/voice_router.rs:948-964` 的 calibration debug 增 `detection_block_index`、`utterance_start_sample`、`anchor_active_ms`（命中块序号可与 `fire_sample / 1280` 互校）；`src/router/voice_router.rs:1048-1070` 的 confirm INFO 带同一组字段（`anchor_sample`、`detection_block_index`、`utterance_start_sample`、`anchor_active_ms`）。

CPU/延迟/STT 成本：无新增推理、无新增 STT/TTS 调用；只是每帧多记几个数、debug 级日志字段变多。

回归风险：低。风险点是 `fire_sample` 语义变化使旧日志不可比，以及顺手改口径会顺带改变 `wakeword_only` 结果。

测试方案（已落地）：`src/adapter/headless/wakeword.rs` 的 `fed_samples_before_accumulates_across_frames`（`:580`）、`fire_sample_is_the_absolute_hit_block_start_for_320_sample_frames`（`:630`，含 320 采样帧下旧口径固定偏晚 960 采样的场景）与 `fire_sample_is_absolute_regardless_of_frame_length`（`:661`）断言绝对锚点；`src/router/voice_feedback.rs:497`（80ms 独占、再攒 120ms 恰好 200ms → 有命令）与 `:523`（100ms 不误判、补到恰好阈值 → 有命令）覆盖阈值边界；`src/router/voice_router.rs:1710` 与 `:1744` 覆盖新的合成裁决与同一阈值边界。

需同步文档：`src/router/voice_feedback.rs:36-44`、`src/router/voice_feedback.rs:92-99` 注释（已改）；`src/adapter/headless/wakeword.rs:65-74` 的滞后说明（已改，旧版「约 1s 平滑均值」的说法删除）；`docs/subsystems/wakeword.md` 的判据描述（已改为锚点口径与 200ms）。

测量协议（未执行）：本机 `models/` 有分类器时才可跑（onnx 不入库、CI 无模型）。用固定语料（纯唤醒词、唤醒词+「停」、唤醒词+「下一首」、唤醒词+长指令各若干遍，覆盖播报中与静默）跑 `RUST_LOG=debug`，读两条分布：纯唤醒词的「锚点后活跃毫秒」（假阳性侧）与命令的「`T_rep − T_end` + 锚点后活跃毫秒」（假阴性侧）。阈值取两条分布之间的间隔。

### O2 不依赖测量的稳健判据

**O2a 锚点移到命中块起点（把该块内剩余采样计入）**：行为变化是计数起点提前一个命中块、块内剩余采样计入；命令自身与纯唤醒词此后共用同一个量 `阈值 − 命中块内唤醒词尾音（≤80ms）`——命令只需这么多有声即判为含命令，纯唤醒词的噪声余量也正是这么多（量化见 `## 落地状态` 的算术结论）。成本为 0；风险是纯唤醒词的块内尾音抖动也计入，假阳性侧容忍度比旧口径的 200ms 低 80ms。这是与 O1 共用同一处 `fire_sample` 修复的选项，本轮已随之落地。测试与文档同步同 O1。

**O2b 把命中前整段时间纳入判定（沿用 200ms）**：唤醒词本身 400–700ms 就已超阈值，`detected` 恒判为「有命令」，确认音路径变成死代码、等于退回「纯唤醒词进 LLM」的旧行为。只有把阈值换成「纯唤醒词总活跃时长的基线」才可用（见 O2e）。未采纳。

**O2c 布尔判据（命中后出现任何 VAD 活跃帧）**：命中点可能落在唤醒词内部（原始分数在词中就越阈），命中后的若干帧仍是唤醒词尾，纯唤醒词会被判成命令；只有锚点确实落在唤醒词末端之后才安全，而那正是未知量。假阳性风险高，单用不可行。未采纳。

**O2d 用原始分数峰值块作锚点**：`src/adapter/headless/wakeword/calibration.rs:97-102` 已把峰值块当作「余量 0 时的边界估计」。现成可得、不需要新埋点，能消掉「≥2 正块累积」那段最多 880ms 的滞后；但峰值可能落在唤醒词中段，之后的唤醒词尾会被算成命令，阈值必须相应上调，仍需要一次测量来定修正量。未采纳。

**O2e 锚点无关的整句基线**：整条 utterance 的 VAD 活跃毫秒 < `K` 判独占，`K` 取纯唤醒词实测上界加余量。没有锚点滞后问题、成本为 0；但与说话速度强相关，唤醒词自身时长（约 500–800ms）与「唤醒词+短命令」重叠，分离度明显差于 O2a。未采纳。

结论：O2a 优先（与 O1 同批，免费回收几何滞后）；O2d 是「不加埋点」时的近似；O2b、O2c 单用不可行。

### O3 判据落点到 STT 之后

行为变化：命中后照常 STT，只有转写结果「去掉唤醒词后为空/过短」才播确认音并丢弃；否则走正常回合。成本是每次只喊唤醒词多一次 STT 调用（网络与计费），且确认音从「准入即播（进程内 opus 编码，见 `src/router/voice_feedback.rs:1-6`，要点在 `:5`）」推迟到 STT 往返之后（数百毫秒起）。致命问题：分类器只给概率不给文本，没有可靠的「去掉唤醒词」操作（按文件名反推短语已被 `../implemented/2026-10-02-wakeword-utterance-passthrough.md:18` 否决），等价做法只能退回按长度判，而「停」（1 字）与唤醒词转写无法用长度区分——要么把要救的短命令一起丢，要么一律当命令、等于取消该特性。另一处硬限制：omni 模式（`src/router/voice_router.rs:1152-1156` → `src/router/voice_router.rs:1179`，`src/router/voice_router.rs:1213` 以 `input_audio` 组装 messages）没有转写，O3 对 omni 完全不可用，而门是 omni/STT 共用（`docs/subsystems/wakeword.md`），必须保留一条音频层判据。回归风险集中在确认音时序与额外费用；测试需注入 STT 桩；文档需重写 `website/docs/usage.md:42` 与英文镜像 `website/i18n/en/docusaurus-plugin-content-docs/current/usage.md:42` 的确认音描述。未做。

### O4 不丢 chunk，只在转写只剩唤醒词时抑制回复

历史：`../implemented/2026-10-02-wakeword-utterance-passthrough.md:17` 否决过「丢弃触发命中的整条 utterance」，理由是破坏文档示例「唤醒词 播放周杰伦的夜曲」；`../implemented/2026-10-03-voice-feedback-phrases.md:30` 把丢弃收窄到只丢 `wakeword_only`，并把「只播确认音、chunk 继续下行」写成无损备选，同时明写它「仍承担同一代价：紧跟唤醒词、活跃语音不足 `WAKE_ONLY_TAIL_MS` 的指令会被判成独占而丢掉」。所以当时否决的是「整条丢弃」，而今天要评估的是「确认音照播 + chunk 照走 + 按转写抑制回复」。

今天是否仍成立：当时接受那个代价的前提是判据可靠，而判据被证明是退化的，那个代价正好落在用户文档承诺的路径上，所以 O4「不丢」的动机比当时强。但两条成本仍在：纯唤醒词会多一次 LLM 轮（费用、延迟、唤醒词进上下文；确认音片段先入 `AudioOutput` FIFO，回复排在其后、前后相接）；抑制回复要靠转写识别「只有唤醒词」，正是 O3 的同一个问题，omni 模式同样没有转写。实现上的风险已随回合注册表重构改变：现流程先 `play_wake_confirmation`（`src/router/voice_router.rs:1100-1135`，登记播报回合并 `mark_producing`）再 `return`；若让 chunk 继续下行，第二道闸 `cancel_displaced_turns`（定义 `src/router/voice_router.rs:379-390`，调用点 `src/router/voice_router.rs:1077-1081`）对「`barge_in_available` 为真且被顶替回合仍在产出」的条目刻意保留（`src/router/voice_router.rs:385-387`），确认音回合正属此类，所以**不会被 `begin_turn` 取消**、无需改挂真实回合的取消位。改为需要注意注册顺序：确认音必须先登记、真实回合后 `begin_turn`；反向顺序下 `play_wake_confirmation` 的 displaced 循环会无条件取消同 clid 全部旧回合（`src/router/voice_router.rs:1121-1125`），刚登记的真实回合会被它取消。另一处代价：同 clid 会同时存在确认音与真实回合两条产出条目（确认音播完由 `release_when_played` 自行释放），回声与「忙」语义因此依赖新注册表的保留规则。成本：只喊唤醒词多一次 STT+LLM+TTS；测试覆盖确认音与两条产出条目并存；文档需改写 `website/docs/usage.md:42` 与英文镜像的对应行。未做。

### O5 直接调低阈值或改为几何量

唯一有几何依据的一段是「报告帧 20ms + 命中块内已喂未计数的 ≤60ms = ≤80ms」。曾按此把 `WAKE_ONLY_TAIL_MS` 从 200 降到 120：对锚点之后出现的命令音频，120ms 与旧口径的 200ms 等价（`200 − 80 = 120`），命令自身只需约 40ms 有声；但按锚点口径的严格记法（见 `## 落地状态` 的算术结论），这个补偿对命令与纯唤醒词是同一个量，120ms 会让纯唤醒词后的 40ms 抖动也被判成含命令。决策最终定为 200ms：几何回收的 80ms 用来把余量稳定在「命令本体不足约 120ms 仍判独占」的一侧，不漏短命令的能力弱于 120ms 方案。更低的值（60ms、0、20ms）没有依据：那是拿一个几何常量去补模型滞后，而滞后随分类器与短语变化（≥2 正块累积 80–880ms，加原始分数越阈位置）。代价是假阳性侧容忍度随阈值下降（120ms 方案叠加锚点后是 40ms，纯唤醒词 + 40ms 级有声抖动就会被判成命令，多走一次 STT/LLM）；0/20ms 会让几乎每条纯唤醒词都进 LLM，确认音特性形同删除。本轮最终落地 200ms（`src/router/voice_feedback.rs:45`）；测试改为 `src/router/voice_feedback.rs:497`（80ms 独占 → 再攒 120ms 恰好 200ms → 有命令）与 `:523`（100ms 不误判、补到恰好阈值），常量几何断言在 `src/router/voice_feedback.rs:570`；注释已重写为 `src/router/voice_feedback.rs:36-44`。O5 与 O2a 的取舍：只取 O5 需靠抬高阈值补回漏判，只取 O2a 时 200ms 把余量稳定在 120ms，两者叠加即当前状态。

重定位复核（本批回合注册表重构后）：O1、O2a、O5 的改动点分别落在 `WakewordGate::feed`/`WakeEvent`（`wakeword.rs`）、`WakeUtterance` 的锚点口径（`voice_feedback.rs`）与 `WAKE_ONLY_TAIL_MS` 常量，重构只改了 `TurnRegistry` 与准入/插话的取消展开，没有触碰 `pending_wake`（`src/router/voice_router.rs:292`）的 `WakeUtterance` 计数、`combine_wake_verdict` 判定或 `feed_wakeword_frame` 的逐帧喂门，因此三者结论与改动点不受影响，只有行号下移；O1 附带的打点也仍是同一处 debug 块。O4 因 `cancel_displaced_turns` 保留产出中的被顶替回合而改变了实现风险（见 O4）。

## 落地状态

O1/O2a 的锚点修复已落地，阈值经决策定为 200ms（O5 曾落到 120ms、随后改回 200ms）；O1 约定的真实语料标定（测量协议）未执行，O2b/O2c/O2d/O2e 未采纳，O3/O4 未做。判定数学未动（`DETECTION_THRESHOLD`、`MIN_POSITIVE_DETECTIONS`、`calculate_average` 的结论保持原样），本轮只改锚点口径、块内回补、打点与阈值。以下逐条映射，只记与计划的偏差与当前引用。

**O1 — 部分落地：锚点与打点已落地，测量协议未做。** `WakeEvent::fire_sample: Option<usize>` 取代了语义退化的 `fire_offset`（`src/adapter/headless/wakeword.rs:76-87`），语义是「命中块在该 clid 时间轴上的绝对起始采样位置」，由 `fed_samples_before − carry + consumed` 推出（`src/adapter/headless/wakeword.rs:195-223`），生产 320 采样帧下旧口径固定偏晚 `CHUNK_SIZE − 320 = 960` 采样 = 60ms（`src/adapter/headless/wakeword.rs:74`）。`WakeUtterance` 改为 `note_detection(anchor_sample)`（`src/router/voice_feedback.rs:126-135`）加 `note_frame(abs_sample, voiced, frame_ms)`（`src/router/voice_feedback.rs:138-161`），保留最近一个命中块跨度（1280 采样/80ms，`WAKE_BLOCK_SAMPLES` 取自 `src/adapter/headless/wakeword` 的 `CHUNK_SIZE`）的帧记录（`src/router/voice_feedback.rs:152-160`），命中时回补报告前已喂帧；`has_command_tail()` = 锚点之后活跃毫秒 ≥ `WAKE_ONLY_TAIL_MS`（`src/router/voice_feedback.rs:184-186`）。打点齐全：`voice.wakeword`（`src/router/voice_router.rs:937-947`）与 `voice.wakeword.calibration`（`src/router/voice_router.rs:948-964`）带 `detection_block_index`；calibration 另有 `utterance_start_sample`、`anchor_active_ms`；`voice.wakeword.confirm`（`src/router/voice_router.rs:1049-1061`）带同一组字段；`WakeVerdict` 增 `anchor_sample`、`anchor_active_ms`、`utterance_start_sample` 三字段传递（`src/router/voice_router.rs:317-331`、`combine_wake_verdict` `src/router/voice_router.rs:350-359`）。**未做**：固定语料跑 `RUST_LOG=debug`、读两条分布、阈值取间隔这套测量协议没有执行，`T_rep − T_end` 仍未测量，本机 `models/` 是否有分类器未验。

**O2a — 已落地**（与 O1 同批，同一处 `fire_sample` 修复）：锚点移到命中块起点，块内剩余采样计入，旧口径那 60ms 的固定偏晚被回收。

**O2b、O2c、O2d、O2e — 未采纳**，理由见各条。

**O3 — 未做。O4 — 未做。**

**O5 — 已落地，阈值 200ms**（`src/router/voice_feedback.rs:45`；曾落到 120ms、随后按决策改回 200ms），与 O2a 叠加；两者叠加后的余量（同时也是命令本体最短有声时长）为 `200 − 80 = 120ms`，见下。

**顺带改对的注释（判定数学未动）**：`src/adapter/headless/wakeword.rs:65-74` 与 `src/adapter/headless/wakeword/model.rs:19-26` 的错误注释改成真实几何——12 块是命中即清空的回看环、需 ≥2 越阈块（`MIN_POSITIVE_DETECTIONS = 2.0`）、`NO_DETECTION_MS = 2000` 是不应期、`avg > DETECTION_THRESHOLD` 在 `positive_count >= MIN_POSITIVE_DETECTIONS` 时恒真是死分支（`src/adapter/headless/wakeword/model.rs:209-227`）。`src/router/voice_feedback.rs` 的 `WAKE_ONLY_TAIL_MS` 注释（`src/router/voice_feedback.rs:36-44`）与 `wake_only_tail_ms_matches_the_documented_geometry` 的文档注释（`src/router/voice_feedback.rs:566-568`）已按同一严格口径改准：阈值 200ms 下余量与命令本体最短有声时长同为 `200 − 80 = 120ms`，且阈值必须大于一个命中块；本笔记的算术记法以该口径为准。

### 锚点口径落地后的算术结论与阈值决策

锚点口径落地后，`has_command_tail`（`src/router/voice_feedback.rs:184-186`）累计的是命中块起点之后的 VAD 活跃毫秒，命中时并按锚点回补报告之前已喂入的块内帧（`note_detection`，`src/router/voice_feedback.rs:126-135`）。由此有一条可算的结论：纯唤醒词 utterance 会把命中块内属于唤醒词自身的音频也计入，上限是一个命中块 = `CHUNK_SIZE = 1280` 采样 @16k = 80ms（`src/adapter/headless/wakeword/model.rs:18`）。严格口径下只有这一个量，记 `m = 阈值 − 命中块内唤醒词尾音（≤80ms）`：它既是纯唤醒词的假阳性余量，也是命令本体需要攒够的有声时长。阈值已定为 200ms，故 `m = 120ms`——纯唤醒词要再有约 120ms 级有声抖动才会被判成命令、多走一次 STT/LLM，命令本体有声不足约 120ms 时仍会被判成唤醒词独占。三个候选阈值的后果（决策依据）：120ms → `m = 40ms`，最不容易漏掉同声短命令，但纯唤醒词后约 40ms 抖动就会误判；200ms → `m = 120ms`（已选）；280ms → `m = 200ms`，与旧口径（只累计报告帧之后的活跃帧）的行为完全一致，代价是放弃了锚点回收的 80ms 几何。该记法与 `src/router/voice_feedback.rs:36-44` 的常量注释、`wake_only_tail_ms_matches_the_documented_geometry` 的文档注释（`src/router/voice_feedback.rs:566-568`）与其断言（`src/router/voice_feedback.rs:570`，断言常量 200、命中块 80ms 且阈值必须大于一个命中块）一致；块内回补的用例是 `frames_inside_the_hit_block_are_backfilled_at_detection`（`src/router/voice_feedback.rs:497`）。

结论：阈值已落地为 200ms，`WAKE_ONLY_TAIL_MS`（`src/router/voice_feedback.rs:45`）与 `src/router/voice_feedback.rs:36-44` 的注释、`src/router/voice_feedback.rs:566-568` 的测试文档注释、`src/router/voice_feedback.rs:570` 的断言、`docs/subsystems/wakeword.md`（写 `WAKE_ONLY_TAIL_MS = 200ms`）以及 `src/router/voice_router.rs:1710` 与 `:1744` 的裁决用例都已同步。余量与命令本体最短有声时长是同一个量，不能分开设定（要让人耳可辨的最短命令更长，就必须接受更大的噪声余量，反之亦然），280ms 及以上的候选点等于放弃几何回收；选 200ms 是「几何回收用于抬高稳健性」的一侧。已知代价有两条：命令本体有声不足约 120ms 时仍被判成唤醒词独占（不进 STT/LLM）；阈值仍只有几何依据，分类器滞后 `T_rep − T_end` 未测量，只能按真实语料标定后复核。若后续改阈值，同步上述各处即可。

## Decision

推荐顺序（标注本轮执行情况）：

1. 兜底（不需测量即可合、随时可回退）：调 `WAKE_ONLY_TAIL_MS`（O5）——曾落到 120ms、按决策最终定为 200ms（`src/router/voice_feedback.rs:45`）；按 `## 落地状态` 的算术结论，200ms 下命令本体最短有声时长与纯唤醒词的噪声余量同为 `200 − 80 = 120ms`。
2. 第一步（与兜底二选一，同一批改动）：修 `fire_sample` 为命中块绝对起点，并把 `WakeUtterance` 的计数口径改成「锚点之后的 VAD 活跃毫秒」（O1 埋点 + O2a 口径）——**已落地**；与第 1 条同时落地即上文所说的「O5 与 O2a 叠加」，因此余量与命令本体最短有声时长同为一个 `200 − 80 = 120ms` 的量，取舍见 `## 落地状态` 的算术结论。
3. 第二步：用脚本化语料测量两条分布（纯唤醒词的锚点后活跃毫秒；命令的 `T_rep − T_end`），据此定阈值或改用 O2d，并把依据写进常量注释——**未做**，因此当前 200ms 仍只有几何依据、没有实测依据。
4. O3/O4 作为「接受一次额外 STT/LLM 调用换无损」的备选，落地前必须先解决 omni 无转写，以及确认音与真实回合的注册顺序（确认音先登记；反向顺序会被 `play_wake_confirmation` 的 displaced 循环取消，见 O4）——**未做**。

O1 需要改的确切函数与数据结构（按当前工作树）：`WakewordGate::feed`（`src/adapter/headless/wakeword.rs:188-248`）与 `SpeakerSession.fed_samples`（`src/adapter/headless/wakeword.rs:160`）；`WakeEvent`（`src/adapter/headless/wakeword.rs:76-87`）新增 `fire_sample`，同步 `fed_samples_before` 的语义注释（`src/adapter/headless/wakeword.rs:84-86`）；`VoiceRouter::feed_wakeword_frame`（`src/router/voice_router.rs:904-972`，锚点在 `:927-928`）；`WakeUtterance`（`src/router/voice_feedback.rs:100-187`，`note_detection`/`note_frame`/`has_command_tail` 与 `recent_frames` 回补窗）；`WAKE_ONLY_TAIL_MS`（`src/router/voice_feedback.rs:45`）与 `src/router/voice_feedback.rs:36-44` 注释；打点 `src/router/voice_router.rs:948-964` 与 `src/router/voice_router.rs:1049-1061`。`combine_wake_verdict`（`src/router/voice_router.rs:350-359`）与 `WakeVerdict`（`src/router/voice_router.rs:317-331`）三个新字段只做传递；`pending_wake`（`src/router/voice_router.rs:292`）仍是 `StdMutex<HashMap<u32, WakeUtterance>>`，重构后未变。

## Alternatives considered

- **维持旧计数口径（只累计报告帧之后的活跃帧）**：命中块起点到报告点固定偏晚 960 采样 = 60ms，阈值必须抬到 280ms 才与旧行为一致，等于放弃锚点回收的 80ms 几何，判据继续把几何误差与噪声余量混进同一个常量。
- **O2b 把命中前整段时间纳入判定**：唤醒词自身 400–700ms 已超阈值，`detected` 恒判为「有命令」，确认音路径变成死代码，退回纯唤醒词进 LLM 的旧行为。
- **O2c 布尔判据（命中后出现任何 VAD 活跃帧）**：命中点可能落在唤醒词内部，命中后的若干帧仍是唤醒词尾，纯唤醒词会被判成命令，假阳性风险高。
- **O2d 用原始分数峰值块作锚点**：现成可得、不需要新埋点，但峰值可能落在唤醒词中段，阈值必须相应上调且仍需一次测量定修正量；保留为「不加埋点」时的近似。
- **O2e 锚点无关的整句基线**：没有锚点滞后问题、成本为 0，但与说话速度强相关，唤醒词自身时长与「唤醒词+短命令」重叠，分离度明显差于 O2a。
- **阈值候选 120ms 与 280ms**：120ms 把纯唤醒词的假阳性余量压到 40ms，280ms 与旧口径行为一致但放弃几何回收；两者都只有几何依据，见 `## 落地状态` 的算术结论。
- **O3/O4 的无损路径（确认音照播、chunk 继续下行）**：未做，omni 模式没有转写，且确认音与真实回合的注册顺序需先解决。

## Acceptance criteria

测量（未执行）：本机 `models/` 有分类器时才可执行（onnx 不入库、CI 无模型），`RUST_LOG=debug` 抓 `voice.wakeword.calibration` 与 `voice.wakeword.confirm`，语料覆盖四类句形 × 播报中/静默；判据是命令组的锚点后活跃毫秒下界与纯唤醒词组的上界不重叠，阈值取两者之间并写进常量注释。单测：`cargo fmt --check`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo test --all-targets --locked`，覆盖绝对 `fire_sample`、块内回补、阈值边界、确认音丢弃路径不变。落地后实机验证 `website/docs/usage.md:42` 的两条承诺（插话后接着处理这一句；纯唤醒词只回确认音）。

## Risks

锚点口径改动本身就会改变 `wakeword_only` 结果，若同时下调阈值，两者叠加会把假阳性余量压到 40ms、假阳性显著上升，因此本应分开提交与验证（本轮曾同时落到 120ms，决策后把阈值调回 200ms、余量回到 120ms；取舍见 `## 落地状态` 的算术结论）。阈值是 per-分类器、per-麦克风的量，写死单次标定值会把部署差异固化为行为差异；缓解办法是优先取可论证的锚点，把残余不确定性留在阈值上并写清依据。`fire_sample` 语义变化后旧日志不可比。O3/O4 在 omni 模式下不成立。O4 不再需要为确认音换取消位，但顺序反过来（真实回合先登记、确认音后登记）会让 `play_wake_confirmation` 取消掉刚登记的真实回合（`src/router/voice_router.rs:1121-1125`）。

## Consequences

UX 变化：落地后「唤醒词 停 / 下一首」这类同声短命令不再在 STT/LLM 之前被丢；纯唤醒词的确认音覆盖窗口变为「锚点后活跃毫秒 < 阈值」；阈值 200ms 下纯唤醒词要再有约 120ms 级尾音抖动才会多走一次 STT/LLM（用户听到一次针对唤醒词的回复而非「嗯哼」）。播报中喊「唤醒词 下一首」会先停声再执行指令，与 `website/docs/usage.md:42` 一致。

误触发风险与漏判代价：判据从「固定 200ms 窗口」变成「命中块锚点起算的窗口」，假阳性侧多承了命中块内唤醒词自身的 ≤80ms（O2a），阈值保持 200ms，因此余量 = `200 − 80 = 120ms`；同一量也是命令本体最短有声时长，即命令本体不足约 120ms 有声时仍被判成唤醒词独占（选 200ms 的代价）。阈值仍缺真实语料标定——锚点只消掉了可论证的几何部分，分类器滞后 `T_rep − T_end` 未测；这是本轮落地后最值得复看的一处。

文档同步：`website/docs/usage.md:42`（中文）与 `website/i18n/en/docusaurus-plugin-content-docs/current/usage.md:42`（英文）——采用 O3/O4 时需改写确认音时序与「只喊唤醒词会走一次 LLM」（本轮未采用，两条文档未改）；`docs/subsystems/wakeword.md` 的判据描述已按锚点口径与 200ms 更新，`src/router/voice_feedback.rs:36-44`、`src/router/voice_feedback.rs:92-99` 与 `src/adapter/headless/wakeword.rs:65-74` 的注释也已改（后者原来是错的：滞后不是「约 1s 平滑均值」）。

状态变更：`.agents/notes/implemented/2026-10-03-wakeword-per-frame-gate.md:15` 与 `.agents/notes/implemented/2026-10-03-voice-feedback-phrases.md:16`、`:30` 里「200ms 判据 + 已知代价」的记述需要按落地结果改写为「阈值 200ms + 锚点口径 + 待实测的假阳性余量」；本笔记只落了 O1/O2a/O5，测量协议与 O3/O4 未做，因此仍留在 `proposed`。