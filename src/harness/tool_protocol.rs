use std::time::Instant;

use serde_json::Value;

use crate::config::ToolVerificationMode;
use crate::domain::{MessageRole, StoredMessage};
use crate::harness::trajectory::ToolCallRecord;
use crate::harness::verifier::{
    IssueSeverity, ToolSchemaVerifier, VerificationIssue, VerificationResult,
};
use crate::json_utils::extract_json_payload;
use crate::mcp::{McpRuntime, McpToolInfo};

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedToolCall {
    pub server: String,
    pub tool: String,
    pub arguments: Value,
}

#[derive(Debug, Clone)]
pub enum ToolProposal {
    Tool(ParsedToolCall),
    FinalAnswer,
    ParseError(VerificationResult),
}

#[derive(Debug, Clone)]
pub struct ToolExecutionEnvelope {
    pub message_json: Value,
    pub record: ToolCallRecord,
}

#[derive(Clone)]
pub struct ToolProtocol {
    tools: Vec<McpToolInfo>,
    max_result_chars: usize,
}

impl ToolProtocol {
    pub fn new(tools: Vec<McpToolInfo>, max_result_chars: usize) -> Self {
        Self {
            tools,
            max_result_chars,
        }
    }

    pub fn parse_reply(&self, reply: &str) -> ToolProposal {
        parse_tool_call_attempt(reply)
    }

    pub fn validate_call(&self, call: &ParsedToolCall) -> Result<&McpToolInfo, VerificationResult> {
        let tool_info = self
            .tools
            .iter()
            .find(|tool| tool.server == call.server && tool.name == call.tool)
            .ok_or_else(|| {
                verification_failure_result(
                    "UNKNOWN_TOOL",
                    format!(
                        "Requested MCP tool is not available: {}::{}",
                        call.server, call.tool
                    ),
                )
            })?;
        let verification = ToolSchemaVerifier::new().verify_arguments(tool_info, &call.arguments);
        if verification.passed {
            Ok(tool_info)
        } else {
            Err(verification)
        }
    }

    pub async fn execute_validated(
        &self,
        runtime: &McpRuntime,
        call: ParsedToolCall,
        iteration: usize,
    ) -> anyhow::Result<ToolExecutionEnvelope> {
        let started = Instant::now();
        let result = runtime
            .call_tool(&call.server, &call.tool, call.arguments.clone())
            .await;
        let duration_ms = started.elapsed().as_millis() as u64;

        Ok(match result {
            Ok(value) => {
                let result = truncate_json_value(&value, self.max_result_chars);
                let record = ToolCallRecord {
                    call_index: 0,
                    server: call.server.clone(),
                    tool: call.tool.clone(),
                    arguments: call.arguments,
                    result: result.clone(),
                    ok: true,
                    duration_ms,
                    iteration,
                };
                ToolExecutionEnvelope {
                    message_json: serde_json::json!({
                        "server": call.server,
                        "tool": call.tool,
                        "ok": true,
                        "result": result
                    }),
                    record,
                }
            }
            Err(err) => {
                let error_json = serde_json::json!({ "error": err.to_string() });
                let record = ToolCallRecord {
                    call_index: 0,
                    server: call.server.clone(),
                    tool: call.tool.clone(),
                    arguments: call.arguments,
                    result: error_json.clone(),
                    ok: false,
                    duration_ms,
                    iteration,
                };
                ToolExecutionEnvelope {
                    message_json: serde_json::json!({
                        "server": call.server,
                        "tool": call.tool,
                        "ok": false,
                        "error": err.to_string()
                    }),
                    record,
                }
            }
        })
    }
}

fn parse_tool_call_attempt(reply: &str) -> ToolProposal {
    let Some(json_text) = extract_json_payload(reply.trim()) else {
        return if looks_like_attempted_mcp_tool_call(reply) {
            ToolProposal::ParseError(verification_failure_result(
                "MALFORMED_TOOL_CALL_JSON",
                "Tool call JSON could not be extracted",
            ))
        } else {
            ToolProposal::FinalAnswer
        };
    };

    let value: Value = match serde_json::from_str(&json_text) {
        Ok(value) => value,
        Err(err) if looks_like_attempted_mcp_tool_call(reply) => {
            return ToolProposal::ParseError(verification_failure_result(
                "MALFORMED_TOOL_CALL_JSON",
                format!("Tool call JSON is malformed: {err}"),
            ));
        }
        Err(_) => return ToolProposal::FinalAnswer,
    };

    match parse_tool_call_value(&value) {
        Some(call) => ToolProposal::Tool(call),
        None if looks_like_attempted_mcp_tool_call(reply) => {
            ToolProposal::ParseError(verification_failure_result(
                "INVALID_TOOL_CALL_SHAPE",
                "Tool call JSON is missing server/tool/arguments shape",
            ))
        }
        None => ToolProposal::FinalAnswer,
    }
}

fn parse_tool_call_value(value: &Value) -> Option<ParsedToolCall> {
    if let Some(inner) = value.get("tool_call") {
        return parse_tool_call_value(inner);
    }
    if let Some(inner) = value.get("mcp_tool_call") {
        return parse_tool_call_value(inner);
    }
    if let Some(items) = value.as_array() {
        return items.first().and_then(parse_tool_call_value);
    }

    let obj = value.as_object()?;
    let arguments = obj
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));

    if let (Some(server), Some(tool)) = (
        obj.get("server").and_then(|v| v.as_str()),
        obj.get("tool").and_then(|v| v.as_str()),
    ) {
        return Some(ParsedToolCall {
            server: server.to_string(),
            tool: tool.to_string(),
            arguments,
        });
    }

    let name = obj.get("name").and_then(|v| v.as_str())?;
    let (server, tool) = name.split_once("::").or_else(|| name.split_once('/'))?;
    Some(ParsedToolCall {
        server: server.to_string(),
        tool: tool.to_string(),
        arguments,
    })
}

fn looks_like_attempted_mcp_tool_call(reply: &str) -> bool {
    let lower = reply.to_ascii_lowercase();
    lower.contains("tool_call")
        || lower.contains("mcp_tool_call")
        || (lower.contains("server") && lower.contains("tool"))
        || (lower.contains("arguments") && lower.contains("tool"))
}

fn verification_failure_result(code: &str, message: impl Into<String>) -> VerificationResult {
    VerificationResult {
        passed: false,
        confidence: 1.0,
        issues: vec![VerificationIssue {
            severity: IssueSeverity::Error,
            code: code.to_string(),
            message: message.into(),
        }],
        suggestion: None,
    }
}

fn verification_result_json(verification: &VerificationResult) -> Value {
    serde_json::to_value(verification).unwrap_or_else(|_| {
        serde_json::json!({
            "passed": verification.passed,
            "confidence": verification.confidence,
            "issues": []
        })
    })
}

pub fn annotate_record_with_verification_failure(
    record: &mut ToolCallRecord,
    verification: &VerificationResult,
) {
    let original_result = record.result.clone();
    record.ok = false;
    record.result = serde_json::json!({
        "verification_failed": true,
        "verification": verification_result_json(verification),
        "original_result": original_result
    });
}

pub(crate) fn verification_failure_record(
    tool_call: &ParsedToolCall,
    verification: &VerificationResult,
    iteration: usize,
) -> ToolCallRecord {
    const UNKNOWN_TOOL_RECORD_SERVER: &str = "unknown";
    const UNKNOWN_TOOL_RECORD_TOOL: &str = "invalid";

    let unknown_tool = verification
        .issues
        .iter()
        .any(|issue| issue.code == "UNKNOWN_TOOL");
    let mut result = serde_json::json!({
        "verification_failed": true,
        "verification": verification_result_json(verification)
    });
    if unknown_tool && let Some(result) = result.as_object_mut() {
        result.insert(
            "requested_server".to_string(),
            Value::String(tool_call.server.clone()),
        );
        result.insert(
            "requested_tool".to_string(),
            Value::String(tool_call.tool.clone()),
        );
    }

    ToolCallRecord {
        call_index: 0,
        server: if unknown_tool {
            UNKNOWN_TOOL_RECORD_SERVER.to_string()
        } else {
            tool_call.server.clone()
        },
        tool: if unknown_tool {
            UNKNOWN_TOOL_RECORD_TOOL.to_string()
        } else {
            tool_call.tool.clone()
        },
        arguments: tool_call.arguments.clone(),
        result,
        ok: false,
        duration_ms: 0,
        iteration,
    }
}

pub fn verification_feedback_message(
    verification: &VerificationResult,
    mode: ToolVerificationMode,
) -> StoredMessage {
    let instruction = match mode {
        ToolVerificationMode::Retry => {
            "The previous MCP tool call failed verification before its result was accepted. Retry once with corrected arguments or provide a final answer without using that failed result."
        }
        ToolVerificationMode::Block => {
            "The previous MCP tool call failed verification. Do not call more tools. Provide a safe final answer that explains the tool failure briefly and avoids relying on the failed result."
        }
        ToolVerificationMode::Observe => {
            "The previous MCP tool call had verification issues. Continue normally."
        }
    };

    StoredMessage {
        role: MessageRole::System,
        content: format!(
            "MCP_TOOL_VERIFICATION_FAILED_JSON:\n{}\n\n{}",
            serde_json::to_string(&verification_result_json(verification))
                .unwrap_or_else(|_| "{\"passed\":false,\"issues\":[]}".to_string()),
            instruction
        ),
    }
}

pub(crate) fn truncate_json_value(value: &Value, max_chars: usize) -> Value {
    let encoded = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    if encoded.chars().count() <= max_chars {
        return value.clone();
    }
    let mut out = encoded.chars().take(max_chars).collect::<String>();
    out.push_str("...(truncated)");
    serde_json::json!({ "truncated": out })
}

#[cfg(test)]
mod tests {
    use super::{ParsedToolCall, verification_failure_record};
    use crate::harness::verifier::{IssueSeverity, VerificationIssue, VerificationResult};

    #[test]
    fn verification_failure_record_marks_unknown_tools_and_preserves_requested_shape() {
        let call = ParsedToolCall {
            server: "demo".to_string(),
            tool: "search".to_string(),
            arguments: serde_json::json!({"q":"rust"}),
        };
        let verification = VerificationResult {
            passed: false,
            confidence: 1.0,
            issues: vec![VerificationIssue {
                severity: IssueSeverity::Error,
                code: "UNKNOWN_TOOL".to_string(),
                message: "Requested MCP tool is not available: demo::search".to_string(),
            }],
            suggestion: None,
        };

        let record = verification_failure_record(&call, &verification, 4);

        assert_eq!(record.server, "unknown");
        assert_eq!(record.tool, "invalid");
        assert_eq!(record.arguments, serde_json::json!({"q":"rust"}));
        assert!(!record.ok);
        assert_eq!(record.iteration, 4);
        assert_eq!(record.result["verification_failed"], true);
        assert_eq!(
            record.result["verification"]["issues"][0]["code"],
            "UNKNOWN_TOOL"
        );
        assert_eq!(record.result["requested_server"], "demo");
        assert_eq!(record.result["requested_tool"], "search");
    }
}
