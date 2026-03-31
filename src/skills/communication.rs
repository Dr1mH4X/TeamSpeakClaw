use crate::adapter::command::{cmd_poke, cmd_send_text};
use crate::skills::{ExecutionContext, Platform, Skill, UnifiedExecutionContext};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::info;

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
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value> {
        let clid = args["clid"].as_u64().ok_or_else(|| {
            anyhow::anyhow!(ctx
                .error_prompts
                .missing_parameter
                .replace("{param}", "clid"))
        })? as u32;
        let msg = args["msg"].as_str().unwrap_or("Poke!");

        if clid == ctx.caller_id {
            return Err(anyhow::anyhow!(ctx.error_prompts.self_target.clone()));
        }

        ctx.adapter.send_raw(&cmd_poke(clid, msg)).await?;
        Ok(json!({"status": "ok", "message": "Poke sent"}))
    }
}

pub struct SendMessage;

#[async_trait]
impl Skill for SendMessage {
    fn name(&self) -> &'static str {
        "send_message"
    }

    fn description(&self) -> &'static str {
        "Send message via explicit routing. TS mode: private/channel/server (requires ts_route=true). NC mode: private/group (default when called from NapCat)."
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

    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value> {
        let msg = args["msg"].as_str().unwrap_or("");
        if msg.is_empty() {
            return Err(anyhow::anyhow!(ctx.error_prompts.empty_message.clone()));
        }

        let mode = args["mode"].as_str().unwrap_or("");

        let (targetmode, target) = match mode {
            "private" => {
                let clid = args["clid"].as_u64().ok_or_else(|| {
                    anyhow::anyhow!(ctx
                        .error_prompts
                        .missing_parameter
                        .replace("{param}", "clid"))
                })? as u32;

                if clid == ctx.caller_id {
                    return Err(anyhow::anyhow!(ctx.error_prompts.self_target.clone()));
                }
                (1, clid)
            }
            "channel" => (2, 0),
            "server" => (3, 0),
            _ => {
                return Err(anyhow::anyhow!(ctx
                    .error_prompts
                    .invalid_mode
                    .replace("{allowed}", "private, channel, server")))
            }
        };

        ctx.adapter
            .send_raw(&cmd_send_text(targetmode, target, msg))
            .await?;

        Ok(json!({
            "status": "ok",
            "message": format!("Message sent successfully in {} mode", mode)
        }))
    }

    async fn execute_unified(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value> {
        info!(
            "SendMessage: unified execution, platform={:?}",
            ctx.platform
        );

        let msg = args["msg"].as_str().unwrap_or("");
        if msg.is_empty() {
            return Err(anyhow::anyhow!(ctx.error_prompts.empty_message.clone()));
        }

        let mode = args["mode"].as_str().unwrap_or("");
        let ts_route = args["ts_route"].as_bool().unwrap_or(false);

        match ctx.platform {
            Platform::TeamSpeak => {
                if let Some(ref ts_adapter) = ctx.ts_adapter {
                    let ts_ctx = ExecutionContext {
                        adapter: ts_adapter.clone(),
                        clients: ctx.ts_clients.ok_or_else(|| {
                            anyhow::anyhow!("TeamSpeak clients list not available")
                        })?,
                        caller_id: ctx.caller_id,
                        caller_groups: ctx.caller_groups.clone(),
                        caller_channel_group_id: ctx.caller_channel_group_id,
                        gate: ctx.gate.clone(),
                        config: ctx.config.clone(),
                        error_prompts: ctx.error_prompts,
                    };
                    return self.execute(args.clone(), &ts_ctx).await;
                }
                Err(anyhow::anyhow!("TeamSpeak adapter not available"))
            }
            Platform::NapCat => {
                if let Some(ref nc_adapter) = ctx.nc_adapter {
                    if ts_route {
                        let ts_adapter = ctx.ts_adapter.as_ref().ok_or_else(|| {
                            anyhow::anyhow!("TeamSpeak adapter not available for ts_route=true")
                        })?;
                        // NC 请求 → TS 执行
                        let target = args["clid"].as_u64().map(|v| v as u32);

                        let (targetmode, target_id) = match mode {
                            "private" => {
                                let clid = target.ok_or_else(|| {
                                    anyhow::anyhow!(ctx
                                        .error_prompts
                                        .missing_parameter
                                        .replace("{param}", "clid"))
                                })?;
                                (1, clid)
                            }
                            "channel" => (2, 0),
                            "server" => (3, 0),
                            _ => {
                                return Err(anyhow::anyhow!(ctx
                                    .error_prompts
                                    .invalid_mode
                                    .replace("{allowed}", "private, channel, server")));
                            }
                        };

                        ts_adapter
                            .send_raw(&cmd_send_text(targetmode, target_id, msg))
                            .await?;

                        // 结果返回给 NC
                        let reply = format!("已在TS发送消息: {} -> {}", mode, msg);
                        return Ok(json!({
                            "status": "ok",
                            "message": reply,
                            "platform": "teamspeak",
                            "routed_by": "ts_route"
                        }));
                    }
                    // 默认：NC 原生发送
                    let segs = vec![crate::adapter::napcat::types::Segment::text(msg)];

                    match mode {
                        "private" => {
                            let target = args["user_id"]
                                .as_i64()
                                .or_else(|| args["clid"].as_i64())
                                .ok_or_else(|| {
                                    anyhow::anyhow!(ctx
                                        .error_prompts
                                        .missing_parameter
                                        .replace("{param}", "user_id"))
                                })?;
                            if ctx.caller_id_nc != 0 && target == ctx.caller_id_nc {
                                return Err(anyhow::anyhow!(ctx.error_prompts.self_target.clone()));
                            }
                            nc_adapter.send_private(target, &segs).await?;
                            Ok(json!({
                                "status": "ok",
                                "message": "Private message sent",
                                "platform": "napcat",
                                "routed_by": "default"
                            }))
                        }
                        "group" => {
                            let group_id = args["group_id"]
                                .as_i64()
                                .or(ctx.nc_group_id)
                                .ok_or_else(|| {
                                    anyhow::anyhow!(ctx
                                        .error_prompts
                                        .missing_parameter
                                        .replace("{param}", "group_id"))
                                })?;
                            nc_adapter.send_group(group_id, &segs).await?;
                            Ok(json!({
                                "status": "ok",
                                "message": "Group message sent",
                                "platform": "napcat",
                                "routed_by": "default"
                            }))
                        }
                        _ => Err(anyhow::anyhow!(ctx
                            .error_prompts
                            .invalid_mode
                            .replace("{allowed}", "private, group"))),
                    }
                } else {
                    Err(anyhow::anyhow!("NapCat adapter not available"))
                }
            }
        }
    }
}
