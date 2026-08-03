use std::sync::Arc;

use async_trait::async_trait;
use axum_test::TestServer;
use xiaomaolv::config::{
    AppConfig, AppSettings, ChannelsConfig, HttpChannelConfig, ProviderConfig,
};
use xiaomaolv::domain::MessageRole;
use xiaomaolv::harness::evolution::{
    EvolutionCandidateStatus, EvolutionCaseAssertions, EvolutionEvalCase,
};
use xiaomaolv::harness::store::{
    EvolutionStore, HarnessStore, SqliteEvolutionStore, SqliteHarnessStore,
};
use xiaomaolv::harness::trajectory::{ToolCallRecord, TrajectoryExitReason};
use xiaomaolv::http::{build_app_runtime, build_router};
use xiaomaolv::memory::SqliteMemoryStore;
use xiaomaolv::provider::{ChatProvider, CompletionRequest};

struct EvolutionHttpProvider;

#[async_trait]
impl ChatProvider for EvolutionHttpProvider {
    async fn complete(&self, req: CompletionRequest) -> anyhow::Result<String> {
        let is_proposal = req
            .messages
            .iter()
            .any(|message| message.content.contains("SELF_EVOLUTION_PROPOSAL_JSON"));
        let candidate_policy = req
            .messages
            .iter()
            .any(|message| message.content.contains("candidate wins"));
        let user = req
            .messages
            .iter()
            .rev()
            .find(|message| message.role == MessageRole::User)
            .map(|message| message.content.as_str())
            .unwrap_or_default();
        if is_proposal {
            return Ok(serde_json::json!({
                "prompt_patch": "candidate wins",
                "rationale": "automated failure analysis"
            })
            .to_string());
        }
        if candidate_policy {
            Ok(format!("pass:{user}"))
        } else {
            Ok(format!("baseline miss:{user}"))
        }
    }
}

fn config(api_key: Option<&str>) -> AppConfig {
    let mut config = AppConfig {
        app: AppSettings {
            bind: "127.0.0.1:0".to_string(),
            default_provider: "openai".to_string(),
            locale: "en-US".to_string(),
            max_history: 16,
            concurrency_limit: 32,
            api_key: api_key.map(str::to_string),
        },
        providers: std::iter::once((
            "openai".to_string(),
            ProviderConfig {
                kind: "openai-compatible".to_string(),
                base_url: Some("http://127.0.0.1:9999/v1".to_string()),
                api_key: Some("unused".to_string()),
                model: Some("fake".to_string()),
                timeout_secs: 30,
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
    config.agent.harness.evolution.enabled = true;
    config.agent.harness.evolution.min_eval_cases = 3;
    config.agent.harness.evolution.min_candidate_score = 1.0;
    config.agent.harness.evolution.min_score_delta = 0.5;
    config.agent.harness.evolution.max_regressions = 0;
    config.agent.harness.evolution.require_human_approval = true;
    config
}

fn auth(request: axum_test::TestRequest) -> axum_test::TestRequest {
    request
        .add_header("authorization", "Bearer evolution-key")
        .add_header("x-evolution-actor", "henry")
}

#[tokio::test]
async fn evolution_control_plane_requires_configured_app_api_key() {
    let app = build_router(
        config(None),
        "sqlite::memory:",
        Some(Arc::new(EvolutionHttpProvider)),
    )
    .await
    .expect("router");
    let server = TestServer::new(app).expect("test server");

    let response = server.get("/v1/harness/evolution/status").await;
    response.assert_status_unauthorized();
    response.assert_text_contains("requires app.api_key");
}

#[tokio::test]
async fn automatic_cycles_require_trajectory_capture_and_operator_key() {
    let mut missing_trajectory = config(Some("evolution-key"));
    missing_trajectory
        .agent
        .harness
        .evolution
        .auto_cycle_enabled = true;
    missing_trajectory
        .agent
        .harness
        .evolution
        .cycle_interval_secs = 60;
    let error = build_router(
        missing_trajectory,
        "sqlite::memory:",
        Some(Arc::new(EvolutionHttpProvider)),
    )
    .await
    .expect_err("auto cycle must require trajectory capture");
    assert!(error.to_string().contains("enable_trajectory=true"));

    let mut missing_key = config(None);
    missing_key.agent.harness.enable_trajectory = true;
    missing_key.agent.harness.evolution.auto_cycle_enabled = true;
    missing_key.agent.harness.evolution.cycle_interval_secs = 60;
    let error = build_router(
        missing_key,
        "sqlite::memory:",
        Some(Arc::new(EvolutionHttpProvider)),
    )
    .await
    .expect_err("auto cycle must require an operator key");
    assert!(error.to_string().contains("requires app.api_key"));

    let mut invalid_evidence_limit = config(Some("evolution-key"));
    invalid_evidence_limit
        .agent
        .harness
        .evolution
        .max_evidence_chars = 128;
    let error = build_router(
        invalid_evidence_limit,
        "sqlite::memory:",
        Some(Arc::new(EvolutionHttpProvider)),
    )
    .await
    .expect_err("evidence bounds must fail at startup");
    assert!(error.to_string().contains("between 512 and 32000"));
}

#[tokio::test]
async fn automatic_worker_proposes_and_evaluates_but_does_not_activate() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let database_url = format!("sqlite://{}", tmp.path().join("auto-cycle.db").display());
    let memory = SqliteMemoryStore::new(&database_url)
        .await
        .expect("memory store");
    let evolution_store = SqliteEvolutionStore::new(memory.clone());
    for id in ["accuracy", "format", "safety"] {
        evolution_store
            .upsert_eval_case(
                EvolutionEvalCase {
                    id: id.to_string(),
                    name: id.to_string(),
                    input: format!("evaluate {id}"),
                    assertions: EvolutionCaseAssertions {
                        required_substrings: vec!["pass".to_string()],
                        forbidden_substrings: vec!["unsafe".to_string()],
                        require_json: false,
                    },
                    weight: 1.0,
                    enabled: true,
                },
                "human:henry",
            )
            .await
            .expect("seed eval case");
    }
    let harness_store = SqliteHarnessStore::new(memory);
    HarnessStore::start_trajectory(
        &harness_store,
        "traj-auto-cycle",
        "session-auto",
        "http",
        "user-a",
        "fake",
    )
    .await
    .expect("start trajectory");
    HarnessStore::insert_trajectory_tool_call(
        &harness_store,
        "traj-auto-cycle",
        ToolCallRecord {
            call_index: 0,
            server: "search".to_string(),
            tool: "lookup".to_string(),
            arguments: serde_json::json!({}),
            result: serde_json::json!({"error": "timeout"}),
            ok: false,
            duration_ms: 1_000,
            iteration: 0,
        },
    )
    .await
    .expect("failed tool call");
    HarnessStore::finish_trajectory(
        &harness_store,
        "traj-auto-cycle",
        None,
        TrajectoryExitReason::ToolError,
    )
    .await
    .expect("finish trajectory");

    let mut cfg = config(Some("evolution-key"));
    cfg.agent.harness.enable_trajectory = true;
    cfg.agent.harness.evolution.auto_cycle_enabled = true;
    cfg.agent.harness.evolution.cycle_interval_secs = 60;
    cfg.agent.harness.evolution.cycle_initial_delay_secs = 0;
    let runtime = build_app_runtime(cfg, &database_url, Some(Arc::new(EvolutionHttpProvider)))
        .await
        .expect("app runtime");
    let (router, handle) = runtime.into_parts();
    let server = TestServer::new(router).expect("test server");

    let mut candidates = Vec::new();
    for _ in 0..100 {
        candidates = evolution_store
            .list_candidates(10)
            .await
            .expect("list candidates");
        if candidates
            .iter()
            .any(|candidate| candidate.status == EvolutionCandidateStatus::Ready)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let active = evolution_store
        .active_policy()
        .await
        .expect("active policy");
    let status = auth(server.get("/v1/harness/evolution/status")).await;
    status.assert_status_ok();
    let status_payload: serde_json::Value = status.json();
    handle.shutdown().await;

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].status, EvolutionCandidateStatus::Ready);
    assert!(
        active.is_none(),
        "automatic cycle must not activate a candidate"
    );
    assert_eq!(status_payload["cycle_status"]["running"], false);
    assert_eq!(status_payload["cycle_status"]["last_outcome"], "ready");
    assert_eq!(
        status_payload["cycle_status"]["last_candidate_id"],
        candidates[0].id
    );
}

#[tokio::test]
async fn authenticated_http_lifecycle_evaluates_activates_applies_and_rolls_back_policy() {
    let app = build_router(
        config(Some("evolution-key")),
        "sqlite::memory:",
        Some(Arc::new(EvolutionHttpProvider)),
    )
    .await
    .expect("router");
    let server = TestServer::new(app).expect("test server");

    let unauthorized = server.get("/v1/harness/evolution/status").await;
    unauthorized.assert_status_unauthorized();

    let idle_cycle = auth(server.post("/v1/harness/evolution/cycle")).await;
    idle_cycle.assert_status_conflict();
    idle_cycle.assert_text_contains("eval suite is not ready");

    let invalid_feedback = auth(server.post("/v1/harness/evolution/feedback"))
        .json(&serde_json::json!({
            "trajectory_id": "missing",
            "score": -2.0,
            "tags": ["incorrect"],
            "comment": null
        }))
        .await;
    invalid_feedback.assert_status_bad_request();

    let feedback = auth(server.get("/v1/harness/evolution/feedback")).await;
    feedback.assert_status_ok();
    let feedback_payload: serde_json::Value = feedback.json();
    assert_eq!(feedback_payload["feedback"], serde_json::json!([]));

    for id in ["accuracy", "format", "safety"] {
        let response = auth(server.post("/v1/harness/evolution/eval-cases"))
            .json(&serde_json::json!({
                "id": id,
                "name": id,
                "input": format!("evaluate {id}"),
                "assertions": {
                    "required_substrings": ["pass"],
                    "forbidden_substrings": ["unsafe"],
                    "require_json": false
                },
                "weight": 1.0,
                "enabled": true
            }))
            .await;
        response.assert_status_ok();
    }

    let candidate_response = auth(server.post("/v1/harness/evolution/candidates"))
        .json(&serde_json::json!({
            "prompt_patch": "candidate wins",
            "rationale": "passes the regression suite",
            "source_trajectory_ids": []
        }))
        .await;
    candidate_response.assert_status_ok();
    let candidate_payload: serde_json::Value = candidate_response.json();
    let candidate_id = candidate_payload["candidate"]["id"]
        .as_str()
        .expect("candidate id")
        .to_string();

    let evaluation = auth(server.post(&format!(
        "/v1/harness/evolution/candidates/{candidate_id}/evaluate"
    )))
    .await;
    evaluation.assert_status_ok();
    let evaluation_payload: serde_json::Value = evaluation.json();
    assert_eq!(
        evaluation_payload["evaluation"]["decision"]["decision"],
        "ready"
    );

    let approval = auth(server.post(&format!(
        "/v1/harness/evolution/candidates/{candidate_id}/approve"
    )))
    .json(&serde_json::json!({"reason": "reviewed scorecard"}))
    .await;
    approval.assert_status_ok();

    let activation = auth(server.post(&format!(
        "/v1/harness/evolution/candidates/{candidate_id}/activate"
    )))
    .json(&serde_json::json!({"reason": "controlled rollout"}))
    .await;
    activation.assert_status_ok();

    let status = auth(server.get("/v1/harness/evolution/status")).await;
    status.assert_status_ok();
    let status_payload: serde_json::Value = status.json();
    assert_eq!(status_payload["enabled"], true);
    assert_eq!(
        status_payload["active_policy"]["candidate_id"],
        candidate_id
    );

    let message = server
        .post("/v1/messages")
        .add_header("authorization", "Bearer evolution-key")
        .json(&serde_json::json!({
            "session_id": "evolved-runtime",
            "user_id": "user-a",
            "text": "live request"
        }))
        .await;
    message.assert_status_ok();
    let message_payload: serde_json::Value = message.json();
    assert_eq!(message_payload["reply"], "pass:live request");

    let rollback = auth(server.post("/v1/harness/evolution/rollback"))
        .json(&serde_json::json!({"reason": "operator rollback"}))
        .await;
    rollback.assert_status_ok();
    let rollback_payload: serde_json::Value = rollback.json();
    assert_eq!(
        rollback_payload["rollback"]["rolled_back_candidate_id"],
        candidate_id
    );
    assert!(rollback_payload["rollback"]["restored_policy"].is_null());

    let final_status = auth(server.get("/v1/harness/evolution/status")).await;
    final_status.assert_status_ok();
    let final_payload: serde_json::Value = final_status.json();
    assert!(final_payload["active_policy"].is_null());
}
