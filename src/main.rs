use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use xiaomaolv::config::AppConfig;
use xiaomaolv::http::build_app_runtime;

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

    let runtime = build_app_runtime(config, database_url, None)
        .await
        .context("failed to build app runtime")?;
    let (router, shutdown_tx, workers) = runtime.into_parts();

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("failed to bind {bind}"))?;

    tracing::info!(bind = %bind, "xiaomaolv is listening");

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server exited with error")?;

    let _ = shutdown_tx.send(true);
    for worker in workers {
        worker.task.abort();
        let _ = worker.task.await;
    }

    Ok(())
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
max_history = 16
concurrency_limit = 128

[providers.openai]
kind = "openai-compatible"
base_url = "https://api.openai.com/v1"
api_key = "${OPENAI_API_KEY}"
model = "gpt-4o-mini"
timeout_secs = 30
max_retries = 2

[channels.http]
enabled = true

[channels.telegram]
enabled = false
bot_token = "${TELEGRAM_BOT_TOKEN}"
mode = "polling"
polling_timeout_secs = 30
streaming_enabled = true
streaming_edit_interval_ms = 900

# Optional webhook mode (requires public HTTPS endpoint):
# mode = "webhook"
# webhook_secret = "${TELEGRAM_WEBHOOK_SECRET}"

[memory]
backend = "sqlite-only" # sqlite-only | hybrid-sqlite-zvec
max_recent_turns = 0 # 0 means fallback to app.max_history
max_semantic_memories = 8
semantic_lookback_days = 90

[memory.zvec]
endpoint = "${ZVEC_SIDECAR_ENDPOINT}"
collection = "agent_memory_v1"
query_topk = 20
request_timeout_secs = 3
upsert_path = "/v1/memory/upsert"
query_path = "/v1/memory/query"
# auth_bearer_token = "${ZVEC_SIDECAR_TOKEN}"
"#;
