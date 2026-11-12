use crate::tools::{
    required_u32, resolve_ts_client, unified_ts_adapter, Platform, Tool, UnifiedExecutionContext,
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::debug;

pub struct GetClientInfo;

pub(crate) fn format_client_status_summary(client: &tsclient_rs::ClientInfo) -> String {
    let mut status_flags = Vec::new();
    if client.away.unwrap_or(false) {
        if let Some(msg) = &client.away_message {
            status_flags.push(format!("away ({msg})"));
        } else {
            status_flags.push("away".to_string());
        }
    }
    if client.input_muted.unwrap_or(false) {
        status_flags.push("mic_muted".to_string());
    }
    if client.output_muted.unwrap_or(false) {
        status_flags.push("speaker_muted".to_string());
    }
    if client.channel_commander.unwrap_or(false) {
        status_flags.push("channel_commander".to_string());
    }
    if status_flags.is_empty() {
        "active".to_string()
    } else {
        status_flags.join(", ")
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
                "away": client.away.unwrap_or(false),
                "away_message": client.away_message,
                "input_muted": client.input_muted.unwrap_or(false),
                "output_muted": client.output_muted.unwrap_or(false),
                "channel_commander": client.channel_commander.unwrap_or(false),
            }));
        }

        let ts_adapter = unified_ts_adapter(ctx)?;
        let mut info = ts_adapter.get_client_info(clid).await?;

        if let Ok(clients) = ts_adapter.list_clients().await {
            if let Some(client) = clients.into_iter().find(|c| c.id as u32 == clid) {
                info.entry("client_away".to_string()).or_insert_with(|| {
                    if client.away.unwrap_or(false) {
                        "1".into()
                    } else {
                        "0".into()
                    }
                });
                if let Some(msg) = client.away_message {
                    info.entry("client_away_message".to_string()).or_insert(msg);
                }
                info.entry("client_input_muted".to_string())
                    .or_insert_with(|| {
                        if client.input_muted.unwrap_or(false) {
                            "1".into()
                        } else {
                            "0".into()
                        }
                    });
                info.entry("client_output_muted".to_string())
                    .or_insert_with(|| {
                        if client.output_muted.unwrap_or(false) {
                            "1".into()
                        } else {
                            "0".into()
                        }
                    });
                info.entry("client_is_channel_commander".to_string())
                    .or_insert_with(|| {
                        if client.channel_commander.unwrap_or(false) {
                            "1".into()
                        } else {
                            "0".into()
                        }
                    });
            }
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
    fn format_client_status_summary_formats_multiple_flags() {
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
            "away (eating), mic_muted, speaker_muted, channel_commander"
        );
    }
}
