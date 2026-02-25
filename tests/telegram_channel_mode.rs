use std::collections::HashMap;
use std::sync::Arc;

use axum_test::TestServer;
use xiaomaolv::config::{
    AppConfig, AppSettings, ChannelsConfig, HttpChannelConfig, ProviderConfig,
    TelegramAdminUserIds, TelegramChannelConfig,
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
async fn telegram_defaults_to_polling_and_webhook_endpoint_is_disabled() {
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
                options: HashMap::new(),
            },
        ))
        .collect(),
        channels: ChannelsConfig {
            http: HttpChannelConfig { enabled: true },
            telegram: Some(TelegramChannelConfig {
                enabled: true,
                bot_token: "fake-token".to_string(),
                bot_username: None,
                webhook_secret: None,
                mode: None,
                polling_timeout_secs: 1,
                streaming_enabled: true,
                streaming_edit_interval_ms: 900,
                streaming_prefer_draft: true,
                startup_online_enabled: false,
                startup_online_text: "online".to_string(),
                commands_enabled: true,
                commands_auto_register: true,
                commands_private_only: true,
                admin_user_ids: TelegramAdminUserIds::List(vec![]),
                group_trigger_mode: "strict".to_string(),
                group_followup_window_secs: 180,
                group_cooldown_secs: 20,
                group_rule_min_score: 70,
                group_llm_gate_enabled: false,
            }),
            plugins: HashMap::new(),
        },
        memory: Default::default(),
        agent: Default::default(),
    };

    let app = build_router(cfg, "sqlite::memory:", Some(Arc::new(FakeProvider)))
        .await
        .expect("router");
    let server = TestServer::new(app).expect("test server");

    let response = server
        .post("/v1/telegram/webhook/whatever")
        .json(&serde_json::json!({"message": {"chat": {"id": 1}, "text": "hello"}}))
        .await;

    response.assert_status_bad_request();

    let mode_response = server.get("/v1/channels/telegram/mode").await;
    mode_response.assert_status_ok();
    mode_response.assert_json(&serde_json::json!({
        "channel": "telegram",
        "mode": "polling"
    }));
}

#[tokio::test]
async fn telegram_mode_endpoint_reports_webhook_when_configured() {
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
                options: HashMap::new(),
            },
        ))
        .collect(),
        channels: ChannelsConfig {
            http: HttpChannelConfig { enabled: true },
            telegram: Some(TelegramChannelConfig {
                enabled: true,
                bot_token: "fake-token".to_string(),
                bot_username: None,
                webhook_secret: Some("hook-secret".to_string()),
                mode: Some("webhook".to_string()),
                polling_timeout_secs: 1,
                streaming_enabled: true,
                streaming_edit_interval_ms: 900,
                streaming_prefer_draft: true,
                startup_online_enabled: false,
                startup_online_text: "online".to_string(),
                commands_enabled: true,
                commands_auto_register: true,
                commands_private_only: true,
                admin_user_ids: TelegramAdminUserIds::List(vec![]),
                group_trigger_mode: "strict".to_string(),
                group_followup_window_secs: 180,
                group_cooldown_secs: 20,
                group_rule_min_score: 70,
                group_llm_gate_enabled: false,
            }),
            plugins: HashMap::new(),
        },
        memory: Default::default(),
        agent: Default::default(),
    };

    let app = build_router(cfg, "sqlite::memory:", Some(Arc::new(FakeProvider)))
        .await
        .expect("router");
    let server = TestServer::new(app).expect("test server");

    let mode_response = server.get("/v1/channels/telegram/mode").await;
    mode_response.assert_status_ok();
    mode_response.assert_json(&serde_json::json!({
        "channel": "telegram",
        "mode": "webhook"
    }));
}
