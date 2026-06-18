use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::RwLock;
use xiaomaolv::code_mode::{
    AgentCodeModeSettings, CodeModePlan, CodeModePlanner, CodeModeToolCall,
};
use xiaomaolv::config::AgentHarnessConfig;
use xiaomaolv::domain::{IncomingMessage, StoredMessage};
use xiaomaolv::harness::store::{HarnessStore, SqliteHarnessStore};
use xiaomaolv::harness::trajectory::{
    ToolCallRecord, TrajectoryExitReason, TrajectoryFilter, TrajectoryLogger, TrajectoryRecord,
};
use xiaomaolv::mcp::{
    BUILTIN_MCP_SERVER_NAME, BUILTIN_MCP_TOOL_CURRENT_TIME, McpRuntime, McpToolInfo,
};
use xiaomaolv::memory::{
    CompactionSummaryLoadRequest, CompactionSummaryRecord, CompactionSummaryUpsertRequest,
    MemoryBackend, MemoryContextRequest, MemoryWriteRequest, SqliteMemoryBackend,
    SqliteMemoryStore,
};
use xiaomaolv::provider::{ChatProvider, CompletionRequest, StreamSink};
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

struct QueueProvider {
    replies: Mutex<VecDeque<String>>,
}

impl QueueProvider {
    fn new(replies: Vec<String>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
        }
    }
}

#[async_trait]
impl ChatProvider for QueueProvider {
    fn model_name(&self) -> Option<&str> {
        Some("queue-model")
    }

    async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
        self.replies
            .lock()
            .expect("replies mutex")
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("missing queued reply"))
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

struct CountingMemoryBackend {
    inner: Arc<SqliteMemoryBackend>,
    finish_calls: Arc<AtomicUsize>,
}

impl CountingMemoryBackend {
    fn new(inner: Arc<SqliteMemoryBackend>, finish_calls: Arc<AtomicUsize>) -> Self {
        Self {
            inner,
            finish_calls,
        }
    }
}

#[async_trait]
impl MemoryBackend for CountingMemoryBackend {
    async fn append(&self, req: MemoryWriteRequest) -> anyhow::Result<()> {
        self.inner.append(req).await
    }

    async fn load_context(&self, req: MemoryContextRequest) -> anyhow::Result<Vec<StoredMessage>> {
        self.inner.load_context(req).await
    }

    async fn insert_trajectory_tool_call(
        &self,
        trajectory_id: &str,
        record: ToolCallRecord,
    ) -> anyhow::Result<()> {
        self.inner
            .insert_trajectory_tool_call(trajectory_id, record)
            .await
    }

    async fn start_trajectory(
        &self,
        trajectory_id: &str,
        session_id: &str,
        channel: &str,
        user_id: &str,
        model: &str,
    ) -> anyhow::Result<()> {
        self.inner
            .start_trajectory(trajectory_id, session_id, channel, user_id, model)
            .await
    }

    async fn finish_trajectory(
        &self,
        trajectory_id: &str,
        final_answer: Option<String>,
        exit_reason: TrajectoryExitReason,
    ) -> anyhow::Result<()> {
        self.finish_calls.fetch_add(1, Ordering::SeqCst);
        self.inner
            .finish_trajectory(trajectory_id, final_answer, exit_reason)
            .await
    }

    async fn get_trajectory(
        &self,
        trajectory_id: &str,
    ) -> anyhow::Result<Option<TrajectoryRecord>> {
        self.inner.get_trajectory(trajectory_id).await
    }

    async fn query_trajectories(
        &self,
        filter: TrajectoryFilter,
    ) -> anyhow::Result<Vec<TrajectoryRecord>> {
        self.inner.query_trajectories(filter).await
    }
}

#[async_trait]
impl HarnessStore for CountingMemoryBackend {
    async fn start_trajectory(
        &self,
        trajectory_id: &str,
        session_id: &str,
        channel: &str,
        user_id: &str,
        model: &str,
    ) -> anyhow::Result<()> {
        self.inner
            .start_trajectory(trajectory_id, session_id, channel, user_id, model)
            .await
    }

    async fn insert_trajectory_tool_call(
        &self,
        trajectory_id: &str,
        record: ToolCallRecord,
    ) -> anyhow::Result<()> {
        self.inner
            .insert_trajectory_tool_call(trajectory_id, record)
            .await
    }

    async fn finish_trajectory(
        &self,
        trajectory_id: &str,
        final_answer: Option<String>,
        exit_reason: TrajectoryExitReason,
    ) -> anyhow::Result<()> {
        self.finish_calls.fetch_add(1, Ordering::SeqCst);
        self.inner
            .finish_trajectory(trajectory_id, final_answer, exit_reason)
            .await
    }

    async fn get_trajectory(
        &self,
        trajectory_id: &str,
    ) -> anyhow::Result<Option<TrajectoryRecord>> {
        self.inner.get_trajectory(trajectory_id).await
    }

    async fn query_trajectories(
        &self,
        filter: TrajectoryFilter,
    ) -> anyhow::Result<Vec<TrajectoryRecord>> {
        self.inner.query_trajectories(filter).await
    }

    async fn load_compaction_summary(
        &self,
        req: CompactionSummaryLoadRequest,
    ) -> anyhow::Result<Option<CompactionSummaryRecord>> {
        self.inner.load_compaction_summary(req).await
    }

    async fn upsert_compaction_summary(
        &self,
        req: CompactionSummaryUpsertRequest,
    ) -> anyhow::Result<()> {
        self.inner.upsert_compaction_summary(req).await
    }
}

async fn service_with_provider(
    provider: Arc<dyn ChatProvider>,
    agent_mcp: AgentMcpSettings,
) -> anyhow::Result<MessageService> {
    let store = SqliteMemoryStore::new("sqlite::memory:").await?;
    let backend: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(store.clone()));
    let harness_store = Arc::new(SqliteHarnessStore::new(store));
    let runtime = Arc::new(RwLock::new(McpRuntime::new(HashMap::new())));
    let logger = TrajectoryLogger::new(harness_store.clone(), true);

    Ok(
        MessageService::new_with_backend(provider, backend, Some(runtime), agent_mcp, 20, 0, 0)
            .with_harness_store(harness_store)
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

struct FailingSink;

#[async_trait]
impl StreamSink for FailingSink {
    async fn on_delta(&mut self, _delta: &str) -> anyhow::Result<()> {
        anyhow::bail!("sink delivery failed")
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

#[tokio::test]
async fn code_mode_stream_sink_failure_finishes_trajectory_as_internal_error() {
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
    let mut sink = FailingSink;

    let err = service
        .handle_stream(incoming("code-mode-stream-sink-failure"), &mut sink)
        .await
        .expect_err("sink should fail");
    assert!(format!("{err:?}").contains("sink delivery failed"));

    let trajectories = service
        .query_trajectories(TrajectoryFilter {
            session_id: Some("code-mode-stream-sink-failure".to_string()),
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
        TrajectoryExitReason::InternalError
    ));
    assert_eq!(trajectories[0].final_answer, None);
    assert_eq!(trajectories[0].tool_calls.len(), 1);
}

#[tokio::test]
async fn mcp_loop_agent_run_finishes_once_on_parse_error_recovery() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let sqlite_backend = Arc::new(SqliteMemoryBackend::new(store.clone()));
    let finish_calls = Arc::new(AtomicUsize::new(0));
    let backend = Arc::new(CountingMemoryBackend::new(
        sqlite_backend,
        finish_calls.clone(),
    ));
    let provider = Arc::new(QueueProvider::new(vec![
        "MCP_TOOL_CALL_JSON: {bad".to_string(),
        "I cannot use that malformed tool call safely.".to_string(),
    ]));
    let runtime = Arc::new(RwLock::new(McpRuntime::default()));
    let service = MessageService::new_with_backend(
        provider,
        backend.clone(),
        Some(runtime),
        AgentMcpSettings {
            enabled: true,
            max_iterations: 2,
            max_tool_result_chars: 4000,
        },
        8,
        0,
        0,
    )
    .with_harness_config(&AgentHarnessConfig {
        enable_trajectory: true,
        ..AgentHarnessConfig::default()
    })
    .with_trajectory_logger(TrajectoryLogger::new(backend.clone(), true));

    let response = service
        .handle(IncomingMessage {
            channel: "http".to_string(),
            session_id: "session-parse".to_string(),
            user_id: "user-parse".to_string(),
            text: "use a tool".to_string(),
            reply_target: None,
        })
        .await
        .expect("parse recovery should still return a final response");
    assert_eq!(
        response.text,
        "I cannot use that malformed tool call safely."
    );

    let records = service
        .query_trajectories(TrajectoryFilter {
            session_id: Some("session-parse".to_string()),
            channel: Some("http".to_string()),
            user_id: Some("user-parse".to_string()),
            exit_reason: None,
            has_tool_errors: None,
            limit: 10,
        })
        .await
        .expect("query trajectories");
    assert_eq!(records.len(), 1);
    assert!(records[0].finished_at.is_some());
    assert_eq!(finish_calls.load(Ordering::SeqCst), 1);
}
