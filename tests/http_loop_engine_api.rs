use std::sync::Arc;

use async_trait::async_trait;
use axum_test::TestServer;
use xiaomaolv::config::{
    AppConfig, AppSettings, ChannelsConfig, HttpChannelConfig, ProviderConfig,
};
use xiaomaolv::http::build_router;
use xiaomaolv::provider::{ChatProvider, CompletionRequest};

struct HarnessHttpProvider;

#[async_trait]
impl ChatProvider for HarnessHttpProvider {
    fn model_name(&self) -> Option<&str> {
        Some("harness-http-test")
    }

    async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
        Ok("analysis".to_string())
    }
}

fn config() -> AppConfig {
    let mut config = AppConfig {
        app: AppSettings {
            bind: "127.0.0.1:0".to_string(),
            default_provider: "test".to_string(),
            locale: "en-US".to_string(),
            max_history: 8,
            concurrency_limit: 8,
            api_key: Some("operator-key".to_string()),
        },
        providers: std::iter::once((
            "test".to_string(),
            ProviderConfig {
                kind: "openai-compatible".to_string(),
                base_url: Some("http://127.0.0.1:9".to_string()),
                api_key: Some("unused".to_string()),
                model: Some("test".to_string()),
                timeout_secs: 1,
                max_retries: 0,
                options: Default::default(),
            },
        ))
        .collect(),
        channels: ChannelsConfig {
            http: HttpChannelConfig {
                enabled: true,
                diag_bearer_token: None,
                diag_rate_limit_per_minute: 120,
                rate_limit_per_minute: 0,
            },
            telegram: None,
            plugins: Default::default(),
        },
        memory: Default::default(),
        agent: Default::default(),
    };
    config.agent.mcp_enabled = false;
    config.agent.swarm.enabled = false;
    config.agent.harness.loop_engine.enabled = true;
    config.agent.harness.loop_engine.ingest_api_key = Some("ingest-key".to_string());
    config
}

fn operator(request: axum_test::TestRequest) -> axum_test::TestRequest {
    request
        .add_header("authorization", "Bearer operator-key")
        .add_header("x-harness-actor", "operator:test")
}

#[tokio::test]
async fn scoped_signal_ingest_and_operator_goal_resume_lifecycle() {
    let temp = tempfile::tempdir().expect("tempdir");
    let database_url = format!("sqlite://{}", temp.path().join("http.db").display());
    let app = build_router(config(), &database_url, Some(Arc::new(HarnessHttpProvider)))
        .await
        .expect("router");
    let server = TestServer::new(app).expect("server");
    let signal_payload = serde_json::json!({
        "kind": "community",
        "trust": "external",
        "source": "github:community",
        "external_id": "discussion-42",
        "content": "Expose durable recovery state",
        "metadata": {}
    });

    let wrong_scope = operator(server.post("/v1/harness/signals"))
        .json(&signal_payload)
        .await;
    wrong_scope.assert_status_unauthorized();
    let signal = server
        .post("/v1/harness/signals")
        .add_header("authorization", "Bearer ingest-key")
        .add_header("x-harness-actor", "ingest:test")
        .json(&signal_payload)
        .await;
    signal.assert_status_ok();

    let goal = operator(server.post("/v1/harness/goals"))
        .json(&serde_json::json!({
            "objective": "Make /resume observable",
            "source_signal_ids": [],
            "auto_plan": true
        }))
        .await;
    goal.assert_status_ok();
    let planned: serde_json::Value = goal.json();
    assert_eq!(planned["goal"]["status"], "review_ready");
    let goal_id = planned["goal"]["id"].as_str().expect("goal id");

    let approved = operator(server.post(&format!("/v1/harness/goals/{goal_id}/approve")))
        .json(&serde_json::json!({
            "expected_goal_revision": planned["goal"]["revision"],
            "expected_plan_hash": planned["plan_hash"]
        }))
        .await;
    approved.assert_status_ok();
    let resumed = operator(server.post(&format!("/v1/harness/goals/{goal_id}/resume"))).await;
    resumed.assert_status_ok();
    let state: serde_json::Value = resumed.json();
    assert_eq!(state["goal"]["status"], "approved");
    assert_eq!(state["work_items"].as_array().map(Vec::len), Some(2));

    let events = operator(server.get(&format!("/v1/harness/goals/{goal_id}/events"))).await;
    events.assert_status_ok();
    let events: serde_json::Value = events.json();
    let event_types = events["events"]
        .as_array()
        .expect("events")
        .iter()
        .filter_map(|event| event["event_type"].as_str())
        .collect::<Vec<_>>();
    assert!(event_types.contains(&"goal.created"));
    assert!(event_types.contains(&"goal.approved"));

    let goals = operator(server.get("/v1/harness/goals?limit=20")).await;
    goals.assert_status_ok();
    let goals: serde_json::Value = goals.json();
    assert!(
        goals["goals"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["id"] == goal_id))
    );

    let signals = operator(server.get("/v1/harness/signals?limit=20")).await;
    signals.assert_status_ok();
    let signals: serde_json::Value = signals.json();
    assert_eq!(signals["signals"].as_array().map(Vec::len), Some(1));

    let artifact = operator(server.post("/v1/harness/artifacts"))
        .json(&serde_json::json!({
            "kind": "desktop_view",
            "name": "goal-list",
            "version": "1",
            "content": {"columns": ["status", "objective"]},
            "source_goal_id": goal_id,
            "parent_artifact_id": null
        }))
        .await;
    artifact.assert_status_ok();
    let artifacts = operator(server.get("/v1/harness/artifacts?limit=20")).await;
    artifacts.assert_status_ok();
    let artifacts: serde_json::Value = artifacts.json();
    assert_eq!(artifacts["artifacts"].as_array().map(Vec::len), Some(1));
}
