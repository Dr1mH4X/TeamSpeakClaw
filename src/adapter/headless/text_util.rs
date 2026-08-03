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
            if let Some(rel_pos) = msg[lookback_start..end].rfind(|c: char| c.is_whitespace()) {
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
mod tests {
    use super::*;
    #[test]
    fn short_message_single_chunk() {
        let result = split_message("hello", 8192);
        assert_eq!(result, vec!["hello"]);
    }

    #[test]
    fn exact_fit() {
        let r = split_message("abcde", 5);
        assert_eq!(r, vec!["abcde"]);
    }

    #[test]
    fn ascii_split_preserves_all_chars() {
        let msg = "hello world foo bar";
        let r = split_message(msg, 8);
        for (i, c) in r.iter().enumerate() {
            assert!(c.len() <= 8, "chunk {i} len {} > 8", c.len());
        }
        let joined: String = r.concat();
        let without_ws: String = joined
            .chars()
            .filter(|c: &char| !c.is_whitespace())
            .collect();
        let expected: String = msg.chars().filter(|c: &char| !c.is_whitespace()).collect();
        assert_eq!(without_ws, expected);
    }

    #[test]
    fn chinese_no_break() {
        let msg = "你好世界这是一个测试消息";
        let r = split_message(msg, 9);
        assert!(r.iter().all(|c: &String| c.len() <= 9), "chunks: {r:?}");
    }

    #[test]
    fn emoji_preserved() {
        let msg = "a🥴bc";
        let r = split_message(msg, 2);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0], "a");
        assert_eq!(r[1], "🥴");
        assert_eq!(r[2], "bc");
    }

    #[test]
    fn whitespace_break_preferred() {
        let r: Vec<String> = split_message("hello world foo bar", 10);
        assert!(r.iter().all(|c: &String| c.len() <= 10));
        let joined: String = r.concat();
        let without_ws: String = joined
            .chars()
            .filter(|c: &char| !c.is_whitespace())
            .collect();
        let expected: String = "hello world foo bar"
            .chars()
            .filter(|c: &char| !c.is_whitespace())
            .collect();
        assert_eq!(without_ws, expected);
    }

    #[test]
    fn mixed_content() {
        let msg = "hello 你好 world 🥴 test";
        let r: Vec<String> = split_message(msg, 10);
        assert!(r.iter().all(|c: &String| c.len() <= 10), "chunks: {r:?}");
        let joined: String = r.concat();
        let without_ws: String = joined
            .chars()
            .filter(|c: &char| !c.is_whitespace())
            .collect();
        let expected: String = msg.chars().filter(|c: &char| !c.is_whitespace()).collect();
        assert_eq!(without_ws, expected);
    }

    #[test]
    fn long_text_roundtrip() {
        let msg = "a".repeat(10000);
        let r: Vec<String> = split_message(&msg, 8192);
        assert!(r.iter().all(|c: &String| c.len() <= 8192));
        assert_eq!(r.join(""), msg);
    }

    #[test]
    fn fullwidth_space_not_corrupt() {
        let msg = "a\u{3000}b\u{3000}c";
        let r: Vec<String> = split_message(msg, 3);
        assert!(r.iter().all(|c: &String| !c.is_empty()));
        assert_eq!(r.concat(), msg);
    }

    #[test]
    fn all_emoji() {
        let msg = "🥴🥴🥴🥴";
        let r: Vec<String> = split_message(msg, 6);
        assert!(r.iter().all(|c: &String| c.len() <= 6));
        assert_eq!(r.join(""), msg);
    }
}
