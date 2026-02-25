use tempfile::tempdir;

use xiaomaolv::mcp::{McpConfigPaths, McpRegistry};
use xiaomaolv::mcp_commands::{McpCommands, execute_mcp_command, parse_telegram_mcp_command};

#[test]
fn parse_telegram_mcp_command_supports_claude_style_tail() {
    let cmd = parse_telegram_mcp_command("add tavily --scope user -- npx -y @tavily/mcp-server")
        .expect("parse should succeed");

    match cmd {
        McpCommands::Add(args) => {
            assert_eq!(args.name, "tavily");
            assert_eq!(args.command.as_deref(), Some("npx"));
            assert_eq!(args.args, vec!["-y", "@tavily/mcp-server"]);
            assert!(args.exec.is_empty());
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[test]
fn parse_telegram_mcp_command_rejects_mixed_styles() {
    let err = parse_telegram_mcp_command("add tavily --command npx --arg demo -- uvx demo-mcp")
        .expect_err("mixed styles should be rejected");

    assert!(
        err.to_string()
            .contains("either --command/--arg or -- <command> <args...>"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn execute_mcp_command_add_ls_rm_reports_reload_hint() {
    let tmp = tempdir().expect("tempdir");
    let registry = McpRegistry::new(McpConfigPaths {
        user: tmp.path().join("user.toml"),
        project: tmp.path().join("project.toml"),
    });

    let add = parse_telegram_mcp_command("add demo --scope user --command echo --arg ok")
        .expect("parse add");
    let add_out = execute_mcp_command(&registry, add)
        .await
        .expect("execute add");
    assert!(add_out.reload_runtime);
    assert!(add_out.text.contains("installed"));

    let ls = parse_telegram_mcp_command("ls --scope merged").expect("parse ls");
    let ls_out = execute_mcp_command(&registry, ls)
        .await
        .expect("execute ls");
    assert!(!ls_out.reload_runtime);
    assert!(ls_out.text.contains("demo"), "ls output: {}", ls_out.text);

    let rm = parse_telegram_mcp_command("rm demo --scope all").expect("parse rm");
    let rm_out = execute_mcp_command(&registry, rm)
        .await
        .expect("execute rm");
    assert!(rm_out.reload_runtime);
    assert!(rm_out.text.contains("removed"));
}
