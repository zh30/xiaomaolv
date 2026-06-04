use std::sync::Arc;

use crate::harness::trajectory::ToolCallRecord;
use crate::mcp::McpToolInfo;
use crate::provider::ChatProvider;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Verification result with confidence score and issues
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationResult {
    pub passed: bool,
    pub confidence: f64,
    pub issues: Vec<VerificationIssue>,
    pub suggestion: Option<String>,
}

/// A single issue found during verification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationIssue {
    pub severity: IssueSeverity,
    pub code: String,
    pub message: String,
}

/// Severity level for issues
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueSeverity {
    Warning,
    Error,
    Critical,
}

impl IssueSeverity {
    pub fn as_str(&self) -> &'static str {
        match self {
            IssueSeverity::Warning => "warning",
            IssueSeverity::Error => "error",
            IssueSeverity::Critical => "critical",
        }
    }
}

/// Trait for tool call verifiers
pub trait ToolCallVerifier: Send + Sync {
    fn verify(&self, tool_call: &ToolCallRecord) -> VerificationResult;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputVerificationRequest {
    pub final_answer: String,
    pub recent_history: Vec<crate::domain::StoredMessage>,
    pub tool_calls: Vec<ToolCallRecord>,
    pub channel: String,
    pub required_format: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputVerificationResult {
    pub passed: bool,
    pub confidence: f64,
    pub issues: Vec<VerificationIssue>,
    pub suggested_revision: Option<String>,
}

#[derive(Clone)]
pub struct DeterministicOutputVerifier;

impl DeterministicOutputVerifier {
    pub fn new() -> Self {
        Self
    }

    pub fn verify(&self, req: &OutputVerificationRequest) -> OutputVerificationResult {
        let mut issues = Vec::new();
        let answer = req.final_answer.trim();

        if answer.is_empty() {
            issues.push(VerificationIssue {
                severity: IssueSeverity::Error,
                code: "EMPTY_ANSWER".to_string(),
                message: "Final answer is empty".to_string(),
            });
        }

        if looks_like_unresolved_tool_call(answer) {
            issues.push(VerificationIssue {
                severity: IssueSeverity::Error,
                code: "UNRESOLVED_TOOL_CALL".to_string(),
                message: "Final answer appears to expose an unresolved MCP tool call".to_string(),
            });
        }

        let has_hidden_tool_error = req.tool_calls.iter().any(|call| {
            !call.ok
                || call
                    .result
                    .get("verification_failed")
                    .and_then(Value::as_bool)
                    == Some(true)
        }) && !mentions_tool_failure(answer);
        if has_hidden_tool_error {
            issues.push(VerificationIssue {
                severity: IssueSeverity::Error,
                code: "HIDDEN_TOOL_ERROR".to_string(),
                message: "Final answer does not disclose a failed tool result".to_string(),
            });
        }

        if req.required_format.as_deref() == Some("json")
            && serde_json::from_str::<Value>(answer).is_err()
        {
            issues.push(VerificationIssue {
                severity: IssueSeverity::Error,
                code: "REQUIRED_FORMAT_MISMATCH".to_string(),
                message: "Final answer is not valid JSON".to_string(),
            });
        }

        let passed = issues.is_empty();
        OutputVerificationResult {
            passed,
            confidence: if passed { 1.0 } else { 0.95 },
            issues,
            suggested_revision: if passed {
                None
            } else {
                Some(
                    "I could not produce a reliable final answer from the available tool results."
                        .to_string(),
                )
            },
        }
    }
}

impl Default for DeterministicOutputVerifier {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Timing Verifier
// =============================================================================

/// Verifier that checks tool call duration
pub struct TimingVerifier {
    max_duration_ms: u64,
    warn_ratio: f64,
}

impl TimingVerifier {
    pub fn new(max_duration_ms: u64, warn_ratio: f64) -> Self {
        Self {
            max_duration_ms,
            warn_ratio,
        }
    }
}

impl ToolCallVerifier for TimingVerifier {
    fn verify(&self, call: &ToolCallRecord) -> VerificationResult {
        if call.duration_ms > self.max_duration_ms {
            return VerificationResult {
                passed: true, // Not a hard failure
                confidence: 0.8,
                issues: vec![VerificationIssue {
                    severity: IssueSeverity::Warning,
                    code: "SLOW_TOOL".to_string(),
                    message: format!(
                        "Tool took {}ms (>{}ms threshold)",
                        call.duration_ms, self.max_duration_ms
                    ),
                }],
                suggestion: Some("Consider caching this result".to_string()),
            };
        }

        // Also warn if duration exceeds warn_ratio of max
        let warn_threshold = (self.max_duration_ms as f64 * self.warn_ratio) as u64;
        if call.duration_ms > warn_threshold {
            return VerificationResult {
                passed: true,
                confidence: 0.9,
                issues: vec![VerificationIssue {
                    severity: IssueSeverity::Warning,
                    code: "SLOW_TOOL".to_string(),
                    message: format!(
                        "Tool took {}ms (>{:.0}% of {}ms threshold)",
                        call.duration_ms,
                        self.warn_ratio * 100.0,
                        self.max_duration_ms
                    ),
                }],
                suggestion: Some("Consider caching this result".to_string()),
            };
        }

        VerificationResult {
            passed: true,
            confidence: 1.0,
            issues: vec![],
            suggestion: None,
        }
    }
}

// =============================================================================
// Tool Schema Verifier
// =============================================================================

/// Verifier that checks MCP tool arguments against common JSON Schema fields.
#[derive(Clone)]
pub struct ToolSchemaVerifier;

impl ToolSchemaVerifier {
    pub fn new() -> Self {
        Self
    }

    pub fn verify_arguments(&self, tool: &McpToolInfo, arguments: &Value) -> VerificationResult {
        let schema = &tool.input_schema;
        let mut issues = Vec::new();

        if !arguments.is_object() {
            issues.push(VerificationIssue {
                severity: IssueSeverity::Error,
                code: "ARGUMENTS_NOT_OBJECT".to_string(),
                message: format!(
                    "Arguments for {}::{} must be an object",
                    tool.server, tool.name
                ),
            });
            return failed_result(issues);
        }

        if let Some(expected_type) = schema.get("type").and_then(Value::as_str)
            && expected_type != "object"
        {
            issues.push(VerificationIssue {
                severity: IssueSeverity::Warning,
                code: "UNSUPPORTED_SCHEMA_ROOT".to_string(),
                message: format!(
                    "Only object input schemas are validated, got root type '{expected_type}'"
                ),
            });
        }

        let Some(args) = arguments.as_object() else {
            return failed_result(issues);
        };
        let properties = schema.get("properties").and_then(Value::as_object);

        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for required_name in required.iter().filter_map(Value::as_str) {
                if !args.contains_key(required_name) {
                    issues.push(VerificationIssue {
                        severity: IssueSeverity::Error,
                        code: "MISSING_REQUIRED_ARGUMENT".to_string(),
                        message: format!(
                            "Missing required argument '{required_name}' for {}::{}",
                            tool.server, tool.name
                        ),
                    });
                }
            }
        }

        if let Some(properties) = properties {
            for (name, value) in args {
                if let Some(prop_schema) = properties.get(name)
                    && let Some(expected_type) = prop_schema.get("type").and_then(Value::as_str)
                    && !json_type_matches(value, expected_type)
                {
                    issues.push(VerificationIssue {
                        severity: IssueSeverity::Error,
                        code: "ARGUMENT_TYPE_MISMATCH".to_string(),
                        message: format!(
                            "Argument '{name}' for {}::{} must be {expected_type}",
                            tool.server, tool.name
                        ),
                    });
                }
            }
        }

        if schema.get("additionalProperties").and_then(Value::as_bool) == Some(false)
            && let Some(properties) = properties
        {
            for name in args.keys() {
                if !properties.contains_key(name) {
                    issues.push(VerificationIssue {
                        severity: IssueSeverity::Error,
                        code: "UNKNOWN_ARGUMENT".to_string(),
                        message: format!(
                            "Unknown argument '{name}' for {}::{}",
                            tool.server, tool.name
                        ),
                    });
                }
            }
        }

        if issues
            .iter()
            .any(|issue| !matches!(issue.severity, IssueSeverity::Warning))
        {
            failed_result(issues)
        } else {
            VerificationResult {
                passed: true,
                confidence: if issues.is_empty() { 1.0 } else { 0.85 },
                issues,
                suggestion: None,
            }
        }
    }
}

impl Default for ToolSchemaVerifier {
    fn default() -> Self {
        Self::new()
    }
}

/// Verifier that checks common MCP result failure shapes.
pub struct ResultShapeVerifier;

impl ResultShapeVerifier {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ResultShapeVerifier {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolCallVerifier for ResultShapeVerifier {
    fn verify(&self, call: &ToolCallRecord) -> VerificationResult {
        let mut issues = Vec::new();
        if !call.ok {
            issues.push(VerificationIssue {
                severity: IssueSeverity::Error,
                code: "TOOL_ERROR".to_string(),
                message: format!("Tool {}::{} returned an error", call.server, call.tool),
            });
        }

        match &call.result {
            Value::Null => issues.push(VerificationIssue {
                severity: IssueSeverity::Error,
                code: "NULL_RESULT".to_string(),
                message: "Tool result is null".to_string(),
            }),
            Value::String(value) if value.trim().is_empty() => issues.push(VerificationIssue {
                severity: IssueSeverity::Error,
                code: "EMPTY_RESULT".to_string(),
                message: "Tool result is an empty string".to_string(),
            }),
            Value::Array(values) if values.is_empty() => issues.push(VerificationIssue {
                severity: IssueSeverity::Error,
                code: "EMPTY_RESULT".to_string(),
                message: "Tool result is an empty array".to_string(),
            }),
            Value::Object(map) => {
                if map.is_empty() {
                    issues.push(VerificationIssue {
                        severity: IssueSeverity::Error,
                        code: "EMPTY_RESULT".to_string(),
                        message: "Tool result is an empty object".to_string(),
                    });
                }
                if map.contains_key("error") {
                    issues.push(VerificationIssue {
                        severity: IssueSeverity::Error,
                        code: "ERROR_OBJECT_RESULT".to_string(),
                        message: "Tool result contains an error field".to_string(),
                    });
                }
                if map.contains_key("truncated") {
                    issues.push(VerificationIssue {
                        severity: IssueSeverity::Error,
                        code: "TRUNCATED_RESULT".to_string(),
                        message: "Tool result was truncated".to_string(),
                    });
                }
            }
            _ => {}
        }

        if issues.is_empty() {
            VerificationResult {
                passed: true,
                confidence: 1.0,
                issues,
                suggestion: None,
            }
        } else {
            failed_result(issues)
        }
    }
}

/// Backwards-compatible no-op verifier retained for callers that constructed the old
/// JSON validity checker. Runtime harness verification now uses `ToolSchemaVerifier`
/// and `ResultShapeVerifier`.
pub struct SchemaVerifier;

impl SchemaVerifier {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SchemaVerifier {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolCallVerifier for SchemaVerifier {
    fn verify(&self, _call: &ToolCallRecord) -> VerificationResult {
        VerificationResult {
            passed: true,
            confidence: 1.0,
            issues: vec![],
            suggestion: None,
        }
    }
}

fn failed_result(issues: Vec<VerificationIssue>) -> VerificationResult {
    VerificationResult {
        passed: false,
        confidence: 1.0,
        issues,
        suggestion: None,
    }
}

fn json_type_matches(value: &Value, expected_type: &str) -> bool {
    match expected_type {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "string" => value.is_string(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => true,
    }
}

fn looks_like_unresolved_tool_call(answer: &str) -> bool {
    let trimmed = answer.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return value.get("server").is_some()
            && (value.get("tool").is_some() || value.get("arguments").is_some());
    }
    trimmed.contains("MCP_TOOL_CALL_JSON")
        || (trimmed.contains("\"server\"")
            && trimmed.contains("\"tool\"")
            && trimmed.contains("\"arguments\""))
}

fn mentions_tool_failure(answer: &str) -> bool {
    let lower = answer.to_ascii_lowercase();
    lower.contains("tool")
        && (lower.contains("fail") || lower.contains("error") || lower.contains("unavailable"))
}

// =============================================================================
// Semantic Verifier
// =============================================================================

/// Verifier that uses AI to check for semantic issues in tool results
pub struct SemanticVerifier {
    #[allow(dead_code)]
    model: Arc<dyn ChatProvider>,
}

impl SemanticVerifier {
    pub fn new(model: Arc<dyn ChatProvider>) -> Self {
        Self { model }
    }
}

impl ToolCallVerifier for SemanticVerifier {
    fn verify(&self, _call: &ToolCallRecord) -> VerificationResult {
        // For semantic verification, we would send the tool result to the model
        // to check for issues. This is a placeholder implementation that always passes.
        // In production, this would call self.model.complete() with a validation prompt.
        VerificationResult {
            passed: true,
            confidence: 0.7, // Lower confidence since we're not doing real semantic check
            issues: vec![],
            suggestion: None,
        }
    }
}

// =============================================================================
// Composite Verifier
// =============================================================================

/// A verifier that combines multiple verifiers
pub struct CompositeVerifier {
    verifiers: Vec<Arc<dyn ToolCallVerifier>>,
}

impl CompositeVerifier {
    pub fn new() -> Self {
        Self {
            verifiers: Vec::new(),
        }
    }

    pub fn add_verifier(mut self, verifier: Arc<dyn ToolCallVerifier>) -> Self {
        self.verifiers.push(verifier);
        self
    }

    #[allow(clippy::should_implement_trait)]
    pub fn add<V: ToolCallVerifier + 'static>(mut self, verifier: V) -> Self {
        self.verifiers.push(Arc::new(verifier));
        self
    }
}

impl Default for CompositeVerifier {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolCallVerifier for CompositeVerifier {
    fn verify(&self, call: &ToolCallRecord) -> VerificationResult {
        let mut all_issues = Vec::new();
        let mut min_confidence: f64 = 1.0;
        let mut passed = true;

        for verifier in &self.verifiers {
            let result = verifier.verify(call);
            min_confidence = min_confidence.min(result.confidence);
            if !result.issues.is_empty() {
                all_issues.extend(result.issues);
            }
            if !result.passed {
                passed = false;
            }
        }

        VerificationResult {
            passed,
            confidence: min_confidence,
            issues: all_issues,
            suggestion: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timing_verifier_flags_slow_calls() {
        let verifier = TimingVerifier::new(1000, 0.8);
        let slow_call = ToolCallRecord {
            call_index: 0,
            server: "test".to_string(),
            tool: "slow_tool".to_string(),
            arguments: serde_json::json!({}),
            result: serde_json::json!({"ok": true}),
            ok: true,
            duration_ms: 2000,
            iteration: 1,
        };
        let result = verifier.verify(&slow_call);
        assert!(!result.issues.is_empty());
        assert_eq!(result.issues[0].code, "SLOW_TOOL");
    }

    #[test]
    fn test_timing_verifier_warns_near_threshold() {
        let verifier = TimingVerifier::new(1000, 0.8);
        // 900ms is 90% of 1000ms, should warn
        let near_threshold = ToolCallRecord {
            call_index: 0,
            server: "test".to_string(),
            tool: "slow_tool".to_string(),
            arguments: serde_json::json!({}),
            result: serde_json::json!({"ok": true}),
            ok: true,
            duration_ms: 900,
            iteration: 1,
        };
        let result = verifier.verify(&near_threshold);
        assert!(!result.issues.is_empty());
        assert_eq!(result.issues[0].code, "SLOW_TOOL");
    }

    #[test]
    fn test_timing_verifier_passes_fast_calls() {
        let verifier = TimingVerifier::new(1000, 0.8);
        let fast_call = ToolCallRecord {
            call_index: 0,
            server: "test".to_string(),
            tool: "fast_tool".to_string(),
            arguments: serde_json::json!({}),
            result: serde_json::json!({"ok": true}),
            ok: true,
            duration_ms: 100,
            iteration: 1,
        };
        let result = verifier.verify(&fast_call);
        assert!(result.issues.is_empty());
        assert_eq!(result.confidence, 1.0);
    }

    #[test]
    fn test_schema_verifier_accepts_all_json_values() {
        // Note: Since ToolCallRecord.result is a serde_json::Value,
        // it can never represent invalid JSON by construction.
        // This test verifies that the schema verifier accepts valid JSON values.
        let verifier = SchemaVerifier::new();

        // String value
        let string_call = ToolCallRecord {
            call_index: 0,
            server: "test".to_string(),
            tool: "tool".to_string(),
            arguments: serde_json::json!({}),
            result: serde_json::json!("hello world"),
            ok: true,
            duration_ms: 100,
            iteration: 1,
        };
        let result = verifier.verify(&string_call);
        assert!(result.passed);

        // Object value
        let object_call = ToolCallRecord {
            call_index: 0,
            server: "test".to_string(),
            tool: "tool".to_string(),
            arguments: serde_json::json!({}),
            result: serde_json::json!({"key": "value", "num": 42}),
            ok: true,
            duration_ms: 100,
            iteration: 1,
        };
        let result = verifier.verify(&object_call);
        assert!(result.passed);

        // Array value
        let array_call = ToolCallRecord {
            call_index: 0,
            server: "test".to_string(),
            tool: "tool".to_string(),
            arguments: serde_json::json!({}),
            result: serde_json::json!([1, 2, 3]),
            ok: true,
            duration_ms: 100,
            iteration: 1,
        };
        let result = verifier.verify(&array_call);
        assert!(result.passed);

        // Null value
        let null_call = ToolCallRecord {
            call_index: 0,
            server: "test".to_string(),
            tool: "tool".to_string(),
            arguments: serde_json::json!({}),
            result: serde_json::json!(null),
            ok: true,
            duration_ms: 100,
            iteration: 1,
        };
        let result = verifier.verify(&null_call);
        assert!(result.passed);
    }

    #[test]
    fn test_schema_verifier_accepts_valid_json() {
        let verifier = SchemaVerifier::new();
        let valid_call = ToolCallRecord {
            call_index: 0,
            server: "test".to_string(),
            tool: "good_tool".to_string(),
            arguments: serde_json::json!({}),
            result: serde_json::json!({"ok": true, "data": [1, 2, 3]}),
            ok: true,
            duration_ms: 100,
            iteration: 1,
        };
        let result = verifier.verify(&valid_call);
        assert!(result.passed);
        assert!(result.issues.is_empty());
    }

    #[test]
    fn test_composite_verifier() {
        let verifier = CompositeVerifier::new()
            .add(TimingVerifier::new(1000, 0.8))
            .add(SchemaVerifier::new());

        let call = ToolCallRecord {
            call_index: 0,
            server: "test".to_string(),
            tool: "tool".to_string(),
            arguments: serde_json::json!({}),
            result: serde_json::json!({"ok": true}),
            ok: true,
            duration_ms: 2000,
            iteration: 1,
        };

        let result = verifier.verify(&call);
        assert!(!result.issues.is_empty());
        // Should have both timing issue
        assert!(result.issues.iter().any(|i| i.code == "SLOW_TOOL"));
    }

    #[test]
    fn test_composite_verifier_collects_all_issues() {
        // Since SchemaVerifier always passes for valid JSON Values,
        // this test verifies that the composite verifier collects timing issues
        let verifier = CompositeVerifier::new()
            .add(TimingVerifier::new(1000, 0.8))
            .add(SchemaVerifier::new());

        let call = ToolCallRecord {
            call_index: 0,
            server: "test".to_string(),
            tool: "tool".to_string(),
            arguments: serde_json::json!({}),
            result: serde_json::json!({"ok": true}),
            ok: true,
            duration_ms: 2000, // Slow call
            iteration: 1,
        };

        let result = verifier.verify(&call);
        // Both verifiers should be checked
        assert!(!result.issues.is_empty());
        // Timing issue should be detected
        assert!(result.issues.iter().any(|i| i.code == "SLOW_TOOL"));
        // Schema should pass for valid JSON
        assert!(result.issues.iter().all(|i| i.code != "INVALID_JSON"));
    }
}
