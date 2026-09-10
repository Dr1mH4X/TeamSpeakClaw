use crate::skills::{required_u32, unified_ts_adapter, Platform, Skill, UnifiedExecutionContext};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

fn required_message<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    let message = args
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Missing required parameter: {}", name))?;
    if message.trim().is_empty() {
        return Err(anyhow::anyhow!("{} cannot be empty", name));
    }
    Ok(message)
}

pub struct PokeClient;

#[async_trait]
impl Skill for PokeClient {
    fn name(&self) -> &'static str {
        "poke_client"
    }
    fn description(&self) -> &'static str {
        "Send a poke (notification) to a client."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "clid": { "type": "integer", "description": "The client ID to poke." },
                "msg": { "type": "string", "description": "The message to send." }
            },
            "required": ["clid", "msg"]
        })
    }

    async fn execute(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value> {
        let clid = required_u32(&args, "clid")?;
        let msg = required_message(&args, "msg")?;

        let ts_adapter = unified_ts_adapter(ctx)?;
        ts_adapter.poke(clid, msg).await?;

        if ctx.platform == Platform::NapCat {
            Ok(json!({
                "status": "ok",
                "message": format!("Poked user {} in TS", clid),
                "platform": "teamspeak",
                "routed_by": "unified"
            }))
        } else {
            Ok(json!({"status": "ok", "message": "Poke sent"}))
        }
    }
}

pub struct SendMessage;

#[async_trait]
impl Skill for SendMessage {
    fn name(&self) -> &'static str {
        "send_message"
    }

    fn description(&self) -> &'static str {
        "Send message via explicit routing. Supports cross-platform: ts_route (NC->TS), nc_route (TS->NC)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["private", "channel", "server", "group"],
                    "description": "Target mode. TS: private/channel/server. NapCat: private/group."
                },
                "msg": {
                    "type": "string",
                    "description": "The message text to send."
                },
                "ts_route": {
                    "type": "boolean",
                    "description": "When called from NapCat, set true to force routing to TeamSpeak."
                },
                "nc_route": {
                    "type": "boolean",
                    "description": "When called from TeamSpeak, set true to force routing to NapCat/QQ."
                },
                "clid": {
                    "type": "integer",
                    "description": "TS client ID for private mode."
                },
                "user_id": {
                    "type": "integer",
                    "description": "NapCat user ID for private mode."
                },
                "group_id": {
                    "type": "integer",
                    "description": "NapCat group ID for group mode."
                }
            },
            "required": ["mode", "msg"]
        })
    }

    async fn execute(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value> {
        let msg = required_message(&args, "msg")?;
        let mode = args["mode"].as_str().unwrap_or("");
        let ts_route = args["ts_route"].as_bool().unwrap_or(false);
        let nc_route = args["nc_route"].as_bool().unwrap_or(false);

        match ctx.platform {
            Platform::TeamSpeak => {
                if nc_route {
                    let nc_adapter = ctx.nc_adapter.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("NapCat adapter not available for nc_route=true")
                    })?;

                    let prefixed_msg = if !ctx.caller_name.is_empty() {
                        format!("ts({}): {}", ctx.caller_name, msg)
                    } else {
                        msg.to_string()
                    };
                    let segs = vec![crate::adapter::napcat::types::Segment::text(&prefixed_msg)];

                    match mode {
                        "private" => {
                            let user_id = args["user_id"].as_i64().ok_or_else(|| {
                                anyhow::anyhow!("Missing required parameter: user_id")
                            })?;
                            nc_adapter.send_private(user_id, &segs).await?;
                            Ok(json!({
                                "status": "ok",
                                "message": format!("Sent private message in QQ: {}", prefixed_msg),
                                "platform": "napcat",
                                "routed_by": "nc_route"
                            }))
                        }
                        "group" => {
                            let group_id = args["group_id"].as_i64().ok_or_else(|| {
                                anyhow::anyhow!("Missing required parameter: group_id")
                            })?;
                            nc_adapter.send_group(group_id, &segs).await?;
                            Ok(json!({
                                "status": "ok",
                                "message": format!("Sent message in QQ group: {}", prefixed_msg),
                                "platform": "napcat",
                                "routed_by": "nc_route"
                            }))
                        }
                        _ => Err(anyhow::anyhow!("Invalid mode, must be private, group")),
                    }
                } else {
                    let ts_adapter = unified_ts_adapter(ctx)?;
                    let (targetmode, target) = match mode {
                        "private" => (1, required_u32(&args, "clid")?),
                        "channel" => (2, 0),
                        "server" => (3, 0),
                        _ => {
                            return Err(anyhow::anyhow!(
                                "Invalid mode, must be private, channel, server"
                            ))
                        }
                    };
                    ts_adapter
                        .send_text_message(targetmode, target, msg)
                        .await?;
                    Ok(json!({
                        "status": "ok",
                        "message": format!("Message sent in {} mode: {}", mode, msg)
                    }))
                }
            }
            Platform::NapCat => {
                let nc_adapter = ctx
                    .nc_adapter
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("NapCat adapter not available"))?;

                if ts_route {
                    let ts_adapter = ctx.ts_adapter.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("TeamSpeak adapter not available for ts_route=true")
                    })?;
                    let (targetmode, target_id) = match mode {
                        "private" => (1, required_u32(&args, "clid")?),
                        "channel" => (2, 0),
                        "server" => (3, 0),
                        _ => {
                            return Err(anyhow::anyhow!(
                                "Invalid mode, must be private, channel, server"
                            ));
                        }
                    };

                    let prefixed_msg = if !ctx.caller_name.is_empty() {
                        format!("nc({}): {}", ctx.caller_name, msg)
                    } else {
                        msg.to_string()
                    };

                    ts_adapter
                        .send_text_message(targetmode, target_id, &prefixed_msg)
                        .await?;

                    return Ok(json!({
                        "status": "ok",
                        "message": format!("Sent message in TS: {} -> {}", mode, prefixed_msg),
                        "platform": "teamspeak",
                        "routed_by": "ts_route"
                    }));
                }

                let segs = vec![crate::adapter::napcat::types::Segment::text(msg)];
                match mode {
                    "private" => {
                        let target = args["user_id"]
                            .as_i64()
                            .or_else(|| args["clid"].as_i64())
                            .ok_or_else(|| {
                                anyhow::anyhow!("Missing required parameter: user_id")
                            })?;
                        if ctx.caller_id_nc != 0 && target == ctx.caller_id_nc {
                            return Err(anyhow::anyhow!("Cannot perform this action on yourself"));
                        }
                        nc_adapter.send_private(target, &segs).await?;
                        Ok(json!({
                            "status": "ok",
                            "message": format!("Private message sent: {}", msg),
                            "platform": "napcat",
                            "routed_by": "default"
                        }))
                    }
                    "group" => {
                        let group_id =
                            args["group_id"]
                                .as_i64()
                                .or(ctx.nc_group_id)
                                .ok_or_else(|| {
                                    anyhow::anyhow!("Missing required parameter: group_id")
                                })?;
                        nc_adapter.send_group(group_id, &segs).await?;
                        Ok(json!({
                            "status": "ok",
                            "message": format!("Group message sent: {}", msg),
                            "platform": "napcat",
                            "routed_by": "default"
                        }))
                    }
                    _ => Err(anyhow::anyhow!("Invalid mode, must be private, group")),
                }
            }
        }
    }
}
