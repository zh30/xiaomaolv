use std::sync::{Arc, Mutex};

use xiaomaolv::config::{AgentHarnessConfig, OutputVerificationMode};
use xiaomaolv::domain::{IncomingMessage, MessageRole};
use xiaomaolv::memory::SqliteMemoryStore;
use xiaomaolv::provider::{ChatProvider, CompletionRequest};
use xiaomaolv::service::{AgentSwarmSettings, MessageService};

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

#[derive(Default)]
struct OutputRevisionProvider {
    calls: Mutex<usize>,
}

#[async_trait::async_trait]
impl ChatProvider for OutputRevisionProvider {
    async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
        let call_index = {
            let mut guard = self.calls.lock().expect("provider call mutex");
            let call_index = *guard;
            *guard = (*guard).saturating_add(1);
            call_index
        };
        if call_index == 0 {
            Ok(r#"{"server":"s","tool":"t","arguments":{}}"#.to_string())
        } else {
            Ok("clean final answer".to_string())
        }
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
async fn service_output_verification_revises_once_before_persisting() {
    let provider = Arc::new(OutputRevisionProvider::default());
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let service = MessageService::new(provider.clone(), store.clone(), 20)
        .with_harness_config(&AgentHarnessConfig {
            output_verification_mode: OutputVerificationMode::ReviseOnce,
            output_verification_llm_enabled: true,
            ..Default::default()
        })
        .with_agent_swarm(AgentSwarmSettings {
            enabled: false,
            ..Default::default()
        });

    let out = service
        .handle(IncomingMessage {
            channel: "http".to_string(),
            session_id: "session-output-verify".to_string(),
            user_id: "u1".to_string(),
            text: "ping".to_string(),
            reply_target: None,
        })
        .await
        .expect("handle message");

    assert_eq!(out.text, "clean final answer");
    assert_eq!(*provider.calls.lock().expect("provider call mutex"), 2);

    let history = store
        .load_recent("session-output-verify", 10)
        .await
        .expect("history");
    assert_eq!(history[1].content, "clean final answer");
}

#[tokio::test]
async fn service_output_verification_without_llm_uses_deterministic_revision() {
    let provider = Arc::new(OutputRevisionProvider::default());
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let service = MessageService::new(provider.clone(), store.clone(), 20)
        .with_harness_config(&AgentHarnessConfig {
            output_verification_mode: OutputVerificationMode::ReviseOnce,
            output_verification_llm_enabled: false,
            ..Default::default()
        })
        .with_agent_swarm(AgentSwarmSettings {
            enabled: false,
            ..Default::default()
        });

    let out = service
        .handle(IncomingMessage {
            channel: "http".to_string(),
            session_id: "session-output-verify-no-llm".to_string(),
            user_id: "u1".to_string(),
            text: "ping".to_string(),
            reply_target: None,
        })
        .await
        .expect("handle message");

    assert_eq!(
        out.text,
        "I could not produce a reliable final answer from the available tool results."
    );
    assert_eq!(*provider.calls.lock().expect("provider call mutex"), 1);

    let history = store
        .load_recent("session-output-verify-no-llm", 10)
        .await
        .expect("history");
    assert_eq!(history[1].content, out.text);
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
