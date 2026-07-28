/// 按 config 中前缀列表逐个匹配并移除首个匹配的触发前缀，
/// 返回去除前缀并 trim 后的文本。无匹配时返回 None。
pub fn strip_trigger_prefix<'a>(text: &'a str, prefixes: &[String]) -> Option<&'a str> {
    for p in prefixes {
        if let Some(rest) = text.strip_prefix(p.as_str()) {
            return Some(rest.trim());
        }
    }
    None
}
