/// Match and strip the first matching trigger prefix from text.
/// Returns the remainder after the prefix, trimmed. Returns None if no prefix matches.
pub fn strip_trigger_prefix<'a>(text: &'a str, prefixes: &[String]) -> Option<&'a str> {
    for p in prefixes {
        if let Some(rest) = text.strip_prefix(p.as_str()) {
            return Some(rest.trim());
        }
    }
    None
}
