use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use crate::acp::{AcpManager, JobCompletionCallback};
use crate::llm_types::ToolDefinition;

use super::{auth_context_from_input, schema_object, Tool, ToolResult};

/// Build all ACP tools sharing a single AcpManager.
pub fn make_acp_tools(manager: Arc<AcpManager>) -> Vec<Box<dyn Tool>> {
    make_acp_tools_with_callback(manager, None)
}

/// Build all ACP tools with an optional job completion callback.
pub fn make_acp_tools_with_callback(
    manager: Arc<AcpManager>,
    on_job_complete: Option<JobCompletionCallback>,
) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(AcpNewSessionTool::new(manager.clone())),
        Box::new(AcpPromptTool::new(manager.clone())),
        Box::new(AcpEndSessionTool::new(manager.clone())),
        Box::new(AcpListSessionsTool::new(manager.clone())),
        Box::new(AcpSubmitJobTool::new(manager.clone(), on_job_complete)),
        Box::new(AcpJobStatusTool::new(manager)),
    ]
}

// ---------------------------------------------------------------------------
// acp_new_session
// ---------------------------------------------------------------------------

struct AcpNewSessionTool {
    manager: Arc<AcpManager>,
}

impl AcpNewSessionTool {
    fn new(manager: Arc<AcpManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for AcpNewSessionTool {
    fn name(&self) -> &str {
        "acp_new_session"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "acp_new_session".into(),
            description: "Create a new ACP agent session. Spawns an external Coding Agent \
                (e.g. Claude Code) as a subprocess. The agent can autonomously read/write files, \
                run commands, and complete coding tasks in the given workspace. \
                Returns a session_id to use with acp_prompt and acp_end_session."
                .into(),
            input_schema: schema_object(
                json!({
                    "agent": {
                        "type": "string",
                        "description": "Agent name from acp.json config (e.g. \"claude\")"
                    },
                    "workspace": {
                        "type": "string",
                        "description": "Working directory for the agent. Defaults to the agent's configured workspace."
                    },
                    "auto_approve": {
                        "type": "boolean",
                        "description": "Auto-approve the agent's tool calls. Defaults to the config setting."
                    }
                }),
                &["agent"],
            ),
        }
    }

    async fn execute(&self, input: serde_json::Value) -> ToolResult {
        let agent = match input.get("agent").and_then(|v| v.as_str()) {
            Some(a) => a,
            None => return ToolResult::error("Missing required parameter: agent".into()),
        };

        let workspace = input.get("workspace").and_then(|v| v.as_str());
        let auto_approve = input.get("auto_approve").and_then(|v| v.as_bool());

        match self
            .manager
            .new_session(agent, workspace, auto_approve)
            .await
        {
            Ok(info) => ToolResult::success(
                json!({
                    "session_id": info.session_id,
                    "agent": info.agent_id,
                    "workspace": info.workspace,
                    "status": "active"
                })
                .to_string(),
            ),
            Err(e) => ToolResult::error(format!("Failed to create ACP session: {e}"))
                .with_error_type("acp_error"),
        }
    }
}

// ---------------------------------------------------------------------------
// acp_prompt
// ---------------------------------------------------------------------------

struct AcpPromptTool {
    manager: Arc<AcpManager>,
}

impl AcpPromptTool {
    fn new(manager: Arc<AcpManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for AcpPromptTool {
    fn name(&self) -> &str {
        "acp_prompt"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "acp_prompt".into(),
            description: "Send a coding task to an active ACP agent session and wait for \
                completion. The agent will autonomously execute the task (read/write files, \
                run commands, etc.) and return the results including output messages, \
                tool calls made, and files changed."
                .into(),
            input_schema: schema_object(
                json!({
                    "session_id": {
                        "type": "string",
                        "description": "Session ID returned by acp_new_session"
                    },
                    "message": {
                        "type": "string",
                        "description": "The coding task or instruction to send to the agent"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Max seconds to wait for completion. Defaults to config value (300s)."
                    }
                }),
                &["session_id", "message"],
            ),
        }
    }

    async fn execute(&self, input: serde_json::Value) -> ToolResult {
        let session_id = match input.get("session_id").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return ToolResult::error("Missing required parameter: session_id".into()),
        };

        let message = match input.get("message").and_then(|v| v.as_str()) {
            Some(m) => m,
            None => return ToolResult::error("Missing required parameter: message".into()),
        };

        let timeout_secs = input.get("timeout_secs").and_then(|v| v.as_u64());

        match self
            .manager
            .prompt(session_id, message, timeout_secs, None)
            .await
        {
            Ok(result) => {
                let tool_call_summaries: Vec<serde_json::Value> = result
                    .tool_calls
                    .iter()
                    .map(|tc| {
                        json!({
                            "tool": tc.name,
                            "input": tc.input,
                        })
                    })
                    .collect();

                let mut output = json!({
                    "completed": result.completed,
                    "messages": result.messages,
                    "tool_calls": tool_call_summaries,
                    "files_changed": result.files_changed,
                    "duration_ms": result.duration_ms,
                });
                if result.context_reset {
                    output["context_reset"] = json!(true);
                    output["context_reset_notice"] =
                        json!("Agent process crashed and was restarted. Previous conversation context was lost.");
                }

                ToolResult::success(output.to_string())
            }
            Err(e) => {
                ToolResult::error(format!("ACP prompt failed: {e}")).with_error_type("acp_error")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// acp_end_session
// ---------------------------------------------------------------------------

struct AcpEndSessionTool {
    manager: Arc<AcpManager>,
}

impl AcpEndSessionTool {
    fn new(manager: Arc<AcpManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for AcpEndSessionTool {
    fn name(&self) -> &str {
        "acp_end_session"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "acp_end_session".into(),
            description: "End an ACP agent session and terminate the agent subprocess. \
                Call this when you're done with the coding agent to free resources."
                .into(),
            input_schema: schema_object(
                json!({
                    "session_id": {
                        "type": "string",
                        "description": "Session ID returned by acp_new_session"
                    }
                }),
                &["session_id"],
            ),
        }
    }

    async fn execute(&self, input: serde_json::Value) -> ToolResult {
        let session_id = match input.get("session_id").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return ToolResult::error("Missing required parameter: session_id".into()),
        };

        match self.manager.end_session(session_id).await {
            Ok(()) => ToolResult::success(
                json!({
                    "status": "ended",
                    "session_id": session_id,
                })
                .to_string(),
            ),
            Err(e) => ToolResult::error(format!("Failed to end ACP session: {e}"))
                .with_error_type("acp_error"),
        }
    }
}

// ---------------------------------------------------------------------------
// acp_list_sessions
// ---------------------------------------------------------------------------

struct AcpListSessionsTool {
    manager: Arc<AcpManager>,
}

impl AcpListSessionsTool {
    fn new(manager: Arc<AcpManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for AcpListSessionsTool {
    fn name(&self) -> &str {
        "acp_list_sessions"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "acp_list_sessions".into(),
            description: "List all active ACP agent sessions with their status, agent type, \
                workspace, and creation time."
                .into(),
            input_schema: schema_object(json!({}), &[]),
        }
    }

    async fn execute(&self, _input: serde_json::Value) -> ToolResult {
        let sessions = self.manager.list_sessions().await;

        let entries: Vec<serde_json::Value> = sessions
            .iter()
            .map(|s| {
                json!({
                    "session_id": s.session_id,
                    "agent": s.agent_id,
                    "workspace": s.workspace,
                    "status": format!("{:?}", s.status),
                    "created_at": s.created_at,
                    "idle_secs": s.idle_secs,
                })
            })
            .collect();

        let available = self.manager.available_agents();

        ToolResult::success(
            json!({
                "sessions": entries,
                "available_agents": available,
            })
            .to_string(),
        )
    }
}

// ---------------------------------------------------------------------------
// acp_submit_job
// ---------------------------------------------------------------------------

struct AcpSubmitJobTool {
    manager: Arc<AcpManager>,
    on_complete: Option<JobCompletionCallback>,
}

impl AcpSubmitJobTool {
    fn new(manager: Arc<AcpManager>, on_complete: Option<JobCompletionCallback>) -> Self {
        Self {
            manager,
            on_complete,
        }
    }
}

#[async_trait]
impl Tool for AcpSubmitJobTool {
    fn name(&self) -> &str {
        "acp_submit_job"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "acp_submit_job".into(),
            description: "Submit a long-running coding task to an ACP agent as an async job. \
                Returns a job_id immediately without waiting for completion. The agent executes \
                in the background and the result is pushed to the chat when done. \
                Use acp_job_status to check progress."
                .into(),
            input_schema: schema_object(
                json!({
                    "session_id": {
                        "type": "string",
                        "description": "Session ID returned by acp_new_session"
                    },
                    "message": {
                        "type": "string",
                        "description": "The coding task or instruction to send to the agent"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Max seconds for execution. Defaults to config value (300s)."
                    }
                }),
                &["session_id", "message"],
            ),
        }
    }

    async fn execute(&self, input: serde_json::Value) -> ToolResult {
        let session_id = match input.get("session_id").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return ToolResult::error("Missing required parameter: session_id".into()),
        };

        let message = match input.get("message").and_then(|v| v.as_str()) {
            Some(m) => m,
            None => return ToolResult::error("Missing required parameter: message".into()),
        };

        let timeout_secs = input.get("timeout_secs").and_then(|v| v.as_u64());

        // Extract caller chat_id for completion notification
        let chat_id = auth_context_from_input(&input).map(|ctx| ctx.caller_chat_id);

        match self
            .manager
            .submit_job(session_id, message, timeout_secs, chat_id, self.on_complete.clone())
            .await
        {
            Ok(job_id) => ToolResult::success(
                json!({
                    "job_id": job_id,
                    "status": "submitted",
                    "session_id": session_id,
                    "message": "Job submitted. Results will be pushed to the chat when complete. Use acp_job_status to check progress."
                })
                .to_string(),
            ),
            Err(e) => {
                ToolResult::error(format!("Failed to submit ACP job: {e}"))
                    .with_error_type("acp_error")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// acp_job_status
// ---------------------------------------------------------------------------

struct AcpJobStatusTool {
    manager: Arc<AcpManager>,
}

impl AcpJobStatusTool {
    fn new(manager: Arc<AcpManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for AcpJobStatusTool {
    fn name(&self) -> &str {
        "acp_job_status"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "acp_job_status".into(),
            description: "Check the status of an async ACP job submitted via acp_submit_job. \
                Returns the current status (running/completed/failed) and result if available."
                .into(),
            input_schema: schema_object(
                json!({
                    "job_id": {
                        "type": "string",
                        "description": "Job ID returned by acp_submit_job"
                    }
                }),
                &["job_id"],
            ),
        }
    }

    async fn execute(&self, input: serde_json::Value) -> ToolResult {
        let job_id = match input.get("job_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return ToolResult::error("Missing required parameter: job_id".into()),
        };

        match self.manager.job_status(job_id).await {
            Ok(summary) => {
                let mut output = json!({
                    "job_id": summary.id,
                    "session_id": summary.session_id,
                    "agent": summary.agent_id,
                    "status": format!("{:?}", summary.status),
                    "created_at": summary.created_at,
                });

                if let Some(completed_at) = &summary.completed_at {
                    output["completed_at"] = json!(completed_at);
                }
                if let Some(duration_ms) = summary.duration_ms {
                    output["duration_ms"] = json!(duration_ms);
                }
                if let Some(error) = &summary.error {
                    output["error"] = json!(error);
                }

                ToolResult::success(output.to_string())
            }
            Err(e) => ToolResult::error(format!("Failed to get job status: {e}"))
                .with_error_type("acp_error"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_manager() -> Arc<AcpManager> {
        Arc::new(AcpManager::from_config_file("/nonexistent/acp.json"))
    }

    #[test]
    fn test_tool_names_unique() {
        let manager = test_manager();
        let tools = make_acp_tools(manager);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(names.len(), 6);

        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 6, "Tool names must be unique");
    }

    #[test]
    fn test_tool_definitions_valid() {
        let manager = test_manager();
        let tools = make_acp_tools(manager);
        for tool in &tools {
            let def = tool.definition();
            assert!(!def.name.is_empty());
            assert!(!def.description.is_empty());
            assert!(def.input_schema.get("type").is_some());
            // Name must match [a-zA-Z0-9_-]{1,64}
            assert!(def.name.len() <= 64);
            assert!(def
                .name
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '-'));
        }
    }

    #[test]
    fn test_tool_names_match() {
        let manager = test_manager();
        let tools = make_acp_tools(manager);
        let expected = vec![
            "acp_new_session",
            "acp_prompt",
            "acp_end_session",
            "acp_list_sessions",
            "acp_submit_job",
            "acp_job_status",
        ];
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(names, expected);
    }

    #[tokio::test]
    async fn test_new_session_missing_agent_param() {
        let manager = test_manager();
        let tool = AcpNewSessionTool::new(manager);
        let result = tool.execute(json!({})).await;
        assert!(result.is_error);
        assert!(result.content.contains("Missing"));
    }

    #[tokio::test]
    async fn test_new_session_unknown_agent() {
        let manager = test_manager();
        let tool = AcpNewSessionTool::new(manager);
        let result = tool.execute(json!({"agent": "nonexistent"})).await;
        assert!(result.is_error);
        assert!(result.content.contains("not configured"));
    }

    #[tokio::test]
    async fn test_prompt_missing_params() {
        let manager = test_manager();
        let tool = AcpPromptTool::new(manager);

        // Missing session_id
        let r1 = tool.execute(json!({"message": "hello"})).await;
        assert!(r1.is_error);
        assert!(r1.content.contains("session_id"));

        // Missing message
        let r2 = tool.execute(json!({"session_id": "abc"})).await;
        assert!(r2.is_error);
        assert!(r2.content.contains("message"));
    }

    #[tokio::test]
    async fn test_prompt_session_not_found() {
        let manager = test_manager();
        let tool = AcpPromptTool::new(manager);
        let result = tool
            .execute(json!({"session_id": "nonexistent", "message": "hello"}))
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("not found"));
    }

    #[tokio::test]
    async fn test_end_session_not_found() {
        let manager = test_manager();
        let tool = AcpEndSessionTool::new(manager);
        let result = tool.execute(json!({"session_id": "nonexistent"})).await;
        assert!(result.is_error);
        assert!(result.content.contains("not found"));
    }

    #[tokio::test]
    async fn test_list_sessions_empty() {
        let manager = test_manager();
        let tool = AcpListSessionsTool::new(manager);
        let result = tool.execute(json!({})).await;
        assert!(!result.is_error);

        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed["sessions"].as_array().unwrap().len(), 0);
        assert!(parsed["available_agents"].as_array().unwrap().is_empty());
    }

    // -----------------------------------------------------------------------
    // Phase 7.2: Additional schema validation tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_tool_schemas_have_required_fields() {
        let manager = test_manager();
        let tools = make_acp_tools(manager);

        for tool in &tools {
            let def = tool.definition();
            let schema = &def.input_schema;

            // Must have "type": "object"
            assert_eq!(
                schema.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "Tool '{}' schema must be type=object",
                def.name
            );

            // Must have "properties" key
            assert!(
                schema.get("properties").is_some(),
                "Tool '{}' schema must have properties",
                def.name
            );
        }
    }

    #[test]
    fn test_tool_schemas_required_params_are_in_properties() {
        let manager = test_manager();
        let tools = make_acp_tools(manager);

        for tool in &tools {
            let def = tool.definition();
            let schema = &def.input_schema;

            let properties = schema.get("properties").and_then(|v| v.as_object());
            let required = schema
                .get("required")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
                .unwrap_or_default();

            if let Some(props) = properties {
                for req_field in &required {
                    assert!(
                        props.contains_key(*req_field),
                        "Tool '{}': required field '{}' not found in properties",
                        def.name,
                        req_field
                    );
                }
            }
        }
    }

    #[test]
    fn test_tool_descriptions_reasonable_length() {
        let manager = test_manager();
        let tools = make_acp_tools(manager);

        for tool in &tools {
            let def = tool.definition();
            assert!(
                def.description.len() >= 10,
                "Tool '{}' description too short ({})",
                def.name,
                def.description.len()
            );
            assert!(
                def.description.len() <= 1024,
                "Tool '{}' description too long ({})",
                def.name,
                def.description.len()
            );
        }
    }

    #[test]
    fn test_acp_new_session_schema_details() {
        let manager = test_manager();
        let tool = AcpNewSessionTool::new(manager);
        let def = tool.definition();

        let props = def.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("agent"), "Must have 'agent' param");
        assert!(
            props.contains_key("workspace"),
            "Must have 'workspace' param"
        );
        assert!(
            props.contains_key("auto_approve"),
            "Must have 'auto_approve' param"
        );

        let required: Vec<&str> = def.input_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"agent"), "'agent' must be required");
        assert!(
            !required.contains(&"workspace"),
            "'workspace' should be optional"
        );
    }

    #[test]
    fn test_acp_prompt_schema_details() {
        let manager = test_manager();
        let tool = AcpPromptTool::new(manager);
        let def = tool.definition();

        let props = def.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("session_id"));
        assert!(props.contains_key("message"));
        assert!(props.contains_key("timeout_secs"));

        let required: Vec<&str> = def.input_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"session_id"));
        assert!(required.contains(&"message"));
        assert!(!required.contains(&"timeout_secs"));
    }

    #[tokio::test]
    async fn test_end_session_missing_param() {
        let manager = test_manager();
        let tool = AcpEndSessionTool::new(manager);
        let result = tool.execute(json!({})).await;
        assert!(result.is_error);
        assert!(result.content.contains("session_id"));
    }

    #[tokio::test]
    async fn test_list_sessions_ignores_extra_params() {
        let manager = test_manager();
        let tool = AcpListSessionsTool::new(manager);
        // Extra params should be silently ignored
        let result = tool.execute(json!({"unexpected": "param"})).await;
        assert!(!result.is_error);
    }

    #[test]
    fn test_tool_risk_levels() {
        use crate::tools::tool_risk;
        use crate::tools::ToolRisk;

        assert_eq!(tool_risk("acp_prompt"), ToolRisk::High);
        assert_eq!(tool_risk("acp_submit_job"), ToolRisk::High);
        assert_eq!(tool_risk("acp_new_session"), ToolRisk::Medium);
        // Other ACP tools default to Low
        assert_eq!(tool_risk("acp_end_session"), ToolRisk::Low);
        assert_eq!(tool_risk("acp_list_sessions"), ToolRisk::Low);
        assert_eq!(tool_risk("acp_job_status"), ToolRisk::Low);
    }

    // -----------------------------------------------------------------------
    // Phase 3: Async job tool tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_submit_job_missing_params() {
        let manager = test_manager();
        let tool = AcpSubmitJobTool::new(manager, None);

        let r1 = tool.execute(json!({"message": "hello"})).await;
        assert!(r1.is_error);
        assert!(r1.content.contains("session_id"));

        let r2 = tool.execute(json!({"session_id": "abc"})).await;
        assert!(r2.is_error);
        assert!(r2.content.contains("message"));
    }

    #[tokio::test]
    async fn test_submit_job_session_not_found() {
        let manager = test_manager();
        let tool = AcpSubmitJobTool::new(manager, None);
        let result = tool
            .execute(json!({"session_id": "nonexistent", "message": "hello"}))
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("not found"));
    }

    #[tokio::test]
    async fn test_job_status_missing_param() {
        let manager = test_manager();
        let tool = AcpJobStatusTool::new(manager);
        let result = tool.execute(json!({})).await;
        assert!(result.is_error);
        assert!(result.content.contains("job_id"));
    }

    #[tokio::test]
    async fn test_job_status_not_found() {
        let manager = test_manager();
        let tool = AcpJobStatusTool::new(manager);
        let result = tool.execute(json!({"job_id": "nonexistent"})).await;
        assert!(result.is_error);
        assert!(result.content.contains("not found"));
    }

    #[test]
    fn test_submit_job_schema_details() {
        let manager = test_manager();
        let tool = AcpSubmitJobTool::new(manager, None);
        let def = tool.definition();

        let props = def.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("session_id"));
        assert!(props.contains_key("message"));
        assert!(props.contains_key("timeout_secs"));

        let required: Vec<&str> = def.input_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"session_id"));
        assert!(required.contains(&"message"));
        assert!(!required.contains(&"timeout_secs"));
    }

    #[test]
    fn test_job_status_schema_details() {
        let manager = test_manager();
        let tool = AcpJobStatusTool::new(manager);
        let def = tool.definition();

        let props = def.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("job_id"));

        let required: Vec<&str> = def.input_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"job_id"));
    }
}
