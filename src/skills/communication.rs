use crate::adapter::command::{cmd_poke, cmd_send_text};
use anyhow::Result;
use crate::skills::{ExecutionContext, Skill};
use async_trait::async_trait;
use serde_json::{json, Value};

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
        let clid = args["clid"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("Missing clid"))? as u32;
        let msg = args["msg"].as_str().unwrap_or("Poke!");

        ctx.adapter.send_raw(&cmd_poke(clid, msg)).await?;
        Ok(json!({"status": "ok", "message": "Poke sent"}))
    }
}

pub struct SendPrivateMsg;

#[async_trait]
impl Skill for SendPrivateMsg {
    fn name(&self) -> &'static str {
        "send_private_msg"
    }
    fn description(&self) -> &'static str {
        "Send a private chat message to a client."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "clid": { "type": "integer", "description": "The client ID to message." },
                "msg": { "type": "string", "description": "The message to send." }
            },
            "required": ["clid", "msg"]
        })
    }
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value> {
        let clid = args["clid"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("Missing clid"))? as u32;
        let msg = args["msg"].as_str().unwrap_or("");

        // targetmode=1 (私聊)
        ctx.adapter.send_raw(&cmd_send_text(1, clid, msg)).await?;
        Ok(json!({"status": "ok", "message": "Message sent"}))
    }
}
