use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::RwLock;
use xiaomaolv::config::{AgentHarnessConfig, ToolVerificationMode};
use xiaomaolv::domain::IncomingMessage;
use xiaomaolv::harness::store::SqliteHarnessStore;
use xiaomaolv::harness::trajectory::{TrajectoryExitReason, TrajectoryFilter};
use xiaomaolv::mcp::{BUILTIN_MCP_SERVER_NAME, BUILTIN_MCP_TOOL_CURRENT_TIME, McpRuntime};
use xiaomaolv::memory::{MemoryBackend, SqliteMemoryBackend, SqliteMemoryStore};
use xiaomaolv::provider::{ChatProvider, CompletionRequest};
use xiaomaolv::service::{AgentMcpSettings, AgentSwarmSettings, MessageService};

struct VerificationProvider {
    final_answer: &'static str,
    requests: Mutex<Vec<String>>,
}

impl VerificationProvider {
    fn new(final_answer: &'static str) -> Self {
        Self {
            final_answer,
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("requests mutex").clone()
    }
}

#[async_trait]
impl ChatProvider for VerificationProvider {
    fn model_name(&self) -> Option<&str> {
        Some("verification-test-model")
    }

    async fn complete(&self, req: CompletionRequest) -> anyhow::Result<String> {
        let request_text = req
            .messages
            .iter()
            .map(|msg| format!("{}:{}", msg.role.as_str(), msg.content))
            .collect::<Vec<_>>()
            .join("\n");
        let call_index = {
            let mut guard = self.requests.lock().expect("requests mutex");
            let call_index = guard.len();
            guard.push(request_text);
            call_index
        };

        if call_index == 0 {
            return Ok(serde_json::json!({
                "server": BUILTIN_MCP_SERVER_NAME,
                "tool": BUILTIN_MCP_TOOL_CURRENT_TIME,
                "arguments": {}
            })
            .to_string());
        }

        Ok(self.final_answer.to_string())
    }
}

async fn service_with_verification(
    provider: Arc<VerificationProvider>,
    mode: ToolVerificationMode,
) -> anyhow::Result<MessageService> {
    let store = SqliteMemoryStore::new("sqlite::memory:").await?;
    let backend: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(store.clone()));
    let harness_store = Arc::new(SqliteHarnessStore::new(store));
    let runtime = Arc::new(RwLock::new(McpRuntime::new(HashMap::new())));

    Ok(MessageService::new_with_backend(
        provider,
        backend,
        Some(runtime),
        AgentMcpSettings {
            enabled: true,
            max_iterations: 3,
            max_tool_result_chars: 1,
        },
        20,
        0,
        0,
    )
    .with_agent_swarm(AgentSwarmSettings {
        enabled: false,
        ..Default::default()
    })
    .with_harness_store(harness_store)
    .with_harness_config(&AgentHarnessConfig {
        enable_trajectory: true,
        enable_verification: true,
        verification_mode: mode,
        ..Default::default()
    }))
}

fn incoming(session_id: &str) -> IncomingMessage {
    IncomingMessage {
        channel: "test".to_string(),
        session_id: session_id.to_string(),
        user_id: "user-1".to_string(),
        text: "please use a tool".to_string(),
        reply_target: None,
    }
}

#[tokio::test]
async fn observe_mode_keeps_feeding_failed_tool_result() {
    let provider = Arc::new(VerificationProvider::new("observe final"));
    let service = service_with_verification(provider.clone(), ToolVerificationMode::Observe)
        .await
        .expect("service");

    let out = service
        .handle(incoming("verify-observe"))
        .await
        .expect("handle");
    assert_eq!(out.text, "observe final");

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("MCP_TOOL_RESULT_JSON"));
    assert!(!requests[1].contains("MCP_TOOL_VERIFICATION_FAILED_JSON"));
}

#[tokio::test]
async fn retry_mode_gives_model_one_retry_before_feeding_failed_result() {
    let provider = Arc::new(VerificationProvider::new("retry final"));
    let service = service_with_verification(provider.clone(), ToolVerificationMode::Retry)
        .await
        .expect("service");

    let out = service
        .handle(incoming("verify-retry"))
        .await
        .expect("handle");
    assert_eq!(out.text, "retry final");

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("MCP_TOOL_VERIFICATION_FAILED_JSON"));
    assert!(!requests[1].contains("MCP_TOOL_RESULT_JSON"));
}

#[tokio::test]
async fn block_mode_finishes_trajectory_as_tool_error_with_visible_issues() {
    let provider = Arc::new(VerificationProvider::new("block final"));
    let service = service_with_verification(provider.clone(), ToolVerificationMode::Block)
        .await
        .expect("service");

    let out = service
        .handle(incoming("verify-block"))
        .await
        .expect("handle");
    assert_eq!(out.text, "block final");

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("MCP_TOOL_VERIFICATION_FAILED_JSON"));

    let trajectories = service
        .query_trajectories(TrajectoryFilter {
            session_id: Some("verify-block".to_string()),
            channel: Some("test".to_string()),
            user_id: Some("user-1".to_string()),
            exit_reason: None,
            has_tool_errors: None,
            limit: 10,
        })
        .await
        .expect("query trajectories");
    assert_eq!(trajectories.len(), 1);
    assert!(matches!(
        trajectories[0].exit_reason,
        TrajectoryExitReason::ToolError
    ));
    assert_eq!(trajectories[0].tool_calls.len(), 1);
    assert!(
        trajectories[0].tool_calls[0]
            .result
            .get("verification_failed")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
    );
    assert!(
        trajectories[0].tool_calls[0]
            .result
            .to_string()
            .contains("TRUNCATED_RESULT")
    );
}
