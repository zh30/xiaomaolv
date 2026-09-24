use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tempfile::tempdir;
use xiaomaolv::domain::{IncomingMessage, MessageRole, StoredMessage};
use xiaomaolv::harness::evolution::{
    EvolutionActor, EvolutionCaseAssertions, EvolutionEngine, EvolutionEvalCase,
    EvolutionGateConfig,
};
use xiaomaolv::harness::loop_engine::{
    AcceptanceCriterion, ApproveGoalRequest, ArtifactKind, CreateGoalRequest, CreateSignalRequest,
    EffectClass, ExecutionBudget, GoalStatus, LoopEngine, LoopWorker, PlanGoalRequest,
    PublishArtifactRequest, ReplayStatus, RetryPolicy, SelfTestStatus, SignalKind, SignalTrust,
    SqliteLoopStore, TrajectoryFrameCapture, TrajectoryFrameDraft, WorkItemStatus, WorkOutcome,
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
                objective: "Make loop recovery observable".to_string(),
                source_signal_ids: Vec::new(),
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

    let worker = LoopWorker::with_builtins(engine.clone(), Arc::new(WorkerProvider), None);
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
                objective: "Evaluate a prompt candidate safely".to_string(),
                source_signal_ids: Vec::new(),
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

    let worker = LoopWorker::with_builtins(loop_engine, Arc::new(WorkerProvider), Some(evolution));
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

    let signals = engine.list_signals(20).await?;
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
                objective: "Make recovery observable".to_string(),
                source_signal_ids: Vec::new(),
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
                objective: "Recover an interrupted evolution evaluation".to_string(),
                source_signal_ids: Vec::new(),
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
                objective: "Continuously self-test the harness".to_string(),
                source_signal_ids: Vec::new(),
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
                objective: "Require a human release decision".to_string(),
                source_signal_ids: Vec::new(),
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
        .prepare_checkpoint(&claim, "manual-gate:v1", "runtime:test")
        .await?;
    engine
        .commit_checkpoint(
            &claim,
            &checkpoint.id,
            WorkOutcome {
                summary: "manual gate reached".to_string(),
                artifact_ids: Vec::new(),
                evidence: serde_json::json!({"manual_gate": "reached"}),
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
                objective: "agent swarm run run-crash: root task".to_string(),
                source_signal_ids: Vec::new(),
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
        .prepare_checkpoint(&root_claim, "root:v1", "internal:swarm")
        .await?;
    engine
        .commit_checkpoint(
            &root_claim,
            &root_checkpoint.id,
            WorkOutcome {
                summary: "root finished before crash".to_string(),
                artifact_ids: Vec::new(),
                evidence: serde_json::json!({"status": "success"}),
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
                objective: "Bounded dynamic expansion".to_string(),
                source_signal_ids: Vec::new(),
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
