use crate::config::web_search::SearchProvider;
use crate::tools::http::shared_client;
use crate::tools::{Tool, UnifiedExecutionContext};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::debug;

const EXA_MCP_URL: &str = "https://mcp.exa.ai/mcp";
const PARALLEL_MCP_URL: &str = "https://search.parallel.ai/mcp";
const USER_AGENT: &str = "TeamSpeakClaw (https://github.com/Dr1mH4X/TeamSpeakClaw)";
const MAX_RESULTS: u8 = 20;
const TIMEOUT_SECS: u64 = 25;

async fn search_exa(
    client: &reqwest::Client,
    endpoint: &str,
    query: &str,
    num_results: u8,
    search_type: &str,
) -> Result<String> {
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

    let resp = client
        .post(endpoint)
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
        if let Some(data) = line.strip_prefix("data:") {
            if let Some(result) = parse_payload(data.trim_start()) {
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

pub struct WebSearch {
    provider: SearchProvider,
    endpoint: String,
    client: reqwest::Client,
}

impl WebSearch {
    pub fn new(provider: SearchProvider) -> Self {
        Self {
            provider,
            endpoint: match provider {
                SearchProvider::Exa => EXA_MCP_URL,
                SearchProvider::Parallel => PARALLEL_MCP_URL,
            }
            .to_string(),
            client: shared_client(TIMEOUT_SECS),
        }
    }
}

#[async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &'static str {
        "web_search"
    }
    fn description(&self) -> &'static str {
        "Search the web for current information beyond knowledge cutoff. Use this to get up-to-date data, news, or any information that may have changed since the model's training date."
    }
    fn parameters(&self) -> Value {
        if self.provider == SearchProvider::Parallel {
            return json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "The search query to look up on the web." }
                },
                "required": ["query"],
                "additionalProperties": false
            });
        }
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

    async fn execute(&self, args: Value, _ctx: &UnifiedExecutionContext) -> Result<Value> {
        let query = args["query"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: query"))?;
        if query.trim().is_empty() {
            anyhow::bail!("query cannot be empty");
        }
        let result = match self.provider {
            SearchProvider::Exa => {
                execute_exa_search(&self.client, &self.endpoint, &args, query).await?
            }
            SearchProvider::Parallel => {
                if args
                    .as_object()
                    .is_some_and(|args| args.keys().any(|key| key != "query"))
                {
                    anyhow::bail!("Parallel search supports only query; search settings are managed by the server");
                }
                search_parallel(&self.client, &self.endpoint, query).await?
            }
        };
        Ok(json!({"status": "ok", "result": result}))
    }
}

async fn execute_exa_search(
    client: &reqwest::Client,
    endpoint: &str,
    args: &Value,
    query: &str,
) -> Result<String> {
    let num_results = match args.get("numResults") {
        Some(value) => value
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("numResults must be an integer"))?,
        None => 8,
    };
    if !(1..=u64::from(MAX_RESULTS)).contains(&num_results) {
        return Err(anyhow::anyhow!(
            "numResults must be between 1 and {}",
            MAX_RESULTS
        ));
    }
    let num_results = num_results as u8;

    let search_type = args["type"].as_str().unwrap_or("auto");
    if !matches!(search_type, "auto" | "fast" | "deep") {
        return Err(anyhow::anyhow!("type must be one of: auto, fast, deep"));
    }

    debug!(query, num_results, search_type, "WebSearch executing");

    search_exa(client, endpoint, query, num_results, search_type).await
}

async fn search_parallel(client: &reqwest::Client, endpoint: &str, query: &str) -> Result<String> {
    let response = client
        .post(endpoint)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2025-03-26")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "web_search",
                "arguments": { "objective": query, "search_queries": [query] }
            }
        }))
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        anyhow::bail!("Parallel MCP returned status {}: {}", status, text);
    }
    if text.trim_start().starts_with('{') {
        return parse_parallel_payload(text.trim());
    }
    // Streamable HTTP 可返回 SSE；同一事件的多行 data 按协议拼接。
    let normalized = text.replace("\r\n", "\n");
    for event in normalized.split("\n\n") {
        let data = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(|line| line.strip_prefix(' ').unwrap_or(line))
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            continue;
        }
        let payload: Value = serde_json::from_str(&data)?;
        if payload.get("id") == Some(&json!(1)) {
            return parse_parallel_payload(&data);
        }
    }
    anyhow::bail!("No search results found in Parallel MCP response")
}

fn parse_parallel_payload(payload: &str) -> Result<String> {
    let parsed: Value = serde_json::from_str(payload)?;
    if let Some(error) = parsed.get("error") {
        anyhow::bail!("Parallel MCP error: {}", error);
    }
    let result = parsed
        .get("result")
        .ok_or_else(|| anyhow::anyhow!("Missing Parallel MCP result"))?;
    if result["isError"].as_bool() == Some(true) {
        anyhow::bail!("Parallel MCP tool error: {}", result["content"]);
    }
    let text = result["content"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Missing Parallel MCP content"))?
        .iter()
        .filter(|item| item["type"] == "text")
        .filter_map(|item| item["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        anyhow::bail!("No text in Parallel MCP result");
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AclConfig, AppConfig};
    use crate::llm::ToolCall;
    use crate::permission::PermissionGate;
    use crate::tools::{Platform, ToolRegistry, VoiceAudioHandles};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    fn context(config: AppConfig) -> UnifiedExecutionContext {
        UnifiedExecutionContext {
            platform: Platform::NapCat,
            ts_adapter: None,
            nc_adapter: None,
            caller_id: 0,
            caller_id_nc: 42,
            caller_name: "test".to_string(),
            caller_groups: vec![],
            caller_channel_group_id: 0,
            nc_group_id: None,
            gate: Arc::new(PermissionGate::new(AclConfig::default())),
            config: Arc::new(config),
        }
    }

    fn configured(provider: &str) -> AppConfig {
        toml::from_str(&format!("[web_search]\nprovider = '{provider}'")).unwrap()
    }

    fn call(args: Value) -> ToolCall {
        ToolCall {
            id: "search-test".to_string(),
            name: "web_search".to_string(),
            arguments: args,
        }
    }

    async fn fixture(
        status: &str,
        body: &str,
        stall: bool,
    ) -> (
        String,
        oneshot::Receiver<String>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buf = [0; 4096];
                let count = stream.read(&mut buf).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buf[..count]);
                let text = String::from_utf8_lossy(&request);
                if let Some((headers, body)) = text.split_once("\r\n\r\n") {
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            line.to_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_owned)
                        })
                        .unwrap()
                        .parse()
                        .unwrap();
                    if body.len() >= length {
                        break;
                    }
                }
            }
            tx.send(String::from_utf8(request).unwrap()).unwrap();
            if stall {
                std::future::pending::<()>().await;
            }
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        (endpoint, rx, task)
    }

    fn fixture_registry(
        provider: SearchProvider,
        endpoint: String,
        timeout: Duration,
    ) -> ToolRegistry {
        let registry = ToolRegistry::with_defaults(
            Arc::new(configured(match provider {
                SearchProvider::Exa => "exa",
                SearchProvider::Parallel => "parallel",
            })),
            VoiceAudioHandles::default(),
        );
        let mut tool = WebSearch::new(provider);
        tool.endpoint = endpoint;
        tool.client = reqwest::Client::builder()
            .no_proxy()
            .timeout(timeout)
            .build()
            .unwrap();
        registry.register(Box::new(tool));
        registry
    }

    #[test]
    fn configured_loader_exposes_only_supported_parameters() {
        for provider in ["exa", "parallel"] {
            let registry = ToolRegistry::with_defaults(
                Arc::new(configured(provider)),
                VoiceAudioHandles::default(),
            );
            let schemas = registry.to_tool_schemas(&["web_search".to_string()]);
            assert_eq!(schemas.len(), 1);
            let properties = &schemas[0]["function"]["parameters"]["properties"];
            assert_eq!(properties["query"]["type"], "string");
            assert_eq!(properties.get("type").is_some(), provider == "exa");
            assert_eq!(properties.get("numResults").is_some(), provider == "exa");
        }
        assert_eq!(WebSearch::new(SearchProvider::Exa).endpoint, EXA_MCP_URL);
        assert_eq!(
            WebSearch::new(SearchProvider::Parallel).endpoint,
            "https://search.parallel.ai/mcp"
        );
    }

    #[tokio::test]
    async fn parallel_registry_sends_anonymous_request_and_consumes_all_text() {
        let response = json!({"jsonrpc":"2.0","id":1,"result":{"content":[
            {"type":"text","text":"Rust release: https://www.rust-lang.org/"},
            {"type":"text","text":"Current release details"}
        ]}})
        .to_string();
        let (endpoint, request, server) = fixture("200 OK", &response, false).await;
        let registry = fixture_registry(SearchProvider::Parallel, endpoint, Duration::from_secs(2));
        let result = registry
            .execute_tool(
                &call(json!({"query":"Rust latest stable release"})),
                context(configured("parallel")),
                &["web_search".to_string()],
            )
            .await;
        let value: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(value["status"], "ok");
        assert_eq!(
            value["result"],
            "Rust release: https://www.rust-lang.org/\nCurrent release details"
        );
        let request = request.await.unwrap();
        let (headers, body) = request.split_once("\r\n\r\n").unwrap();
        assert!(headers.starts_with("POST /mcp HTTP/1.1"));
        assert!(headers
            .to_lowercase()
            .contains(&format!("user-agent: {USER_AGENT}").to_lowercase()));
        assert!(headers
            .to_lowercase()
            .contains("accept: application/json, text/event-stream"));
        assert!(headers
            .to_lowercase()
            .contains("mcp-protocol-version: 2025-03-26"));
        assert!(!headers.to_lowercase().contains("authorization"));
        assert!(!headers.to_lowercase().contains("api-key"));
        let payload: Value = serde_json::from_str(body).unwrap();
        assert_eq!(payload["params"]["name"], "web_search");
        assert_eq!(
            payload["params"]["arguments"],
            json!({
                "objective":"Rust latest stable release", "search_queries":["Rust latest stable release"]
            })
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn parallel_handles_sse_and_surfaces_protocol_and_http_errors() {
        for (status, response, expected) in [
            ("200 OK", "event: message\r\ndata: {\r\ndata: \"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"SSE result\"}]}}\r\n\r\n", "SSE result"),
            ("200 OK", r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"Invalid arguments"}}"#, "Parallel MCP error"),
            ("200 OK", r#"{"jsonrpc":"2.0","id":1,"result":{"isError":true,"content":[{"type":"text","text":"quota reached"}]}}"#, "Parallel MCP tool error"),
            ("429 Too Many Requests", "quota reached", "Parallel MCP returned status 429"),
            ("200 OK", "not JSON or SSE", "No search results"),
            ("200 OK", r#"{"result":{"content":[]}}"#, "No text"),
        ] {
            let (endpoint, request, server) = fixture(status, response, false).await;
            let registry = fixture_registry(SearchProvider::Parallel, endpoint, Duration::from_secs(2));
            let result = registry.execute_tool(&call(json!({"query":"rust releases"})), context(configured("parallel")), &["*".to_string()]).await;
            assert!(result.contains(expected), "{result}");
            if expected != "SSE result" {
                assert!(result.starts_with("Tool execution failed:"));
            }
            request.await.unwrap();
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn exa_request_and_result_wrapper_are_preserved() {
        let (endpoint, request, server) = fixture(
            "200 OK",
            r#"{"result":{"content":[{"text":"Exa result"}]}}"#,
            false,
        )
        .await;
        let registry = fixture_registry(SearchProvider::Exa, endpoint, Duration::from_secs(2));
        let result = registry
            .execute_tool(
                &call(json!({"query":"rust releases"})),
                context(AppConfig::default()),
                &["web_search".to_string()],
            )
            .await;
        assert_eq!(
            serde_json::from_str::<Value>(&result).unwrap(),
            json!({"status":"ok","result":"Exa result"})
        );
        let request = request.await.unwrap();
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["params"]["name"], "web_search_exa");
        assert_eq!(
            body["params"]["arguments"],
            json!({"query":"rust releases","type":"auto","numResults":8,"livecrawl":"fallback"})
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn acl_and_invalid_arguments_do_not_send_requests() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let registry = fixture_registry(
            SearchProvider::Parallel,
            endpoint.clone(),
            Duration::from_millis(100),
        );
        let denied = registry
            .execute_tool(
                &call(json!({"query":"rust releases"})),
                context(configured("parallel")),
                &[],
            )
            .await;
        assert_eq!(denied, "Tool execution denied");
        for args in [
            json!({}),
            json!({"query":" "}),
            json!({"query":42}),
            json!({"query":"rust","type":"deep"}),
            json!({"query":"rust","numResults":3}),
        ] {
            let result = registry
                .execute_tool(
                    &call(args),
                    context(configured("parallel")),
                    &["web_search".to_string()],
                )
                .await;
            assert!(result.starts_with("Tool execution failed:"));
        }
        let exa = fixture_registry(SearchProvider::Exa, endpoint, Duration::from_millis(100));
        for args in [
            json!({"query":"rust", "numResults":0}),
            json!({"query":"rust", "numResults":21}),
            json!({"query":"rust", "numResults":1.5}),
            json!({"query":"rust", "type":"invalid"}),
        ] {
            let result = exa
                .execute_tool(
                    &call(args),
                    context(AppConfig::default()),
                    &["*".to_string()],
                )
                .await;
            assert!(result.starts_with("Tool execution failed:"));
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn parallel_timeout_and_cancellation_stop_the_caller() {
        for cancel in [false, true] {
            let (endpoint, request, server) = fixture("200 OK", "", true).await;
            let registry = fixture_registry(
                SearchProvider::Parallel,
                endpoint,
                Duration::from_millis(100),
            );
            let caller = tokio::spawn(async move {
                registry
                    .execute_tool(
                        &call(json!({"query":"rust releases"})),
                        context(configured("parallel")),
                        &["*".to_string()],
                    )
                    .await
            });
            request.await.unwrap();
            if cancel {
                caller.abort();
                assert!(caller.await.unwrap_err().is_cancelled());
            } else {
                let result = caller.await.unwrap();
                assert!(result.starts_with("Tool execution failed:"));
            }
            server.abort();
        }
    }

    // 需联网；通过真实配置与注册表调用匿名端点，会消耗免费额度。
    #[tokio::test]
    #[ignore]
    async fn parallel_live_registry_search() {
        let config = configured("parallel");
        let registry =
            ToolRegistry::with_defaults(Arc::new(config.clone()), VoiceAudioHandles::default());
        let result = registry
            .execute_tool(
                &call(json!({"query":"Rust latest stable release site:rust-lang.org"})),
                context(config),
                &["web_search".to_string()],
            )
            .await;
        println!("{result}");
        let value: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(value["status"], "ok");
        assert!(value["result"].as_str().unwrap().contains("https://"));
    }
}
