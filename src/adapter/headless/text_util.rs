/// 按 UTF-8 字节长度分片，每片不超过 max_bytes。
/// 优先在空白符处分片，回退到字符边界处截断（绝不断开多字节字符）。
/// 注意：空白符被丢弃在分片边界，不影响语义。
pub fn split_message(msg: &str, max_bytes: usize) -> Vec<String> {
    if msg.len() <= max_bytes || max_bytes == 0 {
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
                    start = ws + 1; // 跳过空白符
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

