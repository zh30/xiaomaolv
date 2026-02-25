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

#[tokio::test]
async fn service_observe_persists_user_message_without_assistant_reply() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let service = MessageService::new(Arc::new(FakeProvider), store.clone(), 20);

    service
        .observe(IncomingMessage {
            channel: "telegram".to_string(),
            session_id: "session-observe".to_string(),
            user_id: "u2".to_string(),
            text: "just watching".to_string(),
            reply_target: None,
        })
        .await
        .expect("observe message");

    let history = store
        .load_recent("session-observe", 10)
        .await
        .expect("history");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].role, MessageRole::User);
    assert_eq!(history[0].content, "just watching");
}

#[tokio::test]
async fn service_persists_and_loads_group_aliases() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let service = MessageService::new(Arc::new(FakeProvider), store.clone(), 20);

    service
        .upsert_group_aliases(
            "telegram".to_string(),
            -100889,
            vec!["小绿".to_string(), "小绿".to_string(), "龙猫".to_string()],
        )
        .await
        .expect("upsert group aliases");

    let aliases = service
        .load_group_aliases("telegram".to_string(), -100889, 10)
        .await
        .expect("load group aliases");

    assert_eq!(aliases.len(), 2);
    assert!(aliases.iter().any(|v| v == "小绿"));
    assert!(aliases.iter().any(|v| v == "龙猫"));
}

#[tokio::test]
async fn service_persists_and_loads_group_user_profiles() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let service = MessageService::new(Arc::new(FakeProvider), store.clone(), 20);

    service
        .upsert_group_user_profile(
            "telegram".to_string(),
            -100778,
            1001,
            "阿青".to_string(),
            Some("aqing_99".to_string()),
        )
        .await
        .expect("upsert profile");

    let profiles = service
        .load_group_user_profiles("telegram".to_string(), -100778, 10)
        .await
        .expect("load profiles");

    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0].user_id, 1001);
    assert_eq!(profiles[0].preferred_name, "阿青");
    assert_eq!(profiles[0].username.as_deref(), Some("aqing_99"));
}
