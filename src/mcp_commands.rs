use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::mcp::{
    McpAddSpec, McpConfigPaths, McpListScope, McpRegistry, McpRemoveScope, McpScope, McpTransport,
    parse_header_kv, parse_kv,
};

#[derive(Debug, Clone, Subcommand)]
pub enum McpCommands {
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
pub enum McpScopeArg {
    User,
    Project,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum McpListScopeArg {
    Merged,
    User,
    Project,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum McpRemoveScopeArg {
    All,
    User,
    Project,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum McpTransportArg {
    Stdio,
    Http,
}

#[derive(Debug, Clone, Args)]
pub struct McpAddArgs {
    /// MCP server name (example: tavily, brave-search)
    pub name: String,
    /// Install scope. Default user-level avoids accidental repo commits.
    #[arg(long, value_enum, default_value = "user")]
    pub scope: McpScopeArg,
    /// Transport type
    #[arg(long, value_enum, default_value = "stdio")]
    pub transport: McpTransportArg,
    /// Command for stdio transport
    #[arg(long)]
    pub command: Option<String>,
    /// Repeated argument for stdio command
    #[arg(long = "arg")]
    pub args: Vec<String>,
    /// Claude-style command tail, for example: -- npx -y @modelcontextprotocol/server-fetch
    #[arg(trailing_var_arg = true)]
    pub exec: Vec<String>,
    /// Working directory for stdio command
    #[arg(long)]
    pub cwd: Option<String>,
    /// Repeated env entry: KEY=VALUE
    #[arg(long = "env")]
    pub envs: Vec<String>,
    /// URL for http transport
    #[arg(long)]
    pub url: Option<String>,
    /// Repeated header entry: KEY=VALUE or KEY: VALUE
    #[arg(long = "header")]
    pub headers: Vec<String>,
    /// Request timeout seconds
    #[arg(long, default_value_t = 20)]
    pub timeout_secs: u64,
    /// Add server as disabled
    #[arg(long, default_value_t = false)]
    pub disabled: bool,
}

#[derive(Debug, Clone, Args)]
pub struct McpLsArgs {
    #[arg(long, value_enum, default_value = "merged")]
    pub scope: McpListScopeArg,
}

#[derive(Debug, Clone, Args)]
pub struct McpRmArgs {
    pub name: String,
    #[arg(long, value_enum, default_value = "all")]
    pub scope: McpRemoveScopeArg,
}

#[derive(Debug, Clone, Args)]
pub struct McpTestArgs {
    pub name: String,
}

#[derive(Debug, Parser)]
#[command(name = "mcp")]
#[command(disable_help_flag = true)]
#[command(disable_version_flag = true)]
struct TelegramMcpCli {
    #[command(subcommand)]
    command: McpCommands,
}

#[derive(Debug, Clone)]
pub struct McpCommandOutput {
    pub text: String,
    pub reload_runtime: bool,
}

pub fn discover_mcp_registry() -> anyhow::Result<McpRegistry> {
    let cwd = std::env::current_dir().context("failed to resolve current directory")?;
    let paths = McpConfigPaths::discover(&cwd)?;
    Ok(McpRegistry::new(paths))
}

pub fn parse_telegram_mcp_command(raw: &str) -> anyhow::Result<McpCommands> {
    let tokens = shlex::split(raw)
        .ok_or_else(|| anyhow::anyhow!("invalid command line: unmatched quote"))?;
    if tokens.is_empty() {
        bail!("missing mcp subcommand");
    }

    let mut argv = Vec::with_capacity(tokens.len() + 1);
    argv.push("mcp".to_string());
    argv.extend(tokens);

    let parsed = TelegramMcpCli::try_parse_from(argv)
        .map_err(|err| anyhow::anyhow!(err.to_string()))?
        .command;

    normalize_mcp_command(parsed)
}

pub fn normalize_mcp_command(command: McpCommands) -> anyhow::Result<McpCommands> {
    match command {
        McpCommands::Add(mut args) => {
            let (command, stdio_args) = resolve_stdio_command(args.command, args.args, args.exec)?;
            args.command = command;
            args.args = stdio_args;
            args.exec = vec![];
            Ok(McpCommands::Add(args))
        }
        other => Ok(other),
    }
}

pub async fn execute_mcp_command(
    registry: &McpRegistry,
    command: McpCommands,
) -> anyhow::Result<McpCommandOutput> {
    match normalize_mcp_command(command)? {
        McpCommands::Add(args) => {
            let McpAddArgs {
                name,
                scope,
                transport,
                command,
                args,
                exec: _,
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

            let text = format!(
                "mcp server '{}' installed ({})",
                name,
                match scope {
                    McpScopeArg::User => "user",
                    McpScopeArg::Project => "project",
                }
            );

            Ok(McpCommandOutput {
                text,
                reload_runtime: true,
            })
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
                return Ok(McpCommandOutput {
                    text: "no mcp servers configured".to_string(),
                    reload_runtime: false,
                });
            }

            let mut lines = Vec::new();
            lines.push(format!(
                "{:<20} {:<8} {:<8} {:<8} target",
                "name", "source", "enabled", "transport"
            ));
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
                lines.push(format!(
                    "{:<20} {:<8} {:<8} {:<8} {}",
                    row.name,
                    row.source,
                    if row.enabled { "yes" } else { "no" },
                    row.transport,
                    target
                ));
            }

            Ok(McpCommandOutput {
                text: lines.join("\n"),
                reload_runtime: false,
            })
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

            let text = if removed {
                format!("removed mcp server '{}'", args.name)
            } else {
                format!("mcp server '{}' not found", args.name)
            };

            Ok(McpCommandOutput {
                text,
                reload_runtime: removed,
            })
        }
        McpCommands::Test(args) => {
            let result = registry.test_server(&args.name).await?;
            let mut lines = Vec::new();
            lines.push(format!(
                "mcp test ok: server={} transport={} elapsed={}ms tools={}",
                result.server,
                result.transport,
                result.elapsed_ms,
                result.tools.len()
            ));
            for tool in result.tools {
                lines.push(format!(
                    "- {}::{}{}",
                    tool.server,
                    tool.name,
                    tool.description
                        .map(|d| format!(" - {d}"))
                        .unwrap_or_default()
                ));
            }
            Ok(McpCommandOutput {
                text: lines.join("\n"),
                reload_runtime: false,
            })
        }
    }
}

pub fn resolve_stdio_command(
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

pub fn mcp_help_text() -> String {
    [
        "可用命令:",
        "/mcp ls [--scope merged|user|project]",
        "/mcp test <name>",
        "/mcp rm <name> [--scope all|user|project]",
        "/mcp add <name> [--scope user|project] [--transport stdio|http] ...",
        "示例:",
        "/mcp add tavily --scope user -- npx -y @tavily/mcp-server",
    ]
    .join("\n")
}

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
