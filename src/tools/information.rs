use std::collections::HashMap;

use crate::tools::{
    required_u32, resolve_ts_client, unified_ts_adapter, Platform, Tool, UnifiedExecutionContext,
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::debug;

pub struct GetClientInfo;

pub(crate) fn format_client_status_summary(client: &tsclient_rs::ClientInfo) -> String {
    let flags = [
        (client.away, "away"),
        (client.input_muted, "mic_muted"),
        (client.output_muted, "speaker_muted"),
        (client.channel_commander, "channel_commander"),
    ];
    let active: Vec<&str> = flags
        .iter()
        .filter_map(|(state, label)| (*state == Some(true)).then_some(*label))
        .collect();
    if !active.is_empty() {
        return active.join(", ");
    }
    if flags.iter().all(|(state, _)| *state == Some(false)) {
        "active".to_string()
    } else {
        "unknown".to_string()
    }
}

pub(crate) fn merge_client_status_from_snapshot(
    info: &mut HashMap<String, String>,
    clid: u32,
    clients: &[tsclient_rs::ClientInfo],
) {
    info.remove("client_away_message");

    let Some(expected_uid) = info
        .get("client_unique_identifier")
        .map(String::as_str)
        .filter(|uid| !uid.is_empty())
    else {
        return;
    };

    let Some(client) = clients
        .iter()
        .find(|c| u32::try_from(c.id).ok() == Some(clid) && c.uid == expected_uid)
    else {
        return;
    };

    let status_fields = [
        ("client_away", client.away),
        ("client_input_muted", client.input_muted),
        ("client_output_muted", client.output_muted),
        ("client_is_channel_commander", client.channel_commander),
    ];
    for (key, value) in status_fields {
        if let Some(flag) = value {
            info.entry(key.to_string())
                .or_insert_with(|| if flag { "1" } else { "0" }.to_string());
        }
    }
}

#[async_trait]
impl Tool for GetClientInfo {
    fn name(&self) -> &'static str {
        "get_client_info"
    }
    fn description(&self) -> &'static str {
        "Get detailed information about an online user by their clid, including connection time, IP address, version, and more."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "clid": { "type": "integer", "description": "The client ID of the online user to query." }
            },
            "required": ["clid"]
        })
    }

    async fn execute(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value> {
        let clid = required_u32(&args, "clid")?;

        if ctx.platform == Platform::NapCat {
            let (client, groups) = resolve_ts_client(ctx, clid).await?;
            let status_str = format_client_status_summary(&client);
            let reply = format!(
                "TS user info - nickname:{}, ID:{}, server groups:{:?}, channel ID:{}, status:{}",
                client.nickname, client.id, groups, client.channel_id, status_str
            );
            return Ok(json!({
                "status": "ok",
                "message": reply,
                "platform": "teamspeak",
                "away": client.away,
                "input_muted": client.input_muted,
                "output_muted": client.output_muted,
                "channel_commander": client.channel_commander,
            }));
        }

        let ts_adapter = unified_ts_adapter(ctx)?;
        let mut info = ts_adapter.get_client_info(clid).await?;
        info.remove("client_away_message");

        if let Ok(clients) = ts_adapter.list_clients().await {
            merge_client_status_from_snapshot(&mut info, clid, &clients);
        }

        debug!(?info, clid, "GetClientInfo raw response");

        if let Some(ts) = info.get("connection_connected_time") {
            if let Ok(ms) = ts.parse::<u64>() {
                let total_secs = ms / 1000;

                let years = total_secs / (365 * 86400);
                let rem = total_secs % (365 * 86400);
                let months = rem / (30 * 86400);
                let rem = rem % (30 * 86400);
                let days = rem / 86400;
                let rem = rem % 86400;
                let hours = rem / 3600;
                let rem = rem % 3600;
                let minutes = rem / 60;
                let seconds = rem % 60;

                let dur_str = format!(
                    "{}{}{}{}{}{}",
                    if years > 0 {
                        format!("{years} years ")
                    } else {
                        String::new()
                    },
                    if months > 0 {
                        format!("{months} months ")
                    } else {
                        String::new()
                    },
                    if days > 0 {
                        format!("{days} days ")
                    } else {
                        String::new()
                    },
                    if hours > 0 {
                        format!("{hours} hours ")
                    } else {
                        String::new()
                    },
                    if minutes > 0 {
                        format!("{minutes} minutes ")
                    } else {
                        String::new()
                    },
                    if seconds > 0
                        || (years == 0 && months == 0 && days == 0 && hours == 0 && minutes == 0)
                    {
                        format!("{seconds} seconds")
                    } else {
                        String::new()
                    },
                );
                let dur_str = dur_str.trim().to_string();

                info.insert("connection_duration".to_string(), dur_str);
                info.remove("connection_connected_time");
            }
        }

        Ok(json!({"status": "ok", "client_info": info}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_client_status_summary_formats_active_when_all_flags_clear() {
        let client = tsclient_rs::ClientInfo {
            nickname: "Bob".to_string(),
            uid: "bob_uid".to_string(),
            server_groups: vec![],
            channel_id: 1,
            r#type: 0,
            id: 8,
            away: Some(false),
            away_message: None,
            input_muted: Some(false),
            output_muted: Some(false),
            channel_commander: Some(false),
        };
        assert_eq!(format_client_status_summary(&client), "active");
    }

    #[test]
    fn format_client_status_summary_formats_multiple_flags_without_away_message() {
        let client = tsclient_rs::ClientInfo {
            nickname: "Alice".to_string(),
            uid: "alice_uid".to_string(),
            server_groups: vec!["6".to_string()],
            channel_id: 3,
            r#type: 0,
            id: 7,
            away: Some(true),
            away_message: Some("eating".to_string()),
            input_muted: Some(true),
            output_muted: Some(true),
            channel_commander: Some(true),
        };
        assert_eq!(
            format_client_status_summary(&client),
            "away, mic_muted, speaker_muted, channel_commander"
        );
    }

    #[test]
    fn format_client_status_summary_returns_unknown_when_flags_omitted() {
        let unknown_client = tsclient_rs::ClientInfo {
            nickname: "Charlie".to_string(),
            uid: "charlie_uid".to_string(),
            server_groups: vec![],
            channel_id: 1,
            r#type: 0,
            id: 9,
            away: None,
            away_message: Some("secret".to_string()),
            input_muted: Some(false),
            output_muted: None,
            channel_commander: Some(false),
        };
        assert_eq!(format_client_status_summary(&unknown_client), "unknown");
    }

    #[test]
    fn merge_client_status_requires_matching_uid_and_strips_away_message() {
        let snapshot = vec![tsclient_rs::ClientInfo {
            nickname: "Alice".to_string(),
            uid: "alice_uid".to_string(),
            server_groups: vec![],
            channel_id: 3,
            r#type: 0,
            id: 7,
            away: Some(true),
            away_message: Some("private note".to_string()),
            input_muted: Some(false),
            output_muted: None,
            channel_commander: Some(true),
        }];

        // UID 不匹配时不合并状态，但始终剔除 client_away_message
        let mut mismatched = HashMap::from([
            (
                "client_unique_identifier".to_string(),
                "other_uid".to_string(),
            ),
            ("client_away_message".to_string(), "leaked".to_string()),
        ]);
        merge_client_status_from_snapshot(&mut mismatched, 7, &snapshot);
        assert!(!mismatched.contains_key("client_away_message"));
        assert!(!mismatched.contains_key("client_away"));

        // UID 匹配时按 Some/None 精确合入，保留既有字段并省略 None/away_message
        let mut matched = HashMap::from([
            (
                "client_unique_identifier".to_string(),
                "alice_uid".to_string(),
            ),
            ("client_away".to_string(), "0".to_string()),
            ("client_away_message".to_string(), "should drop".to_string()),
        ]);
        merge_client_status_from_snapshot(&mut matched, 7, &snapshot);
        assert!(!matched.contains_key("client_away_message"));
        assert_eq!(matched.get("client_away").map(String::as_str), Some("0"));
        assert_eq!(
            matched.get("client_input_muted").map(String::as_str),
            Some("0")
        );
        assert!(!matched.contains_key("client_output_muted"));
        assert_eq!(
            matched
                .get("client_is_channel_commander")
                .map(String::as_str),
            Some("1")
        );
    }
}
