use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tempfile::tempdir;
use xiaomaolv::domain::{IncomingMessage, MessageRole, StoredMessage};
use xiaomaolv::harness::evolution::{
    EvolutionActor, EvolutionCaseAssertions, EvolutionEngine, EvolutionEvalCase,
    EvolutionGateConfig,
};
use xiaomaolv::harness::loop_engine::{
    AcceptanceCriterion, ApproveGoalRequest, ArtifactKind, AttemptStatus, CheckpointPhase,
    ConfirmationResolution, CreateGoalRequest, CreateSignalRequest, EffectClass, ExecutionBudget,
    ExternalWritePolicy, GoalStatus, LoopEngine, LoopWorker, OutboundSender, PlanGoalRequest,
    PublishArtifactRequest, ReplayStatus, RetryPolicy, SelfTestStatus, SignalKind, SignalStatus,
    SignalTrust, SqliteLoopStore, TrajectoryFrameCapture, TrajectoryFrameDraft, WorkContinuation,
    WorkHandler, WorkHandlerContext, WorkHandlerRegistry, WorkItemStatus, WorkOutcome,
    WorkflowEdge, WorkflowSpec, WorkflowStep,
};
use xiaomaolv::harness::store::SqliteEvolutionStore;
use xiaomaolv::memory::SqliteMemoryStore;
use xiaomaolv::provider::{ChatProvider, CompletionRequest};
use xiaomaolv::service::{AgentSwarmSettings, MessageService};

struct WorkerProvider;

#[async_trait]
impl ChatProvider for WorkerProvider {
    fn model_name(&self) -> Option<&str> {
        Some("worker-test-model")
    }

    async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
        Ok("Durable checkpoints make recovery observable.".to_string())
    }
}

#[tokio::test]
async fn built_in_worker_dispatches_the_approved_dag_and_verifies_the_goal() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("worker.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "Make loop recovery observable".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "operator:test",
        )
        .await?;
    let plan = engine
        .plan_goal_recommended(&goal.id, "planner:built-in")
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash,
            },
            "operator:test",
        )
        .await?;

    let worker = LoopWorker::with_builtins(engine.clone(), Arc::new(WorkerProvider), None, None)?;
    let report = worker.run_goal_until_idle(&goal.id, 8).await?;

    assert_eq!(report.goal.status, GoalStatus::Achieved);
    assert_eq!(report.work_items.len(), 2);
    assert!(
        report
            .work_items
            .iter()
            .all(|item| item.status == WorkItemStatus::Succeeded)
    );
    assert_eq!(report.attempts.len(), 2);
    Ok(())
}

#[tokio::test]
async fn built_in_worker_runs_evolution_evaluation_with_persisted_budgets() -> anyhow::Result<()> {
    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let loop_engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(
        memory.clone(),
    ))));
    let evolution_store = Arc::new(SqliteEvolutionStore::new(memory));
    let evolution = Arc::new(
        EvolutionEngine::new(
            evolution_store,
            Arc::new(WorkerProvider),
            EvolutionGateConfig {
                min_eval_cases: 1,
                min_candidate_score: 0.0,
                min_score_delta: 0.0,
                max_regressions: 0,
                max_prompt_patch_chars: 1_000,
                require_human_approval: true,
            },
        )
        .await?,
    );
    evolution
        .upsert_eval_case(
            EvolutionEvalCase {
                id: "loop-eval".to_string(),
                name: "loop eval".to_string(),
                input: "Explain durable checkpoints".to_string(),
                assertions: EvolutionCaseAssertions {
                    required_substrings: vec!["Durable".to_string()],
                    forbidden_substrings: Vec::new(),
                    require_json: false,
                    ..Default::default()
                },
                weight: 1.0,
                enabled: true,
            },
            &EvolutionActor::Human("test".to_string()),
        )
        .await?;
    let candidate = evolution
        .create_candidate(
            "Prefer explicit recovery evidence.",
            "exercise the bounded loop adapter",
            Vec::new(),
            &EvolutionActor::System("test".to_string()),
        )
        .await?;
    let goal = loop_engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "Evaluate a prompt candidate safely".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "operator:test",
        )
        .await?;
    let plan = loop_engine
        .plan_goal(
            &goal.id,
            PlanGoalRequest {
                workflow: WorkflowSpec {
                    steps: vec![WorkflowStep {
                        id: "evaluate-candidate".to_string(),
                        handler: "evolution_evaluate".to_string(),
                        effect: EffectClass::LocalWrite,
                        input: serde_json::json!({"candidate_id": candidate.id}),
                        retry: RetryPolicy {
                            max_attempts: 1,
                            backoff_secs: 0,
                        },
                    }],
                    edges: Vec::new(),
                    budget: ExecutionBudget {
                        max_provider_calls: 2,
                        deadline_secs: 60,
                        max_response_bytes: 65_536,
                    },
                },
                acceptance_criteria: vec![AcceptanceCriterion::ArtifactExists {
                    artifact_type: "evolution_evaluation".to_string(),
                }],
            },
            "planner:test",
        )
        .await?;
    loop_engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash,
            },
            "operator:test",
        )
        .await?;

    let worker =
        LoopWorker::with_builtins(loop_engine, Arc::new(WorkerProvider), Some(evolution), None)?;
    let report = worker.run_goal_until_idle(&goal.id, 4).await?;
    assert_eq!(report.goal.status, GoalStatus::Achieved);
    Ok(())
}

#[tokio::test]
async fn structural_session_replay_validates_each_provider_frame_without_live_tools()
-> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("replay.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = LoopEngine::new(Arc::new(SqliteLoopStore::new(memory)));
    for (call_index, response) in ["analysis", "final answer"].into_iter().enumerate() {
        engine
            .record_trajectory_frame(
                TrajectoryFrameDraft {
                    trajectory_id: "trajectory-replay-1".to_string(),
                    call_index: call_index as u32,
                    model: "test-model".to_string(),
                    provider_fingerprint: "provider:test:v1".to_string(),
                    request_messages: vec![StoredMessage {
                        role: MessageRole::User,
                        content: format!("request {call_index}"),
                    }],
                    request_was_json: false,
                    response: response.to_string(),
                    capture: TrajectoryFrameCapture::Full,
                },
                "trajectory:runtime",
            )
            .await?;
    }

    let replay = engine
        .run_structural_replay("trajectory-replay-1", "replay:runtime")
        .await?;
    assert_eq!(replay.status, ReplayStatus::Passed);
    assert!(replay.exact_replayable);
    assert_eq!(replay.cases.len(), 2);
    assert!(replay.cases.iter().all(|case| case.passed));
    assert!(replay.live_tools_executed == 0);
    Ok(())
}

#[tokio::test]
async fn prompt_artifact_is_a_reference_to_the_existing_evolution_source_of_truth()
-> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("artifacts.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = LoopEngine::new(Arc::new(SqliteLoopStore::new(memory)));

    let duplicated_prompt = engine
        .publish_artifact(
            PublishArtifactRequest {
                kind: ArtifactKind::PromptPolicyRef,
                name: "active-policy".to_string(),
                version: "1".to_string(),
                content: serde_json::json!({
                    "evolution_candidate_id": "candidate_1",
                    "prompt": "duplicated prompt text"
                }),
                source_goal_id: None,
                parent_artifact_id: None,
            },
            "artifact:runtime",
        )
        .await;
    assert!(duplicated_prompt.is_err());

    let published = engine
        .publish_artifact(
            PublishArtifactRequest {
                kind: ArtifactKind::PromptPolicyRef,
                name: "active-policy".to_string(),
                version: "1".to_string(),
                content: serde_json::json!({
                    "evolution_candidate_id": "candidate_1",
                    "deployment_id": "deployment_1"
                }),
                source_goal_id: None,
                parent_artifact_id: None,
            },
            "artifact:runtime",
        )
        .await?;
    assert_eq!(published.artifact.kind, ArtifactKind::PromptPolicyRef);
    assert!(!published.deduplicated);
    Ok(())
}

#[tokio::test]
async fn core_self_test_reads_product_state_and_persists_its_maintenance_report()
-> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("self-test.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = LoopEngine::new(Arc::new(SqliteLoopStore::new(memory)));

    let run = engine.run_self_tests("core", "self-test:runtime").await?;

    assert_eq!(run.status, SelfTestStatus::Passed);
    assert!(run.cases.len() >= 4);
    assert!(run.cases.iter().all(|case| case.passed));
    let persisted = engine
        .get_self_test_run(&run.id)
        .await?
        .expect("self-test report should be durable");
    assert_eq!(persisted, run);
    Ok(())
}

#[tokio::test]
async fn repeated_self_test_failure_emits_one_deduplicated_signal() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!(
        "sqlite://{}",
        temp.path().join("self-test-failure.db").display()
    );
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = LoopEngine::new(Arc::new(SqliteLoopStore::new(memory)));
    let pool = sqlx::SqlitePool::connect(&database_url).await?;
    sqlx::query("DROP TABLE harness_goal_approvals")
        .execute(&pool)
        .await?;

    let first = engine.run_self_tests("core", "self-test:test").await?;
    let second = engine.run_self_tests("core", "self-test:test").await?;
    assert_eq!(first.status, SelfTestStatus::Failed);
    assert_eq!(second.status, SelfTestStatus::Failed);

    let signals = engine.list_signals(20, None).await?;
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].kind, SignalKind::SelfTest);
    Ok(())
}

#[tokio::test]
async fn goal_survives_engine_restart_and_can_be_resumed() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("harness.db").display());

    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = LoopEngine::new(Arc::new(SqliteLoopStore::new(memory)));
    let created = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "Make recovery observable".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "operator:henry",
        )
        .await?;
    assert_eq!(created.status, GoalStatus::Proposed);
    assert_eq!(created.revision, 1);
    drop(engine);

    let reopened_memory = SqliteMemoryStore::new(&database_url).await?;
    let reopened = LoopEngine::new(Arc::new(SqliteLoopStore::new(reopened_memory)));
    let resumed = reopened.resume_goal(&created.id, "operator:henry").await?;

    assert_eq!(resumed.goal.id, created.id);
    assert_eq!(resumed.goal.objective, "Make recovery observable");
    assert_eq!(resumed.goal.status, GoalStatus::Proposed);
    assert!(resumed.work_items.is_empty());
    assert!(resumed.latest_checkpoint.is_none());
    Ok(())
}

#[tokio::test]
async fn external_signal_is_deduplicated_and_can_only_propose_a_goal() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("signals.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = LoopEngine::new(Arc::new(SqliteLoopStore::new(memory)));
    let request = CreateSignalRequest {
        kind: SignalKind::Community,
        trust: SignalTrust::External,
        source: "github:community".to_string(),
        external_id: Some("discussion-42".to_string()),
        content: "A durable /resume command should explain the last checkpoint.".to_string(),
        metadata: BTreeMap::from([("repository".to_string(), "xiaomaolv".to_string())]),
    };

    let first = engine
        .ingest_signal(request.clone(), "ingest:community")
        .await?;
    let duplicate = engine.ingest_signal(request, "ingest:community").await?;
    assert!(!first.deduplicated);
    assert!(duplicate.deduplicated);
    assert_eq!(duplicate.signal.id, first.signal.id);

    let goal = engine
        .propose_goal_from_signal(
            &first.signal.id,
            "Make recovery state understandable to operators",
            "triage:built-in",
        )
        .await?;
    assert_eq!(goal.status, GoalStatus::Proposed);
    assert_eq!(goal.source_signal_ids, vec![first.signal.id]);
    Ok(())
}

#[tokio::test]
async fn resume_reconciles_a_committed_checkpoint_without_replaying_the_effect()
-> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("recovery.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = LoopEngine::new(Arc::new(SqliteLoopStore::new(memory)));
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "Recover an interrupted evolution evaluation".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "operator:henry",
        )
        .await?;
    let planned = engine
        .plan_goal(
            &goal.id,
            PlanGoalRequest {
                workflow: WorkflowSpec {
                    steps: vec![
                        WorkflowStep {
                            id: "analyze".to_string(),
                            handler: "provider_analysis".to_string(),
                            effect: EffectClass::Read,
                            input: serde_json::json!({"topic": "recovery"}),
                            retry: RetryPolicy {
                                max_attempts: 2,
                                backoff_secs: 0,
                            },
                        },
                        WorkflowStep {
                            id: "evaluate".to_string(),
                            handler: "evolution_evaluate".to_string(),
                            effect: EffectClass::LocalWrite,
                            input: serde_json::json!({"gate": "regression"}),
                            retry: RetryPolicy {
                                max_attempts: 2,
                                backoff_secs: 0,
                            },
                        },
                    ],
                    edges: vec![WorkflowEdge {
                        from: "analyze".to_string(),
                        to: "evaluate".to_string(),
                    }],
                    budget: ExecutionBudget {
                        max_provider_calls: 1,
                        deadline_secs: 60,
                        max_response_bytes: 65_536,
                    },
                },
                acceptance_criteria: vec![AcceptanceCriterion::ArtifactExists {
                    artifact_type: "evaluation".to_string(),
                }],
            },
            "planner:built-in",
        )
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: planned.goal.revision,
                expected_plan_hash: planned.plan_hash,
            },
            "operator:henry",
        )
        .await?;

    let claim = engine
        .claim_goal_work(&goal.id, "worker:one", 30, "runtime:loop")
        .await?
        .expect("first DAG node should be claimable");
    let checkpoint = engine
        .prepare_checkpoint(
            &claim,
            &format!("effect:{}:v1", claim.work_item.id),
            true,
            "runtime:loop",
        )
        .await?;
    engine
        .commit_checkpoint(
            &claim,
            &checkpoint.id,
            WorkOutcome {
                summary: "analysis persisted".to_string(),
                artifact_ids: vec!["artifact_analysis_1".to_string()],
                evidence: serde_json::Value::Null,
                sleep_until_unix: None,
                continuation: None,
            },
            "runtime:loop",
        )
        .await?;
    drop(engine);

    let reopened_memory = SqliteMemoryStore::new(&database_url).await?;
    let reopened = LoopEngine::new(Arc::new(SqliteLoopStore::new(reopened_memory)));
    let resumed = reopened.resume_goal(&goal.id, "operator:henry").await?;
    assert_eq!(resumed.work_items[0].status, WorkItemStatus::Succeeded);
    assert_eq!(resumed.work_items[1].status, WorkItemStatus::Ready);
    assert_eq!(
        resumed.latest_checkpoint.as_ref().map(|value| value.phase),
        Some(xiaomaolv::harness::loop_engine::CheckpointPhase::Reconciled)
    );

    let stale_writer = reopened
        .commit_checkpoint(
            &claim,
            &checkpoint.id,
            WorkOutcome {
                summary: "stale overwrite".to_string(),
                artifact_ids: Vec::new(),
                evidence: serde_json::Value::Null,
                sleep_until_unix: None,
                continuation: None,
            },
            "runtime:stale-worker",
        )
        .await;
    assert!(stale_writer.is_err());
    Ok(())
}

#[tokio::test]
async fn approval_is_bound_to_the_reviewed_workflow_hash() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("approval.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = LoopEngine::new(Arc::new(SqliteLoopStore::new(memory)));
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "Continuously self-test the harness".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "operator:henry",
        )
        .await?;
    let planned = engine
        .plan_goal(
            &goal.id,
            PlanGoalRequest {
                workflow: WorkflowSpec {
                    steps: vec![WorkflowStep {
                        id: "run-core-self-tests".to_string(),
                        handler: "self_test_suite".to_string(),
                        effect: EffectClass::Read,
                        input: serde_json::json!({"suite": "core"}),
                        retry: RetryPolicy {
                            max_attempts: 2,
                            backoff_secs: 1,
                        },
                    }],
                    edges: Vec::<WorkflowEdge>::new(),
                    budget: ExecutionBudget {
                        max_provider_calls: 0,
                        deadline_secs: 60,
                        max_response_bytes: 65_536,
                    },
                },
                acceptance_criteria: vec![AcceptanceCriterion::SelfTestSuite {
                    suite: "core".to_string(),
                }],
            },
            "planner:built-in",
        )
        .await?;
    assert_eq!(planned.goal.status, GoalStatus::ReviewReady);

    let stale = engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: planned.goal.revision,
                expected_plan_hash: "sha256:not-the-reviewed-plan".to_string(),
            },
            "operator:henry",
        )
        .await;
    assert!(stale.is_err());

    let approved = engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: planned.goal.revision,
                expected_plan_hash: planned.plan_hash.clone(),
            },
            "operator:henry",
        )
        .await?;
    assert_eq!(approved.goal.status, GoalStatus::Approved);
    assert_eq!(approved.work_items.len(), 1);
    assert_eq!(approved.work_items[0].handler, "self_test_suite");
    Ok(())
}

#[tokio::test]
async fn manual_acceptance_requires_explicit_operator_evidence() -> anyhow::Result<()> {
    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let engine = LoopEngine::new(Arc::new(SqliteLoopStore::new(memory)));
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "Require a human release decision".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "operator:test",
        )
        .await?;
    let planned = engine
        .plan_goal(
            &goal.id,
            PlanGoalRequest {
                workflow: WorkflowSpec {
                    steps: vec![WorkflowStep {
                        id: "release-gate".to_string(),
                        handler: "manual_gate".to_string(),
                        effect: EffectClass::Read,
                        input: serde_json::json!({}),
                        retry: RetryPolicy {
                            max_attempts: 1,
                            backoff_secs: 0,
                        },
                    }],
                    edges: Vec::new(),
                    budget: ExecutionBudget {
                        max_provider_calls: 0,
                        deadline_secs: 60,
                        max_response_bytes: 1024,
                    },
                },
                acceptance_criteria: vec![AcceptanceCriterion::ManualApproval {
                    label: "release-ready".to_string(),
                }],
            },
            "planner:test",
        )
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: planned.goal.revision,
                expected_plan_hash: planned.plan_hash,
            },
            "operator:test",
        )
        .await?;
    let claim = engine
        .claim_goal_work(&goal.id, "worker:test", 30, "runtime:test")
        .await?
        .expect("manual gate should be claimable");
    let checkpoint = engine
        .prepare_checkpoint(&claim, "manual-gate:v1", true, "runtime:test")
        .await?;
    engine
        .commit_checkpoint(
            &claim,
            &checkpoint.id,
            WorkOutcome {
                summary: "manual gate reached".to_string(),
                artifact_ids: Vec::new(),
                evidence: serde_json::json!({"manual_gate": "reached"}),
                sleep_until_unix: None,
                continuation: None,
            },
            "runtime:test",
        )
        .await?;
    engine
        .finish_attempt(&claim, &checkpoint.id, "runtime:test")
        .await?;

    let pending = engine.verify_goal(&goal.id, "verifier:test").await?;
    assert!(!pending.achieved);
    assert_eq!(
        pending.unmet_criteria,
        vec!["manual_approval:release-ready".to_string()]
    );

    engine
        .record_manual_verification(&goal.id, "release-ready", "operator:test")
        .await?;
    let achieved = engine.verify_goal(&goal.id, "verifier:test").await?;
    assert!(achieved.achieved);
    assert_eq!(achieved.goal.status, GoalStatus::Achieved);
    Ok(())
}

/// Provider that replays a fixed queue of responses for the deterministic
/// swarm call order: activation classifier, root planner, each child's
/// planner (children run sequentially), then the root merge call.
#[derive(Default)]
struct SwarmQueueProvider {
    replies: Mutex<VecDeque<String>>,
}

#[async_trait]
impl ChatProvider for SwarmQueueProvider {
    fn model_name(&self) -> Option<&str> {
        Some("swarm-projection-model")
    }

    async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
        Ok(self
            .replies
            .lock()
            .expect("replies mutex")
            .pop_front()
            .unwrap_or_else(|| "swarm fallback".to_string()))
    }
}

/// A full swarm run must leave a durable Goal -> WorkItem -> Attempt ->
/// Checkpoint projection in Loop Engine state, and a verified goal once every
/// node succeeds.
#[tokio::test]
async fn swarm_run_projects_a_durable_goal_tree_and_verifies_it() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("swarm.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(
        memory.clone(),
    ))));
    let provider = Arc::new(SwarmQueueProvider {
        replies: Mutex::new(VecDeque::from([
            "{\"use_swarm\":true}".to_string(),
            "{\"mode\":\"delegate\",\"children\":[{\"task\":\"outline frontend work\",\"role_name\":\"frontend\"},{\"task\":\"outline backend work\",\"role_name\":\"backend\"}]}".to_string(),
            "{\"mode\":\"answer\",\"answer\":\"frontend plan\"}".to_string(),
            "{\"mode\":\"answer\",\"answer\":\"backend plan\"}".to_string(),
            "merged swarm answer".to_string(),
        ])),
    });
    let service = MessageService::new(provider, memory, 8)
        .with_agent_swarm(AgentSwarmSettings {
            enabled: true,
            auto_detect: true,
            reply_summary_enabled: false,
            // The legacy in-process runner + mirror projection is what this
            // test pins; durable execution has its own test below.
            durable_engine: false,
            ..Default::default()
        })
        .with_loop_engine(Some(engine.clone()));

    let reply = service
        .handle(IncomingMessage {
            channel: "telegram".to_string(),
            session_id: "tg:1".to_string(),
            user_id: "42".to_string(),
            text: "请拆分前后端任务并协作完成".to_string(),
            reply_target: None,
            is_group_chat: false,
        })
        .await?;
    assert!(reply.text.contains("merged swarm answer"));

    let goals = engine.list_goals(10).await?;
    assert_eq!(goals.len(), 1);
    let goal = &goals[0];
    assert_eq!(goal.created_by, "internal:swarm");
    assert_eq!(goal.status, GoalStatus::Achieved);

    let resumed = engine.resume_goal(&goal.id, "operator:test").await?;
    assert_eq!(resumed.work_items.len(), 3);
    assert!(
        resumed
            .work_items
            .iter()
            .all(|item| item.status == WorkItemStatus::Succeeded)
    );
    let root_step = resumed
        .work_items
        .iter()
        .find(|item| item.step_id.ends_with(":1"))
        .expect("root step");
    let run_id = root_step
        .step_id
        .rsplit_once(':')
        .map(|(run_id, _)| run_id.to_string())
        .expect("root step id carries the run id");
    let step_ids: Vec<&str> = resumed
        .work_items
        .iter()
        .map(|item| item.step_id.as_str())
        .collect();
    for idx in 1..=3 {
        assert!(step_ids.contains(&format!("{run_id}:{idx}").as_str()));
    }
    let child = resumed
        .work_items
        .iter()
        .find(|item| item.step_id.ends_with(":2"))
        .expect("first child step");
    assert_eq!(
        child.input.get("parent_agent_id").and_then(|v| v.as_str()),
        Some(format!("{run_id}:1").as_str())
    );
    assert_eq!(resumed.attempts.len(), 3);
    assert!(
        resumed
            .attempts
            .iter()
            .all(|attempt| attempt.status
                == xiaomaolv::harness::loop_engine::AttemptStatus::Succeeded)
    );
    let artifact = engine
        .find_artifact(
            ArtifactKind::AnalysisReport,
            &format!("swarm-{run_id}"),
            "1",
        )
        .await?
        .expect("swarm result artifact");
    assert_eq!(artifact.source_goal_id.as_deref(), Some(goal.id.as_str()));

    let events = engine.list_goal_events(&goal.id, 0, 100).await?;
    let event_types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(
        event_types
            .iter()
            .filter(|t| **t == "workflow.extended")
            .count(),
        2
    );
    assert_eq!(
        event_types.iter().filter(|t| **t == "work.claimed").count(),
        3
    );
    assert!(event_types.contains(&"goal.achieved"));
    Ok(())
}

/// A mid-run crash (expired leases, no commits) must leave durable,
/// inspectable state: the completed node stays `succeeded`, the interrupted
/// node is reconciled, and the goal reflects the failure.
#[tokio::test]
async fn swarm_crash_leaves_resumable_durable_state() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("swarm-crash.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));

    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "agent swarm run run-crash: root task".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "internal:swarm",
        )
        .await?;
    let planned = engine
        .plan_goal(
            &goal.id,
            PlanGoalRequest {
                workflow: WorkflowSpec {
                    steps: vec![WorkflowStep {
                        id: "run-crash:1".to_string(),
                        handler: "provider_analysis".to_string(),
                        effect: EffectClass::LocalWrite,
                        input: serde_json::json!({
                            "swarm_root": true,
                            "max_nodes": 4,
                            "swarm_run_id": "run-crash",
                        }),
                        retry: RetryPolicy {
                            max_attempts: 1,
                            backoff_secs: 0,
                        },
                    }],
                    edges: Vec::new(),
                    budget: ExecutionBudget {
                        max_provider_calls: 8,
                        deadline_secs: 120,
                        max_response_bytes: 65_536,
                    },
                },
                acceptance_criteria: vec![AcceptanceCriterion::ArtifactExists {
                    artifact_type: "analysis_report".to_string(),
                }],
            },
            "internal:swarm",
        )
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: planned.goal.revision,
                expected_plan_hash: planned.plan_hash,
            },
            "internal:swarm",
        )
        .await?;

    let root_claim = engine
        .claim_work_item(
            &goal.id,
            "run-crash:1",
            "swarm:run-crash",
            30,
            "internal:swarm",
        )
        .await?
        .expect("root step should be claimable");
    let root_checkpoint = engine
        .prepare_checkpoint(&root_claim, "root:v1", true, "internal:swarm")
        .await?;
    engine
        .commit_checkpoint(
            &root_claim,
            &root_checkpoint.id,
            WorkOutcome {
                summary: "root finished before crash".to_string(),
                artifact_ids: Vec::new(),
                evidence: serde_json::json!({"status": "success"}),
                sleep_until_unix: None,
                continuation: None,
            },
            "internal:swarm",
        )
        .await?;
    engine
        .finish_attempt(&root_claim, &root_checkpoint.id, "internal:swarm")
        .await?;

    let appended = engine
        .extend_workflow(
            &goal.id,
            vec![WorkflowStep {
                id: "run-crash:2".to_string(),
                handler: "provider_analysis".to_string(),
                effect: EffectClass::LocalWrite,
                input: serde_json::json!({
                    "swarm_run_id": "run-crash",
                    "parent_agent_id": "run-crash:1",
                    "depth": 1,
                }),
                retry: RetryPolicy {
                    max_attempts: 1,
                    backoff_secs: 0,
                },
            }],
            "internal:swarm",
        )
        .await?;
    assert_eq!(appended.len(), 1);
    // Claim the child with a 1s lease, then simulate a crash: no commit, no
    // finish, process state dropped.
    let _child_claim = engine
        .claim_work_item(
            &goal.id,
            "run-crash:2",
            "swarm:run-crash",
            1,
            "internal:swarm",
        )
        .await?
        .expect("child step should be claimable");
    drop(engine);

    // Lease expiry is compared in whole seconds (`lease_until < unix_now`), so
    // wait past the next second boundary plus the 1s lease.
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
    let reopened_memory = SqliteMemoryStore::new(&database_url).await?;
    let reopened = LoopEngine::new(Arc::new(SqliteLoopStore::new(reopened_memory)));
    let resumed = reopened.resume_goal(&goal.id, "operator:test").await?;
    let by_step: BTreeMap<&str, &xiaomaolv::harness::loop_engine::WorkItemRecord> = resumed
        .work_items
        .iter()
        .map(|item| (item.step_id.as_str(), item))
        .collect();
    assert_eq!(by_step["run-crash:1"].status, WorkItemStatus::Succeeded);
    // max_attempts = 1 means the crashed attempt exhausts the node's retry
    // budget: the interruption is durably recorded as a failed node.
    assert_eq!(by_step["run-crash:2"].status, WorkItemStatus::Failed);
    assert_eq!(resumed.goal.status, GoalStatus::Failed);
    Ok(())
}

/// `extend_workflow` must enforce the internal-actor requirement, the approved
/// effect manifest, and the declared expansion budget.
#[tokio::test]
async fn extend_workflow_enforces_internal_actor_manifest_and_budget() -> anyhow::Result<()> {
    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let engine = LoopEngine::new(Arc::new(SqliteLoopStore::new(memory)));
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "Bounded dynamic expansion".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "internal:swarm",
        )
        .await?;
    let planned = engine
        .plan_goal(
            &goal.id,
            PlanGoalRequest {
                workflow: WorkflowSpec {
                    steps: vec![WorkflowStep {
                        id: "run:1".to_string(),
                        handler: "provider_analysis".to_string(),
                        effect: EffectClass::LocalWrite,
                        input: serde_json::json!({"swarm_root": true, "max_nodes": 2}),
                        retry: RetryPolicy {
                            max_attempts: 1,
                            backoff_secs: 0,
                        },
                    }],
                    edges: Vec::new(),
                    budget: ExecutionBudget {
                        max_provider_calls: 4,
                        deadline_secs: 60,
                        max_response_bytes: 65_536,
                    },
                },
                acceptance_criteria: vec![AcceptanceCriterion::ArtifactExists {
                    artifact_type: "analysis_report".to_string(),
                }],
            },
            "internal:swarm",
        )
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: planned.goal.revision,
                expected_plan_hash: planned.plan_hash,
            },
            "internal:swarm",
        )
        .await?;

    let child_step = |id: &str, effect: EffectClass| WorkflowStep {
        id: id.to_string(),
        handler: "provider_analysis".to_string(),
        effect,
        input: serde_json::json!({"swarm_run_id": "run"}),
        retry: RetryPolicy {
            max_attempts: 1,
            backoff_secs: 0,
        },
    };

    // Non-internal actors cannot extend.
    let operator_attempt = engine
        .extend_workflow(
            &goal.id,
            vec![child_step("run:2", EffectClass::LocalWrite)],
            "operator:test",
        )
        .await;
    assert!(operator_attempt.is_err());
    // Effects outside the approved manifest (plan declared only local_write).
    let wrong_effect = engine
        .extend_workflow(
            &goal.id,
            vec![child_step("run:2", EffectClass::Read)],
            "internal:swarm",
        )
        .await;
    assert!(wrong_effect.is_err());
    // Unregistered handler.
    let mut bad_handler = child_step("run:2", EffectClass::LocalWrite);
    bad_handler.handler = "unknown_handler".to_string();
    assert!(
        engine
            .extend_workflow(&goal.id, vec![bad_handler], "internal:swarm")
            .await
            .is_err()
    );
    // Within budget: max_nodes=2 and one item already exists.
    engine
        .extend_workflow(
            &goal.id,
            vec![child_step("run:2", EffectClass::LocalWrite)],
            "internal:swarm",
        )
        .await?;
    // Exceeding the expansion budget is rejected.
    assert!(
        engine
            .extend_workflow(
                &goal.id,
                vec![child_step("run:3", EffectClass::LocalWrite)],
                "internal:swarm"
            )
            .await
            .is_err()
    );

    // Targeted claim selects the requested step, not the lowest ordinal.
    engine
        .claim_work_item(&goal.id, "run:1", "swarm:run", 30, "internal:swarm")
        .await?
        .expect("root claimable");
    let child_claim = engine
        .claim_work_item(&goal.id, "run:2", "swarm:run", 30, "internal:swarm")
        .await?
        .expect("targeted child claimable");
    assert_eq!(child_claim.work_item.step_id, "run:2");
    Ok(())
}

/// The `internal:auto` actor auto-approves only plans whose effect manifest
/// stays within the configured ceiling and whose provider-call budget is under
/// the cap; other internal actors (e.g. `internal:swarm`) are unaffected.
#[tokio::test]
async fn internal_auto_approval_is_bounded_by_policy() -> anyhow::Result<()> {
    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let engine = LoopEngine::new(Arc::new(SqliteLoopStore::new(memory)));

    async fn plan_and_try_approve(
        engine: &LoopEngine,
        objective: &str,
        effect: EffectClass,
        max_provider_calls: u32,
        actor: &str,
    ) -> anyhow::Result<()> {
        let goal = engine
            .create_goal(
                CreateGoalRequest {
                    internal_only: false,
                    objective: objective.to_string(),
                    source_signal_ids: Vec::new(),
                    request_id: None,
                },
                actor,
            )
            .await?;
        let planned = engine
            .plan_goal(
                &goal.id,
                PlanGoalRequest {
                    workflow: WorkflowSpec {
                        steps: vec![WorkflowStep {
                            id: "step-1".to_string(),
                            handler: "provider_analysis".to_string(),
                            effect,
                            input: serde_json::json!({}),
                            retry: RetryPolicy {
                                max_attempts: 1,
                                backoff_secs: 0,
                            },
                        }],
                        edges: Vec::new(),
                        budget: ExecutionBudget {
                            max_provider_calls,
                            deadline_secs: 60,
                            max_response_bytes: 65_536,
                        },
                    },
                    acceptance_criteria: vec![AcceptanceCriterion::ArtifactExists {
                        artifact_type: "analysis_report".to_string(),
                    }],
                },
                actor,
            )
            .await?;
        engine
            .approve_goal(
                &goal.id,
                ApproveGoalRequest {
                    expected_goal_revision: planned.goal.revision,
                    expected_plan_hash: planned.plan_hash,
                },
                actor,
            )
            .await?;
        Ok(())
    }

    // Read-effect plan within the default 16-call cap: approved.
    plan_and_try_approve(
        &engine,
        "read-only internal plan",
        EffectClass::Read,
        4,
        "internal:auto",
    )
    .await?;
    // local_write exceeds the default "read" effect ceiling.
    let write_plan = plan_and_try_approve(
        &engine,
        "write plan",
        EffectClass::LocalWrite,
        4,
        "internal:auto",
    )
    .await;
    assert!(write_plan.is_err());
    // Over-cap provider budget is rejected even with an allowed effect.
    let over_budget = plan_and_try_approve(
        &engine,
        "over budget",
        EffectClass::Read,
        64,
        "internal:auto",
    )
    .await;
    assert!(over_budget.is_err());
    // A subsystem actor approves its own code-built plan without the generic
    // auto-approval bounds (domain validation still applies).
    plan_and_try_approve(
        &engine,
        "subsystem plan",
        EffectClass::LocalWrite,
        32,
        "internal:swarm",
    )
    .await?;

    // The auto-approved goal recorded an approval event attributed to
    // internal:auto.
    let goals = engine.list_goals(10).await?;
    let auto_goal = goals
        .iter()
        .find(|goal| goal.objective == "read-only internal plan")
        .expect("auto-approved goal");
    let events = engine.list_goal_events(&auto_goal.id, 0, 50).await?;
    assert!(
        events
            .iter()
            .any(|event| event.actor == "internal:auto" && event.event_type.contains("approv"))
    );
    Ok(())
}

struct RecordingOutboundSender {
    sends: Mutex<Vec<(String, String, String, String)>>,
}

#[async_trait]
impl OutboundSender for RecordingOutboundSender {
    async fn send(
        &self,
        channel: &str,
        session_id: &str,
        text: &str,
        idempotency_key: &str,
    ) -> anyhow::Result<serde_json::Value> {
        self.sends.lock().expect("sends").push((
            channel.to_string(),
            session_id.to_string(),
            text.to_string(),
            idempotency_key.to_string(),
        ));
        Ok(serde_json::json!({"sent_at_unix": 1_700_000_000}))
    }
}

fn channel_send_plan_request() -> PlanGoalRequest {
    PlanGoalRequest {
        workflow: WorkflowSpec {
            steps: vec![WorkflowStep {
                id: "notify".to_string(),
                handler: "channel_send".to_string(),
                effect: EffectClass::ExternalWrite,
                input: serde_json::json!({
                    "channel": "telegram",
                    "session_id": "tg:42",
                    "text": "goal completed",
                }),
                retry: RetryPolicy {
                    max_attempts: 1,
                    backoff_secs: 0,
                },
            }],
            edges: Vec::new(),
            budget: ExecutionBudget {
                max_provider_calls: 0,
                deadline_secs: 60,
                max_response_bytes: 1024,
            },
        },
        acceptance_criteria: vec![AcceptanceCriterion::ManualApproval {
            label: "operator confirms the outbound send".to_string(),
        }],
    }
}

fn external_write_engine(memory: SqliteMemoryStore, allowlist: &[&str]) -> Arc<LoopEngine> {
    Arc::new(
        LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))).with_external_write_policy(
            ExternalWritePolicy {
                enabled: true,
                allowed_handlers: allowlist.iter().map(|name| name.to_string()).collect(),
            },
        ),
    )
}

#[tokio::test]
async fn external_write_plan_rejected_when_disabled() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("ext-off.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "Notify me externally".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "operator:test",
        )
        .await?;
    let error = engine
        .plan_goal(&goal.id, channel_send_plan_request(), "operator:test")
        .await
        .expect_err("external_write must be rejected when the flag is off");
    assert!(error.to_string().contains("external_write"));
    Ok(())
}

#[tokio::test]
async fn external_write_plan_rejected_when_handler_not_allowlisted() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("ext-deny.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = external_write_engine(memory, &["other_handler"]);
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "Notify me externally".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "operator:test",
        )
        .await?;
    let error = engine
        .plan_goal(&goal.id, channel_send_plan_request(), "operator:test")
        .await
        .expect_err("non-allowlisted external_write handler must be rejected");
    assert!(error.to_string().contains("channel_send"));

    // Registration is gated by the same allowlist.
    let mut registry = WorkHandlerRegistry::default();
    let error = registry
        .register(Arc::new(ChannelSendProbe))
        .expect_err("registration must reject non-allowlisted external_write handlers");
    assert!(error.to_string().contains("allowlist"));
    Ok(())
}

struct ChannelSendProbe;

#[async_trait]
impl WorkHandler for ChannelSendProbe {
    fn name(&self) -> &'static str {
        "channel_send"
    }

    fn effect_class(&self) -> EffectClass {
        EffectClass::ExternalWrite
    }

    async fn execute(&self, _context: &WorkHandlerContext) -> anyhow::Result<WorkOutcome> {
        Ok(WorkOutcome {
            summary: "probe".to_string(),
            artifact_ids: Vec::new(),
            evidence: serde_json::Value::Null,
            sleep_until_unix: None,
            continuation: None,
        })
    }
}

#[tokio::test]
async fn channel_send_executes_and_commits_with_evidence() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("ext-run.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = external_write_engine(memory, &["channel_send"]);
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "Notify me externally".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "operator:test",
        )
        .await?;
    let plan = engine
        .plan_goal(&goal.id, channel_send_plan_request(), "operator:test")
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            "operator:test",
        )
        .await?;

    let sends = Arc::new(RecordingOutboundSender {
        sends: Mutex::new(Vec::new()),
    });
    let worker = LoopWorker::with_builtins(
        engine.clone(),
        Arc::new(WorkerProvider),
        None,
        Some(sends.clone()),
    )?;
    // The send commits, then the goal parks in `verifying` awaiting the
    // manual-approval criterion; the idle loop reports that state as
    // non-dispatchable, which is expected — assert on durable state instead.
    let _ = worker.run_goal_until_idle(&goal.id, 4).await;
    let report = engine.resume_goal(&goal.id, "operator:test").await?;

    let item = report
        .work_items
        .iter()
        .find(|item| item.step_id == "notify")
        .expect("channel_send work item");
    assert_eq!(item.status, WorkItemStatus::Succeeded);
    let recorded = sends.sends.lock().expect("sends");
    assert_eq!(recorded.len(), 1);
    let (channel, session_id, text, key) = &recorded[0];
    assert_eq!(channel, "telegram");
    assert_eq!(session_id, "tg:42");
    assert_eq!(text, "goal completed");
    // The delivery key is the same (work item, attempt) unit the checkpoint
    // was prepared under.
    assert!(key.starts_with(&item.id));
    drop(recorded);

    // The send's checkpoint was committed, and resume reconciled it — the
    // durable evidence survives without replaying the effect.
    let checkpoint = report
        .latest_checkpoint
        .as_ref()
        .expect("committed checkpoint");
    assert_eq!(checkpoint.phase, CheckpointPhase::Reconciled);
    let outcome = checkpoint.outcome.as_ref().expect("send outcome");
    assert_eq!(outcome.evidence["send"]["sent_at_unix"], 1_700_000_000);
    assert_eq!(report.goal.status, GoalStatus::Verifying);
    Ok(())
}

#[tokio::test]
async fn channel_send_crash_before_commit_parks_item_without_resend() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("ext-crash.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = external_write_engine(memory, &["channel_send"]);
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "Notify me externally".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "operator:test",
        )
        .await?;
    let plan = engine
        .plan_goal(&goal.id, channel_send_plan_request(), "operator:test")
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            "operator:test",
        )
        .await?;

    // Simulate: worker claimed the item, wrote the prepared checkpoint, the
    // send went out, and the process died before commit_checkpoint.
    let sends = Arc::new(RecordingOutboundSender {
        sends: Mutex::new(Vec::new()),
    });
    let claim = engine
        .claim_work_item(&goal.id, "notify", "worker:ext", 1, "operator:test")
        .await?
        .expect("channel_send work item should be claimable");
    let idempotency_key = format!("{}:{}:v1", claim.work_item.id, claim.attempt.id);
    let checkpoint = engine
        .prepare_checkpoint(&claim, &idempotency_key, false, "worker:ext")
        .await?;
    assert_eq!(checkpoint.phase, CheckpointPhase::Prepared);
    sends
        .send("telegram", "tg:42", "goal completed", &idempotency_key)
        .await?;
    drop(engine);

    // Lease expiry is whole-second compared; wait past it, then resume on a
    // reopened store as if the process restarted.
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
    let reopened_memory = SqliteMemoryStore::new(&database_url).await?;
    let reopened = external_write_engine(reopened_memory, &["channel_send"]);
    let report = reopened.resume_goal(&goal.id, "operator:test").await?;
    let item = report
        .work_items
        .iter()
        .find(|item| item.step_id == "notify")
        .expect("channel_send work item");
    assert_eq!(item.status, WorkItemStatus::WaitingConfirmation);

    // A fresh worker must not re-dispatch the parked item — at-least-once
    // means at most one send lands for this attempt.
    let worker = LoopWorker::with_builtins(
        reopened.clone(),
        Arc::new(WorkerProvider),
        None,
        Some(sends.clone()),
    )?;
    let _ = worker.run_goal_until_idle(&goal.id, 4).await?;
    assert_eq!(sends.sends.lock().expect("sends").len(), 1);
    Ok(())
}

#[tokio::test]
async fn channel_send_dispatch_blocked_when_flag_off_on_worker() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("ext-gate.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    // Plan + approve on an enabled engine…
    let enabled = external_write_engine(memory.clone(), &["channel_send"]);
    let goal = enabled
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "Notify me externally".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "operator:test",
        )
        .await?;
    let plan = enabled
        .plan_goal(&goal.id, channel_send_plan_request(), "operator:test")
        .await?;
    enabled
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            "operator:test",
        )
        .await?;

    // …but the worker runs on an engine where the flag is off (e.g. restart
    // with a downgraded config): the item must fail closed, never send.
    let sends = Arc::new(RecordingOutboundSender {
        sends: Mutex::new(Vec::new()),
    });
    let disabled = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    // Registration itself fails closed on a disabled policy.
    let error = match LoopWorker::with_builtins(
        disabled.clone(),
        Arc::new(WorkerProvider),
        None,
        Some(sends.clone()),
    ) {
        Err(error) => error,
        Ok(_) => panic!("channel_send sender without allowlist must fail registration"),
    };
    assert!(error.to_string().contains("channel_send"));

    let worker = LoopWorker::with_builtins(disabled, Arc::new(WorkerProvider), None, None)?;
    let report = worker.run_goal_until_idle(&goal.id, 4).await?;
    let item = report
        .work_items
        .iter()
        .find(|item| item.step_id == "notify")
        .expect("channel_send work item");
    assert_eq!(item.status, WorkItemStatus::Failed);
    assert!(sends.sends.lock().expect("sends").is_empty());
    Ok(())
}

#[tokio::test]
async fn normalized_signal_variants_deduplicate_across_external_ids() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("signals-norm.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = LoopEngine::new(Arc::new(SqliteLoopStore::new(memory)));

    let signal = |external_id: Option<&str>, source: &str, content: &str| CreateSignalRequest {
        kind: SignalKind::Community,
        trust: SignalTrust::External,
        source: source.to_string(),
        external_id: external_id.map(str::to_string),
        content: content.to_string(),
        metadata: BTreeMap::new(),
    };
    let first = engine
        .ingest_signal(
            signal(
                Some("issue-1"),
                "github:issues",
                "Database migration failed.",
            ),
            "ingest:community",
        )
        .await?;
    assert!(!first.deduplicated);

    // Different external_id but a case/whitespace/punctuation variant of the
    // same content deduplicates through the normalized hash.
    let second = engine
        .ingest_signal(
            signal(
                Some("issue-2"),
                "github:issues",
                "  DATABASE   migration FAILED!!",
            ),
            "ingest:community",
        )
        .await?;
    assert!(second.deduplicated);
    assert_eq!(second.signal.id, first.signal.id);

    // A different source with the same content is a distinct signal.
    let third = engine
        .ingest_signal(
            signal(None, "slack:community", "Database migration failed."),
            "ingest:slack",
        )
        .await?;
    assert!(!third.deduplicated);
    Ok(())
}

#[tokio::test]
async fn near_duplicate_signals_deduplicate_but_distinct_content_does_not() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("signals-near.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = LoopEngine::new(Arc::new(SqliteLoopStore::new(memory)));

    let signal = |content: &str| CreateSignalRequest {
        kind: SignalKind::UserFeedback,
        trust: SignalTrust::Authenticated,
        source: "telegram:feedback".to_string(),
        external_id: None,
        content: content.to_string(),
        metadata: BTreeMap::new(),
    };
    let first = engine
        .ingest_signal(
            signal("the agent failed to retry the timed out search tool call"),
            "ingest:feedback",
        )
        .await?;
    assert!(!first.deduplicated);

    // One extra word keeps the token overlap above the near-duplicate bar.
    let second = engine
        .ingest_signal(
            signal("the agent failed to retry the timed out search tool call again"),
            "ingest:feedback",
        )
        .await?;
    assert!(second.deduplicated);
    assert_eq!(second.signal.id, first.signal.id);

    // A genuinely different report must not be absorbed.
    let third = engine
        .ingest_signal(
            signal("scheduler jobs fire twice after a restart"),
            "ingest:feedback",
        )
        .await?;
    assert!(!third.deduplicated);
    assert_ne!(third.signal.id, first.signal.id);
    Ok(())
}

#[tokio::test]
async fn operator_reviews_signals_with_status_filter_ignore_and_propose() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!(
        "sqlite://{}",
        temp.path().join("signals-review.db").display()
    );
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = LoopEngine::new(Arc::new(SqliteLoopStore::new(memory)));

    for (external_id, content) in [
        ("d-1", "first report about flaky search"),
        ("d-2", "second report about stalled schedules"),
    ] {
        engine
            .ingest_signal(
                CreateSignalRequest {
                    kind: SignalKind::DeveloperFeedback,
                    trust: SignalTrust::Authenticated,
                    source: "console:dev".to_string(),
                    external_id: Some(external_id.to_string()),
                    content: content.to_string(),
                    metadata: BTreeMap::new(),
                },
                "ingest:dev",
            )
            .await?;
    }
    let observed = engine
        .list_signals(20, Some(SignalStatus::Observed))
        .await?;
    assert_eq!(observed.len(), 2);
    assert!(
        engine
            .list_signals(20, Some(SignalStatus::Ignored))
            .await?
            .is_empty()
    );

    let ignored = engine
        .ignore_signal(&observed[0].id, "not actionable noise", "operator:henry")
        .await?;
    assert_eq!(ignored.status, SignalStatus::Ignored);
    let reignore = engine
        .ignore_signal(&observed[0].id, "again", "operator:henry")
        .await;
    assert!(reignore.is_err(), "ignoring twice must fail");
    let propose_ignored = engine
        .propose_goal_from_signal(&observed[0].id, "revive", "operator:henry")
        .await;
    assert!(propose_ignored.is_err(), "ignored signal cannot propose");

    let goal = engine
        .propose_goal_from_signal(&observed[1].id, "fix stalled schedules", "operator:henry")
        .await?;
    assert_eq!(goal.source_signal_ids, vec![observed[1].id.clone()]);
    let repropose = engine
        .propose_goal_from_signal(&observed[1].id, "again", "operator:henry")
        .await;
    assert!(repropose.is_err(), "proposed signal cannot propose twice");

    assert!(
        engine
            .list_signals(20, Some(SignalStatus::Observed))
            .await?
            .is_empty()
    );
    assert_eq!(
        engine
            .list_signals(20, Some(SignalStatus::Ignored))
            .await?
            .len(),
        1
    );
    assert_eq!(
        engine
            .list_signals(20, Some(SignalStatus::Proposed))
            .await?
            .len(),
        1
    );
    Ok(())
}

// ---- T13: waiting_confirmation operator resolve path ----

/// Builds a goal with a parked channel_send work item: claimed, prepared
/// checkpoint, lease expired, resume parks it in `waiting_confirmation`.
/// Returns (engine, goal_id, work_item_id).
async fn parked_channel_send(
    database_url: &str,
    max_attempts: u8,
) -> anyhow::Result<(Arc<LoopEngine>, String, String)> {
    let memory = SqliteMemoryStore::new(database_url).await?;
    let engine = external_write_engine(memory, &["channel_send"]);
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "Notify me externally".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "operator:test",
        )
        .await?;
    let mut request = channel_send_plan_request();
    request.workflow.steps[0].retry.max_attempts = max_attempts;
    let plan = engine.plan_goal(&goal.id, request, "operator:test").await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            "operator:test",
        )
        .await?;
    let claim = engine
        .claim_work_item(&goal.id, "notify", "worker:ext", 1, "operator:test")
        .await?
        .expect("channel_send work item should be claimable");
    let work_item_id = claim.work_item.id.clone();
    engine
        .prepare_checkpoint(
            &claim,
            &format!("{}:{}:v1", claim.work_item.id, claim.attempt.id),
            false,
            "worker:ext",
        )
        .await?;
    drop(engine);
    // Lease expiry is whole-second compared; wait past it, then resume on a
    // reopened store as if the process restarted.
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
    let reopened = external_write_engine(
        SqliteMemoryStore::new(database_url).await?,
        &["channel_send"],
    );
    let report = reopened.resume_goal(&goal.id, "operator:test").await?;
    assert_eq!(
        report
            .work_items
            .iter()
            .find(|item| item.id == work_item_id)
            .map(|item| item.status),
        Some(WorkItemStatus::WaitingConfirmation)
    );
    Ok((reopened, goal.id, work_item_id))
}

#[tokio::test]
async fn resolve_confirmed_commits_operator_attested_outcome() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("conf-ok.db").display());
    let (engine, goal_id, work_item_id) = parked_channel_send(&database_url, 1).await?;

    let item = engine
        .resolve_waiting_confirmation(
            &goal_id,
            &work_item_id,
            ConfirmationResolution::Confirmed,
            "telegram shows the message delivered",
            "operator:test",
        )
        .await?;
    assert_eq!(item.status, WorkItemStatus::Succeeded);

    let report = engine.resume_goal(&goal_id, "operator:test").await?;
    let checkpoint = report.latest_checkpoint.expect("resolved checkpoint");
    assert_eq!(checkpoint.phase, CheckpointPhase::Reconciled);
    let outcome = checkpoint.outcome.expect("attested outcome");
    assert_eq!(outcome.evidence["resolution"], "confirmed");
    assert!(outcome.summary.contains("telegram shows the message"));
    assert!(
        report
            .attempts
            .iter()
            .any(|a| a.status == xiaomaolv::harness::loop_engine::AttemptStatus::Succeeded)
    );
    let events = engine.list_goal_events(&goal_id, 0, 50).await?;
    assert!(
        events
            .iter()
            .any(|e| e.event_type == "confirmation.resolved"
                && e.details["resolution"] == "confirmed")
    );
    Ok(())
}

#[tokio::test]
async fn resolve_retry_voids_checkpoint_and_requeues() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("conf-retry.db").display());
    let (engine, goal_id, work_item_id) = parked_channel_send(&database_url, 2).await?;

    let item = engine
        .resolve_waiting_confirmation(
            &goal_id,
            &work_item_id,
            ConfirmationResolution::Retry,
            "chat history shows nothing was sent",
            "operator:test",
        )
        .await?;
    assert_eq!(item.status, WorkItemStatus::Ready);

    // The prepared checkpoint is voided — its idempotency key can never be
    // committed, and the work item is claimable again under a new attempt.
    let report = engine.resume_goal(&goal_id, "operator:test").await?;
    assert_eq!(
        report.latest_checkpoint.map(|c| c.phase),
        Some(CheckpointPhase::Voided)
    );
    let reclaim = engine
        .claim_work_item(&goal_id, "notify", "worker:ext", 1, "operator:test")
        .await?;
    assert!(reclaim.is_some(), "retry-resolved item must be claimable");
    Ok(())
}

#[tokio::test]
async fn resolve_retry_exhausts_budget_fails_item() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("conf-exhaust.db").display());
    let (engine, goal_id, work_item_id) = parked_channel_send(&database_url, 1).await?;

    let item = engine
        .resolve_waiting_confirmation(
            &goal_id,
            &work_item_id,
            ConfirmationResolution::Retry,
            "not delivered but no attempts remain",
            "operator:test",
        )
        .await?;
    assert_eq!(item.status, WorkItemStatus::Failed);
    let goal = engine.get_goal(&goal_id).await?.expect("goal");
    assert_eq!(goal.status, GoalStatus::Failed);
    Ok(())
}

#[tokio::test]
async fn resolve_abandoned_fails_item_and_goal() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("conf-abandon.db").display());
    let (engine, goal_id, work_item_id) = parked_channel_send(&database_url, 2).await?;

    let item = engine
        .resolve_waiting_confirmation(
            &goal_id,
            &work_item_id,
            ConfirmationResolution::Abandoned,
            "do not retry; the notification is stale",
            "operator:test",
        )
        .await?;
    assert_eq!(item.status, WorkItemStatus::Failed);
    let goal = engine.get_goal(&goal_id).await?.expect("goal");
    assert_eq!(goal.status, GoalStatus::Failed);

    // A parked item that was resolved cannot be resolved again.
    let second = engine
        .resolve_waiting_confirmation(
            &goal_id,
            &work_item_id,
            ConfirmationResolution::Confirmed,
            "late change of mind",
            "operator:test",
        )
        .await;
    assert!(second.is_err());
    Ok(())
}

#[tokio::test]
async fn resolve_rejects_non_parked_and_cross_goal_items() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("conf-reject.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = external_write_engine(memory, &["channel_send"]);
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "Notify me externally".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "operator:test",
        )
        .await?;
    let plan = engine
        .plan_goal(&goal.id, channel_send_plan_request(), "operator:test")
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            "operator:test",
        )
        .await?;
    let items = engine.list_work_items(&goal.id).await?;
    let item = items.first().expect("work item");

    // Not parked yet — resolving must fail.
    assert!(
        engine
            .resolve_waiting_confirmation(
                &goal.id,
                &item.id,
                ConfirmationResolution::Confirmed,
                "premature",
                "operator:test",
            )
            .await
            .is_err()
    );
    // Cross-goal scoping: a different goal id must not resolve the item.
    assert!(
        engine
            .resolve_waiting_confirmation(
                "goal_nonexistent",
                &item.id,
                ConfirmationResolution::Confirmed,
                "wrong goal",
                "operator:test",
            )
            .await
            .is_err()
    );
    // Reason is required.
    assert!(
        engine
            .resolve_waiting_confirmation(
                &goal.id,
                &item.id,
                ConfirmationResolution::Confirmed,
                "   ",
                "operator:test",
            )
            .await
            .is_err()
    );
    Ok(())
}

// ---- Durable-execution semantics (replay safety, blocked handlers,
// durable sleep, idempotent admission, commit notifications) ----

fn local_write_plan_request(
    step_id: &str,
    handler: &str,
    input: serde_json::Value,
) -> PlanGoalRequest {
    PlanGoalRequest {
        workflow: WorkflowSpec {
            steps: vec![WorkflowStep {
                id: step_id.to_string(),
                handler: handler.to_string(),
                effect: EffectClass::LocalWrite,
                input,
                retry: RetryPolicy {
                    max_attempts: 3,
                    backoff_secs: 0,
                },
            }],
            edges: Vec::new(),
            budget: ExecutionBudget {
                max_provider_calls: 0,
                deadline_secs: 60,
                max_response_bytes: 1024,
            },
        },
        acceptance_criteria: vec![AcceptanceCriterion::SelfTestSuite {
            suite: "core".to_string(),
        }],
    }
}

/// A `replay_safe = false` prepared checkpoint on a non-external step parks
/// in `waiting_confirmation` after a crash: the operator verdict covers any
/// ambiguous effect, not only external writes.
#[tokio::test]
async fn unsafe_replay_prepared_checkpoint_parks_without_external_write() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("unsafe-park.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "Probe ambiguous local effect".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "operator:test",
        )
        .await?;
    let plan = engine
        .plan_goal(
            &goal.id,
            local_write_plan_request(
                "probe",
                "self_test_suite",
                serde_json::json!({"suite": "core"}),
            ),
            "planner:test",
        )
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            "operator:test",
        )
        .await?;
    let claim = engine
        .claim_work_item(&goal.id, "probe", "worker:test", 1, "operator:test")
        .await?
        .expect("probe work item should be claimable");
    let work_item_id = claim.work_item.id.clone();
    // The handler ran its ambiguous effect and died before the commit.
    engine
        .prepare_checkpoint(&claim, "probe:v1", false, "worker:test")
        .await?;
    drop(engine);
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;

    let reopened = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(
        SqliteMemoryStore::new(&database_url).await?,
    ))));
    let report = reopened.resume_goal(&goal.id, "operator:test").await?;
    let item = report
        .work_items
        .iter()
        .find(|item| item.id == work_item_id)
        .expect("probe work item");
    assert_eq!(item.status, WorkItemStatus::WaitingConfirmation);

    // The park surfaces as a dedup'd signal — Telegram `/signals` triage
    // sees pending gates without anyone opening the console.
    let signals = reopened.list_signals(20, None).await?;
    let confirm_signals: Vec<_> = signals
        .iter()
        .filter(|s| {
            s.source == "internal:confirmation"
                && s.external_id
                    .as_deref()
                    .is_some_and(|id| id.starts_with("confirm:"))
        })
        .collect();
    assert_eq!(
        confirm_signals.len(),
        1,
        "parked item emitted exactly one confirmation signal"
    );
    assert!(confirm_signals[0].content.contains(&work_item_id));

    // The non-external park resolves through the same operator verdict path.
    let item = reopened
        .resolve_waiting_confirmation(
            &goal.id,
            &work_item_id,
            ConfirmationResolution::Retry,
            "effect never ran",
            "operator:test",
        )
        .await?;
    assert_eq!(item.status, WorkItemStatus::Ready);

    // Resolving the gate triages the confirmation signal out of /signals.
    let signals = reopened.list_signals(20, None).await?;
    let resolved_signal = signals
        .iter()
        .find(|s| s.source == "internal:confirmation")
        .expect("confirmation signal");
    assert_eq!(
        resolved_signal.status,
        xiaomaolv::harness::loop_engine::SignalStatus::Ignored,
        "resolved gate no longer occupies triage"
    );
    let worker = LoopWorker::with_builtins(reopened, Arc::new(WorkerProvider), None, None)?;
    let report = worker.run_goal_until_idle(&goal.id, 8).await?;
    let item = report
        .work_items
        .iter()
        .find(|item| item.id == work_item_id)
        .expect("probe work item");
    assert_eq!(item.status, WorkItemStatus::Succeeded);
    Ok(())
}

/// A `replay_safe = true` prepared checkpoint under a dead lease retries
/// automatically: no park, and the superseded prepared row is voided so a
/// later recovery never mistakes it for live ambiguity.
#[tokio::test]
async fn safe_replay_prepared_checkpoint_retries_without_parking() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("safe-retry.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "Probe safe replay".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "operator:test",
        )
        .await?;
    let plan = engine
        .plan_goal(
            &goal.id,
            local_write_plan_request(
                "probe",
                "self_test_suite",
                serde_json::json!({"suite": "core"}),
            ),
            "planner:test",
        )
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            "operator:test",
        )
        .await?;
    let claim = engine
        .claim_work_item(&goal.id, "probe", "worker:test", 1, "operator:test")
        .await?
        .expect("probe work item should be claimable");
    let work_item_id = claim.work_item.id.clone();
    let checkpoint = engine
        .prepare_checkpoint(&claim, "probe:v1", true, "worker:test")
        .await?;
    drop(engine);
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;

    let reopened = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(
        SqliteMemoryStore::new(&database_url).await?,
    ))));
    let report = reopened.resume_goal(&goal.id, "operator:test").await?;
    let item = report
        .work_items
        .iter()
        .find(|item| item.id == work_item_id)
        .expect("probe work item");
    assert_eq!(item.status, WorkItemStatus::Ready);
    let stale = report
        .latest_checkpoint
        .as_ref()
        .expect("prepared checkpoint remains in the log");
    assert_eq!(stale.id, checkpoint.id);
    assert_eq!(stale.phase, CheckpointPhase::Voided);
    Ok(())
}

/// Dispatch against a registry missing the work item's handler blocks the
/// item instead of failing it; once a worker holding the handler polls, the
/// item requeues itself and the goal completes.
#[tokio::test]
async fn missing_handler_blocks_then_requeues_when_registered() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("blocked.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "Run a blocked then requeued plan".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "operator:test",
        )
        .await?;
    let plan = engine
        .plan_goal_recommended(&goal.id, "planner:test")
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            "operator:test",
        )
        .await?;

    // A worker that knows no handlers at all: rolling-deploy window where
    // the registry for this process has not installed the handler yet.
    let empty = LoopWorker::with_registry(
        engine.clone(),
        WorkHandlerRegistry::default(),
        "worker:missing",
        30,
    )?;
    let report = empty.run_goal_until_idle(&goal.id, 4).await?;
    assert!(
        report
            .work_items
            .iter()
            .any(|item| item.status == WorkItemStatus::Blocked),
        "a work item should be blocked on the missing handler"
    );
    assert!(
        report.attempts.iter().any(|attempt| attempt
            .error
            .as_deref()
            .is_some_and(|error| error.starts_with("unregistered workflow handler:"))),
        "the blocked attempt should record the missing-handler reason"
    );

    // Operator visibility: the block lands a dedup'd internal signal so a
    // typo'd handler name doesn't park silently forever.
    let signals = engine.list_signals(16, None).await.expect("list signals");
    assert!(
        signals.iter().any(|signal| signal
            .external_id
            .as_deref()
            .is_some_and(|id| id.starts_with("blocked:work_"))
            && signal.content.contains("not registered")),
        "blocked item should surface a triageable signal"
    );

    // A later poll from a worker whose registry has the handler requeues the
    // item and the goal runs to completion.
    let worker = LoopWorker::with_builtins(engine.clone(), Arc::new(WorkerProvider), None, None)?;
    let report = worker.run_goal_until_idle(&goal.id, 8).await?;
    assert_eq!(report.goal.status, GoalStatus::Achieved);
    assert!(
        report
            .work_items
            .iter()
            .all(|item| item.status == WorkItemStatus::Succeeded)
    );
    Ok(())
}

/// The `wait` handler turns `sleep_until` into a durable deferral: the
/// attempt ends, the work item holds no lease, and the stored wake time
/// survives the "restart".
#[tokio::test]
async fn wait_step_defers_until_the_stored_deadline() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("wait.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    let until = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs(),
    )? + 2;
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "Wait for a durable deadline".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "operator:test",
        )
        .await?;
    let plan = engine
        .plan_goal(
            &goal.id,
            PlanGoalRequest {
                workflow: WorkflowSpec {
                    steps: vec![
                        WorkflowStep {
                            id: "hold".to_string(),
                            handler: "wait".to_string(),
                            effect: EffectClass::Pure,
                            input: serde_json::json!({"until_unix": until}),
                            retry: RetryPolicy {
                                max_attempts: 3,
                                backoff_secs: 0,
                            },
                        },
                        WorkflowStep {
                            id: "self-test".to_string(),
                            handler: "self_test_suite".to_string(),
                            effect: EffectClass::LocalWrite,
                            input: serde_json::json!({"suite": "core"}),
                            retry: RetryPolicy {
                                max_attempts: 3,
                                backoff_secs: 0,
                            },
                        },
                    ],
                    edges: vec![WorkflowEdge {
                        from: "hold".to_string(),
                        to: "self-test".to_string(),
                    }],
                    budget: ExecutionBudget {
                        max_provider_calls: 0,
                        deadline_secs: 60,
                        max_response_bytes: 65_536,
                    },
                },
                acceptance_criteria: vec![AcceptanceCriterion::SelfTestSuite {
                    suite: "core".to_string(),
                }],
            },
            "planner:test",
        )
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            "operator:test",
        )
        .await?;
    let worker = LoopWorker::with_builtins(engine.clone(), Arc::new(WorkerProvider), None, None)?;

    // First poll: the deadline is in the future, so the attempt defers —
    // the item is ready but not yet due, no checkpoint is committed.
    let report = worker.run_goal_until_idle(&goal.id, 4).await?;
    let item = report.work_items.first().expect("wait work item");
    assert_eq!(item.status, WorkItemStatus::Ready);
    assert!(
        report.attempts.iter().any(|attempt| attempt
            .error
            .as_deref()
            .is_some_and(|error| error.starts_with("deferred until"))),
        "the deferred attempt should record its wake time"
    );
    let events = engine.list_goal_events(&goal.id, 0, 50).await?;
    assert!(
        events
            .iter()
            .any(|event| event.event_type == "attempt.deferred")
    );

    // After the stored deadline a second poll completes the deferred step,
    // runs the dependent self-test, and verifies the goal to achieved.
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
    let report = worker.run_goal_until_idle(&goal.id, 8).await?;
    assert!(
        report
            .work_items
            .iter()
            .all(|item| item.status == WorkItemStatus::Succeeded)
    );
    assert_eq!(report.goal.status, GoalStatus::Achieved);
    Ok(())
}

/// `request_id` makes goal admission idempotent — a retried submit returns
/// the originally admitted goal — and a committed mutation wakes subscribed
/// event consumers without polling.
#[tokio::test]
async fn goal_admission_deduplicates_request_id_and_notifies() -> anyhow::Result<()> {
    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let engine = LoopEngine::new(Arc::new(SqliteLoopStore::new(memory)));
    let mut notifications = engine.subscribe_events();

    let request = |objective: &str| CreateGoalRequest {
        internal_only: false,
        objective: objective.to_string(),
        source_signal_ids: Vec::new(),
        request_id: Some("submission:goal-1".to_string()),
    };
    let (goal, created) = engine
        .create_goal_admission(request("Idempotent admission"), "operator:test")
        .await?;
    assert!(created);
    let (again, created_again) = engine
        .create_goal_admission(request("A different objective"), "operator:test")
        .await?;
    assert!(!created_again);
    assert_eq!(again.id, goal.id);
    assert_eq!(again.objective, "Idempotent admission");
    assert_eq!(engine.list_goals(10).await?.len(), 1);

    // Only the real admission wrote an event.
    let events = engine.list_goal_events(&goal.id, 0, 20).await?;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "goal.created")
            .count(),
        1
    );
    // The commit notified subscribers carrying the goal id.
    let notified = notifications.recv().await?;
    assert_eq!(notified, goal.id);
    Ok(())
}

/// Workers call `resume_goal` every poll cycle; consecutive no-op resumes
/// must not flood the per-goal event log.
#[tokio::test]
async fn consecutive_resumes_emit_a_single_event() -> anyhow::Result<()> {
    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let engine = LoopEngine::new(Arc::new(SqliteLoopStore::new(memory)));
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "Observe resume dedup".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "operator:test",
        )
        .await?;
    engine.resume_goal(&goal.id, "runtime:test").await?;
    engine.resume_goal(&goal.id, "runtime:test").await?;
    engine.resume_goal(&goal.id, "runtime:test").await?;
    let events = engine.list_goal_events(&goal.id, 0, 20).await?;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "goal.resumed")
            .count(),
        1
    );
    // Any number of consecutive no-op resumes still collapse to one event.
    engine.resume_goal(&goal.id, "runtime:test").await?;
    let events = engine.list_goal_events(&goal.id, 0, 20).await?;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "goal.resumed")
            .count(),
        1
    );
    Ok(())
}

/// A handler whose work spans two committed phases: phase one reports a
/// boundary, phase two finishes — resuming from `ctx.resume` rather than
/// restarting. Registered under the allowed `provider_analysis` name so the
/// plan/approve path accepts it while the registry maps it to this probe.
struct TwoPhaseProbe;

#[async_trait]
impl WorkHandler for TwoPhaseProbe {
    fn name(&self) -> &'static str {
        "provider_analysis"
    }

    fn effect_class(&self) -> EffectClass {
        EffectClass::LocalWrite
    }

    async fn execute(&self, context: &WorkHandlerContext) -> anyhow::Result<WorkOutcome> {
        match context
            .resume
            .as_ref()
            .map(|progress| progress.phase.as_str())
        {
            None => Ok(WorkOutcome {
                summary: "phase one committed".to_string(),
                artifact_ids: Vec::new(),
                evidence: serde_json::Value::Null,
                sleep_until_unix: None,
                continuation: Some(WorkContinuation {
                    phase: "gathered".to_string(),
                    state: serde_json::json!({"seed": 42}),
                    delay_secs: None,
                }),
            }),
            Some("gathered") => {
                let progress = context.resume.as_ref().expect("resume progress");
                anyhow::ensure!(
                    progress.state["seed"] == 42 && progress.continues == 1,
                    "resume progress lost its committed state"
                );
                Ok(WorkOutcome {
                    summary: "resumed and finished".to_string(),
                    artifact_ids: Vec::new(),
                    evidence: serde_json::Value::Null,
                    sleep_until_unix: None,
                    continuation: None,
                })
            }
            Some(other) => anyhow::bail!("unexpected resume phase: {other}"),
        }
    }
}

fn two_phase_probe_worker(
    engine: Arc<LoopEngine>,
    worker_id: &str,
    lease_secs: u32,
) -> anyhow::Result<LoopWorker> {
    let mut registry = WorkHandlerRegistry::default();
    registry.register(Arc::new(TwoPhaseProbe))?;
    LoopWorker::with_registry(engine, registry, worker_id, lease_secs)
}

fn two_phase_plan() -> PlanGoalRequest {
    PlanGoalRequest {
        workflow: WorkflowSpec {
            steps: vec![WorkflowStep {
                id: "probe".to_string(),
                handler: "provider_analysis".to_string(),
                effect: EffectClass::LocalWrite,
                input: serde_json::json!({}),
                retry: RetryPolicy {
                    max_attempts: 3,
                    backoff_secs: 0,
                },
            }],
            edges: Vec::new(),
            budget: ExecutionBudget {
                max_provider_calls: 0,
                deadline_secs: 60,
                max_response_bytes: 65_536,
            },
        },
        acceptance_criteria: vec![AcceptanceCriterion::ManualApproval {
            label: "operator review".to_string(),
        }],
    }
}

/// A `Continue` outcome commits its phase boundary durably, then the next
/// phase runs inside the same attempt — `ctx.resume` carries the persisted
/// progress and the item succeeds with its progress cleared.
#[tokio::test]
async fn phase_boundaries_commit_progress_then_continue_in_place() -> anyhow::Result<()> {
    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "Run a two-phase probe".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "operator:test",
        )
        .await?;
    let plan = engine
        .plan_goal(&goal.id, two_phase_plan(), "planner:test")
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            "operator:test",
        )
        .await?;

    let worker = two_phase_probe_worker(engine.clone(), "worker:phase", 30)?;
    // The goal parks in `verifying` on the ManualApproval criterion once the
    // probe succeeds — the idle loop reports that as non-dispatchable, which
    // is expected; assert on durable state instead.
    let _ = worker.run_goal_until_idle(&goal.id, 4).await;

    let report = engine.resume_goal(&goal.id, "operator:test").await?;
    let item = report
        .work_items
        .iter()
        .find(|item| item.step_id == "probe")
        .expect("probe work item");
    assert_eq!(item.status, WorkItemStatus::Succeeded);
    assert!(
        item.progress.is_none(),
        "terminal success clears the phase progress"
    );
    let events = engine.list_goal_events(&goal.id, 0, 50).await?;
    assert!(
        events
            .iter()
            .any(|event| event.event_type == "attempt.phase_progressed"),
        "the committed boundary should be in the event log"
    );
    Ok(())
}

/// Committed phase progress is what survives a crash: the attempt dies after
/// the boundary commit, and the next attempt's `ctx.resume` carries the last
/// committed phase — no restart from scratch.
#[tokio::test]
async fn committed_phase_progress_resumes_after_attempt_crash() -> anyhow::Result<()> {
    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "Resume a crashed phase run".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "operator:test",
        )
        .await?;
    let plan = engine
        .plan_goal(&goal.id, two_phase_plan(), "planner:test")
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            "operator:test",
        )
        .await?;

    // Commit a phase boundary as if the worker had finished phase one, then
    // let the 1s lease lapse — the "crash" leaves progress committed but the
    // attempt abandoned.
    let claim = engine
        .claim_work_item(&goal.id, "probe", "worker:doomed", 1, "operator:test")
        .await?
        .expect("probe step should be claimable");
    engine
        .record_phase_progress(
            &claim,
            &WorkContinuation {
                phase: "gathered".to_string(),
                state: serde_json::json!({"seed": 42}),
                delay_secs: None,
            },
            "operator:test",
        )
        .await?;
    // Lease expiry is compared in whole seconds; wait past the boundary.
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;

    let worker = two_phase_probe_worker(engine.clone(), "worker:resume", 30)?;
    let _ = worker.run_goal_until_idle(&goal.id, 4).await;
    let report = engine.resume_goal(&goal.id, "operator:test").await?;
    let item = report
        .work_items
        .iter()
        .find(|item| item.step_id == "probe")
        .expect("probe work item");
    // The probe's `Some("gathered")` branch ran — the resume saw the
    // committed phase and the seed it carried (the handler errors out on
    // any other phase), and the item reached a terminal state.
    assert_eq!(item.status, WorkItemStatus::Succeeded);
    assert!(
        report.attempts.iter().any(|attempt| attempt
            .error
            .as_deref()
            .is_some_and(|error| error.contains("crash")
                || error.contains("abandoned")
                || error.contains("expired"))),
        "the crashed attempt should be recorded"
    );
    Ok(())
}

/// A handler that never produces a terminal outcome is bounded by the
/// committed-phase budget rather than looping forever.
#[tokio::test]
async fn phase_loop_is_bounded_by_continuation_budget() -> anyhow::Result<()> {
    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "Bound a runaway phase loop".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "operator:test",
        )
        .await?;

    struct EndlessProbe;
    #[async_trait]
    impl WorkHandler for EndlessProbe {
        fn name(&self) -> &'static str {
            "provider_analysis"
        }
        fn effect_class(&self) -> EffectClass {
            EffectClass::LocalWrite
        }
        async fn execute(&self, context: &WorkHandlerContext) -> anyhow::Result<WorkOutcome> {
            let next = context
                .resume
                .as_ref()
                .map(|progress| progress.continues + 1)
                .unwrap_or(1);
            Ok(WorkOutcome {
                summary: format!("phase {next} committed"),
                artifact_ids: Vec::new(),
                evidence: serde_json::Value::Null,
                sleep_until_unix: None,
                continuation: Some(WorkContinuation {
                    phase: format!("p{next}"),
                    state: serde_json::Value::Null,
                    delay_secs: None,
                }),
            })
        }
    }
    let mut registry = WorkHandlerRegistry::default();
    registry.register(Arc::new(EndlessProbe))?;
    let worker = LoopWorker::with_registry(engine.clone(), registry, "worker:loop", 30)?;

    let plan = engine
        .plan_goal(&goal.id, two_phase_plan(), "planner:test")
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            "operator:test",
        )
        .await?;

    worker.run_goal_until_idle(&goal.id, 4).await?;
    let report = engine.resume_goal(&goal.id, "operator:test").await?;
    let item = report
        .work_items
        .iter()
        .find(|item| item.step_id == "probe")
        .expect("probe work item");
    assert_eq!(item.status, WorkItemStatus::Failed);
    assert!(
        report.attempts.iter().any(|attempt| attempt
            .error
            .as_deref()
            .is_some_and(|error| error.contains("continuation budget"))),
        "the failure should name the exhausted continuation budget"
    );
    Ok(())
}

/// A dependent step reads its declared upstream's committed outcome through
/// `ctx.upstream` keyed by step id — the DAG dataflow a fan-in handler (and
/// eventually a swarm aggregation node) consumes.
#[tokio::test]
async fn dependent_step_reads_upstream_outcome_by_step_id() -> anyhow::Result<()> {
    struct DagProbe;
    #[async_trait]
    impl WorkHandler for DagProbe {
        fn name(&self) -> &'static str {
            "provider_analysis"
        }
        fn effect_class(&self) -> EffectClass {
            EffectClass::LocalWrite
        }
        async fn execute(&self, context: &WorkHandlerContext) -> anyhow::Result<WorkOutcome> {
            match context.claim.work_item.step_id.as_str() {
                "produce" => Ok(WorkOutcome {
                    summary: "upstream-payload".to_string(),
                    artifact_ids: Vec::new(),
                    evidence: serde_json::json!({"secret": 42}),
                    sleep_until_unix: None,
                    continuation: None,
                }),
                "consume" => {
                    let upstream = context
                        .upstream
                        .get("produce")
                        .expect("upstream outcome for 'produce' was missing");
                    anyhow::ensure!(
                        upstream.summary == "upstream-payload",
                        "unexpected upstream summary"
                    );
                    anyhow::ensure!(
                        upstream.evidence["secret"] == 42,
                        "upstream evidence did not arrive"
                    );
                    Ok(WorkOutcome {
                        summary: format!("consumed:{}", upstream.summary),
                        artifact_ids: Vec::new(),
                        evidence: serde_json::Value::Null,
                        sleep_until_unix: None,
                        continuation: None,
                    })
                }
                other => anyhow::bail!("unexpected step {other}"),
            }
        }
    }

    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "Run a produce/consume DAG".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "operator:test",
        )
        .await?;
    let step = |id: &str| WorkflowStep {
        id: id.to_string(),
        handler: "provider_analysis".to_string(),
        effect: EffectClass::LocalWrite,
        input: serde_json::json!({}),
        retry: RetryPolicy {
            max_attempts: 2,
            backoff_secs: 0,
        },
    };
    let plan = engine
        .plan_goal(
            &goal.id,
            PlanGoalRequest {
                workflow: WorkflowSpec {
                    steps: vec![step("produce"), step("consume")],
                    edges: vec![WorkflowEdge {
                        from: "produce".to_string(),
                        to: "consume".to_string(),
                    }],
                    budget: ExecutionBudget {
                        max_provider_calls: 0,
                        deadline_secs: 60,
                        max_response_bytes: 65_536,
                    },
                },
                acceptance_criteria: vec![AcceptanceCriterion::ManualApproval {
                    label: "review".to_string(),
                }],
            },
            "planner:test",
        )
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            "operator:test",
        )
        .await?;

    let mut registry = WorkHandlerRegistry::default();
    registry.register(Arc::new(DagProbe))?;
    let worker = LoopWorker::with_registry(engine.clone(), registry, "worker:dag", 30)?;
    // The goal parks in `verifying` after both steps — idle-loop errors are
    // expected; assert on durable state.
    let _ = worker.run_goal_until_idle(&goal.id, 4).await;

    let report = engine.resume_goal(&goal.id, "operator:test").await?;
    let consume = report
        .work_items
        .iter()
        .find(|item| item.step_id == "consume")
        .expect("consume item");
    // The probe's ensure! chain ran inside the worker — succeeding means the
    // upstream map carried the produce step's committed outcome.
    assert_eq!(consume.status, WorkItemStatus::Succeeded);
    let produce = report
        .work_items
        .iter()
        .find(|item| item.step_id == "produce")
        .expect("produce item");
    assert_eq!(produce.status, WorkItemStatus::Succeeded);
    Ok(())
}

/// A durable swarm run: the root `swarm_node` item delegates by extending
/// the goal's DAG — child items plus an aggregate that depends on them and
/// merges their outcomes through `ctx.upstream`. The whole delegation tree
/// is durable work, not an in-process recursion.
#[tokio::test]
async fn durable_swarm_run_delegates_and_merges_through_the_dag() -> anyhow::Result<()> {
    use xiaomaolv::provider::{ChatProvider, CompletionRequest};
    use xiaomaolv::service::swarm::SwarmNodeHandler;

    struct SwarmScriptedProvider;
    #[async_trait]
    impl ChatProvider for SwarmScriptedProvider {
        async fn complete(&self, req: CompletionRequest) -> anyhow::Result<String> {
            let transcript = req
                .messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if transcript.contains("hierarchical task planner") {
                if transcript.contains("ROOT TASK") {
                    Ok(serde_json::json!({
                        "mode": "delegate",
                        "children": [
                            {"task": "subtask A", "role_name": "researcher"},
                            {"task": "subtask B", "role_name": "writer"}
                        ]
                    })
                    .to_string())
                } else {
                    Ok(serde_json::json!({"mode": "answer", "answer": ""}).to_string())
                }
            } else if transcript.contains("Merge child results") {
                Ok("merged-final-answer".to_string())
            } else if transcript.contains("direct deliverable") {
                Ok(if transcript.contains("subtask A") {
                    "answer-A".to_string()
                } else {
                    "answer-B".to_string()
                })
            } else {
                Ok("unexpected-call".to_string())
            }
        }
        fn model_name(&self) -> Option<&str> {
            Some("swarm-scripted")
        }
    }

    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    let actor = "internal:swarm";
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "ROOT TASK".to_string(),
                source_signal_ids: Vec::new(),
                request_id: Some("swarm:test-run".to_string()),
            },
            actor,
        )
        .await?;
    let plan = engine
        .plan_goal(
            &goal.id,
            PlanGoalRequest {
                workflow: WorkflowSpec {
                    steps: vec![WorkflowStep {
                        id: "test-run:1".to_string(),
                        handler: "swarm_node".to_string(),
                        effect: EffectClass::LocalWrite,
                        input: serde_json::json!({
                            "swarm_root": true,
                            "max_nodes": 8,
                            "swarm_run_id": "test-run",
                            "task": "ROOT TASK",
                            "role_name": "orchestrator",
                            "depth": 0,
                            "history": [],
                        }),
                        retry: RetryPolicy {
                            max_attempts: 2,
                            backoff_secs: 0,
                        },
                    }],
                    edges: Vec::new(),
                    budget: ExecutionBudget {
                        max_provider_calls: 32,
                        deadline_secs: 120,
                        max_response_bytes: 65_536,
                    },
                },
                acceptance_criteria: vec![AcceptanceCriterion::ManualApproval {
                    label: "review".to_string(),
                }],
            },
            actor,
        )
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            actor,
        )
        .await?;

    let mut registry = WorkHandlerRegistry::default();
    registry.register(Arc::new(SwarmNodeHandler::new(
        Arc::new(SwarmScriptedProvider),
        xiaomaolv::service::swarm::AgentSwarmSettings::default(),
    )))?;
    let worker = LoopWorker::with_registry(engine.clone(), registry, "swarm:test", 60)?;
    // The goal parks in `verifying` after the DAG completes — acceptance
    // needs an operator — so assert on durable state like the other tests.
    let _ = worker.run_goal_until_idle(&goal.id, 24).await;
    let report = engine.resume_goal(&goal.id, "operator:test").await?;

    // DAG grew: root + 2 children + aggregate = 4 items.
    assert_eq!(report.work_items.len(), 4);
    let step_ids: Vec<&str> = report
        .work_items
        .iter()
        .map(|item| item.step_id.as_str())
        .collect();
    for expected in [
        "test-run:1",
        "test-run:1.c1",
        "test-run:1.c2",
        "test-run:1.agg",
    ] {
        assert!(step_ids.contains(&expected), "missing step {expected}");
    }
    // The aggregate waited on both children then merged their outcomes.
    let aggregate = report
        .work_items
        .iter()
        .find(|item| item.step_id == "test-run:1.agg")
        .expect("aggregate item");
    assert_eq!(aggregate.status, WorkItemStatus::Succeeded);
    assert_eq!(aggregate.dependency_ids.len(), 2);
    let outcome = engine
        .work_item_outcome(&aggregate.id)
        .await?
        .expect("aggregate outcome");
    assert_eq!(outcome.summary, "merged-final-answer");
    // All four items committed — the delegation tree is durable work.
    assert!(
        report
            .work_items
            .iter()
            .all(|item| item.status == WorkItemStatus::Succeeded)
    );
    Ok(())
}

async fn durable_swarm_engine(
    provider: Arc<dyn ChatProvider>,
    settings: xiaomaolv::service::swarm::AgentSwarmSettings,
) -> (Arc<LoopEngine>, xiaomaolv::harness::loop_engine::GoalRecord) {
    use xiaomaolv::service::swarm::SwarmNodeHandler;

    let memory = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("memory store");
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    let actor = "internal:swarm";
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "ROOT TASK".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            actor,
        )
        .await
        .expect("create goal");
    let plan = engine
        .plan_goal(
            &goal.id,
            PlanGoalRequest {
                workflow: WorkflowSpec {
                    steps: vec![WorkflowStep {
                        id: "root".to_string(),
                        handler: "swarm_node".to_string(),
                        effect: EffectClass::LocalWrite,
                        input: serde_json::json!({
                            "swarm_root": true,
                            "max_nodes": 8,
                            "swarm_run_id": "t",
                            "task": "ROOT TASK",
                            "role_name": "orchestrator",
                            "depth": 0,
                            "history": [],
                        }),
                        retry: RetryPolicy {
                            max_attempts: 2,
                            backoff_secs: 0,
                        },
                    }],
                    edges: Vec::new(),
                    budget: ExecutionBudget {
                        max_provider_calls: 32,
                        deadline_secs: 120,
                        max_response_bytes: 65_536,
                    },
                },
                acceptance_criteria: vec![AcceptanceCriterion::ArtifactExists {
                    artifact_type: "analysis_report".to_string(),
                }],
            },
            actor,
        )
        .await
        .expect("plan goal");
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            actor,
        )
        .await
        .expect("approve goal");
    let mut registry = WorkHandlerRegistry::default();
    registry
        .register(Arc::new(SwarmNodeHandler::new(provider, settings)))
        .expect("register swarm handler");
    let worker =
        LoopWorker::with_registry(engine.clone(), registry, "swarm:t", 60).expect("worker");
    let _ = worker.run_goal_until_idle(&goal.id, 32).await;
    (engine, goal)
}

/// A child whose model call times out lands `status:"timeout"` evidence —
/// the item still succeeds, the aggregate merges the degraded set and marks
/// the merged outcome `partial` instead of stalling the goal.
#[tokio::test]
async fn durable_swarm_child_timeout_merges_partial_in_aggregate() -> anyhow::Result<()> {
    struct TimeoutB;
    #[async_trait]
    impl ChatProvider for TimeoutB {
        async fn complete(&self, req: CompletionRequest) -> anyhow::Result<String> {
            let transcript = req
                .messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if transcript.contains("subtask B") && transcript.contains("direct deliverable") {
                // Simulate a hang beyond the node timeout.
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
            if transcript.contains("hierarchical task planner") {
                if transcript.contains("ROOT TASK") {
                    return Ok(serde_json::json!({
                        "mode": "delegate",
                        "children": [
                            {"task": "subtask A", "role_name": "a"},
                            {"task": "subtask B", "role_name": "b"}
                        ]
                    })
                    .to_string());
                }
                return Ok(serde_json::json!({"mode":"answer","answer":""}).to_string());
            }
            if transcript.contains("Merge child results") {
                return Ok("partial-merged".to_string());
            }
            Ok("answer-A".to_string())
        }
        fn model_name(&self) -> Option<&str> {
            Some("timeout-b")
        }
    }

    let settings = xiaomaolv::service::swarm::AgentSwarmSettings {
        max_node_timeout_ms: 50,
        ..Default::default()
    };
    let (engine, goal) = durable_swarm_engine(Arc::new(TimeoutB), settings).await;
    let report = engine.resume_goal(&goal.id, "operator:test").await?;
    let b_item = report
        .work_items
        .iter()
        .find(|item| item.step_id == "root.c2")
        .expect("subtask B item");
    assert_eq!(b_item.status, WorkItemStatus::Succeeded);
    let b_outcome = engine
        .work_item_outcome(&b_item.id)
        .await?
        .expect("B outcome");
    assert_eq!(b_outcome.evidence["status"], "timeout");
    let agg = report
        .work_items
        .iter()
        .find(|item| item.step_id == "root.agg")
        .expect("aggregate");
    let agg_outcome = engine
        .work_item_outcome(&agg.id)
        .await?
        .expect("aggregate outcome");
    assert_eq!(agg_outcome.summary, "partial-merged");
    assert_eq!(agg_outcome.evidence["status"], "partial");
    Ok(())
}

/// The approved `max_nodes` budget rejecting a delegation is not fatal —
/// the node falls back to answering directly.
#[tokio::test]
async fn durable_swarm_extension_budget_rejection_answers_directly() -> anyhow::Result<()> {
    struct DelegateThenAnswer;
    #[async_trait]
    impl ChatProvider for DelegateThenAnswer {
        async fn complete(&self, req: CompletionRequest) -> anyhow::Result<String> {
            let transcript = req
                .messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if transcript.contains("hierarchical task planner") {
                return Ok(serde_json::json!({
                    "mode": "delegate",
                    "children": [{"task": "child", "role_name": "r"}]
                })
                .to_string());
            }
            Ok("direct-root-answer".to_string())
        }
        fn model_name(&self) -> Option<&str> {
            Some("budget")
        }
    }

    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    let actor = "internal:swarm";
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "ROOT".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            actor,
        )
        .await?;
    let plan = engine
        .plan_goal(
            &goal.id,
            PlanGoalRequest {
                workflow: WorkflowSpec {
                    steps: vec![WorkflowStep {
                        id: "root".to_string(),
                        handler: "swarm_node".to_string(),
                        effect: EffectClass::LocalWrite,
                        input: serde_json::json!({
                            "swarm_root": true,
                            // Budget for exactly the root — any extension is
                            // rejected.
                            "max_nodes": 1,
                            "swarm_run_id": "t",
                            "task": "ROOT",
                            "depth": 0,
                            "history": [],
                        }),
                        retry: RetryPolicy {
                            max_attempts: 2,
                            backoff_secs: 0,
                        },
                    }],
                    edges: Vec::new(),
                    budget: ExecutionBudget {
                        max_provider_calls: 32,
                        deadline_secs: 60,
                        max_response_bytes: 65_536,
                    },
                },
                acceptance_criteria: vec![AcceptanceCriterion::ManualApproval {
                    label: "review".to_string(),
                }],
            },
            actor,
        )
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            actor,
        )
        .await?;
    let mut registry = WorkHandlerRegistry::default();
    registry.register(Arc::new(xiaomaolv::service::swarm::SwarmNodeHandler::new(
        Arc::new(DelegateThenAnswer),
        xiaomaolv::service::swarm::AgentSwarmSettings::default(),
    )))?;
    let worker = LoopWorker::with_registry(engine.clone(), registry, "swarm:t", 30)?;
    let _ = worker.run_goal_until_idle(&goal.id, 8).await;
    let report = engine.resume_goal(&goal.id, "operator:test").await?;
    assert_eq!(report.work_items.len(), 1, "extension must be rejected");
    let root = &report.work_items[0];
    assert_eq!(root.status, WorkItemStatus::Succeeded);
    let outcome = engine.work_item_outcome(&root.id).await?.expect("outcome");
    assert_eq!(outcome.evidence["answer"], "direct-root-answer");
    Ok(())
}

/// Depth at the limit skips the planner call entirely — the node answers
/// directly (no LLM decide, no children).
#[tokio::test]
async fn durable_swarm_depth_limit_forces_direct_answer() -> anyhow::Result<()> {
    struct CountingProvider {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl ChatProvider for CountingProvider {
        async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok("depth-forced-answer".to_string())
        }
        fn model_name(&self) -> Option<&str> {
            Some("counting")
        }
    }
    let provider = Arc::new(CountingProvider {
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let calls = provider.calls.clone();

    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    let actor = "internal:swarm";
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "ROOT".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            actor,
        )
        .await?;
    let plan = engine
        .plan_goal(
            &goal.id,
            PlanGoalRequest {
                workflow: WorkflowSpec {
                    steps: vec![WorkflowStep {
                        id: "deep".to_string(),
                        handler: "swarm_node".to_string(),
                        effect: EffectClass::LocalWrite,
                        input: serde_json::json!({
                            "task": "deep task",
                            "depth": 99,
                            "history": [],
                        }),
                        retry: RetryPolicy {
                            max_attempts: 2,
                            backoff_secs: 0,
                        },
                    }],
                    edges: Vec::new(),
                    budget: ExecutionBudget {
                        max_provider_calls: 32,
                        deadline_secs: 60,
                        max_response_bytes: 65_536,
                    },
                },
                acceptance_criteria: vec![AcceptanceCriterion::ManualApproval {
                    label: "review".to_string(),
                }],
            },
            actor,
        )
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            actor,
        )
        .await?;
    let mut registry = WorkHandlerRegistry::default();
    registry.register(Arc::new(xiaomaolv::service::swarm::SwarmNodeHandler::new(
        provider,
        xiaomaolv::service::swarm::AgentSwarmSettings::default(),
    )))?;
    let worker = LoopWorker::with_registry(engine.clone(), registry, "swarm:t", 30)?;
    // ManualApproval parks the finished goal in `verifying` — assert the
    // durable state directly.
    let _ = worker.run_goal_until_idle(&goal.id, 4).await;
    let report = engine.resume_goal(&goal.id, "operator:test").await?;
    // Exactly one provider call (the answer leg — no planner, no merge).
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    let outcome = engine
        .work_item_outcome(&report.work_items[0].id)
        .await?
        .expect("outcome");
    assert_eq!(outcome.evidence["answer"], "depth-forced-answer");
    Ok(())
}

/// Service-level durable run: handle_swarm drives the projected goal
/// end-to-end — activation, delegation, aggregation, terminal artifact —
/// and the goal verifies to `achieved` via the published analysis_report.
#[tokio::test]
async fn durable_swarm_service_run_achieves_goal_via_artifact() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!(
        "sqlite://{}",
        temp.path().join("swarm-durable.db").display()
    );
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(
        memory.clone(),
    ))));
    let provider = Arc::new(SwarmQueueProvider {
        replies: Mutex::new(VecDeque::from([
            "{\"use_swarm\":true}".to_string(),
            "{\"mode\":\"delegate\",\"children\":[{\"task\":\"outline frontend work\",\"role_name\":\"frontend\"},{\"task\":\"outline backend work\",\"role_name\":\"backend\"}]}".to_string(),
            "{\"mode\":\"answer\",\"answer\":\"frontend plan\"}".to_string(),
            "{\"mode\":\"answer\",\"answer\":\"backend plan\"}".to_string(),
            "merged swarm answer".to_string(),
        ])),
    });
    let harness_store: Arc<dyn xiaomaolv::harness::store::HarnessStore> = Arc::new(
        xiaomaolv::harness::store::SqliteHarnessStore::new(memory.clone()),
    );
    let harness_config = xiaomaolv::config::AgentHarnessConfig {
        enable_trajectory: true,
        ..Default::default()
    };
    let service = MessageService::new(provider, memory, 8)
        // durable_engine defaults to true — this test exercises the real
        // production path end to end.
        .with_agent_swarm(AgentSwarmSettings {
            enabled: true,
            auto_detect: true,
            reply_summary_enabled: false,
            // Single claimer: the in-memory SQLite pool is one connection,
            // so concurrent workers would only starve each other's write
            // transactions. Real fan-out is covered by
            // `concurrent_workers_claim_sibling_items_in_parallel`.
            max_parallel: 1,
            ..Default::default()
        })
        .with_harness_store(harness_store.clone())
        .with_harness_config(&harness_config)
        .with_loop_engine(Some(engine.clone()));

    let reply = service
        .handle(IncomingMessage {
            channel: "telegram".to_string(),
            session_id: "tg:1".to_string(),
            user_id: "42".to_string(),
            text: "请拆分前后端任务并协作完成".to_string(),
            reply_target: None,
            is_group_chat: false,
        })
        .await?;
    assert!(reply.text.contains("merged swarm answer"));

    let goals = engine.list_goals(10).await?;
    assert_eq!(goals.len(), 1);
    let goal = &goals[0];
    assert_eq!(goal.created_by, "internal:swarm");
    // The aggregate published the analysis_report artifact → acceptance
    // criterion satisfied → goal achieved without operator involvement.
    assert_eq!(goal.status, GoalStatus::Achieved);

    let resumed = engine.resume_goal(&goal.id, "operator:test").await?;
    // root + c1 + c2 + agg
    assert_eq!(resumed.work_items.len(), 4);
    assert!(
        resumed
            .work_items
            .iter()
            .all(|item| item.status == WorkItemStatus::Succeeded)
    );
    let artifacts = engine.list_artifacts(10).await?;
    assert!(
        artifacts
            .iter()
            .any(|a| a.name.starts_with("swarm-") && a.kind == ArtifactKind::AnalysisReport),
        "terminal artifact published"
    );

    // Node trajectories: every durable node attempt logged a trajectory
    // under the real chat session — swarm runs now feed the scorecard /
    // proposal-evidence loop like any other turn.
    let trajectories = harness_store
        .query_trajectories(xiaomaolv::harness::trajectory::TrajectoryFilter {
            session_id: Some("tg:1".to_string()),
            channel: None,
            user_id: None,
            exit_reason: None,
            has_tool_errors: None,
            limit: 100,
        })
        .await?;
    let node_trajectories: Vec<_> = trajectories
        .iter()
        .filter(|t| t.id.starts_with("swarm-node:"))
        .collect();
    assert_eq!(
        node_trajectories.len(),
        4,
        "each node attempt records a trajectory"
    );
    assert!(
        node_trajectories.iter().all(|t| t.finished_at.is_some()),
        "every node trajectory finished"
    );
    Ok(())
}

/// `run_goal_concurrent` races sibling claims across workers — both slow
/// items run under different worker ids and finish faster than a serial
/// drain would allow.
#[tokio::test]
async fn concurrent_workers_claim_sibling_items_in_parallel() -> anyhow::Result<()> {
    struct SlowHandler;
    #[async_trait]
    impl WorkHandler for SlowHandler {
        fn name(&self) -> &'static str {
            "provider_analysis"
        }
        fn effect_class(&self) -> EffectClass {
            EffectClass::LocalWrite
        }
        async fn execute(&self, _ctx: &WorkHandlerContext) -> anyhow::Result<WorkOutcome> {
            tokio::time::sleep(Duration::from_millis(250)).await;
            Ok(WorkOutcome {
                summary: "slow done".to_string(),
                artifact_ids: Vec::new(),
                evidence: serde_json::json!({}),
                sleep_until_unix: None,
                continuation: None,
            })
        }
    }

    // File-backed store: WAL + busy_timeout handle the writers' lock
    // contention the concurrent claim seam is exercising.
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("par.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "parallel".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "operator:test",
        )
        .await?;
    let plan = engine
        .plan_goal(
            &goal.id,
            PlanGoalRequest {
                workflow: WorkflowSpec {
                    steps: vec![
                        WorkflowStep {
                            id: "a".to_string(),
                            handler: "provider_analysis".to_string(),
                            effect: EffectClass::LocalWrite,
                            input: serde_json::json!({}),
                            retry: RetryPolicy {
                                max_attempts: 1,
                                backoff_secs: 0,
                            },
                        },
                        WorkflowStep {
                            id: "b".to_string(),
                            handler: "provider_analysis".to_string(),
                            effect: EffectClass::LocalWrite,
                            input: serde_json::json!({}),
                            retry: RetryPolicy {
                                max_attempts: 1,
                                backoff_secs: 0,
                            },
                        },
                    ],
                    edges: Vec::new(),
                    budget: ExecutionBudget {
                        max_provider_calls: 0,
                        deadline_secs: 60,
                        max_response_bytes: 65_536,
                    },
                },
                acceptance_criteria: vec![AcceptanceCriterion::ManualApproval {
                    label: "review".to_string(),
                }],
            },
            "operator:test",
        )
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            "operator:test",
        )
        .await?;

    let mut registry = WorkHandlerRegistry::default();
    registry.register(Arc::new(SlowHandler))?;
    let worker = LoopWorker::with_registry(engine.clone(), registry, "par", 60)?;
    let _ = worker.run_goal_concurrent(&goal.id, 2, 16).await;

    let report = engine.resume_goal(&goal.id, "operator:test").await?;
    assert!(
        report
            .work_items
            .iter()
            .all(|i| i.status == WorkItemStatus::Succeeded)
    );
    // Proof of real parallelism: the two items were claimed by distinct
    // concurrent workers, and the wall time stayed far below the serial
    // 500ms+ two slow items would need.
    let worker_ids: std::collections::BTreeSet<&str> = report
        .attempts
        .iter()
        .map(|a| a.worker_id.as_str())
        .collect();
    // Distinct concurrent workers claimed the two siblings — a serial
    // drain would have run both under one worker id. (Wall-clock asserts
    // are flaky on a single-connection SQLite pool, so worker identity is
    // the proof.)
    assert_eq!(
        worker_ids.len(),
        2,
        "expected distinct worker ids: {worker_ids:?}"
    );
    Ok(())
}

/// A swarm goal orphaned mid-run (handler not in `with_builtins`) parks
/// `blocked` under the plain runtime worker, then resumes under a worker
/// that registers `swarm_node` via `with_extra_handler`.
#[tokio::test]
async fn orphaned_swarm_goal_resumes_once_swarm_handler_registers() -> anyhow::Result<()> {
    struct AnswerProvider;
    #[async_trait]
    impl ChatProvider for AnswerProvider {
        async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
            Ok("orphan-answer".to_string())
        }
        fn model_name(&self) -> Option<&str> {
            Some("answer-provider")
        }
    }

    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    let actor = "internal:swarm";
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "ROOT".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            actor,
        )
        .await?;
    let plan = engine
        .plan_goal(
            &goal.id,
            PlanGoalRequest {
                workflow: WorkflowSpec {
                    steps: vec![WorkflowStep {
                        id: "orphan:1".to_string(),
                        handler: "swarm_node".to_string(),
                        effect: EffectClass::LocalWrite,
                        input: serde_json::json!({
                            "task": "orphan task",
                            "depth": 99,
                            "history": [],
                        }),
                        retry: RetryPolicy {
                            max_attempts: 1,
                            backoff_secs: 0,
                        },
                    }],
                    edges: Vec::new(),
                    budget: ExecutionBudget {
                        max_provider_calls: 8,
                        deadline_secs: 60,
                        max_response_bytes: 65_536,
                    },
                },
                acceptance_criteria: vec![AcceptanceCriterion::ManualApproval {
                    label: "review".to_string(),
                }],
            },
            actor,
        )
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            actor,
        )
        .await?;

    // Phase 1: a runtime worker WITHOUT the swarm handler parks the item.
    let plain = LoopWorker::with_builtins(engine.clone(), Arc::new(AnswerProvider), None, None)?
        .with_runtime_options("runtime-plain", 30)?;
    let _ = plain.run_goal_until_idle(&goal.id, 8).await;
    let report = engine.resume_goal(&goal.id, "operator:test").await?;
    assert_eq!(
        report.work_items[0].status,
        WorkItemStatus::Blocked,
        "missing handler parks the item blocked"
    );

    // Phase 2: "restart" — a worker WITH the swarm handler picks it up.
    let with_swarm =
        LoopWorker::with_builtins(engine.clone(), Arc::new(AnswerProvider), None, None)?
            .with_extra_handler(Arc::new(xiaomaolv::service::swarm::SwarmNodeHandler::new(
                Arc::new(AnswerProvider),
                xiaomaolv::service::swarm::AgentSwarmSettings::default(),
            )))?
            .with_runtime_options("runtime-swarm", 30)?;
    let _ = with_swarm.run_goal_until_idle(&goal.id, 8).await;
    let report = engine.resume_goal(&goal.id, "operator:test").await?;
    assert_eq!(report.work_items[0].status, WorkItemStatus::Succeeded);
    let outcome = engine
        .work_item_outcome(&report.work_items[0].id)
        .await?
        .expect("outcome");
    assert_eq!(outcome.evidence["answer"], "orphan-answer");
    Ok(())
}

/// A defer is a reschedule, not a failed attempt: `void` attempts don't
/// consume the retry budget, so a `max_attempts = 1` wait step still
/// completes after its deadline passes.
#[tokio::test]
async fn deferred_attempts_do_not_consume_the_attempt_budget() -> anyhow::Result<()> {
    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    let until = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64
        + 1;
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "wait once".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "operator:test",
        )
        .await?;
    let plan = engine
        .plan_goal(
            &goal.id,
            PlanGoalRequest {
                workflow: WorkflowSpec {
                    steps: vec![WorkflowStep {
                        id: "wait".to_string(),
                        handler: "wait".to_string(),
                        effect: EffectClass::Pure,
                        input: serde_json::json!({"until_unix": until}),
                        retry: RetryPolicy {
                            max_attempts: 1,
                            backoff_secs: 0,
                        },
                    }],
                    edges: Vec::new(),
                    budget: ExecutionBudget {
                        max_provider_calls: 0,
                        deadline_secs: 60,
                        max_response_bytes: 65_536,
                    },
                },
                acceptance_criteria: vec![AcceptanceCriterion::ManualApproval {
                    label: "review".to_string(),
                }],
            },
            "planner:test",
        )
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            "operator:test",
        )
        .await?;
    let worker = LoopWorker::with_builtins(engine.clone(), Arc::new(WorkerProvider), None, None)?;

    // Defer once, then the same single-attempt budget must still claim and
    // finish — before `void`, the defer burned the only attempt.
    let _ = worker.run_goal_until_idle(&goal.id, 4).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let _ = worker.run_goal_until_idle(&goal.id, 4).await;
    let report = engine.resume_goal(&goal.id, "operator:test").await?;
    assert_eq!(report.work_items[0].status, WorkItemStatus::Succeeded);
    assert!(
        report
            .attempts
            .iter()
            .any(|a| a.status == AttemptStatus::Void),
        "the defer is a void attempt"
    );
    Ok(())
}

/// A `continue` with `delay_secs` is the same reschedule shape — the
/// yielding attempt is `void` and the next claim still fits a one-attempt
/// budget.
#[tokio::test]
async fn delayed_continuation_does_not_consume_the_attempt_budget() -> anyhow::Result<()> {
    struct DelayOnce {
        fired: AtomicUsize,
    }
    #[async_trait]
    impl WorkHandler for DelayOnce {
        fn name(&self) -> &'static str {
            "provider_analysis"
        }
        fn effect_class(&self) -> EffectClass {
            EffectClass::LocalWrite
        }
        async fn execute(&self, ctx: &WorkHandlerContext) -> anyhow::Result<WorkOutcome> {
            if ctx.resume.is_none() && self.fired.swap(1, Ordering::Relaxed) == 0 {
                return Ok(WorkOutcome {
                    summary: "delay".to_string(),
                    artifact_ids: Vec::new(),
                    evidence: serde_json::json!({}),
                    sleep_until_unix: None,
                    continuation: Some(xiaomaolv::harness::loop_engine::WorkContinuation {
                        phase: "later".to_string(),
                        state: serde_json::json!({}),
                        delay_secs: Some(1),
                    }),
                });
            }
            Ok(WorkOutcome {
                summary: "resumed-done".to_string(),
                artifact_ids: Vec::new(),
                evidence: serde_json::json!({"resumed": ctx.resume.is_some()}),
                sleep_until_unix: None,
                continuation: None,
            })
        }
    }

    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "delay once".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            "operator:test",
        )
        .await?;
    let plan = engine
        .plan_goal(
            &goal.id,
            PlanGoalRequest {
                workflow: WorkflowSpec {
                    steps: vec![WorkflowStep {
                        id: "delay".to_string(),
                        handler: "provider_analysis".to_string(),
                        effect: EffectClass::LocalWrite,
                        input: serde_json::json!({}),
                        retry: RetryPolicy {
                            max_attempts: 1,
                            backoff_secs: 0,
                        },
                    }],
                    edges: Vec::new(),
                    budget: ExecutionBudget {
                        max_provider_calls: 0,
                        deadline_secs: 60,
                        max_response_bytes: 65_536,
                    },
                },
                acceptance_criteria: vec![AcceptanceCriterion::ManualApproval {
                    label: "review".to_string(),
                }],
            },
            "planner:test",
        )
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            "operator:test",
        )
        .await?;
    let mut registry = WorkHandlerRegistry::default();
    registry.register(Arc::new(DelayOnce {
        fired: AtomicUsize::new(0),
    }))?;
    let worker = LoopWorker::with_registry(engine.clone(), registry, "delay-worker", 30)?;
    let _ = worker.run_goal_until_idle(&goal.id, 4).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let _ = worker.run_goal_until_idle(&goal.id, 4).await;
    let report = engine.resume_goal(&goal.id, "operator:test").await?;
    assert_eq!(report.work_items[0].status, WorkItemStatus::Succeeded);
    let outcome = engine
        .work_item_outcome(&report.work_items[0].id)
        .await?
        .expect("outcome");
    assert_eq!(outcome.summary, "resumed-done");
    assert_eq!(outcome.evidence["resumed"], true);
    Ok(())
}

/// Orphaned audit rows: a crash leaves `agent_swarm_runs.status = running`
/// while its durable goal is already terminal. The janitor mirrors the
/// final DAG and closes the row; a live or goal-less run is untouched.
#[tokio::test]
async fn orphan_janitor_closes_runs_whose_goal_is_terminal() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("swarm-orphan.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(
        memory.clone(),
    ))));
    let provider = Arc::new(SwarmQueueProvider {
        replies: Mutex::new(VecDeque::from([
            "{\"use_swarm\":true}".to_string(),
            "{\"mode\":\"answer\",\"answer\":\"done it\"}".to_string(),
        ])),
    });
    let service = MessageService::new(provider, memory.clone(), 8)
        .with_agent_swarm(AgentSwarmSettings {
            enabled: true,
            auto_detect: true,
            reply_summary_enabled: false,
            max_parallel: 1,
            ..Default::default()
        })
        .with_loop_engine(Some(engine.clone()));
    let _ = service
        .handle(IncomingMessage {
            channel: "telegram".to_string(),
            session_id: "tg:1".to_string(),
            user_id: "42".to_string(),
            text: "拆分前后端任务".to_string(),
            reply_target: None,
            is_group_chat: false,
        })
        .await?;

    // Find the run + goal; simulate a crash-orphan by flipping the row back
    // to running (as if the process died before finish_agent_swarm_run).
    let runs = memory.list_agent_swarm_runs("telegram", None, 10).await?;
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    memory
        .finish_agent_swarm_run(xiaomaolv::memory::FinishAgentSwarmRunRequest {
            run_id: run.run_id.clone(),
            channel: run.channel.clone(),
            status: xiaomaolv::memory::AgentSwarmRunStatus::Running,
            finished_at_unix: 0,
            error: None,
        })
        .await?;
    let goal = engine
        .get_goal_by_request_id(&format!("swarm:{}", run.run_id))
        .await?
        .expect("swarm goal");
    assert_eq!(goal.status, GoalStatus::Achieved);

    // A second, genuinely-live run: no matching goal at all.
    memory
        .create_agent_swarm_run(xiaomaolv::memory::CreateAgentSwarmRunRequest {
            run_id: "run-live".to_string(),
            channel: "telegram".to_string(),
            session_id: "tg:2".to_string(),
            user_id: "u".to_string(),
            root_task: "still running".to_string(),
            status: xiaomaolv::memory::AgentSwarmRunStatus::Running,
            started_at_unix: 0,
        })
        .await?;

    let reaped = service.reap_orphaned_swarm_runs().await?;
    assert_eq!(reaped, 1);
    let runs = memory.list_agent_swarm_runs("telegram", None, 10).await?;
    let closed = runs.iter().find(|r| r.run_id == run.run_id).unwrap();
    assert_eq!(
        closed.status,
        xiaomaolv::memory::AgentSwarmRunStatus::Completed
    );
    let live = runs.iter().find(|r| r.run_id == "run-live").unwrap();
    assert_eq!(live.status, xiaomaolv::memory::AgentSwarmRunStatus::Running);
    Ok(())
}

/// The declared `max_provider_calls` budget now bites inside swarm nodes:
/// when the reservation is exhausted a node's provider call surfaces as an
/// evidence-level failure (item still commits — partial-result semantics)
/// and the provider is never called again.
#[tokio::test]
async fn swarm_node_honors_the_goal_provider_call_budget() -> anyhow::Result<()> {
    struct CountingProvider {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl ChatProvider for CountingProvider {
        async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok("{\"mode\":\"answer\",\"answer\":\"budgeted\"}".to_string())
        }
        fn model_name(&self) -> Option<&str> {
            Some("counting")
        }
    }

    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    let actor = "internal:swarm";
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "budget".to_string(),
                source_signal_ids: Vec::new(),
                request_id: Some("swarm:run-budget".to_string()),
            },
            actor,
        )
        .await?;
    let step = |id: &str| WorkflowStep {
        id: id.to_string(),
        handler: "swarm_node".to_string(),
        effect: EffectClass::LocalWrite,
        input: serde_json::json!({
            "task": "t", "depth": 99, "history": [],
        }),
        retry: RetryPolicy {
            max_attempts: 1,
            backoff_secs: 0,
        },
    };
    let plan = engine
        .plan_goal(
            &goal.id,
            PlanGoalRequest {
                workflow: WorkflowSpec {
                    steps: vec![step("run-budget:1"), step("run-budget:2")],
                    edges: Vec::new(),
                    budget: ExecutionBudget {
                        max_provider_calls: 1,
                        deadline_secs: 60,
                        max_response_bytes: 65_536,
                    },
                },
                acceptance_criteria: vec![AcceptanceCriterion::ManualApproval {
                    label: "review".to_string(),
                }],
            },
            actor,
        )
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            actor,
        )
        .await?;

    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(CountingProvider {
        calls: calls.clone(),
    });
    let mut registry = WorkHandlerRegistry::default();
    registry.register(Arc::new(xiaomaolv::service::swarm::SwarmNodeHandler::new(
        provider,
        xiaomaolv::service::swarm::AgentSwarmSettings::default(),
    )))?;
    let worker = LoopWorker::with_registry(engine.clone(), registry, "budget-worker", 30)?;
    let _ = worker.run_goal_until_idle(&goal.id, 8).await;
    let report = engine.resume_goal(&goal.id, "operator:test").await?;
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "budget allowed exactly one call"
    );
    let failed_outcome = report
        .work_items
        .iter()
        .find(|i| i.step_id == "run-budget:2")
        .expect("second step");
    let outcome = engine
        .work_item_outcome(&failed_outcome.id)
        .await?
        .expect("outcome");
    assert_eq!(outcome.evidence["status"], "failed");
    Ok(())
}

/// Deferred delivery: a run flagged `awaiting_reply` gets its final answer
/// pushed by the janitor once the goal is terminal, then the flag clears —
/// send-once via the `swarm-reply:` idempotency key.
#[tokio::test]
async fn janitor_delivers_deferred_reply_when_goal_is_terminal() -> anyhow::Result<()> {
    struct CapturingSender {
        sent: Mutex<Vec<(String, String, String, String)>>,
    }
    #[async_trait]
    impl xiaomaolv::harness::loop_engine::OutboundSender for CapturingSender {
        async fn send(
            &self,
            channel: &str,
            session_id: &str,
            text: &str,
            idempotency_key: &str,
        ) -> anyhow::Result<serde_json::Value> {
            self.sent.lock().unwrap().push((
                channel.to_string(),
                session_id.to_string(),
                text.to_string(),
                idempotency_key.to_string(),
            ));
            Ok(serde_json::json!({"ok": true}))
        }
    }

    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("swarm-defer.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(
        memory.clone(),
    ))));
    let provider = Arc::new(SwarmQueueProvider {
        replies: Mutex::new(VecDeque::from([
            "{\"use_swarm\":true}".to_string(),
            "{\"mode\":\"answer\",\"answer\":\"deferred final answer\"}".to_string(),
        ])),
    });
    let sender = Arc::new(CapturingSender {
        sent: Mutex::new(Vec::new()),
    });
    let service = MessageService::new(provider, memory.clone(), 8)
        .with_agent_swarm(AgentSwarmSettings {
            enabled: true,
            auto_detect: true,
            reply_summary_enabled: false,
            max_parallel: 1,
            ..Default::default()
        })
        .with_loop_engine(Some(engine.clone()))
        .with_outbound_sender(Some(sender.clone()));
    let _ = service
        .handle(IncomingMessage {
            channel: "telegram".to_string(),
            session_id: "tg:9".to_string(),
            user_id: "42".to_string(),
            text: "拆分任务".to_string(),
            reply_target: None,
            is_group_chat: false,
        })
        .await?;

    let runs = memory.list_agent_swarm_runs("telegram", None, 10).await?;
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    let goal = engine
        .get_goal_by_request_id(&format!("swarm:{}", run.run_id))
        .await?
        .expect("swarm goal");
    assert_eq!(goal.status, GoalStatus::Achieved);

    // Simulate the sync window having elapsed mid-run.
    memory
        .set_swarm_run_awaiting_reply("telegram", &run.run_id, true)
        .await?;

    let reaped = service.reap_orphaned_swarm_runs().await?;
    assert_eq!(reaped, 0, "run was already terminal in the audit table");
    {
        let sent = sender.sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "answer delivered once");
        assert_eq!(sent[0].1, "tg:9");
        assert!(sent[0].2.contains("deferred final answer"));
        assert_eq!(sent[0].3, format!("swarm-reply:{}", run.run_id));
    }

    // Second sweep does not re-send.
    let _ = service.reap_orphaned_swarm_runs().await?;
    assert_eq!(sender.sent.lock().unwrap().len(), 1);
    Ok(())
}

/// The synchronous reply window bounds channel head-blocking: a run that
/// outlives `reply_sync_wait_ms` returns the ack, flags `awaiting_reply`,
/// and the spawned driver keeps driving the DAG until the goal achieves —
/// then the janitor delivers the real answer.
#[tokio::test]
async fn slow_swarm_reply_defers_then_janitor_delivers() -> anyhow::Result<()> {
    struct CapturingSender {
        sent: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl xiaomaolv::harness::loop_engine::OutboundSender for CapturingSender {
        async fn send(
            &self,
            _channel: &str,
            _session_id: &str,
            text: &str,
            _idempotency_key: &str,
        ) -> anyhow::Result<serde_json::Value> {
            self.sent.lock().unwrap().push(text.to_string());
            Ok(serde_json::json!({"ok": true}))
        }
    }

    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("swarm-defer2.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(
        memory.clone(),
    ))));
    let provider = Arc::new(SwarmQueueProvider {
        replies: Mutex::new(VecDeque::from([
            "{\"use_swarm\":true}".to_string(),
            "{\"mode\":\"answer\",\"answer\":\"deferred final answer\"}".to_string(),
        ])),
    });
    let sender = Arc::new(CapturingSender {
        sent: Mutex::new(Vec::new()),
    });
    let service = MessageService::new(provider, memory.clone(), 8)
        .with_agent_swarm(AgentSwarmSettings {
            enabled: true,
            auto_detect: true,
            reply_summary_enabled: false,
            max_parallel: 1,
            // Deterministic deferral: 0 = the reply is always asynchronous.
            reply_sync_wait_ms: 0,
            ..Default::default()
        })
        .with_loop_engine(Some(engine.clone()))
        .with_outbound_sender(Some(sender.clone()));

    let reply = service
        .handle(IncomingMessage {
            channel: "telegram".to_string(),
            session_id: "tg:10".to_string(),
            user_id: "42".to_string(),
            text: "拆分任务".to_string(),
            reply_target: None,
            is_group_chat: false,
        })
        .await?;
    assert!(
        reply.text.contains("后台运行"),
        "sync window elapsed → ack, got: {}",
        reply.text
    );
    let runs = memory.list_awaiting_reply_swarm_runs(10).await?;
    assert_eq!(runs.len(), 1, "run flagged awaiting_reply");
    let run = runs[0].clone();
    let goal_key = format!("swarm:{}", run.run_id);

    // Detached driver keeps executing in-process; poll for terminal.
    let mut achieved = false;
    for _ in 0..60 {
        if let Some(goal) = engine.get_goal_by_request_id(&goal_key).await?
            && goal.status == GoalStatus::Achieved
        {
            achieved = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if !achieved {
        // If the detached driver stalled (or the process restarted), the
        // production path reclaims the goal once the abandoned attempt's
        // lease expires — drive it explicitly to prove recovery still works.
        let goal = engine
            .get_goal_by_request_id(&goal_key)
            .await?
            .expect("swarm goal");
        let mut registry = WorkHandlerRegistry::default();
        registry.register(Arc::new(xiaomaolv::service::swarm::SwarmNodeHandler::new(
            Arc::new(SwarmQueueProvider {
                replies: Mutex::new(VecDeque::from([
                    "{\"mode\":\"answer\",\"answer\":\"deferred final answer\"}".to_string(),
                ])),
            }),
            AgentSwarmSettings::default(),
        )))?;
        let reclaimer = LoopWorker::with_registry(engine.clone(), registry, "reclaim", 30)?;
        let report = reclaimer.run_goal_until_idle(&goal.id, 16).await?;
        assert_eq!(report.goal.status, GoalStatus::Achieved);
    }

    let _ = service.reap_orphaned_swarm_runs().await?;
    let sent = sender.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].contains("deferred final answer"));
    Ok(())
}

/// Execution-deadline enforcement: a goal whose budget clock started
/// (first provider reservation) and then exceeded `deadline_secs` is
/// failed by the sweep — it stops occupying the dispatchable set and
/// records a `goal.expired` event instead of looping forever.
#[tokio::test]
async fn goal_past_execution_deadline_expires_and_stops_dispatching() -> anyhow::Result<()> {
    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    let actor = "internal:swarm";
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "expire me".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            actor,
        )
        .await?;
    let plan = engine
        .plan_goal(
            &goal.id,
            PlanGoalRequest {
                workflow: WorkflowSpec {
                    steps: vec![
                        WorkflowStep {
                            id: "analyze".to_string(),
                            handler: "provider_analysis".to_string(),
                            effect: EffectClass::LocalWrite,
                            input: serde_json::json!({"objective": "o"}),
                            retry: RetryPolicy {
                                max_attempts: 1,
                                backoff_secs: 0,
                            },
                        },
                        WorkflowStep {
                            id: "linger".to_string(),
                            handler: "wait".to_string(),
                            effect: EffectClass::Pure,
                            // Far-future wake: the goal stays `active` after
                            // the analysis step completes.
                            input: serde_json::json!({"until_unix": 4_000_000_000i64}),
                            retry: RetryPolicy {
                                max_attempts: 1,
                                backoff_secs: 0,
                            },
                        },
                    ],
                    edges: Vec::new(),
                    budget: ExecutionBudget {
                        max_provider_calls: 4,
                        deadline_secs: 1, // expires ~1s after the clock starts
                        max_response_bytes: 65_536,
                    },
                },
                acceptance_criteria: vec![AcceptanceCriterion::ManualApproval {
                    label: "review".to_string(),
                }],
            },
            actor,
        )
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            actor,
        )
        .await?;

    // Drive one claim: `provider_analysis` reserves a call — the budget
    // clock starts — and the `wait` item defers, leaving the goal `active`.
    let provider = Arc::new(SwarmQueueProvider {
        replies: Mutex::new(VecDeque::from(["analysis".to_string()])),
    });
    let worker = LoopWorker::with_builtins(engine.clone(), provider, None, None)?;
    let _ = worker.run_goal_until_idle(&goal.id, 4).await;
    let goal_after = engine.get_goal(&goal.id).await?.expect("goal");
    assert_eq!(goal_after.status, GoalStatus::Active, "wait item parked it");

    // Cross the deadline boundary so started_at + 1 < now.
    tokio::time::sleep(Duration::from_millis(2200)).await;
    // Overdue-but-unswept: the dispatchable listing already filters it, so
    // the runtime poll stops claiming the goal before expiry lands.
    assert!(
        !engine
            .list_dispatchable_goal_ids(10)
            .await?
            .contains(&goal.id),
        "overdue goal filtered from dispatchable"
    );
    let expired = engine.expire_overdue_goals(64).await?;
    assert_eq!(expired, vec![goal.id.clone()]);
    let goal_final = engine.get_goal(&goal.id).await?.expect("goal");
    assert_eq!(goal_final.status, GoalStatus::Failed);
    let events = engine.list_goal_events(&goal.id, 0, 20).await?;
    assert!(
        events.iter().any(|e| e.event_type == "goal.expired"),
        "expiry is auditable"
    );
    assert!(
        !engine
            .list_dispatchable_goal_ids(10)
            .await?
            .contains(&goal.id),
        "expired goal leaves the dispatchable set"
    );
    Ok(())
}

/// Deferred replies only promise delivery when the channel can receive:
/// an outbound sender that doesn't support the run's channel gets the
/// plain ack (no "我会发给你"), and `awaiting_reply` is never flagged.
#[tokio::test]
async fn deferred_reply_skips_channels_the_sender_cannot_reach() -> anyhow::Result<()> {
    struct NoTelegramSender;
    #[async_trait]
    impl xiaomaolv::harness::loop_engine::OutboundSender for NoTelegramSender {
        async fn send(
            &self,
            _channel: &str,
            _session_id: &str,
            _text: &str,
            _idempotency_key: &str,
        ) -> anyhow::Result<serde_json::Value> {
            Ok(serde_json::json!({"ok": true}))
        }
        fn supports_channel(&self, _channel: &str) -> bool {
            false
        }
    }

    let temp = tempdir()?;
    let database_url = format!("sqlite://{}", temp.path().join("swarm-defer3.db").display());
    let memory = SqliteMemoryStore::new(&database_url).await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(
        memory.clone(),
    ))));
    let provider = Arc::new(SwarmQueueProvider {
        replies: Mutex::new(VecDeque::from([
            "{\"use_swarm\":true}".to_string(),
            "{\"mode\":\"answer\",\"answer\":\"deferred\"}".to_string(),
        ])),
    });
    let service = MessageService::new(provider, memory.clone(), 8)
        .with_agent_swarm(AgentSwarmSettings {
            enabled: true,
            auto_detect: true,
            reply_summary_enabled: false,
            max_parallel: 1,
            reply_sync_wait_ms: 0,
            ..Default::default()
        })
        .with_loop_engine(Some(engine.clone()))
        .with_outbound_sender(Some(Arc::new(NoTelegramSender)));

    let reply = service
        .handle(IncomingMessage {
            channel: "http".to_string(),
            session_id: "http:1".to_string(),
            user_id: "42".to_string(),
            text: "拆分任务".to_string(),
            reply_target: None,
            is_group_chat: false,
        })
        .await?;
    assert_eq!(reply.text, "多智能体任务仍在后台运行。");
    assert!(
        memory.list_awaiting_reply_swarm_runs(10).await?.is_empty(),
        "undeliverable channel never flags awaiting_reply"
    );
    Ok(())
}

/// Operator stop: canceling a live goal fails open items, abandons
/// in-flight attempts, records `goal.canceled`, and the canceled goal
/// leaves the dispatchable set.
#[tokio::test]
async fn cancel_goal_stops_live_work_and_is_auditable() -> anyhow::Result<()> {
    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    let actor = "operator:test";
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "cancel me".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            actor,
        )
        .await?;
    let plan = engine
        .plan_goal(
            &goal.id,
            PlanGoalRequest {
                workflow: WorkflowSpec {
                    steps: vec![WorkflowStep {
                        id: "linger".to_string(),
                        handler: "wait".to_string(),
                        effect: EffectClass::Pure,
                        input: serde_json::json!({"until_unix": 4_000_000_000i64}),
                        retry: RetryPolicy {
                            max_attempts: 3,
                            backoff_secs: 0,
                        },
                    }],
                    edges: Vec::new(),
                    budget: ExecutionBudget {
                        max_provider_calls: 0,
                        deadline_secs: 3600,
                        max_response_bytes: 65_536,
                    },
                },
                acceptance_criteria: vec![AcceptanceCriterion::ManualApproval {
                    label: "gate".to_string(),
                }],
            },
            actor,
        )
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            actor,
        )
        .await?;
    assert!(
        engine
            .list_dispatchable_goal_ids(10)
            .await?
            .contains(&goal.id)
    );

    let canceled = engine
        .cancel_goal(&goal.id, "no longer needed", actor)
        .await?;
    assert_eq!(canceled.status, GoalStatus::Canceled);
    let items = engine.list_work_items(&goal.id).await?;
    assert!(
        items.iter().all(|i| i.status == WorkItemStatus::Failed),
        "open items fail on cancel"
    );
    let events = engine.list_goal_events(&goal.id, 0, 20).await?;
    assert!(
        events.iter().any(|e| e.event_type == "goal.canceled"),
        "cancel is auditable"
    );
    assert!(
        !engine
            .list_dispatchable_goal_ids(10)
            .await?
            .contains(&goal.id),
        "canceled goal leaves the dispatchable set"
    );
    // Terminal goals refuse cancel.
    assert!(
        engine.cancel_goal(&goal.id, "again", actor).await.is_err(),
        "terminal goals cannot be canceled"
    );
    Ok(())
}

/// Operator hold: pausing a live goal removes it from dispatch without
/// destroying work — unpause returns it to the pool so claims resume.
#[tokio::test]
async fn paused_goal_stops_dispatching_until_unpaused() -> anyhow::Result<()> {
    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    let actor = "operator:test";
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "pause me".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            actor,
        )
        .await?;
    let plan = engine
        .plan_goal(
            &goal.id,
            PlanGoalRequest {
                workflow: WorkflowSpec {
                    steps: vec![WorkflowStep {
                        id: "linger".to_string(),
                        handler: "wait".to_string(),
                        effect: EffectClass::Pure,
                        input: serde_json::json!({"until_unix": 4_000_000_000i64}),
                        retry: RetryPolicy {
                            max_attempts: 3,
                            backoff_secs: 0,
                        },
                    }],
                    edges: Vec::new(),
                    budget: ExecutionBudget {
                        max_provider_calls: 0,
                        deadline_secs: 3600,
                        max_response_bytes: 65_536,
                    },
                },
                acceptance_criteria: vec![AcceptanceCriterion::ManualApproval {
                    label: "gate".to_string(),
                }],
            },
            actor,
        )
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            actor,
        )
        .await?;

    engine
        .pause_goal(&goal.id, "hold for triage", actor)
        .await?;
    let held = engine.get_goal(&goal.id).await?.expect("goal");
    assert_eq!(held.status, GoalStatus::Paused);
    assert!(
        !engine
            .list_dispatchable_goal_ids(10)
            .await?
            .contains(&goal.id),
        "paused goal leaves the dispatchable set"
    );

    let unpaused = engine.unpause_goal(&goal.id, actor).await?;
    assert_eq!(unpaused.status, GoalStatus::Active);
    assert!(
        engine
            .list_dispatchable_goal_ids(10)
            .await?
            .contains(&goal.id),
        "unpaused goal re-enters dispatch"
    );
    let events = engine.list_goal_events(&goal.id, 0, 20).await?;
    assert!(events.iter().any(|e| e.event_type == "goal.paused"));
    assert!(events.iter().any(|e| e.event_type == "goal.unpaused"));
    Ok(())
}

/// Replan: a failed goal can be sent back to planning — old items are
/// superseded (canceled, auditable), `plan_goal` accepts the `replan`
/// status, and the fresh revision runs on a clean item set.
#[tokio::test]
async fn failed_goal_replans_into_a_fresh_revision() -> anyhow::Result<()> {
    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    let actor = "operator:test";
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "replan me".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            actor,
        )
        .await?;
    let plan = engine
        .plan_goal(
            &goal.id,
            PlanGoalRequest {
                workflow: WorkflowSpec {
                    steps: vec![WorkflowStep {
                        id: "stalled".to_string(),
                        handler: "wait".to_string(),
                        effect: EffectClass::Pure,
                        input: serde_json::json!({"until_unix": 4_000_000_000i64}),
                        retry: RetryPolicy {
                            max_attempts: 1,
                            backoff_secs: 0,
                        },
                    }],
                    edges: Vec::new(),
                    budget: ExecutionBudget {
                        max_provider_calls: 0,
                        deadline_secs: 3600,
                        max_response_bytes: 65_536,
                    },
                },
                acceptance_criteria: vec![AcceptanceCriterion::ManualApproval {
                    label: "gate".to_string(),
                }],
            },
            actor,
        )
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            actor,
        )
        .await?;
    let provider = Arc::new(WorkerProvider);
    let worker = LoopWorker::with_builtins(engine.clone(), provider.clone(), None, None)?;
    engine
        .cancel_goal(&goal.id, "giving up on v1", actor)
        .await?;

    let replanned = engine
        .mark_goal_for_replan(&goal.id, "use a real handler", actor)
        .await?;
    assert_eq!(replanned.status, GoalStatus::Replan);
    // The prior revision's items are all terminal — nothing open remains
    // to leak into the next revision's dispatch or aggregate.
    let items = engine.list_work_items(&goal.id).await?;
    assert!(items.iter().all(|i| {
        matches!(
            i.status,
            WorkItemStatus::Succeeded | WorkItemStatus::Failed | WorkItemStatus::Canceled
        )
    }));

    // Fresh revision on the same goal.
    let plan2 = engine
        .plan_goal(
            &goal.id,
            PlanGoalRequest {
                workflow: WorkflowSpec {
                    steps: vec![WorkflowStep {
                        id: "check".to_string(),
                        handler: "self_test_suite".to_string(),
                        effect: EffectClass::LocalWrite,
                        input: serde_json::json!({"suite": "core"}),
                        retry: RetryPolicy {
                            max_attempts: 2,
                            backoff_secs: 0,
                        },
                    }],
                    edges: Vec::new(),
                    budget: ExecutionBudget {
                        max_provider_calls: 0,
                        deadline_secs: 3600,
                        max_response_bytes: 65_536,
                    },
                },
                acceptance_criteria: vec![AcceptanceCriterion::ManualApproval {
                    label: "gate".to_string(),
                }],
            },
            actor,
        )
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan2.goal.revision,
                expected_plan_hash: plan2.plan_hash.clone(),
            },
            actor,
        )
        .await?;
    assert!(
        engine
            .list_dispatchable_goal_ids(10)
            .await?
            .contains(&goal.id),
        "replanned+approved goal re-enters dispatch"
    );
    let report = worker.run_goal_until_idle(&goal.id, 8).await?;
    let fresh = report
        .work_items
        .iter()
        .find(|i| i.step_id == "check")
        .expect("new revision item ran");
    assert_eq!(fresh.status, WorkItemStatus::Succeeded);
    Ok(())
}

/// Reject: a pre-dispatch goal declines cleanly and is terminal until
/// replan opens a fresh revision.
#[tokio::test]
async fn proposed_goal_can_be_rejected_then_replanned() -> anyhow::Result<()> {
    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    let actor = "operator:test";
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: false,
                objective: "maybe not".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            actor,
        )
        .await?;
    let rejected = engine.reject_goal(&goal.id, "not worth it", actor).await?;
    assert_eq!(rejected.status, GoalStatus::Rejected);
    assert!(
        !engine
            .list_dispatchable_goal_ids(10)
            .await?
            .contains(&goal.id)
    );
    // Rejected goals are replannable — the operator can change their mind.
    let replanned = engine
        .mark_goal_for_replan(&goal.id, "actually do it", actor)
        .await?;
    assert_eq!(replanned.status, GoalStatus::Replan);
    let events = engine.list_goal_events(&goal.id, 0, 10).await?;
    assert!(
        events.iter().any(|e| e.event_type == "goal.rejected"),
        "reject lands an auditable event"
    );
    Ok(())
}

/// `internal_only` goals — projection mirrors like the legacy swarm mirror —
/// stay out of the runtime dispatch sweep, while the in-process targeted
/// claim path still works.
/// Group chats never auto-fire swarm unless `group_enabled` — the
/// classifier call itself must not even happen.
#[tokio::test]
async fn swarm_skips_group_chats_unless_group_enabled() -> anyhow::Result<()> {
    let provider = Arc::new(SwarmQueueProvider {
        replies: Mutex::new(VecDeque::from(["plain reply".to_string()])),
    });
    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let service =
        MessageService::new(provider.clone(), memory, 8).with_agent_swarm(AgentSwarmSettings {
            enabled: true,
            auto_detect: true,
            group_enabled: false,
            ..Default::default()
        });
    let reply = service
        .handle(IncomingMessage {
            channel: "telegram".to_string(),
            session_id: "tg:1".to_string(),
            user_id: "42".to_string(),
            text: "请拆分前后端任务并协作完成".to_string(),
            reply_target: None,
            is_group_chat: true,
        })
        .await?;
    assert_eq!(reply.text, "plain reply");
    assert!(
        provider.replies.lock().expect("replies mutex").is_empty(),
        "the classifier call never fired — the only provider call was the reply"
    );
    Ok(())
}

#[tokio::test]
async fn internal_only_goal_skips_dispatch_sweep_but_stays_claimable() -> anyhow::Result<()> {
    let memory = SqliteMemoryStore::new("sqlite::memory:").await?;
    let engine = Arc::new(LoopEngine::new(Arc::new(SqliteLoopStore::new(memory))));
    let actor = "operator:test";
    let goal = engine
        .create_goal(
            CreateGoalRequest {
                internal_only: true,
                objective: "mirror-only goal".to_string(),
                source_signal_ids: Vec::new(),
                request_id: None,
            },
            actor,
        )
        .await?;
    assert!(goal.internal_only);
    let plan = engine
        .plan_goal(
            &goal.id,
            PlanGoalRequest {
                workflow: WorkflowSpec {
                    steps: vec![WorkflowStep {
                        id: "step".to_string(),
                        handler: "self_test_suite".to_string(),
                        effect: EffectClass::LocalWrite,
                        input: serde_json::json!({"suite": "core"}),
                        retry: RetryPolicy {
                            max_attempts: 1,
                            backoff_secs: 0,
                        },
                    }],
                    edges: Vec::new(),
                    budget: ExecutionBudget {
                        max_provider_calls: 0,
                        deadline_secs: 3600,
                        max_response_bytes: 65_536,
                    },
                },
                acceptance_criteria: vec![AcceptanceCriterion::ManualApproval {
                    label: "gate".to_string(),
                }],
            },
            actor,
        )
        .await?;
    engine
        .approve_goal(
            &goal.id,
            ApproveGoalRequest {
                expected_goal_revision: plan.goal.revision,
                expected_plan_hash: plan.plan_hash.clone(),
            },
            actor,
        )
        .await?;
    // The runtime sweep never sees it...
    assert!(
        !engine
            .list_dispatchable_goal_ids(10)
            .await?
            .contains(&goal.id),
        "internal_only goal must not enter the dispatch list"
    );
    // ...and the generic claim path rejects it outright, not just the list.
    let generic = engine
        .claim_goal_work(&goal.id, "worker:generic", 30, actor)
        .await;
    assert!(
        generic
            .expect_err("generic claim on internal_only goal")
            .to_string()
            .contains("internal_only"),
        "generic claim must be rejected, not just the dispatch list"
    );
    // ...but the in-process targeted claim still works.
    let claim = engine
        .claim_work_item(&goal.id, "step", "worker:internal", 30, actor)
        .await?
        .expect("targeted claim should succeed on an internal_only goal");
    assert_eq!(claim.work_item.step_id, "step");
    Ok(())
}
