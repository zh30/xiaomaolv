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

#[tokio::test]
async fn operator_filters_signals_by_status_and_ignores_with_reason() {
    let temp = tempfile::tempdir().expect("tempdir");
    let database_url = format!("sqlite://{}", temp.path().join("http-signals.db").display());
    let app = build_router(config(), &database_url, Some(Arc::new(HarnessHttpProvider)))
        .await
        .expect("router");
    let server = TestServer::new(app).expect("server");

    let ingest = |external_id: &str, content: &str| {
        server
            .post("/v1/harness/signals")
            .add_header("authorization", "Bearer ingest-key")
            .add_header("x-harness-actor", "ingest:test")
            .json(&serde_json::json!({
                "kind": "community",
                "trust": "external",
                "source": "github:community",
                "external_id": external_id,
                "content": content,
                "metadata": {}
            }))
    };
    ingest("d-1", "First report about flaky search")
        .await
        .assert_status_ok();
    ingest("d-2", "Second report about stalled schedules")
        .await
        .assert_status_ok();

    let observed = operator(server.get("/v1/harness/signals?status=observed")).await;
    observed.assert_status_ok();
    let observed: serde_json::Value = observed.json();
    assert_eq!(observed["signals"].as_array().map(Vec::len), Some(2));
    let signal_id = observed["signals"][0]["id"].as_str().expect("signal id");

    // Ignoring requires an operator key, not the scoped ingest key.
    let wrong_scope = server
        .post(&format!("/v1/harness/signals/{signal_id}/ignore"))
        .add_header("authorization", "Bearer ingest-key")
        .json(&serde_json::json!({"reason": "noise"}))
        .await;
    wrong_scope.assert_status_unauthorized();

    let ignored = operator(server.post(&format!("/v1/harness/signals/{signal_id}/ignore")))
        .json(&serde_json::json!({"reason": "not actionable"}))
        .await;
    ignored.assert_status_ok();
    let ignored: serde_json::Value = ignored.json();
    assert_eq!(ignored["status"], "ignored");

    let after = operator(server.get("/v1/harness/signals?status=observed")).await;
    let after: serde_json::Value = after.json();
    assert_eq!(after["signals"].as_array().map(Vec::len), Some(1));
    let ignored_list = operator(server.get("/v1/harness/signals?status=ignored")).await;
    let ignored_list: serde_json::Value = ignored_list.json();
    assert_eq!(ignored_list["signals"].as_array().map(Vec::len), Some(1));

    // An ignored signal cannot be proposed into a goal.
    let propose = operator(server.post(&format!("/v1/harness/signals/{signal_id}/propose-goal")))
        .json(&serde_json::json!({"objective": "revive it"}))
        .await;
    propose.assert_status_bad_request();
}

#[tokio::test]
async fn work_item_confirmation_resolve_over_http() {
    let temp = tempfile::tempdir().expect("tempdir");
    let database_url = format!("sqlite://{}", temp.path().join("resolve.db").display());
    let mut cfg = config();
    cfg.agent.harness.loop_engine.external_write_enabled = true;
    cfg.agent.harness.loop_engine.external_write_handlers = vec!["channel_send".to_string()];
    let app = build_router(
        cfg.clone(),
        &database_url,
        Some(Arc::new(HarnessHttpProvider)),
    )
    .await
    .expect("router");
    let server = TestServer::new(app).expect("server");

    // Create + plan + approve a channel_send goal over HTTP.
    let created = operator(server.post("/v1/harness/goals"))
        .json(&serde_json::json!({"objective": "notify", "auto_plan": false}))
        .await;
    created.assert_status_ok();
    let goal: serde_json::Value = created.json();
    let goal_id = goal["id"].as_str().expect("goal id");
    let planned = operator(server.post(&format!("/v1/harness/goals/{goal_id}/plan")))
        .json(&serde_json::json!({
            "workflow": {
                "steps": [{
                    "id": "notify", "handler": "channel_send", "effect": "external_write",
                    "input": {"channel": "telegram", "session_id": "tg:42", "text": "hi"},
                    "retry": {"max_attempts": 1, "backoff_secs": 0}
                }],
                "edges": [],
                "budget": {"max_provider_calls": 0, "deadline_secs": 60, "max_response_bytes": 1024}
            },
            "acceptance_criteria": [{"kind": "manual_approval", "label": "op"}]
        }))
        .await;
    planned.assert_status_ok();
    let planned: serde_json::Value = planned.json();
    operator(server.post(&format!("/v1/harness/goals/{goal_id}/approve")))
        .json(&serde_json::json!({
            "expected_goal_revision": planned["goal"]["revision"],
            "expected_plan_hash": planned["plan_hash"]
        }))
        .await
        .assert_status_ok();

    // Work items are visible to the operator.
    let items = operator(server.get(&format!("/v1/harness/goals/{goal_id}/work-items"))).await;
    items.assert_status_ok();
    let items: serde_json::Value = items.json();
    let work_item_id = items["work_items"][0]["id"]
        .as_str()
        .expect("work item id")
        .to_string();
    assert_eq!(items["work_items"][0]["status"], "ready");

    // Park the item through a second engine on the same database (crash path).
    {
        use xiaomaolv::harness::loop_engine::{ExternalWritePolicy, LoopEngine, SqliteLoopStore};
        use xiaomaolv::memory::SqliteMemoryStore;
        let memory = SqliteMemoryStore::new(&database_url).await.expect("memory");
        let engine = std::sync::Arc::new(
            LoopEngine::new(std::sync::Arc::new(SqliteLoopStore::new(memory)))
                .with_external_write_policy(ExternalWritePolicy {
                    enabled: true,
                    allowed_handlers: ["channel_send".to_string()].into_iter().collect(),
                }),
        );
        let claim = engine
            .claim_work_item(goal_id, "notify", "worker:ext", 1, "operator:test")
            .await
            .expect("claim")
            .expect("claimable");
        engine
            .prepare_checkpoint(
                &claim,
                &format!("{}:{}:v1", claim.work_item.id, claim.attempt.id),
                "worker:ext",
            )
            .await
            .expect("prepare");
        tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
        engine
            .resume_goal(goal_id, "operator:test")
            .await
            .expect("resume");
    }

    let parked = operator(server.get(&format!("/v1/harness/goals/{goal_id}/work-items"))).await;
    let parked: serde_json::Value = parked.json();
    assert_eq!(parked["work_items"][0]["status"], "waiting_confirmation");

    // Wrong scope (ingest key) is rejected.
    server
        .post(&format!(
            "/v1/harness/goals/{goal_id}/work-items/{work_item_id}/resolve-confirmation"
        ))
        .add_header("authorization", "Bearer ingest-key")
        .json(&serde_json::json!({"resolution": "confirmed", "reason": "x"}))
        .await
        .assert_status_unauthorized();
    // Unknown resolution is rejected.
    operator(server.post(&format!(
        "/v1/harness/goals/{goal_id}/work-items/{work_item_id}/resolve-confirmation"
    )))
    .json(&serde_json::json!({"resolution": "bogus", "reason": "x"}))
    .await
    .assert_status_bad_request();
    // Cross-goal scoping is rejected.
    operator(server.post(&format!(
        "/v1/harness/goals/goal_other/work-items/{work_item_id}/resolve-confirmation"
    )))
    .json(&serde_json::json!({"resolution": "confirmed", "reason": "x"}))
    .await
    .assert_status_not_found();

    // Confirmed resolution succeeds the item.
    let resolved = operator(server.post(&format!(
        "/v1/harness/goals/{goal_id}/work-items/{work_item_id}/resolve-confirmation"
    )))
    .json(&serde_json::json!({"resolution": "confirmed", "reason": "verified in telegram"}))
    .await;
    resolved.assert_status_ok();
    let resolved: serde_json::Value = resolved.json();
    assert_eq!(resolved["status"], "succeeded");

    // Resolving again is a conflict.
    operator(server.post(&format!(
        "/v1/harness/goals/{goal_id}/work-items/{work_item_id}/resolve-confirmation"
    )))
    .json(&serde_json::json!({"resolution": "confirmed", "reason": "again"}))
    .await
    .assert_status(axum::http::StatusCode::CONFLICT);
}
