use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use prometheus::Registry;
use tokio::sync::RwLock;
use xiaomaolv::config::{AgentHarnessConfig, OutputVerificationMode};
use xiaomaolv::domain::IncomingMessage;
use xiaomaolv::harness::observability::TrajectoryMetrics;
use xiaomaolv::harness::store::SqliteHarnessStore;
use xiaomaolv::harness::trajectory::{TrajectoryExitReason, TrajectoryFilter};
use xiaomaolv::mcp::McpRuntime;
use xiaomaolv::memory::{MemoryBackend, SqliteMemoryBackend, SqliteMemoryStore};
use xiaomaolv::provider::{ChatProvider, CompletionRequest, StreamSink};
use xiaomaolv::service::{AgentMcpSettings, AgentSwarmSettings, MessageService};

struct SequenceProvider {
    replies: Vec<&'static str>,
    requests: Mutex<Vec<String>>,
}

impl SequenceProvider {
    fn new(replies: Vec<&'static str>) -> Self {
        Self {
            replies,
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("requests mutex").clone()
    }
}

#[async_trait]
impl ChatProvider for SequenceProvider {
    async fn complete(&self, req: CompletionRequest) -> anyhow::Result<String> {
        let request = req
            .messages
            .iter()
            .map(|msg| msg.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let idx = {
            let mut guard = self.requests.lock().expect("requests mutex");
            let idx = guard.len();
            guard.push(request);
            idx
        };
        Ok(self
            .replies
            .get(idx)
            .copied()
            .unwrap_or("fallback final")
            .to_string())
    }
}

async fn service(provider: Arc<SequenceProvider>) -> anyhow::Result<MessageService> {
    service_with_harness(provider, AgentHarnessConfig::default()).await
}

async fn service_with_harness(
    provider: Arc<SequenceProvider>,
    harness: AgentHarnessConfig,
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
            max_tool_result_chars: 4000,
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
    .with_harness_config(&harness))
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

#[derive(Default)]
struct CollectSink {
    chunks: Vec<String>,
}

impl CollectSink {
    fn text(&self) -> String {
        self.chunks.concat()
    }
}

#[async_trait]
impl StreamSink for CollectSink {
    async fn on_delta(&mut self, delta: &str) -> anyhow::Result<()> {
        self.chunks.push(delta.to_string());
        Ok(())
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
async fn mcp_loop_recovers_from_malformed_tool_call_json_once() {
    let provider = Arc::new(SequenceProvider::new(vec![
        r#"{"server":"xiaomaolv_builtin","tool":"get_current_time","arguments":"#,
        "recovered final",
    ]));
    let service = service(provider.clone()).await.expect("service");

    let out = service
        .handle(incoming("mcp-malformed"))
        .await
        .expect("handle");
    assert_eq!(out.text, "recovered final");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("MCP_TOOL_VERIFICATION_FAILED_JSON"));
    assert!(requests[1].contains("MALFORMED_TOOL_CALL_JSON"));
}

#[tokio::test]
async fn mcp_loop_rejects_unknown_tool_before_runtime_call() {
    let provider = Arc::new(SequenceProvider::new(vec![
        r#"{"server":"missing","tool":"nope","arguments":{}}"#,
        "unknown-tool final",
    ]));
    let service = service(provider.clone()).await.expect("service");

    let out = service
        .handle(incoming("mcp-unknown"))
        .await
        .expect("handle");
    assert_eq!(out.text, "unknown-tool final");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("UNKNOWN_TOOL"));
}

#[tokio::test]
async fn unknown_tool_metrics_use_bounded_labels_and_preserve_request() {
    let provider = Arc::new(SequenceProvider::new(vec![
        r#"{"server":"missing-server","tool":"made_up_tool","arguments":{}}"#,
        "bounded metrics final",
    ]));
    let registry = Registry::new();
    let metrics = TrajectoryMetrics::new(&registry);
    let service = service_with_harness(
        provider,
        AgentHarnessConfig {
            enable_trajectory: true,
            ..Default::default()
        },
    )
    .await
    .expect("service")
    .with_trajectory_metrics(metrics.clone());

    let out = service
        .handle(incoming("mcp-unknown-metrics"))
        .await
        .expect("handle");
    assert_eq!(out.text, "bounded metrics final");

    let prometheus = metrics.render_prometheus();
    assert!(
        prometheus.contains(
            r#"xiaomaolv_tool_calls_total{ok="false",server="unknown",tool="invalid"} 1"#
        ),
        "{prometheus}"
    );
    assert!(
        !prometheus.contains(r#"server="missing-server""#),
        "{prometheus}"
    );
    assert!(
        !prometheus.contains(r#"tool="made_up_tool""#),
        "{prometheus}"
    );

    let trajectories = service
        .query_trajectories(TrajectoryFilter {
            session_id: Some("mcp-unknown-metrics".to_string()),
            channel: Some("test".to_string()),
            user_id: Some("user-1".to_string()),
            exit_reason: None,
            has_tool_errors: None,
            limit: 10,
        })
        .await
        .expect("query trajectories");
    assert_eq!(trajectories.len(), 1);
    let call = &trajectories[0].tool_calls[0];
    assert_eq!(call.server, "unknown");
    assert_eq!(call.tool, "invalid");
    assert_eq!(
        call.result.get("requested_server").and_then(|v| v.as_str()),
        Some("missing-server")
    );
    assert_eq!(
        call.result.get("requested_tool").and_then(|v| v.as_str()),
        Some("made_up_tool")
    );
}

#[tokio::test]
async fn mcp_loop_rejects_schema_invalid_arguments_before_runtime_call() {
    let provider = Arc::new(SequenceProvider::new(vec![
        r#"{"server":"xiaomaolv_builtin","tool":"get_current_time","arguments":{"timezone":123}}"#,
        "schema final",
    ]));
    let service = service(provider.clone()).await.expect("service");

    let out = service
        .handle(incoming("mcp-schema"))
        .await
        .expect("handle");
    assert_eq!(out.text, "schema final");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("ARGUMENT_TYPE_MISMATCH"));
}

#[tokio::test]
async fn output_verification_uses_tool_errors_and_updates_trajectory_final_answer() {
    let provider = Arc::new(SequenceProvider::new(vec![
        r#"{"server":"missing","tool":"nope","arguments":{}}"#,
        r#"{"server":"missing","tool":"nope","arguments":{}}"#,
        "confident final answer",
    ]));
    let service = service_with_harness(
        provider,
        AgentHarnessConfig {
            enable_trajectory: true,
            output_verification_mode: OutputVerificationMode::Block,
            ..Default::default()
        },
    )
    .await
    .expect("service");

    let out = service
        .handle(incoming("mcp-output-hidden-tool-error"))
        .await
        .expect("handle");
    assert!(
        out.text
            .contains("could not produce a reliable final answer")
    );

    let trajectories = service
        .query_trajectories(TrajectoryFilter {
            session_id: Some("mcp-output-hidden-tool-error".to_string()),
            channel: Some("test".to_string()),
            user_id: Some("user-1".to_string()),
            exit_reason: None,
            has_tool_errors: None,
            limit: 10,
        })
        .await
        .expect("query trajectories");
    assert_eq!(trajectories.len(), 1);
    assert_eq!(
        trajectories[0].final_answer.as_deref(),
        Some(out.text.as_str())
    );
}

#[tokio::test]
async fn streaming_mcp_loop_recovers_from_malformed_tool_call_json_once() {
    let provider = Arc::new(SequenceProvider::new(vec![
        r#"{"server":"xiaomaolv_builtin","tool":"get_current_time","arguments":"#,
        "stream recovered final",
    ]));
    let service = service(provider.clone()).await.expect("service");
    let mut sink = CollectSink::default();

    let out = service
        .handle_stream(incoming("mcp-stream-malformed"), &mut sink)
        .await
        .expect("stream handle");

    assert_eq!(out.text, "stream recovered final");
    assert_eq!(sink.text(), "stream recovered final");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("MALFORMED_TOOL_CALL_JSON"));
    assert!(!sink.text().contains("\"tool\""));
}

#[tokio::test]
async fn streaming_mcp_loop_rejects_unknown_tool_before_runtime_call() {
    let provider = Arc::new(SequenceProvider::new(vec![
        r#"{"server":"missing","tool":"nope","arguments":{}}"#,
        "stream unknown-tool final",
    ]));
    let service = service(provider.clone()).await.expect("service");
    let mut sink = CollectSink::default();

    let out = service
        .handle_stream(incoming("mcp-stream-unknown"), &mut sink)
        .await
        .expect("stream handle");

    assert_eq!(out.text, "stream unknown-tool final");
    assert_eq!(sink.text(), "stream unknown-tool final");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("UNKNOWN_TOOL"));
}

#[tokio::test]
async fn streaming_mcp_loop_sink_failure_finishes_trajectory_as_internal_error() {
    let provider = Arc::new(SequenceProvider::new(vec!["stream final before sink"]));
    let service = service_with_harness(
        provider,
        AgentHarnessConfig {
            enable_trajectory: true,
            ..Default::default()
        },
    )
    .await
    .expect("service");
    let mut sink = FailingSink;

    let err = service
        .handle_stream(incoming("mcp-stream-sink-failure"), &mut sink)
        .await
        .expect_err("sink should fail");
    assert!(format!("{err:?}").contains("sink delivery failed"));

    let trajectories = service
        .query_trajectories(TrajectoryFilter {
            session_id: Some("mcp-stream-sink-failure".to_string()),
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
}
