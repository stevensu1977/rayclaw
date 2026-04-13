use std::collections::HashMap;
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::codex_auth::{
    codex_auth_file_has_access_token, is_openai_codex_provider, provider_allows_empty_api_key,
};
use crate::error::RayClawError;

fn default_telegram_bot_token() -> String {
    String::new()
}
fn default_bot_username() -> String {
    String::new()
}
fn default_llm_provider() -> String {
    "anthropic".into()
}
fn default_api_key() -> String {
    String::new()
}
fn default_model() -> String {
    String::new()
}
fn default_max_tokens() -> u32 {
    8192
}
fn default_max_tool_iterations() -> usize {
    100
}
fn default_max_loop_repeats() -> usize {
    3
}
fn default_llm_idle_timeout_secs() -> u64 {
    30
}
fn default_max_history_messages() -> usize {
    50
}
fn default_max_document_size_mb() -> u64 {
    100
}
fn default_memory_token_budget() -> usize {
    1500
}
fn default_data_dir() -> String {
    "./rayclaw.data".into()
}
fn default_working_dir() -> String {
    "./tmp".into()
}
fn default_working_dir_isolation() -> WorkingDirIsolation {
    WorkingDirIsolation::Chat
}
fn default_timezone() -> String {
    "UTC".into()
}
fn default_max_session_messages() -> usize {
    40
}
fn default_compact_keep_recent() -> usize {
    20
}
fn default_control_chat_ids() -> Vec<i64> {
    Vec::new()
}
fn default_web_enabled() -> bool {
    true
}
fn default_web_host() -> String {
    "127.0.0.1".into()
}
fn default_web_port() -> u16 {
    10962
}
fn default_web_max_inflight_per_session() -> usize {
    2
}
fn default_web_max_requests_per_window() -> usize {
    8
}
fn default_web_rate_window_seconds() -> u64 {
    10
}
fn default_web_run_history_limit() -> usize {
    512
}
fn default_web_session_idle_ttl_seconds() -> u64 {
    300
}

fn default_model_prices() -> Vec<ModelPrice> {
    Vec::new()
}
fn default_reflector_enabled() -> bool {
    true
}
fn default_reflector_interval_mins() -> u64 {
    15
}
fn default_soul_path() -> Option<String> {
    None
}
fn default_skip_tool_approval() -> bool {
    false
}
fn default_prompt_cache_ttl() -> String {
    "none".into()
}
fn is_local_web_host(host: &str) -> bool {
    let h = host.trim().to_ascii_lowercase();
    h == "127.0.0.1" || h == "localhost" || h == "::1"
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkingDirIsolation {
    Shared,
    Chat,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelPrice {
    pub model: String,
    pub input_per_million_usd: f64,
    pub output_per_million_usd: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    // --- LLM / API ---
    #[serde(default = "default_llm_provider")]
    pub llm_provider: String,
    #[serde(default = "default_api_key")]
    pub api_key: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default)]
    pub llm_base_url: Option<String>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_prompt_cache_ttl")]
    pub prompt_cache_ttl: String,
    #[serde(default = "default_max_tool_iterations")]
    pub max_tool_iterations: usize,
    #[serde(default = "default_max_loop_repeats")]
    pub max_loop_repeats: usize,
    #[serde(default = "default_llm_idle_timeout_secs")]
    pub llm_idle_timeout_secs: u64,
    #[serde(default = "default_max_history_messages")]
    pub max_history_messages: usize,
    #[serde(default = "default_max_document_size_mb")]
    pub max_document_size_mb: u64,
    #[serde(default = "default_memory_token_budget")]
    pub memory_token_budget: usize,
    #[serde(default = "default_max_session_messages")]
    pub max_session_messages: usize,
    #[serde(default = "default_compact_keep_recent")]
    pub compact_keep_recent: usize,
    #[serde(default)]
    pub show_thinking: bool,

    // --- Paths & environment ---
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    #[serde(default = "default_working_dir")]
    pub working_dir: String,
    #[serde(default = "default_working_dir_isolation")]
    pub working_dir_isolation: WorkingDirIsolation,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    #[serde(default = "default_control_chat_ids")]
    pub control_chat_ids: Vec<i64>,

    // --- Web UI ---
    #[serde(default = "default_web_enabled")]
    pub web_enabled: bool,
    #[serde(default = "default_web_host")]
    pub web_host: String,
    #[serde(default = "default_web_port")]
    pub web_port: u16,
    #[serde(default)]
    pub web_auth_token: Option<String>,
    #[serde(default = "default_web_max_inflight_per_session")]
    pub web_max_inflight_per_session: usize,
    #[serde(default = "default_web_max_requests_per_window")]
    pub web_max_requests_per_window: usize,
    #[serde(default = "default_web_rate_window_seconds")]
    pub web_rate_window_seconds: u64,
    #[serde(default = "default_web_run_history_limit")]
    pub web_run_history_limit: usize,
    #[serde(default = "default_web_session_idle_ttl_seconds")]
    pub web_session_idle_ttl_seconds: u64,

    // --- Embedding ---
    #[serde(default)]
    pub embedding_provider: Option<String>,
    #[serde(default)]
    pub embedding_api_key: Option<String>,
    #[serde(default)]
    pub embedding_base_url: Option<String>,
    #[serde(default)]
    pub embedding_model: Option<String>,
    #[serde(default)]
    pub embedding_dim: Option<usize>,
    #[serde(default)]
    pub openai_api_key: Option<String>,

    // --- Pricing ---
    #[serde(default = "default_model_prices")]
    pub model_prices: Vec<ModelPrice>,

    // --- Reflector ---
    #[serde(default = "default_reflector_enabled")]
    pub reflector_enabled: bool,
    #[serde(default = "default_reflector_interval_mins")]
    pub reflector_interval_mins: u64,

    // --- AWS Bedrock ---
    #[serde(default)]
    pub aws_region: Option<String>,
    #[serde(default)]
    pub aws_access_key_id: Option<String>,
    #[serde(default)]
    pub aws_secret_access_key: Option<String>,
    #[serde(default)]
    pub aws_session_token: Option<String>,
    #[serde(default)]
    pub aws_profile: Option<String>,

    // --- Soul ---
    /// Path to a SOUL.md file that defines the bot's personality, voice, and values.
    /// If not set, looks for SOUL.md in data_dir root, then current directory.
    #[serde(default = "default_soul_path")]
    pub soul_path: Option<String>,

    /// Skip the high-risk tool approval loop (e.g. for bash).
    /// Useful when running inside an isolated environment like Firecracker.
    /// Can also be set via RAYCLAW_SKIP_TOOL_APPROVAL=true env var.
    #[serde(default = "default_skip_tool_approval")]
    pub skip_tool_approval: bool,

    /// Override the skills directory path. When set, `skills_data_dir()` returns
    /// this value instead of computing `{data_dir}/skills`. Useful when `data_dir`
    /// is repointed (e.g. to a runtime subdirectory) but skills remain at the
    /// original data root.
    #[serde(default)]
    pub skills_dir: Option<String>,

    // --- Channel registry (new dynamic config) ---
    /// Per-channel configuration. Keys are channel names (e.g. "telegram", "discord", "slack", "web").
    /// Each value is channel-specific config deserialized by the adapter.
    /// If empty, synthesized from legacy flat fields below in post_deserialize().
    #[serde(default)]
    pub channels: HashMap<String, serde_yaml::Value>,

    // --- Legacy channel fields (deprecated, use `channels:` instead) ---
    #[serde(default = "default_telegram_bot_token")]
    pub telegram_bot_token: String,
    #[serde(default = "default_bot_username")]
    pub bot_username: String,
    #[serde(default)]
    pub allowed_groups: Vec<i64>,
    #[serde(default)]
    pub discord_bot_token: Option<String>,
    #[serde(default)]
    pub discord_allowed_channels: Vec<u64>,
}

impl Config {
    /// Data root directory from config.
    pub fn data_root_dir(&self) -> PathBuf {
        PathBuf::from(&self.data_dir)
    }

    /// Runtime data directory (db, memory, exports, etc.).
    pub fn runtime_data_dir(&self) -> String {
        self.data_root_dir()
            .join("runtime")
            .to_string_lossy()
            .to_string()
    }

    /// Skills directory. Uses `skills_dir` override if set, otherwise `{data_dir}/skills`.
    pub fn skills_data_dir(&self) -> String {
        if let Some(ref dir) = self.skills_dir {
            return dir.clone();
        }
        self.data_root_dir()
            .join("skills")
            .to_string_lossy()
            .to_string()
    }

    pub fn resolve_config_path() -> Result<Option<PathBuf>, RayClawError> {
        // 1. Check RAYCLAW_CONFIG env var for custom path
        if let Ok(custom) = std::env::var("RAYCLAW_CONFIG") {
            if std::path::Path::new(&custom).exists() {
                return Ok(Some(PathBuf::from(custom)));
            }
            return Err(RayClawError::Config(format!(
                "RAYCLAW_CONFIG points to non-existent file: {custom}"
            )));
        }

        if std::path::Path::new("./rayclaw.config.yaml").exists() {
            return Ok(Some(PathBuf::from("./rayclaw.config.yaml")));
        }
        if std::path::Path::new("./rayclaw.config.yml").exists() {
            return Ok(Some(PathBuf::from("./rayclaw.config.yml")));
        }
        Ok(None)
    }

    /// Load config from YAML file.
    pub fn load() -> Result<Self, RayClawError> {
        let yaml_path = Self::resolve_config_path()?;

        if let Some(path) = yaml_path {
            let path_str = path.to_string_lossy().to_string();
            let content = std::fs::read_to_string(&path)
                .map_err(|e| RayClawError::Config(format!("Failed to read {path_str}: {e}")))?;
            let mut config: Config = serde_yaml::from_str(&content)
                .map_err(|e| RayClawError::Config(format!("Failed to parse {path_str}: {e}")))?;
            config.post_deserialize()?;
            return Ok(config);
        }

        // No config file found at all
        Err(RayClawError::Config(
            "No rayclaw.config.yaml found. Run `rayclaw setup` to create one.".into(),
        ))
    }

    /// SDK mode validation: normalizes fields and validates LLM config,
    /// but skips the "at least one channel" requirement.
    pub fn validate_for_sdk(&mut self) -> Result<(), RayClawError> {
        self.normalize_fields()?;
        self.validate_llm()?;
        Ok(())
    }

    /// Normalize common fields (provider, model, timezone, paths, etc.).
    fn normalize_fields(&mut self) -> Result<(), RayClawError> {
        self.llm_provider = self.llm_provider.trim().to_lowercase();

        // Apply provider-specific default model if empty
        if self.model.is_empty() {
            self.model = match self.llm_provider.as_str() {
                "anthropic" => "claude-sonnet-4-5-20250929".into(),
                "bedrock" => "anthropic.claude-sonnet-4-5-v2".into(),
                "ollama" => "llama3.2".into(),
                "openai-codex" => "gpt-5.3-codex".into(),
                _ => "gpt-5.2".into(),
            };
        }

        // Validate timezone
        self.timezone
            .parse::<chrono_tz::Tz>()
            .map_err(|_| RayClawError::Config(format!("Invalid timezone: {}", self.timezone)))?;

        // Filter empty llm_base_url
        if let Some(ref url) = self.llm_base_url {
            if url.trim().is_empty() {
                self.llm_base_url = None;
            }
        }
        if self.working_dir.trim().is_empty() {
            self.working_dir = default_working_dir();
        }
        if self.web_host.trim().is_empty() {
            self.web_host = default_web_host();
        }
        if let Some(token) = &self.web_auth_token {
            if token.trim().is_empty() {
                self.web_auth_token = None;
            }
        }
        if let Some(provider) = &self.embedding_provider {
            let p = provider.trim().to_lowercase();
            self.embedding_provider = if p.is_empty() { None } else { Some(p) };
        }
        if let Some(v) = &self.embedding_api_key {
            if v.trim().is_empty() {
                self.embedding_api_key = None;
            }
        }
        if let Some(v) = &self.embedding_base_url {
            if v.trim().is_empty() {
                self.embedding_base_url = None;
            }
        }
        if let Some(v) = &self.embedding_model {
            let m = v.trim().to_string();
            self.embedding_model = if m.is_empty() { None } else { Some(m) };
        }
        if let Some(v) = self.embedding_dim {
            if v == 0 {
                self.embedding_dim = None;
            }
        }
        if self.web_enabled && !is_local_web_host(&self.web_host) && self.web_auth_token.is_none() {
            return Err(RayClawError::Config(
                "web_auth_token is required when web_enabled=true and web_host is not local".into(),
            ));
        }
        if self.web_max_inflight_per_session == 0 {
            self.web_max_inflight_per_session = default_web_max_inflight_per_session();
        }
        if self.web_max_requests_per_window == 0 {
            self.web_max_requests_per_window = default_web_max_requests_per_window();
        }
        if self.web_rate_window_seconds == 0 {
            self.web_rate_window_seconds = default_web_rate_window_seconds();
        }
        if self.web_run_history_limit == 0 {
            self.web_run_history_limit = default_web_run_history_limit();
        }
        if self.web_session_idle_ttl_seconds == 0 {
            self.web_session_idle_ttl_seconds = default_web_session_idle_ttl_seconds();
        }
        if self.max_document_size_mb == 0 {
            self.max_document_size_mb = default_max_document_size_mb();
        }
        if self.memory_token_budget == 0 {
            self.memory_token_budget = default_memory_token_budget();
        }
        for price in &mut self.model_prices {
            price.model = price.model.trim().to_string();
            if price.model.is_empty() {
                return Err(RayClawError::Config(
                    "model_prices entries must include non-empty model".into(),
                ));
            }
            if !(price.input_per_million_usd.is_finite() && price.input_per_million_usd >= 0.0) {
                return Err(RayClawError::Config(format!(
                    "model_prices[{}].input_per_million_usd must be >= 0",
                    price.model
                )));
            }
            if !(price.output_per_million_usd.is_finite() && price.output_per_million_usd >= 0.0) {
                return Err(RayClawError::Config(format!(
                    "model_prices[{}].output_per_million_usd must be >= 0",
                    price.model
                )));
            }
        }

        // Allow env var override for skip_tool_approval
        if let Ok(val) = std::env::var("RAYCLAW_SKIP_TOOL_APPROVAL") {
            self.skip_tool_approval = matches!(val.as_str(), "1" | "true" | "yes");
        }

        Ok(())
    }

    /// Validate LLM provider and API key configuration.
    fn validate_llm(&self) -> Result<(), RayClawError> {
        if self.api_key.is_empty() && !provider_allows_empty_api_key(&self.llm_provider) {
            return Err(RayClawError::Config("api_key is required".into()));
        }
        if is_openai_codex_provider(&self.llm_provider) {
            if !self.api_key.trim().is_empty() {
                return Err(RayClawError::Config(
                    "openai-codex ignores rayclaw.config.yaml api_key. Configure ~/.codex/auth.json or run `codex login` instead.".into(),
                ));
            }
            if self
                .llm_base_url
                .as_ref()
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
            {
                return Err(RayClawError::Config(
                    "openai-codex ignores rayclaw.config.yaml llm_base_url. Configure ~/.codex/config.toml instead.".into(),
                ));
            }
            let has_codex_auth = codex_auth_file_has_access_token()?;
            if !has_codex_auth {
                return Err(RayClawError::Config(
                    "openai-codex requires ~/.codex/auth.json (access token or OPENAI_API_KEY), or OPENAI_CODEX_ACCESS_TOKEN. Run `codex login` or update Codex config files.".into(),
                ));
            }
        }
        Ok(())
    }

    /// Apply post-deserialization normalization and validation.
    pub(crate) fn post_deserialize(&mut self) -> Result<(), RayClawError> {
        self.normalize_fields()?;

        // Synthesize `channels` map from legacy flat fields if empty
        if self.channels.is_empty() {
            if !self.telegram_bot_token.trim().is_empty() {
                self.channels.insert(
                    "telegram".into(),
                    serde_yaml::to_value(serde_json::json!({
                        "bot_token": self.telegram_bot_token,
                        "bot_username": self.bot_username,
                        "allowed_groups": self.allowed_groups,
                    }))
                    .unwrap(),
                );
            }
            if let Some(ref token) = self.discord_bot_token {
                if !token.trim().is_empty() {
                    self.channels.insert(
                        "discord".into(),
                        serde_yaml::to_value(serde_json::json!({
                            "bot_token": token,
                            "allowed_channels": self.discord_allowed_channels,
                        }))
                        .unwrap(),
                    );
                }
            }
            if self.web_enabled {
                self.channels.insert(
                    "web".into(),
                    serde_yaml::to_value(serde_json::json!({
                        "enabled": true,
                        "host": self.web_host,
                        "port": self.web_port,
                        "auth_token": self.web_auth_token,
                    }))
                    .unwrap(),
                );
            }
        }

        // Validate required fields
        let has_telegram =
            !self.telegram_bot_token.trim().is_empty() || self.channels.contains_key("telegram");
        let has_discord = self
            .discord_bot_token
            .as_deref()
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
            || self.channels.contains_key("discord");
        let has_slack = self.channels.contains_key("slack");
        let has_feishu = self.channels.contains_key("feishu");
        let has_weixin = self.channels.contains_key("weixin");
        let has_web = self.web_enabled || self.channels.contains_key("web");

        if !(has_telegram || has_discord || has_slack || has_feishu || has_weixin || has_web) {
            return Err(RayClawError::Config(
                "At least one channel must be enabled: telegram_bot_token, discord_bot_token, channels.slack, channels.feishu, channels.weixin, or web_enabled=true".into(),
            ));
        }
        self.validate_llm()?;

        Ok(())
    }

    /// Deserialize a typed channel config from the `channels` map.
    pub fn channel_config<T: DeserializeOwned>(&self, name: &str) -> Option<T> {
        self.channels
            .get(name)
            .and_then(|v| serde_yaml::from_value(v.clone()).ok())
    }

    pub fn model_price(&self, model: &str) -> Option<&ModelPrice> {
        let needle = model.trim();
        self.model_prices
            .iter()
            .find(|p| p.model.eq_ignore_ascii_case(needle))
            .or_else(|| self.model_prices.iter().find(|p| p.model == "*"))
    }

    pub fn estimate_cost_usd(
        &self,
        model: &str,
        input_tokens: i64,
        output_tokens: i64,
    ) -> Option<f64> {
        let price = self.model_price(model)?;
        let in_tok = input_tokens.max(0) as f64;
        let out_tok = output_tokens.max(0) as f64;
        Some(
            (in_tok / 1_000_000.0) * price.input_per_million_usd
                + (out_tok / 1_000_000.0) * price.output_per_million_usd,
        )
    }

    /// Save config as YAML to the given path.
    #[allow(dead_code)]
    pub fn save_yaml(&self, path: &str) -> Result<(), RayClawError> {
        let content = serde_yaml::to_string(self)
            .map_err(|e| RayClawError::Config(format!("Failed to serialize config: {e}")))?;
        std::fs::write(path, content)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static ENV_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        ENV_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .expect("env lock poisoned")
    }

    pub fn test_config() -> Config {
        Config {
            telegram_bot_token: "tok".into(),
            bot_username: "bot".into(),
            llm_provider: "anthropic".into(),
            api_key: "key".into(),
            model: "claude-sonnet-4-5-20250929".into(),
            llm_base_url: None,
            max_tokens: 8192,
            prompt_cache_ttl: "none".into(),
            max_tool_iterations: 100,
            max_loop_repeats: 3,
            llm_idle_timeout_secs: 30,
            max_history_messages: 50,
            max_document_size_mb: 100,
            memory_token_budget: 1500,
            data_dir: "./rayclaw.data".into(),
            working_dir: "./tmp".into(),
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
            web_enabled: true,
            web_host: "127.0.0.1".into(),
            web_port: 10961,
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
            aws_region: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
            aws_profile: None,
            soul_path: None,
            skip_tool_approval: false,
            skills_dir: None,
            channels: HashMap::new(),
        }
    }

    #[test]
    fn test_config_struct_clone_and_debug() {
        let config = test_config();
        let cloned = config.clone();
        assert_eq!(cloned.telegram_bot_token, "tok");
        assert_eq!(cloned.max_tokens, 8192);
        assert_eq!(cloned.max_tool_iterations, 100);
        assert_eq!(cloned.max_history_messages, 50);
        assert_eq!(cloned.max_document_size_mb, 100);
        assert_eq!(cloned.memory_token_budget, 1500);
        assert!(cloned.openai_api_key.is_none());
        assert_eq!(cloned.timezone, "UTC");
        assert!(cloned.allowed_groups.is_empty());
        assert!(cloned.control_chat_ids.is_empty());
        assert_eq!(cloned.max_session_messages, 40);
        assert_eq!(cloned.compact_keep_recent, 20);
        assert!(cloned.discord_bot_token.is_none());
        assert!(cloned.discord_allowed_channels.is_empty());
        let _ = format!("{:?}", config);
    }

    #[test]
    fn test_config_default_values() {
        let mut config = test_config();
        config.openai_api_key = Some("sk-test".into());
        config.timezone = "US/Eastern".into();
        config.allowed_groups = vec![123, 456];
        config.control_chat_ids = vec![999];
        assert_eq!(config.model, "claude-sonnet-4-5-20250929");
        assert_eq!(config.data_dir, "./rayclaw.data");
        assert_eq!(config.working_dir, "./tmp");
        assert_eq!(config.openai_api_key.as_deref(), Some("sk-test"));
        assert_eq!(config.timezone, "US/Eastern");
        assert_eq!(config.allowed_groups, vec![123, 456]);
        assert_eq!(config.control_chat_ids, vec![999]);
    }

    #[test]
    fn test_config_yaml_roundtrip() {
        let config = test_config();
        let yaml = serde_yaml::to_string(&config).unwrap();
        let parsed: Config = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.telegram_bot_token, "tok");
        assert_eq!(parsed.max_tokens, 8192);
        assert_eq!(parsed.llm_provider, "anthropic");
    }

    #[test]
    fn test_config_yaml_defaults() {
        let yaml = "telegram_bot_token: tok\nbot_username: bot\napi_key: key\n";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.llm_provider, "anthropic");
        assert_eq!(config.max_tokens, 8192);
        assert_eq!(config.max_tool_iterations, 100);
        assert_eq!(config.data_dir, "./rayclaw.data");
        assert_eq!(config.working_dir, "./tmp");
        assert_eq!(config.memory_token_budget, 1500);
        assert!(matches!(
            config.working_dir_isolation,
            WorkingDirIsolation::Chat
        ));
        assert_eq!(config.max_document_size_mb, 100);
        assert_eq!(config.timezone, "UTC");
    }

    #[test]
    fn test_post_deserialize_empty_working_dir_uses_default() {
        let yaml = "telegram_bot_token: tok\nbot_username: bot\napi_key: key\nworking_dir: '  '\n";
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        config.post_deserialize().unwrap();
        assert_eq!(config.working_dir, "./tmp");
    }

    #[test]
    fn test_post_deserialize_zero_memory_budget_uses_default() {
        let yaml =
            "telegram_bot_token: tok\nbot_username: bot\napi_key: key\nmemory_token_budget: 0\n";
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        config.post_deserialize().unwrap();
        assert_eq!(config.memory_token_budget, 1500);
    }

    #[test]
    fn test_config_working_dir_isolation_defaults_to_chat() {
        let yaml = "telegram_bot_token: tok\nbot_username: bot\napi_key: key\n";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(
            config.working_dir_isolation,
            WorkingDirIsolation::Chat
        ));
    }

    #[test]
    fn test_config_working_dir_isolation_accepts_chat() {
        let yaml = "telegram_bot_token: tok\nbot_username: bot\napi_key: key\nworking_dir_isolation: chat\n";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(
            config.working_dir_isolation,
            WorkingDirIsolation::Chat
        ));
    }

    #[test]
    fn test_config_post_deserialize() {
        let yaml =
            "telegram_bot_token: tok\nbot_username: bot\napi_key: key\nllm_provider: ANTHROPIC\n";
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        config.post_deserialize().unwrap();
        assert_eq!(config.llm_provider, "anthropic");
        assert_eq!(config.model, "claude-sonnet-4-5-20250929");
    }

    #[test]
    fn test_runtime_and_skills_dirs_from_root_data_dir() {
        let mut config = test_config();
        config.data_dir = "./rayclaw.data".into();

        let runtime = std::path::PathBuf::from(config.runtime_data_dir());
        let skills = std::path::PathBuf::from(config.skills_data_dir());

        assert!(runtime.ends_with(std::path::Path::new("rayclaw.data").join("runtime")));
        assert!(skills.ends_with(std::path::Path::new("rayclaw.data").join("skills")));
    }

    #[test]
    fn test_runtime_and_skills_dirs_from_runtime_data_dir() {
        let mut config = test_config();
        config.data_dir = "./rayclaw.data/runtime".into();

        let runtime = std::path::PathBuf::from(config.runtime_data_dir());
        let skills = std::path::PathBuf::from(config.skills_data_dir());

        assert!(runtime.ends_with(
            std::path::Path::new("rayclaw.data")
                .join("runtime")
                .join("runtime")
        ));
        assert!(skills.ends_with(
            std::path::Path::new("rayclaw.data")
                .join("runtime")
                .join("skills")
        ));
    }

    #[test]
    fn test_post_deserialize_invalid_timezone() {
        let yaml =
            "telegram_bot_token: tok\nbot_username: bot\napi_key: key\ntimezone: Mars/Olympus\n";
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        let err = config.post_deserialize().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Invalid timezone"));
    }

    #[test]
    fn test_post_deserialize_missing_api_key() {
        let yaml = "telegram_bot_token: tok\nbot_username: bot\n";
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        let err = config.post_deserialize().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("api_key is required"));
    }

    #[test]
    fn test_post_deserialize_openai_codex_allows_empty_api_key() {
        let _guard = env_lock();
        let prev_codex_home = std::env::var("CODEX_HOME").ok();
        let prev_access = std::env::var("OPENAI_CODEX_ACCESS_TOKEN").ok();
        std::env::remove_var("OPENAI_CODEX_ACCESS_TOKEN");

        let auth_dir = std::env::temp_dir().join(format!(
            "rayclaw-codex-auth-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&auth_dir).unwrap();
        std::fs::write(
            auth_dir.join("auth.json"),
            r#"{"tokens":{"access_token":"tok"}}"#,
        )
        .unwrap();
        std::env::set_var("CODEX_HOME", &auth_dir);

        let yaml =
            "telegram_bot_token: tok\nbot_username: bot\nllm_provider: openai-codex\nmodel: gpt-5.3-codex\n";
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        config.post_deserialize().unwrap();

        if let Some(prev) = prev_codex_home {
            std::env::set_var("CODEX_HOME", prev);
        } else {
            std::env::remove_var("CODEX_HOME");
        }
        if let Some(prev) = prev_access {
            std::env::set_var("OPENAI_CODEX_ACCESS_TOKEN", prev);
        } else {
            std::env::remove_var("OPENAI_CODEX_ACCESS_TOKEN");
        }
        let _ = std::fs::remove_file(auth_dir.join("auth.json"));
        let _ = std::fs::remove_dir(auth_dir);
        assert_eq!(config.llm_provider, "openai-codex");
    }

    #[test]
    fn test_post_deserialize_missing_bot_tokens() {
        let yaml = "bot_username: bot\napi_key: key\nweb_enabled: false\n";
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        let err = config.post_deserialize().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("channel must be enabled"));
    }

    #[test]
    fn test_post_deserialize_discord_only() {
        let yaml = "bot_username: bot\napi_key: key\ndiscord_bot_token: discord_tok\n";
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        // Should succeed: discord_bot_token is set even though telegram_bot_token is empty
        config.post_deserialize().unwrap();
    }

    #[test]
    fn test_post_deserialize_openai_default_model() {
        let yaml =
            "telegram_bot_token: tok\nbot_username: bot\napi_key: key\nllm_provider: openai\n";
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        config.post_deserialize().unwrap();
        assert_eq!(config.model, "gpt-5.2");
    }

    #[test]
    fn test_post_deserialize_openai_codex_default_model() {
        let _guard = env_lock();
        let prev_codex_home = std::env::var("CODEX_HOME").ok();
        let prev_access = std::env::var("OPENAI_CODEX_ACCESS_TOKEN").ok();
        std::env::remove_var("OPENAI_CODEX_ACCESS_TOKEN");

        let auth_dir = std::env::temp_dir().join(format!(
            "rayclaw-codex-auth-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&auth_dir).unwrap();
        std::fs::write(
            auth_dir.join("auth.json"),
            r#"{"tokens":{"access_token":"tok"}}"#,
        )
        .unwrap();
        std::env::set_var("CODEX_HOME", &auth_dir);

        let yaml = "telegram_bot_token: tok\nbot_username: bot\nllm_provider: openai-codex\n";
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        config.post_deserialize().unwrap();

        if let Some(prev) = prev_codex_home {
            std::env::set_var("CODEX_HOME", prev);
        } else {
            std::env::remove_var("CODEX_HOME");
        }
        if let Some(prev) = prev_access {
            std::env::set_var("OPENAI_CODEX_ACCESS_TOKEN", prev);
        } else {
            std::env::remove_var("OPENAI_CODEX_ACCESS_TOKEN");
        }
        let _ = std::fs::remove_file(auth_dir.join("auth.json"));
        let _ = std::fs::remove_dir(auth_dir);
        assert_eq!(config.model, "gpt-5.3-codex");
    }

    #[test]
    fn test_post_deserialize_openai_codex_missing_oauth_token() {
        let _guard = env_lock();
        let prev_codex_home = std::env::var("CODEX_HOME").ok();
        let prev_access = std::env::var("OPENAI_CODEX_ACCESS_TOKEN").ok();
        std::env::remove_var("OPENAI_CODEX_ACCESS_TOKEN");

        let auth_dir = std::env::temp_dir().join(format!(
            "rayclaw-codex-auth-missing-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&auth_dir).unwrap();
        std::env::set_var("CODEX_HOME", &auth_dir);

        let yaml = "telegram_bot_token: tok\nbot_username: bot\nllm_provider: openai-codex\n";
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        let err = config.post_deserialize().unwrap_err();
        let msg = err.to_string();

        if let Some(prev) = prev_codex_home {
            std::env::set_var("CODEX_HOME", prev);
        } else {
            std::env::remove_var("CODEX_HOME");
        }
        if let Some(prev) = prev_access {
            std::env::set_var("OPENAI_CODEX_ACCESS_TOKEN", prev);
        } else {
            std::env::remove_var("OPENAI_CODEX_ACCESS_TOKEN");
        }
        let _ = std::fs::remove_dir(auth_dir);

        assert!(msg.contains("openai-codex requires ~/.codex/auth.json"));
    }

    #[test]
    fn test_post_deserialize_openai_codex_rejects_plain_api_key_without_oauth() {
        let _guard = env_lock();
        let prev_codex_home = std::env::var("CODEX_HOME").ok();
        let prev_access = std::env::var("OPENAI_CODEX_ACCESS_TOKEN").ok();
        std::env::remove_var("OPENAI_CODEX_ACCESS_TOKEN");

        let auth_dir = std::env::temp_dir().join(format!(
            "rayclaw-codex-auth-plain-key-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&auth_dir).unwrap();
        std::env::set_var("CODEX_HOME", &auth_dir);

        let yaml = "telegram_bot_token: tok\nbot_username: bot\nllm_provider: openai-codex\napi_key: sk-user-stale\n";
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        let err = config.post_deserialize().unwrap_err();
        let msg = err.to_string();

        if let Some(prev) = prev_codex_home {
            std::env::set_var("CODEX_HOME", prev);
        } else {
            std::env::remove_var("CODEX_HOME");
        }
        if let Some(prev) = prev_access {
            std::env::set_var("OPENAI_CODEX_ACCESS_TOKEN", prev);
        } else {
            std::env::remove_var("OPENAI_CODEX_ACCESS_TOKEN");
        }
        let _ = std::fs::remove_dir(auth_dir);

        assert!(msg.contains("ignores rayclaw.config.yaml api_key"));
    }

    #[test]
    fn test_post_deserialize_openai_codex_allows_env_access_token() {
        let _guard = env_lock();
        let prev_codex_home = std::env::var("CODEX_HOME").ok();
        let prev_access = std::env::var("OPENAI_CODEX_ACCESS_TOKEN").ok();
        std::env::remove_var("CODEX_HOME");
        std::env::set_var("OPENAI_CODEX_ACCESS_TOKEN", "env-token");

        let yaml = "telegram_bot_token: tok\nbot_username: bot\nllm_provider: openai-codex\n";
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        config.post_deserialize().unwrap();

        if let Some(prev) = prev_codex_home {
            std::env::set_var("CODEX_HOME", prev);
        } else {
            std::env::remove_var("CODEX_HOME");
        }
        if let Some(prev) = prev_access {
            std::env::set_var("OPENAI_CODEX_ACCESS_TOKEN", prev);
        } else {
            std::env::remove_var("OPENAI_CODEX_ACCESS_TOKEN");
        }

        assert_eq!(config.llm_provider, "openai-codex");
    }

    #[test]
    fn test_post_deserialize_ollama_default_model_and_empty_key() {
        let yaml = "telegram_bot_token: tok\nbot_username: bot\nllm_provider: ollama\n";
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        config.post_deserialize().unwrap();
        assert_eq!(config.model, "llama3.2");
    }

    #[test]
    fn test_post_deserialize_empty_base_url_becomes_none() {
        let yaml = "telegram_bot_token: tok\nbot_username: bot\napi_key: key\nllm_base_url: '  '\n";
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        config.post_deserialize().unwrap();
        assert!(config.llm_base_url.is_none());
    }

    #[test]
    fn test_post_deserialize_provider_case_insensitive() {
        let yaml = "telegram_bot_token: tok\nbot_username: bot\napi_key: key\nllm_provider: '  ANTHROPIC  '\n";
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        config.post_deserialize().unwrap();
        assert_eq!(config.llm_provider, "anthropic");
        assert_eq!(config.model, "claude-sonnet-4-5-20250929");
    }

    #[test]
    fn test_post_deserialize_web_non_local_requires_token() {
        let yaml = "telegram_bot_token: tok\nbot_username: bot\napi_key: key\nweb_enabled: true\nweb_host: 0.0.0.0\n";
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        let err = config.post_deserialize().unwrap_err();
        assert!(err
            .to_string()
            .contains("web_auth_token is required when web_enabled=true"));
    }

    #[test]
    fn test_post_deserialize_web_non_local_with_token_ok() {
        let yaml = "telegram_bot_token: tok\nbot_username: bot\napi_key: key\nweb_enabled: true\nweb_host: 0.0.0.0\nweb_auth_token: token123\n";
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        config.post_deserialize().unwrap();
        assert_eq!(config.web_auth_token.as_deref(), Some("token123"));
    }

    #[test]
    fn test_model_prices_parse_and_estimate() {
        let yaml = r#"
telegram_bot_token: tok
bot_username: bot
api_key: key
model_prices:
  - model: claude-sonnet-4-5-20250929
    input_per_million_usd: 3.0
    output_per_million_usd: 15.0
"#;
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        config.post_deserialize().unwrap();
        let est = config
            .estimate_cost_usd("claude-sonnet-4-5-20250929", 1000, 2000)
            .unwrap();
        assert!((est - 0.033).abs() < 1e-9);
    }

    #[test]
    fn test_model_prices_invalid_rejected() {
        let yaml = r#"
telegram_bot_token: tok
bot_username: bot
api_key: key
model_prices:
  - model: ""
    input_per_million_usd: 1.0
    output_per_million_usd: 1.0
"#;
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        let err = config.post_deserialize().unwrap_err();
        assert!(err
            .to_string()
            .contains("model_prices entries must include non-empty model"));
    }

    #[test]
    fn test_config_yaml_with_all_optional_fields() {
        let yaml = r#"
telegram_bot_token: tok
bot_username: bot
api_key: key
openai_api_key: sk-test
timezone: US/Eastern
allowed_groups: [123, 456]
control_chat_ids: [999]
max_session_messages: 60
compact_keep_recent: 30
discord_bot_token: discord_tok
discord_allowed_channels: [111, 222]
"#;
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        config.post_deserialize().unwrap();
        assert_eq!(config.openai_api_key.as_deref(), Some("sk-test"));
        assert_eq!(config.timezone, "US/Eastern");
        assert_eq!(config.allowed_groups, vec![123, 456]);
        assert_eq!(config.control_chat_ids, vec![999]);
        assert_eq!(config.max_session_messages, 60);
        assert_eq!(config.compact_keep_recent, 30);
        assert_eq!(config.discord_allowed_channels, vec![111, 222]);
    }

    #[test]
    fn test_config_save_yaml() {
        let config = test_config();
        let dir = std::env::temp_dir();
        let path = dir.join("rayclaw_test_config.yaml");
        config.save_yaml(path.to_str().unwrap()).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("telegram_bot_token"));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_prompt_cache_ttl_default() {
        let yaml = "telegram_bot_token: tok\nbot_username: bot\napi_key: key\n";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.prompt_cache_ttl, "none");
    }

    #[test]
    fn test_prompt_cache_ttl_from_yaml() {
        let yaml =
            "telegram_bot_token: tok\nbot_username: bot\napi_key: key\nprompt_cache_ttl: \"1h\"\n";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.prompt_cache_ttl, "1h");
    }

    #[test]
    fn test_prompt_cache_ttl_from_yaml_5m() {
        let yaml =
            "telegram_bot_token: tok\nbot_username: bot\napi_key: key\nprompt_cache_ttl: \"5m\"\n";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.prompt_cache_ttl, "5m");
    }
}
