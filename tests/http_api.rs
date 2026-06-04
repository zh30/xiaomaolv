use std::sync::{Arc, Mutex};

use axum_test::TestServer;
use xiaomaolv::config::{
    AppConfig, AppSettings, ChannelsConfig, HttpChannelConfig, ProviderConfig,
};
use xiaomaolv::domain::MessageRole;
use xiaomaolv::http::build_router;
use xiaomaolv::mcp::{BUILTIN_MCP_SERVER_NAME, BUILTIN_MCP_TOOL_CURRENT_TIME};
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

#[derive(Default)]
struct ToolCallingProvider {
    calls: Mutex<usize>,
}

#[async_trait::async_trait]
impl ChatProvider for ToolCallingProvider {
    fn model_name(&self) -> Option<&str> {
        Some("tool-test-model")
    }

    async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
        let call_index = {
            let mut guard = self.calls.lock().expect("provider call mutex");
            let call_index = *guard;
            *guard = (*guard).saturating_add(1);
            call_index
        };

        if call_index == 0 {
            return Ok(serde_json::json!({
                "server": BUILTIN_MCP_SERVER_NAME,
                "tool": BUILTIN_MCP_TOOL_CURRENT_TIME,
                "arguments": {}
            })
            .to_string());
        }

        Ok("tool final answer".to_string())
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
async fn post_messages_retains_runtime_api_cors_headers() {
    let cfg = test_config(None, 120);

    let app = build_router(cfg, "sqlite::memory:", Some(Arc::new(FakeProvider)))
        .await
        .expect("router");
    let server = TestServer::new(app).expect("test server");

    let response = server
        .post("/v1/messages")
        .add_header("origin", "https://example.invalid")
        .json(&serde_json::json!({
            "session_id": "s-http-cors",
            "user_id": "u-http",
            "text": "hello"
        }))
        .await;

    response.assert_status_ok();
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("*")
    );
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
async fn get_code_mode_metrics_includes_harness_metrics_after_tool_request() {
    let mut cfg = test_config(Some("diag-token"), 120);
    cfg.agent.harness.enable_trajectory = true;
    cfg.agent.swarm.enabled = false;

    let app = build_router(
        cfg,
        "sqlite::memory:",
        Some(Arc::new(ToolCallingProvider::default())),
    )
    .await
    .expect("router");
    let server = TestServer::new(app).expect("test server");

    let message = server
        .post("/v1/messages")
        .json(&serde_json::json!({
            "session_id": "s-harness-metrics",
            "user_id": "u-harness-metrics",
            "text": "please call the time tool"
        }))
        .await;
    message.assert_status_ok();

    let response = server
        .get("/v1/code-mode/metrics")
        .add_header("authorization", "Bearer diag-token")
        .await;
    response.assert_status_ok();
    let body = response.text();

    assert!(
        prometheus_line_has_value(
            &body,
            "xiaomaolv_trajectories_total",
            &[("status", "final_answer")],
            "1",
        ),
        "{body}"
    );
    assert!(
        prometheus_line_has_value(
            &body,
            "xiaomaolv_tool_calls_total",
            &[
                ("server", BUILTIN_MCP_SERVER_NAME),
                ("tool", BUILTIN_MCP_TOOL_CURRENT_TIME),
                ("ok", "true")
            ],
            "1",
        ),
        "{body}"
    );
    assert!(
        prometheus_line_has_value(
            &body,
            "xiaomaolv_trajectory_duration_seconds_count",
            &[],
            "1"
        ),
        "{body}"
    );
    assert!(
        prometheus_line_has_value(&body, "xiaomaolv_avg_iterations_per_trajectory", &[], "2"),
        "{body}"
    );
}

#[tokio::test]
async fn get_harness_trajectories_requires_api_key() {
    let mut cfg = test_config(None, 120);
    cfg.app.api_key = Some("api-token".to_string());

    let app = build_router(cfg, "sqlite::memory:", Some(Arc::new(FakeProvider)))
        .await
        .expect("router");
    let server = TestServer::new(app).expect("test server");

    let unauthorized = server.get("/v1/harness/trajectories").await;
    unauthorized.assert_status_unauthorized();

    let authorized = server
        .get("/v1/harness/trajectories")
        .add_header("authorization", "Bearer api-token")
        .await;
    authorized.assert_status_ok();
}

#[tokio::test]
async fn get_harness_trajectories_is_rate_limited() {
    let mut cfg = test_config(None, 120);
    cfg.channels.http.rate_limit_per_minute = 1;

    let app = build_router(cfg, "sqlite::memory:", Some(Arc::new(FakeProvider)))
        .await
        .expect("router");
    let server = TestServer::new(app).expect("test server");

    let first = server.get("/v1/harness/trajectories").await;
    first.assert_status_ok();

    let second = server.get("/v1/harness/trajectories").await;
    second.assert_status_too_many_requests();
}

#[tokio::test]
async fn get_harness_trajectory_detail_returns_tool_calls() {
    let mut cfg = test_config(None, 120);
    cfg.agent.harness.enable_trajectory = true;
    cfg.agent.swarm.enabled = false;

    let app = build_router(
        cfg,
        "sqlite::memory:",
        Some(Arc::new(ToolCallingProvider::default())),
    )
    .await
    .expect("router");
    let server = TestServer::new(app).expect("test server");

    let message = server
        .post("/v1/messages")
        .json(&serde_json::json!({
            "session_id": "s-harness-detail",
            "user_id": "u-harness-detail",
            "text": "please call the time tool"
        }))
        .await;
    message.assert_status_ok();

    let list = server
        .get("/v1/harness/trajectories?session_id=s-harness-detail&limit=9999&exit_reason=final_answer")
        .await;
    list.assert_status_ok();
    let payload: serde_json::Value = list.json();
    let trajectories = payload
        .get("trajectories")
        .and_then(|value| value.as_array())
        .expect("trajectories array");
    assert_eq!(trajectories.len(), 1);
    let trajectory_id = trajectories[0]
        .get("id")
        .and_then(|value| value.as_str())
        .expect("trajectory id");
    assert_eq!(
        trajectories[0]
            .get("tool_calls")
            .and_then(|value| value.as_array())
            .and_then(|calls| calls.first())
            .and_then(|call| call.get("call_index"))
            .and_then(|value| value.as_u64()),
        Some(0)
    );

    let detail = server
        .get(&format!("/v1/harness/trajectories/{trajectory_id}"))
        .await;
    detail.assert_status_ok();
    let payload: serde_json::Value = detail.json();
    assert_eq!(
        payload
            .get("trajectory")
            .and_then(|value| value.get("id"))
            .and_then(|value| value.as_str()),
        Some(trajectory_id)
    );
    assert_eq!(
        payload
            .get("trajectory")
            .and_then(|value| value.get("tool_calls"))
            .and_then(|value| value.as_array())
            .map(Vec::len),
        Some(1)
    );
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
            api_key: None,
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
                rate_limit_per_minute: 0,
            },
            telegram: None,
            plugins: std::collections::HashMap::new(),
        },
        memory: Default::default(),
        agent: Default::default(),
    }
}

fn prometheus_line_has_value(
    body: &str,
    metric: &str,
    labels: &[(&str, &str)],
    value: &str,
) -> bool {
    body.lines().any(|line| {
        line.starts_with(metric)
            && labels.iter().all(|(key, value)| {
                let label = format!("{key}=\"{value}\"");
                line.contains(&label)
            })
            && line.split_whitespace().last() == Some(value)
    })
}
