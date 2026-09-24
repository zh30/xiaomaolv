use std::sync::Arc;

use anyhow::Context;

use super::domain::{
    AcceptanceCriterion, ApproveGoalRequest, CheckpointRecord, CreateGoalRequest, EffectClass,
    ExecutionBudget, ExternalWritePolicy, GoalRecord, GoalVerificationReport, LoopEventRecord,
    PlanGoalRequest, PlannedGoal, ProviderBudgetReservation, ResumeReport, RetryPolicy, WorkClaim,
    WorkItemRecord, WorkOutcome, WorkflowEdge, WorkflowSpec, WorkflowStep, hash_serializable,
    validate_actor,
};
use super::store::LoopStore;
use super::{
    ArtifactKind, ArtifactPublishResult, ArtifactRecord, CreateSignalRequest,
    PublishArtifactRequest, ReplayRun, SelfTestRun, SelfTestStatus, SignalIngestResult, SignalKind,
    SignalRecord, SignalStatus, SignalTrust, TrajectoryFrame, TrajectoryFrameDraft,
};

#[derive(Clone)]
pub struct LoopEngine {
    store: Arc<dyn LoopStore>,
    external_write_policy: ExternalWritePolicy,
}

impl LoopEngine {
    pub fn new(store: Arc<dyn LoopStore>) -> Self {
        Self {
            store,
            external_write_policy: ExternalWritePolicy::default(),
        }
    }

    /// Feature gate + handler allowlist for `external_write` steps. Defaults
    /// to disabled — the strictest policy — so callers must opt in explicitly.
    pub fn with_external_write_policy(mut self, policy: ExternalWritePolicy) -> Self {
        self.external_write_policy = policy;
        self
    }

    pub fn external_write_policy(&self) -> &ExternalWritePolicy {
        &self.external_write_policy
    }

    pub async fn create_goal(
        &self,
        request: CreateGoalRequest,
        actor: &str,
    ) -> anyhow::Result<GoalRecord> {
        request.validate()?;
        validate_actor(actor)?;
        self.store.create_goal(request, actor).await
    }

    pub async fn get_goal(&self, goal_id: &str) -> anyhow::Result<Option<GoalRecord>> {
        self.store.get_goal(goal_id).await
    }

    pub async fn list_goals(&self, limit: usize) -> anyhow::Result<Vec<GoalRecord>> {
        self.store.list_goals(limit).await
    }

    pub async fn list_goal_events(
        &self,
        goal_id: &str,
        after_sequence: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<LoopEventRecord>> {
        self.store
            .list_goal_events(goal_id, after_sequence, limit)
            .await
    }

    pub async fn list_dispatchable_goal_ids(&self, limit: usize) -> anyhow::Result<Vec<String>> {
        self.store.list_dispatchable_goal_ids(limit).await
    }

    pub async fn ingest_signal(
        &self,
        request: CreateSignalRequest,
        actor: &str,
    ) -> anyhow::Result<SignalIngestResult> {
        request.validate()?;
        validate_actor(actor)?;
        self.store.ingest_signal(request, actor).await
    }

    pub async fn get_signal(&self, signal_id: &str) -> anyhow::Result<Option<SignalRecord>> {
        self.store.get_signal(signal_id).await
    }

    pub async fn list_signals(
        &self,
        limit: usize,
        status: Option<SignalStatus>,
    ) -> anyhow::Result<Vec<SignalRecord>> {
        self.store.list_signals(limit, status).await
    }

    pub async fn ignore_signal(
        &self,
        signal_id: &str,
        reason: &str,
        actor: &str,
    ) -> anyhow::Result<SignalRecord> {
        validate_actor(actor)?;
        self.store.ignore_signal(signal_id, reason, actor).await
    }

    pub async fn propose_goal_from_signal(
        &self,
        signal_id: &str,
        objective: &str,
        actor: &str,
    ) -> anyhow::Result<GoalRecord> {
        validate_actor(actor)?;
        let signal = self
            .store
            .get_signal(signal_id)
            .await?
            .context("signal not found")?;
        anyhow::ensure!(
            matches!(
                signal.status,
                SignalStatus::Observed | SignalStatus::Triaged
            ),
            "signal cannot be proposed from status '{}'",
            signal.status.as_str()
        );
        let goal = self
            .create_goal(
                CreateGoalRequest {
                    objective: objective.to_string(),
                    source_signal_ids: vec![signal_id.to_string()],
                },
                actor,
            )
            .await?;
        self.store
            .mark_signal_proposed(signal_id, &goal.id, actor)
            .await?;
        Ok(goal)
    }

    pub async fn run_self_tests(&self, suite: &str, actor: &str) -> anyhow::Result<SelfTestRun> {
        validate_actor(actor)?;
        let run = self.store.run_self_tests(suite, actor).await?;
        if run.status == SelfTestStatus::Failed {
            let failed_cases = run
                .cases
                .iter()
                .filter(|case| !case.passed)
                .map(|case| case.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let failure_digest = hash_serializable(&(suite, &failed_cases))?;
            if let Err(error) = self
                .ingest_signal(
                    CreateSignalRequest {
                        kind: SignalKind::SelfTest,
                        trust: SignalTrust::Internal,
                        source: "harness:self-test".to_string(),
                        external_id: Some(format!("{suite}:{failure_digest}")),
                        content: format!("Self-test suite {suite} failed: {failed_cases}"),
                        metadata: std::collections::BTreeMap::from([
                            ("run_id".to_string(), run.id.clone()),
                            ("suite".to_string(), suite.to_string()),
                        ]),
                    },
                    actor,
                )
                .await
            {
                tracing::warn!(%error, run_id = %run.id, "failed to emit self-test signal");
            }
        }
        Ok(run)
    }

    pub async fn get_self_test_run(&self, run_id: &str) -> anyhow::Result<Option<SelfTestRun>> {
        self.store.get_self_test_run(run_id).await
    }

    pub async fn record_trajectory_frame(
        &self,
        draft: TrajectoryFrameDraft,
        actor: &str,
    ) -> anyhow::Result<TrajectoryFrame> {
        draft.validate()?;
        validate_actor(actor)?;
        self.store.record_trajectory_frame(draft, actor).await
    }

    pub async fn list_trajectory_frames(
        &self,
        trajectory_id: &str,
    ) -> anyhow::Result<Vec<TrajectoryFrame>> {
        self.store.list_trajectory_frames(trajectory_id).await
    }

    pub async fn run_structural_replay(
        &self,
        trajectory_id: &str,
        actor: &str,
    ) -> anyhow::Result<ReplayRun> {
        validate_actor(actor)?;
        self.store.run_structural_replay(trajectory_id, actor).await
    }

    pub async fn plan_goal(
        &self,
        goal_id: &str,
        request: PlanGoalRequest,
        actor: &str,
    ) -> anyhow::Result<PlannedGoal> {
        request.validate_with_policy(&self.external_write_policy)?;
        validate_actor(actor)?;
        self.store.plan_goal(goal_id, request, actor).await
    }

    /// Latest planned workflow for a goal — approval surfaces use this to
    /// show the effect manifest again after `plan_goal` already returned.
    pub async fn get_goal_plan(&self, goal_id: &str) -> anyhow::Result<Option<PlannedGoal>> {
        self.store.latest_plan(goal_id).await
    }

    pub async fn plan_goal_recommended(
        &self,
        goal_id: &str,
        actor: &str,
    ) -> anyhow::Result<PlannedGoal> {
        let goal = self
            .get_goal(goal_id)
            .await?
            .with_context(|| format!("goal not found: {goal_id}"))?;
        self.plan_goal(
            goal_id,
            PlanGoalRequest {
                workflow: WorkflowSpec {
                    steps: vec![
                        WorkflowStep {
                            id: "analyze-goal".to_string(),
                            handler: "provider_analysis".to_string(),
                            effect: EffectClass::LocalWrite,
                            input: serde_json::json!({"objective": goal.objective}),
                            retry: RetryPolicy {
                                max_attempts: 2,
                                backoff_secs: 2,
                            },
                        },
                        WorkflowStep {
                            id: "run-core-self-tests".to_string(),
                            handler: "self_test_suite".to_string(),
                            effect: EffectClass::LocalWrite,
                            input: serde_json::json!({"suite": "core"}),
                            retry: RetryPolicy {
                                max_attempts: 2,
                                backoff_secs: 2,
                            },
                        },
                    ],
                    edges: vec![WorkflowEdge {
                        from: "analyze-goal".to_string(),
                        to: "run-core-self-tests".to_string(),
                    }],
                    budget: ExecutionBudget {
                        max_provider_calls: 2,
                        deadline_secs: 300,
                        max_response_bytes: 262_144,
                    },
                },
                acceptance_criteria: vec![
                    AcceptanceCriterion::ArtifactExists {
                        artifact_type: "analysis_report".to_string(),
                    },
                    AcceptanceCriterion::SelfTestSuite {
                        suite: "core".to_string(),
                    },
                ],
            },
            actor,
        )
        .await
    }

    pub async fn approve_goal(
        &self,
        goal_id: &str,
        request: ApproveGoalRequest,
        actor: &str,
    ) -> anyhow::Result<ResumeReport> {
        validate_actor(actor)?;
        self.store
            .approve_goal(goal_id, request, actor, &self.external_write_policy)
            .await
    }

    pub async fn claim_goal_work(
        &self,
        goal_id: &str,
        worker_id: &str,
        lease_secs: u32,
        actor: &str,
    ) -> anyhow::Result<Option<WorkClaim>> {
        validate_actor(worker_id)?;
        validate_actor(actor)?;
        anyhow::ensure!(
            (1..=3600).contains(&lease_secs),
            "lease must be 1..=3600 seconds"
        );
        self.store
            .claim_goal_work(goal_id, worker_id, lease_secs, actor)
            .await
    }

    /// Targeted claim for in-process subsystems (e.g. the agent swarm) that
    /// bind claims to a specific `step_id` rather than the lowest-ordinal
    /// ready item.
    pub async fn claim_work_item(
        &self,
        goal_id: &str,
        step_id: &str,
        worker_id: &str,
        lease_secs: u32,
        actor: &str,
    ) -> anyhow::Result<Option<WorkClaim>> {
        validate_actor(worker_id)?;
        validate_actor(actor)?;
        anyhow::ensure!(
            (1..=3600).contains(&lease_secs),
            "lease must be 1..=3600 seconds"
        );
        self.store
            .claim_work_item(goal_id, step_id, worker_id, lease_secs, actor)
            .await
    }

    /// Append steps to an `approved`/`active` goal. Restricted to `internal:`
    /// actors; the approved workflow must declare an expansion marker
    /// (`swarm_root` + `max_nodes`) and appended steps stay within its effect
    /// manifest.
    pub async fn extend_workflow(
        &self,
        goal_id: &str,
        steps: Vec<WorkflowStep>,
        actor: &str,
    ) -> anyhow::Result<Vec<WorkItemRecord>> {
        validate_actor(actor)?;
        self.store
            .extend_workflow(goal_id, steps, actor, &self.external_write_policy)
            .await
    }

    pub async fn publish_artifact(
        &self,
        request: PublishArtifactRequest,
        actor: &str,
    ) -> anyhow::Result<ArtifactPublishResult> {
        request.validate()?;
        validate_actor(actor)?;
        self.store.publish_artifact(request, actor).await
    }

    pub async fn get_artifact(&self, artifact_id: &str) -> anyhow::Result<Option<ArtifactRecord>> {
        self.store.get_artifact(artifact_id).await
    }

    pub async fn list_artifacts(&self, limit: usize) -> anyhow::Result<Vec<ArtifactRecord>> {
        self.store.list_artifacts(limit).await
    }

    pub async fn find_artifact(
        &self,
        kind: ArtifactKind,
        name: &str,
        version: &str,
    ) -> anyhow::Result<Option<ArtifactRecord>> {
        self.store.find_artifact(kind, name, version).await
    }

    pub async fn verify_goal(
        &self,
        goal_id: &str,
        actor: &str,
    ) -> anyhow::Result<GoalVerificationReport> {
        validate_actor(actor)?;
        self.store.verify_goal(goal_id, actor).await
    }

    pub async fn record_manual_verification(
        &self,
        goal_id: &str,
        label: &str,
        actor: &str,
    ) -> anyhow::Result<GoalRecord> {
        validate_actor(actor)?;
        self.store
            .record_manual_verification(goal_id, label, actor)
            .await
    }

    pub async fn reserve_provider_call(
        &self,
        goal_id: &str,
    ) -> anyhow::Result<ProviderBudgetReservation> {
        self.store.reserve_provider_call(goal_id).await
    }

    pub async fn record_provider_response_bytes(
        &self,
        goal_id: &str,
        response_bytes: u32,
    ) -> anyhow::Result<()> {
        self.store
            .record_provider_response_bytes(goal_id, response_bytes)
            .await
    }

    pub async fn prepare_checkpoint(
        &self,
        claim: &WorkClaim,
        idempotency_key: &str,
        actor: &str,
    ) -> anyhow::Result<CheckpointRecord> {
        validate_actor(actor)?;
        self.store
            .prepare_checkpoint(claim, idempotency_key, actor)
            .await
    }

    pub async fn renew_claim(&self, claim: &WorkClaim, lease_secs: u32) -> anyhow::Result<i64> {
        self.store.renew_claim(claim, lease_secs).await
    }

    pub async fn commit_checkpoint(
        &self,
        claim: &WorkClaim,
        checkpoint_id: &str,
        outcome: WorkOutcome,
        actor: &str,
    ) -> anyhow::Result<CheckpointRecord> {
        validate_actor(actor)?;
        outcome.validate()?;
        self.store
            .commit_checkpoint(claim, checkpoint_id, outcome, actor)
            .await
    }

    pub async fn finish_attempt(
        &self,
        claim: &WorkClaim,
        checkpoint_id: &str,
        actor: &str,
    ) -> anyhow::Result<ResumeReport> {
        validate_actor(actor)?;
        self.store.finish_attempt(claim, checkpoint_id, actor).await
    }

    pub async fn fail_attempt(
        &self,
        claim: &WorkClaim,
        error: &str,
        retryable: bool,
        actor: &str,
    ) -> anyhow::Result<ResumeReport> {
        validate_actor(actor)?;
        self.store
            .fail_attempt(claim, error, retryable, actor)
            .await
    }

    pub async fn resume_goal(&self, goal_id: &str, actor: &str) -> anyhow::Result<ResumeReport> {
        validate_actor(actor)?;
        self.store
            .resume_goal(goal_id, actor)
            .await?
            .with_context(|| format!("goal not found: {goal_id}"))
    }
}
