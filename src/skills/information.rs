use crate::error::Result;
use crate::skills::{ExecutionContext, Skill};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct GetClientList;

#[async_trait]
impl Skill for GetClientList {
    fn name(&self) -> &'static str { "get_client_list" }
    fn description(&self) -> &'static str { "Get the list of online clients." }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }
    async fn execute(&self, _args: Value, ctx: &ExecutionContext) -> Result<Value> {
        let clients: Vec<_> = ctx.cache.list_clients();
        let json_clients: Vec<_> = clients.iter().map(|c| {
            json!({
                "clid": c.clid,
                "nickname": c.nickname,
                "dbid": c.cldbid,
                "groups": c.server_groups
            })
        }).collect();
        
        Ok(json!({"status": "ok", "clients": json_clients}))
    }
}
