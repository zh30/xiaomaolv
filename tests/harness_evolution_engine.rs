use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use xiaomaolv::domain::StoredMessage;
use xiaomaolv::harness::evolution::{
    EvolutionActor, EvolutionCandidateStatus, EvolutionCaseAssertions, EvolutionEngine,
    EvolutionEvalCase, EvolutionFeedbackDraft, EvolutionGateConfig, EvolutionPromotionDecision,
};
use xiaomaolv::harness::store::{
    EvolutionStore, HarnessStore, SqliteEvolutionStore, SqliteHarnessStore,
};
use xiaomaolv::harness::trajectory::{ToolCallRecord, TrajectoryExitReason};
use xiaomaolv::memory::SqliteMemoryStore;
use xiaomaolv::provider::{ChatProvider, CompletionRequest};

struct InvalidProposalProvider;

#[async_trait]
impl ChatProvider for InvalidProposalProvider {
    async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
        Ok("not-json".to_string())
    }
}

struct BarrierProposalProvider {
    barrier: Arc<tokio::sync::Barrier>,
}

#[async_trait]
impl ChatProvider for BarrierProposalProvider {
    async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
        self.barrier.wait().await;
        Ok(serde_json::json!({
            "prompt_patch": "candidate wins",
            "rationale": "deduplicate shared failure evidence"
        })
        .to_string())
    }
}

#[derive(Default)]
struct FakeEvolutionProvider {
    requests: Mutex<Vec<Vec<StoredMessage>>>,
}

impl FakeEvolutionProvider {
    fn request_count(&self) -> usize {
        self.requests.lock().expect("requests mutex").len()
    }
}

#[async_trait]
impl ChatProvider for FakeEvolutionProvider {
    fn model_name(&self) -> Option<&str> {
        Some("fake-evolution-model")
    }

    async fn complete(&self, req: CompletionRequest) -> anyhow::Result<String> {
        let is_proposal = req
            .messages
            .iter()
            .any(|message| message.content.contains("SELF_EVOLUTION_PROPOSAL_JSON"));
        let candidate_policy = req
            .messages
            .iter()
            .any(|message| message.content.contains("candidate wins"));
        self.requests
            .lock()
            .expect("requests mutex")
            .push(req.messages);

        if is_proposal {
            return Ok(serde_json::json!({
                "prompt_patch": "candidate wins",
                "rationale": "failed trajectories need the passing policy"
            })
            .to_string());
        }
        if candidate_policy {
            Ok("pass".to_string())
        } else {
            Ok("baseline miss".to_string())
        }
    }
}

fn gate_config() -> EvolutionGateConfig {
    EvolutionGateConfig {
        min_eval_cases: 3,
        min_candidate_score: 1.0,
        min_score_delta: 0.5,
        max_regressions: 0,
        max_prompt_patch_chars: 1_000,
        require_human_approval: true,
    }
}

async fn seed_eval_cases(store: &SqliteEvolutionStore) -> anyhow::Result<()> {
    for id in ["accuracy", "format", "safety"] {
        store
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
                "operator:henry",
            )
            .await?;
    }
    Ok(())
}

async fn seed_failed_trajectory(
    store: &SqliteHarnessStore,
    trajectory_id: &str,
) -> anyhow::Result<()> {
    HarnessStore::start_trajectory(
        store,
        trajectory_id,
        "session-a",
        "http",
        "user-a",
        "fake-model",
    )
    .await?;
    HarnessStore::insert_trajectory_tool_call(
        store,
        trajectory_id,
        ToolCallRecord {
            call_index: 0,
            server: "search".to_string(),
            tool: "lookup".to_string(),
            arguments: serde_json::json!({"q": "test"}),
            result: serde_json::json!({"error": "timeout"}),
            ok: false,
            duration_ms: 1_500,
            iteration: 0,
        },
    )
    .await?;
    HarnessStore::finish_trajectory(
        store,
        trajectory_id,
        Some("fallback".to_string()),
        TrajectoryExitReason::ToolError,
    )
    .await
}

#[tokio::test]
async fn bounded_evaluation_rejects_cumulative_provider_output_over_budget() {
    let memory = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("memory store");
    let store = Arc::new(SqliteEvolutionStore::new(memory));
    seed_eval_cases(&store).await.expect("seed eval cases");
    let provider = Arc::new(FakeEvolutionProvider::default());
    let engine = EvolutionEngine::new(store, provider, gate_config())
        .await
        .expect("evolution engine");
    let candidate = engine
        .create_candidate(
            "candidate wins",
            "exercise bounded evaluation",
            vec![],
            &EvolutionActor::System("test".to_string()),
        )
        .await
        .expect("candidate");

    let error = engine
        .evaluate_candidate_bounded(&candidate.id, 8)
        .await
        .expect_err("combined baseline/candidate output should exceed eight bytes");
    assert!(error.to_string().contains("response byte budget"));
}

#[tokio::test]
async fn engine_evaluates_approves_activates_and_rolls_back_with_human_gate() {
    let memory = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("memory store");
    let store = Arc::new(SqliteEvolutionStore::new(memory));
    seed_eval_cases(&store).await.expect("seed eval cases");
    let provider = Arc::new(FakeEvolutionProvider::default());
    let engine = EvolutionEngine::new(store.clone(), provider.clone(), gate_config())
        .await
        .expect("evolution engine");

    let candidate = engine
        .create_candidate(
            "candidate wins",
            "improve the deterministic suite",
            vec![],
            &EvolutionActor::System("evolution-engine".to_string()),
        )
        .await
        .expect("candidate");
    let evaluation = engine
        .evaluate_candidate(&candidate.id)
        .await
        .expect("evaluate candidate");
    assert_eq!(evaluation.decision, EvolutionPromotionDecision::Ready);
    assert_eq!(evaluation.scorecard.baseline_score, 0.0);
    assert_eq!(evaluation.scorecard.candidate_score, 1.0);
    assert_eq!(provider.request_count(), 6);
    assert!(
        provider
            .requests
            .lock()
            .expect("requests mutex")
            .iter()
            .flatten()
            .any(|message| {
                message.content.contains("ACTIVE_EVOLUTION_POLICY")
                    && message.content.contains("deployment_id=shadow-evaluation")
            })
    );
    assert_eq!(
        store
            .get_candidate(&candidate.id)
            .await
            .expect("candidate query")
            .expect("candidate exists")
            .status,
        EvolutionCandidateStatus::Ready
    );

    let system_approval = engine
        .approve_candidate(
            &candidate.id,
            &EvolutionActor::System("evolution-engine".to_string()),
            "automatic approval",
        )
        .await
        .expect_err("human approval is required");
    assert!(system_approval.to_string().contains("human actor"));

    let human = EvolutionActor::Human("henry".to_string());
    engine
        .approve_candidate(&candidate.id, &human, "reviewed scorecard")
        .await
        .expect("approve candidate");
    let deployment = engine
        .activate_candidate(&candidate.id, &human, "controlled rollout")
        .await
        .expect("activate candidate");
    let active = engine
        .policy_runtime()
        .active()
        .await
        .expect("active runtime policy");
    assert_eq!(active.deployment_id, deployment.id);
    assert_eq!(active.prompt_patch.as_str(), "candidate wins");

    let rollback = engine
        .rollback_active(&human, "observed regression")
        .await
        .expect("rollback active policy");
    assert_eq!(rollback.rolled_back_candidate_id, candidate.id);
    assert!(rollback.restored_policy.is_none());
    assert!(engine.policy_runtime().active().await.is_none());
}

#[tokio::test]
async fn operator_can_abandon_an_interrupted_evaluation_and_retry_it() {
    let memory = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("memory store");
    let store = Arc::new(SqliteEvolutionStore::new(memory));
    seed_eval_cases(&store).await.expect("seed eval cases");
    let engine = EvolutionEngine::new(
        store.clone(),
        Arc::new(FakeEvolutionProvider::default()),
        gate_config(),
    )
    .await
    .expect("evolution engine");
    let candidate = engine
        .create_candidate(
            "candidate wins",
            "recover an interrupted evaluation",
            vec![],
            &EvolutionActor::System("evolution-engine".to_string()),
        )
        .await
        .expect("candidate");
    store
        .transition_candidate(
            &candidate.id,
            EvolutionCandidateStatus::Draft,
            EvolutionCandidateStatus::Evaluating,
            "system:evolution-engine",
            serde_json::json!({"reason": "simulated process exit"}),
        )
        .await
        .expect("mark evaluating");

    engine
        .abandon_evaluation(
            &candidate.id,
            &EvolutionActor::System("evolution-engine".to_string()),
            "automatic recovery",
        )
        .await
        .expect_err("abandonment requires a human");
    let abandoned = engine
        .abandon_evaluation(
            &candidate.id,
            &EvolutionActor::Human("henry".to_string()),
            "the prior process exited",
        )
        .await
        .expect("abandon stale evaluation");
    assert_eq!(abandoned.status, EvolutionCandidateStatus::Failed);

    let evaluation = engine
        .evaluate_candidate(&candidate.id)
        .await
        .expect("retry failed candidate");
    assert_eq!(evaluation.decision, EvolutionPromotionDecision::Ready);
    assert_eq!(
        store
            .get_candidate(&candidate.id)
            .await
            .expect("candidate query")
            .expect("candidate")
            .status,
        EvolutionCandidateStatus::Ready
    );
}

#[tokio::test]
async fn stale_baselines_block_approval_and_activation() {
    let memory = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("memory store");
    let store = Arc::new(SqliteEvolutionStore::new(memory));
    seed_eval_cases(&store).await.expect("seed eval cases");
    let engine = EvolutionEngine::new(
        store,
        Arc::new(FakeEvolutionProvider::default()),
        gate_config(),
    )
    .await
    .expect("evolution engine");
    let human = EvolutionActor::Human("henry".to_string());

    let stale_unapproved = engine
        .create_candidate(
            "candidate wins",
            "candidate awaiting approval",
            vec![],
            &human,
        )
        .await
        .expect("stale unapproved candidate");
    engine
        .evaluate_candidate(&stale_unapproved.id)
        .await
        .expect("evaluate stale unapproved candidate");

    let stale_approved = engine
        .create_candidate(
            "candidate wins",
            "candidate approved before baseline changes",
            vec![],
            &human,
        )
        .await
        .expect("stale approved candidate");
    engine
        .evaluate_candidate(&stale_approved.id)
        .await
        .expect("evaluate stale approved candidate");
    engine
        .approve_candidate(
            &stale_approved.id,
            &human,
            "approved against built-in baseline",
        )
        .await
        .expect("approve stale candidate before activation race");

    let winner = engine
        .create_candidate(
            "candidate wins",
            "candidate activated first",
            vec![],
            &human,
        )
        .await
        .expect("winner candidate");
    engine
        .evaluate_candidate(&winner.id)
        .await
        .expect("evaluate winner");
    engine
        .approve_candidate(&winner.id, &human, "winner reviewed")
        .await
        .expect("approve winner");
    engine
        .activate_candidate(&winner.id, &human, "winner rollout")
        .await
        .expect("activate winner");

    let stale_approval = engine
        .approve_candidate(&stale_unapproved.id, &human, "attempt stale approval")
        .await
        .expect_err("approval must use the current baseline");
    assert!(stale_approval.to_string().contains("stale baseline"));

    let stale_activation = engine
        .activate_candidate(&stale_approved.id, &human, "attempt stale activation")
        .await
        .expect_err("activation must recheck the baseline transactionally");
    assert!(stale_activation.to_string().contains("stale baseline"));
    assert_eq!(
        engine
            .active_policy()
            .await
            .expect("winner remains active")
            .candidate_id,
        winner.id
    );
}

#[tokio::test]
async fn autonomous_cycle_discovers_failed_trajectory_then_stops_at_ready() {
    let memory = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("memory store");
    let store = Arc::new(SqliteEvolutionStore::new(memory.clone()));
    let harness_store = Arc::new(SqliteHarnessStore::new(memory));
    seed_eval_cases(&store).await.expect("seed eval cases");

    HarnessStore::start_trajectory(
        harness_store.as_ref(),
        "traj-failure",
        "session-a",
        "http",
        "user-a",
        "fake-model",
    )
    .await
    .expect("start trajectory");
    HarnessStore::insert_trajectory_tool_call(
        harness_store.as_ref(),
        "traj-failure",
        ToolCallRecord {
            call_index: 0,
            server: "search".to_string(),
            tool: "lookup".to_string(),
            arguments: serde_json::json!({"q": "test"}),
            result: serde_json::json!({"error": "timeout"}),
            ok: false,
            duration_ms: 1_500,
            iteration: 0,
        },
    )
    .await
    .expect("record failed call");
    HarnessStore::finish_trajectory(
        harness_store.as_ref(),
        "traj-failure",
        Some("fallback".to_string()),
        TrajectoryExitReason::ToolError,
    )
    .await
    .expect("finish trajectory");

    let provider = Arc::new(FakeEvolutionProvider::default());
    let engine = EvolutionEngine::new(store.clone(), provider, gate_config())
        .await
        .expect("evolution engine")
        .with_harness_store(harness_store);
    let cycle = engine.run_cycle().await.expect("evolution cycle");

    assert_eq!(cycle.candidate.source_trajectory_ids, vec!["traj-failure"]);
    assert_eq!(cycle.candidate.status, EvolutionCandidateStatus::Ready);
    assert_eq!(cycle.evaluation.decision, EvolutionPromotionDecision::Ready);
    assert!(
        store
            .active_policy()
            .await
            .expect("active policy")
            .is_none()
    );

    let duplicate = engine
        .run_cycle()
        .await
        .expect_err("the same evidence must not generate duplicate candidates");
    assert!(duplicate.to_string().contains("no new failure evidence"));
    let status = engine.cycle_status().await;
    assert_eq!(status.last_outcome.as_deref(), Some("skipped"));
    assert!(status.last_error.is_none());
    assert!(
        status
            .last_skip_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("no new failure evidence"))
    );
}

#[tokio::test]
async fn negative_feedback_promotes_a_normal_final_answer_into_cycle_evidence() {
    let memory = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("memory store");
    let store = Arc::new(SqliteEvolutionStore::new(memory.clone()));
    let harness_store = Arc::new(SqliteHarnessStore::new(memory));
    seed_eval_cases(&store).await.expect("seed eval cases");

    HarnessStore::start_trajectory(
        harness_store.as_ref(),
        "traj-user-rejected",
        "session-feedback",
        "http",
        "user-a",
        "fake-model",
    )
    .await
    .expect("start trajectory");
    HarnessStore::finish_trajectory(
        harness_store.as_ref(),
        "traj-user-rejected",
        Some("technically completed but factually wrong".to_string()),
        TrajectoryExitReason::FinalAnswer,
    )
    .await
    .expect("finish trajectory");
    store
        .record_feedback(
            EvolutionFeedbackDraft {
                trajectory_id: "traj-user-rejected".to_string(),
                score: -1.0,
                tags: vec!["incorrect".to_string()],
                comment: Some("The factual claim was wrong.".to_string()),
            },
            "human:henry",
        )
        .await
        .expect("record feedback");

    let engine = EvolutionEngine::new(
        store,
        Arc::new(FakeEvolutionProvider::default()),
        gate_config(),
    )
    .await
    .expect("evolution engine")
    .with_harness_store(harness_store);
    let cycle = engine.run_cycle().await.expect("feedback-driven cycle");

    assert_eq!(
        cycle.candidate.source_trajectory_ids,
        vec!["traj-user-rejected"]
    );
    assert_eq!(cycle.candidate.status, EvolutionCandidateStatus::Ready);
}

#[tokio::test]
async fn new_feedback_changes_the_evidence_snapshot_for_the_same_trajectory() {
    let memory = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("memory store");
    let store = Arc::new(SqliteEvolutionStore::new(memory.clone()));
    let harness_store = Arc::new(SqliteHarnessStore::new(memory));
    HarnessStore::start_trajectory(
        harness_store.as_ref(),
        "traj-feedback-version",
        "session-feedback",
        "http",
        "user-a",
        "fake-model",
    )
    .await
    .expect("start trajectory");
    HarnessStore::finish_trajectory(
        harness_store.as_ref(),
        "traj-feedback-version",
        Some("completed but disliked".to_string()),
        TrajectoryExitReason::FinalAnswer,
    )
    .await
    .expect("finish trajectory");
    store
        .record_feedback(
            EvolutionFeedbackDraft {
                trajectory_id: "traj-feedback-version".to_string(),
                score: -0.5,
                tags: vec!["style".to_string()],
                comment: Some("Too verbose.".to_string()),
            },
            "human:henry",
        )
        .await
        .expect("first feedback");
    let engine = EvolutionEngine::new(
        store.clone(),
        Arc::new(FakeEvolutionProvider::default()),
        gate_config(),
    )
    .await
    .expect("evolution engine")
    .with_harness_store(harness_store);

    let first = engine
        .propose_from_trajectories()
        .await
        .expect("first proposal");
    store
        .transition_candidate(
            &first.id,
            EvolutionCandidateStatus::Draft,
            EvolutionCandidateStatus::Rejected,
            "human:henry",
            serde_json::json!({"reason": "finish the first proposal for dedup testing"}),
        )
        .await
        .expect("finish first proposal");
    engine
        .propose_from_trajectories()
        .await
        .expect_err("unchanged evidence must be skipped");
    store
        .record_feedback(
            EvolutionFeedbackDraft {
                trajectory_id: "traj-feedback-version".to_string(),
                score: -1.0,
                tags: vec!["incorrect".to_string()],
                comment: Some("It also contains a factual error.".to_string()),
            },
            "human:henry",
        )
        .await
        .expect("second feedback");
    let second = engine
        .propose_from_trajectories()
        .await
        .expect("new feedback must create new evidence");

    assert_eq!(first.source_trajectory_ids, second.source_trajectory_ids);
    assert_ne!(first.evidence_fingerprint, second.evidence_fingerprint);
    assert_eq!(
        store.list_candidates(10).await.expect("candidates").len(),
        2
    );
}

#[tokio::test]
async fn cycle_resumes_a_persisted_draft_after_interruption() {
    let memory = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("memory store");
    let store = Arc::new(SqliteEvolutionStore::new(memory.clone()));
    let harness_store = Arc::new(SqliteHarnessStore::new(memory));
    seed_eval_cases(&store).await.expect("seed eval cases");
    seed_failed_trajectory(harness_store.as_ref(), "traj-resume-draft")
        .await
        .expect("seed failed trajectory");
    let engine = EvolutionEngine::new(
        store.clone(),
        Arc::new(FakeEvolutionProvider::default()),
        gate_config(),
    )
    .await
    .expect("evolution engine")
    .with_harness_store(harness_store);

    let draft = engine
        .propose_from_trajectories()
        .await
        .expect("persist proposal before simulated interruption");
    assert_eq!(draft.status, EvolutionCandidateStatus::Draft);
    let cycle = engine.run_cycle().await.expect("resume draft cycle");

    assert_eq!(cycle.candidate.id, draft.id);
    assert_eq!(cycle.candidate.status, EvolutionCandidateStatus::Ready);
    let events = store.list_audit_events(50).await.expect("audit events");
    assert!(events.iter().any(|event| {
        event.event_type == "proposal_resumed"
            && event.candidate_id.as_deref() == Some(draft.id.as_str())
    }));
}

#[tokio::test]
async fn proposal_failure_is_audited_and_visible_in_cycle_status() {
    let memory = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("memory store");
    let store = Arc::new(SqliteEvolutionStore::new(memory.clone()));
    let harness_store = Arc::new(SqliteHarnessStore::new(memory));
    seed_eval_cases(&store).await.expect("seed eval cases");
    seed_failed_trajectory(harness_store.as_ref(), "traj-invalid-proposal")
        .await
        .expect("seed failed trajectory");
    let engine = EvolutionEngine::new(
        store.clone(),
        Arc::new(InvalidProposalProvider),
        gate_config(),
    )
    .await
    .expect("evolution engine")
    .with_harness_store(harness_store);

    let error = engine
        .run_cycle()
        .await
        .expect_err("invalid proposal must fail the cycle");
    assert!(error.to_string().contains("not valid JSON"));
    let status = engine.cycle_status().await;
    assert!(!status.running);
    assert_eq!(status.last_outcome.as_deref(), Some("failed"));
    assert!(
        status
            .last_error
            .as_deref()
            .is_some_and(|message| message.contains("not valid JSON"))
    );

    let events = store.list_audit_events(20).await.expect("audit events");
    assert!(
        events
            .iter()
            .any(|event| event.event_type == "proposal_started")
    );
    let failure = events
        .iter()
        .find(|event| event.event_type == "proposal_failed")
        .expect("proposal failure audit");
    assert_eq!(failure.details["stage"], "invalid_response");
    assert_eq!(
        failure.details["evidence_fingerprint"]
            .as_str()
            .map(str::len),
        Some(64)
    );
}

#[tokio::test]
async fn concurrent_engines_create_only_one_candidate_for_shared_evidence() {
    let memory = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("memory store");
    let store = Arc::new(SqliteEvolutionStore::new(memory.clone()));
    let harness_store = Arc::new(SqliteHarnessStore::new(memory));
    seed_failed_trajectory(harness_store.as_ref(), "traj-shared-evidence")
        .await
        .expect("seed failed trajectory");
    let provider = Arc::new(BarrierProposalProvider {
        barrier: Arc::new(tokio::sync::Barrier::new(2)),
    });
    let first = EvolutionEngine::new(store.clone(), provider.clone(), gate_config())
        .await
        .expect("first engine")
        .with_harness_store(harness_store.clone());
    let second = EvolutionEngine::new(store.clone(), provider, gate_config())
        .await
        .expect("second engine")
        .with_harness_store(harness_store);

    let (first_result, second_result) = tokio::join!(
        first.propose_from_trajectories(),
        second.propose_from_trajectories()
    );
    assert_ne!(first_result.is_ok(), second_result.is_ok());
    let candidates = store.list_candidates(10).await.expect("list candidates");
    assert_eq!(candidates.len(), 1);
    assert!(candidates[0].evidence_fingerprint.is_some());
    let events = store.list_audit_events(20).await.expect("audit events");
    assert!(
        events
            .iter()
            .any(|event| event.event_type == "proposal_deduplicated")
    );
}
