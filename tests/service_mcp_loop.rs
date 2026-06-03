use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::RwLock;
use xiaomaolv::domain::IncomingMessage;
use xiaomaolv::mcp::McpRuntime;
use xiaomaolv::memory::{MemoryBackend, SqliteMemoryBackend, SqliteMemoryStore};
use xiaomaolv::provider::{ChatProvider, CompletionRequest};
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
    let store = SqliteMemoryStore::new("sqlite::memory:").await?;
    let backend: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(store));
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
