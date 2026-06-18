use std::sync::Arc;

use xiaomaolv::config::{AgentHarnessConfig, OutputVerificationMode};
use xiaomaolv::domain::{IncomingMessage, MessageRole};
use xiaomaolv::memory::SqliteMemoryStore;
use xiaomaolv::provider::{ChatProvider, CompletionRequest, StreamSink};
use xiaomaolv::service::{AgentSwarmSettings, MessageService};

struct FakeStreamingProvider;

#[async_trait::async_trait]
impl ChatProvider for FakeStreamingProvider {
    async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
        Ok("fallback".to_string())
    }

    async fn complete_stream(
        &self,
        _req: CompletionRequest,
        sink: &mut dyn StreamSink,
    ) -> anyhow::Result<String> {
        sink.on_delta("hello ").await?;
        sink.on_delta("world").await?;
        Ok("hello world".to_string())
    }
}

#[derive(Default)]
struct CollectSink {
    chunks: Vec<String>,
}

#[async_trait::async_trait]
impl StreamSink for CollectSink {
    async fn on_delta(&mut self, delta: &str) -> anyhow::Result<()> {
        self.chunks.push(delta.to_string());
        Ok(())
    }
}

struct SwarmToolLeakProvider;

#[async_trait::async_trait]
impl ChatProvider for SwarmToolLeakProvider {
    async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
        Ok(r#"{"server":"demo","tool":"search","arguments":{}}"#.to_string())
    }
}

struct HiddenToolCallStreamingProvider;

#[async_trait::async_trait]
impl ChatProvider for HiddenToolCallStreamingProvider {
    async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
        Ok(r#"{"server":"demo","tool":"search","arguments":{}}"#.to_string())
    }

    async fn complete_stream(
        &self,
        _req: CompletionRequest,
        sink: &mut dyn StreamSink,
    ) -> anyhow::Result<String> {
        let text = r#"{"server":"demo","tool":"search","arguments":{}}"#;
        sink.on_delta(text).await?;
        Ok(text.to_string())
    }
}

#[tokio::test]
async fn service_streams_reply_and_persists_final_message() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let service = MessageService::new(Arc::new(FakeStreamingProvider), store.clone(), 20);
    let mut sink = CollectSink::default();

    let out = service
        .handle_stream(
            IncomingMessage {
                channel: "telegram".to_string(),
                session_id: "tg:stream".to_string(),
                user_id: "u-stream".to_string(),
                text: "stream please".to_string(),
                reply_target: None,
            },
            &mut sink,
        )
        .await
        .expect("streamed message");

    assert_eq!(out.text, "hello world");
    assert_eq!(sink.chunks, vec!["hello ", "world"]);

    let history = store.load_recent("tg:stream", 10).await.expect("history");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].role, MessageRole::User);
    assert_eq!(history[1].role, MessageRole::Assistant);
    assert_eq!(history[1].content, "hello world");
}

#[tokio::test]
async fn service_streaming_verifies_swarm_reply_before_delivery() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let service = MessageService::new(Arc::new(SwarmToolLeakProvider), store.clone(), 20)
        .with_harness_config(&AgentHarnessConfig {
            output_verification_mode: OutputVerificationMode::Block,
            ..Default::default()
        })
        .with_agent_swarm(AgentSwarmSettings {
            enabled: true,
            auto_detect: false,
            reply_summary_enabled: false,
            ..Default::default()
        });
    let mut sink = CollectSink::default();

    let out = service
        .handle_stream(
            IncomingMessage {
                channel: "telegram".to_string(),
                session_id: "tg:stream:swarm-verify".to_string(),
                user_id: "u-stream".to_string(),
                text: "please coordinate this".to_string(),
                reply_target: None,
            },
            &mut sink,
        )
        .await
        .expect("streamed message");

    assert_eq!(
        out.text,
        "I could not produce a reliable final answer from the available tool results."
    );
    assert_eq!(sink.chunks, vec![out.text.clone()]);

    let history = store
        .load_recent("tg:stream:swarm-verify", 10)
        .await
        .expect("history");
    assert_eq!(history[1].content, out.text);
}

#[tokio::test]
async fn service_streaming_verifies_plain_reply_before_delivery_when_output_exit_enabled() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let service = MessageService::new(Arc::new(HiddenToolCallStreamingProvider), store.clone(), 20)
        .with_harness_config(&AgentHarnessConfig {
            output_verification_mode: OutputVerificationMode::Block,
            ..Default::default()
        });
    let mut sink = CollectSink::default();

    let out = service
        .handle_stream(
            IncomingMessage {
                channel: "telegram".to_string(),
                session_id: "tg:stream:plain-output-verify".to_string(),
                user_id: "u-stream".to_string(),
                text: "stream please".to_string(),
                reply_target: None,
            },
            &mut sink,
        )
        .await
        .expect("streamed message");

    let expected =
        "I could not produce a reliable final answer from the available tool results.".to_string();
    assert_eq!(out.text, expected);
    assert_eq!(sink.chunks, vec![expected.clone()]);
    assert!(!sink.chunks.iter().any(|chunk| chunk.contains("\"server\"")));

    let history = store
        .load_recent("tg:stream:plain-output-verify", 10)
        .await
        .expect("history");
    assert_eq!(history.len(), 2);
    assert_eq!(history[1].role, MessageRole::Assistant);
    assert_eq!(history[1].content, expected);
}
