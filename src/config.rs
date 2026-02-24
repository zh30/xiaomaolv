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
            if let Some(secret) = telegram.webhook_secret.as_mut() {
                *secret = resolve_env_placeholder(secret);
            }
            if let Some(mode) = telegram.mode.as_mut() {
                *mode = resolve_env_placeholder(mode);
            }
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
    pub webhook_secret: Option<String>,
    pub mode: Option<String>,
    #[serde(default = "default_polling_timeout_secs")]
    pub polling_timeout_secs: u64,
    #[serde(default = "default_telegram_streaming_enabled")]
    pub streaming_enabled: bool,
    #[serde(default = "default_telegram_streaming_edit_interval_ms")]
    pub streaming_edit_interval_ms: u64,
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

fn resolve_env_placeholder(value: &str) -> String {
    if value.starts_with("${") && value.ends_with('}') {
        let key = &value[2..value.len() - 1];
        if let Ok(env_value) = std::env::var(key) {
            return env_value;
        }
    }
    value.to_string()
}

fn resolve_env_map(map: &mut HashMap<String, String>) {
    for value in map.values_mut() {
        *value = resolve_env_placeholder(value);
    }
}
