use std::str::FromStr;

use sqlx::sqlite::SqliteConnectOptions;
use tempfile::TempDir;
use xiaomaolv::harness::evolution::{
    EvolutionCandidateDraft, EvolutionCandidateStatus, EvolutionCaseAssertions, EvolutionEvalCase,
    EvolutionEvaluationDraft, EvolutionFeedbackDraft, EvolutionGateConfig,
    EvolutionPromotionDecision, EvolutionScorecard, MAX_ENABLED_EVOLUTION_EVAL_CASES, PromptPatch,
};
use xiaomaolv::harness::store::{EvolutionStore, SqliteEvolutionStore};
use xiaomaolv::memory::SqliteMemoryStore;

fn database_url(tmp: &TempDir) -> String {
    format!("sqlite://{}", tmp.path().join("evolution.db").display())
}

fn candidate(id: &str, parent_candidate_id: Option<&str>) -> EvolutionCandidateDraft {
    EvolutionCandidateDraft {
        id: id.to_string(),
        parent_candidate_id: parent_candidate_id.map(str::to_string),
        evidence_fingerprint: None,
        prompt_patch: PromptPatch::new(format!("candidate policy {id}"), 1_000)
            .expect("prompt patch"),
        rationale: format!("improve behavior for {id}"),
        source_trajectory_ids: vec![format!("traj-{id}")],
    }
}

#[tokio::test]
async fn existing_database_is_migrated_with_the_evidence_fingerprint_column() {
    let tmp = TempDir::new().expect("tempdir");
    let url = database_url(&tmp);
    let options = SqliteConnectOptions::from_str(&url)
        .expect("sqlite options")
        .create_if_missing(true);
    let pool = sqlx::SqlitePool::connect_with(options)
        .await
        .expect("legacy pool");
    sqlx::query(
        "CREATE TABLE evolution_candidates (
            id TEXT PRIMARY KEY,
            parent_candidate_id TEXT REFERENCES evolution_candidates(id),
            prompt_patch TEXT NOT NULL,
            rationale TEXT NOT NULL,
            source_trajectory_ids_json TEXT NOT NULL,
            status TEXT NOT NULL,
            created_at INTEGER NOT NULL DEFAULT (unixepoch()),
            updated_at INTEGER NOT NULL DEFAULT (unixepoch())
        )",
    )
    .execute(&pool)
    .await
    .expect("legacy candidates table");
    pool.close().await;

    let store = SqliteEvolutionStore::new(
        SqliteMemoryStore::new(&url)
            .await
            .expect("migrated memory store"),
    );
    let fingerprint = "b".repeat(64);
    let mut draft = candidate("candidate-after-migration", None);
    draft.evidence_fingerprint = Some(fingerprint.clone());
    let created = store
        .create_candidate(draft, "engine")
        .await
        .expect("candidate after migration");

    assert_eq!(
        created.evidence_fingerprint.as_deref(),
        Some(fingerprint.as_str())
    );
}

async fn approve(store: &SqliteEvolutionStore, id: &str) -> anyhow::Result<()> {
    store
        .transition_candidate(
            id,
            EvolutionCandidateStatus::Draft,
            EvolutionCandidateStatus::Evaluating,
            "engine",
            serde_json::json!({"reason": "evaluation started"}),
        )
        .await?;
    let baseline_candidate_id = store
        .active_policy()
        .await?
        .map(|policy| policy.candidate_id);
    store
        .record_evaluation(EvolutionEvaluationDraft {
            id: format!("evaluation-{id}"),
            candidate_id: id.to_string(),
            baseline_candidate_id,
            scorecard: EvolutionScorecard {
                baseline_score: 0.0,
                candidate_score: 1.0,
                score_delta: 1.0,
                regressions: 0,
                baseline_passed_cases: 0,
                candidate_passed_cases: 1,
                total_cases: 1,
                case_results: vec![],
            },
            decision: EvolutionPromotionDecision::Ready,
            gate_config: EvolutionGateConfig {
                min_eval_cases: 1,
                ..Default::default()
            },
        })
        .await?;
    store
        .transition_candidate(
            id,
            EvolutionCandidateStatus::Evaluating,
            EvolutionCandidateStatus::Ready,
            "engine",
            serde_json::json!({"reason": "gates passed"}),
        )
        .await?;
    store
        .transition_candidate(
            id,
            EvolutionCandidateStatus::Ready,
            EvolutionCandidateStatus::Approved,
            "operator:henry",
            serde_json::json!({"reason": "reviewed"}),
        )
        .await?;
    Ok(())
}

#[tokio::test]
async fn sqlite_store_persists_candidate_state_and_rejects_stale_transition() {
    let tmp = TempDir::new().expect("tempdir");
    let url = database_url(&tmp);
    let memory = SqliteMemoryStore::new(&url).await.expect("memory store");
    let store = SqliteEvolutionStore::new(memory);

    let created = store
        .create_candidate(candidate("candidate-a", None), "engine")
        .await
        .expect("create candidate");
    assert_eq!(created.status, EvolutionCandidateStatus::Draft);
    assert_eq!(created.source_trajectory_ids, vec!["traj-candidate-a"]);

    store
        .transition_candidate(
            "candidate-a",
            EvolutionCandidateStatus::Draft,
            EvolutionCandidateStatus::Evaluating,
            "engine",
            serde_json::json!({}),
        )
        .await
        .expect("begin evaluation");

    let stale = store
        .transition_candidate(
            "candidate-a",
            EvolutionCandidateStatus::Draft,
            EvolutionCandidateStatus::Rejected,
            "engine",
            serde_json::json!({}),
        )
        .await
        .expect_err("compare-and-set transition must reject stale state");
    assert!(stale.to_string().contains("expected status 'draft'"));

    drop(store);
    let reopened = SqliteEvolutionStore::new(
        SqliteMemoryStore::new(&url)
            .await
            .expect("reopen memory store"),
    );
    let persisted = reopened
        .get_candidate("candidate-a")
        .await
        .expect("load candidate")
        .expect("candidate exists");
    assert_eq!(persisted.status, EvolutionCandidateStatus::Evaluating);
}

#[tokio::test]
async fn evidence_fingerprint_is_globally_unique_and_queryable() {
    let store = SqliteEvolutionStore::new(
        SqliteMemoryStore::new("sqlite::memory:")
            .await
            .expect("memory store"),
    );
    let fingerprint = "a".repeat(64);
    let mut first = candidate("candidate-first", None);
    first.evidence_fingerprint = Some(fingerprint.clone());
    store
        .create_candidate(first, "engine")
        .await
        .expect("first candidate");

    store
        .create_candidate(candidate("candidate-parent", None), "engine")
        .await
        .expect("parent candidate");
    let mut duplicate = candidate("candidate-duplicate", Some("candidate-parent"));
    duplicate.evidence_fingerprint = Some(fingerprint.clone());
    store
        .create_candidate(duplicate, "engine")
        .await
        .expect_err("duplicate evidence must be rejected by SQLite");

    let found = store
        .find_candidate_by_evidence(&fingerprint)
        .await
        .expect("find candidate")
        .expect("candidate exists");
    assert_eq!(found.id, "candidate-first");
    assert_eq!(
        found.evidence_fingerprint.as_deref(),
        Some(fingerprint.as_str())
    );
    assert_eq!(
        store
            .list_candidates(10)
            .await
            .expect("list candidates")
            .len(),
        2
    );
}

#[tokio::test]
async fn activation_is_singleton_and_rollback_restores_previous_policy_after_restart() {
    let tmp = TempDir::new().expect("tempdir");
    let url = database_url(&tmp);
    let store =
        SqliteEvolutionStore::new(SqliteMemoryStore::new(&url).await.expect("memory store"));

    store
        .create_candidate(candidate("candidate-a", None), "engine")
        .await
        .expect("candidate a");
    approve(&store, "candidate-a").await.expect("approve a");
    let deployment_a = store
        .activate_candidate("candidate-a", "operator:henry", "first rollout")
        .await
        .expect("activate a");
    assert!(deployment_a.previous_deployment_id.is_none());

    store
        .create_candidate(candidate("candidate-b", Some("candidate-a")), "engine")
        .await
        .expect("candidate b");
    approve(&store, "candidate-b").await.expect("approve b");
    let deployment_b = store
        .activate_candidate("candidate-b", "operator:henry", "second rollout")
        .await
        .expect("activate b");
    assert_eq!(
        deployment_b.previous_deployment_id.as_deref(),
        Some(deployment_a.id.as_str())
    );
    assert_eq!(
        store
            .active_policy()
            .await
            .expect("active policy")
            .expect("candidate b active")
            .candidate_id,
        "candidate-b"
    );

    drop(store);
    let reopened = SqliteEvolutionStore::new(
        SqliteMemoryStore::new(&url)
            .await
            .expect("reopen memory store"),
    );
    let rollback = reopened
        .rollback_active("operator:henry", "quality regression")
        .await
        .expect("rollback");
    assert_eq!(rollback.rolled_back_candidate_id, "candidate-b");
    assert_eq!(
        rollback
            .restored_policy
            .as_ref()
            .expect("candidate a restored")
            .candidate_id,
        "candidate-a"
    );
    assert_eq!(
        reopened
            .active_policy()
            .await
            .expect("active policy")
            .expect("restored policy")
            .candidate_id,
        "candidate-a"
    );

    let events = reopened.list_audit_events(100).await.expect("audit events");
    assert!(
        events
            .iter()
            .any(|event| event.event_type == "candidate_approved")
    );
    assert!(
        events
            .iter()
            .any(|event| event.event_type == "candidate_activated")
    );
    assert!(
        events
            .iter()
            .any(|event| event.event_type == "deployment_rolled_back")
    );
}

#[tokio::test]
async fn eval_cases_and_scorecards_round_trip_through_public_store_interface() {
    let memory = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("memory store");
    let store = SqliteEvolutionStore::new(memory);
    store
        .create_candidate(candidate("candidate-eval", None), "engine")
        .await
        .expect("candidate");

    let case = EvolutionEvalCase {
        id: "json-case".to_string(),
        name: "returns json".to_string(),
        input: "Return a JSON object".to_string(),
        assertions: EvolutionCaseAssertions {
            required_substrings: vec!["ok".to_string()],
            forbidden_substrings: vec!["secret".to_string()],
            require_json: true,
        },
        weight: 2.0,
        enabled: true,
    };
    store
        .upsert_eval_case(case.clone(), "operator:henry")
        .await
        .expect("upsert eval case");
    assert_eq!(
        store.list_eval_cases(true).await.expect("list eval cases"),
        vec![case]
    );

    let scorecard = EvolutionScorecard {
        baseline_score: 0.0,
        candidate_score: 1.0,
        score_delta: 1.0,
        regressions: 0,
        baseline_passed_cases: 0,
        candidate_passed_cases: 1,
        total_cases: 1,
        case_results: vec![],
    };
    let evaluation = store
        .record_evaluation(EvolutionEvaluationDraft {
            id: "evaluation-a".to_string(),
            candidate_id: "candidate-eval".to_string(),
            baseline_candidate_id: None,
            scorecard: scorecard.clone(),
            decision: EvolutionPromotionDecision::Ready,
            gate_config: EvolutionGateConfig::default(),
        })
        .await
        .expect("record evaluation");
    assert_eq!(evaluation.scorecard, scorecard);
    store
        .record_evaluation(EvolutionEvaluationDraft {
            id: "evaluation-0-newer-but-lexically-smaller".to_string(),
            candidate_id: "candidate-eval".to_string(),
            baseline_candidate_id: None,
            scorecard,
            decision: EvolutionPromotionDecision::Rejected {
                reasons: vec!["newer decision".to_string()],
            },
            gate_config: EvolutionGateConfig::default(),
        })
        .await
        .expect("record newer evaluation");
    assert_eq!(
        store
            .latest_evaluation("candidate-eval")
            .await
            .expect("latest evaluation")
            .expect("evaluation exists")
            .id,
        "evaluation-0-newer-but-lexically-smaller"
    );
}

#[tokio::test]
async fn enabled_eval_suite_has_a_bounded_size() {
    let store = SqliteEvolutionStore::new(
        SqliteMemoryStore::new("sqlite::memory:")
            .await
            .expect("memory store"),
    );
    for index in 0..MAX_ENABLED_EVOLUTION_EVAL_CASES {
        store
            .upsert_eval_case(
                EvolutionEvalCase {
                    id: format!("case-{index}"),
                    name: format!("case {index}"),
                    input: "Return pass".to_string(),
                    assertions: EvolutionCaseAssertions {
                        required_substrings: vec!["pass".to_string()],
                        ..Default::default()
                    },
                    weight: 1.0,
                    enabled: true,
                },
                "operator:henry",
            )
            .await
            .expect("bounded eval case");
    }
    let overflow = store
        .upsert_eval_case(
            EvolutionEvalCase {
                id: "overflow".to_string(),
                name: "overflow".to_string(),
                input: "Return pass".to_string(),
                assertions: EvolutionCaseAssertions {
                    required_substrings: vec!["pass".to_string()],
                    ..Default::default()
                },
                weight: 1.0,
                enabled: true,
            },
            "operator:henry",
        )
        .await
        .expect_err("enabled suite must be bounded");
    assert!(overflow.to_string().contains("cannot exceed"));
}

#[tokio::test]
async fn trajectory_feedback_is_validated_persisted_and_queryable_as_negative_evidence() {
    let memory = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("memory store");
    memory
        .start_trajectory(
            "traj-feedback",
            "session-feedback",
            "http",
            "user-a",
            "model-a",
        )
        .await
        .expect("start trajectory");
    memory
        .finish_trajectory(
            "traj-feedback",
            Some("plausible but wrong".to_string()),
            xiaomaolv::harness::trajectory::TrajectoryExitReason::FinalAnswer,
        )
        .await
        .expect("finish trajectory");
    let store = SqliteEvolutionStore::new(memory);

    let invalid = store
        .record_feedback(
            EvolutionFeedbackDraft {
                trajectory_id: "traj-feedback".to_string(),
                score: -2.0,
                tags: vec!["incorrect".to_string()],
                comment: None,
            },
            "human:henry",
        )
        .await
        .expect_err("feedback score must be bounded");
    assert!(invalid.to_string().contains("between -1 and 1"));

    let feedback = store
        .record_feedback(
            EvolutionFeedbackDraft {
                trajectory_id: "traj-feedback".to_string(),
                score: -1.0,
                tags: vec!["incorrect".to_string(), "missing-source".to_string()],
                comment: Some("The answer omitted the primary evidence.".to_string()),
            },
            "human:henry",
        )
        .await
        .expect("record feedback");
    assert_eq!(feedback.trajectory_id, "traj-feedback");
    assert_eq!(feedback.score, -1.0);

    let negative = store
        .list_feedback(true, 20)
        .await
        .expect("list negative feedback");
    assert_eq!(negative, vec![feedback]);
}
