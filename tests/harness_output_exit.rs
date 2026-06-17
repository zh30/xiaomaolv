use std::sync::Arc;

use async_trait::async_trait;
use xiaomaolv::config::OutputVerificationMode;
use xiaomaolv::domain::{MessageRole, StoredMessage};
use xiaomaolv::harness::output_exit::{OutputExit, OutputExitRequest};
use xiaomaolv::harness::trajectory::ToolCallRecord;
use xiaomaolv::provider::{ChatProvider, CompletionRequest, StreamSink};

#[derive(Clone)]
struct EchoProvider;

#[async_trait]
impl ChatProvider for EchoProvider {
    async fn complete(&self, req: CompletionRequest) -> anyhow::Result<String> {
        Ok(req
            .messages
            .last()
            .map(|message| message.content.clone())
            .unwrap_or_default())
    }

    async fn complete_stream(
        &self,
        req: CompletionRequest,
        sink: &mut dyn StreamSink,
    ) -> anyhow::Result<String> {
        let text = self.complete(req).await?;
        sink.on_delta(&text).await?;
        Ok(text)
    }
}

#[tokio::test]
async fn output_exit_blocks_hidden_tool_error() {
    let exit = OutputExit::new(
        Arc::new(EchoProvider),
        Some(xiaomaolv::harness::verifier::DeterministicOutputVerifier::new()),
        OutputVerificationMode::Block,
        false,
        6000,
        2000,
    );

    let tool_calls = vec![ToolCallRecord {
        call_index: 0,
        server: "demo".to_string(),
        tool: "search".to_string(),
        arguments: serde_json::json!({}),
        result: serde_json::json!({"error": "network failed"}),
        ok: false,
        duration_ms: 1,
        iteration: 0,
    }];

    let result = exit
        .finalize(OutputExitRequest {
            history: &[StoredMessage {
                role: MessageRole::User,
                content: "answer with search result".to_string(),
            }],
            channel: "http",
            final_answer: "The answer is 42.".to_string(),
            tool_calls: &tool_calls,
            required_format: None,
        })
        .await
        .expect("finalize");

    assert_ne!(result.text, "The answer is 42.");
    assert!(result.verified);
    assert!(result.blocked_or_revised);
    assert_eq!(result.issue_codes, vec!["HIDDEN_TOOL_ERROR".to_string()]);
}
