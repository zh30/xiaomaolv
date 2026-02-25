use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;

use xiaomaolv::config::AppConfig;
use xiaomaolv::http::build_app_runtime;
use xiaomaolv::mcp::{
    McpAddSpec, McpConfigPaths, McpListScope, McpRegistry, McpRemoveScope, McpScope, McpTransport,
    parse_header_kv, parse_kv,
};

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
}

#[derive(Debug, Subcommand)]
enum McpCommands {
    /// Add an MCP server
    Add(McpAddArgs),
    /// List configured MCP servers
    Ls(McpLsArgs),
    /// Remove an MCP server
    Rm(McpRmArgs),
    /// Test MCP server connectivity and list tools
    Test(McpTestArgs),
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum McpScopeArg {
    User,
    Project,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum McpListScopeArg {
    Merged,
    User,
    Project,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum McpRemoveScopeArg {
    All,
    User,
    Project,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum McpTransportArg {
    Stdio,
    Http,
}

#[derive(Debug, Args)]
struct McpAddArgs {
    /// MCP server name (example: tavily, brave-search)
    name: String,
    /// Install scope. Default user-level avoids accidental repo commits.
    #[arg(long, value_enum, default_value = "user")]
    scope: McpScopeArg,
    /// Transport type
    #[arg(long, value_enum, default_value = "stdio")]
    transport: McpTransportArg,
    /// Command for stdio transport
    #[arg(long)]
    command: Option<String>,
    /// Repeated argument for stdio command
    #[arg(long = "arg")]
    args: Vec<String>,
    /// Claude-style command tail, for example: -- npx -y @modelcontextprotocol/server-fetch
    #[arg(trailing_var_arg = true)]
    exec: Vec<String>,
    /// Working directory for stdio command
    #[arg(long)]
    cwd: Option<String>,
    /// Repeated env entry: KEY=VALUE
    #[arg(long = "env")]
    envs: Vec<String>,
    /// URL for http transport
    #[arg(long)]
    url: Option<String>,
    /// Repeated header entry: KEY=VALUE or KEY: VALUE
    #[arg(long = "header")]
    headers: Vec<String>,
    /// Request timeout seconds
    #[arg(long, default_value_t = 20)]
    timeout_secs: u64,
    /// Add server as disabled
    #[arg(long, default_value_t = false)]
    disabled: bool,
}

#[derive(Debug, Args)]
struct McpLsArgs {
    #[arg(long, value_enum, default_value = "merged")]
    scope: McpListScopeArg,
}

#[derive(Debug, Args)]
struct McpRmArgs {
    name: String,
    #[arg(long, value_enum, default_value = "all")]
    scope: McpRemoveScopeArg,
}

#[derive(Debug, Args)]
struct McpTestArgs {
    name: String,
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

async fn handle_mcp(command: McpCommands) -> anyhow::Result<()> {
    let registry = mcp_registry()?;

    match command {
        McpCommands::Add(args) => {
            let McpAddArgs {
                name,
                scope,
                transport,
                command,
                args,
                exec,
                cwd,
                envs,
                url,
                headers: header_args,
                timeout_secs,
                disabled,
            } = args;

            let mut env = std::collections::HashMap::new();
            for raw in envs {
                let (k, v) = parse_kv(&raw)?;
                env.insert(k, v);
            }

            let mut headers = std::collections::HashMap::new();
            for raw in header_args {
                let (k, v) = parse_header_kv(&raw)?;
                headers.insert(k, v);
            }

            let (command, args) = resolve_stdio_command(command, args, exec)?;
            let transport = match transport {
                McpTransportArg::Stdio => McpTransport::Stdio,
                McpTransportArg::Http => McpTransport::Http,
            };

            match transport {
                McpTransport::Stdio => {
                    if url.is_some() {
                        bail!("stdio transport does not use --url");
                    }
                }
                McpTransport::Http => {
                    if command.is_some() || !args.is_empty() || cwd.is_some() || !env.is_empty() {
                        bail!(
                            "http transport does not use stdio fields (--command/--arg/--cwd/--env/-- <cmd>)"
                        );
                    }
                }
            }

            let spec = McpAddSpec {
                transport,
                command,
                args,
                cwd,
                env,
                url,
                headers,
                timeout_secs: timeout_secs.max(1),
                enabled: !disabled,
            };

            registry
                .add_server(
                    match scope {
                        McpScopeArg::User => McpScope::User,
                        McpScopeArg::Project => McpScope::Project,
                    },
                    &name,
                    spec,
                )
                .await?;

            println!(
                "mcp server '{}' installed ({})",
                name,
                match scope {
                    McpScopeArg::User => "user",
                    McpScopeArg::Project => "project",
                }
            );
        }
        McpCommands::Ls(args) => {
            let rows = registry
                .list_servers(match args.scope {
                    McpListScopeArg::Merged => McpListScope::Merged,
                    McpListScopeArg::User => McpListScope::User,
                    McpListScopeArg::Project => McpListScope::Project,
                })
                .await?;

            if rows.is_empty() {
                println!("no mcp servers configured");
                return Ok(());
            }

            println!(
                "{:<20} {:<8} {:<8} {:<8} target",
                "name", "source", "enabled", "transport"
            );
            for row in rows {
                let target = row
                    .command
                    .map(|cmd| {
                        if row.args.is_empty() {
                            cmd
                        } else {
                            format!("{cmd} {}", row.args.join(" "))
                        }
                    })
                    .or(row.url)
                    .unwrap_or_else(|| "-".to_string());
                println!(
                    "{:<20} {:<8} {:<8} {:<8} {}",
                    row.name,
                    row.source,
                    if row.enabled { "yes" } else { "no" },
                    row.transport,
                    target
                );
            }
        }
        McpCommands::Rm(args) => {
            let removed = registry
                .remove_server(
                    match args.scope {
                        McpRemoveScopeArg::All => McpRemoveScope::All,
                        McpRemoveScopeArg::User => McpRemoveScope::User,
                        McpRemoveScopeArg::Project => McpRemoveScope::Project,
                    },
                    &args.name,
                )
                .await?;
            if removed {
                println!("removed mcp server '{}'", args.name);
            } else {
                println!("mcp server '{}' not found", args.name);
            }
        }
        McpCommands::Test(args) => {
            let result = registry.test_server(&args.name).await?;
            println!(
                "mcp test ok: server={} transport={} elapsed={}ms tools={}",
                result.server,
                result.transport,
                result.elapsed_ms,
                result.tools.len()
            );
            for tool in result.tools {
                println!(
                    "- {}::{}{}",
                    tool.server,
                    tool.name,
                    tool.description
                        .map(|d| format!(" - {d}"))
                        .unwrap_or_default()
                );
            }
        }
    }

    Ok(())
}

fn resolve_stdio_command(
    command: Option<String>,
    args: Vec<String>,
    exec: Vec<String>,
) -> anyhow::Result<(Option<String>, Vec<String>)> {
    if exec.is_empty() {
        return Ok((command, args));
    }

    if command.is_some() || !args.is_empty() {
        bail!("use either --command/--arg or -- <command> <args...>, not both");
    }

    let mut iter = exec.into_iter();
    let cmd = iter
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing stdio command after '--'"))?;
    Ok((Some(cmd), iter.collect()))
}

fn mcp_registry() -> anyhow::Result<McpRegistry> {
    let cwd = std::env::current_dir().context("failed to resolve current directory")?;
    let paths = McpConfigPaths::discover(&cwd)?;
    Ok(McpRegistry::new(paths))
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
# Optional: your bot username without '@', used for group @mention filtering
# bot_username = "${TELEGRAM_BOT_USERNAME}"
mode = "polling"
polling_timeout_secs = 30
streaming_enabled = true
streaming_edit_interval_ms = 900
streaming_prefer_draft = true
startup_online_enabled = false
startup_online_text = "online"

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

[agent]
mcp_enabled = true
mcp_max_iterations = 4
mcp_max_tool_result_chars = 4000
"#;

#[cfg(test)]
mod tests {
    use super::resolve_stdio_command;

    #[test]
    fn resolve_stdio_command_supports_claude_style_tail() {
        let (command, args) = resolve_stdio_command(
            None,
            vec![],
            vec!["npx".to_string(), "-y".to_string(), "demo-mcp".to_string()],
        )
        .expect("resolve");
        assert_eq!(command.as_deref(), Some("npx"));
        assert_eq!(args, vec!["-y".to_string(), "demo-mcp".to_string()]);
    }

    #[test]
    fn resolve_stdio_command_rejects_mixed_styles() {
        let err = resolve_stdio_command(
            Some("npx".to_string()),
            vec![],
            vec!["uvx".to_string(), "demo-mcp".to_string()],
        )
        .expect_err("should fail");
        assert!(
            err.to_string()
                .contains("either --command/--arg or -- <command> <args...>")
        );
    }
}
