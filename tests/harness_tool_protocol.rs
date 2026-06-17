use std::collections::HashMap;

use xiaomaolv::harness::tool_protocol::{
    ParsedToolCall, ToolExecutionEnvelope, ToolProposal, ToolProtocol,
};
use xiaomaolv::mcp::{
    BUILTIN_MCP_SERVER_NAME, BUILTIN_MCP_TOOL_CURRENT_TIME, McpRuntime, McpToolInfo,
};

fn demo_tools() -> Vec<McpToolInfo> {
    vec![McpToolInfo {
        server: "demo".to_string(),
        name: "search".to_string(),
        description: Some("search demo".to_string()),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"q": {"type": "string"}},
            "required": ["q"],
            "additionalProperties": false
        }),
        code_mode_capabilities: None,
    }]
}

#[test]
fn tool_protocol_parses_valid_tool_call() {
    let protocol = ToolProtocol::new(demo_tools(), 4000);
    let proposal =
        protocol.parse_reply(r#"{"server":"demo","tool":"search","arguments":{"q":"rust"}}"#);
    match proposal {
        ToolProposal::Tool(call) => {
            assert_eq!(call.server, "demo");
            assert_eq!(call.tool, "search");
            assert_eq!(call.arguments["q"], "rust");
        }
        other => panic!("expected tool proposal, got {other:?}"),
    }
}

#[test]
fn tool_protocol_rejects_schema_invalid_arguments() {
    let protocol = ToolProtocol::new(demo_tools(), 4000);
    let call = ParsedToolCall {
        server: "demo".to_string(),
        tool: "search".to_string(),
        arguments: serde_json::json!({"unknown": true}),
    };
    let result = protocol.validate_call(&call);
    assert!(result.is_err());
    let verification = result.err().expect("verification");
    assert!(
        verification
            .issues
            .iter()
            .any(|issue| issue.code == "MISSING_REQUIRED_ARGUMENT")
    );
    assert!(
        verification
            .issues
            .iter()
            .any(|issue| issue.code == "UNKNOWN_ARGUMENT")
    );
}

fn builtin_protocol(max_result_chars: usize) -> ToolProtocol {
    ToolProtocol::new(
        vec![McpToolInfo {
            server: BUILTIN_MCP_SERVER_NAME.to_string(),
            name: BUILTIN_MCP_TOOL_CURRENT_TIME.to_string(),
            description: Some("Get current trusted runtime time".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "timezone": {
                        "type": "string"
                    }
                },
                "additionalProperties": false
            }),
            code_mode_capabilities: None,
        }],
        max_result_chars,
    )
}

fn runtime() -> McpRuntime {
    McpRuntime::new(HashMap::new())
}

fn assert_success_envelope(envelope: &ToolExecutionEnvelope) {
    assert_eq!(envelope.message_json["server"], BUILTIN_MCP_SERVER_NAME);
    assert_eq!(envelope.message_json["tool"], BUILTIN_MCP_TOOL_CURRENT_TIME);
    assert_eq!(envelope.message_json["ok"], true);
    assert_eq!(envelope.record.server, BUILTIN_MCP_SERVER_NAME);
    assert_eq!(envelope.record.tool, BUILTIN_MCP_TOOL_CURRENT_TIME);
    assert!(envelope.record.ok);
    assert_eq!(envelope.record.iteration, 2);
}

#[tokio::test]
async fn tool_protocol_executes_validated_call_and_builds_success_envelope() {
    let protocol = builtin_protocol(4096);
    let call = ParsedToolCall {
        server: BUILTIN_MCP_SERVER_NAME.to_string(),
        tool: BUILTIN_MCP_TOOL_CURRENT_TIME.to_string(),
        arguments: serde_json::json!({"timezone":"Asia/Shanghai"}),
    };

    let envelope = protocol
        .execute_validated(&runtime(), call, 2)
        .await
        .expect("execute_validated");

    assert_success_envelope(&envelope);
    assert_eq!(envelope.record.arguments["timezone"], "Asia/Shanghai");
    assert_eq!(envelope.record.result["timezone"], "Asia/Shanghai");
    assert_eq!(envelope.message_json["result"]["timezone"], "Asia/Shanghai");
}

#[tokio::test]
async fn tool_protocol_wraps_runtime_failures_in_error_envelope() {
    let protocol = builtin_protocol(128);
    let call = ParsedToolCall {
        server: BUILTIN_MCP_SERVER_NAME.to_string(),
        tool: BUILTIN_MCP_TOOL_CURRENT_TIME.to_string(),
        arguments: serde_json::json!({"timezone":"Not/AZone"}),
    };

    let envelope = protocol
        .execute_validated(&runtime(), call, 1)
        .await
        .expect("execute_validated");

    assert_eq!(envelope.message_json["server"], BUILTIN_MCP_SERVER_NAME);
    assert_eq!(envelope.message_json["tool"], BUILTIN_MCP_TOOL_CURRENT_TIME);
    assert_eq!(envelope.message_json["ok"], false);
    assert!(envelope.message_json["error"].as_str().is_some());
    assert!(!envelope.record.ok);
    assert_eq!(envelope.record.iteration, 1);
    assert!(envelope.record.result["error"].as_str().is_some());
}
