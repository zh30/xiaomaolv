use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use xiaomaolv::config::AppConfig;
use xiaomaolv::http::build_app_runtime_with_config_paths;
use xiaomaolv::mcp_commands::{McpCommands, discover_mcp_registry, execute_mcp_command};
use xiaomaolv::skills_commands::{SkillsCommands, discover_skill_registry, execute_skills_command};

#[derive(Debug, Parser)]
#[command(name = "xiaomaolv")]
#[command(about = "High-performance xiaomaolv-style gateway in Rust")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Generate a minimal configuration file
    Init {
        #[arg(long, default_value = "config/xiaomaolv.toml")]
        output: PathBuf,
    },
    /// Start the gateway service
    Serve {
        #[arg(long, default_value = "config/xiaomaolv.toml")]
        config: PathBuf,
        #[arg(long, default_value = "sqlite://xiaomaolv.db")]
        database: String,
    },
    /// Manage MCP servers (Claude-style install UX)
    Mcp {
        #[command(subcommand)]
        command: McpCommands,
    },
    /// Manage agent skills (install/search/use/update/remove)
    Skills {
        #[command(subcommand)]
        command: SkillsCommands,
    },
    #[command(name = "__code-mode-exec", hide = true)]
    CodeModeExec,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .compact()
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Init { output } => init_config(output).await,
        Commands::Serve { config, database } => serve(config, &database).await,
        Commands::Mcp { command } => handle_mcp(command).await,
        Commands::Skills { command } => handle_skills(command).await,
        Commands::CodeModeExec => handle_code_mode_exec().await,
    }
}

async fn init_config(path: PathBuf) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create directory: {}", parent.display()))?;
    }

    tokio::fs::write(&path, DEFAULT_CONFIG)
        .await
        .with_context(|| format!("failed to write config file: {}", path.display()))?;

    println!("generated config: {}", path.display());
    Ok(())
}

async fn serve(config_path: PathBuf, database_url: &str) -> anyhow::Result<()> {
    let config = AppConfig::from_path(&config_path).await?;
    let bind = config.app.bind.clone();
    let env_file_path = resolve_env_file_path()?;

    let runtime =
        build_app_runtime_with_config_paths(&config_path, &env_file_path, database_url, None)
            .await
            .context("failed to build app runtime")?;
    let (router, runtime_handle) = runtime.into_parts();

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("failed to bind {bind}"))?;

    tracing::info!(bind = %bind, "xiaomaolv is listening");

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server exited with error")?;

    runtime_handle.shutdown().await;

    Ok(())
}

fn resolve_env_file_path() -> anyhow::Result<PathBuf> {
    if let Ok(value) = std::env::var("XIAOMAOLV_ENV_FILE")
        && !value.trim().is_empty()
    {
        return Ok(PathBuf::from(value));
    }
    let cwd = std::env::current_dir().context("failed to resolve current directory")?;
    Ok(cwd.join(".env.realtest"))
}

async fn handle_mcp(command: McpCommands) -> anyhow::Result<()> {
    let registry = discover_mcp_registry()?;
    let output = execute_mcp_command(&registry, command).await?;
    if !output.text.is_empty() {
        println!("{}", output.text);
    }
    Ok(())
}

async fn handle_skills(command: SkillsCommands) -> anyhow::Result<()> {
    let registry = discover_skill_registry()?;
    let output = execute_skills_command(&registry, command).await?;
    if !output.text.is_empty() {
        println!("{}", output.text);
    }
    Ok(())
}

async fn handle_code_mode_exec() -> anyhow::Result<()> {
    xiaomaolv::code_mode::run_subprocess_exec_from_stdin().await
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
            sigterm.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

const DEFAULT_CONFIG: &str = r#"[app]
bind = "0.0.0.0:8080"
default_provider = "openai"
locale = "${XIAOMAOLV_LOCALE:-en-US}"
max_history = 16
concurrency_limit = 128
# Optional: protects HTTP APIs and config state/save endpoints when set.
api_key = "${XIAOMAOLV_APP_API_KEY:-}"

[providers.openai]
kind = "openai-compatible"
base_url = "https://api.openai.com/v1"
api_key = "${OPENAI_API_KEY}"
model = "gpt-4o-mini"
timeout_secs = 30
max_retries = 2

[channels.http]
enabled = true
# Required for GET /v1/code-mode/diag (Bearer token)
# diag_bearer_token = "${HTTP_DIAG_BEARER_TOKEN}"
diag_rate_limit_per_minute = 120

[channels.telegram]
enabled = false
bot_token = "${TELEGRAM_BOT_TOKEN}"
# Optional: your bot username without '@', used for group @mention filtering
# bot_username = "${TELEGRAM_BOT_USERNAME}"
polling_timeout_secs = 30
streaming_enabled = true
streaming_edit_interval_ms = 900
streaming_prefer_draft = true
startup_online_enabled = false
startup_online_text = "online"
commands_enabled = true
commands_auto_register = true
commands_private_only = true
admin_user_ids = "${TELEGRAM_ADMIN_USER_IDS:-}"

[memory]
backend = "sqlite-only" # sqlite-only | hybrid-sqlite-zvec
max_recent_turns = 0 # 0 means fallback to app.max_history
max_semantic_memories = 8
semantic_lookback_days = 90
context_window_tokens = 200000 # MiniMax 200k context window
context_reserved_tokens = 8192 # reserve for model output and system overhead
hybrid_keyword_enabled = true
hybrid_keyword_topk = 8
hybrid_keyword_candidate_limit = 256
hybrid_memory_snippet_max_chars = 420
hybrid_min_score = 0.18
context_memory_budget_ratio = 35
context_min_recent_messages = 8

[memory.zvec]
endpoint = "${ZVEC_SIDECAR_ENDPOINT}"
collection = "agent_memory_v1"
query_topk = 20
request_timeout_secs = 3
upsert_path = "/v1/memory/upsert"
query_path = "/v1/memory/query"
# auth_bearer_token = "${ZVEC_SIDECAR_TOKEN}"

[agent]
mcp_enabled = true
mcp_max_iterations = 4
mcp_max_tool_result_chars = 4000
skills_enabled = true
skills_max_selected = 3
skills_max_prompt_chars = 8000
skills_match_min_score = 0.45
skills_llm_rerank_enabled = false

[agent.harness]
enable_trajectory = false

[agent.harness.loop_engine]
# Durable /goal, /resume, approved Dynamic Workflow DAGs, read-only self-test,
# structural replay, immutable artifacts, and Desktop-ready HTTP/SSE.
enabled = false
# Scoped only to POST /v1/harness/signals; never reuse app.api_key.
ingest_api_key = "${XIAOMAOLV_HARNESS_INGEST_API_KEY:-}"
# Registered safe handlers only; external_write is rejected.
worker_enabled = false
# Valid: poll 1..=60s, lease 1..=3600s, parallel 1..=16.
worker_poll_interval_secs = 2
worker_lease_secs = 30
worker_max_parallel = 2
# 0 disables periodic read-only maintenance; otherwise valid 10..=2592000s.
self_test_interval_secs = 0

[agent.harness.evolution]
enabled = false
auto_cycle_enabled = false
cycle_interval_secs = 3600
cycle_initial_delay_secs = 60
max_source_trajectories = 20
max_evidence_chars = 8000
min_eval_cases = 3
min_candidate_score = 0.80
min_score_delta = 0.05
max_regressions = 0
max_prompt_patch_chars = 4000
require_human_approval = true

[agent.code_mode]
enabled = false
shadow_mode = true
max_calls = 6
max_parallel = 2
max_runtime_ms = 2500
max_call_timeout_ms = 1200
timeout_warn_ratio = 0.4
timeout_auto_shadow_enabled = false
timeout_auto_shadow_probe_every = 5
timeout_auto_shadow_streak = 3
max_result_chars = 12000
execution_mode = "local" # local | subprocess
subprocess_timeout_secs = 8
# Code Mode allows only MCP tools whose server config declares matching
# code_mode_capabilities. Tools with missing capability metadata are denied.
allow_network = false
allow_filesystem = false
allow_env = false
"#;
