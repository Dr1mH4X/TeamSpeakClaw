/// 匹配并从文本中剥离第一个命中的触发前缀。
/// 返回前缀之后的剩余部分（已去除首尾空白）；无前缀匹配时返回 None。
pub fn strip_trigger_prefix<'a>(text: &'a str, prefixes: &[String]) -> Option<&'a str> {
    for p in prefixes {
        if let Some(rest) = text.strip_prefix(p.as_str()) {
            return Some(rest.trim());
        }
    }
    None
}
