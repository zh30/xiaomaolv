use std::sync::Arc;

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use xiaomaolv::harness::evolution::{
    EvolutionActor, EvolutionCaseAssertions, EvolutionEngine, EvolutionEvalCase,
    EvolutionGateConfig,
};
use xiaomaolv::harness::loop_engine::{LoopStore, SignalKind, SqliteLoopStore};
use xiaomaolv::harness::store::{
    EvolutionStore, HarnessStore, SqliteEvolutionStore, SqliteHarnessStore,
};
use xiaomaolv::harness::trajectory::{
    RunScorecardFilter, ToolCallRecord, TrajectoryExitReason, TrajectoryRecord,
    compute_run_scorecard,
};
use xiaomaolv::memory::SqliteMemoryStore;
use xiaomaolv::provider::{ChatProvider, CompletionRequest};

struct ScoreProvider;

#[async_trait]
impl ChatProvider for ScoreProvider {
    fn model_name(&self) -> Option<&str> {
        Some("score-provider")
    }

    async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
        Ok("pass".to_string())
    }
}

fn trajectory(
    id: &str,
    started: i64,
    finished: i64,
    exit_reason: TrajectoryExitReason,
    final_answer: Option<&str>,
    tool_calls: Vec<ToolCallRecord>,
) -> TrajectoryRecord {
    trajectory_in_session(
        "session",
        id,
        started,
        finished,
        exit_reason,
        final_answer,
        tool_calls,
    )
}

fn trajectory_in_session(
    session_id: &str,
    id: &str,
    started: i64,
    finished: i64,
    exit_reason: TrajectoryExitReason,
    final_answer: Option<&str>,
    tool_calls: Vec<ToolCallRecord>,
) -> TrajectoryRecord {
    TrajectoryRecord {
        id: id.to_string(),
        session_id: session_id.to_string(),
        channel: "telegram".to_string(),
        user_id: "user".to_string(),
        started_at: Utc.timestamp_opt(started, 0).unwrap(),
        finished_at: Some(Utc.timestamp_opt(finished, 0).unwrap()),
        tool_calls,
        final_answer: final_answer.map(str::to_string),
        exit_reason,
        model: "model".to_string(),
        total_tokens: None,
    }
}

fn tool_call(ok: bool) -> ToolCallRecord {
    ToolCallRecord {
        call_index: 0,
        server: "srv".to_string(),
        tool: "tool".to_string(),
        arguments: serde_json::json!({}),
        result: serde_json::json!({}),
        ok,
        duration_ms: 10,
        iteration: 0,
    }
}

#[test]
fn scorecard_rewards_clean_completion_and_penalizes_failures() {
    let clean = compute_run_scorecard(
        &trajectory(
            "t1",
            1_000,
            1_005,
            TrajectoryExitReason::FinalAnswer,
            Some("ok"),
            vec![],
        ),
        1_005,
    );
    assert_eq!(clean.score, 100);

    // MaxIterations completion weight (0.4) drags the score below a full
    // completion even with a good answer and no tool errors.
    let capped = compute_run_scorecard(
        &trajectory(
            "t2",
            1_000,
            1_005,
            TrajectoryExitReason::MaxIterations,
            Some("partial"),
            vec![],
        ),
        1_005,
    );
    assert_eq!(capped.score, 64);

    // A failing tool call plus an error exit lands far below a clean run.
    let broken = compute_run_scorecard(
        &trajectory(
            "t3",
            1_000,
            1_005,
            TrajectoryExitReason::ToolError,
            None,
            vec![tool_call(false), tool_call(true)],
        ),
        1_005,
    );
    assert_eq!(broken.tool_health, 0.5);
    assert_eq!(broken.failed_tool_calls, 1);
    // 0.1*60 + 0 + 0.5*15 + 1.0*15 - 4 = 24.5 -> 25 (fast, no answer)
    assert_eq!(broken.score, 25);

    // Latency past five minutes zeroes the speed component.
    let slow = compute_run_scorecard(
        &trajectory(
            "t4",
            1_000,
            1_400,
            TrajectoryExitReason::FinalAnswer,
            Some("ok"),
            vec![],
        ),
        1_400,
    );
    assert_eq!(slow.speed, 0.0);
    assert_eq!(slow.score, 85);
}

#[tokio::test]
async fn finish_persists_scorecard_and_queries_window() -> anyhow::Result<()> {
    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    memory
        .start_trajectory("traj-1", "s", "telegram", "u", "m")
        .await?;
    memory
        .insert_trajectory_tool_call(
            "traj-1",
            ToolCallRecord {
                call_index: 0,
                server: "srv".to_string(),
                tool: "tool".to_string(),
                arguments: serde_json::json!({}),
                result: serde_json::json!({"verification_failed": true}),
                ok: false,
                duration_ms: 5,
                iteration: 0,
            },
        )
        .await?;
    memory
        .finish_trajectory(
            "traj-1",
            Some("done".to_string()),
            TrajectoryExitReason::FinalAnswer,
        )
        .await?;

    let scorecard = memory
        .get_run_scorecard("traj-1")
        .await?
        .expect("scorecard persisted at finish");
    assert_eq!(scorecard.failed_tool_calls, 1);
    assert_eq!(scorecard.verification_failures, 1);
    assert!(scorecard.score < 100);

    let listed = memory
        .query_run_scorecards(RunScorecardFilter {
            channel: Some("telegram".to_string()),
            limit: 10,
            ..Default::default()
        })
        .await?;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].trajectory_id, "traj-1");

    // Backfill: a finished trajectory without a scorecard is computed lazily.
    memory
        .record_finished_trajectory(&trajectory(
            "traj-legacy",
            1_700_000_000,
            1_700_000_005,
            TrajectoryExitReason::FinalAnswer,
            Some("old"),
            vec![],
        ))
        .await?;
    let legacy = memory
        .get_run_scorecard("traj-legacy")
        .await?
        .expect("legacy scorecard");
    assert_eq!(legacy.score, 100);
    Ok(())
}

#[tokio::test]
async fn scorecard_summary_reports_bands_and_failures() -> anyhow::Result<()> {
    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    for (id, score) in [("a", 95_i64), ("b", 60), ("c", 20)] {
        memory
            .record_finished_trajectory(&trajectory(
                id,
                1_700_000_000,
                1_700_000_005,
                TrajectoryExitReason::FinalAnswer,
                Some("ok"),
                vec![],
            ))
            .await?;
        // Pin the composite score; component fields stay consistent.
        let mut card = memory.get_run_scorecard(id).await?.expect("scorecard");
        card.score = score;
        memory.upsert_run_scorecard(&card).await?;
    }
    let summary = memory
        .run_scorecard_summary(1_699_999_000, 1_700_001_000)
        .await?;
    assert_eq!(summary.run_count, 3);
    assert_eq!(summary.high_count, 1);
    assert_eq!(summary.mid_count, 1);
    assert_eq!(summary.low_count, 1);
    let mean = summary.mean_score.expect("mean");
    assert!((mean - 58.33).abs() < 0.5);
    Ok(())
}

fn gate_config() -> EvolutionGateConfig {
    EvolutionGateConfig {
        min_eval_cases: 1,
        min_candidate_score: 0.0,
        min_score_delta: 0.0,
        max_regressions: 0,
        max_prompt_patch_chars: 1_000,
        require_human_approval: true,
    }
}

/// The deployment review is the measurable close of the self-improvement
/// loop: post-activation scorecards regressing against the baseline emit an
/// auditable event and one deduplicated operator signal.
#[tokio::test]
async fn deployment_regression_emits_audit_and_deduped_signal() -> anyhow::Result<()> {
    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let harness: Arc<dyn HarnessStore> = Arc::new(SqliteHarnessStore::new(memory.clone()));
    let store = Arc::new(SqliteEvolutionStore::new(memory.clone()));
    store
        .upsert_eval_case(
            EvolutionEvalCase {
                id: "case".to_string(),
                name: "case".to_string(),
                input: "input".to_string(),
                assertions: EvolutionCaseAssertions {
                    required_substrings: vec!["pass".to_string()],
                    forbidden_substrings: Vec::new(),
                    require_json: false,
                    ..Default::default()
                },
                weight: 1.0,
                enabled: true,
            },
            "operator:test",
        )
        .await?;
    let engine = EvolutionEngine::new(store.clone(), Arc::new(ScoreProvider), gate_config())
        .await?
        .with_harness_store(harness.clone());

    let candidate = engine
        .create_candidate(
            "patch",
            "improve",
            vec![],
            &EvolutionActor::System("evolution-engine".to_string()),
        )
        .await?;
    engine.evaluate_candidate(&candidate.id).await?;
    let human = EvolutionActor::Human("henry".to_string());
    engine
        .approve_candidate(&candidate.id, &human, "ok")
        .await?;
    let deployment = engine
        .activate_candidate(&candidate.id, &human, "rollout")
        .await?;
    let activated = deployment.activated_at.timestamp();

    // Baseline: strong runs strictly before activation.
    for index in 0..10 {
        memory
            .record_finished_trajectory(&trajectory(
                &format!("baseline-{index}"),
                activated - 3_600 + index * 60,
                activated - 3_540 + index * 60,
                TrajectoryExitReason::FinalAnswer,
                Some("great"),
                vec![],
            ))
            .await?;
    }
    // Post-activation: every run crashes on a failed tool.
    for index in 0..10 {
        let start = activated + 60 + index * 60;
        memory
            .record_finished_trajectory(&trajectory(
                &format!("post-{index}"),
                start,
                start + 5,
                TrajectoryExitReason::ToolError,
                None,
                vec![tool_call(false)],
            ))
            .await?;
    }

    engine.review_deployment_outcome().await?;

    let audit = store.list_audit_events(20).await?;
    assert!(
        audit
            .iter()
            .any(|event| event.event_type == "deployment_regressed")
    );

    let loop_store = SqliteLoopStore::new(memory.clone());
    let signals = LoopStore::list_signals(&loop_store, 20, None).await?;
    let regression_signals: Vec<_> = signals
        .iter()
        .filter(|signal| signal.kind == SignalKind::DeploymentOutcome)
        .collect();
    assert_eq!(regression_signals.len(), 1);
    assert!(
        regression_signals[0].content.contains(&deployment.id),
        "signal names the regressed deployment"
    );

    // The review dedups on the deployment id: a second pass emits nothing.
    engine.review_deployment_outcome().await?;
    let signals = LoopStore::list_signals(&loop_store, 20, None).await?;
    assert_eq!(
        signals
            .iter()
            .filter(|signal| signal.kind == SignalKind::DeploymentOutcome)
            .count(),
        1
    );
    Ok(())
}

/// Low-score runs self-report into the operator signal pipeline; identical
/// failure shapes collapse into one signal through near-duplicate dedup.
#[tokio::test]
async fn low_score_finish_emits_one_deduped_signal() -> anyhow::Result<()> {
    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let harness = SqliteHarnessStore::new(memory.clone());
    let loop_store = SqliteLoopStore::new(memory.clone());

    for id in ["bad-1", "bad-2"] {
        HarnessStore::start_trajectory(&harness, id, "s", "telegram", "u", "m").await?;
        HarnessStore::insert_trajectory_tool_call(
            &harness,
            id,
            ToolCallRecord {
                call_index: 0,
                server: "srv".to_string(),
                tool: "tool".to_string(),
                arguments: serde_json::json!({}),
                result: serde_json::json!({}),
                ok: false,
                duration_ms: 5,
                iteration: 0,
            },
        )
        .await?;
        HarnessStore::finish_trajectory(&harness, id, None, TrajectoryExitReason::ToolError)
            .await?;
    }

    let signals = LoopStore::list_signals(&loop_store, 20, None).await?;
    let emitted: Vec<_> = signals
        .iter()
        .filter(|signal| signal.source == "trajectory:scorecard")
        .collect();
    assert_eq!(
        emitted.len(),
        1,
        "identical failure shapes deduplicate to one signal"
    );
    assert_eq!(
        emitted[0].kind,
        xiaomaolv::harness::loop_engine::SignalKind::Trajectory
    );

    // A clean run reports nothing.
    HarnessStore::start_trajectory(&harness, "good", "s", "telegram", "u", "m").await?;
    HarnessStore::finish_trajectory(
        &harness,
        "good",
        Some("done".to_string()),
        TrajectoryExitReason::FinalAnswer,
    )
    .await?;
    let signals = LoopStore::list_signals(&loop_store, 20, None).await?;
    assert_eq!(
        signals
            .iter()
            .filter(|signal| signal.source == "trajectory:scorecard")
            .count(),
        1
    );
    Ok(())
}

struct RecordingProposalProvider {
    requests: std::sync::Mutex<Vec<Vec<xiaomaolv::domain::StoredMessage>>>,
}

#[async_trait]
impl ChatProvider for RecordingProposalProvider {
    async fn complete(&self, req: CompletionRequest) -> anyhow::Result<String> {
        self.requests
            .lock()
            .expect("requests mutex")
            .push(req.messages);
        Ok(serde_json::json!({
            "prompt_patch": "new approach",
            "rationale": "not a repeat"
        })
        .to_string())
    }
}

/// The proposal evidence carries the loop's failed-intervention memory:
/// rejected candidates and rolled-back deployments are listed so the model
/// does not resurface a buried idea.
#[tokio::test]
async fn proposal_evidence_includes_prior_interventions() -> anyhow::Result<()> {
    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let harness: Arc<dyn HarnessStore> = Arc::new(SqliteHarnessStore::new(memory.clone()));
    let store = Arc::new(SqliteEvolutionStore::new(memory.clone()));
    store
        .upsert_eval_case(
            EvolutionEvalCase {
                id: "case".to_string(),
                name: "case".to_string(),
                input: "input".to_string(),
                assertions: EvolutionCaseAssertions {
                    required_substrings: vec!["pass".to_string()],
                    forbidden_substrings: Vec::new(),
                    require_json: false,
                    ..Default::default()
                },
                weight: 1.0,
                enabled: true,
            },
            "operator:test",
        )
        .await?;
    let provider = Arc::new(RecordingProposalProvider {
        requests: std::sync::Mutex::new(Vec::new()),
    });
    let engine = EvolutionEngine::new(store.clone(), provider.clone(), gate_config())
        .await?
        .with_harness_store(harness.clone());

    // One rejected candidate and one rolled-back deployment seed the memory.
    let rejected = engine
        .create_candidate(
            "rejected patch",
            "bad idea",
            vec![],
            &EvolutionActor::System("evolution-engine".to_string()),
        )
        .await?;
    store
        .transition_candidate(
            &rejected.id,
            xiaomaolv::harness::evolution::EvolutionCandidateStatus::Draft,
            xiaomaolv::harness::evolution::EvolutionCandidateStatus::Rejected,
            "operator:test",
            serde_json::json!({"reason": "denied"}),
        )
        .await?;

    let candidate = engine
        .create_candidate(
            "deployed patch",
            "tried and buried",
            vec![],
            &EvolutionActor::System("evolution-engine".to_string()),
        )
        .await?;
    engine.evaluate_candidate(&candidate.id).await?;
    let human = EvolutionActor::Human("henry".to_string());
    engine
        .approve_candidate(&candidate.id, &human, "ok")
        .await?;
    engine
        .activate_candidate(&candidate.id, &human, "rollout")
        .await?;
    engine.rollback_active(&human, "regression seen").await?;

    // A failed run for the proposal to consume.
    harness
        .start_trajectory("fail-1", "s", "telegram", "u", "m")
        .await?;
    harness
        .insert_trajectory_tool_call(
            "fail-1",
            ToolCallRecord {
                call_index: 0,
                server: "srv".to_string(),
                tool: "tool".to_string(),
                arguments: serde_json::json!({}),
                result: serde_json::json!({}),
                ok: false,
                duration_ms: 5,
                iteration: 0,
            },
        )
        .await?;
    harness
        .finish_trajectory("fail-1", None, TrajectoryExitReason::ToolError)
        .await?;

    engine.propose_from_trajectories().await?;

    let requests = provider.requests.lock().expect("requests mutex");
    let evidence = requests
        .iter()
        .flatten()
        .find(|message| message.content.contains("rolled_back_deployments"))
        .map(|message| message.content.clone())
        .expect("proposal evidence carries prior interventions");
    assert!(evidence.contains(&rejected.id), "rejected candidate listed");
    assert!(
        evidence.contains(&candidate.id),
        "rolled-back candidate listed"
    );
    assert!(
        evidence.contains("rollback_reason"),
        "rollback context listed"
    );
    Ok(())
}

/// Reply-markers bind to the chat's most recent finished run and become
/// trajectory feedback the proposal engine consumes — closing the loop from
/// real user signal to evidence.
#[tokio::test]
async fn reply_feedback_binds_to_latest_finished_run() -> anyhow::Result<()> {
    use xiaomaolv::memory::{MemoryBackend, SqliteMemoryBackend};
    use xiaomaolv::service::{AgentMcpSettings, MessageService};

    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let harness: Arc<dyn HarnessStore> = Arc::new(SqliteHarnessStore::new(memory.clone()));
    let evolution: Arc<dyn EvolutionStore> = Arc::new(SqliteEvolutionStore::new(memory.clone()));
    let backend: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(memory.clone()));
    let service = MessageService::new_with_backend(
        Arc::new(ScoreProvider),
        backend,
        None,
        AgentMcpSettings::default(),
        20,
        0,
        0,
    )
    .with_harness_store(harness)
    .with_evolution_store(evolution.clone());

    let now = chrono::Utc::now().timestamp();
    // A stale run under the same chat prefix must not absorb new feedback.
    memory
        .record_finished_trajectory(&trajectory_in_session(
            "tg:42",
            "tg-old",
            now - 4 * 3_600,
            now - 4 * 3_600 + 10,
            TrajectoryExitReason::FinalAnswer,
            Some("stale"),
            vec![],
        ))
        .await?;
    // The current run under the chat prefix.
    memory
        .record_finished_trajectory(&trajectory_in_session(
            "tg:42",
            "tg-new",
            now - 60,
            now - 30,
            TrajectoryExitReason::FinalAnswer,
            Some("fresh"),
            vec![],
        ))
        .await?;
    // A newer run in an unrelated session must not win either.
    memory
        .record_finished_trajectory(&trajectory_in_session(
            "tg:99",
            "other-chat",
            now - 20,
            now - 10,
            TrajectoryExitReason::FinalAnswer,
            Some("other"),
            vec![],
        ))
        .await?;

    let recorded = service
        .record_evolution_reply_feedback(42, None, "telegram", "777", -0.5, None)
        .await?;
    assert!(recorded);

    let feedback = evolution.list_feedback(false, 20).await?;
    assert_eq!(feedback.len(), 1);
    assert_eq!(feedback[0].trajectory_id, "tg-new");
    assert_eq!(feedback[0].score, -0.5);
    assert!(feedback[0].tags.iter().any(|tag| tag == "telegram-reply"));

    // A second 👎 on the same run dedups? It does not — repeated user
    // signals are distinct records; the evidence layer bounds their volume.
    let recorded_again = service
        .record_evolution_reply_feedback(42, None, "telegram", "777", -0.5, None)
        .await?;
    assert!(recorded_again);

    // Unknown chat prefixes and missing evolution wiring both decline.
    assert!(
        !service
            .record_evolution_reply_feedback(777, None, "telegram", "777", 0.5, None)
            .await?
    );
    let bare_service = MessageService::new_with_backend(
        Arc::new(ScoreProvider),
        Arc::new(SqliteMemoryBackend::new(memory.clone())),
        None,
        AgentMcpSettings::default(),
        20,
        0,
        0,
    );
    assert!(
        !bare_service
            .record_evolution_reply_feedback(42, None, "telegram", "777", 0.5, None)
            .await?
    );
    Ok(())
}

/// A failed production run seeds a disabled eval case — the eval suite
/// grows from real failures while staying human-gated (disabled by default,
/// a second operator action enables it).
#[tokio::test]
async fn eval_case_seeds_from_trajectory_with_frame_input() -> anyhow::Result<()> {
    use xiaomaolv::domain::{MessageRole, StoredMessage};
    use xiaomaolv::harness::loop_engine::{TrajectoryFrameCapture, TrajectoryFrameDraft};

    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let harness: Arc<dyn HarnessStore> = Arc::new(SqliteHarnessStore::new(memory.clone()));
    let store = Arc::new(SqliteEvolutionStore::new(memory.clone()));
    let engine = EvolutionEngine::new(store.clone(), Arc::new(ScoreProvider), gate_config())
        .await?
        .with_harness_store(harness.clone());
    let human = EvolutionActor::Human("henry".to_string());
    let system = EvolutionActor::System("evolution-engine".to_string());

    memory
        .record_finished_trajectory(&trajectory(
            "seed-src",
            1_800_000_000,
            1_800_000_010,
            TrajectoryExitReason::FinalAnswer,
            Some("bad answer"),
            vec![],
        ))
        .await?;
    harness
        .record_provider_frame(TrajectoryFrameDraft {
            trajectory_id: "seed-src".to_string(),
            call_index: 0,
            model: "model".to_string(),
            provider_fingerprint: "fp".to_string(),
            request_messages: vec![
                StoredMessage {
                    role: MessageRole::System,
                    content: "system".to_string(),
                },
                StoredMessage {
                    role: MessageRole::User,
                    content: "what timezone is dinner?".to_string(),
                },
            ],
            request_was_json: false,
            response: "bad answer".to_string(),
            capture: TrajectoryFrameCapture::Full,
        })
        .await?;

    // Frame-derived input.
    let case = engine
        .seed_eval_case_from_trajectory(
            "seed-src",
            "timezone confusion",
            None,
            EvolutionCaseAssertions {
                required_substrings: vec!["Asia".to_string()],
                ..Default::default()
            },
            1.0,
            &human,
        )
        .await?;
    assert!(!case.enabled, "seeded cases arrive disabled");
    assert_eq!(case.input, "what timezone is dinner?");
    assert!(case.id.starts_with("evcase-"));
    let audit = store.list_audit_events(10).await?;
    assert!(
        audit
            .iter()
            .any(|event| event.event_type == "eval_case_seeded")
    );

    // Explicit input wins over the frame.
    let manual = engine
        .seed_eval_case_from_trajectory(
            "seed-src",
            "explicit input",
            Some("custom prompt".to_string()),
            EvolutionCaseAssertions {
                forbidden_substrings: vec!["wrong".to_string()],
                ..Default::default()
            },
            2.0,
            &human,
        )
        .await?;
    assert_eq!(manual.input, "custom prompt");

    // Guards: human actor, existing trajectory, at least one assertion.
    assert!(
        engine
            .seed_eval_case_from_trajectory(
                "seed-src",
                "x",
                None,
                EvolutionCaseAssertions {
                    required_substrings: vec!["a".to_string()],
                    ..Default::default()
                },
                1.0,
                &system,
            )
            .await
            .is_err()
    );
    assert!(
        engine
            .seed_eval_case_from_trajectory(
                "missing-traj",
                "x",
                Some("input".to_string()),
                EvolutionCaseAssertions {
                    required_substrings: vec!["a".to_string()],
                    ..Default::default()
                },
                1.0,
                &human,
            )
            .await
            .is_err()
    );
    assert!(
        engine
            .seed_eval_case_from_trajectory(
                "seed-src",
                "x",
                None,
                EvolutionCaseAssertions::default(),
                1.0,
                &human,
            )
            .await
            .is_err()
    );
    Ok(())
}

/// Three negative-flagged runs inside the spike window emit one
/// deduplicated hourly-bucket `user_feedback` signal into operator triage.
#[tokio::test]
async fn negative_feedback_spike_emits_one_deduped_signal() -> anyhow::Result<()> {
    use xiaomaolv::harness::evolution::EvolutionFeedbackDraft;

    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let store = SqliteEvolutionStore::new(memory.clone());
    let loop_store = SqliteLoopStore::new(memory.clone());

    for index in 0..4 {
        let trajectory_id = format!("fb-{index}");
        memory
            .record_finished_trajectory(&trajectory(
                &trajectory_id,
                1_800_000_000,
                1_800_000_010,
                TrajectoryExitReason::FinalAnswer,
                Some("bad"),
                vec![],
            ))
            .await?;
        EvolutionStore::record_feedback(
            &store,
            EvolutionFeedbackDraft {
                trajectory_id: trajectory_id.clone(),
                score: -0.5,
                tags: vec!["telegram-reply".to_string()],
                comment: None,
            },
            "telegram:777",
        )
        .await?;
    }

    let signals = LoopStore::list_signals(&loop_store, 20, None).await?;
    let spikes: Vec<_> = signals
        .iter()
        .filter(|signal| signal.kind == SignalKind::UserFeedback)
        .collect();
    assert_eq!(
        spikes.len(),
        1,
        "the same-hour bucket dedups repeated spike emissions"
    );
    assert!(spikes[0].content.contains("negative feedback spike"));
    Ok(())
}

/// A reply-marker bound via the recorded outbound message resolves the
/// exact producing run — a later unrelated run in the same session cannot
/// steal the feedback, and replies to unrecorded messages still fall back
/// to the recency heuristic.
#[tokio::test]
async fn reply_marker_binds_exactly_to_the_answered_run() -> anyhow::Result<()> {
    use xiaomaolv::memory::{MemoryBackend, SqliteMemoryBackend};
    use xiaomaolv::service::{AgentMcpSettings, MessageService};

    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let harness: Arc<dyn HarnessStore> = Arc::new(SqliteHarnessStore::new(memory.clone()));
    let evolution: Arc<dyn EvolutionStore> = Arc::new(SqliteEvolutionStore::new(memory.clone()));
    let backend: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(memory.clone()));
    let service = MessageService::new_with_backend(
        Arc::new(ScoreProvider),
        backend,
        None,
        AgentMcpSettings::default(),
        20,
        0,
        0,
    )
    .with_harness_store(harness)
    .with_evolution_store(evolution.clone())
    .with_telegram_outbound_store(Arc::new(memory.clone()));

    let now = chrono::Utc::now().timestamp();
    // The run that produced the answered message finished at T.
    memory
        .record_finished_trajectory(&trajectory_in_session(
            "tg:42",
            "tg-answer",
            now - 120,
            now - 90,
            TrajectoryExitReason::FinalAnswer,
            Some("answer"),
            vec![],
        ))
        .await?;
    // Bot message 500 was sent right after that finish.
    memory
        .record_telegram_outbound_message_at(42, 500, "tg:42", now - 85)
        .await?;
    // A NEWER unrelated run finished in the same session — the recency
    // heuristic alone would pick this one and be wrong.
    memory
        .record_finished_trajectory(&trajectory_in_session(
            "tg:42",
            "tg-newer",
            now - 60,
            now - 30,
            TrajectoryExitReason::FinalAnswer,
            Some("newer"),
            vec![],
        ))
        .await?;

    // Exact: replying to message 500 binds the producing run.
    let recorded = service
        .record_evolution_reply_feedback(42, Some(500), "telegram", "777", -0.5, None)
        .await?;
    assert!(recorded);
    let feedback = evolution.list_feedback(false, 20).await?;
    assert_eq!(feedback.len(), 1);
    assert_eq!(feedback[0].trajectory_id, "tg-answer");

    // Fallback: a reply to an unrecorded message still binds heuristically.
    let recorded = service
        .record_evolution_reply_feedback(42, Some(999), "telegram", "777", 0.5, None)
        .await?;
    assert!(recorded);
    let feedback = evolution.list_feedback(false, 20).await?;
    assert_eq!(feedback.len(), 2);
    assert_eq!(feedback[0].trajectory_id, "tg-newer");
    Ok(())
}
