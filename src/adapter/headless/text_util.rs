/// TeamSpeak ServerQuery 单行消息最大字节数限制。
pub const MAX_MESSAGE_BYTES: usize = 8192;

/// 按 UTF-8 字节长度分片，每片不超过 `max_bytes`。
/// 若单个字符超过 `max_bytes`，该字符独自成片（不截断字符）。
/// 优先在空白符处分片，回退到字符边界截断。
/// 注意：分片边界上的空白符被丢弃，不影响语义。
pub fn split_message(msg: &str, max_bytes: usize) -> Vec<String> {
    debug_assert!(max_bytes > 0, "max_bytes must be > 0");
    if msg.len() <= max_bytes {
        return vec![msg.to_string()];
    }

    let mut chunks = Vec::new();
    let mut start = 0;
    let len = msg.len();

    while start < len {
        let mut end = (start + max_bytes).min(len);

        if end == len {
            chunks.push(msg[start..].to_string());
            break;
        }

        // 回退到字符边界
        while !msg.is_char_boundary(end) {
            end -= 1;
        }

        // 若一个字符就超过 max_bytes，强制包含整个字符（不截断）
        if end == start {
            end = start + 1;
            while !msg.is_char_boundary(end) {
                end += 1;
            }
        }

        // 在 end 之前找最近的空白符（最多回看 256 字节）
        let lookback_start = end.saturating_sub(256).max(start);
        if lookback_start < end {
            if let Some(rel_pos) = msg[lookback_start..end]
                .rfind(|c: char| c.is_whitespace())
            {
                let ws = lookback_start + rel_pos;
                if ws > start {
                    chunks.push(msg[start..ws].to_string());
                    // 跳过完整空白符（可能多字节，如全角空格）
                    let ws_chars: Vec<char> = msg[ws..].chars().collect();
                    start = ws + ws_chars[0].len_utf8();
                    continue;
                }
            }
        }

        // 没有合适的空白符，直接按字符边界截断
        chunks.push(msg[start..end].to_string());
        start = end;
    }

    chunks
}

#[cfg(test)]
include!("../../test/text_util.rs");

