use std::sync::Arc;

use xiaomaolv::domain::{IncomingMessage, MessageRole};
use xiaomaolv::memory::SqliteMemoryStore;
use xiaomaolv::provider::{ChatProvider, CompletionRequest};
use xiaomaolv::service::MessageService;

struct FakeProvider;

#[async_trait::async_trait]
impl ChatProvider for FakeProvider {
    async fn complete(&self, req: CompletionRequest) -> anyhow::Result<String> {
        let user = req
            .messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::User)
            .map(|m| m.content.as_str())
            .unwrap_or("");
        Ok(format!("echo:{user}"))
    }
}

#[tokio::test]
async fn service_generates_reply_and_persists_messages() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let service = MessageService::new(Arc::new(FakeProvider), store.clone(), 20);

    let out = service
        .handle(IncomingMessage {
            channel: "http".to_string(),
            session_id: "session-2".to_string(),
            user_id: "u1".to_string(),
            text: "ping".to_string(),
            reply_target: None,
        })
        .await
        .expect("handle message");

    assert_eq!(out.text, "echo:ping");

    let history = store.load_recent("session-2", 10).await.expect("history");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].role, MessageRole::User);
    assert_eq!(history[1].role, MessageRole::Assistant);
}
