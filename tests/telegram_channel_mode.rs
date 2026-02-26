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

fn test_config() -> AppConfig {
    AppConfig {
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
            http: HttpChannelConfig {
                enabled: true,
                diag_bearer_token: None,
                diag_rate_limit_per_minute: 120,
            },
            telegram: Some(TelegramChannelConfig {
                enabled: true,
                bot_token: "fake-token".to_string(),
                bot_username: None,
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
                scheduler_enabled: true,
                scheduler_tick_secs: 2,
                scheduler_batch_size: 8,
                scheduler_lease_secs: 30,
                scheduler_default_timezone: "Asia/Shanghai".to_string(),
                scheduler_nl_enabled: true,
                scheduler_nl_min_confidence: 0.78,
                scheduler_require_confirm: true,
                scheduler_max_jobs_per_owner: 64,
            }),
            plugins: HashMap::new(),
        },
        memory: Default::default(),
        agent: Default::default(),
    }
}

#[tokio::test]
async fn telegram_mode_endpoint_always_reports_polling() {
    let app = build_router(
        test_config(),
        "sqlite::memory:",
        Some(Arc::new(FakeProvider)),
    )
    .await
    .expect("router");
    let server = TestServer::new(app).expect("test server");

    let mode_response = server.get("/v1/channels/telegram/mode").await;
    mode_response.assert_status_ok();
    mode_response.assert_json(&serde_json::json!({
        "channel": "telegram",
        "mode": "polling"
    }));
}

#[tokio::test]
async fn telegram_webhook_endpoint_is_removed_and_inbound_rejected() {
    let app = build_router(
        test_config(),
        "sqlite::memory:",
        Some(Arc::new(FakeProvider)),
    )
    .await
    .expect("router");
    let server = TestServer::new(app).expect("test server");

    let webhook_response = server
        .post("/v1/telegram/webhook/whatever")
        .json(&serde_json::json!({"message": {"chat": {"id": 1}, "text": "hello"}}))
        .await;
    webhook_response.assert_status_not_found();

    let inbound_response = server
        .post("/v1/channels/telegram/inbound/whatever")
        .json(&serde_json::json!({"message": {"chat": {"id": 1}, "text": "hello"}}))
        .await;
    inbound_response.assert_status_bad_request();
}
