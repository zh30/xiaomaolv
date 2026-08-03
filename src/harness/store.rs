use anyhow::{Context, bail};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::Row;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::harness::evolution::{
    ABSOLUTE_MAX_PROMPT_PATCH_CHARS, ActiveEvolutionPolicy, EvolutionAuditEvent,
    EvolutionCandidateDraft, EvolutionCandidateRecord, EvolutionCandidateStatus,
    EvolutionDeploymentRecord, EvolutionEvalCase, EvolutionEvaluationDraft,
    EvolutionEvaluationRecord, EvolutionFeedbackDraft, EvolutionFeedbackRecord,
    EvolutionPromotionDecision, EvolutionRollbackResult, MAX_ENABLED_EVOLUTION_EVAL_CASES,
    PromptPatch,
};
use crate::harness::loop_engine::{LoopStore, SqliteLoopStore, TrajectoryFrameDraft};
use crate::harness::trajectory::{
    ToolCallRecord, TrajectoryExitReason, TrajectoryFilter, TrajectoryRecord,
};
use crate::memory::{
    CompactionSummaryLoadRequest, CompactionSummaryRecord, CompactionSummaryUpsertRequest,
    SqliteMemoryStore,
};

#[async_trait]
pub trait HarnessStore: Send + Sync {
    async fn start_trajectory(
        &self,
        trajectory_id: &str,
        session_id: &str,
        channel: &str,
        user_id: &str,
        model: &str,
    ) -> anyhow::Result<()>;

    async fn insert_trajectory_tool_call(
        &self,
        trajectory_id: &str,
        record: ToolCallRecord,
    ) -> anyhow::Result<()>;

    async fn finish_trajectory(
        &self,
        trajectory_id: &str,
        final_answer: Option<String>,
        exit_reason: TrajectoryExitReason,
    ) -> anyhow::Result<()>;

    async fn get_trajectory(&self, trajectory_id: &str)
    -> anyhow::Result<Option<TrajectoryRecord>>;

    async fn query_trajectories(
        &self,
        filter: TrajectoryFilter,
    ) -> anyhow::Result<Vec<TrajectoryRecord>>;

    async fn load_compaction_summary(
        &self,
        req: CompactionSummaryLoadRequest,
    ) -> anyhow::Result<Option<CompactionSummaryRecord>>;

    async fn upsert_compaction_summary(
        &self,
        req: CompactionSummaryUpsertRequest,
    ) -> anyhow::Result<()>;

    async fn record_provider_frame(&self, _draft: TrajectoryFrameDraft) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
pub struct SqliteHarnessStore {
    store: SqliteMemoryStore,
}

impl SqliteHarnessStore {
    pub fn new(store: SqliteMemoryStore) -> Self {
        Self { store }
    }
}

#[async_trait]
impl HarnessStore for SqliteHarnessStore {
    async fn start_trajectory(
        &self,
        trajectory_id: &str,
        session_id: &str,
        channel: &str,
        user_id: &str,
        model: &str,
    ) -> anyhow::Result<()> {
        self.store
            .start_trajectory(trajectory_id, session_id, channel, user_id, model)
            .await
    }

    async fn insert_trajectory_tool_call(
        &self,
        trajectory_id: &str,
        record: ToolCallRecord,
    ) -> anyhow::Result<()> {
        self.store
            .insert_trajectory_tool_call(trajectory_id, record)
            .await
    }

    async fn finish_trajectory(
        &self,
        trajectory_id: &str,
        final_answer: Option<String>,
        exit_reason: TrajectoryExitReason,
    ) -> anyhow::Result<()> {
        self.store
            .finish_trajectory(trajectory_id, final_answer, exit_reason)
            .await
    }

    async fn get_trajectory(
        &self,
        trajectory_id: &str,
    ) -> anyhow::Result<Option<TrajectoryRecord>> {
        self.store.get_trajectory(trajectory_id).await
    }

    async fn query_trajectories(
        &self,
        filter: TrajectoryFilter,
    ) -> anyhow::Result<Vec<TrajectoryRecord>> {
        self.store.query_trajectories(filter).await
    }

    async fn load_compaction_summary(
        &self,
        req: CompactionSummaryLoadRequest,
    ) -> anyhow::Result<Option<CompactionSummaryRecord>> {
        self.store.load_compaction_summary(req).await
    }

    async fn upsert_compaction_summary(
        &self,
        req: CompactionSummaryUpsertRequest,
    ) -> anyhow::Result<()> {
        self.store.upsert_compaction_summary(req).await
    }

    async fn record_provider_frame(&self, draft: TrajectoryFrameDraft) -> anyhow::Result<()> {
        let store = SqliteLoopStore::new(self.store.clone());
        LoopStore::record_trajectory_frame(&store, draft, "trajectory:message-service")
            .await
            .map(|_| ())
    }
}

#[async_trait]
pub trait EvolutionStore: Send + Sync {
    async fn create_candidate(
        &self,
        draft: EvolutionCandidateDraft,
        actor: &str,
    ) -> anyhow::Result<EvolutionCandidateRecord>;

    async fn get_candidate(
        &self,
        candidate_id: &str,
    ) -> anyhow::Result<Option<EvolutionCandidateRecord>>;

    async fn list_candidates(&self, limit: usize) -> anyhow::Result<Vec<EvolutionCandidateRecord>>;

    async fn find_candidate_by_evidence(
        &self,
        evidence_fingerprint: &str,
    ) -> anyhow::Result<Option<EvolutionCandidateRecord>>;

    async fn transition_candidate(
        &self,
        candidate_id: &str,
        expected: EvolutionCandidateStatus,
        next: EvolutionCandidateStatus,
        actor: &str,
        details: serde_json::Value,
    ) -> anyhow::Result<EvolutionCandidateRecord>;

    async fn upsert_eval_case(
        &self,
        eval_case: EvolutionEvalCase,
        actor: &str,
    ) -> anyhow::Result<()>;

    async fn list_eval_cases(&self, enabled_only: bool) -> anyhow::Result<Vec<EvolutionEvalCase>>;

    async fn record_evaluation(
        &self,
        draft: EvolutionEvaluationDraft,
    ) -> anyhow::Result<EvolutionEvaluationRecord>;

    async fn latest_evaluation(
        &self,
        candidate_id: &str,
    ) -> anyhow::Result<Option<EvolutionEvaluationRecord>>;

    async fn record_feedback(
        &self,
        draft: EvolutionFeedbackDraft,
        actor: &str,
    ) -> anyhow::Result<EvolutionFeedbackRecord>;

    async fn list_feedback(
        &self,
        negative_only: bool,
        limit: usize,
    ) -> anyhow::Result<Vec<EvolutionFeedbackRecord>>;

    async fn activate_candidate(
        &self,
        candidate_id: &str,
        actor: &str,
        reason: &str,
    ) -> anyhow::Result<EvolutionDeploymentRecord>;

    async fn active_policy(&self) -> anyhow::Result<Option<ActiveEvolutionPolicy>>;

    async fn rollback_active(
        &self,
        actor: &str,
        reason: &str,
    ) -> anyhow::Result<EvolutionRollbackResult>;

    async fn record_audit_event(
        &self,
        candidate_id: Option<&str>,
        deployment_id: Option<&str>,
        event_type: &str,
        actor: &str,
        details: serde_json::Value,
    ) -> anyhow::Result<()>;

    async fn list_audit_events(&self, limit: usize) -> anyhow::Result<Vec<EvolutionAuditEvent>>;
}

#[derive(Clone)]
pub struct SqliteEvolutionStore {
    store: SqliteMemoryStore,
}

impl SqliteEvolutionStore {
    pub fn new(store: SqliteMemoryStore) -> Self {
        Self { store }
    }
}

#[async_trait]
impl EvolutionStore for SqliteEvolutionStore {
    async fn create_candidate(
        &self,
        draft: EvolutionCandidateDraft,
        actor: &str,
    ) -> anyhow::Result<EvolutionCandidateRecord> {
        draft.validate()?;
        validate_actor(actor)?;
        if draft.parent_candidate_id.as_deref() == Some(draft.id.as_str()) {
            bail!("candidate cannot be its own parent");
        }

        let mut tx = self
            .store
            .pool()
            .begin()
            .await
            .context("failed to start evolution candidate transaction")?;
        if let Some(parent_id) = &draft.parent_candidate_id {
            let parent_exists: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM evolution_candidates WHERE id = ?1")
                    .bind(parent_id)
                    .fetch_one(&mut *tx)
                    .await
                    .context("failed to validate parent evolution candidate")?;
            if parent_exists == 0 {
                bail!("parent evolution candidate '{parent_id}' does not exist");
            }
        }

        let source_ids = serde_json::to_string(&draft.source_trajectory_ids)
            .context("failed to serialize source trajectory ids")?;
        sqlx::query(
            "INSERT INTO evolution_candidates
             (id, parent_candidate_id, evidence_fingerprint, prompt_patch, rationale,
              source_trajectory_ids_json, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'draft')",
        )
        .bind(&draft.id)
        .bind(&draft.parent_candidate_id)
        .bind(&draft.evidence_fingerprint)
        .bind(draft.prompt_patch.as_str())
        .bind(draft.rationale.trim())
        .bind(source_ids)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("failed to insert evolution candidate '{}'", draft.id))?;

        insert_audit_event(
            &mut tx,
            Some(&draft.id),
            None,
            "candidate_created",
            actor,
            &serde_json::json!({
                "parent_candidate_id": draft.parent_candidate_id,
                "evidence_fingerprint": draft.evidence_fingerprint,
                "source_trajectory_ids": draft.source_trajectory_ids,
            }),
        )
        .await?;
        tx.commit()
            .await
            .context("failed to commit evolution candidate")?;

        self.get_candidate(&draft.id)
            .await?
            .context("created evolution candidate is missing")
    }

    async fn get_candidate(
        &self,
        candidate_id: &str,
    ) -> anyhow::Result<Option<EvolutionCandidateRecord>> {
        let row = sqlx::query(
            "SELECT id, parent_candidate_id, evidence_fingerprint, prompt_patch, rationale,
                    source_trajectory_ids_json, status, created_at, updated_at
             FROM evolution_candidates WHERE id = ?1",
        )
        .bind(candidate_id)
        .fetch_optional(self.store.pool())
        .await
        .context("failed to get evolution candidate")?;
        row.map(candidate_from_row).transpose()
    }

    async fn list_candidates(&self, limit: usize) -> anyhow::Result<Vec<EvolutionCandidateRecord>> {
        let rows = sqlx::query(
            "SELECT id, parent_candidate_id, evidence_fingerprint, prompt_patch, rationale,
                    source_trajectory_ids_json, status, created_at, updated_at
             FROM evolution_candidates
             ORDER BY created_at DESC, rowid DESC
             LIMIT ?1",
        )
        .bind(clamp_evolution_limit(limit) as i64)
        .fetch_all(self.store.pool())
        .await
        .context("failed to list evolution candidates")?;
        rows.into_iter().map(candidate_from_row).collect()
    }

    async fn find_candidate_by_evidence(
        &self,
        evidence_fingerprint: &str,
    ) -> anyhow::Result<Option<EvolutionCandidateRecord>> {
        let row = sqlx::query(
            "SELECT id, parent_candidate_id, evidence_fingerprint, prompt_patch, rationale,
                    source_trajectory_ids_json, status, created_at, updated_at
             FROM evolution_candidates
             WHERE evidence_fingerprint = ?1
             LIMIT 1",
        )
        .bind(evidence_fingerprint)
        .fetch_optional(self.store.pool())
        .await
        .context("failed to find evolution candidate by evidence")?;
        row.map(candidate_from_row).transpose()
    }

    async fn transition_candidate(
        &self,
        candidate_id: &str,
        expected: EvolutionCandidateStatus,
        next: EvolutionCandidateStatus,
        actor: &str,
        details: serde_json::Value,
    ) -> anyhow::Result<EvolutionCandidateRecord> {
        validate_actor(actor)?;
        if !expected.can_transition_to(next) {
            bail!(
                "invalid evolution candidate transition '{} -> {}'",
                expected.as_str(),
                next.as_str()
            );
        }

        let mut tx = self
            .store
            .pool()
            .begin()
            .await
            .context("failed to start evolution transition transaction")?;
        let result = sqlx::query(
            "UPDATE evolution_candidates
             SET status = ?3, updated_at = unixepoch()
             WHERE id = ?1 AND status = ?2",
        )
        .bind(candidate_id)
        .bind(expected.as_str())
        .bind(next.as_str())
        .execute(&mut *tx)
        .await
        .context("failed to transition evolution candidate")?;

        if result.rows_affected() != 1 {
            let actual: Option<String> =
                sqlx::query_scalar("SELECT status FROM evolution_candidates WHERE id = ?1")
                    .bind(candidate_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .context("failed to inspect evolution candidate transition conflict")?;
            match actual {
                Some(actual) => bail!(
                    "candidate '{candidate_id}' expected status '{}' but is '{actual}'",
                    expected.as_str()
                ),
                None => bail!("evolution candidate '{candidate_id}' does not exist"),
            }
        }

        insert_audit_event(
            &mut tx,
            Some(candidate_id),
            None,
            transition_event_type(next),
            actor,
            &details,
        )
        .await?;
        tx.commit()
            .await
            .context("failed to commit evolution candidate transition")?;

        self.get_candidate(candidate_id)
            .await?
            .context("transitioned evolution candidate is missing")
    }

    async fn upsert_eval_case(
        &self,
        eval_case: EvolutionEvalCase,
        actor: &str,
    ) -> anyhow::Result<()> {
        eval_case.validate()?;
        validate_actor(actor)?;
        let assertions = serde_json::to_string(&eval_case.assertions)
            .context("failed to serialize evolution eval assertions")?;
        let mut tx = self
            .store
            .pool()
            .begin()
            .await
            .context("failed to start evolution eval case transaction")?;
        if eval_case.enabled {
            let enabled_count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM evolution_eval_cases WHERE enabled = 1 AND id <> ?1",
            )
            .bind(&eval_case.id)
            .fetch_one(&mut *tx)
            .await
            .context("failed to count enabled evolution eval cases")?;
            if enabled_count as usize >= MAX_ENABLED_EVOLUTION_EVAL_CASES {
                bail!("enabled eval cases cannot exceed {MAX_ENABLED_EVOLUTION_EVAL_CASES}");
            }
        }
        sqlx::query(
            "INSERT INTO evolution_eval_cases
             (id, name, input, assertions_json, weight, enabled)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
               name = excluded.name,
               input = excluded.input,
               assertions_json = excluded.assertions_json,
               weight = excluded.weight,
               enabled = excluded.enabled,
               updated_at = unixepoch()",
        )
        .bind(&eval_case.id)
        .bind(&eval_case.name)
        .bind(&eval_case.input)
        .bind(assertions)
        .bind(eval_case.weight)
        .bind(eval_case.enabled)
        .execute(&mut *tx)
        .await
        .context("failed to upsert evolution eval case")?;

        insert_audit_event(
            &mut tx,
            None,
            None,
            "eval_case_upserted",
            actor,
            &serde_json::json!({"eval_case_id": eval_case.id}),
        )
        .await?;
        tx.commit()
            .await
            .context("failed to commit evolution eval case")?;
        Ok(())
    }

    async fn list_eval_cases(&self, enabled_only: bool) -> anyhow::Result<Vec<EvolutionEvalCase>> {
        let sql = if enabled_only {
            "SELECT id, name, input, assertions_json, weight, enabled
             FROM evolution_eval_cases WHERE enabled = 1 ORDER BY id ASC"
        } else {
            "SELECT id, name, input, assertions_json, weight, enabled
             FROM evolution_eval_cases ORDER BY id ASC"
        };
        let rows = sqlx::query(sql)
            .fetch_all(self.store.pool())
            .await
            .context("failed to list evolution eval cases")?;
        rows.into_iter().map(eval_case_from_row).collect()
    }

    async fn record_evaluation(
        &self,
        draft: EvolutionEvaluationDraft,
    ) -> anyhow::Result<EvolutionEvaluationRecord> {
        if draft.id.trim().is_empty() || draft.candidate_id.trim().is_empty() {
            bail!("evaluation id and candidate id cannot be empty");
        }
        draft.gate_config.validate()?;
        let scorecard = serde_json::to_string(&draft.scorecard)
            .context("failed to serialize evolution scorecard")?;
        let decision = serde_json::to_string(&draft.decision)
            .context("failed to serialize evolution decision")?;
        let gate_config = serde_json::to_string(&draft.gate_config)
            .context("failed to serialize evolution gate config")?;
        let mut tx = self
            .store
            .pool()
            .begin()
            .await
            .context("failed to start evolution evaluation transaction")?;
        let candidate_exists: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM evolution_candidates WHERE id = ?1")
                .bind(&draft.candidate_id)
                .fetch_one(&mut *tx)
                .await
                .context("failed to validate evaluated candidate")?;
        if candidate_exists == 0 {
            bail!(
                "evolution candidate '{}' does not exist",
                draft.candidate_id
            );
        }

        sqlx::query(
            "INSERT INTO evolution_evaluations
             (id, candidate_id, baseline_candidate_id, scorecard_json,
              decision_json, gate_config_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(&draft.id)
        .bind(&draft.candidate_id)
        .bind(&draft.baseline_candidate_id)
        .bind(scorecard)
        .bind(decision)
        .bind(gate_config)
        .execute(&mut *tx)
        .await
        .context("failed to record evolution evaluation")?;

        insert_audit_event(
            &mut tx,
            Some(&draft.candidate_id),
            None,
            "evaluation_recorded",
            "system:evolution-engine",
            &serde_json::json!({
                "evaluation_id": draft.id,
                "decision": draft.decision,
            }),
        )
        .await?;
        tx.commit()
            .await
            .context("failed to commit evolution evaluation")?;

        self.get_evaluation(&draft.id)
            .await?
            .context("recorded evolution evaluation is missing")
    }

    async fn latest_evaluation(
        &self,
        candidate_id: &str,
    ) -> anyhow::Result<Option<EvolutionEvaluationRecord>> {
        let row = sqlx::query(
            "SELECT id, candidate_id, baseline_candidate_id, scorecard_json,
                    decision_json, gate_config_json, created_at
             FROM evolution_evaluations
             WHERE candidate_id = ?1
             ORDER BY created_at DESC, rowid DESC
             LIMIT 1",
        )
        .bind(candidate_id)
        .fetch_optional(self.store.pool())
        .await
        .context("failed to load latest evolution evaluation")?;
        row.map(evaluation_from_row).transpose()
    }

    async fn record_feedback(
        &self,
        draft: EvolutionFeedbackDraft,
        actor: &str,
    ) -> anyhow::Result<EvolutionFeedbackRecord> {
        draft.validate()?;
        validate_actor(actor)?;
        let tags = draft
            .tags
            .iter()
            .map(|tag| tag.trim().to_string())
            .collect::<Vec<_>>();
        let tags_json =
            serde_json::to_string(&tags).context("failed to serialize evolution feedback tags")?;
        let comment = draft
            .comment
            .as_deref()
            .map(str::trim)
            .and_then(|comment| (!comment.is_empty()).then(|| comment.to_string()));
        let mut tx = self
            .store
            .pool()
            .begin()
            .await
            .context("failed to start evolution feedback transaction")?;
        let trajectory_exists: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM mcp_trajectories WHERE id = ?1")
                .bind(&draft.trajectory_id)
                .fetch_one(&mut *tx)
                .await
                .context("failed to validate feedback trajectory")?;
        if trajectory_exists == 0 {
            bail!("trajectory '{}' does not exist", draft.trajectory_id);
        }

        let result = sqlx::query(
            "INSERT INTO evolution_feedback
             (trajectory_id, score, tags_json, comment, actor)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(&draft.trajectory_id)
        .bind(draft.score)
        .bind(tags_json)
        .bind(&comment)
        .bind(actor)
        .execute(&mut *tx)
        .await
        .context("failed to insert evolution feedback")?;
        let feedback_id = result.last_insert_rowid();

        insert_audit_event(
            &mut tx,
            None,
            None,
            "feedback_recorded",
            actor,
            &serde_json::json!({
                "feedback_id": feedback_id,
                "trajectory_id": draft.trajectory_id,
                "score": draft.score,
                "tags": tags,
            }),
        )
        .await?;
        tx.commit()
            .await
            .context("failed to commit evolution feedback")?;

        let row = sqlx::query(
            "SELECT id, trajectory_id, score, tags_json, comment, actor, created_at
             FROM evolution_feedback WHERE id = ?1",
        )
        .bind(feedback_id)
        .fetch_one(self.store.pool())
        .await
        .context("recorded evolution feedback is missing")?;
        feedback_from_row(row)
    }

    async fn list_feedback(
        &self,
        negative_only: bool,
        limit: usize,
    ) -> anyhow::Result<Vec<EvolutionFeedbackRecord>> {
        let sql = if negative_only {
            "SELECT id, trajectory_id, score, tags_json, comment, actor, created_at
             FROM evolution_feedback WHERE score < 0
             ORDER BY created_at DESC, id DESC LIMIT ?1"
        } else {
            "SELECT id, trajectory_id, score, tags_json, comment, actor, created_at
             FROM evolution_feedback
             ORDER BY created_at DESC, id DESC LIMIT ?1"
        };
        let rows = sqlx::query(sql)
            .bind(clamp_evolution_limit(limit) as i64)
            .fetch_all(self.store.pool())
            .await
            .context("failed to list evolution feedback")?;
        rows.into_iter().map(feedback_from_row).collect()
    }

    async fn activate_candidate(
        &self,
        candidate_id: &str,
        actor: &str,
        reason: &str,
    ) -> anyhow::Result<EvolutionDeploymentRecord> {
        validate_actor(actor)?;
        validate_reason(reason)?;
        let deployment_id = new_evolution_id("deployment");
        let mut tx = self
            .store
            .pool()
            .begin()
            .await
            .context("failed to start evolution activation transaction")?;
        acquire_runtime_state_write_lock(&mut tx).await?;

        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM evolution_candidates WHERE id = ?1")
                .bind(candidate_id)
                .fetch_optional(&mut *tx)
                .await
                .context("failed to inspect evolution candidate before activation")?;
        match status.as_deref() {
            Some("approved") => {}
            Some(actual) => bail!(
                "candidate '{candidate_id}' must be approved before activation; current status is '{actual}'"
            ),
            None => bail!("evolution candidate '{candidate_id}' does not exist"),
        }

        let current_active_candidate_id: Option<String> = sqlx::query_scalar(
            "SELECT d.candidate_id
             FROM evolution_runtime_state s
             LEFT JOIN evolution_deployments d ON d.id = s.active_deployment_id
             WHERE s.singleton_id = 1",
        )
        .fetch_one(&mut *tx)
        .await
        .context("failed to load active candidate before activation")?;
        let latest_evaluation = sqlx::query(
            "SELECT baseline_candidate_id, decision_json
             FROM evolution_evaluations
             WHERE candidate_id = ?1
             ORDER BY created_at DESC, rowid DESC
             LIMIT 1",
        )
        .bind(candidate_id)
        .fetch_optional(&mut *tx)
        .await
        .context("failed to load candidate evaluation before activation")?
        .context("approved candidate has no evaluation")?;
        let evaluation_baseline: Option<String> = latest_evaluation.get("baseline_candidate_id");
        let decision_json: String = latest_evaluation.get("decision_json");
        let decision: EvolutionPromotionDecision = serde_json::from_str(&decision_json)
            .context("failed to deserialize activation evaluation decision")?;
        if !matches!(decision, EvolutionPromotionDecision::Ready) {
            bail!("approved candidate latest evaluation is not ready");
        }
        if evaluation_baseline != current_active_candidate_id {
            bail!(
                "candidate '{candidate_id}' was evaluated against a stale baseline and must be re-evaluated and re-approved"
            );
        }

        let previous_deployment_id: Option<String> = sqlx::query_scalar(
            "SELECT active_deployment_id FROM evolution_runtime_state WHERE singleton_id = 1",
        )
        .fetch_one(&mut *tx)
        .await
        .context("failed to load active evolution deployment")?;

        sqlx::query(
            "INSERT INTO evolution_deployments
             (id, candidate_id, previous_deployment_id, activated_by, reason)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(&deployment_id)
        .bind(candidate_id)
        .bind(&previous_deployment_id)
        .bind(actor)
        .bind(reason.trim())
        .execute(&mut *tx)
        .await
        .context("failed to insert evolution deployment")?;
        sqlx::query(
            "UPDATE evolution_runtime_state
             SET active_deployment_id = ?1, updated_at = unixepoch()
             WHERE singleton_id = 1",
        )
        .bind(&deployment_id)
        .execute(&mut *tx)
        .await
        .context("failed to activate evolution deployment")?;
        let deployment_row = sqlx::query(
            "SELECT id, candidate_id, previous_deployment_id, activated_by, reason,
                    activated_at, rolled_back_at, rolled_back_by, rollback_reason
             FROM evolution_deployments WHERE id = ?1",
        )
        .bind(&deployment_id)
        .fetch_one(&mut *tx)
        .await
        .context("created evolution deployment is missing")?;
        let deployment = deployment_from_row(deployment_row)?;

        insert_audit_event(
            &mut tx,
            Some(candidate_id),
            Some(&deployment_id),
            "candidate_activated",
            actor,
            &serde_json::json!({
                "reason": reason.trim(),
                "previous_deployment_id": previous_deployment_id,
            }),
        )
        .await?;
        tx.commit()
            .await
            .context("failed to commit evolution activation")?;
        Ok(deployment)
    }

    async fn active_policy(&self) -> anyhow::Result<Option<ActiveEvolutionPolicy>> {
        let row = sqlx::query(
            "SELECT d.id AS deployment_id, d.candidate_id, c.prompt_patch
             FROM evolution_runtime_state s
             JOIN evolution_deployments d ON d.id = s.active_deployment_id
             JOIN evolution_candidates c ON c.id = d.candidate_id
             WHERE s.singleton_id = 1 AND d.rolled_back_at IS NULL",
        )
        .fetch_optional(self.store.pool())
        .await
        .context("failed to load active evolution policy")?;
        row.map(|row| {
            let prompt_patch: String = row.get("prompt_patch");
            Ok(ActiveEvolutionPolicy {
                deployment_id: row.get("deployment_id"),
                candidate_id: row.get("candidate_id"),
                prompt_patch: PromptPatch::new(prompt_patch, ABSOLUTE_MAX_PROMPT_PATCH_CHARS)?,
            })
        })
        .transpose()
    }

    async fn rollback_active(
        &self,
        actor: &str,
        reason: &str,
    ) -> anyhow::Result<EvolutionRollbackResult> {
        validate_actor(actor)?;
        validate_reason(reason)?;
        let mut tx = self
            .store
            .pool()
            .begin()
            .await
            .context("failed to start evolution rollback transaction")?;
        acquire_runtime_state_write_lock(&mut tx).await?;

        let active = sqlx::query(
            "SELECT d.id, d.candidate_id, d.previous_deployment_id, d.rolled_back_at
             FROM evolution_runtime_state s
             JOIN evolution_deployments d ON d.id = s.active_deployment_id
             WHERE s.singleton_id = 1",
        )
        .fetch_optional(&mut *tx)
        .await
        .context("failed to load active deployment for rollback")?
        .context("there is no active evolution deployment to roll back")?;
        let deployment_id: String = active.get("id");
        let candidate_id: String = active.get("candidate_id");
        let previous_deployment_id: Option<String> = active.get("previous_deployment_id");
        let rolled_back_at: Option<i64> = active.get("rolled_back_at");
        if rolled_back_at.is_some() {
            bail!("active evolution deployment '{deployment_id}' is already rolled back");
        }
        let restored_policy = if let Some(previous_id) = previous_deployment_id.as_deref() {
            let row = sqlx::query(
                "SELECT d.id AS deployment_id, d.candidate_id, c.prompt_patch
                 FROM evolution_deployments d
                 JOIN evolution_candidates c ON c.id = d.candidate_id
                 WHERE d.id = ?1 AND d.rolled_back_at IS NULL",
            )
            .bind(previous_id)
            .fetch_optional(&mut *tx)
            .await
            .context("failed to load rollback restoration policy")?
            .context("previous evolution deployment is unavailable for rollback")?;
            let prompt_patch: String = row.get("prompt_patch");
            Some(ActiveEvolutionPolicy {
                deployment_id: row.get("deployment_id"),
                candidate_id: row.get("candidate_id"),
                prompt_patch: PromptPatch::new(prompt_patch, ABSOLUTE_MAX_PROMPT_PATCH_CHARS)?,
            })
        } else {
            None
        };

        sqlx::query(
            "UPDATE evolution_deployments
             SET rolled_back_at = unixepoch(), rolled_back_by = ?2, rollback_reason = ?3
             WHERE id = ?1 AND rolled_back_at IS NULL",
        )
        .bind(&deployment_id)
        .bind(actor)
        .bind(reason.trim())
        .execute(&mut *tx)
        .await
        .context("failed to mark evolution deployment rolled back")?;
        sqlx::query(
            "UPDATE evolution_runtime_state
             SET active_deployment_id = ?1, updated_at = unixepoch()
             WHERE singleton_id = 1",
        )
        .bind(&previous_deployment_id)
        .execute(&mut *tx)
        .await
        .context("failed to restore previous evolution deployment")?;

        insert_audit_event(
            &mut tx,
            Some(&candidate_id),
            Some(&deployment_id),
            "deployment_rolled_back",
            actor,
            &serde_json::json!({
                "reason": reason.trim(),
                "restored_deployment_id": previous_deployment_id,
            }),
        )
        .await?;
        tx.commit()
            .await
            .context("failed to commit evolution rollback")?;

        Ok(EvolutionRollbackResult {
            rolled_back_deployment_id: deployment_id,
            rolled_back_candidate_id: candidate_id,
            restored_policy,
        })
    }

    async fn record_audit_event(
        &self,
        candidate_id: Option<&str>,
        deployment_id: Option<&str>,
        event_type: &str,
        actor: &str,
        details: serde_json::Value,
    ) -> anyhow::Result<()> {
        validate_audit_fields(event_type, actor)?;
        let mut tx = self
            .store
            .pool()
            .begin()
            .await
            .context("failed to start evolution audit transaction")?;
        insert_audit_event(
            &mut tx,
            candidate_id,
            deployment_id,
            event_type,
            actor,
            &details,
        )
        .await?;
        tx.commit()
            .await
            .context("failed to commit evolution audit event")?;
        Ok(())
    }

    async fn list_audit_events(&self, limit: usize) -> anyhow::Result<Vec<EvolutionAuditEvent>> {
        let rows = sqlx::query(
            "SELECT id, candidate_id, deployment_id, event_type, actor,
                    details_json, created_at
             FROM evolution_audit_events
             ORDER BY created_at DESC, id DESC
             LIMIT ?1",
        )
        .bind(clamp_evolution_limit(limit) as i64)
        .fetch_all(self.store.pool())
        .await
        .context("failed to list evolution audit events")?;
        rows.into_iter().map(audit_event_from_row).collect()
    }
}

impl SqliteEvolutionStore {
    async fn get_evaluation(
        &self,
        evaluation_id: &str,
    ) -> anyhow::Result<Option<EvolutionEvaluationRecord>> {
        let row = sqlx::query(
            "SELECT id, candidate_id, baseline_candidate_id, scorecard_json,
                    decision_json, gate_config_json, created_at
             FROM evolution_evaluations WHERE id = ?1",
        )
        .bind(evaluation_id)
        .fetch_optional(self.store.pool())
        .await
        .context("failed to get evolution evaluation")?;
        row.map(evaluation_from_row).transpose()
    }
}

async fn acquire_runtime_state_write_lock(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE evolution_runtime_state SET updated_at = updated_at WHERE singleton_id = 1",
    )
    .execute(&mut **tx)
    .await
    .context("failed to lock evolution runtime state")?;
    Ok(())
}

async fn insert_audit_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    candidate_id: Option<&str>,
    deployment_id: Option<&str>,
    event_type: &str,
    actor: &str,
    details: &serde_json::Value,
) -> anyhow::Result<()> {
    validate_audit_fields(event_type, actor)?;
    let details_json =
        serde_json::to_string(details).context("failed to serialize evolution audit details")?;
    if details_json.len() > 65_536 {
        bail!("evolution audit details cannot exceed 65536 bytes");
    }
    sqlx::query(
        "INSERT INTO evolution_audit_events
         (candidate_id, deployment_id, event_type, actor, details_json)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(candidate_id)
    .bind(deployment_id)
    .bind(event_type)
    .bind(actor)
    .bind(details_json)
    .execute(&mut **tx)
    .await
    .context("failed to insert evolution audit event")?;
    Ok(())
}

fn candidate_from_row(row: sqlx::sqlite::SqliteRow) -> anyhow::Result<EvolutionCandidateRecord> {
    let source_ids_json: String = row.get("source_trajectory_ids_json");
    let prompt_patch: String = row.get("prompt_patch");
    let status: String = row.get("status");
    Ok(EvolutionCandidateRecord {
        id: row.get("id"),
        parent_candidate_id: row.get("parent_candidate_id"),
        evidence_fingerprint: row.get("evidence_fingerprint"),
        prompt_patch: PromptPatch::new(prompt_patch, ABSOLUTE_MAX_PROMPT_PATCH_CHARS)?,
        rationale: row.get("rationale"),
        source_trajectory_ids: serde_json::from_str(&source_ids_json)
            .context("failed to deserialize evolution source trajectory ids")?,
        status: EvolutionCandidateStatus::parse(&status)?,
        created_at: evolution_timestamp(row.get("created_at")),
        updated_at: evolution_timestamp(row.get("updated_at")),
    })
}

fn eval_case_from_row(row: sqlx::sqlite::SqliteRow) -> anyhow::Result<EvolutionEvalCase> {
    let assertions_json: String = row.get("assertions_json");
    Ok(EvolutionEvalCase {
        id: row.get("id"),
        name: row.get("name"),
        input: row.get("input"),
        assertions: serde_json::from_str(&assertions_json)
            .context("failed to deserialize evolution eval assertions")?,
        weight: row.get("weight"),
        enabled: row.get("enabled"),
    })
}

fn evaluation_from_row(row: sqlx::sqlite::SqliteRow) -> anyhow::Result<EvolutionEvaluationRecord> {
    let scorecard_json: String = row.get("scorecard_json");
    let decision_json: String = row.get("decision_json");
    let gate_config_json: String = row.get("gate_config_json");
    Ok(EvolutionEvaluationRecord {
        id: row.get("id"),
        candidate_id: row.get("candidate_id"),
        baseline_candidate_id: row.get("baseline_candidate_id"),
        scorecard: serde_json::from_str(&scorecard_json)
            .context("failed to deserialize evolution scorecard")?,
        decision: serde_json::from_str(&decision_json)
            .context("failed to deserialize evolution decision")?,
        gate_config: serde_json::from_str(&gate_config_json)
            .context("failed to deserialize evolution gate config")?,
        created_at: evolution_timestamp(row.get("created_at")),
    })
}

fn deployment_from_row(row: sqlx::sqlite::SqliteRow) -> anyhow::Result<EvolutionDeploymentRecord> {
    Ok(EvolutionDeploymentRecord {
        id: row.get("id"),
        candidate_id: row.get("candidate_id"),
        previous_deployment_id: row.get("previous_deployment_id"),
        activated_by: row.get("activated_by"),
        reason: row.get("reason"),
        activated_at: evolution_timestamp(row.get("activated_at")),
        rolled_back_at: row
            .get::<Option<i64>, _>("rolled_back_at")
            .map(evolution_timestamp),
        rolled_back_by: row.get("rolled_back_by"),
        rollback_reason: row.get("rollback_reason"),
    })
}

fn feedback_from_row(row: sqlx::sqlite::SqliteRow) -> anyhow::Result<EvolutionFeedbackRecord> {
    let tags_json: String = row.get("tags_json");
    Ok(EvolutionFeedbackRecord {
        id: row.get("id"),
        trajectory_id: row.get("trajectory_id"),
        score: row.get("score"),
        tags: serde_json::from_str(&tags_json)
            .context("failed to deserialize evolution feedback tags")?,
        comment: row.get("comment"),
        actor: row.get("actor"),
        created_at: evolution_timestamp(row.get("created_at")),
    })
}

fn audit_event_from_row(row: sqlx::sqlite::SqliteRow) -> anyhow::Result<EvolutionAuditEvent> {
    let details_json: String = row.get("details_json");
    Ok(EvolutionAuditEvent {
        id: row.get("id"),
        candidate_id: row.get("candidate_id"),
        deployment_id: row.get("deployment_id"),
        event_type: row.get("event_type"),
        actor: row.get("actor"),
        details: serde_json::from_str(&details_json)
            .context("failed to deserialize evolution audit details")?,
        created_at: evolution_timestamp(row.get("created_at")),
    })
}

fn evolution_timestamp(value: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(value, 0).unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
}

fn validate_actor(actor: &str) -> anyhow::Result<()> {
    let actor = actor.trim();
    if actor.is_empty() {
        bail!("evolution actor cannot be empty");
    }
    if actor.len() > 160 || actor.chars().any(char::is_control) {
        bail!("evolution actor cannot exceed 160 bytes or contain control characters");
    }
    Ok(())
}

fn validate_audit_fields(event_type: &str, actor: &str) -> anyhow::Result<()> {
    validate_actor(actor)?;
    let event_type = event_type.trim();
    if event_type.is_empty() || event_type.len() > 128 {
        bail!("evolution audit event type must contain between 1 and 128 bytes");
    }
    Ok(())
}

fn validate_reason(reason: &str) -> anyhow::Result<()> {
    let reason = reason.trim();
    if reason.is_empty() {
        bail!("evolution reason cannot be empty");
    }
    if reason.chars().count() > 2_000 {
        bail!("evolution reason cannot exceed 2000 characters");
    }
    Ok(())
}

fn transition_event_type(next: EvolutionCandidateStatus) -> &'static str {
    match next {
        EvolutionCandidateStatus::Evaluating => "evaluation_started",
        EvolutionCandidateStatus::Ready => "candidate_ready",
        EvolutionCandidateStatus::Rejected => "candidate_rejected",
        EvolutionCandidateStatus::Approved => "candidate_approved",
        EvolutionCandidateStatus::Failed => "candidate_failed",
        EvolutionCandidateStatus::Draft => "candidate_drafted",
    }
}

fn clamp_evolution_limit(limit: usize) -> usize {
    if limit == 0 { 100 } else { limit.min(500) }
}

static EVOLUTION_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn new_evolution_id(prefix: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let sequence = EVOLUTION_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{now:x}-{sequence:x}")
}
