# Agent Note: 命中唤醒词的 utterance 整体下行

Status: implemented

## Problem

唤醒门看到的音频来自收帧路径的逐帧喂入（[唤醒门逐帧喂入，命中即插话](../../implemented/feature/2026-10-03-wakeword-per-frame-gate.md)），裁决本身不切分音频。命中唤醒词时这条 utterance 原样继续下行：STT 路径把含唤醒词的文本交给 LLM，omni 路径把含唤醒词的音频交给 LLM。用户只说了唤醒词、没有说指令时，会多产生一次针对唤醒词本身的 LLM 轮次。

## Decision

- 唤醒门只返回 `GateVerdict { open, detected, probability }`，不修改音频；命中即开门并刷新 `window_secs`，命中状态随该 utterance 进入准入，音频继续走原有 STT/omni 路径。
- 不做唤醒词文本剔除：分类器只给概率不给文本，唤醒短语由使用者自选（模板默认文件名 `wakeword.onnx`），社区模型库的 onnx 也不带短语元数据，任何按文件名反推短语的匹配都不可靠。
- STT 的最小长度门维持 `preprocess_stt_text(raw, wakeword_gate)` 的语义：门开着时不再按长度丢弃短句，保证「唤醒词+指令」同声或唤醒后的短指令（如「下一首」）都能进 LLM。

## Alternatives considered

- **丢弃触发命中的整条 utterance**：确定性强，但破坏文档示例「唤醒词 播放周杰伦的夜曲」这类同声指令，用户必须说两遍。
- **按分类器文件名反推短语、STT 文本等值即丢弃**：文件名由使用者决定且常被改成 `wakeword.onnx`，短语语言与 STT 输出语言也未必一致，命中率低且失败时无提示。
- **按「检测点之后的音频长度」判定纯唤醒词**：平滑均值触发点在唤醒词尾附近，但尾音含最多 600ms VAD 静音、长短语与短指令的尾长区间重叠，会把同声短指令误丢。该判据只用于「只喊唤醒词」的确认音与丢弃，且改为命中点之后的 VAD 活跃毫秒数（`WAKE_ONLY_TAIL_MS`），见 [唤醒门逐帧喂入，命中即插话](../../implemented/feature/2026-10-03-wakeword-per-frame-gate.md) 与 [短反馈音频走预生成 PCM 缓存与播报回合](../../implemented/feature/2026-10-03-voice-feedback-phrases.md)。
- **omni 路径缓存触发 utterance，等窗口内后续音频再合并发送**：能消除纯唤醒词轮次，但给每条语音指令加一段等待与音频缓存，属独立特性，需要单独决策。

## Consequences

- 所得：唤醒判定与音频切分解耦，门内无文本假设，STT/omni 两条路径共用同一裁决；同声指令与唤醒后短指令都可用。
- 代价：只喊唤醒词（未识别为唤醒词独占时）会得到一次针对唤醒词的 LLM 回复；窗口已开，随后说指令仍正常。识别为唤醒词独占的 utterance 改播确认音，见 [短反馈音频走预生成 PCM 缓存与播报回合](../../implemented/feature/2026-10-03-voice-feedback-phrases.md)。
- 验证：`cargo test --all-targets --locked` 覆盖门的开窗、说话人隔离、逐帧携带余量与只读窗口查询（`src/adapter/headless/wakeword.rs` 的 gate 测试）。
