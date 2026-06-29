use crate::skills::{ExecutionContext, Skill, UnifiedExecutionContext};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::OnceLock;
use std::time::Duration;
use tracing::debug;

const EXA_MCP_URL: &str = "https://mcp.exa.ai/mcp";
const MAX_RESULTS: u8 = 20;
const TIMEOUT_SECS: u64 = 25;

fn shared_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .build()
            .unwrap_or_default()
    })
}

async fn search_exa(query: &str, num_results: u8, search_type: &str) -> Result<String> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "web_search_exa",
            "arguments": {
                "query": query,
                "type": search_type,
                "numResults": num_results,
                "livecrawl": "fallback"
            }
        }
    });

    let resp = shared_client()
        .post(EXA_MCP_URL)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&body)
        .send()
        .await?;

    let status = resp.status();
    let text = resp.text().await?;

    if !status.is_success() {
        anyhow::bail!("Exa MCP returned status {}: {}", status, text);
    }

    let trimmed = text.trim();
    if let Some(result) = parse_payload(trimmed) {
        return Ok(result);
    }

    for line in text.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            if let Some(result) = parse_payload(data.trim()) {
                return Ok(result);
            }
        }
    }

    anyhow::bail!("No search results found in response")
}

fn parse_payload(payload: &str) -> Option<String> {
    if !payload.starts_with('{') {
        return None;
    }
    let parsed: Value = serde_json::from_str(payload).ok()?;
    let content = parsed
        .get("result")?
        .get("content")?
        .as_array()?
        .iter()
        .filter_map(|item| item.get("text")?.as_str())
        .next()?;
    Some(content.to_string())
}

pub struct WebSearch;

#[async_trait]
impl Skill for WebSearch {
    fn name(&self) -> &'static str {
        "web_search"
    }
    fn description(&self) -> &'static str {
        "Search the web for current information beyond knowledge cutoff. Use this to get up-to-date data, news, or any information that may have changed since the model's training date."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "The search query to look up on the web." },
                "numResults": { "type": "integer", "description": "Number of search results to return (1-20, default: 8)", "minimum": 1, "maximum": MAX_RESULTS },
                "type": { "type": "string", "enum": ["auto", "fast", "deep"], "description": "Search type - auto: balanced, fast: quick results, deep: comprehensive (default: auto)" }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value> {
        let _ = ctx;
        execute_search(args).await
    }

    async fn execute_unified(&self, args: Value, _ctx: &UnifiedExecutionContext) -> Result<Value> {
        execute_search(args).await
    }
}

async fn execute_search(args: Value) -> Result<Value> {
    let query = args["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required parameter: query"))?;

    let num_results = args["num_results"]
        .as_u64()
        .unwrap_or(8)
        .min(MAX_RESULTS as u64) as u8;

    let search_type = args["type"].as_str().unwrap_or("auto");

    debug!(query, num_results, search_type, "WebSearch executing");

    let result = search_exa(query, num_results, search_type).await?;

    Ok(json!({
        "status": "ok",
        "result": result
    }))
}
