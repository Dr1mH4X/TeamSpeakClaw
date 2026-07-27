// 此文件通过 include! 编译到 adapter::headless::text_util 模块中，
// 可直接访问 split_message 等模块内函数。

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
    let without_ws: String = joined.chars().filter(|c: &char| !c.is_whitespace()).collect();
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
fn max_bytes_zero_returns_whole() {
    assert_eq!(split_message("test", 0), vec!["test"]);
}

#[test]
fn whitespace_break_preferred() {
    let r: Vec<String> = split_message("hello world foo bar", 10);
    assert!(r.iter().all(|c: &String| c.len() <= 10));
    let joined: String = r.concat();
    let without_ws: String = joined.chars().filter(|c: &char| !c.is_whitespace()).collect();
    let expected: String = "hello world foo bar".chars().filter(|c: &char| !c.is_whitespace()).collect();
    assert_eq!(without_ws, expected);
}

#[test]
fn mixed_content() {
    let msg = "hello 你好 world 🥴 test";
    let r: Vec<String> = split_message(msg, 10);
    assert!(r.iter().all(|c: &String| c.len() <= 10), "chunks: {r:?}");
    let joined: String = r.concat();
    let without_ws: String = joined.chars().filter(|c: &char| !c.is_whitespace()).collect();
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
fn all_emoji() {
    let msg = "🥴🥴🥴🥴";
    let r: Vec<String> = split_message(msg, 6);
    assert!(r.iter().all(|c: &String| c.len() <= 6));
    assert_eq!(r.join(""), msg);
}
