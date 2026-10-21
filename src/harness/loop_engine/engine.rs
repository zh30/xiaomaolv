use std::sync::Arc;

use anyhow::Context;

use super::domain::{
    AcceptanceCriterion, ApproveGoalRequest, CheckpointRecord, ConfirmationResolution,
    CreateGoalRequest, EffectClass, ExecutionBudget, ExternalWritePolicy, GoalRecord,
    GoalVerificationReport, LoopEventRecord, PlanGoalRequest, PlannedGoal,
    ProviderBudgetReservation, ResumeReport, RetryPolicy, WorkClaim, WorkItemRecord, WorkOutcome,
    WorkflowEdge, WorkflowSpec, WorkflowStep, hash_serializable, validate_actor,
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
    /// Best-effort wake-up for event consumers (SSE streams): carries the
    /// `goal_id` of a committed mutation so watchers re-read instead of
    /// polling. Losing a notification is harmless — consumers keep a slow
    /// fallback poll and the monotonic `sequence` cursor still drives them.
    event_tx: tokio::sync::broadcast::Sender<String>,
}

impl LoopEngine {
    pub fn new(store: Arc<dyn LoopStore>) -> Self {
        let (event_tx, _) = tokio::sync::broadcast::channel(1024);
        Self {
            store,
            external_write_policy: ExternalWritePolicy::default(),
            event_tx,
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

    /// Subscribes to goal-mutation notifications. Receivers get the affected
    /// `goal_id` for every committed change that may have produced events;
    /// lagged receivers should fall back to a bounded poll.
    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.event_tx.subscribe()
    }

    fn notify(&self, goal_id: &str) {
        // No receivers (or a full channel) is not an error.
        let _ = self.event_tx.send(goal_id.to_string());
    }

    pub async fn create_goal(
        &self,
        request: CreateGoalRequest,
        actor: &str,
    ) -> anyhow::Result<GoalRecord> {
        Ok(self.create_goal_admission(request, actor).await?.0)
    }

    /// `request.request_id` makes admission idempotent: the same key returns
    /// the already-admitted goal with `created = false`, so a retried submit
    /// cannot duplicate a goal.
    pub async fn create_goal_admission(
        &self,
        request: CreateGoalRequest,
        actor: &str,
    ) -> anyhow::Result<(GoalRecord, bool)> {
        request.validate()?;
        validate_actor(actor)?;
        let (goal, created) = self.store.create_goal_admission(request, actor).await?;
        if created {
            self.notify(&goal.id);
        }
        Ok((goal, created))
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
        // The request_id makes the create+mark pair idempotent: a crash or
        // retry between them finds the same goal instead of duplicating it.
        let goal = self
            .create_goal(
                CreateGoalRequest {
                    objective: objective.to_string(),
                    source_signal_ids: vec![signal_id.to_string()],
                    request_id: Some(format!("signal:{signal_id}")),
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
        let planned = self.store.plan_goal(goal_id, request, actor).await?;
        self.notify(goal_id);
        Ok(planned)
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
        let report = self
            .store
            .approve_goal(goal_id, request, actor, &self.external_write_policy)
            .await?;
        self.notify(goal_id);
        Ok(report)
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
        let claim = self
            .store
            .claim_goal_work(goal_id, worker_id, lease_secs, actor)
            .await?;
        if claim.is_some() {
            self.notify(goal_id);
        }
        Ok(claim)
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
        let claim = self
            .store
            .claim_work_item(goal_id, step_id, worker_id, lease_secs, actor)
            .await?;
        if claim.is_some() {
            self.notify(goal_id);
        }
        Ok(claim)
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
        let items = self
            .store
            .extend_workflow(goal_id, steps, actor, &self.external_write_policy)
            .await?;
        self.notify(goal_id);
        Ok(items)
    }

    pub async fn publish_artifact(
        &self,
        request: PublishArtifactRequest,
        actor: &str,
    ) -> anyhow::Result<ArtifactPublishResult> {
        request.validate()?;
        validate_actor(actor)?;
        let source_goal_id = request.source_goal_id.clone();
        let result = self.store.publish_artifact(request, actor).await?;
        if let Some(goal_id) = source_goal_id {
            self.notify(&goal_id);
        }
        Ok(result)
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
        let report = self.store.verify_goal(goal_id, actor).await?;
        self.notify(goal_id);
        Ok(report)
    }

    pub async fn record_manual_verification(
        &self,
        goal_id: &str,
        label: &str,
        actor: &str,
    ) -> anyhow::Result<GoalRecord> {
        validate_actor(actor)?;
        let goal = self
            .store
            .record_manual_verification(goal_id, label, actor)
            .await?;
        self.notify(goal_id);
        Ok(goal)
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

    /// `replay_safe` is the handler's declared crash semantics (see
    /// `WorkHandler::replay_safe`): `false` means a crash between the effect
    /// and this checkpoint's commit must park for operator confirmation
    /// rather than re-run blindly.
    pub async fn prepare_checkpoint(
        &self,
        claim: &WorkClaim,
        idempotency_key: &str,
        replay_safe: bool,
        actor: &str,
    ) -> anyhow::Result<CheckpointRecord> {
        validate_actor(actor)?;
        let checkpoint = self
            .store
            .prepare_checkpoint(claim, idempotency_key, replay_safe, actor)
            .await?;
        self.notify(&claim.attempt.goal_id);
        Ok(checkpoint)
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
        let checkpoint = self
            .store
            .commit_checkpoint(claim, checkpoint_id, outcome, actor)
            .await?;
        self.notify(&claim.attempt.goal_id);
        Ok(checkpoint)
    }

    pub async fn finish_attempt(
        &self,
        claim: &WorkClaim,
        checkpoint_id: &str,
        actor: &str,
    ) -> anyhow::Result<ResumeReport> {
        validate_actor(actor)?;
        let report = self
            .store
            .finish_attempt(claim, checkpoint_id, actor)
            .await?;
        self.notify(&claim.attempt.goal_id);
        Ok(report)
    }

    pub async fn fail_attempt(
        &self,
        claim: &WorkClaim,
        error: &str,
        retryable: bool,
        actor: &str,
    ) -> anyhow::Result<ResumeReport> {
        validate_actor(actor)?;
        let report = self
            .store
            .fail_attempt(claim, error, retryable, actor)
            .await?;
        self.notify(&claim.attempt.goal_id);
        Ok(report)
    }

    /// All attempts across the goal's work items.
    pub async fn list_attempts(
        &self,
        goal_id: &str,
    ) -> anyhow::Result<Vec<super::domain::AttemptRecord>> {
        self.store.list_attempts(goal_id).await
    }

    /// The goal's checkpoint trail for operator inspection.
    pub async fn list_checkpoints(
        &self,
        goal_id: &str,
    ) -> anyhow::Result<Vec<super::domain::CheckpointRecord>> {
        self.store.list_checkpoints(goal_id).await
    }

    /// Operator hold — new claims stop while the goal is paused;
    /// `unpause_goal` returns it to `active`. See `LoopStore::pause_goal`.
    pub async fn pause_goal(
        &self,
        goal_id: &str,
        reason: &str,
        actor: &str,
    ) -> anyhow::Result<GoalRecord> {
        self.store.pause_goal(goal_id, reason, actor).await
    }

    pub async fn unpause_goal(&self, goal_id: &str, actor: &str) -> anyhow::Result<GoalRecord> {
        self.store.unpause_goal(goal_id, actor).await
    }

    /// Send a dead-ended goal back to planning — `plan_goal` accepts the
    /// `replan` status and issues a fresh revision. `LoopStore::mark_goal_for_replan`.
    pub async fn mark_goal_for_replan(
        &self,
        goal_id: &str,
        reason: &str,
        actor: &str,
    ) -> anyhow::Result<GoalRecord> {
        self.store
            .mark_goal_for_replan(goal_id, reason, actor)
            .await
    }

    /// Operator stop for a live goal — see `LoopStore::cancel_goal`.
    pub async fn cancel_goal(
        &self,
        goal_id: &str,
        reason: &str,
        actor: &str,
    ) -> anyhow::Result<GoalRecord> {
        self.store.cancel_goal(goal_id, reason, actor).await
    }

    /// Fails goals whose execution deadline passed — called periodically by
    /// the runtime worker loop so an overdue goal stops occupying the
    /// dispatchable set and lands as a terminal, auditable `goal.expired`.
    pub async fn expire_overdue_goals(&self, limit: usize) -> anyhow::Result<Vec<String>> {
        self.store.expire_overdue_goals(limit).await
    }

    pub async fn resume_goal(&self, goal_id: &str, actor: &str) -> anyhow::Result<ResumeReport> {
        validate_actor(actor)?;
        let report = self
            .store
            .resume_goal(goal_id, actor)
            .await?
            .with_context(|| format!("goal not found: {goal_id}"))?;
        self.notify(goal_id);
        Ok(report)
    }

    /// Lists durable work items for a goal (operator inspection surface).
    pub async fn list_work_items(&self, goal_id: &str) -> anyhow::Result<Vec<WorkItemRecord>> {
        self.store.list_work_items(goal_id).await
    }

    /// Latest committed outcome per declared dependency of the work item,
    /// keyed by dependency step id — read-only, no event notification.
    pub async fn upstream_outcomes(
        &self,
        work_item_id: &str,
    ) -> anyhow::Result<std::collections::BTreeMap<String, WorkOutcome>> {
        self.store.upstream_outcomes(work_item_id).await
    }

    /// Goal lookup by the durable admission `request_id` — the idempotent
    /// admission key stamped at creation.
    pub async fn get_goal_by_request_id(
        &self,
        request_id: &str,
    ) -> anyhow::Result<Option<GoalRecord>> {
        self.store.get_goal_by_request_id(request_id).await
    }

    /// The work item's own latest committed outcome — read-only.
    pub async fn work_item_outcome(
        &self,
        work_item_id: &str,
    ) -> anyhow::Result<Option<WorkOutcome>> {
        self.store.work_item_outcome(work_item_id).await
    }

    /// Commits a phase boundary while the attempt keeps running. The
    /// persisted progress is injected into the next phase's `ctx.resume` and
    /// survives attempt death — a crash after this call resumes from the
    /// committed phase rather than restarting.
    pub async fn record_phase_progress(
        &self,
        claim: &WorkClaim,
        continuation: &crate::harness::loop_engine::WorkContinuation,
        actor: &str,
    ) -> anyhow::Result<crate::harness::loop_engine::WorkPhaseProgress> {
        validate_actor(actor)?;
        let progress = self
            .store
            .record_phase_progress(claim, continuation, actor)
            .await?;
        self.notify(&claim.attempt.goal_id);
        Ok(progress)
    }

    /// Persists phase progress and yields the attempt — the work item
    /// becomes claimable after `continuation.delay_secs` (immediately when
    /// unset) with the progress carried forward.
    pub async fn continue_attempt(
        &self,
        claim: &WorkClaim,
        continuation: &crate::harness::loop_engine::WorkContinuation,
        actor: &str,
    ) -> anyhow::Result<ResumeReport> {
        validate_actor(actor)?;
        let report = self
            .store
            .continue_attempt(claim, continuation, actor)
            .await?;
        self.notify(&claim.attempt.goal_id);
        Ok(report)
    }

    /// Operator verdict for a `waiting_confirmation` work item. The operator
    /// attests whether the external effect happened (`confirmed`), did not
    /// happen and may retry (`retry`), or should be abandoned (`abandoned`).
    /// `reason` is required so the decision is auditable in the event log.
    pub async fn resolve_waiting_confirmation(
        &self,
        goal_id: &str,
        work_item_id: &str,
        resolution: ConfirmationResolution,
        reason: &str,
        actor: &str,
    ) -> anyhow::Result<WorkItemRecord> {
        validate_actor(actor)?;
        let item = self
            .store
            .resolve_waiting_confirmation(goal_id, work_item_id, resolution, reason, actor)
            .await?;
        self.notify(goal_id);
        Ok(item)
    }

    /// Parks a claimed work item in `blocked`: dispatch could not proceed in
    /// this process (for example the named handler is not registered). The
    /// item stays blocked — retries do not consume its attempt budget beyond
    /// the attempt already abandoned here — until `requeue_blocked_items`
    /// sees the handler exist.
    pub async fn block_work_item(
        &self,
        claim: &WorkClaim,
        reason: &str,
        actor: &str,
    ) -> anyhow::Result<WorkItemRecord> {
        validate_actor(actor)?;
        let item = self.store.block_work_item(claim, reason, actor).await?;
        self.notify(&claim.attempt.goal_id);
        Ok(item)
    }

    /// Re-queues work items that were blocked because their handler was not
    /// registered, once `handlers` contains that handler again. Returns the
    /// number of items moved back to `ready`.
    pub async fn requeue_blocked_items(
        &self,
        goal_id: &str,
        reason_prefix: &str,
        handlers: &[String],
        actor: &str,
    ) -> anyhow::Result<u64> {
        validate_actor(actor)?;
        let requeued = self
            .store
            .requeue_blocked_items(goal_id, reason_prefix, handlers, actor)
            .await?;
        if requeued > 0 {
            self.notify(goal_id);
        }
        Ok(requeued)
    }

    /// Durable sleep: the claimed attempt ends without an outcome and the
    /// work item becomes claimable again at `until_unix`. The sleep is
    /// persisted, holds no lease, and survives process restarts.
    pub async fn defer_attempt(
        &self,
        claim: &WorkClaim,
        until_unix: i64,
        actor: &str,
    ) -> anyhow::Result<ResumeReport> {
        validate_actor(actor)?;
        let report = self.store.defer_attempt(claim, until_unix, actor).await?;
        self.notify(&claim.attempt.goal_id);
        Ok(report)
    }
}
