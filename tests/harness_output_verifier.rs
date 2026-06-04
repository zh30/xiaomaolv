use xiaomaolv::domain::{MessageRole, StoredMessage};
use xiaomaolv::harness::trajectory::ToolCallRecord;
use xiaomaolv::harness::verifier::{DeterministicOutputVerifier, OutputVerificationRequest};

fn request(final_answer: &str, tool_calls: Vec<ToolCallRecord>) -> OutputVerificationRequest {
    OutputVerificationRequest {
        final_answer: final_answer.to_string(),
        recent_history: vec![StoredMessage {
            role: MessageRole::User,
            content: "question".to_string(),
        }],
        tool_calls,
        channel: "test".to_string(),
        required_format: None,
    }
}

#[test]
fn output_verifier_detects_unresolved_tool_call_json() {
    let verifier = DeterministicOutputVerifier::new();
    let result = verifier.verify(&request(
        r#"{"server":"s","tool":"t","arguments":{}}"#,
        Vec::new(),
    ));
    assert!(!result.passed);
    assert!(
        result
            .issues
            .iter()
            .any(|issue| issue.code == "UNRESOLVED_TOOL_CALL")
    );
}

#[test]
fn output_verifier_detects_hidden_tool_error() {
    let verifier = DeterministicOutputVerifier::new();
    let tool_call = ToolCallRecord {
        call_index: 0,
        server: "s".to_string(),
        tool: "t".to_string(),
        arguments: serde_json::json!({}),
        result: serde_json::json!({"error": "failed"}),
        ok: false,
        duration_ms: 10,
        iteration: 0,
    };
    let result = verifier.verify(&request("Everything is fine.", vec![tool_call]));
    assert!(!result.passed);
    assert!(
        result
            .issues
            .iter()
            .any(|issue| issue.code == "HIDDEN_TOOL_ERROR")
    );
}

#[test]
fn output_verifier_passes_when_tool_error_is_disclosed() {
    let verifier = DeterministicOutputVerifier::new();
    let tool_call = ToolCallRecord {
        call_index: 0,
        server: "s".to_string(),
        tool: "t".to_string(),
        arguments: serde_json::json!({}),
        result: serde_json::json!({"error": "failed"}),
        ok: false,
        duration_ms: 10,
        iteration: 0,
    };
    let result = verifier.verify(&request(
        "The tool failed, so I cannot verify the answer.",
        vec![tool_call],
    ));
    assert!(result.passed);
}
