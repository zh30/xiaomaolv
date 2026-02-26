use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use axum::extract::{Path, Query, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, Semaphore, watch};
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
use crate::service::{AgentMcpSettings, AgentSkillsSettings, CodeModeDiagnostics, MessageService};
use crate::skills::{SkillConfigPaths, SkillRegistry, SkillRuntime};

const CODE_MODE_DIAG_OVERFLOW_SOURCE_KEY: &str = "__overflow__";

#[derive(Clone)]
pub struct AppState {
    pub service: Arc<MessageService>,
    pub channel_plugins: Arc<HashMap<String, Arc<dyn ChannelPlugin>>>,
    pub mcp_runtime: Arc<RwLock<McpRuntime>>,
    pub semaphore: Arc<Semaphore>,
    pub code_mode_diag_bearer_token: Option<String>,
    code_mode_diag_rate_limiter: Arc<FixedWindowRateLimiter>,
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
    shutdown_tx: watch::Sender<bool>,
    workers: Vec<ChannelWorker>,
}

impl AppRuntime {
    pub fn into_parts(self) -> (Router, watch::Sender<bool>, Vec<ChannelWorker>) {
        (self.router, self.shutdown_tx, self.workers)
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

pub async fn build_router_with_registries(
    config: AppConfig,
    database_url: &str,
    provider_override: Option<Arc<dyn ChatProvider>>,
    provider_registry: ProviderRegistry,
    channel_registry: ChannelRegistry,
) -> anyhow::Result<Router> {
    let (state, http_enabled) = build_state(
        config,
        database_url,
        provider_override,
        provider_registry,
        channel_registry,
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
    let (state, http_enabled) = build_state(
        config,
        database_url,
        provider_override,
        provider_registry,
        channel_registry,
    )
    .await?;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let workers = start_channel_workers(&state, shutdown_rx).await?;
    let router = build_axum_router(state, http_enabled);

    Ok(AppRuntime {
        router,
        shutdown_tx,
        workers,
    })
}

async fn build_state(
    config: AppConfig,
    database_url: &str,
    provider_override: Option<Arc<dyn ChatProvider>>,
    provider_registry: ProviderRegistry,
    channel_registry: ChannelRegistry,
) -> anyhow::Result<(AppState, bool)> {
    let http_enabled = config.channels.http.enabled;

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
        ),
    );

    let channel_plugins = load_channel_plugins(&config, &channel_registry)?;

    let state =
        AppState {
            service,
            channel_plugins: Arc::new(channel_plugins),
            mcp_runtime,
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

fn build_axum_router(state: AppState, http_enabled: bool) -> Router {
    let mut router = Router::new().route("/health", get(health));

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
    state: &AppState,
    shutdown_rx: watch::Receiver<bool>,
) -> anyhow::Result<Vec<ChannelWorker>> {
    let mut workers = Vec::new();

    for (channel_name, plugin) in state.channel_plugins.iter() {
        let worker = plugin
            .start_background(ChannelRuntimeContext {
                service: state.service.clone(),
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

async fn get_mcp_servers(
    State(state): State<AppState>,
) -> Result<Json<McpServersResponse>, (StatusCode, String)> {
    let _permit = state.semaphore.acquire().await.map_err(|err| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("concurrency semaphore closed: {err}"),
        )
    })?;

    let runtime = state.mcp_runtime.read().await.clone();
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

    Ok(Json(state.service.code_mode_diagnostics()))
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
    Ok((
        response_headers,
        state.service.code_mode_metrics_prometheus(),
    ))
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

    let runtime = state.mcp_runtime.read().await.clone();
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

    let runtime = state.mcp_runtime.read().await.clone();
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

    let out = state
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
    let plugin = state.channel_plugins.get(&channel).ok_or_else(|| {
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
    let plugin = state.channel_plugins.get(&channel).ok_or_else(|| {
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

    let plugin = state.channel_plugins.get(&channel_name).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("channel '{}' is not configured", channel_name),
        )
    })?;

    let response = plugin
        .handle_inbound(
            ChannelContext {
                service: state.service.clone(),
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
