use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub app: AppSettings,
    pub providers: HashMap<String, ProviderConfig>,
    pub channels: ChannelsConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub agent: AgentConfig,
}

impl AppConfig {
    pub fn from_toml(content: &str) -> anyhow::Result<Self> {
        let mut parsed: Self = toml::from_str(content).context("failed to parse config toml")?;
        parsed.resolve_env_placeholders();
        Ok(parsed)
    }

    pub async fn from_path(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path_ref = path.as_ref();
        let content = tokio::fs::read_to_string(path_ref)
            .await
            .with_context(|| format!("failed to read config file: {}", path_ref.display()))?;
        Self::from_toml(&content)
    }

    fn resolve_env_placeholders(&mut self) {
        for provider in self.providers.values_mut() {
            if let Some(api_key) = provider.api_key.as_mut() {
                *api_key = resolve_env_placeholder(api_key);
            }
            if let Some(base_url) = provider.base_url.as_mut() {
                *base_url = resolve_env_placeholder(base_url);
            }
            if let Some(model) = provider.model.as_mut() {
                *model = resolve_env_placeholder(model);
            }
            resolve_env_map(&mut provider.options);
        }

        if let Some(telegram) = self.channels.telegram.as_mut() {
            telegram.bot_token = resolve_env_placeholder(&telegram.bot_token);
            if let Some(username) = telegram.bot_username.as_mut() {
                *username = resolve_env_placeholder(username);
            }
            if let Some(secret) = telegram.webhook_secret.as_mut() {
                *secret = resolve_env_placeholder(secret);
            }
            if let Some(mode) = telegram.mode.as_mut() {
                *mode = resolve_env_placeholder(mode);
            }
            telegram.startup_online_text = resolve_env_placeholder(&telegram.startup_online_text);
            telegram.group_trigger_mode = resolve_env_placeholder(&telegram.group_trigger_mode);
            telegram.scheduler_default_timezone =
                resolve_env_placeholder(&telegram.scheduler_default_timezone);
            telegram.admin_user_ids.resolve_env_placeholders();
        }

        for channel in self.channels.plugins.values_mut() {
            resolve_env_map(&mut channel.settings);
        }

        self.memory.zvec.endpoint = resolve_env_placeholder(&self.memory.zvec.endpoint);
        self.memory.zvec.collection = resolve_env_placeholder(&self.memory.zvec.collection);
        self.memory.zvec.upsert_path = resolve_env_placeholder(&self.memory.zvec.upsert_path);
        self.memory.zvec.query_path = resolve_env_placeholder(&self.memory.zvec.query_path);
        if let Some(token) = self.memory.zvec.auth_bearer_token.as_mut() {
            *token = resolve_env_placeholder(token);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    pub bind: String,
    pub default_provider: String,
    #[serde(default = "default_max_history")]
    pub max_history: usize,
    #[serde(default = "default_concurrency")]
    pub concurrency_limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub kind: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_retries")]
    pub max_retries: usize,
    #[serde(default)]
    pub options: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelsConfig {
    pub http: HttpChannelConfig,
    pub telegram: Option<TelegramChannelConfig>,
    #[serde(default)]
    pub plugins: HashMap<String, ChannelPluginConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpChannelConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramChannelConfig {
    #[serde(default = "default_disabled")]
    pub enabled: bool,
    pub bot_token: String,
    pub bot_username: Option<String>,
    pub webhook_secret: Option<String>,
    pub mode: Option<String>,
    #[serde(default = "default_polling_timeout_secs")]
    pub polling_timeout_secs: u64,
    #[serde(default = "default_telegram_streaming_enabled")]
    pub streaming_enabled: bool,
    #[serde(default = "default_telegram_streaming_edit_interval_ms")]
    pub streaming_edit_interval_ms: u64,
    #[serde(default = "default_telegram_streaming_prefer_draft")]
    pub streaming_prefer_draft: bool,
    #[serde(default = "default_telegram_startup_online_enabled")]
    pub startup_online_enabled: bool,
    #[serde(default = "default_telegram_startup_online_text")]
    pub startup_online_text: String,
    #[serde(default = "default_telegram_commands_enabled")]
    pub commands_enabled: bool,
    #[serde(default = "default_telegram_commands_auto_register")]
    pub commands_auto_register: bool,
    #[serde(default = "default_telegram_commands_private_only")]
    pub commands_private_only: bool,
    #[serde(default)]
    pub admin_user_ids: TelegramAdminUserIds,
    #[serde(default = "default_telegram_group_trigger_mode")]
    pub group_trigger_mode: String,
    #[serde(default = "default_telegram_group_followup_window_secs")]
    pub group_followup_window_secs: u64,
    #[serde(default = "default_telegram_group_cooldown_secs")]
    pub group_cooldown_secs: u64,
    #[serde(default = "default_telegram_group_rule_min_score")]
    pub group_rule_min_score: u32,
    #[serde(default = "default_telegram_group_llm_gate_enabled")]
    pub group_llm_gate_enabled: bool,
    #[serde(default = "default_telegram_scheduler_enabled")]
    pub scheduler_enabled: bool,
    #[serde(default = "default_telegram_scheduler_tick_secs")]
    pub scheduler_tick_secs: u64,
    #[serde(default = "default_telegram_scheduler_batch_size")]
    pub scheduler_batch_size: usize,
    #[serde(default = "default_telegram_scheduler_lease_secs")]
    pub scheduler_lease_secs: u64,
    #[serde(default = "default_telegram_scheduler_default_timezone")]
    pub scheduler_default_timezone: String,
    #[serde(default = "default_telegram_scheduler_nl_enabled")]
    pub scheduler_nl_enabled: bool,
    #[serde(default = "default_telegram_scheduler_nl_min_confidence")]
    pub scheduler_nl_min_confidence: f32,
    #[serde(default = "default_telegram_scheduler_require_confirm")]
    pub scheduler_require_confirm: bool,
    #[serde(default = "default_telegram_scheduler_max_jobs_per_owner")]
    pub scheduler_max_jobs_per_owner: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TelegramAdminUserIds {
    Csv(String),
    List(Vec<i64>),
}

impl Default for TelegramAdminUserIds {
    fn default() -> Self {
        Self::List(vec![])
    }
}

impl TelegramAdminUserIds {
    fn resolve_env_placeholders(&mut self) {
        if let Self::Csv(raw) = self {
            *raw = resolve_env_placeholder(raw);
        }
    }

    pub fn to_csv(&self) -> String {
        match self {
            Self::Csv(raw) => raw.clone(),
            Self::List(ids) => ids
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(","),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelPluginConfig {
    pub kind: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub settings: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_memory_backend")]
    pub backend: String,
    #[serde(default)]
    pub max_recent_turns: usize,
    #[serde(default = "default_max_semantic_memories")]
    pub max_semantic_memories: usize,
    #[serde(default = "default_semantic_lookback_days")]
    pub semantic_lookback_days: u32,
    #[serde(default)]
    pub zvec: ZvecSidecarMemoryConfig,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            backend: default_memory_backend(),
            max_recent_turns: 0,
            max_semantic_memories: default_max_semantic_memories(),
            semantic_lookback_days: default_semantic_lookback_days(),
            zvec: ZvecSidecarMemoryConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default = "default_agent_mcp_enabled")]
    pub mcp_enabled: bool,
    #[serde(default = "default_agent_mcp_max_iterations")]
    pub mcp_max_iterations: usize,
    #[serde(default = "default_agent_mcp_max_tool_result_chars")]
    pub mcp_max_tool_result_chars: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            mcp_enabled: default_agent_mcp_enabled(),
            mcp_max_iterations: default_agent_mcp_max_iterations(),
            mcp_max_tool_result_chars: default_agent_mcp_max_tool_result_chars(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZvecSidecarMemoryConfig {
    #[serde(default = "default_sidecar_endpoint")]
    pub endpoint: String,
    #[serde(default = "default_zvec_collection")]
    pub collection: String,
    #[serde(default = "default_zvec_query_topk")]
    pub query_topk: usize,
    #[serde(default = "default_sidecar_timeout_secs")]
    pub request_timeout_secs: u64,
    #[serde(default = "default_zvec_upsert_path")]
    pub upsert_path: String,
    #[serde(default = "default_zvec_query_path")]
    pub query_path: String,
    pub auth_bearer_token: Option<String>,
}

impl Default for ZvecSidecarMemoryConfig {
    fn default() -> Self {
        Self {
            endpoint: default_sidecar_endpoint(),
            collection: default_zvec_collection(),
            query_topk: default_zvec_query_topk(),
            request_timeout_secs: default_sidecar_timeout_secs(),
            upsert_path: default_zvec_upsert_path(),
            query_path: default_zvec_query_path(),
            auth_bearer_token: None,
        }
    }
}

fn default_max_history() -> usize {
    16
}

fn default_concurrency() -> usize {
    128
}

fn default_timeout() -> u64 {
    30
}

fn default_retries() -> usize {
    2
}

fn default_enabled() -> bool {
    true
}

fn default_disabled() -> bool {
    false
}

fn default_polling_timeout_secs() -> u64 {
    30
}

fn default_telegram_streaming_enabled() -> bool {
    true
}

fn default_telegram_streaming_edit_interval_ms() -> u64 {
    900
}

fn default_telegram_streaming_prefer_draft() -> bool {
    true
}

fn default_telegram_startup_online_enabled() -> bool {
    false
}

fn default_telegram_startup_online_text() -> String {
    "online".to_string()
}

fn default_telegram_commands_enabled() -> bool {
    true
}

fn default_telegram_commands_auto_register() -> bool {
    true
}

fn default_telegram_commands_private_only() -> bool {
    true
}

fn default_telegram_group_trigger_mode() -> String {
    "smart".to_string()
}

fn default_telegram_group_followup_window_secs() -> u64 {
    180
}

fn default_telegram_group_cooldown_secs() -> u64 {
    20
}

fn default_telegram_group_rule_min_score() -> u32 {
    70
}

fn default_telegram_group_llm_gate_enabled() -> bool {
    false
}

fn default_telegram_scheduler_enabled() -> bool {
    true
}

fn default_telegram_scheduler_tick_secs() -> u64 {
    2
}

fn default_telegram_scheduler_batch_size() -> usize {
    8
}

fn default_telegram_scheduler_lease_secs() -> u64 {
    30
}

fn default_telegram_scheduler_default_timezone() -> String {
    "Asia/Shanghai".to_string()
}

fn default_telegram_scheduler_nl_enabled() -> bool {
    true
}

fn default_telegram_scheduler_nl_min_confidence() -> f32 {
    0.78
}

fn default_telegram_scheduler_require_confirm() -> bool {
    true
}

fn default_telegram_scheduler_max_jobs_per_owner() -> usize {
    64
}

fn default_memory_backend() -> String {
    "sqlite-only".to_string()
}

fn default_max_semantic_memories() -> usize {
    8
}

fn default_semantic_lookback_days() -> u32 {
    90
}

fn default_sidecar_endpoint() -> String {
    "http://127.0.0.1:3711".to_string()
}

fn default_zvec_collection() -> String {
    "agent_memory_v1".to_string()
}

fn default_zvec_query_topk() -> usize {
    20
}

fn default_sidecar_timeout_secs() -> u64 {
    3
}

fn default_zvec_upsert_path() -> String {
    "/v1/memory/upsert".to_string()
}

fn default_zvec_query_path() -> String {
    "/v1/memory/query".to_string()
}

fn default_agent_mcp_enabled() -> bool {
    true
}

fn default_agent_mcp_max_iterations() -> usize {
    4
}

fn default_agent_mcp_max_tool_result_chars() -> usize {
    4000
}

fn resolve_env_placeholder(value: &str) -> String {
    if value.starts_with("${") && value.ends_with('}') {
        let body = &value[2..value.len() - 1];
        let (key, default_value) = if let Some((k, default)) = body.split_once(":-") {
            (k, Some(default))
        } else {
            (body, None)
        };

        if let Ok(env_value) = std::env::var(key) {
            if !env_value.is_empty() {
                return env_value;
            }
        }

        if let Some(default) = default_value {
            return default.to_string();
        }
    }
    value.to_string()
}

fn resolve_env_map(map: &mut HashMap<String, String>) {
    for value in map.values_mut() {
        *value = resolve_env_placeholder(value);
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_env_placeholder;

    #[test]
    fn resolve_env_placeholder_keeps_literal_for_missing_env_without_default() {
        let value = "${XIAOMAOLV_TEST_UNSET_KEY_001}";
        assert_eq!(resolve_env_placeholder(value), value);
    }

    #[test]
    fn resolve_env_placeholder_uses_default_for_missing_env() {
        let value = "${XIAOMAOLV_TEST_UNSET_KEY_002:-MiniMax-M2.5-highspeed}";
        assert_eq!(
            resolve_env_placeholder(value),
            "MiniMax-M2.5-highspeed".to_string()
        );
    }

    #[test]
    fn resolve_env_placeholder_prefers_existing_env_over_default() {
        let (key, expected_value) = std::env::vars()
            .find(|(_, value)| !value.is_empty())
            .expect("expected at least one env var");
        let placeholder_with_default = format!("${{{key}:-fallback_value}}");
        assert_eq!(
            resolve_env_placeholder(&placeholder_with_default),
            expected_value
        );
    }
}
