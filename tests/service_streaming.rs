use std::sync::Arc;

use xiaomaolv::domain::{IncomingMessage, MessageRole};
use xiaomaolv::memory::SqliteMemoryStore;
use xiaomaolv::provider::{ChatProvider, CompletionRequest, StreamSink};
use xiaomaolv::service::MessageService;

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
