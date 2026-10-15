# Agent Note: 语音 API 的响应体上限是传输层约束

Status: implemented

## Problem

STT 与 TTS 的响应体读取各有一个常量上限：`src/adapter/headless/speech.rs` 的 `MAX_STT_RESPONSE_BYTES = 64 KiB` 与 `MAX_TTS_AUDIO_BYTES = 8 MiB`（MiMo 的 base64 形态另由它折算）。这两个数容易被读成供应商的能力上限，从而被当成随手设的、可以删掉的任意值：在线 STT 与 TTS 模型各自有数千 token 的上下文与输出窗口，看起来与 64 KiB、8 MiB 无关。但读取路径没有别的边界，删除上限等于让响应体长度由对方决定。

## Decision

- 两个上限约束的是单次 HTTP 响应体的读取字节数，不是模型能吃多少：流式累积超过上限即报错结束，不截断、不在丢弃尾部后继续（`read_speech_response`、`ensure_payload_within_limit`）。
- 余量按本地的实际请求形状折算，与供应商窗口无关：STT 输入最长 12 s（`MAX_CHUNK_MS`），转写 JSON 远小于 64 KiB；TTS 按最多 28 字句段请求，8 MiB 相当于约 43 s 的 48 kHz 双声道 PCM，远大于一个句段。
- 上限只拦异常：正常流量碰不到；命中上限意味着对方回了与请求形状不符的体量（模型错配、无限流、代理插入的错误页），此时失败并记日志，比把不确定的字节缓进内存好。
- MiMo 的 base64 包装不另立语义：上限由音频上限折算（`MAX_TTS_BASE64_BYTES`）再加固定余量，解码后仍按音频字节数复核。
- 上限不随供应商的上下文或输出窗口调整；要改就先给出新的请求形状与折算依据。

## Alternatives considered

- **删掉上限、靠供应商窗口兜底**：窗口描述模型接受多少，描述不了对方会回多少字节；chunked 响应没有自然终点，读取循环会一直长。
- **按输入时长推导上限**（如 STT 上限 = 输入秒数乘每字节转录率）：转录长度与音频长度不成固定比例（静音、重复、verbose JSON 的时间戳），推导式只是把常量换成算式，却引入一个与请求无关的估计量。
- **超限时截断并继续**：截断的 JSON 解析失败、截断的音频是半个 WAV，两者都要在下游再判一次；直接失败把判断留在读取处。
- **只在内存分配后检查**：分配已经发生，保护不了进程；上限必须在累积过程中生效。

## Consequences

- 正常语音流量不受影响：命中上限只发生在响应体与请求形状严重不符时。
- 上限是常量、不进用户配置：它们是防御边界而不是可调参数；改动要连带 MiMo 的折算常量与相关单测（`speech_apis_reject_chunked_responses_over_their_byte_limits`、`mimo_rejects_decoded_audio_over_the_limit_at_base64_boundary`）。
- 供应商窗口变化不引起这里的改动；若将来 TTS 改成整段长文本一次请求，「最多 28 字句段」这一前提消失，两个数需要重新折算。