use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, ensure};
use async_trait::async_trait;
use sqlx::{Row, SqlitePool};

use crate::memory::SqliteMemoryStore;

use super::artifacts::{
    ArtifactKind, ArtifactPublishResult, ArtifactRecord, PublishArtifactRequest, find_artifact,
    get_artifact, initialize_artifact_schema, list_artifacts, publish_artifact,
};
use super::domain::{
    AcceptanceCriterion, ApproveGoalRequest, AttemptRecord, AttemptStatus, CheckpointPhase,
    CheckpointRecord, CreateGoalRequest, EffectClass, GoalRecord, GoalStatus,
    GoalVerificationReport, LoopEventRecord, PlanGoalRequest, PlannedGoal,
    ProviderBudgetReservation, ResumeReport, WorkClaim, WorkItemRecord, WorkItemStatus,
    WorkOutcome, WorkflowSpec, hash_serializable,
};
use super::replay::{
    ReplayRun, TrajectoryFrame, TrajectoryFrameDraft, initialize_replay_schema, list_frames,
    record_trajectory_frame, run_structural_replay,
};
use super::self_test::{
    SelfTestRun, get_self_test_run, initialize_self_test_schema, run_self_tests,
};
use super::signals::{
    CreateSignalRequest, SignalIngestResult, SignalRecord, get_signal, ingest_signal,
    initialize_signal_schema, list_signals, mark_signal_proposed,
};

#[async_trait]
pub trait LoopStore: Send + Sync {
    async fn create_goal(
        &self,
        request: CreateGoalRequest,
        actor: &str,
    ) -> anyhow::Result<GoalRecord>;

    async fn get_goal(&self, goal_id: &str) -> anyhow::Result<Option<GoalRecord>>;

    async fn list_goals(&self, limit: usize) -> anyhow::Result<Vec<GoalRecord>>;

    async fn list_goal_events(
        &self,
        goal_id: &str,
        after_sequence: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<LoopEventRecord>>;

    async fn list_dispatchable_goal_ids(&self, limit: usize) -> anyhow::Result<Vec<String>>;

    async fn ingest_signal(
        &self,
        request: CreateSignalRequest,
        actor: &str,
    ) -> anyhow::Result<SignalIngestResult>;

    async fn get_signal(&self, signal_id: &str) -> anyhow::Result<Option<SignalRecord>>;

    async fn list_signals(&self, limit: usize) -> anyhow::Result<Vec<SignalRecord>>;

    async fn mark_signal_proposed(
        &self,
        signal_id: &str,
        goal_id: &str,
        actor: &str,
    ) -> anyhow::Result<()>;

    async fn run_self_tests(&self, suite: &str, actor: &str) -> anyhow::Result<SelfTestRun>;

    async fn get_self_test_run(&self, run_id: &str) -> anyhow::Result<Option<SelfTestRun>>;

    async fn record_trajectory_frame(
        &self,
        draft: TrajectoryFrameDraft,
        actor: &str,
    ) -> anyhow::Result<TrajectoryFrame>;

    async fn list_trajectory_frames(
        &self,
        trajectory_id: &str,
    ) -> anyhow::Result<Vec<TrajectoryFrame>>;

    async fn run_structural_replay(
        &self,
        trajectory_id: &str,
        actor: &str,
    ) -> anyhow::Result<ReplayRun>;

    async fn publish_artifact(
        &self,
        request: PublishArtifactRequest,
        actor: &str,
    ) -> anyhow::Result<ArtifactPublishResult>;

    async fn get_artifact(&self, artifact_id: &str) -> anyhow::Result<Option<ArtifactRecord>>;

    async fn list_artifacts(&self, limit: usize) -> anyhow::Result<Vec<ArtifactRecord>>;

    async fn find_artifact(
        &self,
        kind: ArtifactKind,
        name: &str,
        version: &str,
    ) -> anyhow::Result<Option<ArtifactRecord>>;

    async fn verify_goal(
        &self,
        goal_id: &str,
        actor: &str,
    ) -> anyhow::Result<GoalVerificationReport>;

    async fn record_manual_verification(
        &self,
        goal_id: &str,
        label: &str,
        actor: &str,
    ) -> anyhow::Result<GoalRecord>;

    async fn reserve_provider_call(
        &self,
        goal_id: &str,
    ) -> anyhow::Result<ProviderBudgetReservation>;

    async fn record_provider_response_bytes(
        &self,
        goal_id: &str,
        response_bytes: u32,
    ) -> anyhow::Result<()>;

    async fn plan_goal(
        &self,
        goal_id: &str,
        request: PlanGoalRequest,
        actor: &str,
    ) -> anyhow::Result<PlannedGoal>;

    async fn approve_goal(
        &self,
        goal_id: &str,
        request: ApproveGoalRequest,
        actor: &str,
    ) -> anyhow::Result<ResumeReport>;

    async fn claim_goal_work(
        &self,
        goal_id: &str,
        worker_id: &str,
        lease_secs: u32,
        actor: &str,
    ) -> anyhow::Result<Option<WorkClaim>>;

    async fn prepare_checkpoint(
        &self,
        claim: &WorkClaim,
        idempotency_key: &str,
        actor: &str,
    ) -> anyhow::Result<CheckpointRecord>;

    async fn renew_claim(&self, claim: &WorkClaim, lease_secs: u32) -> anyhow::Result<i64>;

    async fn commit_checkpoint(
        &self,
        claim: &WorkClaim,
        checkpoint_id: &str,
        outcome: WorkOutcome,
        actor: &str,
    ) -> anyhow::Result<CheckpointRecord>;

    async fn finish_attempt(
        &self,
        claim: &WorkClaim,
        checkpoint_id: &str,
        actor: &str,
    ) -> anyhow::Result<ResumeReport>;

    async fn fail_attempt(
        &self,
        claim: &WorkClaim,
        error: &str,
        retryable: bool,
        actor: &str,
    ) -> anyhow::Result<ResumeReport>;

    async fn resume_goal(&self, goal_id: &str, actor: &str)
    -> anyhow::Result<Option<ResumeReport>>;
}

#[derive(Clone)]
pub struct SqliteLoopStore {
    store: SqliteMemoryStore,
}

impl SqliteLoopStore {
    pub fn new(store: SqliteMemoryStore) -> Self {
        Self { store }
    }
}

#[async_trait]
impl LoopStore for SqliteLoopStore {
    async fn create_goal(
        &self,
        request: CreateGoalRequest,
        actor: &str,
    ) -> anyhow::Result<GoalRecord> {
        let id = new_loop_id("goal");
        let source_signal_ids = serde_json::to_string(&request.source_signal_ids)
            .context("failed to serialize goal source signals")?;
        let mut tx = self
            .store
            .pool()
            .begin()
            .await
            .context("failed to start goal transaction")?;
        sqlx::query(
            "INSERT INTO harness_goals
             (id, objective, status, revision, source_signal_ids_json, created_by)
             VALUES (?1, ?2, 'proposed', 1, ?3, ?4)",
        )
        .bind(&id)
        .bind(request.objective.trim())
        .bind(source_signal_ids)
        .bind(actor)
        .execute(&mut *tx)
        .await
        .context("failed to create goal")?;
        insert_event(&mut tx, &id, "goal.created", actor, "{}").await?;
        tx.commit().await.context("failed to commit goal")?;
        self.get_goal(&id)
            .await?
            .context("created goal disappeared")
    }

    async fn get_goal(&self, goal_id: &str) -> anyhow::Result<Option<GoalRecord>> {
        ensure_valid_id(goal_id, "goal id")?;
        let row = sqlx::query(
            "SELECT id, objective, status, revision, source_signal_ids_json, created_by,
                    created_at, updated_at
             FROM harness_goals WHERE id = ?1",
        )
        .bind(goal_id)
        .fetch_optional(self.store.pool())
        .await
        .context("failed to load goal")?;
        row.map(decode_goal).transpose()
    }

    async fn list_goals(&self, limit: usize) -> anyhow::Result<Vec<GoalRecord>> {
        let limit = limit.clamp(1, 500);
        sqlx::query(
            "SELECT id, objective, status, revision, source_signal_ids_json, created_by,
                    created_at, updated_at
             FROM harness_goals ORDER BY updated_at DESC, id DESC LIMIT ?1",
        )
        .bind(i64::try_from(limit).context("goal limit exceeds sqlite range")?)
        .fetch_all(self.store.pool())
        .await
        .context("failed to list goals")?
        .into_iter()
        .map(decode_goal)
        .collect()
    }

    async fn list_goal_events(
        &self,
        goal_id: &str,
        after_sequence: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<LoopEventRecord>> {
        ensure_valid_id(goal_id, "goal id")?;
        ensure!(after_sequence >= 0, "event cursor cannot be negative");
        let limit = limit.clamp(1, 500);
        sqlx::query(
            "SELECT sequence, goal_id, event_type, actor, details_json, created_at
             FROM harness_events
             WHERE goal_id = ?1 AND sequence > ?2
             ORDER BY sequence LIMIT ?3",
        )
        .bind(goal_id)
        .bind(after_sequence)
        .bind(i64::try_from(limit).context("event limit exceeds sqlite range")?)
        .fetch_all(self.store.pool())
        .await
        .context("failed to list goal events")?
        .into_iter()
        .map(decode_loop_event)
        .collect()
    }

    async fn list_dispatchable_goal_ids(&self, limit: usize) -> anyhow::Result<Vec<String>> {
        let limit = limit.clamp(1, 500);
        sqlx::query_scalar::<_, String>(
            "SELECT id FROM harness_goals
             WHERE status IN ('approved', 'active', 'verifying')
             ORDER BY updated_at, id LIMIT ?1",
        )
        .bind(i64::try_from(limit).context("goal limit exceeds sqlite range")?)
        .fetch_all(self.store.pool())
        .await
        .context("failed to list dispatchable goals")
    }

    async fn ingest_signal(
        &self,
        request: CreateSignalRequest,
        actor: &str,
    ) -> anyhow::Result<SignalIngestResult> {
        ingest_signal(self.store.pool(), request, actor).await
    }

    async fn get_signal(&self, signal_id: &str) -> anyhow::Result<Option<SignalRecord>> {
        get_signal(self.store.pool(), signal_id).await
    }

    async fn list_signals(&self, limit: usize) -> anyhow::Result<Vec<SignalRecord>> {
        list_signals(self.store.pool(), limit).await
    }

    async fn mark_signal_proposed(
        &self,
        signal_id: &str,
        goal_id: &str,
        actor: &str,
    ) -> anyhow::Result<()> {
        mark_signal_proposed(self.store.pool(), signal_id, goal_id, actor).await
    }

    async fn run_self_tests(&self, suite: &str, actor: &str) -> anyhow::Result<SelfTestRun> {
        run_self_tests(self.store.pool(), suite, actor).await
    }

    async fn get_self_test_run(&self, run_id: &str) -> anyhow::Result<Option<SelfTestRun>> {
        get_self_test_run(self.store.pool(), run_id).await
    }

    async fn record_trajectory_frame(
        &self,
        draft: TrajectoryFrameDraft,
        actor: &str,
    ) -> anyhow::Result<TrajectoryFrame> {
        record_trajectory_frame(self.store.pool(), draft, actor).await
    }

    async fn list_trajectory_frames(
        &self,
        trajectory_id: &str,
    ) -> anyhow::Result<Vec<TrajectoryFrame>> {
        list_frames(self.store.pool(), trajectory_id).await
    }

    async fn run_structural_replay(
        &self,
        trajectory_id: &str,
        actor: &str,
    ) -> anyhow::Result<ReplayRun> {
        run_structural_replay(self.store.pool(), trajectory_id, actor).await
    }

    async fn publish_artifact(
        &self,
        request: PublishArtifactRequest,
        actor: &str,
    ) -> anyhow::Result<ArtifactPublishResult> {
        publish_artifact(self.store.pool(), request, actor).await
    }

    async fn get_artifact(&self, artifact_id: &str) -> anyhow::Result<Option<ArtifactRecord>> {
        get_artifact(self.store.pool(), artifact_id).await
    }

    async fn list_artifacts(&self, limit: usize) -> anyhow::Result<Vec<ArtifactRecord>> {
        list_artifacts(self.store.pool(), limit).await
    }

    async fn find_artifact(
        &self,
        kind: ArtifactKind,
        name: &str,
        version: &str,
    ) -> anyhow::Result<Option<ArtifactRecord>> {
        find_artifact(self.store.pool(), kind, name, version).await
    }

    async fn verify_goal(
        &self,
        goal_id: &str,
        actor: &str,
    ) -> anyhow::Result<GoalVerificationReport> {
        ensure_valid_id(goal_id, "goal id")?;
        let mut tx = self.store.pool().begin().await?;
        let status: String = sqlx::query_scalar("SELECT status FROM harness_goals WHERE id = ?1")
            .bind(goal_id)
            .fetch_optional(&mut *tx)
            .await?
            .context("goal not found")?;
        if status == "achieved" {
            tx.commit().await?;
            let goal = self.get_goal(goal_id).await?.context("goal disappeared")?;
            return Ok(GoalVerificationReport {
                goal,
                achieved: true,
                unmet_criteria: Vec::new(),
            });
        }
        ensure!(status == "verifying", "goal is not ready for verification");
        let acceptance_json: String = sqlx::query_scalar(
            "SELECT acceptance_json FROM harness_workflows
             WHERE goal_id = ?1 ORDER BY goal_revision DESC LIMIT 1",
        )
        .bind(goal_id)
        .fetch_one(&mut *tx)
        .await?;
        let criteria: Vec<AcceptanceCriterion> = serde_json::from_str(&acceptance_json)?;
        let outcomes = sqlx::query_scalar::<_, String>(
            "SELECT outcome_json FROM harness_checkpoints
             WHERE goal_id = ?1 AND phase = 'reconciled' AND outcome_json IS NOT NULL
             ORDER BY sequence",
        )
        .bind(goal_id)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(|json| serde_json::from_str::<WorkOutcome>(&json))
        .collect::<Result<Vec<_>, _>>()?;
        let artifact_ids = outcomes
            .iter()
            .flat_map(|outcome| outcome.artifact_ids.iter())
            .collect::<Vec<_>>();
        let mut unmet = Vec::new();
        for criterion in criteria {
            match criterion {
                AcceptanceCriterion::ArtifactExists { artifact_type } => {
                    let mut matched = false;
                    for artifact_id in &artifact_ids {
                        let kind: Option<String> =
                            sqlx::query_scalar("SELECT kind FROM harness_artifacts WHERE id = ?1")
                                .bind(artifact_id)
                                .fetch_optional(&mut *tx)
                                .await?;
                        if kind.as_deref() == Some(artifact_type.as_str()) {
                            matched = true;
                            break;
                        }
                    }
                    if !matched {
                        unmet.push(format!("artifact_exists:{artifact_type}"));
                    }
                }
                AcceptanceCriterion::SelfTestSuite { suite } => {
                    let run_ids = outcomes.iter().filter_map(|outcome| {
                        if outcome
                            .evidence
                            .get("suite")
                            .and_then(|value| value.as_str())
                            == Some(suite.as_str())
                        {
                            outcome
                                .evidence
                                .get("self_test_run_id")
                                .and_then(|value| value.as_str())
                        } else {
                            None
                        }
                    });
                    let mut matched = false;
                    for run_id in run_ids {
                        let passed: i64 = sqlx::query_scalar(
                            "SELECT COUNT(*) FROM harness_self_test_runs
                             WHERE id = ?1 AND suite = ?2 AND status = 'passed'",
                        )
                        .bind(run_id)
                        .bind(&suite)
                        .fetch_one(&mut *tx)
                        .await?;
                        if passed == 1 {
                            matched = true;
                            break;
                        }
                    }
                    if !matched {
                        unmet.push(format!("self_test_suite:{suite}"));
                    }
                }
                AcceptanceCriterion::ManualApproval { label } => {
                    let approved: i64 = sqlx::query_scalar(
                        "SELECT COUNT(*) FROM harness_manual_verifications
                         WHERE goal_id = ?1 AND label = ?2",
                    )
                    .bind(goal_id)
                    .bind(&label)
                    .fetch_one(&mut *tx)
                    .await?;
                    if approved == 0 {
                        unmet.push(format!("manual_approval:{label}"));
                    }
                }
            }
        }
        let achieved = unmet.is_empty();
        if achieved {
            sqlx::query(
                "UPDATE harness_goals
                 SET status = 'achieved', revision = revision + 1, updated_at = unixepoch()
                 WHERE id = ?1 AND status = 'verifying'",
            )
            .bind(goal_id)
            .execute(&mut *tx)
            .await?;
        }
        insert_event(
            &mut tx,
            goal_id,
            if achieved {
                "goal.achieved"
            } else {
                "goal.verification_pending"
            },
            actor,
            &serde_json::json!({"unmet_criteria": &unmet}).to_string(),
        )
        .await?;
        tx.commit().await?;
        let goal = self.get_goal(goal_id).await?.context("goal disappeared")?;
        Ok(GoalVerificationReport {
            goal,
            achieved,
            unmet_criteria: unmet,
        })
    }

    async fn record_manual_verification(
        &self,
        goal_id: &str,
        label: &str,
        actor: &str,
    ) -> anyhow::Result<GoalRecord> {
        ensure_valid_id(goal_id, "goal id")?;
        let label = label.trim();
        ensure!(
            !label.is_empty() && label.len() <= 160,
            "manual verification label must be 1..=160 bytes"
        );
        let mut tx = self.store.pool().begin().await?;
        let status: String = sqlx::query_scalar("SELECT status FROM harness_goals WHERE id = ?1")
            .bind(goal_id)
            .fetch_optional(&mut *tx)
            .await?
            .context("goal not found")?;
        ensure!(
            status == "verifying" || status == "achieved",
            "goal is not ready for manual verification"
        );
        let acceptance_json: String = sqlx::query_scalar(
            "SELECT acceptance_json FROM harness_workflows
             WHERE goal_id = ?1 ORDER BY goal_revision DESC LIMIT 1",
        )
        .bind(goal_id)
        .fetch_one(&mut *tx)
        .await?;
        let criteria: Vec<AcceptanceCriterion> = serde_json::from_str(&acceptance_json)?;
        ensure!(
            criteria.iter().any(|criterion| matches!(
                criterion,
                AcceptanceCriterion::ManualApproval { label: expected } if expected == label
            )),
            "manual verification label is not part of the approved acceptance criteria"
        );
        let inserted = sqlx::query(
            "INSERT OR IGNORE INTO harness_manual_verifications
             (goal_id, label, actor) VALUES (?1, ?2, ?3)",
        )
        .bind(goal_id)
        .bind(label)
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 1 {
            insert_event(
                &mut tx,
                goal_id,
                "goal.manual_verified",
                actor,
                &serde_json::json!({"label": label}).to_string(),
            )
            .await?;
        }
        tx.commit().await?;
        self.get_goal(goal_id)
            .await?
            .context("verified goal disappeared")
    }

    async fn reserve_provider_call(
        &self,
        goal_id: &str,
    ) -> anyhow::Result<ProviderBudgetReservation> {
        ensure_valid_id(goal_id, "goal id")?;
        let now = unix_now();
        let mut tx = self.store.pool().begin().await?;
        let row = sqlx::query(
            "SELECT max_provider_calls, deadline_secs, max_response_bytes,
                    provider_calls, response_bytes, started_at
             FROM harness_goal_budget_usage WHERE goal_id = ?1",
        )
        .bind(goal_id)
        .fetch_optional(&mut *tx)
        .await?
        .context("goal execution budget not found")?;
        let max_calls: i64 = row.try_get("max_provider_calls")?;
        let calls: i64 = row.try_get("provider_calls")?;
        let deadline_secs: i64 = row.try_get("deadline_secs")?;
        let max_response_bytes: i64 = row.try_get("max_response_bytes")?;
        let response_bytes: i64 = row.try_get("response_bytes")?;
        let started_at = row.try_get::<Option<i64>, _>("started_at")?.unwrap_or(now);
        let deadline_at = started_at + deadline_secs;
        ensure!(now <= deadline_at, "goal execution deadline is exhausted");
        ensure!(calls < max_calls, "goal provider call budget is exhausted");
        sqlx::query(
            "UPDATE harness_goal_budget_usage
             SET provider_calls = provider_calls + 1,
                 started_at = COALESCE(started_at, ?1), updated_at = unixepoch()
             WHERE goal_id = ?2 AND provider_calls < max_provider_calls",
        )
        .bind(now)
        .bind(goal_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ProviderBudgetReservation {
            deadline_at_unix: deadline_at,
            remaining_provider_calls: u32::try_from(max_calls - calls - 1)
                .context("invalid remaining provider calls")?,
            remaining_response_bytes: u32::try_from(max_response_bytes - response_bytes)
                .context("invalid remaining response bytes")?,
        })
    }

    async fn record_provider_response_bytes(
        &self,
        goal_id: &str,
        response_bytes: u32,
    ) -> anyhow::Result<()> {
        ensure_valid_id(goal_id, "goal id")?;
        let result = sqlx::query(
            "UPDATE harness_goal_budget_usage
             SET response_bytes = response_bytes + ?1, updated_at = unixepoch()
             WHERE goal_id = ?2 AND response_bytes + ?1 <= max_response_bytes",
        )
        .bind(i64::from(response_bytes))
        .bind(goal_id)
        .execute(self.store.pool())
        .await?;
        ensure!(
            result.rows_affected() == 1,
            "goal provider response byte budget is exhausted"
        );
        Ok(())
    }

    async fn plan_goal(
        &self,
        goal_id: &str,
        request: PlanGoalRequest,
        actor: &str,
    ) -> anyhow::Result<PlannedGoal> {
        ensure_valid_id(goal_id, "goal id")?;
        let plan_hash = request.plan_hash()?;
        let workflow_json = serde_json::to_string(&request.workflow)?;
        let acceptance_json = serde_json::to_string(&request.acceptance_criteria)?;
        let effect_manifest = request.effect_manifest();
        let effect_manifest_json = serde_json::to_string(&effect_manifest)?;
        let plan_id = new_loop_id("plan");
        let mut tx = self.store.pool().begin().await?;
        let current = sqlx::query("SELECT status, revision FROM harness_goals WHERE id = ?1")
            .bind(goal_id)
            .fetch_optional(&mut *tx)
            .await
            .context("failed to load goal for planning")?
            .context("goal not found")?;
        let status = GoalStatus::from_db(current.try_get("status")?)?;
        ensure!(
            matches!(status, GoalStatus::Proposed | GoalStatus::Replan),
            "goal cannot be planned from status {}",
            status.as_str()
        );
        let revision: i64 = current.try_get("revision")?;
        let next_revision = revision + 1;
        sqlx::query(
            "INSERT INTO harness_workflows
             (id, goal_id, goal_revision, plan_hash, workflow_json, acceptance_json,
              effect_manifest_json, created_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .bind(&plan_id)
        .bind(goal_id)
        .bind(next_revision)
        .bind(&plan_hash)
        .bind(&workflow_json)
        .bind(&acceptance_json)
        .bind(&effect_manifest_json)
        .bind(actor)
        .execute(&mut *tx)
        .await
        .context("failed to persist workflow")?;
        let updated = sqlx::query(
            "UPDATE harness_goals
             SET status = 'review_ready', revision = ?1, updated_at = unixepoch()
             WHERE id = ?2 AND revision = ?3 AND status = ?4",
        )
        .bind(next_revision)
        .bind(goal_id)
        .bind(revision)
        .bind(status.as_str())
        .execute(&mut *tx)
        .await
        .context("failed to move goal to review_ready")?;
        ensure!(
            updated.rows_affected() == 1,
            "goal changed while being planned"
        );
        insert_event(
            &mut tx,
            goal_id,
            "goal.planned",
            actor,
            &serde_json::json!({
                "plan_hash": &plan_hash,
                "goal_revision": next_revision,
            })
            .to_string(),
        )
        .await?;
        tx.commit().await?;
        let goal = self
            .get_goal(goal_id)
            .await?
            .context("planned goal disappeared")?;
        Ok(PlannedGoal {
            goal,
            plan_hash,
            workflow: request.workflow,
            acceptance_criteria: request.acceptance_criteria,
            effect_manifest,
        })
    }

    async fn approve_goal(
        &self,
        goal_id: &str,
        request: ApproveGoalRequest,
        actor: &str,
    ) -> anyhow::Result<ResumeReport> {
        ensure_valid_id(goal_id, "goal id")?;
        ensure!(
            request.expected_plan_hash.starts_with("sha256:"),
            "expected plan hash must be sha256"
        );
        let mut tx = self.store.pool().begin().await?;
        let goal_row = sqlx::query("SELECT status, revision FROM harness_goals WHERE id = ?1")
            .bind(goal_id)
            .fetch_optional(&mut *tx)
            .await?
            .context("goal not found")?;
        let status = GoalStatus::from_db(goal_row.try_get("status")?)?;
        let revision: i64 = goal_row.try_get("revision")?;
        ensure!(
            status == GoalStatus::ReviewReady,
            "goal is not review_ready"
        );
        ensure!(
            revision == i64::from(request.expected_goal_revision),
            "goal revision changed after review"
        );
        let plan_row = sqlx::query(
            "SELECT id, plan_hash, workflow_json, acceptance_json, effect_manifest_json
             FROM harness_workflows
             WHERE goal_id = ?1 AND goal_revision = ?2
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(goal_id)
        .bind(revision)
        .fetch_optional(&mut *tx)
        .await?
        .context("reviewed workflow not found")?;
        let workflow_id: String = plan_row.try_get("id")?;
        let stored_hash: String = plan_row.try_get("plan_hash")?;
        ensure!(
            stored_hash == request.expected_plan_hash,
            "reviewed plan hash does not match approval"
        );
        let workflow_json: String = plan_row.try_get("workflow_json")?;
        let acceptance_json: String = plan_row.try_get("acceptance_json")?;
        let effect_json: String = plan_row.try_get("effect_manifest_json")?;
        let workflow: WorkflowSpec = serde_json::from_str(&workflow_json)?;
        let acceptance: Vec<AcceptanceCriterion> = serde_json::from_str(&acceptance_json)?;
        let effects: Vec<EffectClass> = serde_json::from_str(&effect_json)?;
        let approval_id = new_loop_id("approval");
        sqlx::query(
            "INSERT INTO harness_goal_approvals
             (id, goal_id, goal_revision, plan_hash, workflow_hash, effect_manifest_hash,
              acceptance_hash, budget_hash, approved_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )
        .bind(&approval_id)
        .bind(goal_id)
        .bind(revision)
        .bind(&stored_hash)
        .bind(hash_serializable(&workflow)?)
        .bind(hash_serializable(&effects)?)
        .bind(hash_serializable(&acceptance)?)
        .bind(hash_serializable(&workflow.budget)?)
        .bind(actor)
        .execute(&mut *tx)
        .await
        .context("failed to bind goal approval")?;
        sqlx::query(
            "INSERT INTO harness_goal_budget_usage
             (goal_id, max_provider_calls, deadline_secs, max_response_bytes)
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(goal_id)
        .bind(i64::from(workflow.budget.max_provider_calls))
        .bind(i64::from(workflow.budget.deadline_secs))
        .bind(i64::from(workflow.budget.max_response_bytes))
        .execute(&mut *tx)
        .await
        .context("failed to initialize goal execution budget")?;

        let mut work_ids = BTreeMap::new();
        for (ordinal, step) in workflow.steps.iter().enumerate() {
            let work_id = new_loop_id("work");
            work_ids.insert(step.id.as_str(), work_id.clone());
            let incoming = workflow.edges.iter().any(|edge| edge.to == step.id);
            let work_status = if incoming { "pending" } else { "ready" };
            sqlx::query(
                "INSERT INTO harness_work_items
                 (id, goal_id, workflow_id, step_id, status, handler, effect_class,
                  input_json, max_attempts, backoff_secs, ordinal)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )
            .bind(&work_id)
            .bind(goal_id)
            .bind(&workflow_id)
            .bind(&step.id)
            .bind(work_status)
            .bind(&step.handler)
            .bind(step.effect.as_str())
            .bind(serde_json::to_string(&step.input)?)
            .bind(i64::from(step.retry.max_attempts))
            .bind(i64::from(step.retry.backoff_secs))
            .bind(i64::try_from(ordinal).context("too many workflow steps")?)
            .execute(&mut *tx)
            .await
            .context("failed to materialize work item")?;
        }
        for edge in &workflow.edges {
            sqlx::query(
                "INSERT INTO harness_work_item_dependencies
                 (work_item_id, depends_on_work_item_id) VALUES (?1, ?2)",
            )
            .bind(work_ids.get(edge.to.as_str()).expect("validated target"))
            .bind(work_ids.get(edge.from.as_str()).expect("validated source"))
            .execute(&mut *tx)
            .await
            .context("failed to materialize work dependency")?;
        }
        let updated = sqlx::query(
            "UPDATE harness_goals SET status = 'approved', revision = revision + 1,
                    updated_at = unixepoch()
             WHERE id = ?1 AND status = 'review_ready' AND revision = ?2",
        )
        .bind(goal_id)
        .bind(revision)
        .execute(&mut *tx)
        .await?;
        ensure!(
            updated.rows_affected() == 1,
            "goal changed while being approved"
        );
        insert_event(
            &mut tx,
            goal_id,
            "goal.approved",
            actor,
            &serde_json::json!({
                "plan_hash": &stored_hash,
                "approval_id": &approval_id,
            })
            .to_string(),
        )
        .await?;
        tx.commit().await?;
        self.resume_goal(goal_id, actor)
            .await?
            .context("approved goal disappeared")
    }

    async fn claim_goal_work(
        &self,
        goal_id: &str,
        worker_id: &str,
        lease_secs: u32,
        actor: &str,
    ) -> anyhow::Result<Option<WorkClaim>> {
        ensure_valid_id(goal_id, "goal id")?;
        recover_goal_state(self.store.pool(), goal_id, actor).await?;
        let now = unix_now();
        let lease_until = now + i64::from(lease_secs);
        let mut tx = self.store.pool().begin().await?;
        let goal_status: Option<String> =
            sqlx::query_scalar("SELECT status FROM harness_goals WHERE id = ?1")
                .bind(goal_id)
                .fetch_optional(&mut *tx)
                .await?;
        let goal_status = goal_status.context("goal not found")?;
        ensure!(
            matches!(goal_status.as_str(), "approved" | "active"),
            "goal is not dispatchable"
        );
        let row = sqlx::query(
            "SELECT id, goal_id, workflow_id, status, step_id, handler, effect_class,
                    input_json, max_attempts, ordinal, fencing_token
             FROM harness_work_items
             WHERE goal_id = ?1 AND status = 'ready'
               AND (next_attempt_at IS NULL OR next_attempt_at <= ?2)
               AND (SELECT COUNT(*) FROM harness_attempts a
                    WHERE a.work_item_id = harness_work_items.id) < max_attempts
             ORDER BY ordinal, id LIMIT 1",
        )
        .bind(goal_id)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        let workflow_id: String = row.try_get("workflow_id")?;
        let mut work_item = decode_work_item(row)?;
        let workflow_json: String =
            sqlx::query_scalar("SELECT workflow_json FROM harness_workflows WHERE id = ?1")
                .bind(&workflow_id)
                .fetch_one(&mut *tx)
                .await?;
        let budget = serde_json::from_str::<WorkflowSpec>(&workflow_json)
            .context("invalid claimed workflow")?
            .budget;
        let lease_token = new_loop_id("lease");
        let updated = sqlx::query(
            "UPDATE harness_work_items
             SET status = 'running', lease_token = ?1, lease_until = ?2,
                 fencing_token = fencing_token + 1, updated_at = unixepoch()
             WHERE id = ?3 AND status = 'ready'",
        )
        .bind(&lease_token)
        .bind(lease_until)
        .bind(&work_item.id)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            tx.rollback().await?;
            return Ok(None);
        }
        let fencing_token: i64 =
            sqlx::query_scalar("SELECT fencing_token FROM harness_work_items WHERE id = ?1")
                .bind(&work_item.id)
                .fetch_one(&mut *tx)
                .await?;
        let attempt_number: i64 =
            sqlx::query_scalar("SELECT COUNT(*) + 1 FROM harness_attempts WHERE work_item_id = ?1")
                .bind(&work_item.id)
                .fetch_one(&mut *tx)
                .await?;
        let attempt_id = new_loop_id("attempt");
        sqlx::query(
            "INSERT INTO harness_attempts
             (id, goal_id, work_item_id, attempt_number, status, worker_id,
              lease_token, fencing_token, started_at)
             VALUES (?1, ?2, ?3, ?4, 'running', ?5, ?6, ?7, ?8)",
        )
        .bind(&attempt_id)
        .bind(goal_id)
        .bind(&work_item.id)
        .bind(attempt_number)
        .bind(worker_id)
        .bind(&lease_token)
        .bind(fencing_token)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE harness_goals SET status = 'active', updated_at = unixepoch()
             WHERE id = ?1 AND status = 'approved'",
        )
        .bind(goal_id)
        .execute(&mut *tx)
        .await?;
        insert_event(
            &mut tx,
            goal_id,
            "work.claimed",
            actor,
            &serde_json::json!({
                "work_item_id": &work_item.id,
                "attempt_id": &attempt_id,
                "worker_id": worker_id,
                "fencing_token": fencing_token,
            })
            .to_string(),
        )
        .await?;
        tx.commit().await?;
        work_item.status = WorkItemStatus::Running;
        let work_item_id = work_item.id.clone();
        Ok(Some(WorkClaim {
            work_item,
            attempt: AttemptRecord {
                id: attempt_id,
                goal_id: goal_id.to_string(),
                work_item_id,
                attempt_number: u8::try_from(attempt_number).context("invalid attempt number")?,
                status: AttemptStatus::Running,
                worker_id: worker_id.to_string(),
                lease_token,
                fencing_token: u64::try_from(fencing_token).context("invalid fencing token")?,
                started_at_unix: now,
                finished_at_unix: None,
                error: None,
            },
            lease_until_unix: lease_until,
            budget,
        }))
    }

    async fn prepare_checkpoint(
        &self,
        claim: &WorkClaim,
        idempotency_key: &str,
        actor: &str,
    ) -> anyhow::Result<CheckpointRecord> {
        ensure!(
            !idempotency_key.trim().is_empty() && idempotency_key.len() <= 200,
            "idempotency key must be 1..=200 bytes"
        );
        let mut tx = self.store.pool().begin().await?;
        ensure_claim_current(&mut tx, claim).await?;
        if let Some(row) = sqlx::query(
            "SELECT id, goal_id, work_item_id, attempt_id, phase, idempotency_key,
                    outcome_json, created_at, updated_at
             FROM harness_checkpoints
             WHERE work_item_id = ?1 AND idempotency_key = ?2",
        )
        .bind(&claim.work_item.id)
        .bind(idempotency_key)
        .fetch_optional(&mut *tx)
        .await?
        {
            let checkpoint = decode_checkpoint(row)?;
            ensure!(
                checkpoint.attempt_id == claim.attempt.id,
                "idempotency key belongs to another attempt"
            );
            tx.commit().await?;
            return Ok(checkpoint);
        }
        let checkpoint_id = new_loop_id("checkpoint");
        sqlx::query(
            "INSERT INTO harness_checkpoints
             (id, goal_id, work_item_id, attempt_id, phase, idempotency_key)
             VALUES (?1, ?2, ?3, ?4, 'prepared', ?5)",
        )
        .bind(&checkpoint_id)
        .bind(&claim.attempt.goal_id)
        .bind(&claim.work_item.id)
        .bind(&claim.attempt.id)
        .bind(idempotency_key)
        .execute(&mut *tx)
        .await?;
        insert_event(
            &mut tx,
            &claim.attempt.goal_id,
            "checkpoint.prepared",
            actor,
            &serde_json::json!({
                "checkpoint_id": &checkpoint_id,
                "attempt_id": &claim.attempt.id,
            })
            .to_string(),
        )
        .await?;
        let row = load_checkpoint_tx(&mut tx, &checkpoint_id).await?;
        tx.commit().await?;
        Ok(row)
    }

    async fn renew_claim(&self, claim: &WorkClaim, lease_secs: u32) -> anyhow::Result<i64> {
        ensure!((1..=3600).contains(&lease_secs), "invalid lease duration");
        let now = unix_now();
        let lease_until = now + i64::from(lease_secs);
        let fencing_token = i64::try_from(claim.attempt.fencing_token)
            .context("fencing token exceeds sqlite range")?;
        let result = sqlx::query(
            "UPDATE harness_work_items
             SET lease_until = ?1, updated_at = unixepoch()
             WHERE id = ?2 AND status = 'running' AND lease_token = ?3
               AND fencing_token = ?4 AND lease_until >= ?5",
        )
        .bind(lease_until)
        .bind(&claim.work_item.id)
        .bind(&claim.attempt.lease_token)
        .bind(fencing_token)
        .bind(now)
        .execute(self.store.pool())
        .await?;
        ensure!(result.rows_affected() == 1, "cannot renew stale work claim");
        Ok(lease_until)
    }

    async fn commit_checkpoint(
        &self,
        claim: &WorkClaim,
        checkpoint_id: &str,
        outcome: WorkOutcome,
        actor: &str,
    ) -> anyhow::Result<CheckpointRecord> {
        ensure_valid_id(checkpoint_id, "checkpoint id")?;
        let outcome_json = serde_json::to_string(&outcome)?;
        let mut tx = self.store.pool().begin().await?;
        ensure_claim_current(&mut tx, claim).await?;
        let current = load_checkpoint_tx(&mut tx, checkpoint_id).await?;
        ensure!(
            current.attempt_id == claim.attempt.id,
            "checkpoint belongs to another attempt"
        );
        if current.phase == CheckpointPhase::Committed {
            ensure!(
                current.outcome.as_ref() == Some(&outcome),
                "checkpoint was committed with a different outcome"
            );
            tx.commit().await?;
            return Ok(current);
        }
        ensure!(
            current.phase == CheckpointPhase::Prepared,
            "checkpoint is not prepared"
        );
        let updated = sqlx::query(
            "UPDATE harness_checkpoints
             SET phase = 'committed', outcome_json = ?1, updated_at = unixepoch()
             WHERE id = ?2 AND phase = 'prepared' AND attempt_id = ?3",
        )
        .bind(outcome_json)
        .bind(checkpoint_id)
        .bind(&claim.attempt.id)
        .execute(&mut *tx)
        .await?;
        ensure!(
            updated.rows_affected() == 1,
            "checkpoint changed while committing"
        );
        sqlx::query(
            "UPDATE harness_attempts SET status = 'committing'
             WHERE id = ?1 AND status = 'running'",
        )
        .bind(&claim.attempt.id)
        .execute(&mut *tx)
        .await?;
        insert_event(
            &mut tx,
            &claim.attempt.goal_id,
            "checkpoint.committed",
            actor,
            &serde_json::json!({"checkpoint_id": checkpoint_id}).to_string(),
        )
        .await?;
        let checkpoint = load_checkpoint_tx(&mut tx, checkpoint_id).await?;
        tx.commit().await?;
        Ok(checkpoint)
    }

    async fn finish_attempt(
        &self,
        claim: &WorkClaim,
        checkpoint_id: &str,
        actor: &str,
    ) -> anyhow::Result<ResumeReport> {
        let mut tx = self.store.pool().begin().await?;
        ensure_claim_current(&mut tx, claim).await?;
        let checkpoint = load_checkpoint_tx(&mut tx, checkpoint_id).await?;
        ensure!(
            checkpoint.attempt_id == claim.attempt.id
                && checkpoint.phase == CheckpointPhase::Committed,
            "attempt has no committed checkpoint"
        );
        reconcile_checkpoint_tx(&mut tx, &checkpoint).await?;
        unlock_ready_work_tx(&mut tx, &claim.attempt.goal_id).await?;
        refresh_goal_status_tx(&mut tx, &claim.attempt.goal_id).await?;
        insert_event(
            &mut tx,
            &claim.attempt.goal_id,
            "attempt.finished",
            actor,
            &serde_json::json!({
                "attempt_id": &claim.attempt.id,
                "checkpoint_id": checkpoint_id,
            })
            .to_string(),
        )
        .await?;
        tx.commit().await?;
        self.resume_goal(&claim.attempt.goal_id, actor)
            .await?
            .context("goal disappeared after attempt completion")
    }

    async fn fail_attempt(
        &self,
        claim: &WorkClaim,
        error: &str,
        retryable: bool,
        actor: &str,
    ) -> anyhow::Result<ResumeReport> {
        let error = error.chars().take(2_000).collect::<String>();
        let mut tx = self.store.pool().begin().await?;
        ensure_claim_current(&mut tx, claim).await?;
        let row = sqlx::query(
            "SELECT max_attempts, backoff_secs,
                    (SELECT COUNT(*) FROM harness_attempts a
                     WHERE a.work_item_id = harness_work_items.id) AS attempt_count
             FROM harness_work_items WHERE id = ?1",
        )
        .bind(&claim.work_item.id)
        .fetch_one(&mut *tx)
        .await?;
        let max_attempts: i64 = row.try_get("max_attempts")?;
        let attempt_count: i64 = row.try_get("attempt_count")?;
        let backoff_secs: i64 = row.try_get("backoff_secs")?;
        let will_retry = retryable && attempt_count < max_attempts;
        sqlx::query(
            "UPDATE harness_attempts
             SET status = 'failed', finished_at = unixepoch(), error = ?1
             WHERE id = ?2 AND status IN ('running', 'committing')",
        )
        .bind(&error)
        .bind(&claim.attempt.id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE harness_work_items
             SET status = ?1, lease_token = NULL, lease_until = NULL,
                 next_attempt_at = ?2, last_error = ?3, updated_at = unixepoch()
             WHERE id = ?4 AND status = 'running'",
        )
        .bind(if will_retry { "ready" } else { "failed" })
        .bind(will_retry.then(|| unix_now() + backoff_secs))
        .bind(&error)
        .bind(&claim.work_item.id)
        .execute(&mut *tx)
        .await?;
        if !will_retry {
            sqlx::query(
                "WITH RECURSIVE downstream(id) AS (
                   SELECT work_item_id FROM harness_work_item_dependencies
                   WHERE depends_on_work_item_id = ?1
                   UNION
                   SELECT dependency.work_item_id
                   FROM harness_work_item_dependencies dependency
                   JOIN downstream ON dependency.depends_on_work_item_id = downstream.id
                 )
                 UPDATE harness_work_items SET status = 'blocked', updated_at = unixepoch()
                 WHERE id IN (SELECT id FROM downstream)
                   AND status IN ('pending', 'ready')",
            )
            .bind(&claim.work_item.id)
            .execute(&mut *tx)
            .await?;
        }
        refresh_goal_status_tx(&mut tx, &claim.attempt.goal_id).await?;
        insert_event(
            &mut tx,
            &claim.attempt.goal_id,
            "attempt.failed",
            actor,
            &serde_json::json!({
                "attempt_id": &claim.attempt.id,
                "retryable": retryable,
                "will_retry": will_retry,
                "error": &error,
            })
            .to_string(),
        )
        .await?;
        tx.commit().await?;
        self.resume_goal(&claim.attempt.goal_id, actor)
            .await?
            .context("goal disappeared after attempt failure")
    }

    async fn resume_goal(
        &self,
        goal_id: &str,
        actor: &str,
    ) -> anyhow::Result<Option<ResumeReport>> {
        recover_goal_state(self.store.pool(), goal_id, actor).await?;
        let Some(goal) = self.get_goal(goal_id).await? else {
            return Ok(None);
        };
        let rows = sqlx::query(
            "SELECT id, goal_id, status, step_id, handler, effect_class, input_json,
                    max_attempts, ordinal
             FROM harness_work_items WHERE goal_id = ?1 ORDER BY ordinal, id",
        )
        .bind(goal_id)
        .fetch_all(self.store.pool())
        .await
        .context("failed to load resumable work items")?;
        let mut work_items = Vec::with_capacity(rows.len());
        for row in rows {
            let mut item = decode_work_item(row)?;
            item.dependency_ids = sqlx::query_scalar::<_, String>(
                "SELECT depends_on_work_item_id FROM harness_work_item_dependencies
                 WHERE work_item_id = ?1 ORDER BY depends_on_work_item_id",
            )
            .bind(&item.id)
            .fetch_all(self.store.pool())
            .await
            .context("failed to load work item dependencies")?;
            work_items.push(item);
        }
        let attempts = sqlx::query(
            "SELECT id, goal_id, work_item_id, attempt_number, status, worker_id,
                    lease_token, fencing_token, started_at, finished_at, error
             FROM harness_attempts WHERE goal_id = ?1 ORDER BY started_at, id",
        )
        .bind(goal_id)
        .fetch_all(self.store.pool())
        .await
        .context("failed to load attempts")?
        .into_iter()
        .map(decode_attempt)
        .collect::<anyhow::Result<Vec<_>>>()?;
        let checkpoint = sqlx::query(
            "SELECT id, goal_id, work_item_id, attempt_id, phase, idempotency_key,
                    outcome_json, created_at, updated_at
             FROM harness_checkpoints WHERE goal_id = ?1
             ORDER BY sequence DESC LIMIT 1",
        )
        .bind(goal_id)
        .fetch_optional(self.store.pool())
        .await
        .context("failed to load latest checkpoint")?
        .map(decode_checkpoint)
        .transpose()?;
        sqlx::query(
            "INSERT INTO harness_events (goal_id, event_type, actor, details_json)
             VALUES (?1, 'goal.resumed', ?2, '{}')",
        )
        .bind(goal_id)
        .bind(actor)
        .execute(self.store.pool())
        .await
        .context("failed to record resume event")?;
        Ok(Some(ResumeReport {
            goal,
            work_items,
            attempts,
            latest_checkpoint: checkpoint,
        }))
    }
}

pub async fn initialize_loop_engine_schema(pool: &SqlitePool) -> anyhow::Result<()> {
    for statement in [
        "CREATE TABLE IF NOT EXISTS harness_schema_migrations (
            version INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            applied_at INTEGER NOT NULL DEFAULT (unixepoch())
        )",
        "CREATE TABLE IF NOT EXISTS harness_goals (
            id TEXT PRIMARY KEY,
            objective TEXT NOT NULL,
            status TEXT NOT NULL,
            revision INTEGER NOT NULL,
            source_signal_ids_json TEXT NOT NULL DEFAULT '[]',
            created_by TEXT NOT NULL,
            created_at INTEGER NOT NULL DEFAULT (unixepoch()),
            updated_at INTEGER NOT NULL DEFAULT (unixepoch())
        )",
        "CREATE INDEX IF NOT EXISTS idx_harness_goals_status_updated
         ON harness_goals(status, updated_at, id)",
        "CREATE TABLE IF NOT EXISTS harness_workflows (
            id TEXT PRIMARY KEY,
            goal_id TEXT NOT NULL REFERENCES harness_goals(id),
            goal_revision INTEGER NOT NULL,
            plan_hash TEXT NOT NULL,
            workflow_json TEXT NOT NULL,
            acceptance_json TEXT NOT NULL,
            effect_manifest_json TEXT NOT NULL,
            created_by TEXT NOT NULL,
            created_at INTEGER NOT NULL DEFAULT (unixepoch()),
            UNIQUE(goal_id, goal_revision)
        )",
        "CREATE TABLE IF NOT EXISTS harness_goal_approvals (
            id TEXT PRIMARY KEY,
            goal_id TEXT NOT NULL REFERENCES harness_goals(id),
            goal_revision INTEGER NOT NULL,
            plan_hash TEXT NOT NULL,
            workflow_hash TEXT NOT NULL,
            effect_manifest_hash TEXT NOT NULL,
            acceptance_hash TEXT NOT NULL,
            budget_hash TEXT NOT NULL,
            approved_by TEXT NOT NULL,
            created_at INTEGER NOT NULL DEFAULT (unixepoch()),
            UNIQUE(goal_id, goal_revision)
        )",
        "CREATE TABLE IF NOT EXISTS harness_goal_budget_usage (
            goal_id TEXT PRIMARY KEY REFERENCES harness_goals(id),
            max_provider_calls INTEGER NOT NULL,
            deadline_secs INTEGER NOT NULL,
            max_response_bytes INTEGER NOT NULL,
            provider_calls INTEGER NOT NULL DEFAULT 0,
            response_bytes INTEGER NOT NULL DEFAULT 0,
            started_at INTEGER,
            updated_at INTEGER NOT NULL DEFAULT (unixepoch())
        )",
        "CREATE TABLE IF NOT EXISTS harness_work_items (
            id TEXT PRIMARY KEY,
            goal_id TEXT NOT NULL REFERENCES harness_goals(id),
            workflow_id TEXT NOT NULL REFERENCES harness_workflows(id),
            step_id TEXT NOT NULL,
            status TEXT NOT NULL,
            handler TEXT NOT NULL,
            effect_class TEXT NOT NULL,
            input_json TEXT NOT NULL,
            max_attempts INTEGER NOT NULL,
            backoff_secs INTEGER NOT NULL,
            ordinal INTEGER NOT NULL,
            lease_token TEXT,
            lease_until INTEGER,
            fencing_token INTEGER NOT NULL DEFAULT 0,
            next_attempt_at INTEGER,
            last_error TEXT,
            created_at INTEGER NOT NULL DEFAULT (unixepoch()),
            updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
            UNIQUE(workflow_id, step_id)
        )",
        "CREATE INDEX IF NOT EXISTS idx_harness_work_items_goal
         ON harness_work_items(goal_id, ordinal)",
        "CREATE INDEX IF NOT EXISTS idx_harness_work_items_claim
         ON harness_work_items(status, next_attempt_at, lease_until, created_at)",
        "CREATE TABLE IF NOT EXISTS harness_work_item_dependencies (
            work_item_id TEXT NOT NULL REFERENCES harness_work_items(id),
            depends_on_work_item_id TEXT NOT NULL REFERENCES harness_work_items(id),
            PRIMARY KEY (work_item_id, depends_on_work_item_id)
        )",
        "CREATE TABLE IF NOT EXISTS harness_attempts (
            id TEXT PRIMARY KEY,
            goal_id TEXT NOT NULL REFERENCES harness_goals(id),
            work_item_id TEXT NOT NULL REFERENCES harness_work_items(id),
            attempt_number INTEGER NOT NULL,
            status TEXT NOT NULL,
            worker_id TEXT NOT NULL,
            lease_token TEXT NOT NULL,
            fencing_token INTEGER NOT NULL,
            started_at INTEGER NOT NULL,
            finished_at INTEGER,
            error TEXT,
            UNIQUE(work_item_id, attempt_number)
        )",
        "CREATE INDEX IF NOT EXISTS idx_harness_attempts_goal_started
         ON harness_attempts(goal_id, started_at, id)",
        "CREATE TABLE IF NOT EXISTS harness_checkpoints (
            sequence INTEGER PRIMARY KEY AUTOINCREMENT,
            id TEXT NOT NULL UNIQUE,
            goal_id TEXT NOT NULL REFERENCES harness_goals(id),
            work_item_id TEXT NOT NULL,
            attempt_id TEXT NOT NULL,
            phase TEXT NOT NULL,
            idempotency_key TEXT NOT NULL,
            outcome_json TEXT,
            created_at INTEGER NOT NULL DEFAULT (unixepoch()),
            updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
            UNIQUE(work_item_id, idempotency_key)
        )",
        "CREATE INDEX IF NOT EXISTS idx_harness_checkpoints_goal_sequence
         ON harness_checkpoints(goal_id, sequence DESC)",
        "CREATE TABLE IF NOT EXISTS harness_events (
            sequence INTEGER PRIMARY KEY AUTOINCREMENT,
            goal_id TEXT,
            event_type TEXT NOT NULL,
            actor TEXT NOT NULL,
            details_json TEXT NOT NULL,
            created_at INTEGER NOT NULL DEFAULT (unixepoch())
        )",
        "CREATE INDEX IF NOT EXISTS idx_harness_events_goal_sequence
         ON harness_events(goal_id, sequence)",
        "CREATE TABLE IF NOT EXISTS harness_manual_verifications (
            goal_id TEXT NOT NULL REFERENCES harness_goals(id),
            label TEXT NOT NULL,
            actor TEXT NOT NULL,
            created_at INTEGER NOT NULL DEFAULT (unixepoch()),
            PRIMARY KEY (goal_id, label)
        )",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .context("failed to initialize loop engine schema")?;
    }
    initialize_signal_schema(pool).await?;
    initialize_self_test_schema(pool).await?;
    initialize_replay_schema(pool).await?;
    initialize_artifact_schema(pool).await?;
    sqlx::query(
        "INSERT OR IGNORE INTO harness_schema_migrations (version, name)
         VALUES (1, 'loop_engine_initial_schema')",
    )
    .execute(pool)
    .await
    .context("failed to record loop engine schema version")?;
    Ok(())
}

fn decode_goal(row: sqlx::sqlite::SqliteRow) -> anyhow::Result<GoalRecord> {
    let revision: i64 = row.try_get("revision")?;
    let source_json: String = row.try_get("source_signal_ids_json")?;
    Ok(GoalRecord {
        id: row.try_get("id")?,
        objective: row.try_get("objective")?,
        status: GoalStatus::from_db(row.try_get("status")?)?,
        revision: u32::try_from(revision).context("invalid goal revision")?,
        source_signal_ids: serde_json::from_str(&source_json)
            .context("invalid goal source signals")?,
        created_by: row.try_get("created_by")?,
        created_at_unix: row.try_get("created_at")?,
        updated_at_unix: row.try_get("updated_at")?,
    })
}

fn decode_loop_event(row: sqlx::sqlite::SqliteRow) -> anyhow::Result<LoopEventRecord> {
    let details_json: String = row.try_get("details_json")?;
    Ok(LoopEventRecord {
        sequence: row.try_get("sequence")?,
        goal_id: row.try_get("goal_id")?,
        event_type: row.try_get("event_type")?,
        actor: row.try_get("actor")?,
        details: serde_json::from_str(&details_json).context("invalid loop event details")?,
        created_at_unix: row.try_get("created_at")?,
    })
}

fn decode_work_item(row: sqlx::sqlite::SqliteRow) -> anyhow::Result<WorkItemRecord> {
    let ordinal: i64 = row.try_get("ordinal")?;
    let max_attempts: i64 = row.try_get("max_attempts")?;
    let input_json: String = row.try_get("input_json")?;
    Ok(WorkItemRecord {
        id: row.try_get("id")?,
        goal_id: row.try_get("goal_id")?,
        status: WorkItemStatus::from_db(row.try_get("status")?)?,
        step_id: row.try_get("step_id")?,
        handler: row.try_get("handler")?,
        effect: EffectClass::from_db(row.try_get("effect_class")?)?,
        input: serde_json::from_str(&input_json).context("invalid work item input")?,
        max_attempts: u8::try_from(max_attempts).context("invalid max attempts")?,
        ordinal: u32::try_from(ordinal).context("invalid work item ordinal")?,
        dependency_ids: Vec::new(),
    })
}

fn decode_checkpoint(row: sqlx::sqlite::SqliteRow) -> anyhow::Result<CheckpointRecord> {
    let outcome_json: Option<String> = row.try_get("outcome_json")?;
    Ok(CheckpointRecord {
        id: row.try_get("id")?,
        goal_id: row.try_get("goal_id")?,
        work_item_id: row.try_get("work_item_id")?,
        attempt_id: row.try_get("attempt_id")?,
        phase: CheckpointPhase::from_db(row.try_get("phase")?)?,
        idempotency_key: row.try_get("idempotency_key")?,
        outcome: outcome_json
            .map(|value| serde_json::from_str(&value).context("invalid checkpoint outcome"))
            .transpose()?,
        created_at_unix: row.try_get("created_at")?,
        updated_at_unix: row.try_get("updated_at")?,
    })
}

fn decode_attempt(row: sqlx::sqlite::SqliteRow) -> anyhow::Result<AttemptRecord> {
    let attempt_number: i64 = row.try_get("attempt_number")?;
    let fencing_token: i64 = row.try_get("fencing_token")?;
    Ok(AttemptRecord {
        id: row.try_get("id")?,
        goal_id: row.try_get("goal_id")?,
        work_item_id: row.try_get("work_item_id")?,
        attempt_number: u8::try_from(attempt_number).context("invalid attempt number")?,
        status: AttemptStatus::from_db(row.try_get("status")?)?,
        worker_id: row.try_get("worker_id")?,
        lease_token: row.try_get("lease_token")?,
        fencing_token: u64::try_from(fencing_token).context("invalid fencing token")?,
        started_at_unix: row.try_get("started_at")?,
        finished_at_unix: row.try_get("finished_at")?,
        error: row.try_get("error")?,
    })
}

async fn ensure_claim_current(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    claim: &WorkClaim,
) -> anyhow::Result<()> {
    let fencing_token =
        i64::try_from(claim.attempt.fencing_token).context("fencing token exceeds sqlite range")?;
    let current: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM harness_work_items w
         JOIN harness_attempts a ON a.work_item_id = w.id
         WHERE w.id = ?1 AND w.goal_id = ?2 AND w.status = 'running'
           AND w.lease_token = ?3 AND w.fencing_token = ?4
           AND w.lease_until >= ?5
           AND a.id = ?6 AND a.lease_token = ?3 AND a.fencing_token = ?4
           AND a.status IN ('running', 'committing')",
    )
    .bind(&claim.work_item.id)
    .bind(&claim.attempt.goal_id)
    .bind(&claim.attempt.lease_token)
    .bind(fencing_token)
    .bind(unix_now())
    .bind(&claim.attempt.id)
    .fetch_one(&mut **tx)
    .await
    .context("failed to validate work claim")?;
    ensure!(current == 1, "work claim is stale or its lease expired");
    Ok(())
}

async fn load_checkpoint_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    checkpoint_id: &str,
) -> anyhow::Result<CheckpointRecord> {
    let row = sqlx::query(
        "SELECT id, goal_id, work_item_id, attempt_id, phase, idempotency_key,
                outcome_json, created_at, updated_at
         FROM harness_checkpoints WHERE id = ?1",
    )
    .bind(checkpoint_id)
    .fetch_optional(&mut **tx)
    .await?
    .context("checkpoint not found")?;
    decode_checkpoint(row)
}

async fn reconcile_checkpoint_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    checkpoint: &CheckpointRecord,
) -> anyhow::Result<()> {
    ensure!(
        checkpoint.phase == CheckpointPhase::Committed,
        "only committed checkpoints can be reconciled"
    );
    sqlx::query(
        "UPDATE harness_checkpoints
         SET phase = 'reconciled', updated_at = unixepoch()
         WHERE id = ?1 AND phase = 'committed'",
    )
    .bind(&checkpoint.id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE harness_attempts
         SET status = 'succeeded', finished_at = unixepoch(), error = NULL
         WHERE id = ?1 AND status IN ('running', 'committing')",
    )
    .bind(&checkpoint.attempt_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE harness_work_items
         SET status = 'succeeded', lease_token = NULL, lease_until = NULL,
             next_attempt_at = NULL, last_error = NULL, updated_at = unixepoch()
         WHERE id = ?1 AND status = 'running'",
    )
    .bind(&checkpoint.work_item_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn unlock_ready_work_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    goal_id: &str,
) -> anyhow::Result<u64> {
    let result = sqlx::query(
        "UPDATE harness_work_items AS candidate
         SET status = 'ready', updated_at = unixepoch()
         WHERE candidate.goal_id = ?1 AND candidate.status = 'pending'
           AND NOT EXISTS (
             SELECT 1 FROM harness_work_item_dependencies dependency
             JOIN harness_work_items predecessor
               ON predecessor.id = dependency.depends_on_work_item_id
             WHERE dependency.work_item_id = candidate.id
               AND predecessor.status != 'succeeded'
           )",
    )
    .bind(goal_id)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected())
}

async fn refresh_goal_status_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    goal_id: &str,
) -> anyhow::Result<()> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS total,
                SUM(CASE WHEN status = 'succeeded' THEN 1 ELSE 0 END) AS succeeded,
                SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END) AS failed,
                SUM(CASE WHEN status IN ('running', 'succeeded', 'waiting_confirmation')
                         THEN 1 ELSE 0 END) AS progressed
         FROM harness_work_items WHERE goal_id = ?1",
    )
    .bind(goal_id)
    .fetch_one(&mut **tx)
    .await?;
    let total: i64 = row.try_get("total")?;
    let succeeded: i64 = row.try_get::<Option<i64>, _>("succeeded")?.unwrap_or(0);
    let failed: i64 = row.try_get::<Option<i64>, _>("failed")?.unwrap_or(0);
    let progressed: i64 = row.try_get::<Option<i64>, _>("progressed")?.unwrap_or(0);
    let next_status = if total > 0 && succeeded == total {
        Some("verifying")
    } else if failed > 0 {
        Some("failed")
    } else if progressed > 0 {
        Some("active")
    } else {
        None
    };
    if let Some(next_status) = next_status {
        sqlx::query(
            "UPDATE harness_goals
             SET status = ?1,
                 revision = CASE WHEN status != ?1 THEN revision + 1 ELSE revision END,
                 updated_at = unixepoch()
             WHERE id = ?2
               AND status IN ('approved', 'active', 'verifying', 'failed')",
        )
        .bind(next_status)
        .bind(goal_id)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn recover_goal_state(pool: &SqlitePool, goal_id: &str, actor: &str) -> anyhow::Result<()> {
    ensure_valid_id(goal_id, "goal id")?;
    let mut tx = pool.begin().await?;
    let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM harness_goals WHERE id = ?1")
        .bind(goal_id)
        .fetch_one(&mut *tx)
        .await?;
    if exists == 0 {
        tx.commit().await?;
        return Ok(());
    }
    let committed = sqlx::query(
        "SELECT id, goal_id, work_item_id, attempt_id, phase, idempotency_key,
                outcome_json, created_at, updated_at
         FROM harness_checkpoints
         WHERE goal_id = ?1 AND phase = 'committed'
         ORDER BY sequence",
    )
    .bind(goal_id)
    .fetch_all(&mut *tx)
    .await?;
    let mut recovered = 0_u64;
    for row in committed {
        let checkpoint = decode_checkpoint(row)?;
        reconcile_checkpoint_tx(&mut tx, &checkpoint).await?;
        recovered += 1;
    }

    let now = unix_now();
    let expired = sqlx::query(
        "SELECT id, effect_class, max_attempts, backoff_secs,
                (SELECT COUNT(*) FROM harness_attempts a
                 WHERE a.work_item_id = harness_work_items.id) AS attempt_count,
                EXISTS(SELECT 1 FROM harness_checkpoints c
                       WHERE c.work_item_id = harness_work_items.id
                         AND c.phase = 'prepared') AS has_prepared
         FROM harness_work_items
         WHERE goal_id = ?1 AND status = 'running'
           AND lease_until IS NOT NULL AND lease_until < ?2",
    )
    .bind(goal_id)
    .bind(now)
    .fetch_all(&mut *tx)
    .await?;
    for row in expired {
        let work_item_id: String = row.try_get("id")?;
        let effect = EffectClass::from_db(row.try_get("effect_class")?)?;
        let max_attempts: i64 = row.try_get("max_attempts")?;
        let attempt_count: i64 = row.try_get("attempt_count")?;
        let backoff_secs: i64 = row.try_get("backoff_secs")?;
        let has_prepared: i64 = row.try_get("has_prepared")?;
        let (work_status, attempt_status, error, next_attempt_at) =
            if effect == EffectClass::ExternalWrite && has_prepared != 0 {
                (
                    "waiting_confirmation",
                    "waiting_confirmation",
                    "external effect outcome requires confirmation",
                    None,
                )
            } else if attempt_count >= max_attempts {
                (
                    "failed",
                    "failed",
                    "attempt budget exhausted after lease expiry",
                    None,
                )
            } else {
                (
                    "ready",
                    "abandoned",
                    "worker lease expired before commit",
                    Some(now + backoff_secs),
                )
            };
        sqlx::query(
            "UPDATE harness_attempts
             SET status = ?1, finished_at = ?2, error = ?3
             WHERE work_item_id = ?4 AND status IN ('running', 'committing')",
        )
        .bind(attempt_status)
        .bind(now)
        .bind(error)
        .bind(&work_item_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE harness_work_items
             SET status = ?1, lease_token = NULL, lease_until = NULL,
                 next_attempt_at = ?2, last_error = ?3, updated_at = unixepoch()
             WHERE id = ?4 AND status = 'running'",
        )
        .bind(work_status)
        .bind(next_attempt_at)
        .bind(error)
        .bind(&work_item_id)
        .execute(&mut *tx)
        .await?;
        recovered += 1;
    }
    let unlocked = unlock_ready_work_tx(&mut tx, goal_id).await?;
    refresh_goal_status_tx(&mut tx, goal_id).await?;
    if recovered > 0 || unlocked > 0 {
        insert_event(
            &mut tx,
            goal_id,
            "goal.recovered",
            actor,
            &serde_json::json!({
                "reconciled_or_expired": recovered,
                "unlocked_work_items": unlocked,
            })
            .to_string(),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

async fn insert_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    goal_id: &str,
    event_type: &str,
    actor: &str,
    details_json: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO harness_events (goal_id, event_type, actor, details_json)
         VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(goal_id)
    .bind(event_type)
    .bind(actor)
    .bind(details_json)
    .execute(&mut **tx)
    .await
    .context("failed to record loop event")?;
    Ok(())
}

fn ensure_valid_id(value: &str, label: &str) -> anyhow::Result<()> {
    ensure!(
        !value.trim().is_empty() && value.len() <= 160,
        "{label} must be 1..=160 bytes"
    );
    Ok(())
}

static LOOP_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn new_loop_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = LOOP_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{nanos:x}_{counter:x}")
}
