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
    let cfg = test_config(None, 120);

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

#[tokio::test]
async fn get_mcp_servers_returns_json_payload() {
    let cfg = test_config(None, 120);

    let app = build_router(cfg, "sqlite::memory:", Some(Arc::new(FakeProvider)))
        .await
        .expect("router");
    let server = TestServer::new(app).expect("test server");

    let response = server.get("/v1/mcp/servers").await;
    response.assert_status_ok();

    let payload: serde_json::Value = response.json();
    assert!(payload.get("servers").is_some());
    assert!(payload.get("servers").and_then(|v| v.as_array()).is_some());
}

#[tokio::test]
async fn get_code_mode_diag_requires_bearer_token() {
    let cfg = test_config(Some("diag-token"), 120);

    let app = build_router(cfg, "sqlite::memory:", Some(Arc::new(FakeProvider)))
        .await
        .expect("router");
    let server = TestServer::new(app).expect("test server");

    let response = server.get("/v1/code-mode/diag").await;
    response.assert_status_unauthorized();
}

#[tokio::test]
async fn get_code_mode_diag_rejects_invalid_bearer_token() {
    let cfg = test_config(Some("diag-token"), 120);

    let app = build_router(cfg, "sqlite::memory:", Some(Arc::new(FakeProvider)))
        .await
        .expect("router");
    let server = TestServer::new(app).expect("test server");

    let response = server
        .get("/v1/code-mode/diag")
        .add_header("authorization", "Bearer wrong-token")
        .await;
    response.assert_status_unauthorized();
}

#[tokio::test]
async fn get_code_mode_diag_returns_breaker_snapshot() {
    let cfg = test_config(Some("diag-token"), 120);

    let app = build_router(cfg, "sqlite::memory:", Some(Arc::new(FakeProvider)))
        .await
        .expect("router");
    let server = TestServer::new(app).expect("test server");

    let response = server
        .get("/v1/code-mode/diag")
        .add_header("authorization", "Bearer diag-token")
        .await;
    response.assert_status_ok();

    let payload: serde_json::Value = response.json();
    assert_eq!(
        payload
            .get("runtime")
            .and_then(|v| v.get("circuit_open"))
            .and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        payload
            .get("runtime")
            .and_then(|v| v.get("timeout_alert_streak"))
            .and_then(|v| v.as_u64()),
        Some(0)
    );
    assert_eq!(
        payload
            .get("runtime")
            .and_then(|v| v.get("probe_counter"))
            .and_then(|v| v.as_u64()),
        Some(0)
    );
    assert_eq!(
        payload
            .get("runtime")
            .and_then(|v| v.get("counters"))
            .and_then(|v| v.get("attempts_total"))
            .and_then(|v| v.as_u64()),
        Some(0)
    );
    assert_eq!(
        payload
            .get("runtime")
            .and_then(|v| v.get("counters"))
            .and_then(|v| v.get("fallback_total"))
            .and_then(|v| v.as_u64()),
        Some(0)
    );
    assert_eq!(
        payload
            .get("runtime")
            .and_then(|v| v.get("counters"))
            .and_then(|v| v.get("circuit_open_total"))
            .and_then(|v| v.as_u64()),
        Some(0)
    );
    assert_eq!(
        payload
            .get("policy")
            .and_then(|v| v.get("timeout_auto_shadow_probe_every"))
            .and_then(|v| v.as_u64()),
        Some(5)
    );
}

#[tokio::test]
async fn get_code_mode_metrics_requires_bearer_token() {
    let cfg = test_config(Some("diag-token"), 120);

    let app = build_router(cfg, "sqlite::memory:", Some(Arc::new(FakeProvider)))
        .await
        .expect("router");
    let server = TestServer::new(app).expect("test server");

    let response = server.get("/v1/code-mode/metrics").await;
    response.assert_status_unauthorized();
}

#[tokio::test]
async fn get_code_mode_metrics_returns_prometheus_text() {
    let cfg = test_config(Some("diag-token"), 120);

    let app = build_router(cfg, "sqlite::memory:", Some(Arc::new(FakeProvider)))
        .await
        .expect("router");
    let server = TestServer::new(app).expect("test server");

    let response = server
        .get("/v1/code-mode/metrics")
        .add_header("authorization", "Bearer diag-token")
        .await;
    response.assert_status_ok();
    response.assert_header("content-type", "text/plain; version=0.0.4; charset=utf-8");
    response.assert_text_contains("xiaomaolv_code_mode_attempts_total");
    response.assert_text_contains("xiaomaolv_code_mode_circuit_open");
    response.assert_text_contains("xiaomaolv_code_mode_timeout_warn_ratio");
    response.assert_text_contains("xiaomaolv_code_mode_timeout_auto_shadow_probe_every");
}

#[tokio::test]
async fn get_code_mode_metrics_accepts_lowercase_bearer_scheme() {
    let cfg = test_config(Some("diag-token"), 120);

    let app = build_router(cfg, "sqlite::memory:", Some(Arc::new(FakeProvider)))
        .await
        .expect("router");
    let server = TestServer::new(app).expect("test server");

    let response = server
        .get("/v1/code-mode/metrics")
        .add_header("authorization", "bearer diag-token")
        .await;
    response.assert_status_ok();
}

#[tokio::test]
async fn get_code_mode_diagnostics_endpoints_are_rate_limited() {
    let cfg = test_config(Some("diag-token"), 1);

    let app = build_router(cfg, "sqlite::memory:", Some(Arc::new(FakeProvider)))
        .await
        .expect("router");
    let server = TestServer::new(app).expect("test server");

    let first = server
        .get("/v1/code-mode/diag")
        .add_header("authorization", "Bearer diag-token")
        .await;
    first.assert_status_ok();

    let second = server
        .get("/v1/code-mode/metrics")
        .add_header("authorization", "Bearer diag-token")
        .await;
    second.assert_status_too_many_requests();
}

#[tokio::test]
async fn unauthorized_code_mode_diag_requests_do_not_consume_rate_limit_budget() {
    let cfg = test_config(Some("diag-token"), 1);

    let app = build_router(cfg, "sqlite::memory:", Some(Arc::new(FakeProvider)))
        .await
        .expect("router");
    let server = TestServer::new(app).expect("test server");

    let unauthorized = server.get("/v1/code-mode/diag").await;
    unauthorized.assert_status_unauthorized();

    let authorized = server
        .get("/v1/code-mode/diag")
        .add_header("authorization", "Bearer diag-token")
        .await;
    authorized.assert_status_ok();
}

#[tokio::test]
async fn get_code_mode_diagnostics_rate_limit_isolated_by_source() {
    let cfg = test_config(Some("diag-token"), 1);

    let app = build_router(cfg, "sqlite::memory:", Some(Arc::new(FakeProvider)))
        .await
        .expect("router");
    let server = TestServer::new(app).expect("test server");

    let first = server
        .get("/v1/code-mode/diag")
        .add_header("authorization", "Bearer diag-token")
        .add_header("x-forwarded-for", "198.51.100.10")
        .await;
    first.assert_status_ok();

    let second = server
        .get("/v1/code-mode/metrics")
        .add_header("authorization", "Bearer diag-token")
        .add_header("x-forwarded-for", "203.0.113.20")
        .await;
    second.assert_status_ok();
}

fn test_config(diag_bearer_token: Option<&str>, diag_rate_limit_per_minute: usize) -> AppConfig {
    AppConfig {
        app: AppSettings {
            bind: "127.0.0.1:0".to_string(),
            default_provider: "openai".to_string(),
            locale: "en-US".to_string(),
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
            http: HttpChannelConfig {
                enabled: true,
                diag_bearer_token: diag_bearer_token.map(|v| v.to_string()),
                diag_rate_limit_per_minute,
            },
            telegram: None,
            plugins: std::collections::HashMap::new(),
        },
        memory: Default::default(),
        agent: Default::default(),
    }
}
