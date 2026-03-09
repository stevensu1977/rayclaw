//! Integration tests for ACP (Agent Client Protocol) module.
//!
//! Tests cover:
//! - 7.3: ACP tool names do not conflict with existing core tools
//! - 7.4: Full lifecycle using a mock ACP agent process
//! - 7.5: E2E test stub with real Claude Code (ignored, requires API key)
//! - 7.6: Concurrent session stress test (ignored, requires mock agent)

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rayclaw::acp::{AcpAgentConfig, AcpConfig, AcpManager};
use rayclaw::channel_adapter::ChannelRegistry;
use rayclaw::config::{Config, WorkingDirIsolation};
use rayclaw::db::Database;
use rayclaw::tools::acp::make_acp_tools;
use rayclaw::tools::ToolRegistry;

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Create a Database backed by a unique temporary directory.
/// Returns the DB and the path (caller should clean up or let OS handle it).
fn temp_db() -> (Arc<Database>, String) {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = format!("/tmp/rayclaw-test-{}-{}", std::process::id(), id);
    let _ = std::fs::create_dir_all(&dir);
    let db = Database::new(&dir).expect("failed to create DB");
    (Arc::new(db), dir)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Minimal valid Config for building a ToolRegistry in tests.
fn minimal_config() -> Config {
    Config {
        telegram_bot_token: "tok".into(),
        bot_username: "testbot".into(),
        llm_provider: "anthropic".into(),
        api_key: "test-key".into(),
        model: String::new(),
        llm_base_url: None,
        max_tokens: 8192,
        max_tool_iterations: 25,
        max_history_messages: 50,
        max_document_size_mb: 100,
        memory_token_budget: 1500,
        data_dir: "./rayclaw.data".into(),
        working_dir: "/tmp/rayclaw-test".into(),
        working_dir_isolation: WorkingDirIsolation::Chat,
        openai_api_key: None,
        timezone: "UTC".into(),
        allowed_groups: vec![],
        control_chat_ids: vec![],
        max_session_messages: 40,
        compact_keep_recent: 20,
        discord_bot_token: None,
        discord_allowed_channels: vec![],
        show_thinking: false,
        web_enabled: false,
        web_host: "127.0.0.1".into(),
        web_port: 3900,
        web_auth_token: None,
        web_max_inflight_per_session: 2,
        web_max_requests_per_window: 8,
        web_rate_window_seconds: 10,
        web_run_history_limit: 512,
        web_session_idle_ttl_seconds: 300,
        model_prices: vec![],
        embedding_provider: None,
        embedding_api_key: None,
        embedding_base_url: None,
        embedding_model: None,
        embedding_dim: None,
        reflector_enabled: true,
        reflector_interval_mins: 15,
        soul_path: None,
        skip_tool_approval: false,
        aws_region: None,
        aws_access_key_id: None,
        aws_secret_access_key: None,
        aws_session_token: None,
        aws_profile: None,
        skills_dir: None,
        channels: std::collections::HashMap::new(),
    }
}

/// Path to the mock ACP agent script relative to project root.
fn mock_agent_path() -> String {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    format!("{manifest}/tests/mock_acp_agent.py")
}

/// Build an AcpManager configured to use the mock agent.
fn mock_manager() -> AcpManager {
    let mut agents = std::collections::HashMap::new();
    agents.insert(
        "mock".to_string(),
        AcpAgentConfig {
            launch: "binary".to_string(),
            command: "python3".to_string(),
            args: vec![mock_agent_path()],
            env: std::collections::HashMap::new(),
            workspace: Some("/tmp".to_string()),
            auto_approve: Some(true),
        },
    );
    let config = AcpConfig {
        default_auto_approve: true,
        prompt_timeout_secs: 30,
        agents,
    };
    AcpManager::from_config(config)
}

// ---------------------------------------------------------------------------
// 7.3: ACP tool names do not conflict with existing core tools
// ---------------------------------------------------------------------------

#[test]
fn test_acp_tool_names_no_conflict_with_core_tools() {
    let config = minimal_config();
    let (db, _tmpdir) = temp_db();
    let channel_registry = Arc::new(ChannelRegistry::new());

    // Build the standard tool registry (contains all core tools)
    let core_registry = ToolRegistry::new(&config, channel_registry, db);
    let core_names: Vec<String> = core_registry
        .definitions()
        .iter()
        .map(|d| d.name.clone())
        .collect();

    // Build the ACP tool set
    let acp_manager = Arc::new(AcpManager::from_config_file("/nonexistent/acp.json"));
    let acp_tools = make_acp_tools(acp_manager);
    let acp_names: Vec<&str> = acp_tools.iter().map(|t| t.name()).collect();

    // Verify no name collisions
    for acp_name in &acp_names {
        assert!(
            !core_names.contains(&acp_name.to_string()),
            "ACP tool name '{}' conflicts with a core tool",
            acp_name
        );
    }
}

#[test]
fn test_acp_tool_names_no_conflict_with_sub_agent_tools() {
    let config = minimal_config();
    let (db, _tmpdir) = temp_db();

    // Build the sub-agent restricted registry
    let sub_registry = ToolRegistry::new_sub_agent(&config, db);
    let sub_names: Vec<String> = sub_registry
        .definitions()
        .iter()
        .map(|d| d.name.clone())
        .collect();

    // Build the ACP tool set
    let acp_manager = Arc::new(AcpManager::from_config_file("/nonexistent/acp.json"));
    let acp_tools = make_acp_tools(acp_manager);
    let acp_names: Vec<&str> = acp_tools.iter().map(|t| t.name()).collect();

    // Verify no ACP tool is in the sub-agent set
    for acp_name in &acp_names {
        assert!(
            !sub_names.contains(&acp_name.to_string()),
            "ACP tool '{}' should NOT be in the sub-agent tool set",
            acp_name
        );
    }
}

#[test]
fn test_combined_registry_has_all_tools() {
    let config = minimal_config();
    let (db, _tmpdir) = temp_db();
    let channel_registry = Arc::new(ChannelRegistry::new());

    let mut registry = ToolRegistry::new(&config, channel_registry, db);

    let core_count = registry.definitions().len();

    // Add ACP tools
    let acp_manager = Arc::new(AcpManager::from_config_file("/nonexistent/acp.json"));
    for tool in make_acp_tools(acp_manager) {
        registry.add_tool(tool);
    }

    let total_count = registry.definitions().len();
    assert_eq!(total_count, core_count + 4, "Should have 4 ACP tools added");

    // Verify all ACP tool names are present
    let all_names: Vec<String> = registry
        .definitions()
        .iter()
        .map(|d| d.name.clone())
        .collect();
    assert!(all_names.contains(&"acp_new_session".to_string()));
    assert!(all_names.contains(&"acp_prompt".to_string()));
    assert!(all_names.contains(&"acp_end_session".to_string()));
    assert!(all_names.contains(&"acp_list_sessions".to_string()));
}

#[test]
fn test_all_tool_names_unique_after_acp_registration() {
    let config = minimal_config();
    let (db, _tmpdir) = temp_db();
    let channel_registry = Arc::new(ChannelRegistry::new());

    let mut registry = ToolRegistry::new(&config, channel_registry, db);
    let acp_manager = Arc::new(AcpManager::from_config_file("/nonexistent/acp.json"));
    for tool in make_acp_tools(acp_manager) {
        registry.add_tool(tool);
    }

    let names: Vec<String> = registry
        .definitions()
        .iter()
        .map(|d| d.name.clone())
        .collect();
    let mut deduped = names.clone();
    deduped.sort();
    deduped.dedup();
    assert_eq!(
        names.len(),
        deduped.len(),
        "All tool names must be unique after ACP registration"
    );
}

// ---------------------------------------------------------------------------
// 7.4: Mock ACP agent lifecycle test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_mock_agent_full_lifecycle() {
    let manager = mock_manager();

    // 1. Verify mock agent is configured
    assert!(manager.has_agent("mock"));
    assert_eq!(manager.available_agents(), vec!["mock"]);

    // 2. Create a new session
    let info = manager.new_session("mock", None, None).await;
    assert!(info.is_ok(), "new_session failed: {:?}", info.err());
    let info = info.unwrap();
    assert_eq!(info.agent_id, "mock");
    assert!(!info.session_id.is_empty());

    // 3. Verify session shows up in list
    let sessions = manager.list_sessions().await;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].agent_id, "mock");

    // 4. Send a prompt
    let result = manager
        .prompt(&info.session_id, "write hello world", None)
        .await;
    assert!(result.is_ok(), "prompt failed: {:?}", result.err());
    let result = result.unwrap();
    assert!(result.completed);
    // Mock agent sends session/update notifications with AgentMessageChunk and ToolCall
    assert!(
        !result.messages.is_empty(),
        "Should have messages from session/update"
    );
    assert!(
        !result.tool_calls.is_empty(),
        "Should have tool calls from session/update"
    );
    assert_eq!(result.tool_calls[0].name, "bash");

    // 5. End the session
    let end_result = manager.end_session(&info.session_id).await;
    assert!(
        end_result.is_ok(),
        "end_session failed: {:?}",
        end_result.err()
    );

    // 6. Verify session is gone
    let sessions = manager.list_sessions().await;
    assert!(sessions.is_empty());
}

#[tokio::test]
async fn test_mock_agent_prompt_collects_notifications() {
    let manager = mock_manager();

    let info = manager.new_session("mock", None, None).await.unwrap();
    let result = manager
        .prompt(&info.session_id, "test notifications", None)
        .await
        .unwrap();

    // The mock agent sends session/update notifications with AgentMessageChunk and ToolCall
    // before the final response
    assert!(result.completed);

    // Should have the message from AgentMessageChunk notification
    assert!(
        !result.messages.is_empty(),
        "Expected at least 1 message, got {}",
        result.messages.len()
    );

    // Check AgentMessageChunk notification was captured
    let has_working_msg = result
        .messages
        .iter()
        .any(|m| m.contains("Working on: test notifications"));
    assert!(has_working_msg, "Should capture AgentMessageChunk text");

    // Check ToolCall notification was captured
    assert_eq!(result.tool_calls.len(), 1);
    assert_eq!(result.tool_calls[0].name, "bash");

    // Cleanup
    let _ = manager.end_session(&info.session_id).await;
}

#[tokio::test]
async fn test_mock_agent_multiple_sessions() {
    let manager = mock_manager();

    // Create two sessions
    let info1 = manager.new_session("mock", None, None).await.unwrap();
    let info2 = manager.new_session("mock", None, None).await.unwrap();

    assert_ne!(info1.session_id, info2.session_id);

    let sessions = manager.list_sessions().await;
    assert_eq!(sessions.len(), 2);

    // Prompt both sessions
    let r1 = manager
        .prompt(&info1.session_id, "task 1", None)
        .await
        .unwrap();
    let r2 = manager
        .prompt(&info2.session_id, "task 2", None)
        .await
        .unwrap();

    assert!(r1.completed);
    assert!(r2.completed);

    // End both
    manager.end_session(&info1.session_id).await.unwrap();
    manager.end_session(&info2.session_id).await.unwrap();

    assert!(manager.list_sessions().await.is_empty());
}

#[tokio::test]
async fn test_mock_agent_cleanup_terminates_all() {
    let manager = mock_manager();

    let _info1 = manager.new_session("mock", None, None).await.unwrap();
    let _info2 = manager.new_session("mock", None, None).await.unwrap();

    assert_eq!(manager.list_sessions().await.len(), 2);

    // Cleanup should terminate all sessions
    manager.cleanup().await;

    assert!(manager.list_sessions().await.is_empty());
}

#[tokio::test]
async fn test_mock_agent_end_session_then_prompt_fails() {
    let manager = mock_manager();

    let info = manager.new_session("mock", None, None).await.unwrap();
    manager.end_session(&info.session_id).await.unwrap();

    // Prompt on ended session should fail
    let result = manager.prompt(&info.session_id, "hello", None).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not found"));
}

#[tokio::test]
async fn test_mock_agent_double_end_fails() {
    let manager = mock_manager();

    let info = manager.new_session("mock", None, None).await.unwrap();
    manager.end_session(&info.session_id).await.unwrap();

    // Second end should fail (session already removed)
    let result = manager.end_session(&info.session_id).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not found"));
}

// ---------------------------------------------------------------------------
// 7.4 (error mode): Mock agent returning errors
// ---------------------------------------------------------------------------

/// Build an AcpManager configured to use the mock agent in error mode.
fn mock_manager_error_mode() -> AcpManager {
    let mut agents = std::collections::HashMap::new();
    agents.insert(
        "mock-error".to_string(),
        AcpAgentConfig {
            launch: "binary".to_string(),
            command: "python3".to_string(),
            args: vec![mock_agent_path()],
            env: std::collections::HashMap::from([(
                "ACP_MOCK_MODE".to_string(),
                "error".to_string(),
            )]),
            workspace: Some("/tmp".to_string()),
            auto_approve: Some(true),
        },
    );
    let config = AcpConfig {
        default_auto_approve: true,
        prompt_timeout_secs: 10,
        agents,
    };
    AcpManager::from_config(config)
}

#[tokio::test]
async fn test_mock_agent_prompt_error_propagates() {
    let manager = mock_manager_error_mode();

    let info = manager.new_session("mock-error", None, None).await.unwrap();
    let result = manager.prompt(&info.session_id, "will fail", None).await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.contains("Mock error") || err.contains("prompt error"),
        "Error should contain mock error text, got: {err}"
    );

    let _ = manager.end_session(&info.session_id).await;
}

// ---------------------------------------------------------------------------
// 7.5: E2E test with real Claude Code (ignored — requires ANTHROPIC_API_KEY + npx)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "Requires Node.js + ANTHROPIC_API_KEY. Run with: cargo test -- --ignored test_e2e_claude_code"]
async fn test_e2e_claude_code() {
    // Check prerequisites
    let api_key = std::env::var("ANTHROPIC_API_KEY");
    if api_key.is_err() || api_key.as_ref().unwrap().is_empty() {
        eprintln!("Skipping E2E test: ANTHROPIC_API_KEY not set");
        return;
    }

    let mut agents = std::collections::HashMap::new();
    agents.insert(
        "claude".to_string(),
        AcpAgentConfig {
            launch: "npx".to_string(),
            command: "@anthropic-ai/claude-code@latest".to_string(),
            args: vec!["--acp".to_string()],
            env: std::collections::HashMap::from([(
                "ANTHROPIC_API_KEY".to_string(),
                api_key.unwrap(),
            )]),
            workspace: Some("/tmp/rayclaw-e2e-test".to_string()),
            auto_approve: Some(true),
        },
    );
    let config = AcpConfig {
        default_auto_approve: true,
        prompt_timeout_secs: 300,
        agents,
    };
    let manager = AcpManager::from_config(config);

    // Ensure workspace exists
    let _ = std::fs::create_dir_all("/tmp/rayclaw-e2e-test");

    // Create session
    let info = manager
        .new_session("claude", None, None)
        .await
        .expect("Failed to create Claude Code session");
    assert_eq!(info.agent_id, "claude");

    // Send a simple prompt
    let result = manager
        .prompt(
            &info.session_id,
            "Create a file called hello.py that prints 'Hello from RayClaw ACP test'",
            None,
        )
        .await
        .expect("Prompt failed");
    assert!(result.completed);

    // Verify the file was created
    let content = std::fs::read_to_string("/tmp/rayclaw-e2e-test/hello.py");
    assert!(
        content.is_ok(),
        "hello.py should have been created by Claude Code"
    );
    assert!(content.unwrap().contains("Hello from RayClaw ACP test"));

    // Cleanup
    manager.end_session(&info.session_id).await.unwrap();
    let _ = std::fs::remove_dir_all("/tmp/rayclaw-e2e-test");
}

// ---------------------------------------------------------------------------
// 7.6: Concurrent session stress test (ignored — spawns multiple processes)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "Stress test. Run with: cargo test -- --ignored test_concurrent_sessions"]
async fn test_concurrent_sessions() {
    let manager = Arc::new(mock_manager());
    let session_count: usize = 5;

    // Create sessions concurrently
    let mut join_set = tokio::task::JoinSet::new();
    for i in 0..session_count {
        let mgr = manager.clone();
        join_set.spawn(async move {
            let info = mgr.new_session("mock", None, None).await.unwrap();
            let result = mgr
                .prompt(&info.session_id, &format!("concurrent task {i}"), None)
                .await
                .unwrap();
            assert!(result.completed);
            assert!(result
                .messages
                .iter()
                .any(|m| m.contains(&format!("concurrent task {i}"))));
            info.session_id
        });
    }

    let mut session_ids = Vec::new();
    while let Some(result) = join_set.join_next().await {
        session_ids.push(result.unwrap());
    }

    // Verify all sessions are unique
    let mut deduped = session_ids.clone();
    deduped.sort();
    deduped.dedup();
    assert_eq!(deduped.len(), session_count);

    // All sessions should be listed
    assert_eq!(manager.list_sessions().await.len(), session_count);

    // Cleanup all
    manager.cleanup().await;
    assert!(manager.list_sessions().await.is_empty());
}
