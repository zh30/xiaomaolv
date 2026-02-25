use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, bail};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, Semaphore, watch};
use tracing::warn;

use crate::channel::{
    ChannelContext, ChannelInbound, ChannelPlugin, ChannelPluginConfig, ChannelPluginError,
    ChannelRegistry, ChannelRuntimeContext, ChannelWorker,
};
use crate::config::AppConfig;
use crate::domain::IncomingMessage;
use crate::mcp::{McpConfigPaths, McpRegistry, McpRuntime};
use crate::memory::{
    HybridSqliteZvecMemoryBackend, MemoryBackend, SqliteMemoryBackend, SqliteMemoryStore,
    ZvecSidecarClient, ZvecSidecarConfig,
};
use crate::provider::{ChatProvider, ProviderRegistry};
use crate::service::{AgentMcpSettings, MessageService};

#[derive(Clone)]
pub struct AppState {
    pub service: Arc<MessageService>,
    pub channel_plugins: Arc<HashMap<String, Arc<dyn ChannelPlugin>>>,
    pub mcp_runtime: Arc<RwLock<McpRuntime>>,
    pub semaphore: Arc<Semaphore>,
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
            Arc::new(HybridSqliteZvecMemoryBackend::new(
                memory_store.clone(),
                sidecar,
            ))
        }
        other => bail!(
            "unsupported memory.backend '{}', expected sqlite-only|hybrid-sqlite-zvec",
            other
        ),
    };

    let mcp_runtime = Arc::new(RwLock::new(load_mcp_runtime().await));
    let service = Arc::new(MessageService::new_with_backend(
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
    ));

    let channel_plugins = load_channel_plugins(&config, &channel_registry)?;

    let state = AppState {
        service,
        channel_plugins: Arc::new(channel_plugins),
        mcp_runtime,
        semaphore: Arc::new(Semaphore::new(config.app.concurrency_limit.max(1))),
    };

    Ok((state, http_enabled))
}

fn build_axum_router(state: AppState, http_enabled: bool) -> Router {
    let mut router = Router::new().route("/health", get(health));

    if http_enabled {
        router = router.route("/v1/messages", post(post_message));
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
        .route("/v1/telegram/webhook/{secret}", post(post_telegram_webhook))
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

        if let Some(secret) = &telegram.webhook_secret
            && !secret.trim().is_empty()
        {
            settings.insert("webhook_secret".to_string(), secret.clone());
        }

        if let Some(mode) = &telegram.mode
            && !mode.trim().is_empty()
        {
            settings.insert("mode".to_string(), mode.clone());
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

async fn post_telegram_webhook(
    State(state): State<AppState>,
    Path(secret): Path<String>,
    Json(payload): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    dispatch_channel_inbound(state, "telegram".to_string(), Some(secret), payload).await
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
