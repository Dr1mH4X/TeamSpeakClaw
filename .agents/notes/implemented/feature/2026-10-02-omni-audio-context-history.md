# Agent Note: omni 音频轮进上下文历史

Status: implemented

## Problem

`omni_model` 开启时音频轮只把 `[Audio message]` 文本占位写进上下文（`VoiceRouter::handle_omni_audio_chunk` → `save_turn`），模型在后续轮次看不到之前说过的话：多轮语音对话里「它刚才提到的那个」这类指代无从解析，而文本轮的历史是原文回放的，两条路径的上下文能力不对等。全模态模型的接口本身支持在 `messages` 里堆 `{"type":"input_audio"}` content 元素，音频上下文不是单独参数，就是某条 user 消息 content 数组的一个元素，因此限制在本地存储而不在协议。

## Decision

- `ContextTurn.user` 由 `String` 改为 `ContextUser::Text(String) | ContextUser::Audio(AudioBytes)`；音频轮以 16 kHz 单声道 WAV 字节为准，base64 是派生的协议编码，首次编码后缓存在同一个 `AudioBytes` 里供后续轮共享（`llm/context.rs`）。
- `LlmEngine::build_context_base` 按变体装配历史：文本轮 `content` 是字符串，音频轮 `content` 是只含 `input_audio` 元素的数组。wire 格式收敛到 `engine::omni_audio_part()` 一处，`VoiceRouter` 只传 WAV 字节，不再自己拼 data URL。
- 当前轮仍是 `[runtime_context 文本, input_audio]` 两元素；历史轮只回放音频，不重复当时的 `runtime_context`，与文本轮「历史只存原始用户消息」的既有约定一致。
- `LlmEngine::save_turn` 保持文本签名，新增 `save_omni_turn(source, wav, assistant)`；音频成本轮请求与上下文各持一份（一次 `clone`）。
- 两种上限都以「一轮对话」（user+assistant 成对）为单位从最早逐轮丢弃，一次只丢一轮：先按 `max_context_turns`，再由单会话 8 MiB 与全局 256 MiB 两个音频预算兜底；字节预算至少保留最新一轮，不会把会话历史清空。装配层另按 wire 体积预算裁剪一次可见历史，见 [omni 音频历史的 wire 体积预算与会话自愈](../../implemented/feature/2026-10-03-omni-audio-history-wire-budget.md)。

## Alternatives considered

- **保留 `[Audio message]` 占位（不存音频）**：内存最省，但多轮语音对话的上下文与文本路径不对等，指代类追问每次都要求用户重复；这正是本次要修的问题。
- **存 base64 字符串**：与 wire 格式同构、实现更快，但同一段音频在内存里长期驻留 1.33 倍体积；字段归属保持 WAV 字节，base64 只作派生缓存，不参与逐出与预算计数。
- **存 `SpeechChunk.pcm16_mono_16k` 原始 i16 采样**：比 WAV 少 44 字节头，但每次装配请求都要重新包 WAV，且丢掉了采样率这一自描述信息；WAV 头相对总量可忽略。
- **超预算时把音频替换为 `[Audio message]` 文本占位**：保留轮次骨架、模型仍能看到「这里说过话」，但历史里真假内容混排，模型无法区分占位与用户原话；整轮丢弃让「历史里有什么」等同「模型看到什么」。
- **超预算时只摘掉音频 content、保留该轮的 assistant 回复**：会出现有回复无提问的悬空轮次，模型看到自己自问自答，且 messages 结构上不再是成对交替。

## Consequences

- 所得：omni 多轮语音对话能引用历史音频；单会话音频常驻上限 8 MiB、全局 256 MiB，最坏常驻音频不再随会话数线性膨胀，且只在使用多模态语音时产生；丢弃整轮后历史仍是 user/assistant 交替的连续窗口，最新一轮始终可见。
- 代价：音频轮按共享指针复制，不再整份拷贝 WAV；超预算时被丢弃轮次的音频与其文本回复一并消失，模型对更早语音的指代会失败；`ContextTurn` 不再是纯字符串结构，`ContextWindow` 的测试助手需按变体构造。
- 同批修正：`voice_mute_flags` 的麦克风开关纳入 `voice_replay.enabled`——回放与 TTS 同走 bot 发送通道，此前「只开回放不开 TTS」会让 bot 处于 `client_input_muted=1`，回放听不到。
- 验证：`cargo test --all-targets --locked` 覆盖音频历史回放（`omni_history_replays_audio_turns_as_input_audio_content`）、预算内保留（`audio_turns_within_the_byte_budget_are_kept`）、从最早逐轮丢弃（`excess_audio_drops_whole_earliest_turns`）、单轮超预算仍保留最新一轮（`single_turn_over_budget_still_keeps_the_latest_turn`）以及 mute 标志组合（`voice_mute_flags_follow_voice_features`、`voice_features_include_voice_replay_flag`）；实机需用支持 `input_audio` 的模型连续说两轮并确认第二轮能引用第一轮内容。
