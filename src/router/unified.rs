use crate::adapter::napcat::event::{GroupMessageEvent, PrivateMessageEvent};
use crate::adapter::napcat::types::segments_to_text;
use crate::adapter::serverquery::event::{TextMessageEvent, TextMessageTarget};
use crate::config::AppConfig;

#[derive(Debug, Clone)]
pub enum InboundSource {
    TeamSpeakText,
    NapCatPrivate,
    NapCatGroup,
    #[allow(dead_code)]
    HeadlessText,
    #[allow(dead_code)]
    HeadlessVoiceStt,
}

#[derive(Debug, Clone)]
pub enum ReplyPolicy {
    TeamSpeak {
        target_mode: u8,
        target: u32,
    },
    NapCatPrivate {
        user_id: i64,
    },
    NapCatGroup {
        group_id: i64,
        at_user_id: Option<i64>,
    },
    #[allow(dead_code)]
    Headless {
        target_mode: i32,
        target_client_id: u32,
    },
}

#[derive(Debug, Clone)]
pub struct UnifiedInboundEvent {
    pub source: InboundSource,
    pub sender_id: String,
    pub sender_name: String,
    pub text: String,
    pub should_trigger_llm: bool,
    pub should_respond: bool,
    pub reply_policy: ReplyPolicy,
    pub trace_id: String,
}

impl UnifiedInboundEvent {
    pub fn from_ts(event: &TextMessageEvent, config: &AppConfig) -> Option<Self> {
        let msg_content = event.message.trim();
        if msg_content.is_empty() {
            return None;
        }

        let is_private = event.target_mode == TextMessageTarget::Private;
        let should_trigger_llm = is_private && config.bot.respond_to_private
            || config
                .bot
                .trigger_prefixes
                .iter()
                .any(|prefix| msg_content.starts_with(prefix));

        let reply_policy = if is_private {
            ReplyPolicy::TeamSpeak {
                target_mode: 1,
                target: event.invoker_id,
            }
        } else {
            match config.bot.default_reply_mode.as_str() {
                "channel" => ReplyPolicy::TeamSpeak {
                    target_mode: 2,
                    target: 0,
                },
                "server" => ReplyPolicy::TeamSpeak {
                    target_mode: 3,
                    target: 0,
                },
                _ => ReplyPolicy::TeamSpeak {
                    target_mode: 1,
                    target: event.invoker_id,
                },
            }
        };

        Some(Self {
            source: InboundSource::TeamSpeakText,
            sender_id: event.invoker_id.to_string(),
            sender_name: event.invoker_name.clone(),
            text: msg_content.to_string(),
            should_trigger_llm,
            should_respond: should_trigger_llm,
            reply_policy,
            trace_id: format!("ts-{}-{}", event.invoker_id, event.invoker_uid),
        })
    }

    pub fn from_nc_private(msg: &PrivateMessageEvent) -> Option<Self> {
        let text = segments_to_text(&msg.message);
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        Some(Self {
            source: InboundSource::NapCatPrivate,
            sender_id: msg.user_id.to_string(),
            sender_name: msg.sender.nickname.clone(),
            text: text.to_string(),
            should_trigger_llm: true,
            should_respond: true,
            reply_policy: ReplyPolicy::NapCatPrivate {
                user_id: msg.user_id,
            },
            trace_id: format!("nc-private-{}-{}", msg.user_id, msg.timestamp),
        })
    }

    pub fn from_nc_group(msg: &GroupMessageEvent, is_triggered: bool) -> Option<Self> {
        let text = segments_to_text(&msg.message);
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        Some(Self {
            source: InboundSource::NapCatGroup,
            sender_id: msg.user_id.to_string(),
            sender_name: msg.sender.nickname.clone(),
            text: text.to_string(),
            should_trigger_llm: is_triggered,
            should_respond: is_triggered,
            reply_policy: ReplyPolicy::NapCatGroup {
                group_id: msg.group_id,
                at_user_id: Some(msg.user_id),
            },
            trace_id: format!("nc-group-{}-{}", msg.group_id, msg.timestamp),
        })
    }
}
