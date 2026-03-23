use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use futures_util::StreamExt;
use tracing::{error, info, warn};

const DEFAULT_PROTOCOL_VERSION: &str = "2025-11-05";
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 120;
const DEFAULT_MAX_RETRIES: u32 = 2;
const DEFAULT_HEALTH_INTERVAL_SECS: u64 = 60;
const TOOLS_CACHE_TTL_SECS: u64 = 300;

// --- JSON-RPC 2.0 types ---

#[derive(Debug, Serialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcResponse {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    #[allow(dead_code)]
    id: Option<serde_json::Value>,
    result: Option<serde_json::Value>,
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

// --- MCP config types ---

fn default_transport() -> String {
    "stdio".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    #[serde(default = "default_transport")]
    pub transport: String,
    #[serde(default, alias = "protocolVersion")]
    pub protocol_version: Option<String>,
    #[serde(default)]
    pub request_timeout_secs: Option<u64>,
    #[serde(default)]
    pub max_retries: Option<u32>,
    #[serde(default)]
    pub health_interval_secs: Option<u64>,

    // stdio transport
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,

    // streamable_http transport
    #[serde(default, alias = "url")]
    pub endpoint: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct McpConfig {
    #[serde(default, alias = "defaultProtocolVersion")]
    pub default_protocol_version: Option<String>,
    #[serde(rename = "mcpServers")]
    pub mcp_servers: HashMap<String, McpServerConfig>,
}

#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub server_name: String,
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Clone)]
struct McpStdioSpawnSpec {
    command: String,
    args: Vec<String>,
    env: HashMap<String, String>,
}

// --- MCP server connection ---

struct McpStdioInner {
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    _child: Child,
    next_id: u64,
}

struct McpHttpInner {
    client: reqwest::Client,
    endpoint: String,
    headers: HashMap<String, String>,
    session_id: Option<String>,
    next_id: u64,
}

impl McpHttpInner {
    /// Build an HTTP request with standard MCP headers (Accept, session ID, custom headers).
    fn build_request(&self, body: &JsonRpcRequest) -> reqwest::RequestBuilder {
        let mut req = self
            .client
            .post(&self.endpoint)
            .json(body)
            .header("Accept", "application/json, text/event-stream");
        if let Some(sid) = &self.session_id {
            req = req.header("Mcp-Session-Id", sid);
        }
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        req
    }
}

enum McpTransport {
    Stdio(Box<Mutex<McpStdioInner>>),
    StreamableHttp(Box<Mutex<McpHttpInner>>),
}

pub struct McpServer {
    name: String,
    requested_protocol: String,
    negotiated_protocol: StdMutex<String>,
    request_timeout: Duration,
    max_retries: u32,
    transport: McpTransport,
    stdio_spawn: Option<McpStdioSpawnSpec>,
    tools_cache: StdMutex<Vec<McpToolInfo>>,
    tools_cache_updated_at: StdMutex<Option<Instant>>,
}

fn spawn_stdio_inner(spec: &McpStdioSpawnSpec, server_name: &str) -> Result<McpStdioInner, String> {
    let mut cmd = Command::new(&spec.command);
    cmd.args(&spec.args);
    cmd.envs(&spec.env);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn MCP server '{server_name}': {e}"))?;

    let stdin = child.stdin.take().ok_or("Failed to get stdin")?;
    let stdout = child.stdout.take().ok_or("Failed to get stdout")?;
    let stdout = BufReader::new(stdout);

    Ok(McpStdioInner {
        stdin,
        stdout,
        _child: child,
        next_id: 1,
    })
}

/// Extract the `data:` payload from a single SSE event block.
fn extract_sse_data(event: &str) -> Option<String> {
    for line in event.lines() {
        let trimmed = line.trim();
        if let Some(data) = trimmed.strip_prefix("data:") {
            let data = data.trim();
            if !data.is_empty() {
                return Some(data.to_string());
            }
        }
    }
    None
}

/// Try to extract a JSON-RPC response matching `request_id` from an SSE event block.
fn try_match_sse_event(event: &str, request_id: u64) -> Option<serde_json::Value> {
    let data = extract_sse_data(event)?;
    let val = serde_json::from_str::<serde_json::Value>(&data).ok()?;
    if val.get("id").and_then(|v| v.as_u64()) == Some(request_id) {
        Some(val)
    } else {
        None
    }
}

/// Read an SSE stream from an HTTP response, returning the JSON-RPC response
/// that matches the given request `id`. Processes chunks incrementally so it
/// works with both buffered and long-lived SSE connections.
async fn read_sse_stream(
    response: reqwest::Response,
    request_id: u64,
) -> Result<serde_json::Value, String> {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("SSE stream error: {e}"))?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        // Process complete SSE events (delimited by double newline)
        while let Some(pos) = buffer.find("\n\n") {
            let event = buffer[..pos].to_string();
            buffer.drain(..pos + 2);

            if let Some(val) = try_match_sse_event(&event, request_id) {
                return Ok(val);
            }
        }
    }

    // Stream ended — check if any remaining data in the buffer contains our response
    if !buffer.trim().is_empty() {
        if let Some(val) = try_match_sse_event(&buffer, request_id) {
            return Ok(val);
        }
    }

    Err("SSE stream ended without a matching JSON-RPC response".to_string())
}

impl McpServer {
    pub async fn connect(
        name: &str,
        config: &McpServerConfig,
        default_protocol_version: Option<&str>,
    ) -> Result<Self, String> {
        let requested_protocol = config
            .protocol_version
            .clone()
            .or_else(|| default_protocol_version.map(|v| v.to_string()))
            .unwrap_or_else(|| DEFAULT_PROTOCOL_VERSION.to_string());

        let request_timeout = Duration::from_secs(
            config
                .request_timeout_secs
                .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS),
        );
        let max_retries = config.max_retries.unwrap_or(DEFAULT_MAX_RETRIES);
        let transport_name = config.transport.trim().to_ascii_lowercase();

        let (transport, stdio_spawn) = match transport_name.as_str() {
            "stdio" | "" => {
                if config.command.trim().is_empty() {
                    return Err(format!(
                        "MCP server '{name}' requires `command` when transport=stdio"
                    ));
                }
                let spec = McpStdioSpawnSpec {
                    command: config.command.clone(),
                    args: config.args.clone(),
                    env: config.env.clone(),
                };
                let inner = spawn_stdio_inner(&spec, name)?;
                (McpTransport::Stdio(Box::new(Mutex::new(inner))), Some(spec))
            }
            "streamable_http" | "http" => {
                if config.endpoint.trim().is_empty() {
                    return Err(format!(
                        "MCP server '{name}' requires `endpoint` when transport=streamable_http"
                    ));
                }

                let client = reqwest::Client::builder()
                    .timeout(request_timeout)
                    .build()
                    .map_err(|e| format!("Failed to build HTTP client for MCP '{name}': {e}"))?;

                (
                    McpTransport::StreamableHttp(Box::new(Mutex::new(McpHttpInner {
                        client,
                        endpoint: config.endpoint.clone(),
                        headers: config.headers.clone(),
                        session_id: None,
                        next_id: 1,
                    }))),
                    None,
                )
            }
            other => {
                return Err(format!(
                    "MCP server '{name}' has unsupported transport '{other}'"
                ));
            }
        };

        let server = McpServer {
            name: name.to_string(),
            requested_protocol: requested_protocol.clone(),
            negotiated_protocol: StdMutex::new(requested_protocol),
            request_timeout,
            max_retries,
            transport,
            stdio_spawn,
            tools_cache: StdMutex::new(Vec::new()),
            tools_cache_updated_at: StdMutex::new(None),
        };

        server.initialize_connection().await?;
        let _ = server.refresh_tools_cache(true).await?;

        Ok(server)
    }

    fn is_cache_fresh(&self) -> bool {
        let guard = self
            .tools_cache_updated_at
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(ts) = *guard {
            ts.elapsed() < Duration::from_secs(TOOLS_CACHE_TTL_SECS)
        } else {
            false
        }
    }

    fn set_tools_cache(&self, tools: Vec<McpToolInfo>) {
        {
            let mut guard = self.tools_cache.lock().unwrap_or_else(|e| e.into_inner());
            *guard = tools;
        }
        let mut ts = self
            .tools_cache_updated_at
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *ts = Some(Instant::now());
    }

    pub fn tools_snapshot(&self) -> Vec<McpToolInfo> {
        self.tools_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn protocol_version(&self) -> String {
        self.negotiated_protocol
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn should_attempt_reconnect(err: &str) -> bool {
        let lower = err.to_ascii_lowercase();
        lower.contains("write error")
            || lower.contains("read error")
            || lower.contains("closed connection")
            || lower.contains("timeout")
            || lower.contains("broken pipe")
    }

    fn is_tool_not_found_error(err: &str) -> bool {
        let lower = err.to_ascii_lowercase();
        lower.contains("not found")
            || lower.contains("unknown tool")
            || lower.contains("tool not found")
    }

    fn invalidate_tools_cache(&self) {
        {
            let mut cache = self.tools_cache.lock().unwrap_or_else(|e| e.into_inner());
            cache.clear();
        }
        let mut ts = self
            .tools_cache_updated_at
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *ts = None;
    }

    async fn reconnect_stdio(&self, attempt: u32) -> Result<(), String> {
        let Some(spec) = self.stdio_spawn.as_ref() else {
            return Err("No stdio spawn spec available for reconnect".into());
        };

        let backoff_ms = 200u64.saturating_mul(2u64.saturating_pow(attempt.min(8)));
        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;

        let new_inner = spawn_stdio_inner(spec, &self.name)?;
        match &self.transport {
            McpTransport::Stdio(inner) => {
                let mut guard = inner.lock().await;
                *guard = new_inner;
            }
            McpTransport::StreamableHttp(_) => {
                return Err("Reconnect is only supported for stdio transport".into());
            }
        }

        self.initialize_stdio_after_spawn().await?;
        Ok(())
    }

    async fn send_request_stdio_once(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let inner = match &self.transport {
            McpTransport::Stdio(inner) => inner,
            McpTransport::StreamableHttp(_) => {
                return Err("Internal error: stdio request on http transport".into());
            }
        };

        let mut inner = inner.lock().await;
        let id = inner.next_id;
        inner.next_id += 1;

        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(id),
            method: method.to_string(),
            params,
        };

        let mut json = serde_json::to_string(&request).map_err(|e| e.to_string())?;
        json.push('\n');

        inner
            .stdin
            .write_all(json.as_bytes())
            .await
            .map_err(|e| format!("Write error: {e}"))?;
        inner
            .stdin
            .flush()
            .await
            .map_err(|e| format!("Flush error: {e}"))?;

        let mut line = String::new();
        let deadline = tokio::time::Instant::now() + self.request_timeout;

        loop {
            line.clear();
            let read_result =
                tokio::time::timeout_at(deadline, inner.stdout.read_line(&mut line)).await;

            match read_result {
                Err(_) => {
                    return Err(format!(
                        "MCP server response timeout ({:?})",
                        self.request_timeout
                    ))
                }
                Ok(Err(e)) => return Err(format!("Read error: {e}")),
                Ok(Ok(0)) => return Err("MCP server closed connection".into()),
                Ok(Ok(_)) => {}
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            if let Ok(response) = serde_json::from_str::<JsonRpcResponse>(trimmed) {
                let is_response = match &response.id {
                    Some(serde_json::Value::Number(n)) => n.as_u64() == Some(id),
                    _ => response.result.is_some() || response.error.is_some(),
                };
                if !is_response {
                    continue;
                }
                if let Some(err) = response.error {
                    return Err(format!("MCP error ({}): {}", err.code, err.message));
                }
                return Ok(response.result.unwrap_or(serde_json::Value::Null));
            }
        }
    }

    async fn send_notification_stdio_once(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<(), String> {
        let inner = match &self.transport {
            McpTransport::Stdio(inner) => inner,
            McpTransport::StreamableHttp(_) => {
                return Err("Internal error: stdio notification on http transport".into());
            }
        };

        let mut inner = inner.lock().await;
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: method.to_string(),
            params,
        };
        let mut json = serde_json::to_string(&request).map_err(|e| e.to_string())?;
        json.push('\n');
        inner
            .stdin
            .write_all(json.as_bytes())
            .await
            .map_err(|e| format!("Write error: {e}"))?;
        inner
            .stdin
            .flush()
            .await
            .map_err(|e| format!("Flush error: {e}"))?;
        Ok(())
    }

    async fn initialize_stdio_after_spawn(&self) -> Result<(), String> {
        let params = serde_json::json!({
            "protocolVersion": self.requested_protocol,
            "capabilities": {},
            "clientInfo": {
                "name": "rayclaw",
                "version": env!("CARGO_PKG_VERSION")
            }
        });

        let result = self
            .send_request_stdio_once("initialize", Some(params))
            .await?;
        let negotiated = result
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.requested_protocol)
            .to_string();

        {
            let mut guard = self
                .negotiated_protocol
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *guard = negotiated;
        }

        self.send_notification_stdio_once("notifications/initialized", None)
            .await?;
        Ok(())
    }

    async fn send_request_stdio_with_retries(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let mut last_err: Option<String> = None;

        for attempt in 0..=self.max_retries {
            match self.send_request_stdio_once(method, params.clone()).await {
                Ok(result) => return Ok(result),
                Err(err) => {
                    last_err = Some(err.clone());
                    if attempt >= self.max_retries
                        || self.stdio_spawn.is_none()
                        || !Self::should_attempt_reconnect(&err)
                    {
                        break;
                    }

                    warn!(
                        "MCP server '{}' request failed (attempt {}): {}. Reconnecting...",
                        self.name,
                        attempt + 1,
                        err
                    );
                    if let Err(reconnect_err) = self.reconnect_stdio(attempt).await {
                        return Err(format!(
                            "{err}; reconnect failed for '{}': {reconnect_err}",
                            self.name
                        ));
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| "Unknown MCP stdio error".to_string()))
    }

    async fn send_request_http(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let inner = match &self.transport {
            McpTransport::StreamableHttp(inner) => inner,
            McpTransport::Stdio(_) => {
                return Err("Internal error: http request on stdio transport".into());
            }
        };

        let mut inner = inner.lock().await;
        let id = inner.next_id;
        inner.next_id += 1;

        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(id),
            method: method.to_string(),
            params,
        };

        let req = inner.build_request(&request);

        let response = req
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {e}"))?;
        let status = response.status();

        // Capture session ID from response header
        if let Some(sid) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            inner.session_id = Some(sid.to_string());
        }

        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            return Err(format!("HTTP MCP request failed with {status}: {body_text}"));
        }

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let body_json = if content_type.contains("text/event-stream") {
            read_sse_stream(response, id).await?
        } else {
            response
                .json()
                .await
                .map_err(|e| format!("Failed to parse HTTP MCP response: {e}"))?
        };

        if let Ok(parsed) = serde_json::from_value::<JsonRpcResponse>(body_json.clone()) {
            if let Some(err) = parsed.error {
                return Err(format!("MCP error ({}): {}", err.code, err.message));
            }
            return Ok(parsed.result.unwrap_or(serde_json::Value::Null));
        }

        if let Some(result) = body_json.get("result") {
            return Ok(result.clone());
        }

        Ok(body_json)
    }

    async fn send_request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        match &self.transport {
            McpTransport::Stdio(_) => self.send_request_stdio_with_retries(method, params).await,
            McpTransport::StreamableHttp(_) => self.send_request_http(method, params).await,
        }
    }

    async fn send_notification(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<(), String> {
        match &self.transport {
            McpTransport::Stdio(_) => self.send_notification_stdio_once(method, params).await,
            McpTransport::StreamableHttp(inner) => {
                let mut inner = inner.lock().await;
                let request = JsonRpcRequest {
                    jsonrpc: "2.0".to_string(),
                    id: None,
                    method: method.to_string(),
                    params,
                };

                let req = inner.build_request(&request);

                let response = req
                    .send()
                    .await
                    .map_err(|e| format!("HTTP notification failed: {e}"))?;

                // Capture session ID from response (spec allows it on any response)
                if let Some(sid) = response
                    .headers()
                    .get("mcp-session-id")
                    .and_then(|v| v.to_str().ok())
                {
                    inner.session_id = Some(sid.to_string());
                }

                if response.status().is_success() {
                    Ok(())
                } else {
                    Err(format!(
                        "HTTP notification failed with status {}",
                        response.status()
                    ))
                }
            }
        }
    }

    async fn initialize_connection(&self) -> Result<(), String> {
        let params = serde_json::json!({
            "protocolVersion": self.requested_protocol,
            "capabilities": {},
            "clientInfo": {
                "name": "rayclaw",
                "version": env!("CARGO_PKG_VERSION")
            }
        });

        let result = self.send_request("initialize", Some(params)).await?;
        let negotiated = result
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.requested_protocol)
            .to_string();

        {
            let mut guard = self
                .negotiated_protocol
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if negotiated != self.requested_protocol {
                info!(
                    "MCP server '{}' negotiated protocol {} (requested {})",
                    self.name, negotiated, self.requested_protocol
                );
            }
            *guard = negotiated;
        }

        self.send_notification("notifications/initialized", None)
            .await?;

        Ok(())
    }

    async fn list_tools_uncached(&self) -> Result<Vec<McpToolInfo>, String> {
        let result = self
            .send_request("tools/list", Some(serde_json::json!({})))
            .await?;

        let tools_value = result.get("tools").ok_or("No tools in response")?;
        let tools_array = tools_value.as_array().ok_or("tools is not an array")?;

        let mut tools = Vec::new();
        for tool in tools_array {
            let name = tool
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let description = tool
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let input_schema = tool
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}}));

            tools.push(McpToolInfo {
                server_name: self.name.clone(),
                name,
                description,
                input_schema,
            });
        }

        Ok(tools)
    }

    pub async fn refresh_tools_cache(&self, force: bool) -> Result<Vec<McpToolInfo>, String> {
        if !force && self.is_cache_fresh() {
            return Ok(self.tools_snapshot());
        }

        let tools = self.list_tools_uncached().await?;
        self.set_tools_cache(tools.clone());
        Ok(tools)
    }

    pub async fn health_probe(&self) -> Result<(), String> {
        let _ = self.refresh_tools_cache(true).await?;
        Ok(())
    }

    pub fn start_health_probe(self: Arc<Self>, interval_secs: u64) {
        if interval_secs == 0 {
            return;
        }

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(interval_secs)).await;
                if let Err(e) = self.health_probe().await {
                    warn!("MCP health probe failed for '{}': {}", self.name, e);
                }
            }
        });
    }

    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<String, String> {
        let snapshot = self.tools_snapshot();
        if !snapshot.iter().any(|t| t.name == tool_name) {
            let _ = self.refresh_tools_cache(true).await;
        } else {
            let _ = self.refresh_tools_cache(false).await;
        }

        let params = serde_json::json!({
            "name": tool_name,
            "arguments": arguments
        });

        let result = match self.send_request("tools/call", Some(params)).await {
            Ok(result) => result,
            Err(err) => {
                if Self::is_tool_not_found_error(&err) {
                    self.invalidate_tools_cache();
                    let _ = self.refresh_tools_cache(true).await;
                }
                return Err(err);
            }
        };

        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if let Some(content) = result.get("content") {
            if let Some(array) = content.as_array() {
                let mut output = String::new();
                for item in array {
                    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                        if !output.is_empty() {
                            output.push('\n');
                        }
                        output.push_str(text);
                    }
                }
                if is_error {
                    if Self::is_tool_not_found_error(&output) {
                        self.invalidate_tools_cache();
                        let _ = self.refresh_tools_cache(true).await;
                    }
                    return Err(output);
                }
                return Ok(output);
            }
        }

        let output = serde_json::to_string_pretty(&result).unwrap_or_default();
        if is_error {
            if Self::is_tool_not_found_error(&output) {
                self.invalidate_tools_cache();
                let _ = self.refresh_tools_cache(true).await;
            }
            Err(output)
        } else {
            Ok(output)
        }
    }
}

// --- MCP manager ---

pub struct McpManager {
    servers: Vec<Arc<McpServer>>,
}

impl McpManager {
    pub async fn from_config_file(path: &str) -> Self {
        let config_str = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => {
                // Config file not found is normal — MCP is optional
                return McpManager {
                    servers: Vec::new(),
                };
            }
        };

        let config: McpConfig = match serde_json::from_str(&config_str) {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to parse MCP config {path}: {e}");
                return McpManager {
                    servers: Vec::new(),
                };
            }
        };

        let mut servers = Vec::new();
        for (name, server_config) in &config.mcp_servers {
            info!("Connecting to MCP server '{name}'...");
            match tokio::time::timeout(
                Duration::from_secs(30),
                McpServer::connect(
                    name,
                    server_config,
                    config.default_protocol_version.as_deref(),
                ),
            )
            .await
            {
                Ok(Ok(server)) => {
                    let server = Arc::new(server);
                    let interval = server_config
                        .health_interval_secs
                        .unwrap_or(DEFAULT_HEALTH_INTERVAL_SECS);
                    server.clone().start_health_probe(interval);

                    info!(
                        "MCP server '{name}' connected ({} tools, protocol {})",
                        server.tools_snapshot().len(),
                        server.protocol_version()
                    );
                    servers.push(server);
                }
                Ok(Err(e)) => {
                    warn!("Failed to connect MCP server '{name}': {e}");
                }
                Err(_) => {
                    warn!("MCP server '{name}' connection timed out (30s)");
                }
            }
        }

        McpManager { servers }
    }

    #[allow(dead_code)]
    pub fn servers(&self) -> &[Arc<McpServer>] {
        &self.servers
    }

    pub fn all_tools(&self) -> Vec<(Arc<McpServer>, McpToolInfo)> {
        let mut tools = Vec::new();
        for server in &self.servers {
            for tool in server.tools_snapshot() {
                tools.push((server.clone(), tool));
            }
        }
        tools
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mcp_config_defaults() {
        let json = r#"{
          "mcpServers": {
            "demo": {
              "command": "npx",
              "args": ["-y", "@modelcontextprotocol/server-filesystem", "."]
            }
          }
        }"#;

        let cfg: McpConfig = serde_json::from_str(json).unwrap();
        let server = cfg.mcp_servers.get("demo").unwrap();
        assert_eq!(server.transport, "stdio");
        assert!(server.protocol_version.is_none());
        assert!(server.max_retries.is_none());
    }

    #[test]
    fn test_tool_not_found_error_detection() {
        assert!(McpServer::is_tool_not_found_error("Tool not found"));
        assert!(McpServer::is_tool_not_found_error("unknown tool: x"));
        assert!(!McpServer::is_tool_not_found_error("permission denied"));
    }

    #[test]
    fn test_mcp_http_config_parse() {
        let json = r#"{
          "default_protocol_version": "2025-11-05",
          "mcpServers": {
            "remote": {
              "transport": "streamable_http",
              "endpoint": "http://127.0.0.1:8080/mcp",
              "headers": {"Authorization": "Bearer test"},
              "max_retries": 3,
              "health_interval_secs": 15
            }
          }
        }"#;

        let cfg: McpConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.default_protocol_version.unwrap(), "2025-11-05");
        let remote = cfg.mcp_servers.get("remote").unwrap();
        assert_eq!(remote.transport, "streamable_http");
        assert_eq!(remote.endpoint, "http://127.0.0.1:8080/mcp");
        assert_eq!(remote.max_retries, Some(3));
        assert_eq!(remote.health_interval_secs, Some(15));
    }

    #[test]
    fn test_extract_sse_data() {
        let event = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}";
        let data = extract_sse_data(event).unwrap();
        let val: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(val["id"], 1);
        assert_eq!(val["result"]["ok"], true);
    }

    #[test]
    fn test_extract_sse_data_no_data_line() {
        let event = "event: ping";
        assert!(extract_sse_data(event).is_none());
    }

    #[tokio::test]
    async fn test_read_sse_stream_single_event() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let response = http::Response::builder()
            .body(body)
            .unwrap();
        let response = reqwest::Response::from(response);
        let val = read_sse_stream(response, 1).await.unwrap();
        assert_eq!(val["id"], 1);
        assert_eq!(val["result"]["ok"], true);
    }

    #[tokio::test]
    async fn test_read_sse_stream_matches_request_id() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"first\":true}}\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"second\":true}}\n\n";
        let response = reqwest::Response::from(
            http::Response::builder().body(body).unwrap(),
        );
        let val = read_sse_stream(response, 2).await.unwrap();
        assert_eq!(val["result"]["second"], true);
    }

    #[tokio::test]
    async fn test_read_sse_stream_no_match() {
        let body = "event: ping\n\n";
        let response = reqwest::Response::from(
            http::Response::builder().body(body).unwrap(),
        );
        assert!(read_sse_stream(response, 1).await.is_err());
    }

    #[test]
    fn test_try_match_sse_event_matching_id() {
        let event = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{\"ok\":true}}";
        let val = try_match_sse_event(event, 5).unwrap();
        assert_eq!(val["id"], 5);
        assert_eq!(val["result"]["ok"], true);
    }

    #[test]
    fn test_try_match_sse_event_wrong_id() {
        let event = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"ok\":true}}";
        assert!(try_match_sse_event(event, 99).is_none());
    }

    #[test]
    fn test_try_match_sse_event_no_data() {
        let event = "event: ping";
        assert!(try_match_sse_event(event, 1).is_none());
    }

    #[test]
    fn test_try_match_sse_event_invalid_json() {
        let event = "data: not-valid-json";
        assert!(try_match_sse_event(event, 1).is_none());
    }

    #[test]
    fn test_try_match_sse_event_no_id_field() {
        let event = "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}";
        assert!(try_match_sse_event(event, 1).is_none());
    }

    #[test]
    fn test_build_request_includes_session_id() {
        let client = reqwest::Client::new();
        let inner = McpHttpInner {
            client,
            endpoint: "http://localhost:8080/mcp".to_string(),
            headers: HashMap::from([("X-Custom".to_string(), "val".to_string())]),
            session_id: Some("test-session-123".to_string()),
            next_id: 1,
        };
        let body = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(1),
            method: "test".to_string(),
            params: None,
        };
        let req = inner.build_request(&body).build().unwrap();
        assert_eq!(req.headers().get("Mcp-Session-Id").unwrap(), "test-session-123");
        assert_eq!(req.headers().get("X-Custom").unwrap(), "val");
        assert!(req.headers().get("Accept").unwrap().to_str().unwrap().contains("text/event-stream"));
    }

    /// Helper: build a reqwest::Response from a stream of byte chunks.
    fn response_from_chunks(chunks: Vec<&'static [u8]>) -> reqwest::Response {
        let stream = futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, std::io::Error>(Vec::from(c))),
        );
        let body = reqwest::Body::wrap_stream(stream);
        reqwest::Response::from(http::Response::new(body))
    }

    #[tokio::test]
    async fn test_read_sse_stream_chunked_event_split_across_chunks() {
        // SSE event arrives in two chunks: header in one, data in another
        let response = response_from_chunks(vec![
            b"event: message\n",
            b"data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n",
        ]);
        let val = read_sse_stream(response, 1).await.unwrap();
        assert_eq!(val["id"], 1);
        assert_eq!(val["result"]["ok"], true);
    }

    #[tokio::test]
    async fn test_read_sse_stream_data_line_split_mid_json() {
        // JSON payload itself is split across two chunks
        let response = response_from_chunks(vec![
            b"data: {\"jsonrpc\":\"2.0\",",
            b"\"id\":3,\"result\":{\"v\":42}}\n\n",
        ]);
        let val = read_sse_stream(response, 3).await.unwrap();
        assert_eq!(val["result"]["v"], 42);
    }

    #[tokio::test]
    async fn test_read_sse_stream_skips_non_matching_then_matches() {
        // First event has id=1, second has id=2 — request wants id=2
        let response = response_from_chunks(vec![
            b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"a\":1}}\n\n",
            b"event: message\n",
            b"data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"b\":2}}\n\n",
        ]);
        let val = read_sse_stream(response, 2).await.unwrap();
        assert_eq!(val["result"]["b"], 2);
    }

    #[tokio::test]
    async fn test_read_sse_stream_trailing_buffer_no_double_newline() {
        // Stream ends with data but no trailing \n\n — falls back to buffer check
        let response = response_from_chunks(vec![
            b"data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"tail\":true}}",
        ]);
        let val = read_sse_stream(response, 7).await.unwrap();
        assert_eq!(val["result"]["tail"], true);
    }

    #[test]
    fn test_build_request_no_session_id() {
        let client = reqwest::Client::new();
        let inner = McpHttpInner {
            client,
            endpoint: "http://localhost:8080/mcp".to_string(),
            headers: HashMap::new(),
            session_id: None,
            next_id: 1,
        };
        let body = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(1),
            method: "test".to_string(),
            params: None,
        };
        let req = inner.build_request(&body).build().unwrap();
        assert!(req.headers().get("Mcp-Session-Id").is_none());
    }
}
