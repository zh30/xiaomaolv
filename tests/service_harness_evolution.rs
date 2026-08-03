use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use xiaomaolv::domain::{IncomingMessage, StoredMessage};
use xiaomaolv::harness::evolution::{ActiveEvolutionPolicy, EvolutionPolicyRuntime, PromptPatch};
use xiaomaolv::memory::{MemoryBackend, SqliteMemoryBackend, SqliteMemoryStore};
use xiaomaolv::provider::{ChatProvider, CompletionRequest, StreamSink};
use xiaomaolv::service::{AgentMcpSettings, AgentSwarmSettings, MessageService};

#[derive(Default)]
struct CaptureProvider {
    requests: Mutex<Vec<Vec<StoredMessage>>>,
}

impl CaptureProvider {
    fn requests(&self) -> Vec<Vec<StoredMessage>> {
        self.requests.lock().expect("requests mutex").clone()
    }
}

#[async_trait]
impl ChatProvider for CaptureProvider {
    async fn complete(&self, req: CompletionRequest) -> anyhow::Result<String> {
        self.requests
            .lock()
            .expect("requests mutex")
            .push(req.messages);
        Ok("evolved answer".to_string())
    }
}

#[derive(Default)]
struct CaptureSink(String);

#[async_trait]
impl StreamSink for CaptureSink {
    async fn on_delta(&mut self, delta: &str) -> anyhow::Result<()> {
        self.0.push_str(delta);
        Ok(())
    }
}

fn incoming(session_id: &str) -> IncomingMessage {
    IncomingMessage {
        channel: "http".to_string(),
        session_id: session_id.to_string(),
        user_id: "user-a".to_string(),
        text: "Explain the result".to_string(),
        reply_target: None,
    }
}

#[tokio::test]
async fn active_evolution_policy_is_injected_into_normal_and_streaming_runs() {
    let provider = Arc::new(CaptureProvider::default());
    let memory_store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("memory store");
    let memory: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(memory_store));
    let policy_runtime = EvolutionPolicyRuntime::new(Some(ActiveEvolutionPolicy {
        deployment_id: "deployment-a".to_string(),
        candidate_id: "candidate-a".to_string(),
        prompt_patch: PromptPatch::new("ACTIVE_EVOLUTION_POLICY: cite evidence", 1_000)
            .expect("prompt patch"),
    }));
    let service = MessageService::new_with_backend(
        provider.clone(),
        memory,
        None,
        AgentMcpSettings {
            enabled: false,
            ..Default::default()
        },
        20,
        0,
        0,
    )
    .with_agent_swarm(AgentSwarmSettings {
        enabled: false,
        ..Default::default()
    })
    .with_evolution_policy_runtime(policy_runtime);

    service
        .handle(incoming("evolution-normal"))
        .await
        .expect("normal response");
    let mut sink = CaptureSink::default();
    service
        .handle_stream(incoming("evolution-stream"), &mut sink)
        .await
        .expect("streaming response");
    assert_eq!(sink.0, "evolved answer");

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    for messages in requests {
        let policy_messages = messages
            .iter()
            .filter(|message| message.content.contains("ACTIVE_EVOLUTION_POLICY"))
            .collect::<Vec<_>>();
        assert_eq!(policy_messages.len(), 1);
        assert!(policy_messages[0].content.contains("candidate-a"));
        assert!(policy_messages[0].content.contains("cite evidence"));
    }
}
