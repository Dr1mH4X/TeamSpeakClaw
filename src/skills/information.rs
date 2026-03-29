use crate::adapter::command::cmd_clientinfo;
use crate::skills::{ExecutionContext, Skill};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct GetClientList;

#[async_trait]
impl Skill for GetClientList {
    fn name(&self) -> &'static str {
        "get_client_list"
    }
    fn description(&self) -> &'static str {
        "Get the list of online clients."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }
    async fn execute(&self, _args: Value, ctx: &ExecutionContext) -> Result<Value> {
        let clients: Vec<_> = ctx.clients.iter().map(|r| r.value().clone()).collect();
        let json_clients: Vec<_> = clients
            .iter()
            .map(|c| {
                json!({
                    "clid": c.clid,
                    "nickname": c.nickname,
                    "dbid": c.cldbid,
                    "groups": c.server_groups
                })
            })
            .collect();

        Ok(json!({"status": "ok", "clients": json_clients}))
    }
}

pub struct GetClientInfo;

#[async_trait]
impl Skill for GetClientInfo {
    fn name(&self) -> &'static str {
        "get_client_info"
    }
    fn description(&self) -> &'static str {
        "Get detailed information about a specific online client by their client ID."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "clid": { "type": "integer", "description": "The client ID to query." }
            },
            "required": ["clid"]
        })
    }
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value> {
        let clid = args["clid"].as_u64().ok_or_else(|| {
            anyhow::anyhow!(ctx
                .error_prompts
                .missing_parameter
                .replace("{param}", "clid"))
        })? as u32;

        // 确认目标客户端在线
        if !ctx.clients.contains_key(&clid) {
            let msg = ctx
                .error_prompts
                .client_offline
                .replace("{clid}", &clid.to_string());
            return Ok(json!({"status": "error", "message": msg}));
        }

        let response = ctx.adapter.send_query(&cmd_clientinfo(clid)).await?;

        // 解析 key=value 响应
        let info = parse_clientinfo_response(&response);
        Ok(json!({"status": "ok", "client_info": info}))
    }
}

fn ts_unescape(s: &str) -> String {
    s.replace("\\s", " ")
        .replace("\\p", "|")
        .replace("\\n", "\n")
        .replace("\\\\", "\\")
        .replace("\\/", "/")
}

/// 解析 clientinfo 响应行，提取关键字段。
fn parse_clientinfo_response(response: &str) -> Value {
    // clientinfo 返回单行 key=value 数据（空格分隔）
    let data_line = response
        .lines()
        .find(|l| !l.starts_with("error id=") && !l.is_empty())
        .unwrap_or("");

    let kv = |key: &str| -> Option<String> {
        // 需要处理 key=value 中 value 可能含空格（TS 转义为 \s）的情况
        // 使用 split_whitespace 找到以 key= 开头的 token
        // 但 value 跨 token 时会截断，所以对长 value 字段用 find+split_once
        if let Some(pos) = data_line.find(&format!("{key}=")) {
            let after = &data_line[pos + key.len() + 1..];
            // value 以空格或行尾结束
            let end = after.find(' ').unwrap_or(after.len());
            Some(ts_unescape(&after[..end]))
        } else {
            None
        }
    };

    json!({
        "clid": kv("clid").and_then(|v| v.parse::<u32>().ok()),
        "nickname": kv("client_nickname"),
        "unique_id": kv("client_unique_identifier"),
        "database_id": kv("client_database_id").and_then(|v| v.parse::<u32>().ok()),
        "type": kv("client_type").and_then(|v| v.parse::<u32>().ok()),
        "country": kv("client_country"),
        "platform": kv("client_platform"),
        "version": kv("client_version"),
        "ip": kv("connection_client_ip").or_else(|| kv("client_ip")),
        "created": kv("client_created").and_then(|v| v.parse::<u64>().ok()),
        "last_connected": kv("client_lastconnected").and_then(|v| v.parse::<u64>().ok()),
        "total_connections": kv("client_totalconnections").and_then(|v| v.parse::<u32>().ok()),
        "channel_id": kv("cid").and_then(|v| v.parse::<u32>().ok()),
        "idle_time": kv("client_idle_time").and_then(|v| v.parse::<u64>().ok()),
    })
}
