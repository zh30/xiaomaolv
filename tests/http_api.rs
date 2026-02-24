use std::sync::Arc;

use axum_test::TestServer;
use xiaomaolv::config::{
    AppConfig, AppSettings, ChannelsConfig, HttpChannelConfig, ProviderConfig,
};
use xiaomaolv::domain::MessageRole;
use xiaomaolv::http::build_router;
use xiaomaolv::provider::{ChatProvider, CompletionRequest};

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
        Ok(format!("ack:{user}"))
    }
}

#[tokio::test]
async fn post_messages_returns_assistant_reply() {
    let cfg = AppConfig {
        app: AppSettings {
            bind: "127.0.0.1:0".to_string(),
            default_provider: "openai".to_string(),
            max_history: 16,
            concurrency_limit: 32,
        },
        providers: std::iter::once((
            "openai".to_string(),
            ProviderConfig {
                kind: "openai-compatible".to_string(),
                base_url: Some("http://127.0.0.1:9999/v1".to_string()),
                api_key: Some("x".to_string()),
                model: Some("m".to_string()),
                timeout_secs: 30,
                max_retries: 0,
                options: std::collections::HashMap::new(),
            },
        ))
        .collect(),
        channels: ChannelsConfig {
            http: HttpChannelConfig { enabled: true },
            telegram: None,
            plugins: std::collections::HashMap::new(),
        },
        memory: Default::default(),
    };

    let app = build_router(cfg, "sqlite::memory:", Some(Arc::new(FakeProvider)))
        .await
        .expect("router");
    let server = TestServer::new(app).expect("test server");

    let response = server
        .post("/v1/messages")
        .json(&serde_json::json!({
            "session_id": "s-http",
            "user_id": "u-http",
            "text": "hello"
        }))
        .await;

    response.assert_status_ok();
}
