use crate::config::BotConfig;

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

/// TS 文本入站决策：是否触发 LLM、剥离后的文本、回复目标。
/// EventRouter 与语音桥共用，策略只写这一份。
#[derive(Debug, Clone)]
pub struct TsInboundDecision {
    pub text: String,
    pub should_trigger_llm: bool,
    pub reply_target_mode: u8,
    pub reply_target: u32,
}

/// 空文本返回 None；其余按 bot 配置计算触发与回复目标。
pub fn resolve_ts_inbound(
    raw: &str,
    target_mode: u8,
    invoker_id: u32,
    bot: &BotConfig,
) -> Option<TsInboundDecision> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    let is_private = target_mode == 1;
    let (text, should_trigger_llm) = if is_private && bot.respond_to_private {
        (raw.to_string(), true)
    } else {
        match strip_trigger_prefix(raw, &bot.trigger_prefixes) {
            Some(stripped) => (stripped.to_string(), true),
            None => (raw.to_string(), false),
        }
    };

    let (reply_target_mode, reply_target) = if is_private {
        (1u8, invoker_id)
    } else {
        let mode = crate::config::reply_target_mode(bot.default_reply_mode.as_str()) as u8;
        let target = if mode == 1 { invoker_id } else { 0 };
        (mode, target)
    };

    Some(TsInboundDecision {
        text,
        should_trigger_llm,
        reply_target_mode,
        reply_target,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BotConfig;

    fn bot() -> BotConfig {
        BotConfig {
            trigger_prefixes: vec!["!bot".to_string()],
            respond_to_private: true,
            default_reply_mode: "channel".to_string(),
            ..BotConfig::default()
        }
    }

    #[test]
    fn empty_text_returns_none() {
        assert!(resolve_ts_inbound("   ", 1, 7, &bot()).is_none());
    }

    #[test]
    fn private_with_respond_to_private_triggers_directly() {
        let d = resolve_ts_inbound("hello", 1, 7, &bot()).unwrap();
        assert!(d.should_trigger_llm);
        assert_eq!(d.text, "hello");
        assert_eq!(d.reply_target_mode, 1);
        assert_eq!(d.reply_target, 7);
    }

    #[test]
    fn channel_requires_prefix() {
        let d = resolve_ts_inbound("hello", 2, 7, &bot()).unwrap();
        assert!(!d.should_trigger_llm);

        let d = resolve_ts_inbound("!bot hello", 2, 7, &bot()).unwrap();
        assert!(d.should_trigger_llm);
        assert_eq!(d.text, "hello");
        assert_eq!(d.reply_target_mode, 2);
        assert_eq!(d.reply_target, 0);
    }

    #[test]
    fn private_without_respond_to_private_requires_prefix() {
        let mut cfg = bot();
        cfg.respond_to_private = false;
        let d = resolve_ts_inbound("hello", 1, 7, &cfg).unwrap();
        assert!(!d.should_trigger_llm);
        let d = resolve_ts_inbound("!bot hello", 1, 7, &cfg).unwrap();
        assert!(d.should_trigger_llm);
        assert_eq!(d.reply_target, 7);
    }
}
