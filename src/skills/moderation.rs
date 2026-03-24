use crate::adapter::command::{cmd_ban, cmd_kick};
use anyhow::Result;
use crate::skills::{ExecutionContext, Skill};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct KickClient;

#[async_trait]
impl Skill for KickClient {
    fn name(&self) -> &'static str {
        "kick_client"
    }
    fn description(&self) -> &'static str {
        "Kick a client from the server."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "clid": { "type": "integer", "description": "The client ID to kick." },
                "reason": { "type": "string", "description": "Kick reason." }
            },
            "required": ["clid"]
        })
    }
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value> {
        let clid = args["clid"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("Missing clid"))? as u32;
        let reason = args["reason"].as_str().unwrap_or("Kicked by bot");

        ctx.adapter.send_raw(&cmd_kick(clid, reason)).await?;
        Ok(json!({"status": "ok", "message": "Client kicked"}))
    }
}

pub struct BanClient;

#[async_trait]
impl Skill for BanClient {
    fn name(&self) -> &'static str {
        "ban_client"
    }
    fn description(&self) -> &'static str {
        "Ban a client from the server."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "clid": { "type": "integer", "description": "The client ID to ban." },
                "time": { "type": "integer", "description": "Ban duration in seconds (0 for permanent)." },
                "reason": { "type": "string", "description": "Ban reason." }
            },
            "required": ["clid", "time"]
        })
    }
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value> {
        let clid = args["clid"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("Missing clid"))? as u32;
        let time = args["time"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("Missing time"))?;
        let reason = args["reason"].as_str().unwrap_or("Banned by bot");

        ctx.adapter.send_raw(&cmd_ban(clid, time, reason)).await?;
        Ok(json!({"status": "ok", "message": "Client banned"}))
    }
}
