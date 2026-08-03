use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, ensure};
use async_trait::async_trait;

use crate::domain::{MessageRole, StoredMessage};
use crate::harness::evolution::EvolutionEngine;
use crate::provider::{ChatProvider, CompletionRequest};

use super::{
    ArtifactKind, EffectClass, GoalStatus, LoopEngine, PublishArtifactRequest, ResumeReport,
    TrajectoryFrameCapture, TrajectoryFrameDraft, WorkClaim, WorkOutcome,
};

pub struct WorkHandlerContext {
    pub engine: Arc<LoopEngine>,
    pub claim: WorkClaim,
}

#[async_trait]
pub trait WorkHandler: Send + Sync {
    fn name(&self) -> &'static str;
    fn effect_class(&self) -> EffectClass;

    fn retryable(&self, _error: &anyhow::Error) -> bool {
        true
    }

    async fn execute(&self, context: &WorkHandlerContext) -> anyhow::Result<WorkOutcome>;
}

#[derive(Default)]
pub struct WorkHandlerRegistry {
    handlers: HashMap<String, Arc<dyn WorkHandler>>,
}

impl WorkHandlerRegistry {
    pub fn register(&mut self, handler: Arc<dyn WorkHandler>) -> anyhow::Result<()> {
        ensure!(
            handler.effect_class() != EffectClass::ExternalWrite,
            "external_write handlers are not enabled in this release"
        );
        let name = handler.name();
        ensure!(!name.trim().is_empty(), "handler name cannot be empty");
        ensure!(
            self.handlers.insert(name.to_string(), handler).is_none(),
            "workflow handler is already registered: {name}"
        );
        Ok(())
    }

    fn get(&self, name: &str) -> Option<Arc<dyn WorkHandler>> {
        self.handlers.get(name).cloned()
    }
}

pub struct LoopWorker {
    engine: Arc<LoopEngine>,
    handlers: Arc<WorkHandlerRegistry>,
    worker_id: String,
    lease_secs: u32,
}

impl LoopWorker {
    pub fn with_builtins(
        engine: Arc<LoopEngine>,
        provider: Arc<dyn ChatProvider>,
        evolution_engine: Option<Arc<EvolutionEngine>>,
    ) -> Self {
        let mut handlers = WorkHandlerRegistry::default();
        handlers
            .register(Arc::new(GoalPlannerHandler))
            .expect("built-in handler names are unique");
        handlers
            .register(Arc::new(ProviderAnalysisHandler { provider }))
            .expect("built-in handler names are unique");
        handlers
            .register(Arc::new(SelfTestHandler))
            .expect("built-in handler names are unique");
        handlers
            .register(Arc::new(SessionReplayHandler))
            .expect("built-in handler names are unique");
        handlers
            .register(Arc::new(ManualGateHandler))
            .expect("built-in handler names are unique");
        if let Some(evolution_engine) = evolution_engine {
            handlers
                .register(Arc::new(EvolutionEvaluateHandler { evolution_engine }))
                .expect("built-in handler names are unique");
        } else {
            handlers
                .register(Arc::new(BoundedEvolutionUnavailableHandler))
                .expect("built-in handler names are unique");
        }
        Self {
            engine,
            handlers: Arc::new(handlers),
            worker_id: format!("loop-worker:{}", std::process::id()),
            lease_secs: 30,
        }
    }

    pub fn with_registry(
        engine: Arc<LoopEngine>,
        handlers: WorkHandlerRegistry,
        worker_id: impl Into<String>,
        lease_secs: u32,
    ) -> anyhow::Result<Self> {
        ensure!((1..=3600).contains(&lease_secs), "invalid worker lease");
        let worker_id = worker_id.into();
        ensure!(
            !worker_id.trim().is_empty() && worker_id.len() <= 160,
            "worker id must be 1..=160 bytes"
        );
        Ok(Self {
            engine,
            handlers: Arc::new(handlers),
            worker_id,
            lease_secs,
        })
    }

    pub fn with_runtime_options(
        mut self,
        worker_id: impl Into<String>,
        lease_secs: u32,
    ) -> anyhow::Result<Self> {
        ensure!((1..=3600).contains(&lease_secs), "invalid worker lease");
        let worker_id = worker_id.into();
        ensure!(
            !worker_id.trim().is_empty() && worker_id.len() <= 160,
            "worker id must be 1..=160 bytes"
        );
        self.worker_id = worker_id;
        self.lease_secs = lease_secs;
        Ok(self)
    }

    pub async fn dispatchable_goal_ids(&self, limit: usize) -> anyhow::Result<Vec<String>> {
        self.engine.list_dispatchable_goal_ids(limit).await
    }

    pub async fn run_goal_until_idle(
        &self,
        goal_id: &str,
        max_steps: usize,
    ) -> anyhow::Result<ResumeReport> {
        ensure!((1..=128).contains(&max_steps), "max_steps must be 1..=128");
        let actor = format!("runtime:{}", self.worker_id);
        let mut report = self.engine.resume_goal(goal_id, &actor).await?;
        for _ in 0..max_steps {
            if report.goal.status == GoalStatus::Verifying {
                self.engine.verify_goal(goal_id, &actor).await?;
                report = self.engine.resume_goal(goal_id, &actor).await?;
            }
            if is_terminal(report.goal.status) {
                return Ok(report);
            }
            let Some(claim) = self
                .engine
                .claim_goal_work(goal_id, &self.worker_id, self.lease_secs, &actor)
                .await?
            else {
                return self.engine.resume_goal(goal_id, &actor).await;
            };
            report = self.process_claim(claim, &actor).await?;
        }
        self.engine.resume_goal(goal_id, &actor).await
    }

    async fn process_claim(&self, claim: WorkClaim, actor: &str) -> anyhow::Result<ResumeReport> {
        let Some(handler) = self.handlers.get(&claim.work_item.handler) else {
            return self
                .engine
                .fail_attempt(
                    &claim,
                    &format!("unregistered workflow handler: {}", claim.work_item.handler),
                    false,
                    actor,
                )
                .await;
        };
        if handler.effect_class() != claim.work_item.effect {
            return self
                .engine
                .fail_attempt(
                    &claim,
                    "approved effect class does not match registered handler",
                    false,
                    actor,
                )
                .await;
        }
        let checkpoint = self
            .engine
            .prepare_checkpoint(
                &claim,
                &format!("{}:{}:v1", claim.work_item.id, claim.attempt.id),
                actor,
            )
            .await?;
        let context = WorkHandlerContext {
            engine: self.engine.clone(),
            claim: claim.clone(),
        };
        let (heartbeat_stop, heartbeat_rx) = tokio::sync::watch::channel(false);
        let heartbeat_engine = self.engine.clone();
        let heartbeat_claim = claim.clone();
        let heartbeat_secs = self.lease_secs;
        let heartbeat = tokio::spawn(async move {
            run_lease_heartbeat(
                heartbeat_engine,
                heartbeat_claim,
                heartbeat_secs,
                heartbeat_rx,
            )
            .await;
        });
        let execution = tokio::time::timeout(
            Duration::from_secs(u64::from(claim.budget.deadline_secs)),
            handler.execute(&context),
        )
        .await;
        let _ = heartbeat_stop.send(true);
        let _ = heartbeat.await;
        let outcome = match execution {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(error)) => {
                let retryable = handler.retryable(&error);
                return self
                    .engine
                    .fail_attempt(&claim, &format!("{error:#}"), retryable, actor)
                    .await;
            }
            Err(_) => {
                return self
                    .engine
                    .fail_attempt(&claim, "workflow handler deadline exceeded", true, actor)
                    .await;
            }
        };
        self.engine
            .commit_checkpoint(&claim, &checkpoint.id, outcome, actor)
            .await?;
        self.engine
            .finish_attempt(&claim, &checkpoint.id, actor)
            .await
    }
}

async fn run_lease_heartbeat(
    engine: Arc<LoopEngine>,
    claim: WorkClaim,
    lease_secs: u32,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let interval = Duration::from_secs(u64::from((lease_secs / 3).max(1)));
    loop {
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
                }
            }
            _ = tokio::time::sleep(interval) => {
                if engine.renew_claim(&claim, lease_secs).await.is_err() {
                    break;
                }
            }
        }
    }
}

struct ProviderAnalysisHandler {
    provider: Arc<dyn ChatProvider>,
}

#[async_trait]
impl WorkHandler for ProviderAnalysisHandler {
    fn name(&self) -> &'static str {
        "provider_analysis"
    }

    fn effect_class(&self) -> EffectClass {
        EffectClass::LocalWrite
    }

    async fn execute(&self, context: &WorkHandlerContext) -> anyhow::Result<WorkOutcome> {
        let name = "goal-analysis";
        let version = context.claim.work_item.id.as_str();
        if let Some(existing) = context
            .engine
            .find_artifact(ArtifactKind::AnalysisReport, name, version)
            .await?
        {
            return Ok(WorkOutcome {
                summary: "reused durable analysis artifact".to_string(),
                artifact_ids: vec![existing.id],
                evidence: serde_json::json!({"artifact_kind": "analysis_report"}),
            });
        }
        let objective = context
            .claim
            .work_item
            .input
            .get("objective")
            .and_then(serde_json::Value::as_str)
            .context("provider_analysis requires input.objective")?;
        let reservation = context
            .engine
            .reserve_provider_call(&context.claim.attempt.goal_id)
            .await?;
        let messages = vec![
            StoredMessage {
                role: MessageRole::System,
                content: "Analyze this harness goal. Identify constraints, failure modes, and a bounded next action. Do not invoke tools.".to_string(),
            },
            StoredMessage {
                role: MessageRole::User,
                content: objective.to_string(),
            },
        ];
        let now = unix_now();
        let remaining = u64::try_from((reservation.deadline_at_unix - now).max(1))
            .context("invalid provider deadline")?;
        let response = tokio::time::timeout(
            Duration::from_secs(remaining),
            self.provider
                .complete(CompletionRequest::from_messages(messages.clone())),
        )
        .await
        .context("provider analysis deadline exceeded")??;
        let response_bytes =
            u32::try_from(response.len()).context("provider response too large")?;
        context
            .engine
            .record_provider_response_bytes(&context.claim.attempt.goal_id, response_bytes)
            .await?;
        let model = self.provider.model_name().unwrap_or("unknown").to_string();
        context
            .engine
            .record_trajectory_frame(
                TrajectoryFrameDraft {
                    // Each attempt is its own replayable unit. Using the goal plus attempt
                    // number would collide when several workflow nodes make their first call.
                    trajectory_id: context.claim.attempt.id.clone(),
                    call_index: 0,
                    model: model.clone(),
                    provider_fingerprint: format!("model:{model};seed:unknown;config:unavailable"),
                    request_messages: messages,
                    request_was_json: false,
                    response: truncate_utf8(&response, 262_144),
                    capture: TrajectoryFrameCapture::Truncated,
                },
                "trajectory:loop-worker",
            )
            .await?;
        let artifact = context
            .engine
            .publish_artifact(
                PublishArtifactRequest {
                    kind: ArtifactKind::AnalysisReport,
                    name: name.to_string(),
                    version: version.to_string(),
                    content: serde_json::json!({
                        "analysis": truncate_utf8(&response, 24_000),
                        "model": model,
                    }),
                    source_goal_id: Some(context.claim.attempt.goal_id.clone()),
                    parent_artifact_id: None,
                },
                "handler:provider_analysis",
            )
            .await?
            .artifact;
        Ok(WorkOutcome {
            summary: truncate_utf8(&response, 8_000),
            artifact_ids: vec![artifact.id],
            evidence: serde_json::json!({"artifact_kind": "analysis_report"}),
        })
    }
}

struct SelfTestHandler;

#[async_trait]
impl WorkHandler for SelfTestHandler {
    fn name(&self) -> &'static str {
        "self_test_suite"
    }

    fn effect_class(&self) -> EffectClass {
        EffectClass::LocalWrite
    }

    async fn execute(&self, context: &WorkHandlerContext) -> anyhow::Result<WorkOutcome> {
        let suite = context
            .claim
            .work_item
            .input
            .get("suite")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("core");
        let name = format!("{suite}-self-test");
        let version = context.claim.work_item.id.as_str();
        if let Some(existing) = context
            .engine
            .find_artifact(ArtifactKind::SelfTestReport, &name, version)
            .await?
        {
            let run_id = existing
                .content
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            return Ok(WorkOutcome {
                summary: "reused durable self-test report".to_string(),
                artifact_ids: vec![existing.id],
                evidence: serde_json::json!({
                    "suite": suite,
                    "self_test_run_id": run_id,
                }),
            });
        }
        let run = context
            .engine
            .run_self_tests(suite, "handler:self_test_suite")
            .await?;
        ensure!(
            run.status == super::SelfTestStatus::Passed,
            "self-test suite failed"
        );
        let artifact = context
            .engine
            .publish_artifact(
                PublishArtifactRequest {
                    kind: ArtifactKind::SelfTestReport,
                    name,
                    version: version.to_string(),
                    content: serde_json::to_value(&run)?,
                    source_goal_id: Some(context.claim.attempt.goal_id.clone()),
                    parent_artifact_id: None,
                },
                "handler:self_test_suite",
            )
            .await?
            .artifact;
        Ok(WorkOutcome {
            summary: format!("self-test suite {suite} passed"),
            artifact_ids: vec![artifact.id],
            evidence: serde_json::json!({
                "suite": suite,
                "self_test_run_id": run.id,
            }),
        })
    }
}

struct SessionReplayHandler;

#[async_trait]
impl WorkHandler for SessionReplayHandler {
    fn name(&self) -> &'static str {
        "session_replay"
    }

    fn effect_class(&self) -> EffectClass {
        EffectClass::LocalWrite
    }

    async fn execute(&self, context: &WorkHandlerContext) -> anyhow::Result<WorkOutcome> {
        let trajectory_id = context
            .claim
            .work_item
            .input
            .get("trajectory_id")
            .and_then(serde_json::Value::as_str)
            .context("session_replay requires input.trajectory_id")?;
        let name = format!("replay-{trajectory_id}");
        let version = context.claim.work_item.id.as_str();
        if let Some(existing) = context
            .engine
            .find_artifact(ArtifactKind::ReplayCorpus, &name, version)
            .await?
        {
            return Ok(WorkOutcome {
                summary: "reused durable replay artifact".to_string(),
                artifact_ids: vec![existing.id],
                evidence: serde_json::json!({"trajectory_id": trajectory_id}),
            });
        }
        let replay = context
            .engine
            .run_structural_replay(trajectory_id, "handler:session_replay")
            .await?;
        let artifact = context
            .engine
            .publish_artifact(
                PublishArtifactRequest {
                    kind: ArtifactKind::ReplayCorpus,
                    name,
                    version: version.to_string(),
                    content: serde_json::to_value(&replay)?,
                    source_goal_id: Some(context.claim.attempt.goal_id.clone()),
                    parent_artifact_id: None,
                },
                "handler:session_replay",
            )
            .await?
            .artifact;
        Ok(WorkOutcome {
            summary: format!("structural replay {} completed", replay.id),
            artifact_ids: vec![artifact.id],
            evidence: serde_json::json!({"trajectory_id": trajectory_id, "replay_id": replay.id}),
        })
    }
}

struct GoalPlannerHandler;

#[async_trait]
impl WorkHandler for GoalPlannerHandler {
    fn name(&self) -> &'static str {
        "goal_planner"
    }

    fn effect_class(&self) -> EffectClass {
        EffectClass::LocalWrite
    }

    async fn execute(&self, context: &WorkHandlerContext) -> anyhow::Result<WorkOutcome> {
        let artifact = context
            .engine
            .publish_artifact(
                PublishArtifactRequest {
                    kind: ArtifactKind::DynamicWorkflow,
                    name: "goal-plan".to_string(),
                    version: context.claim.work_item.id.clone(),
                    content: context.claim.work_item.input.clone(),
                    source_goal_id: Some(context.claim.attempt.goal_id.clone()),
                    parent_artifact_id: None,
                },
                "handler:goal_planner",
            )
            .await?
            .artifact;
        Ok(WorkOutcome {
            summary: "goal plan artifact published".to_string(),
            artifact_ids: vec![artifact.id],
            evidence: serde_json::json!({"artifact_kind": "dynamic_workflow"}),
        })
    }
}

struct ManualGateHandler;

#[async_trait]
impl WorkHandler for ManualGateHandler {
    fn name(&self) -> &'static str {
        "manual_gate"
    }

    fn effect_class(&self) -> EffectClass {
        EffectClass::Read
    }

    async fn execute(&self, _context: &WorkHandlerContext) -> anyhow::Result<WorkOutcome> {
        Ok(WorkOutcome {
            summary: "manual gate reached; verification still requires operator evidence"
                .to_string(),
            artifact_ids: Vec::new(),
            evidence: serde_json::json!({"manual_gate": "reached"}),
        })
    }
}

struct BoundedEvolutionUnavailableHandler;

struct EvolutionEvaluateHandler {
    evolution_engine: Arc<EvolutionEngine>,
}

#[async_trait]
impl WorkHandler for EvolutionEvaluateHandler {
    fn name(&self) -> &'static str {
        "evolution_evaluate"
    }

    fn effect_class(&self) -> EffectClass {
        EffectClass::LocalWrite
    }

    async fn execute(&self, context: &WorkHandlerContext) -> anyhow::Result<WorkOutcome> {
        let candidate_id = context
            .claim
            .work_item
            .input
            .get("candidate_id")
            .and_then(serde_json::Value::as_str)
            .context("evolution_evaluate requires input.candidate_id")?;
        let eval_cases = self.evolution_engine.list_eval_cases(true).await?;
        let required_calls = eval_cases
            .len()
            .checked_mul(2)
            .context("evolution provider call count overflowed")?;
        ensure!(required_calls > 0, "evolution eval suite is empty");
        ensure!(
            required_calls
                <= usize::try_from(context.claim.budget.max_provider_calls)
                    .context("invalid provider call budget")?,
            "evolution evaluation requires {required_calls} provider calls but the workflow budget allows {}",
            context.claim.budget.max_provider_calls
        );
        let mut deadline_at_unix = None;
        for _ in 0..required_calls {
            deadline_at_unix = Some(
                context
                    .engine
                    .reserve_provider_call(&context.claim.attempt.goal_id)
                    .await?
                    .deadline_at_unix,
            );
        }
        let remaining_secs = u64::try_from(
            deadline_at_unix
                .context("evolution provider budget was not reserved")?
                .saturating_sub(unix_now())
                .max(1),
        )
        .context("invalid evolution deadline")?;
        let max_response_bytes = usize::try_from(context.claim.budget.max_response_bytes)
            .context("invalid evolution response byte budget")?;
        let (evaluation, response_bytes) = self
            .evolution_engine
            .evaluate_candidate_bounded_until(
                candidate_id,
                max_response_bytes,
                tokio::time::Instant::now() + Duration::from_secs(remaining_secs),
            )
            .await?;
        context
            .engine
            .record_provider_response_bytes(
                &context.claim.attempt.goal_id,
                u32::try_from(response_bytes).context("evolution response exceeds u32")?,
            )
            .await?;
        let artifact = context
            .engine
            .publish_artifact(
                PublishArtifactRequest {
                    kind: ArtifactKind::EvolutionEvaluation,
                    name: candidate_id.to_string(),
                    version: evaluation.id.clone(),
                    content: serde_json::json!({
                        "candidate_id": &evaluation.candidate_id,
                        "evaluation_id": &evaluation.id,
                        "decision": &evaluation.decision,
                        "baseline_score": evaluation.scorecard.baseline_score,
                        "candidate_score": evaluation.scorecard.candidate_score,
                        "score_delta": evaluation.scorecard.score_delta,
                        "regressions": evaluation.scorecard.regressions,
                        "total_cases": evaluation.scorecard.total_cases,
                    }),
                    source_goal_id: Some(context.claim.attempt.goal_id.clone()),
                    parent_artifact_id: None,
                },
                "handler:evolution_evaluate",
            )
            .await?
            .artifact;
        Ok(WorkOutcome {
            summary: format!("evolution evaluation {} completed", evaluation.id),
            artifact_ids: vec![artifact.id],
            evidence: serde_json::json!({
                "candidate_id": candidate_id,
                "evaluation_id": evaluation.id,
                "provider_calls": required_calls,
                "response_bytes": response_bytes,
            }),
        })
    }
}

#[async_trait]
impl WorkHandler for BoundedEvolutionUnavailableHandler {
    fn name(&self) -> &'static str {
        "evolution_evaluate"
    }

    fn effect_class(&self) -> EffectClass {
        EffectClass::LocalWrite
    }

    fn retryable(&self, _error: &anyhow::Error) -> bool {
        false
    }

    async fn execute(&self, _context: &WorkHandlerContext) -> anyhow::Result<WorkOutcome> {
        anyhow::bail!("evolution_evaluate requires agent.harness.evolution.enabled=true")
    }
}

fn is_terminal(status: GoalStatus) -> bool {
    matches!(
        status,
        GoalStatus::Achieved
            | GoalStatus::Failed
            | GoalStatus::Blocked
            | GoalStatus::Canceled
            | GoalStatus::Rejected
    )
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value[..boundary].to_string()
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
