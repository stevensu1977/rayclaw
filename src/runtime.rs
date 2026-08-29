use std::collections::HashMap;
use std::sync::Arc;

use anyhow::anyhow;
use tokio::sync::Mutex;
use tracing::info;
#[cfg(feature = "sqlite-vec")]
use tracing::warn;

/// Wait for any termination signal: SIGTERM, SIGHUP, or Ctrl-C.
/// Returns a human-readable label of which signal was received.
#[cfg(unix)]
async fn shutdown_signal() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
    let mut sighup = signal(SignalKind::hangup()).expect("failed to register SIGHUP handler");

    tokio::select! {
        _ = tokio::signal::ctrl_c() => "SIGINT (Ctrl-C)",
        _ = sigterm.recv() => "SIGTERM",
        _ = sighup.recv() => "SIGHUP",
    }
}

/// Windows has no SIGTERM/SIGHUP; Ctrl-C is the only termination signal.
#[cfg(windows)]
async fn shutdown_signal() -> &'static str {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to listen for Ctrl-C");
    "Ctrl-C"
}

use crate::channel_adapter::ChannelRegistry;
#[cfg(feature = "telegram")]
use crate::channels::telegram::TelegramChannelConfig;
#[cfg(feature = "discord")]
use crate::channels::DiscordAdapter;
#[cfg(feature = "feishu")]
use crate::channels::FeishuAdapter;
#[cfg(feature = "slack")]
use crate::channels::SlackAdapter;
#[cfg(feature = "telegram")]
use crate::channels::TelegramAdapter;
#[cfg(feature = "weixin")]
use crate::channels::WeixinAdapter;
use crate::config::Config;
use crate::db::Database;
use crate::embedding::EmbeddingProvider;
use crate::llm::LlmProvider;
use crate::memory::MemoryManager;
use crate::skills::SkillManager;
use crate::tools::ToolRegistry;
#[cfg(feature = "web")]
use crate::web::WebAdapter;

/// Per-chat mutex map to prevent concurrent agent loops for the same chat_id.
/// When a second request arrives for a chat_id that is already processing,
/// it waits for the first to finish before starting.
pub type ChatLocks = Mutex<HashMap<i64, Arc<Mutex<()>>>>;

pub struct AppState {
    pub config: Config,
    pub channel_registry: Arc<ChannelRegistry>,
    pub db: Arc<Database>,
    pub memory: MemoryManager,
    pub skills: SkillManager,
    pub llm: Box<dyn LlmProvider>,
    pub embedding: Option<Arc<dyn EmbeddingProvider>>,
    pub tools: ToolRegistry,
    pub acp_manager: Arc<crate::acp::AcpManager>,
    /// Per-chat concurrency lock: ensures only one agent loop runs per chat_id at a time.
    pub chat_locks: ChatLocks,
}

/// Build an `AppState` without starting any channels, schedulers, or signal handlers.
/// This is the shared initialization used by both the full `run()` and the SDK entry point.
#[allow(clippy::too_many_arguments)]
pub async fn create_app_state(
    config: Config,
    db: Arc<Database>,
    channel_registry: Arc<ChannelRegistry>,
    memory: MemoryManager,
    skills: SkillManager,
    mcp_manager: crate::mcp::McpManager,
    acp_manager: crate::acp::AcpManager,
    use_sdk_tools: bool,
) -> anyhow::Result<Arc<AppState>> {
    let llm = crate::llm::create_provider(&config);
    let embedding = crate::embedding::create_provider(&config);
    #[cfg(feature = "sqlite-vec")]
    {
        let dim = embedding
            .as_ref()
            .map(|e| e.dimension())
            .or(config.embedding_dim)
            .unwrap_or(1536);
        if let Err(e) = db.prepare_vector_index(dim) {
            warn!("Failed to initialize sqlite-vec index: {e}");
        }
    }

    let mut tools = if use_sdk_tools {
        ToolRegistry::new_for_sdk(&config, db.clone())
    } else {
        ToolRegistry::new(&config, channel_registry.clone(), db.clone())
    };

    for (server, tool_info) in mcp_manager.all_tools() {
        tools.add_tool(Box::new(crate::tools::mcp::McpTool::new(server, tool_info)));
    }

    let acp_manager = Arc::new(acp_manager);

    // Build completion callback for async ACP jobs — delivers results to the
    // originating chat via the channel adapter.
    let job_callback: Option<crate::acp::JobCompletionCallback> = if !use_sdk_tools {
        let cb_registry = channel_registry.clone();
        let cb_db = db.clone();
        let cb_bot = config.bot_username.clone();
        Some(std::sync::Arc::new(move |chat_id: i64, text: String| {
            let reg = cb_registry.clone();
            let db = cb_db.clone();
            let bot = cb_bot.clone();
            Box::pin(async move {
                if let Err(e) =
                    crate::channel::deliver_and_store_bot_message(&reg, db, &bot, chat_id, &text)
                        .await
                {
                    tracing::warn!("ACP job callback: failed to deliver to chat {chat_id}: {e}");
                }
            })
        }))
    } else {
        None
    };

    // Build notification callback for ACP tools (send status messages to chats).
    let notify_fn: Option<crate::tools::acp::NotifyFn> = if !use_sdk_tools {
        let n_registry = channel_registry.clone();
        let n_db = db.clone();
        let n_bot = config.bot_username.clone();
        Some(std::sync::Arc::new(move |chat_id: i64, text: String| {
            let reg = n_registry.clone();
            let db = n_db.clone();
            let bot = n_bot.clone();
            Box::pin(async move {
                if let Err(e) =
                    crate::channel::deliver_and_store_bot_message(&reg, db, &bot, chat_id, &text)
                        .await
                {
                    tracing::warn!("ACP notify: failed to deliver to chat {chat_id}: {e}");
                }
            })
        }))
    } else {
        None
    };

    // Register ACP tools so the model can directly create/prompt/end coding agent sessions.
    for tool in crate::tools::acp::make_acp_tools_with_callback(
        acp_manager.clone(),
        job_callback,
        notify_fn,
    ) {
        tools.add_tool(tool);
    }

    Ok(Arc::new(AppState {
        config,
        channel_registry,
        db,
        memory,
        skills,
        llm,
        embedding,
        tools,
        acp_manager,
        chat_locks: Mutex::new(HashMap::new()),
    }))
}

pub async fn run(
    config: Config,
    db: Database,
    memory: MemoryManager,
    skills: SkillManager,
    mcp_manager: crate::mcp::McpManager,
    acp_manager: crate::acp::AcpManager,
) -> anyhow::Result<()> {
    let db = Arc::new(db);

    // Clear stale TODO.json files from previous runs to prevent
    // in_progress tasks from being blindly resumed after a restart.
    {
        let groups_dir = std::path::PathBuf::from(&config.data_dir).join("groups");
        if let Ok(entries) = std::fs::read_dir(&groups_dir) {
            for entry in entries.flatten() {
                let todo = entry.path().join("TODO.json");
                if todo.exists() {
                    let _ = std::fs::remove_file(&todo);
                }
            }
        }
    }

    // Build channel registry from config
    #[allow(unused_mut)]
    let mut registry = ChannelRegistry::new();

    #[cfg(feature = "telegram")]
    let mut telegram_bot: Option<teloxide::Bot> = None;
    #[cfg(feature = "discord")]
    let mut discord_token: Option<String> = None;
    #[cfg(feature = "slack")]
    let mut has_slack = false;
    #[cfg(feature = "feishu")]
    let mut has_feishu = false;
    #[cfg(feature = "weixin")]
    let mut has_weixin = false;

    #[cfg(feature = "telegram")]
    if let Some(tg_cfg) = config.channel_config::<TelegramChannelConfig>("telegram") {
        if !tg_cfg.bot_token.trim().is_empty() {
            let bot = teloxide::Bot::new(&tg_cfg.bot_token);
            telegram_bot = Some(bot.clone());
            registry.register(Arc::new(TelegramAdapter::new(bot, tg_cfg)));
        }
    }

    #[cfg(feature = "discord")]
    if let Some(dc_cfg) =
        config.channel_config::<crate::channels::discord::DiscordChannelConfig>("discord")
    {
        if !dc_cfg.bot_token.trim().is_empty() {
            discord_token = Some(dc_cfg.bot_token.clone());
            registry.register(Arc::new(DiscordAdapter::new(dc_cfg.bot_token)));
        }
    }

    #[cfg(feature = "slack")]
    if let Some(slack_cfg) =
        config.channel_config::<crate::channels::slack::SlackChannelConfig>("slack")
    {
        if !slack_cfg.bot_token.trim().is_empty() && !slack_cfg.app_token.trim().is_empty() {
            has_slack = true;
            registry.register(Arc::new(SlackAdapter::new(slack_cfg.bot_token)));
        }
    }

    #[cfg(feature = "feishu")]
    if let Some(feishu_cfg) =
        config.channel_config::<crate::channels::feishu::FeishuChannelConfig>("feishu")
    {
        if !feishu_cfg.app_id.trim().is_empty() && !feishu_cfg.app_secret.trim().is_empty() {
            has_feishu = true;
            registry.register(Arc::new(FeishuAdapter::new(
                feishu_cfg.app_id.clone(),
                feishu_cfg.app_secret.clone(),
                feishu_cfg.domain.clone(),
            )));
        }
    }

    #[cfg(feature = "weixin")]
    let mut weixin_adapter: Option<Arc<WeixinAdapter>> = None;
    #[cfg(feature = "weixin")]
    if let Some(weixin_cfg) =
        config.channel_config::<crate::channels::weixin::WeixinChannelConfig>("weixin")
    {
        if !weixin_cfg.bot_token.trim().is_empty() {
            has_weixin = true;
            let adapter = Arc::new(WeixinAdapter::new(&weixin_cfg));
            registry.register(adapter.clone());
            weixin_adapter = Some(adapter);
        }
    }

    #[cfg(feature = "web")]
    if config.web_enabled {
        registry.register(Arc::new(WebAdapter));
    }

    let channel_registry = Arc::new(registry);

    let state = create_app_state(
        config,
        db,
        channel_registry,
        memory,
        skills,
        mcp_manager,
        acp_manager,
        false,
    )
    .await?;

    crate::scheduler::spawn_scheduler(state.clone());
    crate::scheduler::spawn_reflector(state.clone());
    crate::acp::spawn_idle_reaper(state.acp_manager.clone());

    #[cfg(feature = "discord")]
    if let Some(ref token) = discord_token {
        let discord_state = state.clone();
        let token = token.clone();
        info!("Starting Discord bot");
        tokio::spawn(async move {
            crate::discord::start_discord_bot(discord_state, &token).await;
        });
    }

    #[cfg(feature = "slack")]
    if has_slack {
        let slack_state = state.clone();
        info!("Starting Slack bot (Socket Mode)");
        tokio::spawn(async move {
            crate::channels::slack::start_slack_bot(slack_state).await;
        });
    }

    #[cfg(feature = "feishu")]
    if has_feishu {
        let feishu_state = state.clone();
        info!("Starting Feishu bot");
        tokio::spawn(async move {
            crate::channels::feishu::start_feishu_bot(feishu_state).await;
        });
    }

    #[cfg(feature = "weixin")]
    if let Some(adapter) = weixin_adapter {
        let weixin_state = state.clone();
        info!("Starting WeChat bot (iLink long-poll)");
        tokio::spawn(async move {
            crate::channels::weixin::start_weixin_bot(weixin_state, adapter).await;
        });
    }

    #[cfg(feature = "web")]
    if state.config.web_enabled {
        let web_state = state.clone();
        info!(
            "Starting Web UI server on {}:{}",
            state.config.web_host, state.config.web_port
        );
        tokio::spawn(async move {
            crate::web::start_web_server(web_state).await;
        });
    }

    // Determine whether any non-Telegram channel is active
    let has_other_channel = {
        #[allow(unused_mut)]
        let mut active = false;
        #[cfg(feature = "web")]
        {
            active = active || state.config.web_enabled;
        }
        #[cfg(feature = "discord")]
        {
            active = active || discord_token.is_some();
        }
        #[cfg(feature = "slack")]
        {
            active = active || has_slack;
        }
        #[cfg(feature = "feishu")]
        {
            active = active || has_feishu;
        }
        #[cfg(feature = "weixin")]
        {
            active = active || has_weixin;
        }
        active
    };

    #[cfg(feature = "telegram")]
    if let Some(bot) = telegram_bot {
        let result = crate::telegram::start_telegram_bot(state.clone(), bot).await;

        // Clean up ACP sessions after Telegram dispatcher exits
        info!("Cleaning up ACP sessions...");
        state.acp_manager.cleanup().await;

        return result;
    }

    if has_other_channel {
        info!("Waiting for channels (no Telegram adapter)");
        let sig = shutdown_signal().await;
        info!("Received {sig}, starting graceful shutdown...");

        // Graceful shutdown: give in-flight tasks a moment to complete
        info!("Allowing in-flight tasks 2s to finish...");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        // Clean up ACP sessions (terminate agent subprocesses)
        info!("Cleaning up ACP sessions...");
        state.acp_manager.cleanup().await;

        // SQLite WAL mode ensures DB consistency even on hard kill,
        // but explicit flush is good practice
        info!("Shutdown complete.");
        Ok(())
    } else {
        Err(anyhow!(
            "No channel is enabled. Configure Telegram, Discord, Slack, Feishu, WeChat, or web_enabled=true."
        ))
    }
}
