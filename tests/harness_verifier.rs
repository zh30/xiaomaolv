use xiaomaolv::harness::trajectory::ToolCallRecord;
use xiaomaolv::harness::verifier::{
    CompositeVerifier, IssueSeverity, SchemaVerifier, TimingVerifier, ToolCallVerifier,
};

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
    assert_eq!(result.issues[0].severity, IssueSeverity::Warning);
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
    assert!(result.passed);
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
fn test_composite_verifier_aggregates_issues() {
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

#[test]
fn test_verification_result_has_correct_confidence() {
    let verifier = TimingVerifier::new(1000, 0.8);

    // Fast call - high confidence
    let fast_call = ToolCallRecord {
        server: "test".to_string(),
        tool: "fast".to_string(),
        arguments: serde_json::json!({}),
        result: serde_json::json!({"ok": true}),
        ok: true,
        duration_ms: 100,
        iteration: 1,
    };
    let result = verifier.verify(&fast_call);
    assert_eq!(result.confidence, 1.0);

    // Slow call - lower confidence
    let slow_call = ToolCallRecord {
        server: "test".to_string(),
        tool: "slow".to_string(),
        arguments: serde_json::json!({}),
        result: serde_json::json!({"ok": true}),
        ok: true,
        duration_ms: 2000,
        iteration: 1,
    };
    let result = verifier.verify(&slow_call);
    assert_eq!(result.confidence, 0.8);
}

#[test]
fn test_verification_issue_contains_details() {
    let verifier = TimingVerifier::new(1000, 0.8);
    let slow_call = ToolCallRecord {
        server: "my-server".to_string(),
        tool: "my-tool".to_string(),
        arguments: serde_json::json!({"param": "value"}),
        result: serde_json::json!({"ok": true}),
        ok: true,
        duration_ms: 2000,
        iteration: 5,
    };
    let result = verifier.verify(&slow_call);
    assert!(!result.issues.is_empty());

    let issue = &result.issues[0];
    assert!(issue.message.contains("2000ms"));
    assert!(issue.message.contains("1000ms"));
    assert!(result.suggestion.is_some());
}

#[test]
fn test_timing_verifier_with_zero_warn_ratio() {
    let verifier = TimingVerifier::new(1000, 0.0);
    let call = ToolCallRecord {
        server: "test".to_string(),
        tool: "tool".to_string(),
        arguments: serde_json::json!({}),
        result: serde_json::json!({"ok": true}),
        ok: true,
        duration_ms: 1,
        iteration: 1,
    };
    // With warn_ratio of 0, even very fast calls might trigger warning if duration > 0
    let result = verifier.verify(&call);
    // The warn threshold is 0, so any duration > 0 will exceed it
    assert!(!result.issues.is_empty());
}

#[test]
fn test_severity_levels() {
    assert_eq!(IssueSeverity::Warning.as_str(), "warning");
    assert_eq!(IssueSeverity::Error.as_str(), "error");
    assert_eq!(IssueSeverity::Critical.as_str(), "critical");
}
