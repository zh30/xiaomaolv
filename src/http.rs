use std::collections::{BTreeMap, HashMap};
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use axum::extract::{Path, Query, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex as AsyncMutex, RwLock, Semaphore, watch};
use tracing::warn;

use crate::channel::{
    ChannelContext, ChannelInbound, ChannelPlugin, ChannelPluginConfig, ChannelPluginError,
    ChannelRegistry, ChannelRuntimeContext, ChannelWorker,
};
use crate::code_mode::LlmCodeModePlanner;
use crate::config::AppConfig;
use crate::domain::IncomingMessage;
use crate::mcp::{McpConfigPaths, McpRegistry, McpRuntime};
use crate::memory::{
    HybridRetrievalOptions, HybridSqliteZvecMemoryBackend, MemoryBackend, SqliteMemoryBackend,
    SqliteMemoryStore, ZvecSidecarClient, ZvecSidecarConfig,
};
use crate::provider::{ChatProvider, ProviderRegistry};
use crate::service::{
    AgentMcpSettings, AgentSkillsSettings, AgentSwarmSettings, CodeModeDiagnostics, MessageService,
};
use crate::skills::{SkillConfigPaths, SkillRegistry, SkillRuntime};

const CODE_MODE_DIAG_OVERFLOW_SOURCE_KEY: &str = "__overflow__";

#[derive(Clone)]
struct RuntimeHandles {
    service: Arc<MessageService>,
    channel_plugins: Arc<HashMap<String, Arc<dyn ChannelPlugin>>>,
    mcp_runtime: Arc<RwLock<McpRuntime>>,
    locale: String,
}

#[derive(Clone)]
pub struct AppState {
    runtime: Arc<RwLock<RuntimeHandles>>,
    worker_supervisor: Arc<AsyncMutex<WorkerSupervisor>>,
    config_ui: Option<Arc<ConfigUiManager>>,
    pub semaphore: Arc<Semaphore>,
    pub code_mode_diag_bearer_token: Option<String>,
    code_mode_diag_rate_limiter: Arc<FixedWindowRateLimiter>,
}

struct WorkerSupervisor {
    shutdown_tx: watch::Sender<bool>,
    workers: Vec<ChannelWorker>,
}

impl WorkerSupervisor {
    fn empty() -> Self {
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        Self {
            shutdown_tx,
            workers: vec![],
        }
    }

    async fn replace(&mut self, shutdown_tx: watch::Sender<bool>, workers: Vec<ChannelWorker>) {
        let previous_shutdown = std::mem::replace(&mut self.shutdown_tx, shutdown_tx);
        let previous_workers = std::mem::replace(&mut self.workers, workers);
        let _ = previous_shutdown.send(true);
        stop_channel_workers(previous_workers).await;
    }

    async fn shutdown(&mut self) {
        let _ = self.shutdown_tx.send(true);
        let workers = std::mem::take(&mut self.workers);
        stop_channel_workers(workers).await;
    }
}

#[derive(Clone)]
struct ConfigUiManager {
    config_path: PathBuf,
    env_file_path: PathBuf,
    database_url: String,
    provider_override: Option<Arc<dyn ChatProvider>>,
    provider_registry: ProviderRegistry,
    channel_registry: ChannelRegistry,
}

impl ConfigUiManager {
    fn new(
        config_path: PathBuf,
        env_file_path: PathBuf,
        database_url: String,
        provider_override: Option<Arc<dyn ChatProvider>>,
        provider_registry: ProviderRegistry,
        channel_registry: ChannelRegistry,
    ) -> Self {
        Self {
            config_path,
            env_file_path,
            database_url,
            provider_override,
            provider_registry,
            channel_registry,
        }
    }
}

#[derive(Debug)]
struct FixedWindowSourceCounter {
    window_start_secs: u64,
    request_count: usize,
}

#[derive(Debug)]
struct FixedWindowRateLimiter {
    window_secs: u64,
    max_requests: usize,
    max_sources: usize,
    by_source: Mutex<HashMap<String, FixedWindowSourceCounter>>,
}

impl FixedWindowRateLimiter {
    fn new(window_secs: u64, max_requests: usize) -> Self {
        Self {
            window_secs: window_secs.max(1),
            max_requests,
            max_sources: 4096,
            by_source: Mutex::new(HashMap::new()),
        }
    }

    fn allow(&self, source_key: &str) -> bool {
        self.allow_at_for_source(current_unix_time_secs(), source_key)
    }

    fn allow_at_for_source(&self, now_secs: u64, source_key: &str) -> bool {
        if self.max_requests == 0 {
            return true;
        }

        let source = normalize_source_key(source_key);
        let current_window = active_window_start(now_secs, self.window_secs);
        let mut guard = match self.by_source.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        compact_source_windows_if_needed(
            &mut guard,
            current_window,
            self.window_secs,
            self.max_sources,
        );
        let has_overflow = guard.contains_key(CODE_MODE_DIAG_OVERFLOW_SOURCE_KEY);
        let tracked_regular_sources = guard.len().saturating_sub(usize::from(has_overflow));
        let effective_source = if guard.contains_key(&source) {
            source
        } else if tracked_regular_sources >= self.max_sources {
            CODE_MODE_DIAG_OVERFLOW_SOURCE_KEY.to_string()
        } else {
            source
        };

        let entry = guard
            .entry(effective_source)
            .or_insert(FixedWindowSourceCounter {
                window_start_secs: current_window,
                request_count: 0,
            });
        if entry.window_start_secs != current_window {
            entry.window_start_secs = current_window;
            entry.request_count = 0;
        }
        entry.request_count = entry.request_count.saturating_add(1);
        entry.request_count <= self.max_requests
    }
}

pub struct AppRuntime {
    pub router: Router,
    worker_supervisor: Arc<AsyncMutex<WorkerSupervisor>>,
}

#[derive(Clone)]
pub struct AppRuntimeHandle {
    worker_supervisor: Arc<AsyncMutex<WorkerSupervisor>>,
}

impl AppRuntimeHandle {
    pub async fn shutdown(self) {
        let mut supervisor = self.worker_supervisor.lock().await;
        supervisor.shutdown().await;
    }
}

impl AppRuntime {
    pub fn into_parts(self) -> (Router, AppRuntimeHandle) {
        (
            self.router,
            AppRuntimeHandle {
                worker_supervisor: self.worker_supervisor,
            },
        )
    }
}

pub async fn build_router(
    config: AppConfig,
    database_url: &str,
    provider_override: Option<Arc<dyn ChatProvider>>,
) -> anyhow::Result<Router> {
    build_router_with_registries(
        config,
        database_url,
        provider_override,
        ProviderRegistry::with_defaults(),
        ChannelRegistry::with_defaults(),
    )
    .await
}

pub async fn build_router_with_config_paths(
    config_path: impl AsRef<FsPath>,
    env_file_path: impl AsRef<FsPath>,
    database_url: &str,
    provider_override: Option<Arc<dyn ChatProvider>>,
) -> anyhow::Result<Router> {
    let config_path = config_path.as_ref().to_path_buf();
    let config = AppConfig::from_path(&config_path).await?;
    let provider_registry = ProviderRegistry::with_defaults();
    let channel_registry = ChannelRegistry::with_defaults();
    let worker_supervisor = Arc::new(AsyncMutex::new(WorkerSupervisor::empty()));
    let manager = Arc::new(ConfigUiManager::new(
        config_path,
        env_file_path.as_ref().to_path_buf(),
        database_url.to_string(),
        provider_override.clone(),
        provider_registry.clone(),
        channel_registry.clone(),
    ));
    let (state, http_enabled) = build_state(
        config,
        database_url,
        provider_override,
        provider_registry,
        channel_registry,
        worker_supervisor,
        Some(manager),
    )
    .await?;
    Ok(build_axum_router(state, http_enabled))
}

pub async fn build_app_runtime(
    config: AppConfig,
    database_url: &str,
    provider_override: Option<Arc<dyn ChatProvider>>,
) -> anyhow::Result<AppRuntime> {
    build_app_runtime_with_registries(
        config,
        database_url,
        provider_override,
        ProviderRegistry::with_defaults(),
        ChannelRegistry::with_defaults(),
    )
    .await
}

pub async fn build_app_runtime_with_config_paths(
    config_path: impl AsRef<FsPath>,
    env_file_path: impl AsRef<FsPath>,
    database_url: &str,
    provider_override: Option<Arc<dyn ChatProvider>>,
) -> anyhow::Result<AppRuntime> {
    let config_path = config_path.as_ref().to_path_buf();
    let config = AppConfig::from_path(&config_path).await?;
    let provider_registry = ProviderRegistry::with_defaults();
    let channel_registry = ChannelRegistry::with_defaults();
    let worker_supervisor = Arc::new(AsyncMutex::new(WorkerSupervisor::empty()));
    let manager = Arc::new(ConfigUiManager::new(
        config_path,
        env_file_path.as_ref().to_path_buf(),
        database_url.to_string(),
        provider_override.clone(),
        provider_registry.clone(),
        channel_registry.clone(),
    ));
    let (state, http_enabled) = build_state(
        config,
        database_url,
        provider_override,
        provider_registry,
        channel_registry,
        worker_supervisor.clone(),
        Some(manager),
    )
    .await?;

    let runtime = state.runtime.read().await.clone();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let workers = start_channel_workers(&runtime, shutdown_rx).await?;
    {
        let mut guard = worker_supervisor.lock().await;
        guard.replace(shutdown_tx, workers).await;
    }

    let router = build_axum_router(state, http_enabled);
    Ok(AppRuntime {
        router,
        worker_supervisor,
    })
}

pub async fn build_router_with_registries(
    config: AppConfig,
    database_url: &str,
    provider_override: Option<Arc<dyn ChatProvider>>,
    provider_registry: ProviderRegistry,
    channel_registry: ChannelRegistry,
) -> anyhow::Result<Router> {
    let worker_supervisor = Arc::new(AsyncMutex::new(WorkerSupervisor::empty()));
    let (state, http_enabled) = build_state(
        config,
        database_url,
        provider_override,
        provider_registry,
        channel_registry,
        worker_supervisor,
        None,
    )
    .await?;
    Ok(build_axum_router(state, http_enabled))
}

pub async fn build_app_runtime_with_registries(
    config: AppConfig,
    database_url: &str,
    provider_override: Option<Arc<dyn ChatProvider>>,
    provider_registry: ProviderRegistry,
    channel_registry: ChannelRegistry,
) -> anyhow::Result<AppRuntime> {
    let worker_supervisor = Arc::new(AsyncMutex::new(WorkerSupervisor::empty()));
    let (state, http_enabled) = build_state(
        config,
        database_url,
        provider_override,
        provider_registry,
        channel_registry,
        worker_supervisor.clone(),
        None,
    )
    .await?;

    let runtime = state.runtime.read().await.clone();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let workers = start_channel_workers(&runtime, shutdown_rx).await?;
    {
        let mut guard = worker_supervisor.lock().await;
        guard.replace(shutdown_tx, workers).await;
    }
    let router = build_axum_router(state, http_enabled);

    Ok(AppRuntime {
        router,
        worker_supervisor,
    })
}

async fn build_state(
    config: AppConfig,
    database_url: &str,
    provider_override: Option<Arc<dyn ChatProvider>>,
    provider_registry: ProviderRegistry,
    channel_registry: ChannelRegistry,
    worker_supervisor: Arc<AsyncMutex<WorkerSupervisor>>,
    config_ui: Option<Arc<ConfigUiManager>>,
) -> anyhow::Result<(AppState, bool)> {
    let http_enabled = config.channels.http.enabled;
    let runtime = build_runtime_handles(
        &config,
        database_url,
        provider_override,
        &provider_registry,
        &channel_registry,
    )
    .await?;

    let state =
        AppState {
            runtime: Arc::new(RwLock::new(runtime)),
            worker_supervisor,
            config_ui,
            semaphore: Arc::new(Semaphore::new(config.app.concurrency_limit.max(1))),
            code_mode_diag_bearer_token: config.channels.http.diag_bearer_token.clone().and_then(
                |v| {
                    let trimmed = v.trim().to_string();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed)
                    }
                },
            ),
            code_mode_diag_rate_limiter: Arc::new(FixedWindowRateLimiter::new(
                60,
                config.channels.http.diag_rate_limit_per_minute,
            )),
        };

    Ok((state, http_enabled))
}

async fn build_runtime_handles(
    config: &AppConfig,
    database_url: &str,
    provider_override: Option<Arc<dyn ChatProvider>>,
    provider_registry: &ProviderRegistry,
    channel_registry: &ChannelRegistry,
) -> anyhow::Result<RuntimeHandles> {
    let provider = if let Some(p) = provider_override {
        p
    } else {
        let provider_cfg = config
            .providers
            .get(&config.app.default_provider)
            .with_context(|| {
                format!(
                    "default provider '{}' not found in config",
                    config.app.default_provider
                )
            })?;
        provider_registry.build(&config.app.default_provider, provider_cfg)?
    };

    let memory_store = SqliteMemoryStore::new(database_url).await?;
    let max_recent_turns = if config.memory.max_recent_turns == 0 {
        config.app.max_history
    } else {
        config.memory.max_recent_turns
    };

    let memory_backend: Arc<dyn MemoryBackend> = match config.memory.backend.as_str() {
        "sqlite-only" => Arc::new(SqliteMemoryBackend::new(memory_store.clone())),
        "hybrid-sqlite-zvec" => {
            let sidecar = ZvecSidecarClient::new(ZvecSidecarConfig {
                endpoint: config.memory.zvec.endpoint.clone(),
                collection: config.memory.zvec.collection.clone(),
                query_topk: config.memory.zvec.query_topk,
                request_timeout_secs: config.memory.zvec.request_timeout_secs,
                upsert_path: config.memory.zvec.upsert_path.clone(),
                query_path: config.memory.zvec.query_path.clone(),
                auth_bearer_token: config.memory.zvec.auth_bearer_token.clone(),
            });
            Arc::new(HybridSqliteZvecMemoryBackend::with_options(
                memory_store.clone(),
                sidecar,
                HybridRetrievalOptions {
                    keyword_enabled: config.memory.hybrid_keyword_enabled,
                    keyword_topk: config.memory.hybrid_keyword_topk,
                    keyword_candidate_limit: config.memory.hybrid_keyword_candidate_limit,
                    memory_snippet_max_chars: config.memory.hybrid_memory_snippet_max_chars,
                    min_score: config.memory.hybrid_min_score,
                },
            ))
        }
        other => bail!(
            "unsupported memory.backend '{}', expected sqlite-only|hybrid-sqlite-zvec",
            other
        ),
    };

    let mcp_runtime = Arc::new(RwLock::new(load_mcp_runtime().await));
    let skills_runtime = Arc::new(RwLock::new(load_skill_runtime().await));
    let code_mode_planner = Arc::new(LlmCodeModePlanner::new(provider.clone()));
    let service = Arc::new(
        MessageService::new_with_backend(
            provider,
            memory_backend,
            Some(mcp_runtime.clone()),
            AgentMcpSettings {
                enabled: config.agent.mcp_enabled,
                max_iterations: config.agent.mcp_max_iterations,
                max_tool_result_chars: config.agent.mcp_max_tool_result_chars,
            },
            max_recent_turns,
            config.memory.max_semantic_memories,
            config.memory.semantic_lookback_days,
        )
        .with_context_budget(
            config.memory.context_window_tokens,
            config.memory.context_reserved_tokens,
            config.memory.context_memory_budget_ratio,
            config.memory.context_min_recent_messages,
        )
        .with_agent_code_mode(config.agent.code_mode.clone())
        .with_code_mode_planner(code_mode_planner)
        .with_agent_skills(
            Some(skills_runtime.clone()),
            AgentSkillsSettings {
                enabled: config.agent.skills_enabled,
                max_selected: config.agent.skills_max_selected,
                max_prompt_chars: config.agent.skills_max_prompt_chars,
                match_min_score: config.agent.skills_match_min_score,
                llm_rerank_enabled: config.agent.skills_llm_rerank_enabled,
            },
        )
        .with_agent_swarm(AgentSwarmSettings {
            enabled: config.agent.swarm.enabled,
            auto_detect: config.agent.swarm.auto_detect,
            max_depth: config.agent.swarm.max_depth,
            max_agents: config.agent.swarm.max_agents,
            max_parallel: config.agent.swarm.max_parallel,
            max_node_timeout_ms: config.agent.swarm.max_node_timeout_ms,
            max_run_timeout_ms: config.agent.swarm.max_run_timeout_ms,
            reply_summary_enabled: config.agent.swarm.reply_summary_enabled,
            audit_retention_days: config.agent.swarm.audit_retention_days,
        }),
    );

    let channel_plugins = load_channel_plugins(config, channel_registry)?;
    Ok(RuntimeHandles {
        service,
        channel_plugins: Arc::new(channel_plugins),
        mcp_runtime,
        locale: std::env::var("XIAOMAOLV_LOCALE")
            .ok()
            .and_then(|value| normalize_supported_locale(&value))
            .or_else(|| normalize_supported_locale(&config.app.locale))
            .unwrap_or_else(|| "en-US".to_string()),
    })
}

impl AppState {
    async fn reload_runtime_from_config(&self, manager: &ConfigUiManager) -> anyhow::Result<()> {
        let config = AppConfig::from_path(&manager.config_path).await?;
        let runtime = build_runtime_handles(
            &config,
            &manager.database_url,
            manager.provider_override.clone(),
            &manager.provider_registry,
            &manager.channel_registry,
        )
        .await?;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let workers = start_channel_workers(&runtime, shutdown_rx).await?;

        {
            let mut current_runtime = self.runtime.write().await;
            *current_runtime = runtime;
        }

        let mut supervisor = self.worker_supervisor.lock().await;
        supervisor.replace(shutdown_tx, workers).await;

        Ok(())
    }
}

fn build_axum_router(state: AppState, http_enabled: bool) -> Router {
    let mut router = Router::new()
        .route("/health", get(health))
        .route("/setup", get(get_setup_page))
        .route("/v1/config/ui/state", get(get_config_ui_state))
        .route("/v1/config/ui/save", post(post_config_ui_save));

    if http_enabled {
        router = router
            .route("/v1/messages", post(post_message))
            .route("/v1/code-mode/diag", get(get_code_mode_diag))
            .route("/v1/code-mode/metrics", get(get_code_mode_metrics));
    }

    router
        .route("/v1/mcp/servers", get(get_mcp_servers))
        .route("/v1/mcp/tools", get(get_mcp_tools))
        .route("/v1/mcp/tools/{server}/{tool}", post(post_mcp_tool_call))
        .route("/v1/channels/{channel}/mode", get(get_channel_mode))
        .route("/v1/channels/{channel}/diag", get(get_channel_diag))
        .route("/v1/channels/{channel}/inbound", post(post_channel_inbound))
        .route(
            "/v1/channels/{channel}/inbound/{secret}",
            post(post_channel_inbound_with_secret),
        )
        .with_state(state)
}

async fn start_channel_workers(
    runtime: &RuntimeHandles,
    shutdown_rx: watch::Receiver<bool>,
) -> anyhow::Result<Vec<ChannelWorker>> {
    let mut workers = Vec::new();

    for (channel_name, plugin) in runtime.channel_plugins.iter() {
        let worker = plugin
            .start_background(ChannelRuntimeContext {
                service: runtime.service.clone(),
                channel_name: channel_name.clone(),
                shutdown: shutdown_rx.clone(),
            })
            .await
            .with_context(|| format!("failed to start channel worker for '{channel_name}'"))?;

        if let Some(worker) = worker {
            workers.push(worker);
        }
    }

    Ok(workers)
}

async fn stop_channel_workers(workers: Vec<ChannelWorker>) {
    for worker in workers {
        worker.task.abort();
        let _ = worker.task.await;
    }
}

fn load_channel_plugins(
    config: &AppConfig,
    channel_registry: &ChannelRegistry,
) -> anyhow::Result<HashMap<String, Arc<dyn ChannelPlugin>>> {
    let mut plugins = HashMap::new();

    for (name, channel_cfg) in &config.channels.plugins {
        if !channel_cfg.enabled {
            continue;
        }

        let plugin = channel_registry.build(name, channel_cfg)?;
        plugins.insert(name.clone(), plugin);
    }

    if let Some(telegram) = &config.channels.telegram
        && telegram.enabled
    {
        if is_missing_required_value(&telegram.bot_token) {
            warn!(
                "channels.telegram.enabled=true but bot_token is missing placeholder value; skip telegram worker until configured"
            );
            return Ok(plugins);
        }

        if plugins.contains_key("telegram") {
            anyhow::bail!(
                "channel instance 'telegram' is duplicated. disable channels.telegram or rename channels.plugins.telegram"
            );
        }

        let mut settings = HashMap::new();
        settings.insert("bot_token".to_string(), telegram.bot_token.clone());
        if let Some(username) = &telegram.bot_username
            && !username.trim().is_empty()
        {
            settings.insert("bot_username".to_string(), username.clone());
        }

        settings.insert(
            "polling_timeout_secs".to_string(),
            telegram.polling_timeout_secs.to_string(),
        );
        settings.insert(
            "streaming_enabled".to_string(),
            telegram.streaming_enabled.to_string(),
        );
        settings.insert(
            "streaming_edit_interval_ms".to_string(),
            telegram.streaming_edit_interval_ms.to_string(),
        );
        settings.insert(
            "streaming_prefer_draft".to_string(),
            telegram.streaming_prefer_draft.to_string(),
        );
        settings.insert(
            "startup_online_enabled".to_string(),
            telegram.startup_online_enabled.to_string(),
        );
        settings.insert(
            "startup_online_text".to_string(),
            telegram.startup_online_text.clone(),
        );
        settings.insert(
            "commands_enabled".to_string(),
            telegram.commands_enabled.to_string(),
        );
        settings.insert(
            "commands_auto_register".to_string(),
            telegram.commands_auto_register.to_string(),
        );
        settings.insert(
            "commands_private_only".to_string(),
            telegram.commands_private_only.to_string(),
        );
        settings.insert(
            "admin_user_ids".to_string(),
            telegram.admin_user_ids.to_csv(),
        );
        settings.insert(
            "group_trigger_mode".to_string(),
            telegram.group_trigger_mode.clone(),
        );
        settings.insert(
            "group_followup_window_secs".to_string(),
            telegram.group_followup_window_secs.to_string(),
        );
        settings.insert(
            "group_cooldown_secs".to_string(),
            telegram.group_cooldown_secs.to_string(),
        );
        settings.insert(
            "group_rule_min_score".to_string(),
            telegram.group_rule_min_score.to_string(),
        );
        settings.insert(
            "group_llm_gate_enabled".to_string(),
            telegram.group_llm_gate_enabled.to_string(),
        );
        settings.insert(
            "scheduler_enabled".to_string(),
            telegram.scheduler_enabled.to_string(),
        );
        settings.insert(
            "scheduler_tick_secs".to_string(),
            telegram.scheduler_tick_secs.to_string(),
        );
        settings.insert(
            "scheduler_batch_size".to_string(),
            telegram.scheduler_batch_size.to_string(),
        );
        settings.insert(
            "scheduler_lease_secs".to_string(),
            telegram.scheduler_lease_secs.to_string(),
        );
        settings.insert(
            "scheduler_default_timezone".to_string(),
            telegram.scheduler_default_timezone.clone(),
        );
        settings.insert(
            "scheduler_nl_enabled".to_string(),
            telegram.scheduler_nl_enabled.to_string(),
        );
        settings.insert(
            "scheduler_nl_min_confidence".to_string(),
            telegram.scheduler_nl_min_confidence.to_string(),
        );
        settings.insert(
            "scheduler_require_confirm".to_string(),
            telegram.scheduler_require_confirm.to_string(),
        );
        settings.insert(
            "scheduler_max_jobs_per_owner".to_string(),
            telegram.scheduler_max_jobs_per_owner.to_string(),
        );

        let cfg = ChannelPluginConfig {
            kind: "telegram".to_string(),
            enabled: true,
            settings,
        };

        let plugin = channel_registry.build("telegram", &cfg)?;
        plugins.insert("telegram".to_string(), plugin);
    }

    Ok(plugins)
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn load_mcp_runtime() -> McpRuntime {
    let cwd = match std::env::current_dir() {
        Ok(path) => path,
        Err(err) => {
            warn!(error = %err, "failed to resolve current dir while loading mcp registry, using empty runtime");
            return McpRuntime::default();
        }
    };

    let paths = match McpConfigPaths::discover(&cwd) {
        Ok(paths) => paths,
        Err(err) => {
            warn!(error = %err, "failed to resolve mcp config paths, using empty runtime");
            return McpRuntime::default();
        }
    };

    let registry = McpRegistry::new(paths);
    match McpRuntime::from_registry(&registry).await {
        Ok(runtime) => runtime,
        Err(err) => {
            warn!(error = %err, "failed to load mcp runtime, using empty runtime");
            McpRuntime::default()
        }
    }
}

async fn load_skill_runtime() -> SkillRuntime {
    let cwd = match std::env::current_dir() {
        Ok(path) => path,
        Err(err) => {
            warn!(error = %err, "failed to resolve current dir while loading skill registry, using empty runtime");
            return SkillRuntime::default();
        }
    };

    let paths = match SkillConfigPaths::discover(&cwd) {
        Ok(paths) => paths,
        Err(err) => {
            warn!(error = %err, "failed to resolve skill config paths, using empty runtime");
            return SkillRuntime::default();
        }
    };

    let registry = match SkillRegistry::new(paths) {
        Ok(registry) => registry,
        Err(err) => {
            warn!(error = %err, "failed to initialize skill registry, using empty runtime");
            return SkillRuntime::default();
        }
    };

    match SkillRuntime::from_registry(&registry).await {
        Ok(runtime) => runtime,
        Err(err) => {
            warn!(error = %err, "failed to load skill runtime, using empty runtime");
            SkillRuntime::default()
        }
    }
}

#[derive(Debug, Deserialize)]
struct HttpMessageRequest {
    session_id: String,
    user_id: String,
    text: String,
}

#[derive(Debug, Serialize)]
struct HttpMessageResponse {
    reply: String,
}

#[derive(Debug, Serialize)]
struct McpServersResponse {
    servers: Vec<crate::mcp::McpServerView>,
}

#[derive(Debug, Deserialize)]
struct McpToolsQuery {
    server: Option<String>,
}

#[derive(Debug, Serialize)]
struct McpToolsResponse {
    tools: Vec<crate::mcp::McpToolInfo>,
}

#[derive(Debug, Serialize)]
struct McpCallResponse {
    server: String,
    tool: String,
    result: serde_json::Value,
}

#[derive(Debug, Clone, Copy)]
struct ConfigUiFieldSpec {
    key: &'static str,
    group_key: &'static str,
    required: bool,
    sensitive: bool,
    default_value: &'static str,
    placeholder: &'static str,
}

#[derive(Debug, Serialize)]
struct ConfigUiFieldView {
    key: String,
    label: String,
    category: String,
    description: String,
    required: bool,
    sensitive: bool,
    value: String,
    placeholder: String,
}

#[derive(Debug, Serialize)]
struct ConfigUiStateResponse {
    first_time: bool,
    required_keys: Vec<String>,
    fields: Vec<ConfigUiFieldView>,
    ui_locale: String,
    global_locale: String,
    available_locales: Vec<ConfigUiLocaleOption>,
    messages: HashMap<String, String>,
}

#[derive(Debug, Serialize)]
struct ConfigUiLocaleOption {
    code: String,
    label: String,
}

#[derive(Debug, Deserialize)]
struct ConfigUiStateQuery {
    locale: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ConfigUiSaveRequest {
    values: HashMap<String, String>,
    mode: Option<String>,
}

#[derive(Debug, Serialize)]
struct ConfigUiSaveResponse {
    saved: bool,
    runtime_reloaded: bool,
    first_time: bool,
    applied_at_unix: u64,
    message: String,
}

const CONFIG_UI_FIELDS: &[ConfigUiFieldSpec] = &[
    ConfigUiFieldSpec {
        key: "MINIMAX_API_KEY",
        group_key: "required",
        required: true,
        sensitive: true,
        default_value: "",
        placeholder: "replace_with_your_minimax_coding_plan_key",
    },
    ConfigUiFieldSpec {
        key: "TELEGRAM_BOT_TOKEN",
        group_key: "required",
        required: true,
        sensitive: true,
        default_value: "",
        placeholder: "replace_with_your_telegram_bot_token",
    },
    ConfigUiFieldSpec {
        key: "MINIMAX_MODEL",
        group_key: "model",
        required: false,
        sensitive: false,
        default_value: "MiniMax-M2.5-highspeed",
        placeholder: "MiniMax-M2.5-highspeed",
    },
    ConfigUiFieldSpec {
        key: "TELEGRAM_BOT_USERNAME",
        group_key: "telegram",
        required: false,
        sensitive: false,
        default_value: "",
        placeholder: "your_bot_username",
    },
    ConfigUiFieldSpec {
        key: "TELEGRAM_STARTUP_ONLINE_TEXT",
        group_key: "telegram",
        required: false,
        sensitive: false,
        default_value: "online",
        placeholder: "online",
    },
    ConfigUiFieldSpec {
        key: "TELEGRAM_GROUP_TRIGGER_MODE",
        group_key: "group",
        required: false,
        sensitive: false,
        default_value: "smart",
        placeholder: "smart",
    },
    ConfigUiFieldSpec {
        key: "TELEGRAM_SCHEDULER_DEFAULT_TIMEZONE",
        group_key: "scheduler",
        required: false,
        sensitive: false,
        default_value: "Asia/Shanghai",
        placeholder: "Asia/Shanghai",
    },
    ConfigUiFieldSpec {
        key: "TELEGRAM_ADMIN_USER_IDS",
        group_key: "scheduler",
        required: false,
        sensitive: false,
        default_value: "",
        placeholder: "123456789,987654321",
    },
    ConfigUiFieldSpec {
        key: "XIAOMAOLV_LOCALE",
        group_key: "system",
        required: false,
        sensitive: false,
        default_value: "en-US",
        placeholder: "en-US",
    },
    ConfigUiFieldSpec {
        key: "HTTP_DIAG_BEARER_TOKEN",
        group_key: "diagnostics",
        required: false,
        sensitive: true,
        default_value: "",
        placeholder: "diag-secret-token",
    },
    ConfigUiFieldSpec {
        key: "ZVEC_SIDECAR_ENDPOINT",
        group_key: "memory",
        required: false,
        sensitive: false,
        default_value: "http://127.0.0.1:3711",
        placeholder: "http://127.0.0.1:3711",
    },
    ConfigUiFieldSpec {
        key: "ZVEC_SIDECAR_TOKEN",
        group_key: "memory",
        required: false,
        sensitive: true,
        default_value: "",
        placeholder: "replace_with_sidecar_bearer_token",
    },
];

fn config_ui_field_spec(key: &str) -> Option<&'static ConfigUiFieldSpec> {
    CONFIG_UI_FIELDS.iter().find(|spec| spec.key == key)
}

const SUPPORTED_UI_LOCALES: &[&str] = &["en-US", "zh-CN", "hi-IN", "es-ES", "ar"];

fn normalize_supported_locale(raw: &str) -> Option<String> {
    let normalized = raw.trim().to_ascii_lowercase();
    let locale = match normalized.as_str() {
        "en" | "en-us" => "en-US",
        "zh" | "zh-cn" | "zh-hans" => "zh-CN",
        "hi" | "hi-in" => "hi-IN",
        "es" | "es-es" | "es-419" => "es-ES",
        "ar" | "ar-sa" | "ar-eg" => "ar",
        _ => return None,
    };
    Some(locale.to_string())
}

fn locale_family(locale: &str) -> &'static str {
    let lower = locale.to_ascii_lowercase();
    if lower.starts_with("zh") {
        "zh"
    } else if lower.starts_with("hi") {
        "hi"
    } else if lower.starts_with("es") {
        "es"
    } else if lower.starts_with("ar") {
        "ar"
    } else {
        "en"
    }
}

fn locale_label(locale: &str) -> &'static str {
    match locale {
        "zh-CN" => "中文",
        "hi-IN" => "हिन्दी",
        "es-ES" => "Español",
        "ar" => "العربية",
        _ => "English",
    }
}

fn localized_group_name(group_key: &str, locale: &str) -> &'static str {
    match (group_key, locale_family(locale)) {
        ("required", "zh") => "1. 首次必填",
        ("required", "hi") => "1. आवश्यक शुरुआत",
        ("required", "es") => "1. Requerido al inicio",
        ("required", "ar") => "1. متطلبات البداية",
        ("required", _) => "1. Required First Step",
        ("model", "zh") => "2. 模型与服务",
        ("model", "hi") => "2. मॉडल और सेवा",
        ("model", "es") => "2. Modelo y servicio",
        ("model", "ar") => "2. النموذج والخدمة",
        ("model", _) => "2. Model & Service",
        ("telegram", "zh") => "3. Telegram 基础",
        ("telegram", "hi") => "3. Telegram आधार",
        ("telegram", "es") => "3. Base de Telegram",
        ("telegram", "ar") => "3. أساسيات Telegram",
        ("telegram", _) => "3. Telegram Basics",
        ("group", "zh") => "4. 群组策略",
        ("group", "hi") => "4. समूह रणनीति",
        ("group", "es") => "4. Política de grupos",
        ("group", "ar") => "4. سياسة المجموعات",
        ("group", _) => "4. Group Policy",
        ("scheduler", "zh") => "5. 调度与权限",
        ("scheduler", "hi") => "5. शेड्यूल और अनुमति",
        ("scheduler", "es") => "5. Programación y permisos",
        ("scheduler", "ar") => "5. الجدولة والصلاحيات",
        ("scheduler", _) => "5. Scheduler & Access",
        ("system", "zh") => "6. 系统语言",
        ("system", "hi") => "6. सिस्टम भाषा",
        ("system", "es") => "6. Idioma del sistema",
        ("system", "ar") => "6. لغة النظام",
        ("system", _) => "6. System Language",
        ("diagnostics", "zh") => "7. 诊断与观测",
        ("diagnostics", "hi") => "7. डायग्नोस्टिक्स",
        ("diagnostics", "es") => "7. Diagnóstico",
        ("diagnostics", "ar") => "7. التشخيص",
        ("diagnostics", _) => "7. Diagnostics",
        ("memory", "zh") => "8. 混合记忆",
        ("memory", "hi") => "8. हाइब्रिड मेमोरी",
        ("memory", "es") => "8. Memoria híbrida",
        ("memory", "ar") => "8. الذاكرة الهجينة",
        ("memory", _) => "8. Hybrid Memory",
        (_, _) => "Other",
    }
}

fn localized_field_label(key: &str, locale: &str) -> &'static str {
    match (key, locale_family(locale)) {
        ("MINIMAX_API_KEY", "zh") => "MiniMax API 密钥",
        ("MINIMAX_API_KEY", "hi") => "MiniMax API कुंजी",
        ("MINIMAX_API_KEY", "es") => "Clave API de MiniMax",
        ("MINIMAX_API_KEY", "ar") => "مفتاح MiniMax API",
        ("MINIMAX_API_KEY", _) => "MiniMax API Key",
        ("TELEGRAM_BOT_TOKEN", "zh") => "Telegram 机器人 Token",
        ("TELEGRAM_BOT_TOKEN", "hi") => "Telegram Bot टोकन",
        ("TELEGRAM_BOT_TOKEN", "es") => "Token del bot de Telegram",
        ("TELEGRAM_BOT_TOKEN", "ar") => "رمز Telegram Bot",
        ("TELEGRAM_BOT_TOKEN", _) => "Telegram Bot Token",
        ("MINIMAX_MODEL", "zh") => "MiniMax 模型",
        ("MINIMAX_MODEL", "hi") => "MiniMax मॉडल",
        ("MINIMAX_MODEL", "es") => "Modelo MiniMax",
        ("MINIMAX_MODEL", "ar") => "نموذج MiniMax",
        ("MINIMAX_MODEL", _) => "MiniMax Model",
        ("TELEGRAM_BOT_USERNAME", "zh") => "Telegram 用户名",
        ("TELEGRAM_BOT_USERNAME", "hi") => "Telegram उपयोगकर्ता नाम",
        ("TELEGRAM_BOT_USERNAME", "es") => "Nombre de usuario de Telegram",
        ("TELEGRAM_BOT_USERNAME", "ar") => "اسم مستخدم Telegram",
        ("TELEGRAM_BOT_USERNAME", _) => "Telegram Bot Username",
        ("TELEGRAM_STARTUP_ONLINE_TEXT", "zh") => "启动在线文案",
        ("TELEGRAM_STARTUP_ONLINE_TEXT", "hi") => "स्टार्टअप ऑनलाइन टेक्स्ट",
        ("TELEGRAM_STARTUP_ONLINE_TEXT", "es") => "Texto de estado al iniciar",
        ("TELEGRAM_STARTUP_ONLINE_TEXT", "ar") => "نص الحالة عند التشغيل",
        ("TELEGRAM_STARTUP_ONLINE_TEXT", _) => "Startup Online Text",
        ("TELEGRAM_GROUP_TRIGGER_MODE", "zh") => "群组触发模式",
        ("TELEGRAM_GROUP_TRIGGER_MODE", "hi") => "ग्रुप ट्रिगर मोड",
        ("TELEGRAM_GROUP_TRIGGER_MODE", "es") => "Modo de activación en grupo",
        ("TELEGRAM_GROUP_TRIGGER_MODE", "ar") => "وضع تشغيل المجموعة",
        ("TELEGRAM_GROUP_TRIGGER_MODE", _) => "Group Trigger Mode",
        ("TELEGRAM_SCHEDULER_DEFAULT_TIMEZONE", "zh") => "调度默认时区",
        ("TELEGRAM_SCHEDULER_DEFAULT_TIMEZONE", "hi") => "शेड्यूलर डिफ़ॉल्ट टाइमज़ोन",
        ("TELEGRAM_SCHEDULER_DEFAULT_TIMEZONE", "es") => "Zona horaria del programador",
        ("TELEGRAM_SCHEDULER_DEFAULT_TIMEZONE", "ar") => "المنطقة الزمنية للجدولة",
        ("TELEGRAM_SCHEDULER_DEFAULT_TIMEZONE", _) => "Scheduler Timezone",
        ("TELEGRAM_ADMIN_USER_IDS", "zh") => "管理员用户 ID",
        ("TELEGRAM_ADMIN_USER_IDS", "hi") => "एडमिन यूज़र ID",
        ("TELEGRAM_ADMIN_USER_IDS", "es") => "IDs de administradores",
        ("TELEGRAM_ADMIN_USER_IDS", "ar") => "معرّفات المدراء",
        ("TELEGRAM_ADMIN_USER_IDS", _) => "Admin User IDs",
        ("XIAOMAOLV_LOCALE", "zh") => "系统默认语言",
        ("XIAOMAOLV_LOCALE", "hi") => "सिस्टम डिफ़ॉल्ट भाषा",
        ("XIAOMAOLV_LOCALE", "es") => "Idioma predeterminado del sistema",
        ("XIAOMAOLV_LOCALE", "ar") => "اللغة الافتراضية للنظام",
        ("XIAOMAOLV_LOCALE", _) => "System Default Language",
        ("HTTP_DIAG_BEARER_TOKEN", "zh") => "HTTP 诊断令牌",
        ("HTTP_DIAG_BEARER_TOKEN", "hi") => "HTTP डायग टोकन",
        ("HTTP_DIAG_BEARER_TOKEN", "es") => "Token de diagnóstico HTTP",
        ("HTTP_DIAG_BEARER_TOKEN", "ar") => "رمز تشخيص HTTP",
        ("HTTP_DIAG_BEARER_TOKEN", _) => "HTTP Diag Bearer Token",
        ("ZVEC_SIDECAR_ENDPOINT", "zh") => "zvec Sidecar 地址",
        ("ZVEC_SIDECAR_ENDPOINT", "hi") => "zvec साइडकार एंडपॉइंट",
        ("ZVEC_SIDECAR_ENDPOINT", "es") => "Endpoint de zvec sidecar",
        ("ZVEC_SIDECAR_ENDPOINT", "ar") => "نقطة zvec الجانبية",
        ("ZVEC_SIDECAR_ENDPOINT", _) => "zvec Sidecar Endpoint",
        ("ZVEC_SIDECAR_TOKEN", "zh") => "zvec Sidecar Token",
        ("ZVEC_SIDECAR_TOKEN", "hi") => "zvec साइडकार टोकन",
        ("ZVEC_SIDECAR_TOKEN", "es") => "Token de zvec sidecar",
        ("ZVEC_SIDECAR_TOKEN", "ar") => "رمز zvec الجانبي",
        ("ZVEC_SIDECAR_TOKEN", _) => "zvec Sidecar Token",
        _ => "Unknown",
    }
}

fn localized_field_description(key: &str, locale: &str) -> &'static str {
    match (key, locale_family(locale)) {
        ("MINIMAX_API_KEY", "zh") => "MiniMax 接口密钥（必填）。",
        ("MINIMAX_API_KEY", "hi") => "MiniMax API कुंजी (आवश्यक)।",
        ("MINIMAX_API_KEY", "es") => "Clave de MiniMax (obligatoria).",
        ("MINIMAX_API_KEY", "ar") => "مفتاح MiniMax (مطلوب).",
        ("MINIMAX_API_KEY", _) => "MiniMax API credential (required).",
        ("TELEGRAM_BOT_TOKEN", "zh") => "BotFather 生成的机器人 token（必填）。",
        ("TELEGRAM_BOT_TOKEN", "hi") => "BotFather से मिला Telegram token (आवश्यक)।",
        ("TELEGRAM_BOT_TOKEN", "es") => "Token del bot de BotFather (obligatorio).",
        ("TELEGRAM_BOT_TOKEN", "ar") => "رمز Telegram BotFather (مطلوب).",
        ("TELEGRAM_BOT_TOKEN", _) => "Telegram bot token from BotFather (required).",
        ("MINIMAX_MODEL", "zh") => "模型名，默认 MiniMax-M2.5-highspeed。",
        ("MINIMAX_MODEL", "hi") => "मॉडल नाम, डिफ़ॉल्ट MiniMax-M2.5-highspeed.",
        ("MINIMAX_MODEL", "es") => "Nombre del modelo, por defecto MiniMax-M2.5-highspeed.",
        ("MINIMAX_MODEL", "ar") => "اسم النموذج، الافتراضي MiniMax-M2.5-highspeed.",
        ("MINIMAX_MODEL", _) => "Model name, default MiniMax-M2.5-highspeed.",
        ("TELEGRAM_BOT_USERNAME", "zh") => "不带 @，提升群组触发判定准确度。",
        ("TELEGRAM_BOT_USERNAME", "hi") => "@ के बिना, ग्रुप ट्रिगर के लिए उपयोगी।",
        ("TELEGRAM_BOT_USERNAME", "es") => "Sin @, mejora la detección en grupos.",
        ("TELEGRAM_BOT_USERNAME", "ar") => "بدون @، لتحسين التعرف في المجموعات.",
        ("TELEGRAM_BOT_USERNAME", _) => "Without @, improves trigger detection in groups.",
        ("TELEGRAM_STARTUP_ONLINE_TEXT", "zh") => "机器人启动后写入的状态文案。",
        ("TELEGRAM_STARTUP_ONLINE_TEXT", "hi") => "स्टार्टअप के बाद दिखाई देने वाला स्टेटस टेक्स्ट।",
        ("TELEGRAM_STARTUP_ONLINE_TEXT", "es") => "Texto de estado al iniciar el bot.",
        ("TELEGRAM_STARTUP_ONLINE_TEXT", "ar") => "نص الحالة بعد تشغيل البوت.",
        ("TELEGRAM_STARTUP_ONLINE_TEXT", _) => "Startup status text shown on Telegram profile.",
        ("TELEGRAM_GROUP_TRIGGER_MODE", "zh") => "strict 或 smart。",
        ("TELEGRAM_GROUP_TRIGGER_MODE", "hi") => "strict या smart चुनें।",
        ("TELEGRAM_GROUP_TRIGGER_MODE", "es") => "Elige strict o smart.",
        ("TELEGRAM_GROUP_TRIGGER_MODE", "ar") => "اختر strict أو smart.",
        ("TELEGRAM_GROUP_TRIGGER_MODE", _) => "Choose strict or smart.",
        ("TELEGRAM_SCHEDULER_DEFAULT_TIMEZONE", "zh") => "IANA 时区，例如 Asia/Shanghai。",
        ("TELEGRAM_SCHEDULER_DEFAULT_TIMEZONE", "hi") => "IANA टाइमज़ोन, जैसे Asia/Shanghai.",
        ("TELEGRAM_SCHEDULER_DEFAULT_TIMEZONE", "es") => {
            "Zona horaria IANA, por ejemplo Asia/Shanghai."
        }
        ("TELEGRAM_SCHEDULER_DEFAULT_TIMEZONE", "ar") => "منطقة IANA مثل Asia/Shanghai.",
        ("TELEGRAM_SCHEDULER_DEFAULT_TIMEZONE", _) => "IANA timezone, for example Asia/Shanghai.",
        ("TELEGRAM_ADMIN_USER_IDS", "zh") => "私聊管理员白名单，逗号分隔。",
        ("TELEGRAM_ADMIN_USER_IDS", "hi") => "एडमिन ID, कॉमा से अलग करें।",
        ("TELEGRAM_ADMIN_USER_IDS", "es") => "IDs de admin separados por coma.",
        ("TELEGRAM_ADMIN_USER_IDS", "ar") => "معرّفات المدراء مفصولة بفواصل.",
        ("TELEGRAM_ADMIN_USER_IDS", _) => "Admin IDs in CSV form.",
        ("XIAOMAOLV_LOCALE", "zh") => "系统默认语言（en-US/zh-CN/hi-IN/es-ES/ar）。",
        ("XIAOMAOLV_LOCALE", "hi") => "सिस्टम डिफ़ॉल्ट भाषा (en-US/zh-CN/hi-IN/es-ES/ar).",
        ("XIAOMAOLV_LOCALE", "es") => "Idioma global por defecto (en-US/zh-CN/hi-IN/es-ES/ar).",
        ("XIAOMAOLV_LOCALE", "ar") => "اللغة الافتراضية للنظام (en-US/zh-CN/hi-IN/es-ES/ar).",
        ("XIAOMAOLV_LOCALE", _) => "Global default locale (en-US/zh-CN/hi-IN/es-ES/ar).",
        ("HTTP_DIAG_BEARER_TOKEN", "zh") => "用于保护诊断接口访问。",
        ("HTTP_DIAG_BEARER_TOKEN", "hi") => "डायग API सुरक्षा हेतु टोकन।",
        ("HTTP_DIAG_BEARER_TOKEN", "es") => "Token para proteger APIs de diagnóstico.",
        ("HTTP_DIAG_BEARER_TOKEN", "ar") => "رمز لحماية واجهات التشخيص.",
        ("HTTP_DIAG_BEARER_TOKEN", _) => "Token protecting diagnostics APIs.",
        ("ZVEC_SIDECAR_ENDPOINT", "zh") => "混合记忆的 sidecar 地址。",
        ("ZVEC_SIDECAR_ENDPOINT", "hi") => "हाइब्रिड मेमोरी sidecar का endpoint.",
        ("ZVEC_SIDECAR_ENDPOINT", "es") => "Endpoint del sidecar de memoria híbrida.",
        ("ZVEC_SIDECAR_ENDPOINT", "ar") => "عنوان sidecar للذاكرة الهجينة.",
        ("ZVEC_SIDECAR_ENDPOINT", _) => "Sidecar endpoint for hybrid memory mode.",
        ("ZVEC_SIDECAR_TOKEN", "zh") => "sidecar Bearer token（可选）。",
        ("ZVEC_SIDECAR_TOKEN", "hi") => "sidecar Bearer token (वैकल्पिक)।",
        ("ZVEC_SIDECAR_TOKEN", "es") => "Token Bearer del sidecar (opcional).",
        ("ZVEC_SIDECAR_TOKEN", "ar") => "رمز Bearer للـ sidecar (اختياري).",
        ("ZVEC_SIDECAR_TOKEN", _) => "Bearer token for sidecar (optional).",
        _ => "",
    }
}

fn localized_messages(locale: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let family = locale_family(locale);
    let entries = match family {
        "zh" => vec![
            ("title", "配置中心"),
            (
                "subtitle",
                "可先用 .env.realtest 作为初始值，也可直接在这里编辑；保存后立即热重载生效。",
            ),
            ("required_title", "首次配置（必填）"),
            ("mobile_tag", "移动优先"),
            ("all_title", "完整配置（分类）"),
            ("save_required", "保存必填并生效"),
            ("save_all", "保存全部并生效"),
            ("reload", "刷新状态"),
            ("notice_first", "当前是首次配置，请先填写必填项。"),
            ("notice_ready", "当前配置已完成，可继续按分类调整。"),
            ("saved", "配置已保存并即时生效"),
            ("view_language", "页面语言"),
        ],
        "hi" => vec![
            ("title", "कॉन्फ़िग केंद्र"),
            (
                "subtitle",
                ".env.realtest को प्रारंभिक मान की तरह उपयोग करें या सीधे यहाँ संपादित करें। सेव करते ही लागू होगा।",
            ),
            ("required_title", "पहली सेटअप (आवश्यक)"),
            ("mobile_tag", "मोबाइल-प्रथम"),
            ("all_title", "पूर्ण कॉन्फ़िग (श्रेणीबद्ध)"),
            ("save_required", "आवश्यक सेव करें"),
            ("save_all", "सब सेव करें"),
            ("reload", "स्थिति रीफ़्रेश करें"),
            ("notice_first", "पहली बार सेटअप है, पहले आवश्यक फ़ील्ड भरें।"),
            ("notice_ready", "कॉन्फ़िग तैयार है, आगे समायोजित कर सकते हैं।"),
            ("saved", "कॉन्फ़िग सेव हुआ और तुरंत लागू हुआ"),
            ("view_language", "पेज भाषा"),
        ],
        "es" => vec![
            ("title", "Centro de Configuración"),
            (
                "subtitle",
                "Puedes usar .env.realtest como valor inicial o editar aquí; al guardar se aplica al instante.",
            ),
            ("required_title", "Configuración inicial (obligatoria)"),
            ("mobile_tag", "Mobile-first"),
            ("all_title", "Configuración completa (por categorías)"),
            ("save_required", "Guardar obligatorios"),
            ("save_all", "Guardar todo"),
            ("reload", "Actualizar estado"),
            (
                "notice_first",
                "Primera configuración: completa primero los campos obligatorios.",
            ),
            (
                "notice_ready",
                "Configuración completa; puedes seguir ajustando por categorías.",
            ),
            ("saved", "Configuración guardada y aplicada al instante"),
            ("view_language", "Idioma de la página"),
        ],
        "ar" => vec![
            ("title", "مركز الإعدادات"),
            (
                "subtitle",
                "يمكنك استخدام .env.realtest كقيمة أولية أو التعديل هنا مباشرة؛ يتم التطبيق فور الحفظ.",
            ),
            ("required_title", "الإعداد الأولي (إلزامي)"),
            ("mobile_tag", "أولوية الجوال"),
            ("all_title", "إعداد كامل (منظم)"),
            ("save_required", "حفظ الإلزامي"),
            ("save_all", "حفظ الكل"),
            ("reload", "تحديث الحالة"),
            (
                "notice_first",
                "هذه أول مرة إعداد، أكمل الحقول الإلزامية أولاً.",
            ),
            (
                "notice_ready",
                "الإعداد مكتمل، ويمكنك التعديل حسب التصنيفات.",
            ),
            ("saved", "تم حفظ الإعداد وتفعيله فوراً"),
            ("view_language", "لغة الصفحة"),
        ],
        _ => vec![
            ("title", "Configuration Center"),
            (
                "subtitle",
                "Use values from .env.realtest as defaults, or edit here directly. Save applies immediately with hot reload.",
            ),
            ("required_title", "First-time Setup (Required)"),
            ("mobile_tag", "Mobile-first"),
            ("all_title", "Full Configuration (Categorized)"),
            ("save_required", "Save Required Fields"),
            ("save_all", "Save All"),
            ("reload", "Refresh State"),
            (
                "notice_first",
                "First-time setup detected. Fill required fields first.",
            ),
            (
                "notice_ready",
                "Configuration is ready. Continue with categorized adjustments.",
            ),
            ("saved", "Configuration saved and applied immediately"),
            ("view_language", "View Language"),
        ],
    };

    for (key, value) in entries {
        map.insert(key.to_string(), value.to_string());
    }
    map
}

fn is_missing_required_value(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.is_empty() || trimmed.starts_with("replace_with_")
}

fn effective_field_value(
    spec: &ConfigUiFieldSpec,
    env_file_values: &BTreeMap<String, String>,
) -> String {
    if let Ok(value) = std::env::var(spec.key)
        && !value.trim().is_empty()
    {
        return value;
    }
    if let Some(value) = env_file_values.get(spec.key)
        && !value.trim().is_empty()
    {
        return value.clone();
    }
    spec.default_value.to_string()
}

async fn read_env_file_values(path: &FsPath) -> anyhow::Result<BTreeMap<String, String>> {
    let content = match tokio::fs::read_to_string(path).await {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to read env file: {}", path.display()));
        }
    };
    Ok(parse_env_file_values(&content))
}

fn parse_env_file_values(content: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((raw_key, raw_value)) = line.split_once('=') else {
            continue;
        };
        let key = raw_key.trim();
        if key.is_empty() {
            continue;
        }
        let value = parse_env_value(raw_value.trim());
        values.insert(key.to_string(), value);
    }
    values
}

fn parse_env_value(raw: &str) -> String {
    if raw.len() >= 2 {
        let bytes = raw.as_bytes();
        let first = bytes[0] as char;
        let last = bytes[raw.len() - 1] as char;
        if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
            return raw[1..raw.len() - 1]
                .replace("\\\"", "\"")
                .replace("\\\\", "\\");
        }
    }
    raw.to_string()
}

fn format_env_value(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    let needs_quotes = value
        .chars()
        .any(|ch| ch.is_whitespace() || ch == '#' || ch == '"' || ch == '\'');
    if !needs_quotes {
        return value.to_string();
    }
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn render_env_file(known: &BTreeMap<String, String>, extras: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    out.push_str("# Managed by xiaomaolv setup UI.\n");
    out.push_str("# You can still edit this file manually.\n\n");

    let mut current_group = "";
    for spec in CONFIG_UI_FIELDS {
        if spec.group_key != current_group {
            if !current_group.is_empty() {
                out.push('\n');
            }
            current_group = spec.group_key;
            out.push_str(&format!(
                "# {}\n",
                localized_group_name(spec.group_key, "en-US")
            ));
        }
        let value = known.get(spec.key).cloned().unwrap_or_default();
        out.push_str(&format!("{}={}\n", spec.key, format_env_value(&value)));
    }

    if !extras.is_empty() {
        out.push_str("\n# Extra variables preserved from previous file\n");
        for (key, value) in extras {
            out.push_str(&format!("{key}={}\n", format_env_value(value)));
        }
    }

    out
}

async fn build_config_ui_state(
    manager: &ConfigUiManager,
    ui_locale: &str,
    global_locale: &str,
) -> anyhow::Result<ConfigUiStateResponse> {
    let env_file_values = read_env_file_values(&manager.env_file_path).await?;
    let mut fields = Vec::with_capacity(CONFIG_UI_FIELDS.len());
    let mut required_keys = Vec::new();
    let mut first_time = false;

    for spec in CONFIG_UI_FIELDS {
        let value = effective_field_value(spec, &env_file_values);
        if spec.required {
            required_keys.push(spec.key.to_string());
            if is_missing_required_value(&value) {
                first_time = true;
            }
        }

        fields.push(ConfigUiFieldView {
            key: spec.key.to_string(),
            label: localized_field_label(spec.key, ui_locale).to_string(),
            category: localized_group_name(spec.group_key, ui_locale).to_string(),
            description: localized_field_description(spec.key, ui_locale).to_string(),
            required: spec.required,
            sensitive: spec.sensitive,
            value,
            placeholder: spec.placeholder.to_string(),
        });
    }

    let available_locales = SUPPORTED_UI_LOCALES
        .iter()
        .map(|code| ConfigUiLocaleOption {
            code: (*code).to_string(),
            label: locale_label(code).to_string(),
        })
        .collect();

    Ok(ConfigUiStateResponse {
        first_time,
        required_keys,
        fields,
        ui_locale: ui_locale.to_string(),
        global_locale: global_locale.to_string(),
        available_locales,
        messages: localized_messages(ui_locale),
    })
}

async fn get_setup_page() -> Html<&'static str> {
    Html(SETUP_PAGE_HTML)
}

async fn get_config_ui_state(
    State(state): State<AppState>,
    Query(query): Query<ConfigUiStateQuery>,
) -> Result<Json<ConfigUiStateResponse>, (StatusCode, String)> {
    let Some(manager) = state.config_ui.clone() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "config ui is unavailable for this runtime".to_string(),
        ));
    };

    let global_locale = {
        let runtime = state.runtime.read().await;
        normalize_supported_locale(&runtime.locale).unwrap_or_else(|| "en-US".to_string())
    };
    let ui_locale = query
        .locale
        .as_deref()
        .and_then(normalize_supported_locale)
        .unwrap_or_else(|| global_locale.clone());

    let snapshot = build_config_ui_state(&manager, &ui_locale, &global_locale)
        .await
        .map_err(internal_err("failed to load config ui state"))?;
    Ok(Json(snapshot))
}

async fn post_config_ui_save(
    State(state): State<AppState>,
    Json(req): Json<ConfigUiSaveRequest>,
) -> Result<Json<ConfigUiSaveResponse>, (StatusCode, String)> {
    let Some(manager) = state.config_ui.clone() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "config ui is unavailable for this runtime".to_string(),
        ));
    };

    let mut env_values = read_env_file_values(&manager.env_file_path)
        .await
        .map_err(internal_err("failed to read env file"))?;

    let mut accepted_keys = 0usize;
    for (key, value) in req.values {
        if config_ui_field_spec(&key).is_some() {
            let normalized_value = if key == "XIAOMAOLV_LOCALE" {
                normalize_supported_locale(value.trim()).unwrap_or_else(|| "en-US".to_string())
            } else {
                value.trim().to_string()
            };
            env_values.insert(key, normalized_value);
            accepted_keys += 1;
        }
    }

    if accepted_keys == 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            "no supported config keys provided".to_string(),
        ));
    }

    let save_mode = req.mode.unwrap_or_else(|| "full".to_string());
    if save_mode == "required" || save_mode == "full" {
        for spec in CONFIG_UI_FIELDS.iter().filter(|item| item.required) {
            let value = env_values
                .get(spec.key)
                .cloned()
                .unwrap_or_else(|| effective_field_value(spec, &BTreeMap::new()));
            if is_missing_required_value(&value) {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("required field '{}' is missing", spec.key),
                ));
            }
        }
    }

    let mut known = BTreeMap::new();
    let mut extras = BTreeMap::new();
    for (key, value) in env_values {
        if config_ui_field_spec(&key).is_some() {
            known.insert(key, value);
        } else {
            extras.insert(key, value);
        }
    }

    if let Some(parent) = manager.env_file_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create env parent dir: {}", parent.display()))
            .map_err(internal_err("failed to create env parent directory"))?;
    }

    let rendered = render_env_file(&known, &extras);
    tokio::fs::write(&manager.env_file_path, rendered)
        .await
        .with_context(|| {
            format!(
                "failed to write env file: {}",
                manager.env_file_path.display()
            )
        })
        .map_err(internal_err("failed to write env file"))?;

    for spec in CONFIG_UI_FIELDS {
        let value = known.get(spec.key).cloned().unwrap_or_default();
        // SAFETY: setup page mutates process env from explicit user action to rebuild runtime in-place.
        unsafe {
            if value.is_empty() {
                std::env::remove_var(spec.key);
            } else {
                std::env::set_var(spec.key, &value);
            }
        }
    }

    state
        .reload_runtime_from_config(&manager)
        .await
        .map_err(internal_err("failed to reload runtime after saving config"))?;

    let global_locale = {
        let runtime = state.runtime.read().await;
        normalize_supported_locale(&runtime.locale).unwrap_or_else(|| "en-US".to_string())
    };
    let snapshot = build_config_ui_state(&manager, &global_locale, &global_locale)
        .await
        .map_err(internal_err("failed to refresh config ui state"))?;
    let message = snapshot
        .messages
        .get("saved")
        .cloned()
        .unwrap_or_else(|| "Configuration saved and applied immediately".to_string());

    Ok(Json(ConfigUiSaveResponse {
        saved: true,
        runtime_reloaded: true,
        first_time: snapshot.first_time,
        applied_at_unix: current_unix_time_secs(),
        message,
    }))
}

const SETUP_PAGE_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="UTF-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>xiaomaolv setup</title>
  <style>
    :root {
      --bg: #f4f5f7;
      --paper: #ffffff;
      --ink: #1a1d22;
      --muted: #606977;
      --line: #e3e8ef;
      --accent: #0b7a75;
      --accent-soft: #e6f5f4;
      --danger: #be3455;
      --radius: 16px;
      --shadow: 0 12px 28px rgba(0, 0, 0, 0.08);
    }
    * { box-sizing: border-box; }
    body {
      margin: 0;
      background:
        radial-gradient(circle at top left, #d8ecea 0%, transparent 42%),
        radial-gradient(circle at 90% 10%, #f7e7d5 0%, transparent 36%),
        var(--bg);
      color: var(--ink);
      font: 15px/1.45 "IBM Plex Sans", "PingFang SC", "Noto Sans CJK SC", sans-serif;
    }
    .wrap { max-width: 980px; margin: 0 auto; padding: 14px; }
    .hero {
      background: var(--paper);
      border: 1px solid var(--line);
      border-radius: var(--radius);
      padding: 18px;
      box-shadow: var(--shadow);
    }
    .hero-head {
      display: flex;
      gap: 10px;
      align-items: center;
      justify-content: space-between;
      flex-wrap: wrap;
    }
    h1 { margin: 0 0 8px 0; font: 700 24px/1.2 "Source Han Sans SC", "PingFang SC", sans-serif; }
    .muted { color: var(--muted); }
    .notice {
      margin-top: 12px;
      padding: 10px 12px;
      border-radius: 12px;
      background: var(--accent-soft);
      color: #0f4b48;
      display: none;
    }
    .notice.error { background: #fdebf0; color: var(--danger); }
    .grid { margin-top: 14px; display: grid; gap: 12px; grid-template-columns: 1fr; }
    .panel {
      background: var(--paper);
      border: 1px solid var(--line);
      border-radius: var(--radius);
      box-shadow: var(--shadow);
      padding: 14px;
    }
    .panel h2 { margin: 0 0 10px 0; font: 700 17px/1.3 "Source Han Sans SC", "PingFang SC", sans-serif; }
    .field { margin-bottom: 10px; }
    .field label { display: block; font-weight: 600; margin-bottom: 4px; }
    .req { color: var(--danger); margin-left: 4px; }
    .desc { margin: 3px 0 6px 0; color: var(--muted); font-size: 13px; }
    input, select {
      width: 100%;
      border: 1px solid var(--line);
      border-radius: 12px;
      padding: 10px 11px;
      font: inherit;
      background: #fff;
    }
    input:focus, select:focus { outline: none; border-color: var(--accent); box-shadow: 0 0 0 3px rgba(11, 122, 117, 0.17); }
    .actions { margin-top: 12px; display: flex; gap: 8px; flex-wrap: wrap; }
    button {
      border: none;
      border-radius: 12px;
      padding: 10px 14px;
      font: 600 14px/1 "IBM Plex Sans", sans-serif;
      cursor: pointer;
    }
    .btn-main { background: var(--accent); color: white; }
    .btn-light { background: #eef2f7; color: #24313d; }
    .tag { display: inline-block; margin-left: 6px; padding: 2px 8px; border-radius: 999px; background: #f0f4fa; color: #465364; font-size: 12px; }
    .locale-box { min-width: 200px; }
    @media (min-width: 920px) {
      .wrap { padding: 22px; }
      .grid { grid-template-columns: 1fr 1fr; }
    }
  </style>
</head>
<body>
  <div class="wrap">
    <section class="hero">
      <div class="hero-head">
        <div>
          <h1 id="ui-title">Configuration Center</h1>
          <div class="muted" id="ui-subtitle">Save applies immediately.</div>
        </div>
        <div class="locale-box">
          <label for="view-language" id="ui-view-language" class="muted">View Language</label>
          <select id="view-language"></select>
        </div>
      </div>
      <div id="notice" class="notice"></div>
    </section>
    <section class="grid">
      <article class="panel" id="required-panel">
        <h2><span id="ui-required-title">First-time Setup</span><span class="tag" id="ui-mobile-tag">Mobile-first</span></h2>
        <div id="required-fields"></div>
        <div class="actions">
          <button class="btn-main" id="save-required">Save Required Fields</button>
          <button class="btn-light" id="reload-state">Refresh State</button>
        </div>
      </article>
      <article class="panel">
        <h2 id="ui-all-title">Full Configuration</h2>
        <div id="all-fields"></div>
        <div class="actions">
          <button class="btn-main" id="save-all">Save All</button>
        </div>
      </article>
    </section>
  </div>

  <script>
    let latest = null;

    function escapeHtml(text) {
      return String(text || "")
        .replaceAll("&", "&amp;")
        .replaceAll("<", "&lt;")
        .replaceAll(">", "&gt;")
        .replaceAll('"', "&quot;");
    }

    function m(key, fallback = "") {
      if (!latest || !latest.messages) return fallback;
      return latest.messages[key] || fallback;
    }

    function notice(message, isError = false) {
      const el = document.getElementById("notice");
      el.textContent = message;
      el.style.display = "block";
      if (isError) el.classList.add("error");
      else el.classList.remove("error");
    }

    function groupedFields(fields) {
      const map = new Map();
      for (const f of fields) {
        if (!map.has(f.category)) map.set(f.category, []);
        map.get(f.category).push(f);
      }
      return map;
    }

    function renderField(field) {
      const type = field.sensitive ? "password" : "text";
      const requiredMark = field.required ? '<span class="req">*</span>' : "";
      return `
        <div class="field" data-key="${escapeHtml(field.key)}">
          <label>${escapeHtml(field.label)}${requiredMark}</label>
          <div class="desc">${escapeHtml(field.description)}</div>
          <input type="${type}" id="f-${escapeHtml(field.key)}" value="${escapeHtml(field.value || "")}" placeholder="${escapeHtml(field.placeholder || "")}" />
        </div>
      `;
    }

    function renderLocaleSelector() {
      const select = document.getElementById("view-language");
      const current = latest.ui_locale;
      select.innerHTML = (latest.available_locales || [])
        .map(item => `<option value="${escapeHtml(item.code)}"${item.code === current ? " selected" : ""}>${escapeHtml(item.label)}</option>`)
        .join("");
    }

    function renderStaticTexts() {
      document.getElementById("ui-title").textContent = m("title", "Configuration Center");
      document.getElementById("ui-subtitle").textContent = m("subtitle", "Save applies immediately.");
      document.getElementById("ui-required-title").textContent = m("required_title", "First-time Setup");
      document.getElementById("ui-mobile-tag").textContent = m("mobile_tag", "Mobile-first");
      document.getElementById("ui-all-title").textContent = m("all_title", "Full Configuration");
      document.getElementById("save-required").textContent = m("save_required", "Save Required Fields");
      document.getElementById("save-all").textContent = m("save_all", "Save All");
      document.getElementById("reload-state").textContent = m("reload", "Refresh State");
      document.getElementById("ui-view-language").textContent = m("view_language", "View Language");
    }

    function render(state) {
      latest = state;
      renderStaticTexts();
      renderLocaleSelector();

      const requiredEl = document.getElementById("required-fields");
      const allEl = document.getElementById("all-fields");
      const requiredOnly = state.fields.filter(f => f.required);
      requiredEl.innerHTML = requiredOnly.map(renderField).join("");

      const grouped = groupedFields(state.fields);
      let html = "";
      for (const [category, fields] of grouped.entries()) {
        html += `<h3>${escapeHtml(category)}</h3>`;
        html += fields.map(renderField).join("");
      }
      allEl.innerHTML = html;

      if (state.first_time) notice(m("notice_first", "First-time setup detected."));
      else notice(m("notice_ready", "Configuration is ready."));
    }

    function collectValues() {
      const payload = {};
      if (!latest) return payload;
      for (const f of latest.fields) {
        const input = document.getElementById(`f-${f.key}`);
        if (input) payload[f.key] = input.value || "";
      }
      return payload;
    }

    async function loadState(locale) {
      const query = locale ? `?locale=${encodeURIComponent(locale)}` : "";
      const res = await fetch(`/v1/config/ui/state${query}`);
      if (!res.ok) throw new Error(await res.text());
      const data = await res.json();
      render(data);
    }

    async function save(mode) {
      try {
        const selectedLocale = document.getElementById("view-language").value || "";
        const res = await fetch("/v1/config/ui/save", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ mode, values: collectValues() }),
        });
        const body = await res.json();
        if (!res.ok) throw new Error(body.message || JSON.stringify(body));
        notice(body.message || m("saved", "Saved."));
        await loadState(selectedLocale);
      } catch (err) {
        notice(String(err.message || err), true);
      }
    }

    async function init() {
      const preferred = localStorage.getItem("setup_ui_locale") || "";
      await loadState(preferred);
      const selector = document.getElementById("view-language");
      selector.addEventListener("change", async () => {
        const locale = selector.value || "";
        localStorage.setItem("setup_ui_locale", locale);
        await loadState(locale);
      });
    }

    document.getElementById("save-required").addEventListener("click", () => save("required"));
    document.getElementById("save-all").addEventListener("click", () => save("full"));
    document.getElementById("reload-state").addEventListener("click", async () => {
      const locale = document.getElementById("view-language").value || "";
      await loadState(locale);
    });

    init().catch(err => notice(String(err.message || err), true));
  </script>
</body>
</html>
"#;

async fn get_mcp_servers(
    State(state): State<AppState>,
) -> Result<Json<McpServersResponse>, (StatusCode, String)> {
    let _permit = state.semaphore.acquire().await.map_err(|err| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("concurrency semaphore closed: {err}"),
        )
    })?;

    let runtime_state = state.runtime.read().await.clone();
    let runtime = runtime_state.mcp_runtime.read().await.clone();
    Ok(Json(McpServersResponse {
        servers: runtime.servers(),
    }))
}

async fn get_code_mode_diag(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<CodeModeDiagnostics>, (StatusCode, String)> {
    guard_code_mode_diag_access(&state, &headers)?;

    let _permit = state.semaphore.acquire().await.map_err(|err| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("concurrency semaphore closed: {err}"),
        )
    })?;

    let runtime_state = state.runtime.read().await.clone();
    Ok(Json(runtime_state.service.code_mode_diagnostics()))
}

async fn get_code_mode_metrics(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<(HeaderMap, String), (StatusCode, String)> {
    guard_code_mode_diag_access(&state, &headers)?;

    let _permit = state.semaphore.acquire().await.map_err(|err| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("concurrency semaphore closed: {err}"),
        )
    })?;

    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );
    Ok((response_headers, {
        let runtime_state = state.runtime.read().await.clone();
        runtime_state.service.code_mode_metrics_prometheus()
    }))
}

async fn get_mcp_tools(
    State(state): State<AppState>,
    Query(query): Query<McpToolsQuery>,
) -> Result<Json<McpToolsResponse>, (StatusCode, String)> {
    let _permit = state.semaphore.acquire().await.map_err(|err| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("concurrency semaphore closed: {err}"),
        )
    })?;

    let runtime_state = state.runtime.read().await.clone();
    let runtime = runtime_state.mcp_runtime.read().await.clone();
    let tools = runtime
        .list_tools(query.server.as_deref())
        .await
        .map_err(internal_err("failed to list mcp tools"))?;

    Ok(Json(McpToolsResponse { tools }))
}

async fn post_mcp_tool_call(
    State(state): State<AppState>,
    Path((server, tool)): Path<(String, String)>,
    Json(args): Json<serde_json::Value>,
) -> Result<Json<McpCallResponse>, (StatusCode, String)> {
    let _permit = state.semaphore.acquire().await.map_err(|err| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("concurrency semaphore closed: {err}"),
        )
    })?;

    let runtime_state = state.runtime.read().await.clone();
    let runtime = runtime_state.mcp_runtime.read().await.clone();
    let result = runtime
        .call_tool(&server, &tool, args)
        .await
        .map_err(internal_err("failed to call mcp tool"))?;

    Ok(Json(McpCallResponse {
        server,
        tool,
        result,
    }))
}

async fn post_message(
    State(state): State<AppState>,
    Json(req): Json<HttpMessageRequest>,
) -> Result<Json<HttpMessageResponse>, (StatusCode, String)> {
    let _permit = state.semaphore.acquire().await.map_err(|err| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("concurrency semaphore closed: {err}"),
        )
    })?;

    let runtime_state = state.runtime.read().await.clone();
    let out = runtime_state
        .service
        .handle(IncomingMessage {
            channel: "http".to_string(),
            session_id: req.session_id,
            user_id: req.user_id,
            text: req.text,
            reply_target: None,
        })
        .await
        .map_err(internal_err("failed to process message"))?;

    Ok(Json(HttpMessageResponse { reply: out.text }))
}

async fn post_channel_inbound(
    State(state): State<AppState>,
    Path(channel): Path<String>,
    Json(payload): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    dispatch_channel_inbound(state, channel, None, payload).await
}

#[derive(Debug, Serialize)]
struct ChannelModeResponse {
    channel: String,
    mode: String,
}

async fn get_channel_mode(
    State(state): State<AppState>,
    Path(channel): Path<String>,
) -> Result<Json<ChannelModeResponse>, (StatusCode, String)> {
    let runtime_state = state.runtime.read().await.clone();
    let plugin = runtime_state
        .channel_plugins
        .get(&channel)
        .cloned()
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("channel '{}' is not configured", channel),
            )
        })?;

    let mode = plugin.mode().ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("channel '{}' does not expose mode", channel),
        )
    })?;

    Ok(Json(ChannelModeResponse {
        channel,
        mode: mode.to_string(),
    }))
}

async fn get_channel_diag(
    State(state): State<AppState>,
    Path(channel): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let runtime_state = state.runtime.read().await.clone();
    let plugin = runtime_state
        .channel_plugins
        .get(&channel)
        .cloned()
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("channel '{}' is not configured", channel),
            )
        })?;

    let diag = plugin
        .diagnostics()
        .await
        .map_err(internal_err("failed to collect channel diagnostics"))?;

    let body = diag.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("channel '{}' does not expose diagnostics", channel),
        )
    })?;

    Ok(Json(body))
}

async fn post_channel_inbound_with_secret(
    State(state): State<AppState>,
    Path((channel, secret)): Path<(String, String)>,
    Json(payload): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    dispatch_channel_inbound(state, channel, Some(secret), payload).await
}

async fn dispatch_channel_inbound(
    state: AppState,
    channel_name: String,
    path_secret: Option<String>,
    payload: serde_json::Value,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let _permit = state.semaphore.acquire().await.map_err(|err| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("concurrency semaphore closed: {err}"),
        )
    })?;

    let runtime_state = state.runtime.read().await.clone();
    let plugin = runtime_state
        .channel_plugins
        .get(&channel_name)
        .cloned()
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("channel '{}' is not configured", channel_name),
            )
        })?;

    let response = plugin
        .handle_inbound(
            ChannelContext {
                service: runtime_state.service.clone(),
                channel_name,
            },
            ChannelInbound {
                payload,
                path_secret,
            },
        )
        .await
        .map_err(channel_err_to_http)?;

    Ok(Json(response.body))
}

fn channel_err_to_http(err: ChannelPluginError) -> (StatusCode, String) {
    match err {
        ChannelPluginError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
        ChannelPluginError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg),
        ChannelPluginError::Internal(inner) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("channel plugin failed: {inner}"),
        ),
    }
}

fn internal_err(message: &'static str) -> impl Fn(anyhow::Error) -> (StatusCode, String) {
    move |err| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("{message}: {err}"),
        )
    }
}

fn guard_code_mode_diag_access(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, String)> {
    if !is_code_mode_diag_authorized(headers, state.code_mode_diag_bearer_token.as_deref()) {
        return Err((
            StatusCode::UNAUTHORIZED,
            "code mode diagnostics unauthorized".to_string(),
        ));
    }

    let source_key = code_mode_diag_source_key(headers);
    if !state.code_mode_diag_rate_limiter.allow(&source_key) {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            "code mode diagnostics rate limited".to_string(),
        ));
    }

    Ok(())
}

fn is_code_mode_diag_authorized(headers: &HeaderMap, expected_token: Option<&str>) -> bool {
    let Some(expected) = expected_token.map(str::trim).filter(|v| !v.is_empty()) else {
        return false;
    };

    let Some(auth_header) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let mut parts = auth_header.splitn(2, char::is_whitespace);
    let Some(scheme) = parts.next() else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("bearer") {
        return false;
    }
    let Some(token) = parts.next().map(str::trim).filter(|v| !v.is_empty()) else {
        return false;
    };

    constant_time_eq(token.as_bytes(), expected.as_bytes())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let max_len = left.len().max(right.len());
    let mut diff = left.len() ^ right.len();
    for i in 0..max_len {
        let a = left.get(i).copied().unwrap_or(0);
        let b = right.get(i).copied().unwrap_or(0);
        diff |= (a ^ b) as usize;
    }
    diff == 0
}

fn code_mode_diag_source_key(headers: &HeaderMap) -> String {
    if let Some(value) = header_value_trimmed(headers, "cf-connecting-ip") {
        return normalize_source_key(&value);
    }
    if let Some(value) = header_value_trimmed(headers, "x-real-ip") {
        return normalize_source_key(&value);
    }
    if let Some(value) = header_value_trimmed(headers, "x-forwarded-for") {
        let first = value.split(',').next().map(str::trim).unwrap_or("");
        return normalize_source_key(first);
    }
    "unknown".to_string()
}

fn header_value_trimmed(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn normalize_source_key(source_key: &str) -> String {
    let trimmed = source_key.trim();
    if trimmed.is_empty() {
        return "unknown".to_string();
    }

    let mut normalized = String::with_capacity(trimmed.len().min(128));
    for ch in trimmed.chars().take(128) {
        normalized.push(ch.to_ascii_lowercase());
    }
    if normalized.is_empty() {
        "unknown".to_string()
    } else {
        normalized
    }
}

fn compact_source_windows_if_needed(
    by_source: &mut HashMap<String, FixedWindowSourceCounter>,
    current_window: u64,
    window_secs: u64,
    max_sources: usize,
) {
    if by_source.is_empty() {
        return;
    }

    let min_window = current_window.saturating_sub(window_secs);
    by_source.retain(|_, counter| counter.window_start_secs >= min_window);

    let hard_cap = max_sources.saturating_add(1);
    if by_source.len() <= hard_cap {
        return;
    }

    by_source.retain(|_, counter| counter.window_start_secs == current_window);
    if by_source.len() <= hard_cap {
        return;
    }

    // Defensive trim to keep memory bounded if the map was previously overgrown.
    let remove_count = by_source.len().saturating_sub(hard_cap);
    if remove_count == 0 {
        return;
    }
    let mut keys: Vec<String> = by_source.keys().cloned().collect();
    keys.sort_unstable();
    for key in keys.into_iter().take(remove_count) {
        by_source.remove(&key);
    }
}

fn active_window_start(now_secs: u64, window_secs: u64) -> u64 {
    let width = window_secs.max(1);
    now_secs - (now_secs % width)
}

fn current_unix_time_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{
        FixedWindowRateLimiter, code_mode_diag_source_key, constant_time_eq,
        is_code_mode_diag_authorized,
    };
    use axum::http::{HeaderMap, HeaderValue, header::AUTHORIZATION};

    #[test]
    fn constant_time_eq_matches_expected() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[test]
    fn code_mode_diag_authorization_accepts_case_insensitive_bearer() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("bearer diag-token"));
        assert!(is_code_mode_diag_authorized(&headers, Some("diag-token")));
    }

    #[test]
    fn code_mode_diag_authorization_rejects_invalid_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("basic abc"));
        assert!(!is_code_mode_diag_authorized(&headers, Some("diag-token")));

        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer wrong-token"),
        );
        assert!(!is_code_mode_diag_authorized(&headers, Some("diag-token")));
        assert!(!is_code_mode_diag_authorized(&headers, None));
    }

    #[test]
    fn fixed_window_rate_limiter_enforces_limit_and_resets_next_window() {
        let limiter = FixedWindowRateLimiter::new(60, 2);
        assert!(limiter.allow_at_for_source(120, "198.51.100.1"));
        assert!(limiter.allow_at_for_source(121, "198.51.100.1"));
        assert!(!limiter.allow_at_for_source(122, "198.51.100.1"));
        assert!(limiter.allow_at_for_source(180, "198.51.100.1"));
    }

    #[test]
    fn fixed_window_rate_limiter_allows_unlimited_when_limit_is_zero() {
        let limiter = FixedWindowRateLimiter::new(60, 0);
        for _ in 0..100 {
            assert!(limiter.allow_at_for_source(120, "198.51.100.1"));
        }
    }

    #[test]
    fn fixed_window_rate_limiter_isolated_by_source() {
        let limiter = FixedWindowRateLimiter::new(60, 1);
        assert!(limiter.allow_at_for_source(120, "198.51.100.1"));
        assert!(!limiter.allow_at_for_source(121, "198.51.100.1"));
        assert!(limiter.allow_at_for_source(121, "203.0.113.7"));
    }

    #[test]
    fn code_mode_diag_source_key_prefers_forwarded_first_ip() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("198.51.100.10, 198.51.100.20"),
        );
        assert_eq!(code_mode_diag_source_key(&headers), "198.51.100.10");
    }

    #[test]
    fn code_mode_diag_source_key_defaults_to_unknown() {
        let headers = HeaderMap::new();
        assert_eq!(code_mode_diag_source_key(&headers), "unknown");
    }
}
