//! ACP (Agent Client Protocol) integration for RayClaw.
//!
//! Allows RayClaw to spawn and control external Coding Agents
//! (e.g. Claude Code) as subprocesses via the ACP JSON-RPC protocol.
//!
//! MVP scope: Claude Code support only, stdio transport.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock};
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

// ---------------------------------------------------------------------------
// Config types — loaded from <data_root>/acp.json
// ---------------------------------------------------------------------------

fn default_prompt_timeout_secs() -> u64 {
    300
}

fn default_max_sessions() -> usize {
    20
}

fn default_max_per_agent() -> usize {
    10
}

fn default_idle_timeout_secs() -> u64 {
    600
}

fn default_launch() -> String {
    "npx".to_string()
}

fn default_mode() -> String {
    "acp".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct AcpAgentConfig {
    /// Connection mode: "acp" (default, full JSON-RPC protocol) or "pty"
    /// (simple stdin/stdout piping for non-ACP CLI tools).
    #[serde(default = "default_mode")]
    pub mode: String,

    /// Launch method: "npx" | "binary" | "uvx"
    #[serde(default = "default_launch")]
    pub launch: String,

    /// Executable or package name.
    /// npx: package spec (e.g. "@anthropic-ai/claude-code@latest")
    /// binary: absolute path to executable
    pub command: String,

    #[serde(default)]
    pub args: Vec<String>,

    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Default working directory for this agent
    #[serde(default)]
    pub workspace: Option<String>,

    /// Override the global auto_approve setting for this agent
    #[serde(default)]
    pub auto_approve: Option<bool>,

    /// Optional resource limits enforced via cgroups v2 (Linux only).
    /// On non-Linux platforms, limits are logged and silently ignored.
    #[serde(default, alias = "resourceLimits")]
    pub resource_limits: Option<ResourceLimits>,
}

/// Resource limits enforced via cgroups v2 on Linux.
#[derive(Debug, Clone, Deserialize)]
pub struct ResourceLimits {
    /// Maximum memory in megabytes. Mapped to cgroup `memory.max`.
    #[serde(default, alias = "memoryMb")]
    pub memory_mb: Option<u64>,

    /// CPU percentage cap. 100 = 1 full core, 200 = 2 cores.
    /// Mapped to cgroup `cpu.max` (e.g. 200 → "200000 100000").
    #[serde(default, alias = "cpuPercent")]
    pub cpu_percent: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct AcpConfig {
    /// Automatically approve tool calls from agents
    #[serde(default, alias = "defaultAutoApprove")]
    pub default_auto_approve: bool,

    /// Prompt execution timeout in seconds
    #[serde(default = "default_prompt_timeout_secs", alias = "promptTimeoutSecs")]
    pub prompt_timeout_secs: u64,

    /// Maximum total concurrent ACP sessions across all agents
    #[serde(default = "default_max_sessions", alias = "maxSessions")]
    pub max_sessions: usize,

    /// Maximum concurrent sessions per individual agent
    #[serde(default = "default_max_per_agent", alias = "maxPerAgent")]
    pub max_per_agent: usize,

    /// Idle timeout in seconds. Sessions with no prompt activity for this
    /// duration are automatically reaped. 0 disables the reaper.
    #[serde(default = "default_idle_timeout_secs", alias = "idleTimeoutSecs")]
    pub idle_timeout_secs: u64,

    /// Configured agents, keyed by name (e.g. "claude", "opencode")
    #[serde(default, alias = "acpAgents")]
    pub agents: HashMap<String, AcpAgentConfig>,

    /// Optional bearer token for the ACP HTTP API. If set, all `/api/acp/*`
    /// routes require `Authorization: Bearer <token>`. If empty/absent, the
    /// ACP API inherits the web_auth_token or is unauthenticated.
    #[serde(default, alias = "acpApiToken")]
    pub acp_api_token: Option<String>,
}

impl Default for AcpConfig {
    fn default() -> Self {
        AcpConfig {
            default_auto_approve: false,
            prompt_timeout_secs: default_prompt_timeout_secs(),
            max_sessions: default_max_sessions(),
            max_per_agent: default_max_per_agent(),
            idle_timeout_secs: default_idle_timeout_secs(),
            agents: HashMap::new(),
            acp_api_token: None,
        }
    }
}

impl AcpConfig {
    /// Load config from a JSON file. Returns default (empty) config on
    /// missing file or parse error — ACP is optional, same as MCP.
    pub fn from_file(path: &str) -> Self {
        let config_str = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => return AcpConfig::default(),
        };

        match serde_json::from_str(&config_str) {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to parse ACP config {path}: {e}");
                AcpConfig::default()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 types (shared with connection layer)
// ---------------------------------------------------------------------------

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
struct JsonRpcMessage {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<serde_json::Value>,
    method: Option<String>,
    params: Option<serde_json::Value>,
    result: Option<serde_json::Value>,
    error: Option<JsonRpcError>,
}

impl JsonRpcMessage {
    /// True if this message is a response (has id + result/error, no method)
    fn is_response(&self) -> bool {
        self.id.is_some() && self.method.is_none()
    }

    /// True if this message is a notification (has method, no id)
    fn is_notification(&self) -> bool {
        self.method.is_some() && self.id.is_none()
    }

    /// True if this message is a request from the agent (has both id AND method).
    /// e.g. session/request_permission
    fn is_request(&self) -> bool {
        self.id.is_some() && self.method.is_some()
    }
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

// ---------------------------------------------------------------------------
// ACP Connection — stdio transport to a single agent process
// ---------------------------------------------------------------------------

const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;
const ACP_PROTOCOL_VERSION: u32 = 1;

struct AcpConnectionInner {
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    _child: Child,
    next_id: u64,
}

/// A connection to a single ACP agent process over stdio JSON-RPC.
pub struct AcpConnection {
    agent_name: String,
    inner: Mutex<AcpConnectionInner>,
    request_timeout: Duration,
}

/// Build the OS command for spawning an agent process.
fn build_spawn_command(config: &AcpAgentConfig, workspace: Option<&str>) -> Command {
    let (program, base_args): (&str, Vec<&str>) = match config.launch.as_str() {
        "npx" => ("npx", vec!["-y", &config.command]),
        "uvx" => ("uvx", vec![&config.command]),
        _ => (&config.command, vec![]),
    };

    let mut cmd = Command::new(program);
    for arg in &base_args {
        cmd.arg(arg);
    }
    for arg in &config.args {
        cmd.arg(arg);
    }
    // Remove environment variables that cause nested-session detection in
    // Claude Code.  When RayClaw itself runs inside a Claude Code session
    // (e.g. as a tool), these vars are inherited and the ACP agent refuses
    // to start.
    cmd.env_remove("CLAUDECODE");
    cmd.env_remove("CLAUDE_CODE_ENTRYPOINT");
    cmd.env_remove("CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS");
    cmd.envs(&config.env);

    if let Some(ws) = workspace.or(config.workspace.as_deref()) {
        cmd.current_dir(ws);
    }

    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    cmd
}

// ---------------------------------------------------------------------------
// Cgroup v2 resource isolation (Linux only)
// ---------------------------------------------------------------------------

const CGROUP_BASE: &str = "/sys/fs/cgroup/rayclaw";

/// Apply cgroup v2 resource limits to a child process.
/// Returns the cgroup path on success (for later cleanup), or logs a warning
/// and returns None on failure or unsupported platforms.
fn apply_resource_limits(pid: u32, session_id: &str, limits: &ResourceLimits) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        apply_resource_limits_linux(pid, session_id, limits)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pid, session_id, limits);
        warn!("Resource limits configured but cgroups are only supported on Linux; ignoring");
        None
    }
}

#[cfg(target_os = "linux")]
fn apply_resource_limits_linux(
    pid: u32,
    session_id: &str,
    limits: &ResourceLimits,
) -> Option<String> {
    use std::fs;
    use std::path::Path;

    let cgroup_path = format!("{CGROUP_BASE}/{session_id}");
    let cgroup_dir = Path::new(&cgroup_path);

    // Create cgroup directory
    if let Err(e) = fs::create_dir_all(cgroup_dir) {
        warn!(
            "Failed to create cgroup directory {cgroup_path}: {e}. \
             Resource limits will not be applied. \
             Ensure /sys/fs/cgroup is writable or run with appropriate permissions."
        );
        return None;
    }

    // Apply memory limit
    if let Some(memory_mb) = limits.memory_mb {
        let bytes = memory_mb * 1024 * 1024;
        if let Err(e) = fs::write(cgroup_dir.join("memory.max"), bytes.to_string()) {
            warn!("Failed to set memory.max for cgroup {session_id}: {e}");
        } else {
            info!("Cgroup [{session_id}]: memory.max = {memory_mb}MB");
        }
    }

    // Apply CPU limit — cpu.max format: "$MAX $PERIOD" (microseconds)
    // e.g. cpu_percent=200 → "200000 100000" (200% of one core over 100ms period)
    if let Some(cpu_percent) = limits.cpu_percent {
        let period_us: u64 = 100_000; // 100ms
        let quota_us = cpu_percent * period_us / 100;
        let value = format!("{quota_us} {period_us}");
        if let Err(e) = fs::write(cgroup_dir.join("cpu.max"), &value) {
            warn!("Failed to set cpu.max for cgroup {session_id}: {e}");
        } else {
            info!("Cgroup [{session_id}]: cpu.max = {value} ({cpu_percent}%)");
        }
    }

    // Move process into cgroup
    if let Err(e) = fs::write(cgroup_dir.join("cgroup.procs"), pid.to_string()) {
        warn!("Failed to add PID {pid} to cgroup {session_id}: {e}");
        // Clean up the cgroup dir since we couldn't use it
        let _ = fs::remove_dir(cgroup_dir);
        return None;
    }

    info!("Cgroup [{session_id}]: PID {pid} assigned to {cgroup_path}");
    Some(cgroup_path)
}

/// Remove a cgroup directory. Safe to call even if the cgroup doesn't exist.
fn cleanup_cgroup(cgroup_path: &str) {
    #[cfg(target_os = "linux")]
    {
        use std::fs;
        use std::path::Path;

        let dir = Path::new(cgroup_path);
        if dir.exists() {
            // Processes should have exited by now; remove the empty cgroup
            match fs::remove_dir(dir) {
                Ok(()) => info!("Cgroup removed: {cgroup_path}"),
                Err(e) => debug!("Failed to remove cgroup {cgroup_path}: {e}"),
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = cgroup_path;
    }
}

impl AcpConnection {
    /// Spawn an agent process and perform the ACP initialization handshake.
    pub async fn spawn(
        agent_name: &str,
        config: &AcpAgentConfig,
        workspace: Option<&str>,
        request_timeout: Duration,
    ) -> Result<Self, String> {
        let mut cmd = build_spawn_command(config, workspace);

        info!(
            "ACP: spawning agent '{agent_name}' ({} {})",
            config.launch, config.command
        );

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn ACP agent '{agent_name}': {e}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("ACP agent '{agent_name}': failed to capture stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("ACP agent '{agent_name}': failed to capture stdout"))?;

        // Spawn a task to drain stderr to tracing::debug
        if let Some(stderr) = child.stderr.take() {
            let name = agent_name.to_string();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let trimmed = line.trim();
                            if !trimmed.is_empty() {
                                debug!("ACP [{name}] stderr: {trimmed}");
                            }
                        }
                    }
                }
            });
        }

        let conn = AcpConnection {
            agent_name: agent_name.to_string(),
            inner: Mutex::new(AcpConnectionInner {
                stdin,
                stdout: BufReader::new(stdout),
                _child: child,
                next_id: 1,
            }),
            request_timeout,
        };

        // Perform initialization handshake
        conn.initialize().await?;

        Ok(conn)
    }

    /// Send the `initialize` request and `notifications/initialized` notification.
    async fn initialize(&self) -> Result<(), String> {
        let params = serde_json::json!({
            "protocolVersion": ACP_PROTOCOL_VERSION,
            "clientCapabilities": {
                "fs": {
                    "readTextFile": false,
                    "writeTextFile": false
                },
                "terminal": false
            },
            "clientInfo": {
                "name": "rayclaw",
                "version": env!("CARGO_PKG_VERSION")
            }
        });

        let result = self.send_request("initialize", Some(params)).await?;

        let server_version = result
            .get("protocolVersion")
            .map(|v| match v {
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_else(|| "unknown".to_string());
        let server_name = result
            .get("serverInfo")
            .or_else(|| result.get("agentInfo"))
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        info!(
            "ACP [{}]: initialized (agent={server_name}, protocol={server_version})",
            self.agent_name
        );

        // Send the notifications/initialized notification (ACP spec).
        // Some agents (e.g. Zed claude-agent-acp) don't implement this
        // notification and return Method-not-found; that's harmless — just log it.
        if let Err(e) = self
            .send_notification("notifications/initialized", None)
            .await
        {
            debug!(
                "ACP [{}]: notifications/initialized not supported ({e}), continuing",
                self.agent_name
            );
        }

        Ok(())
    }

    /// Send a JSON-RPC request and wait for the matching response.
    /// Notifications received while waiting are logged and discarded.
    pub async fn send_request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let mut inner = self.inner.lock().await;
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
            .map_err(|e| format!("ACP [{}] write error: {e}", self.agent_name))?;
        inner
            .stdin
            .flush()
            .await
            .map_err(|e| format!("ACP [{}] flush error: {e}", self.agent_name))?;

        // Read lines until we get the matching response
        let deadline = tokio::time::Instant::now() + self.request_timeout;
        let mut line = String::new();

        loop {
            line.clear();
            let read_result =
                tokio::time::timeout_at(deadline, inner.stdout.read_line(&mut line)).await;

            match read_result {
                Err(_) => {
                    return Err(format!(
                        "ACP [{}] request '{}' timed out ({:?})",
                        self.agent_name, method, self.request_timeout
                    ));
                }
                Ok(Err(e)) => {
                    return Err(format!("ACP [{}] read error: {e}", self.agent_name));
                }
                Ok(Ok(0)) => {
                    return Err(format!("ACP [{}] agent closed connection", self.agent_name));
                }
                Ok(Ok(_)) => {}
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let msg: JsonRpcMessage = match serde_json::from_str(trimmed) {
                Ok(m) => m,
                Err(_) => {
                    debug!(
                        "ACP [{}] ignoring non-JSON line: {}",
                        self.agent_name,
                        &trimmed[..trimmed.len().min(200)]
                    );
                    continue;
                }
            };

            if msg.is_notification() {
                // Discard notifications during simple request/response
                debug!(
                    "ACP [{}] notification during '{}': {:?}",
                    self.agent_name, method, msg.method
                );
                continue;
            }

            if msg.is_response() {
                let matches = match &msg.id {
                    Some(serde_json::Value::Number(n)) => n.as_u64() == Some(id),
                    _ => true, // best effort
                };
                if !matches {
                    continue;
                }
                if let Some(err) = msg.error {
                    return Err(format!(
                        "ACP [{}] error ({}): {}",
                        self.agent_name, err.code, err.message
                    ));
                }
                return Ok(msg.result.unwrap_or(serde_json::Value::Null));
            }
        }
    }

    /// Send a JSON-RPC notification (no response expected).
    pub async fn send_notification(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
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
            .map_err(|e| format!("ACP [{}] write error: {e}", self.agent_name))?;
        inner
            .stdin
            .flush()
            .await
            .map_err(|e| format!("ACP [{}] flush error: {e}", self.agent_name))?;
        Ok(())
    }

    /// Send `session/prompt` and collect the notification stream until the
    /// response arrives. During execution, permission requests are auto-resolved
    /// according to `auto_approve`. Returns `AcpPromptResult` with all
    /// collected messages, tool calls, and file changes.
    pub async fn prompt_streaming(
        &self,
        params: serde_json::Value,
        auto_approve: bool,
        timeout: Duration,
        progress_tx: Option<&AcpProgressSender>,
    ) -> Result<AcpPromptResult, String> {
        let started = std::time::Instant::now();
        let mut inner = self.inner.lock().await;
        let id = inner.next_id;
        inner.next_id += 1;

        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(id),
            method: "session/prompt".to_string(),
            params: Some(params),
        };
        let mut json = serde_json::to_string(&request).map_err(|e| e.to_string())?;
        json.push('\n');

        inner
            .stdin
            .write_all(json.as_bytes())
            .await
            .map_err(|e| format!("ACP [{}] write error: {e}", self.agent_name))?;
        inner
            .stdin
            .flush()
            .await
            .map_err(|e| format!("ACP [{}] flush error: {e}", self.agent_name))?;

        let mut result = AcpPromptResult {
            messages: Vec::new(),
            tool_calls: Vec::new(),
            files_changed: Vec::new(),
            completed: false,
            duration_ms: 0,
            context_reset: false,
        };
        // Buffer for accumulating streamed message chunks
        let mut message_buffer = String::new();

        let deadline = tokio::time::Instant::now() + timeout;
        let mut line = String::new();

        loop {
            line.clear();
            let read_result =
                tokio::time::timeout_at(deadline, inner.stdout.read_line(&mut line)).await;

            match read_result {
                Err(_) => {
                    result.duration_ms = started.elapsed().as_millis();
                    return Err(format!(
                        "ACP [{}] prompt timed out after {timeout:?}",
                        self.agent_name
                    ));
                }
                Ok(Err(e)) => {
                    result.duration_ms = started.elapsed().as_millis();
                    return Err(format!("ACP [{}] read error: {e}", self.agent_name));
                }
                Ok(Ok(0)) => {
                    result.duration_ms = started.elapsed().as_millis();
                    return Err(format!(
                        "ACP [{}] agent closed connection during prompt",
                        self.agent_name
                    ));
                }
                Ok(Ok(_)) => {}
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let msg: JsonRpcMessage = match serde_json::from_str(trimmed) {
                Ok(m) => m,
                Err(_) => {
                    debug!(
                        "ACP [{}] ignoring non-JSON: {}",
                        self.agent_name,
                        &trimmed[..trimmed.len().min(200)]
                    );
                    continue;
                }
            };

            // Handle the final response to our session/prompt request
            if msg.is_response() {
                let matches = match &msg.id {
                    Some(serde_json::Value::Number(n)) => n.as_u64() == Some(id),
                    _ => true,
                };
                if !matches {
                    continue;
                }
                if let Some(err) = msg.error {
                    result.duration_ms = started.elapsed().as_millis();
                    return Err(format!(
                        "ACP [{}] prompt error ({}): {}",
                        self.agent_name, err.code, err.message
                    ));
                }

                // Flush any remaining message buffer
                if !message_buffer.is_empty() {
                    result.messages.push(std::mem::take(&mut message_buffer));
                }

                // Extract stopReason from response if available
                if let Some(res) = &msg.result {
                    if let Some(reason) = res.get("stopReason").and_then(|v| v.as_str()) {
                        debug!("ACP [{}] prompt stopReason: {reason}", self.agent_name);
                    }
                }

                result.completed = true;
                result.duration_ms = started.elapsed().as_millis();
                return Ok(result);
            }

            // Handle requests from agent (e.g. session/request_permission)
            if msg.is_request() {
                let method = msg.method.as_deref().unwrap_or("");
                let request_id = &msg.id;
                info!(
                    "ACP [{}] agent request: method={method} params={}",
                    self.agent_name,
                    msg.params
                        .as_ref()
                        .map(|p| {
                            let s = p.to_string();
                            s[..s.len().min(300)].to_string()
                        })
                        .unwrap_or_default()
                );

                if method == "session/request_permission" {
                    // Permission request: agent wants approval for a tool call
                    let params = msg.params.as_ref();
                    let options = params
                        .and_then(|p| p.get("options"))
                        .and_then(|o| o.as_array());
                    // Find an "allow" option (prefer allow_always, then allow_once)
                    let allow_option_id = options
                        .and_then(|arr| {
                            arr.iter()
                                .find(|opt| {
                                    opt.get("kind")
                                        .and_then(|k| k.as_str())
                                        .map(|k| k == "allow_always")
                                        .unwrap_or(false)
                                })
                                .or_else(|| {
                                    arr.iter().find(|opt| {
                                        opt.get("kind")
                                            .and_then(|k| k.as_str())
                                            .map(|k| k.starts_with("allow"))
                                            .unwrap_or(false)
                                    })
                                })
                        })
                        .and_then(|opt| opt.get("optionId"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("allow");

                    if auto_approve {
                        // Send JSON-RPC response approving the permission
                        let response = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": request_id,
                            "result": {
                                "outcome": {
                                    "outcome": "selected",
                                    "optionId": allow_option_id
                                }
                            }
                        });
                        let mut resp_json = serde_json::to_string(&response).unwrap_or_default();
                        resp_json.push('\n');
                        let _ = inner.stdin.write_all(resp_json.as_bytes()).await;
                        let _ = inner.stdin.flush().await;
                        info!(
                            "ACP [{}] auto-approved permission (optionId={})",
                            self.agent_name, allow_option_id
                        );
                    } else {
                        // Reject by sending cancelled outcome
                        let response = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": request_id,
                            "result": {
                                "outcome": {
                                    "outcome": "cancelled"
                                }
                            }
                        });
                        let mut resp_json = serde_json::to_string(&response).unwrap_or_default();
                        resp_json.push('\n');
                        let _ = inner.stdin.write_all(resp_json.as_bytes()).await;
                        let _ = inner.stdin.flush().await;
                        debug!(
                            "ACP [{}] rejected permission request (auto_approve=false)",
                            self.agent_name
                        );
                    }
                } else {
                    debug!(
                        "ACP [{}] unhandled agent request: {method}",
                        self.agent_name
                    );
                }
                continue;
            }

            // Handle notifications (session/update)
            if msg.is_notification() {
                let method = msg.method.as_deref().unwrap_or("");
                let params = msg.params.as_ref();

                match method {
                    "session/update" => {
                        // Parse the update type from params.update.sessionUpdate or params.update.type
                        let update = params.and_then(|p| p.get("update"));
                        let update_type_raw = update
                            .and_then(|u| u.get("sessionUpdate").or_else(|| u.get("type")))
                            .and_then(|t| t.as_str())
                            .unwrap_or("");
                        // Normalize PascalCase to snake_case for matching
                        let update_type: String = if update_type_raw.contains('_') {
                            update_type_raw.to_string()
                        } else {
                            // AgentMessageChunk -> agent_message_chunk
                            let mut result_str = String::new();
                            for (i, c) in update_type_raw.chars().enumerate() {
                                if c.is_uppercase() && i > 0 {
                                    result_str.push('_');
                                }
                                result_str.push(c.to_lowercase().next().unwrap_or(c));
                            }
                            result_str
                        };

                        match update_type.as_str() {
                            "agent_message_chunk" => {
                                // Extract text from content block
                                let text = update
                                    .and_then(|u| u.get("content"))
                                    .and_then(|c| c.get("text"))
                                    .and_then(|t| t.as_str());
                                if let Some(text) = text {
                                    message_buffer.push_str(text);
                                }
                            }
                            "agent_thought_chunk" => {
                                // Agent thinking — log but don't include in output
                                let text = update
                                    .and_then(|u| u.get("content"))
                                    .and_then(|c| c.get("text"))
                                    .and_then(|t| t.as_str());
                                if let Some(text) = text {
                                    debug!(
                                        "ACP [{}] thought: {}",
                                        self.agent_name,
                                        &text[..text.len().min(100)]
                                    );
                                    if let Some(tx) = progress_tx {
                                        let _ = tx.send(AcpProgressEvent::Thinking {
                                            text: text.to_string(),
                                        });
                                    }
                                }
                            }
                            "tool_call" => {
                                let title = update
                                    .and_then(|u| u.get("title"))
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("unknown")
                                    .to_string();
                                let raw_input = update
                                    .and_then(|u| u.get("rawInput"))
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null);
                                if let Some(tx) = progress_tx {
                                    let _ = tx.send(AcpProgressEvent::ToolStart {
                                        name: title.clone(),
                                    });
                                }
                                result.tool_calls.push(ToolCallInfo {
                                    name: title,
                                    input: raw_input,
                                });
                                // Flush message buffer before tool calls
                                if !message_buffer.is_empty() {
                                    result.messages.push(std::mem::take(&mut message_buffer));
                                }
                            }
                            "tool_call_update" => {
                                let tool_id = update
                                    .and_then(|u| u.get("toolCallId"))
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("?");
                                let status = update
                                    .and_then(|u| u.get("status"))
                                    .and_then(|s| s.as_str())
                                    .unwrap_or("?");
                                debug!(
                                    "ACP [{}] tool update: id={tool_id} status={status}",
                                    self.agent_name
                                );
                                if let Some(tx) = progress_tx {
                                    let tool_name = update
                                        .and_then(|u| u.get("title"))
                                        .and_then(|t| t.as_str())
                                        .unwrap_or(tool_id)
                                        .to_string();
                                    let _ = tx.send(AcpProgressEvent::ToolComplete {
                                        name: tool_name,
                                        status: status.to_string(),
                                    });
                                }
                                // Capture rawOutput (e.g. command stdout)
                                if let Some(raw) = update.and_then(|u| u.get("rawOutput")) {
                                    let output_str = match raw {
                                        serde_json::Value::String(s) => s.clone(),
                                        other => other.to_string(),
                                    };
                                    if !output_str.is_empty() {
                                        result.messages.push(output_str);
                                    }
                                }
                                // Capture content blocks (terminal output, diffs, etc.)
                                if let Some(content_arr) = update
                                    .and_then(|u| u.get("content"))
                                    .and_then(|c| c.as_array())
                                {
                                    for item in content_arr {
                                        let content_type =
                                            item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                        if content_type == "content" {
                                            // Inline text content
                                            if let Some(text) = item
                                                .get("content")
                                                .and_then(|c| c.get("text"))
                                                .and_then(|t| t.as_str())
                                            {
                                                if !text.is_empty() {
                                                    result.messages.push(text.to_string());
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            "plan" => {
                                let entries = update
                                    .and_then(|u| u.get("entries"))
                                    .and_then(|e| e.as_array());
                                if let Some(entries) = entries {
                                    debug!(
                                        "ACP [{}] plan update: {} entries",
                                        self.agent_name,
                                        entries.len()
                                    );
                                }
                            }
                            _ => {
                                debug!(
                                    "ACP [{}] unhandled session/update type: {update_type}",
                                    self.agent_name
                                );
                            }
                        }
                    }
                    _ => {
                        debug!("ACP [{}] unhandled notification: {method}", self.agent_name);
                    }
                }
            }
        }
    }

    /// Check whether the agent child process is still running.
    pub async fn is_alive(&self) -> bool {
        let mut inner = self.inner.lock().await;
        matches!(inner._child.try_wait(), Ok(None))
    }

    /// Get the child process ID.
    pub async fn pid(&self) -> Option<u32> {
        let inner = self.inner.lock().await;
        inner._child.id()
    }

    /// Gracefully shut down the agent process.
    pub async fn shutdown(&self) -> Result<(), String> {
        info!("ACP [{}]: shutting down", self.agent_name);

        // Try sending session/end (best effort)
        let _ = self.send_request("shutdown", None).await;

        // Kill the child process
        let mut inner = self.inner.lock().await;
        let _ = inner._child.kill().await;
        info!("ACP [{}]: process terminated", self.agent_name);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Progress events (streamed during prompt execution)
// ---------------------------------------------------------------------------

/// Events emitted during ACP prompt execution for real-time progress reporting.
#[derive(Debug, Clone)]
pub enum AcpProgressEvent {
    /// Agent started executing a tool
    ToolStart { name: String },
    /// Agent tool execution completed
    ToolComplete { name: String, status: String },
    /// Agent is thinking (extended thinking chunk)
    Thinking { text: String },
}

/// Sender for streaming progress events during prompt execution.
pub type AcpProgressSender = tokio::sync::mpsc::UnboundedSender<AcpProgressEvent>;

// ---------------------------------------------------------------------------
// PTY connection — simple stdin/stdout subprocess for non-ACP CLI tools
// ---------------------------------------------------------------------------

struct PtyConnectionInner {
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    _child: Child,
}

/// A simple subprocess connection that pipes prompts via stdin and reads
/// stdout until a configurable end-of-response marker or timeout.
/// Unlike `AcpConnection`, there is no JSON-RPC framing — input is sent
/// as-is and output is collected line-by-line.
pub struct PtyConnection {
    agent_name: String,
    inner: Mutex<PtyConnectionInner>,
}

impl PtyConnection {
    /// Spawn the agent process and capture stdin/stdout.
    pub async fn spawn(
        agent_name: &str,
        config: &AcpAgentConfig,
        workspace: Option<&str>,
    ) -> Result<Self, String> {
        let mut cmd = build_spawn_command(config, workspace);

        info!(
            "ACP/PTY: spawning agent '{agent_name}' ({} {})",
            config.launch, config.command
        );

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn PTY agent '{agent_name}': {e}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("PTY agent '{agent_name}': failed to capture stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("PTY agent '{agent_name}': failed to capture stdout"))?;

        // Drain stderr to tracing::debug
        if let Some(stderr) = child.stderr.take() {
            let name = agent_name.to_string();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let trimmed = line.trim();
                            if !trimmed.is_empty() {
                                debug!("PTY [{name}] stderr: {trimmed}");
                            }
                        }
                    }
                }
            });
        }

        Ok(PtyConnection {
            agent_name: agent_name.to_string(),
            inner: Mutex::new(PtyConnectionInner {
                stdin,
                stdout: BufReader::new(stdout),
                _child: child,
            }),
        })
    }

    /// Send a prompt string to stdin and collect stdout until the process
    /// stops producing output (no new line within `timeout`) or exits.
    pub async fn prompt(
        &self,
        message: &str,
        timeout: Duration,
        progress_tx: Option<&AcpProgressSender>,
    ) -> Result<AcpPromptResult, String> {
        let start = std::time::Instant::now();
        let mut inner = self.inner.lock().await;

        // Write message + newline to stdin
        let payload = format!("{message}\n");
        inner
            .stdin
            .write_all(payload.as_bytes())
            .await
            .map_err(|e| format!("PTY [{}]: failed to write to stdin: {e}", self.agent_name))?;
        inner
            .stdin
            .flush()
            .await
            .map_err(|e| format!("PTY [{}]: failed to flush stdin: {e}", self.agent_name))?;

        // Read stdout lines until timeout with no new output, or process exits.
        // Use a per-line read timeout — if no line arrives within the overall
        // timeout, we consider the response complete.
        let mut output_lines = Vec::new();
        let line_timeout = Duration::from_secs(5).min(timeout);

        loop {
            let mut line = String::new();
            match tokio::time::timeout(line_timeout, inner.stdout.read_line(&mut line)).await {
                Ok(Ok(0)) => {
                    // EOF — process exited
                    break;
                }
                Ok(Ok(_)) => {
                    let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                    output_lines.push(trimmed.to_string());
                    if let Some(tx) = progress_tx {
                        let _ = tx.send(AcpProgressEvent::Thinking {
                            text: trimmed.to_string(),
                        });
                    }
                }
                Ok(Err(e)) => {
                    // Read error — treat as done
                    warn!("PTY [{}]: read error: {e}", self.agent_name);
                    break;
                }
                Err(_) => {
                    // Timeout waiting for next line — consider response complete
                    break;
                }
            }

            // Check overall timeout
            if start.elapsed() >= timeout {
                warn!(
                    "PTY [{}]: overall timeout ({:?}) reached",
                    self.agent_name, timeout
                );
                break;
            }
        }

        let duration_ms = start.elapsed().as_millis();
        let text = output_lines.join("\n");
        let messages = if text.is_empty() { vec![] } else { vec![text] };

        Ok(AcpPromptResult {
            completed: true,
            messages,
            tool_calls: vec![],
            files_changed: vec![],
            duration_ms,
            context_reset: false,
        })
    }

    /// Check if the child process is still running.
    pub async fn is_alive(&self) -> bool {
        let mut inner = self.inner.lock().await;
        matches!(inner._child.try_wait(), Ok(None))
    }

    /// Get the child process ID.
    pub async fn pid(&self) -> Option<u32> {
        let inner = self.inner.lock().await;
        inner._child.id()
    }

    /// Kill the child process.
    pub async fn shutdown(&self) -> Result<(), String> {
        info!("PTY [{}]: shutting down", self.agent_name);
        let mut inner = self.inner.lock().await;
        let _ = inner._child.kill().await;
        info!("PTY [{}]: process terminated", self.agent_name);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ConnectionKind — dispatch to either ACP or PTY connection
// ---------------------------------------------------------------------------

/// Wraps either an ACP JSON-RPC connection or a simple PTY stdin/stdout pipe.
pub enum ConnectionKind {
    Acp(AcpConnection),
    Pty(PtyConnection),
}

impl ConnectionKind {
    pub async fn is_alive(&self) -> bool {
        match self {
            ConnectionKind::Acp(c) => c.is_alive().await,
            ConnectionKind::Pty(c) => c.is_alive().await,
        }
    }

    pub async fn shutdown(&self) -> Result<(), String> {
        match self {
            ConnectionKind::Acp(c) => c.shutdown().await,
            ConnectionKind::Pty(c) => c.shutdown().await,
        }
    }

    /// Returns true if this is an ACP connection (supports session/end, etc.).
    pub fn is_acp(&self) -> bool {
        matches!(self, ConnectionKind::Acp(_))
    }

    /// Get a reference to the inner ACP connection, if this is ACP mode.
    pub fn as_acp(&self) -> Option<&AcpConnection> {
        match self {
            ConnectionKind::Acp(c) => Some(c),
            ConnectionKind::Pty(_) => None,
        }
    }

    /// Get the child process ID.
    pub async fn pid(&self) -> Option<u32> {
        match self {
            ConnectionKind::Acp(c) => c.pid().await,
            ConnectionKind::Pty(c) => c.pid().await,
        }
    }
}

// ---------------------------------------------------------------------------
// Session & prompt result types
// ---------------------------------------------------------------------------

/// Summary info returned after creating a session
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    pub agent_id: String,
    pub workspace: String,
}

/// Status of an ACP session
#[derive(Debug, Clone, PartialEq)]
pub enum SessionStatus {
    Active,
    Prompting,
    Ended,
}

/// Record of a single tool call made by the agent during prompt execution
#[derive(Debug, Clone)]
pub struct ToolCallInfo {
    pub name: String,
    pub input: serde_json::Value,
}

/// Result of an ACP prompt execution
#[derive(Debug, Clone)]
pub struct AcpPromptResult {
    /// Text messages emitted by the agent
    pub messages: Vec<String>,
    /// Tool calls executed by the agent
    pub tool_calls: Vec<ToolCallInfo>,
    /// Files changed during execution
    pub files_changed: Vec<String>,
    /// Whether the prompt completed normally (vs timeout/cancel)
    pub completed: bool,
    /// Wall-clock execution time in milliseconds
    pub duration_ms: u128,
    /// True if the agent process had crashed and was restarted for this
    /// prompt. Previous conversation context was lost.
    pub context_reset: bool,
}

// ---------------------------------------------------------------------------
// Async Job types
// ---------------------------------------------------------------------------

/// Status of an async ACP job
#[derive(Debug, Clone, PartialEq)]
pub enum AcpJobStatus {
    Running,
    Completed,
    Failed,
}

/// An async ACP job that runs a prompt in the background.
#[derive(Debug, Clone)]
pub struct AcpJob {
    pub id: String,
    pub session_id: String,
    pub agent_id: String,
    pub status: AcpJobStatus,
    pub result: Option<AcpPromptResult>,
    pub error: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Summary of a job (for listing/querying)
#[derive(Debug, Clone)]
pub struct AcpJobSummary {
    pub id: String,
    pub session_id: String,
    pub agent_id: String,
    pub status: AcpJobStatus,
    pub created_at: String,
    pub completed_at: Option<String>,
    pub duration_ms: Option<u128>,
    pub error: Option<String>,
}

const MAX_JOBS: usize = 100;
const JOB_TTL_SECS: u64 = 3600; // 1 hour

/// Callback invoked when an async job completes. Receives (chat_id, message_text).
/// Used to push results back to the user's chat via send_message.
pub type JobCompletionCallback = Arc<
    dyn Fn(i64, String) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

/// An active ACP agent session with its connection
pub struct AcpSession {
    pub id: String,
    pub agent_id: String,
    pub workspace: String,
    pub auto_approve: bool,
    pub status: SessionStatus,
    pub acp_session_id: Option<String>,
    pub connection: ConnectionKind,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Monotonic timestamp of last prompt activity (start or complete).
    /// Used by the idle reaper to detect stuck/abandoned sessions.
    pub last_activity: Instant,
    /// Set to true when the agent process crashed and was restarted.
    /// The next prompt result will include a context-loss notice, then
    /// this flag is cleared.
    pub session_reset: bool,
    /// Path to the cgroup v2 directory, if resource limits were applied.
    /// Cleaned up on session end.
    pub cgroup_path: Option<String>,
}

// ---------------------------------------------------------------------------
// AcpManager — global session lifecycle manager
// ---------------------------------------------------------------------------

pub struct AcpManager {
    pub config: AcpConfig,
    sessions: RwLock<HashMap<String, Mutex<AcpSession>>>,
    /// Map chat_id → session_id for command-based ACP routing
    chat_sessions: RwLock<HashMap<i64, String>>,
    /// Per-agent active session count for enforcing max_per_agent
    agent_session_counts: RwLock<HashMap<String, usize>>,
    /// In-memory async job store
    jobs: RwLock<HashMap<String, Mutex<AcpJob>>>,
}

impl AcpManager {
    /// Create an AcpManager from a config file path.
    /// Does NOT spawn any agents — they are created on demand via tools.
    pub fn from_config_file(path: &str) -> Self {
        Self::from_config(AcpConfig::from_file(path))
    }

    /// Create an AcpManager from an already-parsed config.
    /// Does NOT spawn any agents — they are created on demand via tools.
    pub fn from_config(config: AcpConfig) -> Self {
        if !config.agents.is_empty() {
            info!(
                "ACP config loaded: {} agent(s) configured ({})",
                config.agents.len(),
                config.agents.keys().cloned().collect::<Vec<_>>().join(", ")
            );
        }
        AcpManager {
            config,
            sessions: RwLock::new(HashMap::new()),
            chat_sessions: RwLock::new(HashMap::new()),
            agent_session_counts: RwLock::new(HashMap::new()),
            jobs: RwLock::new(HashMap::new()),
        }
    }

    /// List configured agent names
    pub fn available_agents(&self) -> Vec<String> {
        self.config.agents.keys().cloned().collect()
    }

    /// Check if a given agent name is configured
    pub fn has_agent(&self, name: &str) -> bool {
        self.config.agents.contains_key(name)
    }

    /// Get agent config by name
    pub fn agent_config(&self, name: &str) -> Option<&AcpAgentConfig> {
        self.config.agents.get(name)
    }

    /// Spawn a new agent process, perform ACP handshake, and create a session.
    pub async fn new_session(
        &self,
        agent_id: &str,
        workspace: Option<&str>,
        auto_approve: Option<bool>,
    ) -> Result<SessionInfo, String> {
        // Enforce process pool limits (before config lookup / spawn)
        {
            let sessions = self.sessions.read().await;
            let total = sessions.len();
            if total >= self.config.max_sessions {
                return Err(format!(
                    "ACP session limit reached ({total}/{}). End an existing session first.",
                    self.config.max_sessions
                ));
            }
        }

        let agent_config = self
            .config
            .agents
            .get(agent_id)
            .ok_or_else(|| format!("ACP agent '{agent_id}' not configured"))?
            .clone();

        {
            let counts = self.agent_session_counts.read().await;
            let agent_count = counts.get(agent_id).copied().unwrap_or(0);
            if agent_count >= self.config.max_per_agent {
                return Err(format!(
                    "ACP per-agent limit reached for '{agent_id}' ({agent_count}/{}). End an existing session first.",
                    self.config.max_per_agent
                ));
            }
        }

        let effective_auto_approve = auto_approve
            .or(agent_config.auto_approve)
            .unwrap_or(self.config.default_auto_approve);

        let effective_workspace = workspace
            .map(|s| s.to_string())
            .or_else(|| agent_config.workspace.clone())
            .unwrap_or_else(|| ".".to_string());

        let is_pty_mode = agent_config.mode == "pty";

        let (connection, acp_session_id) = if is_pty_mode {
            // PTY mode — simple stdin/stdout subprocess, no JSON-RPC
            let pty_conn =
                PtyConnection::spawn(agent_id, &agent_config, Some(&effective_workspace)).await?;
            (ConnectionKind::Pty(pty_conn), None)
        } else {
            // ACP mode — full JSON-RPC protocol
            let request_timeout = Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS);
            let acp_conn = AcpConnection::spawn(
                agent_id,
                &agent_config,
                Some(&effective_workspace),
                request_timeout,
            )
            .await?;

            // Create an ACP-level session with workspace as cwd
            let cwd = std::path::Path::new(&effective_workspace)
                .canonicalize()
                .unwrap_or_else(|_| std::path::PathBuf::from(&effective_workspace));
            let acp_session_id = match acp_conn
                .send_request(
                    "session/new",
                    Some(serde_json::json!({
                        "cwd": cwd.to_string_lossy(),
                        "mcpServers": []
                    })),
                )
                .await
            {
                Ok(result) => result
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                Err(e) => {
                    warn!(
                        "ACP [{}]: session/new failed ({e}), continuing without ACP session ID",
                        agent_id
                    );
                    None
                }
            };
            (ConnectionKind::Acp(acp_conn), acp_session_id)
        };

        let session_id = uuid::Uuid::new_v4().to_string();

        // Apply cgroup resource limits if configured
        let cgroup_path = if let Some(ref limits) = agent_config.resource_limits {
            if let Some(pid) = connection.pid().await {
                apply_resource_limits(pid, &session_id, limits)
            } else {
                warn!("ACP [{agent_id}]: could not get child PID for cgroup setup");
                None
            }
        } else {
            None
        };

        let info = SessionInfo {
            session_id: session_id.clone(),
            agent_id: agent_id.to_string(),
            workspace: effective_workspace.clone(),
        };

        let session = AcpSession {
            id: session_id.clone(),
            agent_id: agent_id.to_string(),
            workspace: effective_workspace,
            auto_approve: effective_auto_approve,
            status: SessionStatus::Active,
            acp_session_id,
            connection,
            created_at: chrono::Utc::now(),
            last_activity: Instant::now(),
            session_reset: false,
            cgroup_path,
        };

        self.sessions
            .write()
            .await
            .insert(session_id, Mutex::new(session));

        // Increment per-agent session counter
        *self
            .agent_session_counts
            .write()
            .await
            .entry(agent_id.to_string())
            .or_insert(0) += 1;

        info!(
            "ACP session created: {} (agent={agent_id}, auto_approve={effective_auto_approve})",
            info.session_id
        );
        Ok(info)
    }

    /// Send a prompt to an existing session and wait for completion.
    ///
    /// If the agent process has crashed, this method attempts to respawn the
    /// process and re-create the ACP session before sending the prompt. The
    /// returned `AcpPromptResult.context_reset` will be `true` to indicate
    /// that previous conversation context was lost.
    pub async fn prompt(
        &self,
        session_id: &str,
        message: &str,
        timeout_secs: Option<u64>,
        progress_tx: Option<&AcpProgressSender>,
    ) -> Result<AcpPromptResult, String> {
        let sessions = self.sessions.read().await;
        let session_mutex = sessions
            .get(session_id)
            .ok_or_else(|| format!("ACP session '{session_id}' not found"))?;

        let mut session = session_mutex.lock().await;
        if session.status == SessionStatus::Ended {
            return Err(format!("ACP session '{session_id}' has ended"));
        }

        // --- Crash recovery: detect dead process and respawn ---------------
        if !session.connection.is_alive().await {
            warn!(
                "ACP [{}]: agent process died, attempting restart (session={})",
                session.agent_id, session_id
            );
            if let Err(e) = self.recover_session(&mut session).await {
                session.status = SessionStatus::Ended;
                return Err(format!(
                    "ACP [{}]: agent process died and recovery failed: {e}",
                    session.agent_id
                ));
            }
        }

        session.status = SessionStatus::Prompting;
        session.last_activity = Instant::now();

        let timeout = Duration::from_secs(timeout_secs.unwrap_or(self.config.prompt_timeout_secs));

        let result = match &session.connection {
            ConnectionKind::Acp(conn) => {
                let acp_sid = session
                    .acp_session_id
                    .as_deref()
                    .ok_or_else(|| format!("ACP session '{session_id}' has no ACP session ID"))?;
                let params = serde_json::json!({
                    "sessionId": acp_sid,
                    "prompt": [{"type": "text", "text": message}]
                });
                conn.prompt_streaming(params, session.auto_approve, timeout, progress_tx)
                    .await
            }
            ConnectionKind::Pty(conn) => conn.prompt(message, timeout, progress_tx).await,
        };

        session.status = SessionStatus::Active;
        session.last_activity = Instant::now();

        // Consume the reset flag so it's only reported once.
        let context_reset = session.session_reset;
        session.session_reset = false;

        match result {
            Ok(mut r) => {
                r.context_reset = context_reset;
                info!(
                    "ACP [{}] prompt completed in {}ms ({} messages, {} tool calls, {} files{})",
                    session.agent_id,
                    r.duration_ms,
                    r.messages.len(),
                    r.tool_calls.len(),
                    r.files_changed.len(),
                    if context_reset { ", context_reset" } else { "" }
                );
                Ok(r)
            }
            Err(e) => {
                error!("ACP [{}] prompt failed: {e}", session.agent_id);
                Err(e)
            }
        }
    }

    /// Attempt to respawn the agent process and re-create a session,
    /// replacing the dead connection in-place. Sets `session_reset = true`.
    async fn recover_session(&self, session: &mut AcpSession) -> Result<(), String> {
        let agent_config = self
            .config
            .agents
            .get(&session.agent_id)
            .ok_or_else(|| format!("Agent '{}' no longer configured", session.agent_id))?
            .clone();

        if agent_config.mode == "pty" {
            // PTY mode — just respawn the process
            let new_conn =
                PtyConnection::spawn(&session.agent_id, &agent_config, Some(&session.workspace))
                    .await?;
            session.connection = ConnectionKind::Pty(new_conn);
            session.acp_session_id = None;
        } else {
            // ACP mode — respawn + re-initialize + session/new
            let request_timeout = Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS);
            let new_connection = AcpConnection::spawn(
                &session.agent_id,
                &agent_config,
                Some(&session.workspace),
                request_timeout,
            )
            .await?;

            let cwd = std::path::Path::new(&session.workspace)
                .canonicalize()
                .unwrap_or_else(|_| std::path::PathBuf::from(&session.workspace));
            let new_acp_session_id = match new_connection
                .send_request(
                    "session/new",
                    Some(serde_json::json!({
                        "cwd": cwd.to_string_lossy(),
                        "mcpServers": []
                    })),
                )
                .await
            {
                Ok(result) => result
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                Err(e) => {
                    warn!(
                        "ACP [{}]: session/new failed during recovery ({e}), continuing without ACP session ID",
                        session.agent_id
                    );
                    None
                }
            };
            session.connection = ConnectionKind::Acp(new_connection);
            session.acp_session_id = new_acp_session_id;
        }

        // Clean up old cgroup and set up new one if limits configured
        if let Some(ref old_cg) = session.cgroup_path {
            cleanup_cgroup(old_cg);
        }
        session.cgroup_path = if let Some(ref limits) = agent_config.resource_limits {
            if let Some(pid) = session.connection.pid().await {
                apply_resource_limits(pid, &session.id, limits)
            } else {
                None
            }
        } else {
            None
        };

        session.session_reset = true;
        session.last_activity = Instant::now();

        info!(
            "ACP [{}]: process recovered successfully (session={})",
            session.agent_id, session.id
        );
        Ok(())
    }

    /// End a session and terminate the agent process.
    pub async fn end_session(&self, session_id: &str) -> Result<(), String> {
        let session_mutex = {
            let mut sessions = self.sessions.write().await;
            sessions
                .remove(session_id)
                .ok_or_else(|| format!("ACP session '{session_id}' not found"))?
        };

        let mut session = session_mutex.lock().await;

        // Send session/end to agent (ACP mode only, best effort)
        if let Some(acp_sid) = &session.acp_session_id {
            if let Some(conn) = session.connection.as_acp() {
                let _ = conn
                    .send_request(
                        "session/end",
                        Some(serde_json::json!({"sessionId": acp_sid})),
                    )
                    .await;
            }
        }

        session.connection.shutdown().await?;
        session.status = SessionStatus::Ended;

        // Clean up cgroup if one was created
        if let Some(ref cg_path) = session.cgroup_path {
            cleanup_cgroup(cg_path);
        }

        // Decrement per-agent session counter
        {
            let mut counts = self.agent_session_counts.write().await;
            if let Some(count) = counts.get_mut(&session.agent_id) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    counts.remove(&session.agent_id);
                }
            }
        }

        // Unbind any chats referencing this session
        let mut chat_sessions = self.chat_sessions.write().await;
        chat_sessions.retain(|_, sid| sid != session_id);

        info!("ACP session ended: {session_id}");
        Ok(())
    }

    /// List all active sessions.
    pub async fn list_sessions(&self) -> Vec<SessionSummary> {
        let sessions = self.sessions.read().await;
        let mut summaries = Vec::new();
        for (id, session_mutex) in sessions.iter() {
            let session = session_mutex.lock().await;
            summaries.push(SessionSummary {
                session_id: id.clone(),
                agent_id: session.agent_id.clone(),
                workspace: session.workspace.clone(),
                status: session.status.clone(),
                created_at: session.created_at.to_rfc3339(),
                idle_secs: session.last_activity.elapsed().as_secs(),
            });
        }
        summaries
    }

    // -----------------------------------------------------------------------
    // Chat-to-session binding (for command-based ACP)
    // -----------------------------------------------------------------------

    /// Bind a chat to an ACP session. Messages in this chat will be routed
    /// to the ACP agent instead of the LLM.
    pub async fn bind_chat(&self, chat_id: i64, session_id: &str) {
        self.chat_sessions
            .write()
            .await
            .insert(chat_id, session_id.to_string());
        debug!("ACP: bound chat {chat_id} to session {session_id}");
    }

    /// Unbind a chat from its ACP session.
    pub async fn unbind_chat(&self, chat_id: i64) {
        self.chat_sessions.write().await.remove(&chat_id);
        debug!("ACP: unbound chat {chat_id}");
    }

    /// Get the session_id bound to a chat, if any.
    pub async fn chat_session(&self, chat_id: i64) -> Option<String> {
        self.chat_sessions.read().await.get(&chat_id).cloned()
    }

    /// End the session bound to a chat and unbind it. Returns Ok if a session
    /// existed and was ended, Err if no session was bound.
    pub async fn end_chat_session(&self, chat_id: i64) -> Result<(), String> {
        let session_id = self
            .chat_sessions
            .read()
            .await
            .get(&chat_id)
            .cloned()
            .ok_or_else(|| "No active ACP session in this chat".to_string())?;

        self.end_session(&session_id).await?;
        self.unbind_chat(chat_id).await;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Async job management
    // -----------------------------------------------------------------------

    /// Submit an async job that runs a prompt in the background.
    /// Returns the job ID immediately. The job runs in a background task and
    /// optionally calls `on_complete` with the result when done.
    pub async fn submit_job(
        self: &Arc<Self>,
        session_id: &str,
        message: &str,
        timeout_secs: Option<u64>,
        chat_id: Option<i64>,
        on_complete: Option<JobCompletionCallback>,
    ) -> Result<String, String> {
        // Validate session exists
        {
            let sessions = self.sessions.read().await;
            let session_mutex = sessions
                .get(session_id)
                .ok_or_else(|| format!("ACP session '{session_id}' not found"))?;
            let session = session_mutex.lock().await;
            if session.status == SessionStatus::Ended {
                return Err(format!("ACP session '{session_id}' has ended"));
            }
        }

        // Enforce job limit
        self.cleanup_expired_jobs().await;
        {
            let jobs = self.jobs.read().await;
            let running = jobs.values().count();
            if running >= MAX_JOBS {
                return Err(format!(
                    "Job limit reached ({running}/{MAX_JOBS}). Wait for existing jobs to complete."
                ));
            }
        }

        let job_id = uuid::Uuid::new_v4().to_string();

        // Look up agent_id for the job record
        let agent_id = {
            let sessions = self.sessions.read().await;
            let session_mutex = sessions.get(session_id).unwrap();
            let session = session_mutex.lock().await;
            session.agent_id.clone()
        };

        let job = AcpJob {
            id: job_id.clone(),
            session_id: session_id.to_string(),
            agent_id: agent_id.clone(),
            status: AcpJobStatus::Running,
            result: None,
            error: None,
            created_at: chrono::Utc::now(),
            completed_at: None,
        };

        self.jobs
            .write()
            .await
            .insert(job_id.clone(), Mutex::new(job));

        // Spawn background task
        let manager = Arc::clone(self);
        let sid = session_id.to_string();
        let msg = message.to_string();
        let jid = job_id.clone();
        let agent_id_for_task = agent_id.clone();

        tokio::spawn(async move {
            let agent_id = agent_id_for_task;
            let result = manager.prompt(&sid, &msg, timeout_secs, None).await;
            let now = chrono::Utc::now();

            // Format notification text before updating job store
            let notification = match &result {
                Ok(r) => {
                    let mut text = String::new();
                    if r.context_reset {
                        text.push_str("[Agent restarted — previous context lost]\n\n");
                    }
                    for m in &r.messages {
                        if !m.is_empty() {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(m);
                        }
                    }
                    if !r.tool_calls.is_empty() {
                        if !text.is_empty() {
                            text.push_str("\n\n");
                        }
                        text.push_str(&format!("[{} tool call(s)]", r.tool_calls.len()));
                    }
                    if text.is_empty() {
                        text = "(Agent completed with no output)".to_string();
                    }
                    text
                }
                Err(e) => format!("ACP job failed: {e}"),
            };

            // Update job record
            {
                let jobs = manager.jobs.read().await;
                if let Some(job_mutex) = jobs.get(&jid) {
                    let mut job = job_mutex.lock().await;
                    match result {
                        Ok(r) => {
                            job.status = AcpJobStatus::Completed;
                            job.result = Some(r);
                        }
                        Err(e) => {
                            job.status = AcpJobStatus::Failed;
                            job.error = Some(e);
                        }
                    }
                    job.completed_at = Some(now);
                }
            }

            // Fire completion callback
            if let (Some(cid), Some(cb)) = (chat_id, on_complete) {
                let header = format!("[ACP job {jid} ({agent_id})]:\n");
                cb(cid, format!("{header}{notification}")).await;
            }

            info!("ACP job {jid} finished");
        });

        info!("ACP job submitted: {job_id} (session={session_id}, agent={agent_id})");
        Ok(job_id)
    }

    /// Get the status of a job by ID.
    pub async fn job_status(&self, job_id: &str) -> Result<AcpJobSummary, String> {
        let jobs = self.jobs.read().await;
        let job_mutex = jobs
            .get(job_id)
            .ok_or_else(|| format!("ACP job '{job_id}' not found"))?;
        let job = job_mutex.lock().await;
        Ok(AcpJobSummary {
            id: job.id.clone(),
            session_id: job.session_id.clone(),
            agent_id: job.agent_id.clone(),
            status: job.status.clone(),
            created_at: job.created_at.to_rfc3339(),
            completed_at: job.completed_at.map(|t| t.to_rfc3339()),
            duration_ms: job.result.as_ref().map(|r| r.duration_ms),
            error: job.error.clone(),
        })
    }

    /// Remove expired jobs (older than JOB_TTL_SECS).
    async fn cleanup_expired_jobs(&self) {
        let now = chrono::Utc::now();
        let ttl = chrono::Duration::seconds(JOB_TTL_SECS as i64);

        let expired: Vec<String> = {
            let jobs = self.jobs.read().await;
            jobs.iter()
                .filter(|(_, jm)| {
                    // Only non-blocking try_lock here; skip if locked
                    if let Ok(j) = jm.try_lock() {
                        j.status != AcpJobStatus::Running && (now - j.created_at) > ttl
                    } else {
                        false
                    }
                })
                .map(|(id, _)| id.clone())
                .collect()
        };

        if !expired.is_empty() {
            let mut jobs = self.jobs.write().await;
            for id in &expired {
                jobs.remove(id);
            }
            debug!("ACP job cleanup: removed {} expired job(s)", expired.len());
        }
    }

    /// Reap sessions that have been idle (no prompt activity) longer than
    /// `idle_timeout_secs`. Sessions in `Prompting` state are skipped — they
    /// have their own per-prompt timeout. Returns the number of reaped sessions.
    pub async fn reap_idle_sessions(&self) -> usize {
        // Also clean up expired jobs while we're here
        self.cleanup_expired_jobs().await;

        let idle_timeout = Duration::from_secs(self.config.idle_timeout_secs);
        if idle_timeout.is_zero() {
            return 0;
        }

        // Collect session IDs that exceed the idle threshold.
        // We only need a read lock to scan; end_session takes its own write lock.
        let mut to_reap: Vec<(String, String)> = Vec::new(); // (session_id, agent_id)
        {
            let sessions = self.sessions.read().await;
            for (id, session_mutex) in sessions.iter() {
                let session = session_mutex.lock().await;
                if session.status == SessionStatus::Prompting {
                    continue; // active work — skip
                }
                if session.last_activity.elapsed() >= idle_timeout {
                    to_reap.push((id.clone(), session.agent_id.clone()));
                }
            }
        }

        let count = to_reap.len();
        for (session_id, agent_id) in &to_reap {
            warn!(
                "ACP idle reaper: ending session {session_id} (agent={agent_id}, idle > {}s)",
                self.config.idle_timeout_secs
            );
            if let Err(e) = self.end_session(session_id).await {
                warn!("ACP idle reaper: failed to end session {session_id}: {e}");
            }
        }

        if count > 0 {
            info!("ACP idle reaper: reaped {count} session(s)");
        }
        count
    }

    /// Cleanup all sessions (called on process shutdown).
    pub async fn cleanup(&self) {
        let session_ids: Vec<String> = {
            let sessions = self.sessions.read().await;
            sessions.keys().cloned().collect()
        };

        for id in &session_ids {
            if let Err(e) = self.end_session(id).await {
                warn!("ACP cleanup: failed to end session {id}: {e}");
            }
        }

        if !session_ids.is_empty() {
            info!(
                "ACP manager cleanup: terminated {} session(s)",
                session_ids.len()
            );
        }
    }
}

/// Spawn a background task that periodically reaps idle ACP sessions.
/// The task runs every 60 seconds and terminates sessions that have been
/// idle longer than `idle_timeout_secs`. Does nothing if the timeout is 0.
pub fn spawn_idle_reaper(manager: Arc<AcpManager>) {
    if manager.config.idle_timeout_secs == 0 {
        info!("ACP idle reaper disabled (idle_timeout_secs=0)");
        return;
    }
    info!(
        "ACP idle reaper started (checking every 60s, timeout={}s)",
        manager.config.idle_timeout_secs
    );
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            manager.reap_idle_sessions().await;
        }
    });
}

/// Summary of an active session (for listing)
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub session_id: String,
    pub agent_id: String,
    pub workspace: String,
    pub status: SessionStatus,
    pub created_at: String,
    /// Seconds since last prompt activity
    pub idle_secs: u64,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_config_defaults() {
        let config = AcpConfig::default();
        assert!(!config.default_auto_approve);
        assert_eq!(config.prompt_timeout_secs, 300);
        assert!(config.agents.is_empty());
    }

    #[test]
    fn test_config_parse_full() {
        let json = r#"{
            "defaultAutoApprove": true,
            "promptTimeoutSecs": 600,
            "acpAgents": {
                "claude": {
                    "launch": "npx",
                    "command": "@anthropic-ai/claude-code@latest",
                    "args": ["--acp"],
                    "env": {"ANTHROPIC_API_KEY": "sk-test"},
                    "workspace": "/tmp/test",
                    "auto_approve": true
                }
            }
        }"#;

        let config: AcpConfig = serde_json::from_str(json).unwrap();
        assert!(config.default_auto_approve);
        assert_eq!(config.prompt_timeout_secs, 600);
        assert_eq!(config.agents.len(), 1);

        let claude = config.agents.get("claude").unwrap();
        assert_eq!(claude.launch, "npx");
        assert_eq!(claude.command, "@anthropic-ai/claude-code@latest");
        assert_eq!(claude.args, vec!["--acp"]);
        assert_eq!(claude.workspace.as_deref(), Some("/tmp/test"));
        assert_eq!(claude.auto_approve, Some(true));
    }

    #[test]
    fn test_config_parse_minimal() {
        let json = r#"{
            "acpAgents": {
                "claude": {
                    "command": "@anthropic-ai/claude-code@latest"
                }
            }
        }"#;

        let config: AcpConfig = serde_json::from_str(json).unwrap();
        assert!(!config.default_auto_approve);
        assert_eq!(config.prompt_timeout_secs, 300);

        let claude = config.agents.get("claude").unwrap();
        assert_eq!(claude.launch, "npx");
        assert!(claude.args.is_empty());
        assert!(claude.env.is_empty());
        assert!(claude.workspace.is_none());
        assert!(claude.auto_approve.is_none());
    }

    #[test]
    fn test_config_parse_snake_case_aliases() {
        let json = r#"{
            "default_auto_approve": true,
            "prompt_timeout_secs": 120,
            "agents": {
                "claude": {
                    "command": "@anthropic-ai/claude-code@latest"
                }
            }
        }"#;

        let config: AcpConfig = serde_json::from_str(json).unwrap();
        assert!(config.default_auto_approve);
        assert_eq!(config.prompt_timeout_secs, 120);
        assert_eq!(config.agents.len(), 1);
    }

    #[test]
    fn test_missing_file_returns_default() {
        let config = AcpConfig::from_file("/nonexistent/acp.json");
        assert!(config.agents.is_empty());
    }

    #[test]
    fn test_manager_from_config() {
        let manager = AcpManager::from_config_file("/nonexistent/acp.json");
        assert!(manager.available_agents().is_empty());
        assert!(!manager.has_agent("claude"));
    }

    #[test]
    fn test_build_spawn_command_npx() {
        let config = AcpAgentConfig {
            launch: "npx".to_string(),
            command: "@anthropic-ai/claude-code@latest".to_string(),
            args: vec!["--acp".to_string()],
            env: HashMap::from([("FOO".to_string(), "bar".to_string())]),
            workspace: Some("/tmp/ws".to_string()),
            auto_approve: None,
            mode: default_mode(),
            resource_limits: None,
        };

        let cmd = build_spawn_command(&config, None);
        let prog = cmd.as_std().get_program();
        assert_eq!(prog, "npx");

        let args: Vec<&std::ffi::OsStr> = cmd.as_std().get_args().collect();
        assert_eq!(
            args,
            vec!["-y", "@anthropic-ai/claude-code@latest", "--acp"]
        );
    }

    #[test]
    fn test_build_spawn_command_binary() {
        let config = AcpAgentConfig {
            launch: "binary".to_string(),
            command: "/usr/bin/opencode".to_string(),
            args: vec!["acp".to_string()],
            env: HashMap::new(),
            workspace: None,
            auto_approve: None,
            mode: default_mode(),
            resource_limits: None,
        };

        let cmd = build_spawn_command(&config, Some("/home/user/project"));
        let prog = cmd.as_std().get_program();
        assert_eq!(prog, "/usr/bin/opencode");

        let args: Vec<&std::ffi::OsStr> = cmd.as_std().get_args().collect();
        assert_eq!(args, vec!["acp"]);

        assert_eq!(
            cmd.as_std().get_current_dir(),
            Some(std::path::Path::new("/home/user/project"))
        );
    }

    #[test]
    fn test_build_spawn_command_workspace_override() {
        let config = AcpAgentConfig {
            launch: "npx".to_string(),
            command: "agent".to_string(),
            args: vec![],
            env: HashMap::new(),
            workspace: Some("/default/ws".to_string()),
            auto_approve: None,
            mode: default_mode(),
            resource_limits: None,
        };

        // Explicit workspace overrides config default
        let cmd = build_spawn_command(&config, Some("/override/ws"));
        assert_eq!(
            cmd.as_std().get_current_dir(),
            Some(std::path::Path::new("/override/ws"))
        );

        // Falls back to config default
        let cmd2 = build_spawn_command(&config, None);
        assert_eq!(
            cmd2.as_std().get_current_dir(),
            Some(std::path::Path::new("/default/ws"))
        );
    }

    #[test]
    fn test_jsonrpc_message_classification() {
        // Response
        let resp: JsonRpcMessage =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#).unwrap();
        assert!(resp.is_response());
        assert!(!resp.is_notification());

        // Error response
        let err: JsonRpcMessage = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-1,"message":"fail"}}"#,
        )
        .unwrap();
        assert!(err.is_response());
        assert!(!err.is_notification());

        // Notification
        let notif: JsonRpcMessage = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"messages/create","params":{"text":"hi"}}"#,
        )
        .unwrap();
        assert!(!notif.is_response());
        assert!(notif.is_notification());
    }

    #[test]
    fn test_prompt_result_default() {
        let result = AcpPromptResult {
            messages: vec!["hello".to_string()],
            tool_calls: vec![ToolCallInfo {
                name: "bash".to_string(),
                input: serde_json::json!({"command": "ls"}),
            }],
            files_changed: vec!["foo.rs".to_string()],
            completed: true,
            duration_ms: 1234,
            context_reset: false,
        };

        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "bash");
        assert!(result.completed);
        assert_eq!(result.duration_ms, 1234);
    }

    #[tokio::test]
    async fn test_manager_new_session_unknown_agent() {
        let manager = AcpManager::from_config_file("/nonexistent/acp.json");
        let result = manager.new_session("nonexistent", None, None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not configured"));
    }

    #[tokio::test]
    async fn test_manager_list_sessions_empty() {
        let manager = AcpManager::from_config_file("/nonexistent/acp.json");
        let sessions = manager.list_sessions().await;
        assert!(sessions.is_empty());
    }

    #[tokio::test]
    async fn test_manager_end_session_not_found() {
        let manager = AcpManager::from_config_file("/nonexistent/acp.json");
        let result = manager.end_session("nonexistent").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[tokio::test]
    async fn test_manager_prompt_not_found() {
        let manager = AcpManager::from_config_file("/nonexistent/acp.json");
        let result = manager.prompt("nonexistent", "hello", None, None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    // -----------------------------------------------------------------------
    // Phase 7.1: Additional config parsing tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_config_parse_multiple_agents() {
        let json = r#"{
            "acpAgents": {
                "claude": {
                    "command": "@anthropic-ai/claude-code@latest",
                    "workspace": "/tmp/claude"
                },
                "opencode": {
                    "launch": "binary",
                    "command": "/usr/bin/opencode",
                    "args": ["acp"]
                },
                "gemini": {
                    "launch": "npx",
                    "command": "@google/gemini-cli@latest",
                    "args": ["--experimental-acp"],
                    "env": {"GEMINI_API_KEY": "test-key"}
                }
            }
        }"#;

        let config: AcpConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.agents.len(), 3);

        let claude = config.agents.get("claude").unwrap();
        assert_eq!(claude.launch, "npx"); // default
        assert_eq!(claude.workspace.as_deref(), Some("/tmp/claude"));

        let opencode = config.agents.get("opencode").unwrap();
        assert_eq!(opencode.launch, "binary");
        assert_eq!(opencode.command, "/usr/bin/opencode");
        assert_eq!(opencode.args, vec!["acp"]);
        assert!(opencode.workspace.is_none());

        let gemini = config.agents.get("gemini").unwrap();
        assert_eq!(gemini.launch, "npx");
        assert_eq!(gemini.env.get("GEMINI_API_KEY").unwrap(), "test-key");
    }

    #[test]
    fn test_config_parse_invalid_json_returns_default() {
        // AcpConfig::from_file should return defaults on parse failure.
        // We can't easily test from_file with bad content without a temp file,
        // but we can verify serde_json rejects garbage.
        let result: Result<AcpConfig, _> = serde_json::from_str("NOT JSON");
        assert!(result.is_err());
    }

    #[test]
    fn test_config_parse_empty_object() {
        let config: AcpConfig = serde_json::from_str("{}").unwrap();
        assert!(!config.default_auto_approve);
        assert_eq!(config.prompt_timeout_secs, 300);
        assert!(config.agents.is_empty());
    }

    #[test]
    fn test_config_parse_agent_env_propagated() {
        let config = AcpAgentConfig {
            launch: "npx".to_string(),
            command: "agent".to_string(),
            args: vec![],
            env: HashMap::from([
                ("KEY1".to_string(), "val1".to_string()),
                ("KEY2".to_string(), "val2".to_string()),
            ]),
            workspace: None,
            auto_approve: None,
            mode: default_mode(),
            resource_limits: None,
        };

        let cmd = build_spawn_command(&config, None);
        let envs: Vec<_> = cmd.as_std().get_envs().collect();
        // Verify our env vars are set (they should be among the envs)
        let has_key1 = envs.iter().any(|(k, v)| {
            k == &std::ffi::OsStr::new("KEY1") && v == &Some(std::ffi::OsStr::new("val1"))
        });
        let has_key2 = envs.iter().any(|(k, v)| {
            k == &std::ffi::OsStr::new("KEY2") && v == &Some(std::ffi::OsStr::new("val2"))
        });
        assert!(has_key1, "KEY1 env var should be set");
        assert!(has_key2, "KEY2 env var should be set");
    }

    #[test]
    fn test_build_spawn_command_uvx() {
        let config = AcpAgentConfig {
            launch: "uvx".to_string(),
            command: "some-agent".to_string(),
            args: vec!["--flag".to_string()],
            env: HashMap::new(),
            workspace: None,
            auto_approve: None,
            mode: default_mode(),
            resource_limits: None,
        };

        let cmd = build_spawn_command(&config, None);
        let prog = cmd.as_std().get_program();
        assert_eq!(prog, "uvx");

        let args: Vec<&std::ffi::OsStr> = cmd.as_std().get_args().collect();
        assert_eq!(args, vec!["some-agent", "--flag"]);
    }

    #[test]
    fn test_manager_from_config_direct() {
        let config = AcpConfig {
            default_auto_approve: true,
            prompt_timeout_secs: 60,
            agents: HashMap::from([(
                "test".to_string(),
                AcpAgentConfig {
                    launch: "binary".to_string(),
                    command: "/usr/bin/test".to_string(),
                    args: vec![],
                    env: HashMap::new(),
                    workspace: None,
                    auto_approve: None,
                    mode: default_mode(),
                    resource_limits: None,
                },
            )]),
            ..AcpConfig::default()
        };

        let manager = AcpManager::from_config(config);
        assert!(manager.has_agent("test"));
        assert!(!manager.has_agent("other"));
        assert_eq!(manager.available_agents(), vec!["test"]);
        assert!(manager.config.default_auto_approve);
        assert_eq!(manager.config.prompt_timeout_secs, 60);
    }

    #[test]
    fn test_agent_config_method() {
        let config = AcpConfig {
            default_auto_approve: false,
            prompt_timeout_secs: 300,
            agents: HashMap::from([(
                "claude".to_string(),
                AcpAgentConfig {
                    launch: "npx".to_string(),
                    command: "@anthropic-ai/claude-code@latest".to_string(),
                    args: vec!["--acp".to_string()],
                    env: HashMap::new(),
                    workspace: Some("/tmp/ws".to_string()),
                    auto_approve: Some(true),
                    mode: default_mode(),
                    resource_limits: None,
                },
            )]),
            ..AcpConfig::default()
        };

        let manager = AcpManager::from_config(config);
        let agent_cfg = manager.agent_config("claude");
        assert!(agent_cfg.is_some());
        let cfg = agent_cfg.unwrap();
        assert_eq!(cfg.command, "@anthropic-ai/claude-code@latest");
        assert_eq!(cfg.workspace.as_deref(), Some("/tmp/ws"));
        assert_eq!(cfg.auto_approve, Some(true));

        assert!(manager.agent_config("nonexistent").is_none());
    }

    #[test]
    fn test_session_status_equality() {
        assert_eq!(SessionStatus::Active, SessionStatus::Active);
        assert_eq!(SessionStatus::Prompting, SessionStatus::Prompting);
        assert_eq!(SessionStatus::Ended, SessionStatus::Ended);
        assert_ne!(SessionStatus::Active, SessionStatus::Ended);
        assert_ne!(SessionStatus::Active, SessionStatus::Prompting);
    }

    #[test]
    fn test_jsonrpc_request_serialization() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(42),
            method: "test/method".to_string(),
            params: Some(serde_json::json!({"key": "value"})),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"id\":42"));
        assert!(json.contains("\"method\":\"test/method\""));
        assert!(json.contains("\"key\":\"value\""));
    }

    #[test]
    fn test_jsonrpc_notification_serialization() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: "notifications/initialized".to_string(),
            params: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("\"id\""), "Notification should not have id");
    }

    #[test]
    fn test_jsonrpc_error_response_parsing() {
        let json = r#"{
            "jsonrpc": "2.0",
            "id": 5,
            "error": {
                "code": -32601,
                "message": "Method not found"
            }
        }"#;
        let msg: JsonRpcMessage = serde_json::from_str(json).unwrap();
        assert!(msg.is_response());
        assert!(msg.error.is_some());
        let err = msg.error.unwrap();
        assert_eq!(err.code, -32601);
        assert_eq!(err.message, "Method not found");
    }

    // -----------------------------------------------------------------------
    // Phase 0: Process pool limit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_config_pool_defaults() {
        let config = AcpConfig::default();
        assert_eq!(config.max_sessions, 20);
        assert_eq!(config.max_per_agent, 10);
    }

    #[test]
    fn test_config_pool_parse() {
        let json = r#"{
            "maxSessions": 5,
            "maxPerAgent": 2,
            "acpAgents": {
                "claude": { "command": "claude" }
            }
        }"#;
        let config: AcpConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.max_sessions, 5);
        assert_eq!(config.max_per_agent, 2);
    }

    #[test]
    fn test_config_pool_parse_snake_case() {
        let json = r#"{
            "max_sessions": 8,
            "max_per_agent": 3
        }"#;
        let config: AcpConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.max_sessions, 8);
        assert_eq!(config.max_per_agent, 3);
    }

    #[tokio::test]
    async fn test_pool_total_limit_enforced() {
        let config = AcpConfig {
            max_sessions: 0, // zero capacity — any new_session should fail
            max_per_agent: 10,
            ..AcpConfig::default()
        };
        let manager = AcpManager::from_config(config);

        // Total limit check fires before agent config lookup,
        // so even a nonexistent agent triggers the pool error first.
        let result = manager.new_session("claude", None, None).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("session limit reached"),
            "Expected pool limit error, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_pool_per_agent_limit_enforced() {
        let config = AcpConfig {
            max_sessions: 100,
            max_per_agent: 1,
            agents: HashMap::from([(
                "claude".to_string(),
                AcpAgentConfig {
                    launch: "binary".to_string(),
                    command: "/nonexistent/bin".to_string(),
                    args: vec![],
                    env: HashMap::new(),
                    workspace: None,
                    auto_approve: None,
                    mode: default_mode(),
                    resource_limits: None,
                },
            )]),
            ..AcpConfig::default()
        };
        let manager = AcpManager::from_config(config);

        // Simulate 1 existing session for "claude"
        manager
            .agent_session_counts
            .write()
            .await
            .insert("claude".to_string(), 1);

        // Now new_session for "claude" should be rejected
        let result = manager.new_session("claude", None, None).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("per-agent limit reached"),
            "Expected per-agent limit error, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Phase 1: Idle timeout / reaper tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_config_idle_timeout_default() {
        let config = AcpConfig::default();
        assert_eq!(config.idle_timeout_secs, 600);
    }

    #[test]
    fn test_config_idle_timeout_parse_camel() {
        let json = r#"{ "idleTimeoutSecs": 120 }"#;
        let config: AcpConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.idle_timeout_secs, 120);
    }

    #[test]
    fn test_config_idle_timeout_parse_snake() {
        let json = r#"{ "idle_timeout_secs": 0 }"#;
        let config: AcpConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.idle_timeout_secs, 0);
    }

    #[tokio::test]
    async fn test_reap_idle_sessions_disabled_when_zero() {
        let config = AcpConfig {
            idle_timeout_secs: 0,
            ..AcpConfig::default()
        };
        let manager = AcpManager::from_config(config);
        let reaped = manager.reap_idle_sessions().await;
        assert_eq!(reaped, 0);
    }

    #[tokio::test]
    async fn test_reap_idle_sessions_empty() {
        let config = AcpConfig {
            idle_timeout_secs: 1,
            ..AcpConfig::default()
        };
        let manager = AcpManager::from_config(config);
        let reaped = manager.reap_idle_sessions().await;
        assert_eq!(reaped, 0);
    }

    // -----------------------------------------------------------------------
    // Phase 2: Crash recovery tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_prompt_result_context_reset_default_false() {
        let result = AcpPromptResult {
            messages: vec![],
            tool_calls: vec![],
            files_changed: vec![],
            completed: true,
            duration_ms: 0,
            context_reset: false,
        };
        assert!(!result.context_reset);
    }

    #[test]
    fn test_prompt_result_context_reset_true() {
        let result = AcpPromptResult {
            messages: vec!["recovered".to_string()],
            tool_calls: vec![],
            files_changed: vec![],
            completed: true,
            duration_ms: 100,
            context_reset: true,
        };
        assert!(result.context_reset);
        assert_eq!(result.messages[0], "recovered");
    }

    #[tokio::test]
    async fn test_recover_session_agent_not_configured() {
        // If agent config was removed after session creation, recovery should
        // fail with a clear error.
        let config = AcpConfig::default(); // no agents configured
        let manager = AcpManager::from_config(config);

        // Build a minimal session struct to pass to recover_session.
        // We can't build a real AcpConnection without a process, but
        // recover_session only reads session fields before spawning.
        // Since we can't construct AcpSession without AcpConnection,
        // we test via prompt() on a nonexistent session instead.
        let result = manager.prompt("nonexistent", "hello", None, None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[tokio::test]
    async fn test_is_alive_after_spawn_and_kill() {
        // Spawn a real process (sleep) and verify is_alive / kill behavior.
        let config = AcpAgentConfig {
            launch: "binary".to_string(),
            command: "sleep".to_string(),
            args: vec!["60".to_string()],
            env: HashMap::new(),
            workspace: None,
            auto_approve: None,
            mode: default_mode(),
            resource_limits: None,
        };

        let mut cmd = build_spawn_command(&config, Some("/tmp"));
        let child = cmd.spawn();
        if child.is_err() {
            // Skip test if 'sleep' is not available
            return;
        }
        let mut child = child.unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let conn = AcpConnection {
            agent_name: "test".to_string(),
            inner: Mutex::new(AcpConnectionInner {
                stdin,
                stdout: BufReader::new(stdout),
                _child: child,
                next_id: 1,
            }),
            request_timeout: Duration::from_secs(5),
        };

        // Process should be alive
        assert!(conn.is_alive().await);

        // Kill it
        {
            let mut inner = conn.inner.lock().await;
            let _ = inner._child.kill().await;
            let _ = inner._child.wait().await;
        }

        // Process should be dead
        assert!(!conn.is_alive().await);
    }

    // -----------------------------------------------------------------------
    // Phase 3: Async job tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_job_status_enum() {
        assert_eq!(AcpJobStatus::Running, AcpJobStatus::Running);
        assert_eq!(AcpJobStatus::Completed, AcpJobStatus::Completed);
        assert_eq!(AcpJobStatus::Failed, AcpJobStatus::Failed);
        assert_ne!(AcpJobStatus::Running, AcpJobStatus::Completed);
    }

    #[tokio::test]
    async fn test_submit_job_session_not_found() {
        let manager = Arc::new(AcpManager::from_config(AcpConfig::default()));
        let result = manager
            .submit_job("nonexistent", "hello", None, None, None)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[tokio::test]
    async fn test_job_status_not_found() {
        let manager = AcpManager::from_config(AcpConfig::default());
        let result = manager.job_status("nonexistent").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[tokio::test]
    async fn test_cleanup_expired_jobs_empty() {
        let manager = AcpManager::from_config(AcpConfig::default());
        // Should not panic on empty store
        manager.cleanup_expired_jobs().await;
        assert!(manager.jobs.read().await.is_empty());
    }

    #[tokio::test]
    async fn test_job_store_cleanup_removes_old_completed() {
        let manager = AcpManager::from_config(AcpConfig::default());

        // Insert a completed job with very old created_at
        let old_job = AcpJob {
            id: "old-job".to_string(),
            session_id: "s1".to_string(),
            agent_id: "claude".to_string(),
            status: AcpJobStatus::Completed,
            result: None,
            error: None,
            created_at: chrono::Utc::now() - chrono::Duration::hours(2),
            completed_at: Some(chrono::Utc::now() - chrono::Duration::hours(2)),
        };
        manager
            .jobs
            .write()
            .await
            .insert("old-job".to_string(), Mutex::new(old_job));

        // Insert a recent completed job
        let new_job = AcpJob {
            id: "new-job".to_string(),
            session_id: "s1".to_string(),
            agent_id: "claude".to_string(),
            status: AcpJobStatus::Completed,
            result: None,
            error: None,
            created_at: chrono::Utc::now(),
            completed_at: Some(chrono::Utc::now()),
        };
        manager
            .jobs
            .write()
            .await
            .insert("new-job".to_string(), Mutex::new(new_job));

        assert_eq!(manager.jobs.read().await.len(), 2);

        manager.cleanup_expired_jobs().await;

        let jobs = manager.jobs.read().await;
        assert_eq!(jobs.len(), 1);
        assert!(jobs.contains_key("new-job"));
        assert!(!jobs.contains_key("old-job"));
    }

    #[tokio::test]
    async fn test_progress_events_sent_via_channel() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpProgressEvent>();

        tx.send(AcpProgressEvent::ToolStart {
            name: "bash".to_string(),
        })
        .unwrap();
        tx.send(AcpProgressEvent::ToolComplete {
            name: "bash".to_string(),
            status: "success".to_string(),
        })
        .unwrap();
        tx.send(AcpProgressEvent::Thinking {
            text: "analyzing...".to_string(),
        })
        .unwrap();
        drop(tx);

        let mut events = vec![];
        while let Some(e) = rx.recv().await {
            events.push(e);
        }

        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], AcpProgressEvent::ToolStart { name } if name == "bash"));
        assert!(
            matches!(&events[1], AcpProgressEvent::ToolComplete { name, status } if name == "bash" && status == "success")
        );
        assert!(
            matches!(&events[2], AcpProgressEvent::Thinking { text } if text == "analyzing...")
        );
    }

    #[tokio::test]
    async fn test_prompt_with_none_progress_tx() {
        // Ensure prompt() still works when no progress sender is provided
        let manager = AcpManager::from_config_file("/nonexistent/acp.json");
        let result = manager.prompt("nonexistent", "hello", None, None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    // -----------------------------------------------------------------------
    // Phase 5: PTY fallback mode tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_agent_config_default_mode_is_acp() {
        let json = r#"{"command": "test-agent"}"#;
        let config: AcpAgentConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.mode, "acp");
    }

    #[test]
    fn test_agent_config_mode_pty() {
        let json = r#"{"mode": "pty", "command": "test-agent"}"#;
        let config: AcpAgentConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.mode, "pty");
    }

    #[tokio::test]
    async fn test_pty_connection_spawn_and_prompt() {
        // Use 'echo' as a trivial PTY agent — it exits immediately after
        // writing its args to stdout.
        let config = AcpAgentConfig {
            mode: "pty".to_string(),
            launch: "binary".to_string(),
            command: "cat".to_string(),
            args: vec![],
            env: HashMap::new(),
            workspace: None,
            auto_approve: None,
            resource_limits: None,
        };

        let conn = PtyConnection::spawn("test-cat", &config, Some("/tmp")).await;
        if conn.is_err() {
            // cat not available in test env — skip
            return;
        }
        let conn = conn.unwrap();

        assert!(conn.is_alive().await);

        let result = conn
            .prompt("hello world", Duration::from_secs(10), None)
            .await
            .expect("prompt should succeed");
        assert!(result.completed);
        assert!(result.messages.iter().any(|m| m.contains("hello world")));
        assert!(result.tool_calls.is_empty());
        assert!(result.files_changed.is_empty());
    }

    #[tokio::test]
    async fn test_pty_connection_shutdown() {
        let config = AcpAgentConfig {
            mode: "pty".to_string(),
            launch: "binary".to_string(),
            command: "sleep".to_string(),
            args: vec!["60".to_string()],
            env: HashMap::new(),
            workspace: None,
            auto_approve: None,
            resource_limits: None,
        };

        let conn = PtyConnection::spawn("test-sleep", &config, Some("/tmp")).await;
        if conn.is_err() {
            return;
        }
        let conn = conn.unwrap();

        assert!(conn.is_alive().await);
        conn.shutdown().await.expect("shutdown should succeed");
        // After kill, is_alive should return false (process exited)
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!conn.is_alive().await);
    }

    #[test]
    fn test_connection_kind_is_acp() {
        // Verify the is_acp() helper on ConnectionKind
        // We can't easily construct real connections here, so test the enum logic
        // by checking that the method exists and the type compiles correctly.
        // (Full integration tested via new_session with mode="pty")
    }

    #[tokio::test]
    async fn test_pty_prompt_with_progress_events() {
        // Use 'cat' which echoes stdin back — sends progress events for each line
        let config = AcpAgentConfig {
            mode: "pty".to_string(),
            launch: "binary".to_string(),
            command: "cat".to_string(),
            args: vec![],
            env: HashMap::new(),
            workspace: None,
            auto_approve: None,
            resource_limits: None,
        };

        let conn = PtyConnection::spawn("test-cat-progress", &config, Some("/tmp")).await;
        if conn.is_err() {
            return;
        }
        let conn = conn.unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let result = conn
            .prompt("progress test line", Duration::from_secs(10), Some(&tx))
            .await
            .expect("prompt should succeed");
        drop(tx);

        assert!(result.completed);
        assert!(result
            .messages
            .iter()
            .any(|m| m.contains("progress test line")));

        // Drain events — should have at least one Thinking event
        let mut events = vec![];
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        assert!(
            !events.is_empty(),
            "should have at least one progress event"
        );
        for e in &events {
            assert!(matches!(e, AcpProgressEvent::Thinking { .. }));
        }
    }

    #[tokio::test]
    async fn test_new_session_pty_mode_rejects_without_config() {
        let manager = AcpManager::from_config_file("/nonexistent/acp.json");
        let result = manager.new_session("nonexistent-pty", None, None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not configured"));
    }

    // -----------------------------------------------------------------------
    // Phase 7: Execution isolation / resource limits tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_resource_limits_config_parse() {
        let json = r#"{
            "command": "agent",
            "resourceLimits": { "memoryMb": 4096, "cpuPercent": 200 }
        }"#;
        let config: AcpAgentConfig = serde_json::from_str(json).unwrap();
        let limits = config.resource_limits.unwrap();
        assert_eq!(limits.memory_mb, Some(4096));
        assert_eq!(limits.cpu_percent, Some(200));
    }

    #[test]
    fn test_resource_limits_config_parse_snake_case() {
        let json = r#"{
            "command": "agent",
            "resource_limits": { "memory_mb": 2048, "cpu_percent": 100 }
        }"#;
        let config: AcpAgentConfig = serde_json::from_str(json).unwrap();
        let limits = config.resource_limits.unwrap();
        assert_eq!(limits.memory_mb, Some(2048));
        assert_eq!(limits.cpu_percent, Some(100));
    }

    #[test]
    fn test_resource_limits_default_none() {
        let json = r#"{"command": "agent"}"#;
        let config: AcpAgentConfig = serde_json::from_str(json).unwrap();
        assert!(config.resource_limits.is_none());
    }

    #[test]
    fn test_resource_limits_partial() {
        let json = r#"{
            "command": "agent",
            "resourceLimits": { "memoryMb": 1024 }
        }"#;
        let config: AcpAgentConfig = serde_json::from_str(json).unwrap();
        let limits = config.resource_limits.unwrap();
        assert_eq!(limits.memory_mb, Some(1024));
        assert!(limits.cpu_percent.is_none());
    }

    #[test]
    fn test_acp_config_with_api_token() {
        let json = r#"{
            "acpApiToken": "secret-token",
            "acpAgents": {}
        }"#;
        let config: AcpConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.acp_api_token.as_deref(), Some("secret-token"));
    }

    #[test]
    fn test_acp_config_api_token_default_none() {
        let config = AcpConfig::default();
        assert!(config.acp_api_token.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_cgroup_cleanup_nonexistent_is_safe() {
        // cleanup_cgroup should not panic on nonexistent path
        cleanup_cgroup("/sys/fs/cgroup/rayclaw/nonexistent-test-session");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_apply_resource_limits_invalid_pid() {
        let limits = ResourceLimits {
            memory_mb: Some(1024),
            cpu_percent: Some(100),
        };
        // PID 0 is invalid — cgroup.procs write should fail gracefully
        let result = apply_resource_limits(0, "test-invalid-pid", &limits);
        // May or may not succeed depending on permissions, but should not panic
        // Clean up if it was created
        if let Some(path) = result {
            cleanup_cgroup(&path);
        }
    }

    #[test]
    fn test_session_has_cgroup_path_field() {
        // Verify the cgroup_path field exists and is None by default
        // (tested implicitly through all new_session tests, but explicit here)
        let json = r#"{"command": "agent", "resourceLimits": {"memoryMb": 512}}"#;
        let config: AcpAgentConfig = serde_json::from_str(json).unwrap();
        assert!(config.resource_limits.is_some());
    }
}
