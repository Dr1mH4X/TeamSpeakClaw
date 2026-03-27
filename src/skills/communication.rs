use crate::adapter::command::{cmd_poke, cmd_send_text};
use crate::skills::{ExecutionContext, Skill};
use anyhow::Result;
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

        if clid == ctx.caller_id {
            return Err(anyhow::anyhow!("不能戳自己"));
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
        "Send a message to a specific client (private), the current channel, or broadcast to the entire server."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["private", "channel", "server"],
                    "description": "The target mode for the message. Must be 'private', 'channel', or 'server'."
                },
                "msg": {
                    "type": "string",
                    "description": "The message text to send."
                },
                "clid": {
                    "type": "integer",
                    "description": "The client ID. Required ONLY if mode is 'private'."
                }
            },
            "required": ["mode", "msg"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value> {
        let msg = args["msg"].as_str().unwrap_or("");
        if msg.is_empty() {
            return Err(anyhow::anyhow!("消息内容不能为空"));
        }

        let mode = args["mode"].as_str().unwrap_or("");

        let (targetmode, target) = match mode {
            "private" => {
                let clid = args["clid"]
                    .as_u64()
                    .ok_or_else(|| anyhow::anyhow!("发送私聊(private)必须提供 clid 参数"))? as u32;

                if clid == ctx.caller_id {
                    return Err(anyhow::anyhow!("不能给自己发私信"));
                }
                (1, clid)
            },
            "channel" => (2, 0),
            "server" => (3, 0),
            _ => return Err(anyhow::anyhow!("无效的模式，mode 必须是 private, channel 或 server")),
        };

        ctx.adapter.send_raw(&cmd_send_text(targetmode, target, msg)).await?;

        Ok(json!({
            "status": "ok",
            "message": format!("Message sent successfully in {} mode", mode)
        }))
    }
}
