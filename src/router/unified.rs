use crate::adapter::headless::{TextMessageEvent, TextMessageTarget};
use crate::config::AppConfig;
use crate::router::resolve_ts_inbound;

#[derive(Debug, Clone)]
pub enum ReplyPolicy {
    TeamSpeak { target_mode: u8, target: u32 },
}

#[derive(Debug, Clone)]
pub struct UnifiedInboundEvent {
    pub text: String,
    pub should_trigger_llm: bool,
    pub reply_policy: ReplyPolicy,
}

impl UnifiedInboundEvent {
    pub fn from_ts(event: &TextMessageEvent, config: &AppConfig) -> Option<Self> {
        let target_mode = match event.target_mode {
            TextMessageTarget::Private => 1u8,
            TextMessageTarget::Channel => 2,
            TextMessageTarget::Server => 3,
        };
        let decision =
            resolve_ts_inbound(&event.message, target_mode, event.invoker_id, &config.bot)?;
        Some(Self {
            text: decision.text,
            should_trigger_llm: decision.should_trigger_llm,
            reply_policy: ReplyPolicy::TeamSpeak {
                target_mode: decision.reply_target_mode,
                target: decision.reply_target,
            },
        })
    }
}
