use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;
use xiaomaolv::code_mode::{
    AgentCodeModeSettings, CodeModePlan, CodeModePlanner, CodeModeToolCall,
};
use xiaomaolv::domain::{IncomingMessage, StoredMessage};
use xiaomaolv::harness::trajectory::{TrajectoryExitReason, TrajectoryFilter, TrajectoryLogger};
use xiaomaolv::mcp::{
    BUILTIN_MCP_SERVER_NAME, BUILTIN_MCP_TOOL_CURRENT_TIME, McpRuntime, McpToolInfo,
};
use xiaomaolv::memory::{MemoryBackend, SqliteMemoryBackend, SqliteMemoryStore};
use xiaomaolv::provider::{ChatProvider, CompletionRequest};
use xiaomaolv::service::{AgentMcpSettings, AgentSwarmSettings, MessageService};

struct FailingProvider;

#[async_trait]
impl ChatProvider for FailingProvider {
    fn model_name(&self) -> Option<&str> {
        Some("failure-model")
    }

    async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
        Err(anyhow::anyhow!("provider down"))
    }
}

struct AnswerProvider;

#[async_trait]
impl ChatProvider for AnswerProvider {
    fn model_name(&self) -> Option<&str> {
        Some("code-model")
    }

    async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
        Ok("code-mode-answer".to_string())
    }
}

struct StaticPlanner;

#[async_trait]
impl CodeModePlanner for StaticPlanner {
    fn name(&self) -> &'static str {
        "test-static"
    }

    async fn build_plan(
        &self,
        _history: &[StoredMessage],
        _tools: &[McpToolInfo],
    ) -> anyhow::Result<Option<CodeModePlan>> {
        Ok(Some(CodeModePlan {
            calls: vec![CodeModeToolCall {
                server: BUILTIN_MCP_SERVER_NAME.to_string(),
                tool: BUILTIN_MCP_TOOL_CURRENT_TIME.to_string(),
                arguments: serde_json::json!({}),
            }],
        }))
    }
}

async fn service_with_provider(
    provider: Arc<dyn ChatProvider>,
    agent_mcp: AgentMcpSettings,
) -> anyhow::Result<MessageService> {
    let store = SqliteMemoryStore::new("sqlite::memory:").await?;
    let backend: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(store));
    let runtime = Arc::new(RwLock::new(McpRuntime::new(HashMap::new())));
    let logger = TrajectoryLogger::new(backend.clone(), true);

    Ok(
        MessageService::new_with_backend(provider, backend, Some(runtime), agent_mcp, 20, 0, 0)
            .with_trajectory_logger(logger)
            .with_agent_swarm(AgentSwarmSettings {
                enabled: false,
                ..Default::default()
            }),
    )
}

fn incoming(session_id: &str) -> IncomingMessage {
    IncomingMessage {
        channel: "test".to_string(),
        session_id: session_id.to_string(),
        user_id: "user-1".to_string(),
        text: "please use the agent harness".to_string(),
        reply_target: None,
    }
}

#[tokio::test]
async fn mcp_provider_error_finishes_trajectory_as_internal_error() {
    let service = service_with_provider(
        Arc::new(FailingProvider),
        AgentMcpSettings {
            enabled: true,
            max_iterations: 1,
            max_tool_result_chars: 4000,
        },
    )
    .await
    .expect("build service");

    let err = service
        .handle(incoming("mcp-provider-error"))
        .await
        .expect_err("provider should fail");
    assert!(
        err.to_string().contains("provider completion failed"),
        "unexpected error: {err:?}"
    );

    let trajectories = service
        .query_trajectories(TrajectoryFilter {
            session_id: Some("mcp-provider-error".to_string()),
            channel: Some("test".to_string()),
            user_id: Some("user-1".to_string()),
            exit_reason: None,
            has_tool_errors: None,
            limit: 10,
        })
        .await
        .expect("query trajectories");

    assert_eq!(trajectories.len(), 1);
    let trajectory = &trajectories[0];
    assert!(trajectory.finished_at.is_some());
    assert!(matches!(
        trajectory.exit_reason,
        TrajectoryExitReason::InternalError
    ));
    assert_eq!(trajectory.model, "failure-model");
    assert_eq!(trajectory.final_answer, None);
    assert_eq!(trajectory.total_tokens, None);
    assert!(trajectory.tool_calls.is_empty());
}

#[tokio::test]
async fn code_mode_direct_success_records_finished_trajectory_and_tool_call() {
    let service = service_with_provider(
        Arc::new(AnswerProvider),
        AgentMcpSettings {
            enabled: true,
            max_iterations: 1,
            max_tool_result_chars: 4000,
        },
    )
    .await
    .expect("build service")
    .with_agent_code_mode(AgentCodeModeSettings {
        enabled: true,
        shadow_mode: false,
        ..Default::default()
    })
    .with_code_mode_planner(Arc::new(StaticPlanner));

    let out = service
        .handle(incoming("code-mode-success"))
        .await
        .expect("handle");
    assert_eq!(out.text, "code-mode-answer");

    let trajectories = service
        .query_trajectories(TrajectoryFilter {
            session_id: Some("code-mode-success".to_string()),
            channel: Some("test".to_string()),
            user_id: Some("user-1".to_string()),
            exit_reason: None,
            has_tool_errors: None,
            limit: 10,
        })
        .await
        .expect("query trajectories");

    assert_eq!(trajectories.len(), 1);
    let trajectory = &trajectories[0];
    assert!(trajectory.finished_at.is_some());
    assert!(matches!(
        trajectory.exit_reason,
        TrajectoryExitReason::FinalAnswer
    ));
    assert_eq!(trajectory.model, "code-model");
    assert_eq!(trajectory.final_answer.as_deref(), Some("code-mode-answer"));
    assert_eq!(trajectory.total_tokens, None);
    assert_eq!(trajectory.tool_calls.len(), 1);

    let call = &trajectory.tool_calls[0];
    assert_eq!(call.server, BUILTIN_MCP_SERVER_NAME);
    assert_eq!(call.tool, BUILTIN_MCP_TOOL_CURRENT_TIME);
    assert_eq!(call.arguments, serde_json::json!({}));
    assert!(call.result.is_object());
    assert!(call.ok);
    assert_eq!(call.duration_ms, 0);
    assert_eq!(call.iteration, 0);
}
