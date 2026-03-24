use std::sync::Arc;

use crate::harness::trajectory::ToolCallRecord;
use crate::provider::ChatProvider;

/// Verification result with confidence score and issues
#[derive(Debug, Clone)]
pub struct VerificationResult {
    pub passed: bool,
    pub confidence: f64,
    pub issues: Vec<VerificationIssue>,
    pub suggestion: Option<String>,
}

/// A single issue found during verification
#[derive(Debug, Clone)]
pub struct VerificationIssue {
    pub severity: IssueSeverity,
    pub code: String,
    pub message: String,
}

/// Severity level for issues
#[derive(Debug, Clone, PartialEq, Eq)]
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
// Schema Verifier
// =============================================================================

/// Verifier that checks if tool result is valid JSON
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
    fn verify(&self, call: &ToolCallRecord) -> VerificationResult {
        // Check if result is valid JSON
        if serde_json::from_str::<serde_json::Value>(&call.result.to_string()).is_err() {
            return VerificationResult {
                passed: false,
                confidence: 1.0,
                issues: vec![VerificationIssue {
                    severity: IssueSeverity::Error,
                    code: "INVALID_JSON".to_string(),
                    message: "Tool result is not valid JSON".to_string(),
                }],
                suggestion: None,
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
