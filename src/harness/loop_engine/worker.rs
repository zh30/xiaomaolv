use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, ensure};
use async_trait::async_trait;

use crate::domain::{MessageRole, StoredMessage};
use crate::harness::evolution::{EvalStepOutcome, EvolutionEngine};
use crate::provider::{ChatProvider, CompletionRequest};

use super::{
    ArtifactKind, EffectClass, GoalStatus, LoopEngine, PublishArtifactRequest, ResumeReport,
    TrajectoryFrameCapture, TrajectoryFrameDraft, WorkClaim, WorkContinuation, WorkItemStatus,
    WorkOutcome,
};

/// `last_error` prefix marking a work item that was blocked because its
/// handler was not registered in the claiming process. `requeue_blocked_items`
/// matches on this prefix to distinguish deployment-transient blocks from
/// dependency-failure cascades.
pub(crate) const MISSING_HANDLER_REASON_PREFIX: &str = "unregistered workflow handler: ";

pub struct WorkHandlerContext {
    pub engine: Arc<LoopEngine>,
    pub claim: WorkClaim,
    /// Committed phase progress handed to this attempt — `None` on the
    /// first phase, `Some` when resuming after a boundary or a crash.
    pub resume: Option<crate::harness::loop_engine::WorkPhaseProgress>,
    /// Latest committed outcome per declared dependency, keyed by step id —
    /// the declared DAG dataflow. Only the work item's own dependencies are
    /// visible; a handler cannot reach for undeclared step outputs.
    pub upstream: std::collections::BTreeMap<String, WorkOutcome>,
}

#[async_trait]
pub trait WorkHandler: Send + Sync {
    fn name(&self) -> &'static str;
    fn effect_class(&self) -> EffectClass;

    fn retryable(&self, _error: &anyhow::Error) -> bool {
        true
    }

    /// Declared crash semantics: `true` means a re-run after the attempt
    /// died before its checkpoint commit is safe (the handler is idempotent
    /// or effect-free). `false` means a crash between the effect and the
    /// commit is ambiguous — the work item parks in `waiting_confirmation`
    /// for an operator verdict instead of silently re-running. `true` by
    /// default, matching the previous implicit behaviour for non-external
    /// handlers; `external_write` handlers should declare `false`.
    fn replay_safe(&self) -> bool {
        true
    }

    async fn execute(&self, context: &WorkHandlerContext) -> anyhow::Result<WorkOutcome>;
}

/// Outbound delivery capability injected into `external_write` handlers such
/// as `channel_send`. `idempotency_key` is stable per (work item, attempt);
/// delivery is at-least-once — implementations should dedupe on the key where
/// the underlying channel supports it.
#[async_trait]
pub trait OutboundSender: Send + Sync {
    async fn send(
        &self,
        channel: &str,
        session_id: &str,
        text: &str,
        idempotency_key: &str,
    ) -> anyhow::Result<serde_json::Value>;
}

#[derive(Default)]
pub struct WorkHandlerRegistry {
    handlers: HashMap<String, Arc<dyn WorkHandler>>,
    external_write_allowlist: BTreeSet<String>,
}

impl WorkHandlerRegistry {
    /// Allowlist applied to `external_write` handler registration. Without it
    /// (the default) every external-write handler is rejected.
    pub fn with_external_write_allowlist(mut self, allowlist: BTreeSet<String>) -> Self {
        self.external_write_allowlist = allowlist;
        self
    }

    pub fn register(&mut self, handler: Arc<dyn WorkHandler>) -> anyhow::Result<()> {
        let name = handler.name();
        ensure!(
            handler.effect_class() != EffectClass::ExternalWrite
                || self.external_write_allowlist.contains(name),
            "external_write handler '{name}' is not in the allowlist"
        );
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

    /// Names of every handler registered in this process. The worker uses
    /// the list to re-queue work items that were blocked only because their
    /// handler was missing at dispatch time.
    pub fn handler_names(&self) -> Vec<String> {
        self.handlers.keys().cloned().collect()
    }
}

pub struct LoopWorker {
    engine: Arc<LoopEngine>,
    handlers: Arc<WorkHandlerRegistry>,
    worker_id: String,
    lease_secs: u32,
}

impl LoopWorker {
    /// `outbound` supplies the `channel_send` delivery capability. It is only
    /// registered when the engine's `ExternalWritePolicy` allowlists it —
    /// passing a sender without the allowlist entry is a startup error.
    pub fn with_builtins(
        engine: Arc<LoopEngine>,
        provider: Arc<dyn ChatProvider>,
        evolution_engine: Option<Arc<EvolutionEngine>>,
        outbound: Option<Arc<dyn OutboundSender>>,
    ) -> anyhow::Result<Self> {
        let mut handlers = WorkHandlerRegistry::default()
            .with_external_write_allowlist(engine.external_write_policy().allowed_handlers.clone());
        handlers
            .register(Arc::new(GoalPlannerHandler))
            .expect("built-in handler names are unique");
        handlers
            .register(Arc::new(WaitHandler))
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
        if let Some(sender) = outbound {
            handlers
                .register(Arc::new(ChannelSendHandler { sender }))
                .context(
                    "channel_send requires external_write_enabled and 'channel_send' in external_write_handlers",
                )?;
        }
        Ok(Self {
            engine,
            handlers: Arc::new(handlers),
            worker_id: format!("loop-worker:{}", std::process::id()),
            lease_secs: 30,
        })
    }

    /// Registers an additional handler after construction. Subsystems that
    /// own their provider/settings wiring (e.g. the durable swarm runner)
    /// use this so runtime-picked goals — including ones orphaned by a
    /// restart — can claim their items instead of parking them `blocked`.
    pub fn with_extra_handler(mut self, handler: Arc<dyn WorkHandler>) -> anyhow::Result<Self> {
        let handlers =
            Arc::get_mut(&mut self.handlers).context("handler registry already shared")?;
        handlers.register(handler)?;
        Ok(self)
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

    /// Fails goals whose execution deadline passed — see
    /// `LoopEngine::expire_overdue_goals`.
    pub async fn expire_overdue_goals(&self, limit: usize) -> anyhow::Result<Vec<String>> {
        self.engine.expire_overdue_goals(limit).await
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
            // A work item blocked solely because its handler was missing
            // becomes ready again once the handler exists in this process
            // (rolling deploys, late registration).
            if let Err(error) = self
                .engine
                .requeue_blocked_items(
                    goal_id,
                    MISSING_HANDLER_REASON_PREFIX,
                    &self.handlers.handler_names(),
                    &actor,
                )
                .await
            {
                tracing::warn!(%error, %goal_id, "blocked work item requeue failed");
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

    /// Drives a goal with `concurrency` claim loops racing on the same
    /// leased-claim seam. At-least-once claims + fencing tokens make
    /// duplicate claims safe; a worker whose claim comes back empty lingers
    /// briefly while other items are still `running`/`ready` (a sibling
    /// may unlock new work), and exits once the goal is terminal or nothing
    /// remains in flight. Used by fan-out workloads like durable swarms.
    pub async fn run_goal_concurrent(
        &self,
        goal_id: &str,
        concurrency: usize,
        max_steps: usize,
    ) -> anyhow::Result<ResumeReport> {
        let workers = concurrency.clamp(1, 8);
        if workers <= 1 {
            return self.run_goal_until_idle(goal_id, max_steps).await;
        }
        let mut handles = Vec::with_capacity(workers);
        for index in 0..workers {
            let child = LoopWorker {
                engine: self.engine.clone(),
                handlers: self.handlers.clone(),
                worker_id: format!("{}:c{}", self.worker_id, index),
                lease_secs: self.lease_secs,
            };
            let goal_id = goal_id.to_string();
            handles.push(tokio::spawn(async move {
                // `claim` errors on a non-dispatchable goal (verifying,
                // terminal) are structural, not transient — after any
                // error, re-read the goal and only retry while claimable
                // work remains in flight.
                let mut retries = 0u8;
                loop {
                    let outcome: anyhow::Result<ResumeReport> =
                        match child.run_goal_until_idle(&goal_id, max_steps).await {
                            Ok(report) => Ok(report),
                            Err(_) => child
                                .engine
                                .resume_goal(&goal_id, "runtime:concurrent")
                                .await
                                .context("failed to inspect goal after worker error"),
                        };
                    match outcome {
                        Ok(next) => {
                            let in_flight = next.work_items.iter().any(|item| {
                                matches!(
                                    item.status,
                                    WorkItemStatus::Running | WorkItemStatus::Ready
                                )
                            });
                            if is_terminal(next.goal.status) || !in_flight {
                                // Pending-alone (deps failed/parked) and
                                // waiting_confirmation both need a
                                // resolution path outside the worker loop.
                                return Ok(next);
                            }
                        }
                        Err(error) => {
                            retries += 1;
                            if retries > 10 {
                                return Err(error);
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(120)).await;
                }
            }));
        }
        // Join ALL workers: an erroring sibling must not strand attempts
        // still executing under the others.
        let mut best: Option<ResumeReport> = None;
        let mut first_error: Option<anyhow::Error> = None;
        for handle in handles {
            match handle.await {
                Ok(Ok(report)) => {
                    let dominated = best.as_ref().is_some_and(|current| {
                        report
                            .work_items
                            .iter()
                            .filter(|i| i.status == WorkItemStatus::Succeeded)
                            .count()
                            <= current
                                .work_items
                                .iter()
                                .filter(|i| i.status == WorkItemStatus::Succeeded)
                                .count()
                    });
                    if !dominated {
                        best = Some(report);
                    }
                }
                Ok(Err(error)) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
                Err(join) => {
                    if first_error.is_none() {
                        first_error = Some(
                            anyhow::Error::new(join).context("concurrent worker task panicked"),
                        );
                    }
                }
            }
        }
        match best {
            Some(report) => Ok(report),
            None => Err(first_error
                .unwrap_or_else(|| anyhow::anyhow!("no concurrent worker produced a report"))),
        }
    }

    async fn process_claim(&self, claim: WorkClaim, actor: &str) -> anyhow::Result<ResumeReport> {
        // Dispatch-time gate: the flag can be turned off after a plan was
        // approved, so admission at plan/approve time is not sufficient. A
        // disabled `external_write` step fails loudly regardless of whether
        // its handler is registered here — a policy decision, not a
        // deployment gap.
        if claim.work_item.effect == EffectClass::ExternalWrite
            && !self.engine.external_write_policy().enabled
        {
            return self
                .engine
                .fail_attempt(&claim, "external_write is disabled", false, actor)
                .await;
        }
        let Some(handler) = self.handlers.get(&claim.work_item.handler) else {
            // A missing handler may be a deployment transient (rolling
            // restart, handler registered later): park the item in `blocked`
            // instead of failing it outright. `requeue_blocked_items` at the
            // top of the loop brings it back once the handler exists.
            self.engine
                .block_work_item(
                    &claim,
                    &format!("{MISSING_HANDLER_REASON_PREFIX}{}", claim.work_item.handler),
                    actor,
                )
                .await?;
            return self.engine.resume_goal(&claim.attempt.goal_id, actor).await;
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
                handler.replay_safe(),
                actor,
            )
            .await?;
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
        // Phase loop: a `Continue` outcome commits its progress before the
        // next phase runs in the same attempt, so a crash mid-attempt resumes
        // from the last committed phase instead of restarting. A `delay_secs`
        // continuation yields the attempt entirely (durable sleep+progress).
        let attempt_deadline = tokio::time::Instant::now()
            + Duration::from_secs(u64::from(claim.budget.deadline_secs));
        let mut resume = claim.work_item.progress.clone();
        // Declared-dependency outcomes are loaded once per attempt — they
        // are already committed before this item could be claimed, so the
        // map is stable for the whole phase loop.
        let upstream = self
            .engine
            .upstream_outcomes(&claim.work_item.id)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(
                    error = %format!("{error:#}"),
                    work_item_id = %claim.work_item.id,
                    "failed to load upstream outcomes; handler sees an empty map"
                );
                std::collections::BTreeMap::new()
            });
        let outcome = loop {
            let context = WorkHandlerContext {
                engine: self.engine.clone(),
                claim: claim.clone(),
                resume: resume.clone(),
                upstream: upstream.clone(),
            };
            let execution =
                tokio::time::timeout_at(attempt_deadline, handler.execute(&context)).await;
            let outcome = match execution {
                Ok(Ok(outcome)) => outcome,
                Ok(Err(error)) => {
                    let retryable = handler.retryable(&error);
                    let _ = heartbeat_stop.send(true);
                    let _ = heartbeat.await;
                    return self
                        .engine
                        .fail_attempt(&claim, &format!("{error:#}"), retryable, actor)
                        .await;
                }
                Err(_) => {
                    let _ = heartbeat_stop.send(true);
                    let _ = heartbeat.await;
                    return self
                        .engine
                        .fail_attempt(&claim, "workflow handler deadline exceeded", true, actor)
                        .await;
                }
            };
            if let Err(error) = outcome.validate() {
                let _ = heartbeat_stop.send(true);
                let _ = heartbeat.await;
                return self
                    .engine
                    .fail_attempt(
                        &claim,
                        &format!("handler returned an invalid outcome: {error:#}"),
                        false,
                        actor,
                    )
                    .await;
            }
            match outcome.continuation.clone() {
                Some(continuation) if continuation.delay_secs.is_some() => {
                    let _ = heartbeat_stop.send(true);
                    let _ = heartbeat.await;
                    return self
                        .engine
                        .continue_attempt(&claim, &continuation, actor)
                        .await;
                }
                Some(continuation) => match self
                    .engine
                    .record_phase_progress(&claim, &continuation, actor)
                    .await
                {
                    Ok(progress) => resume = Some(progress),
                    Err(error) => {
                        let _ = heartbeat_stop.send(true);
                        let _ = heartbeat.await;
                        return self
                            .engine
                            .fail_attempt(&claim, &format!("{error:#}"), false, actor)
                            .await;
                    }
                },
                None => break outcome,
            }
        };
        let _ = heartbeat_stop.send(true);
        let _ = heartbeat.await;
        // A `sleep_until` outcome is not an outcome: the attempt defers and
        // the work item becomes claimable again at the stored wake time.
        // The sleep survives restarts without holding a lease.
        if let Some(until_unix) = outcome.sleep_until_unix {
            return self.engine.defer_attempt(&claim, until_unix, actor).await;
        }
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
                sleep_until_unix: None,
                continuation: None,
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
            sleep_until_unix: None,
            continuation: None,
        })
    }
}

/// `channel_send` — the first controlled `external_write` handler. Sends a
/// bounded text through the injected `OutboundSender`. The checkpoint written
/// by `process_claim` before `execute` runs carries the same idempotency key,
/// so a crash between send and commit parks the item in
/// `waiting_confirmation` instead of resending.
struct ChannelSendHandler {
    sender: Arc<dyn OutboundSender>,
}

#[async_trait]
impl WorkHandler for ChannelSendHandler {
    fn name(&self) -> &'static str {
        "channel_send"
    }

    fn effect_class(&self) -> EffectClass {
        EffectClass::ExternalWrite
    }

    /// A crash between send and commit cannot prove whether the message
    /// went out: the item must park for operator confirmation, never
    /// silently re-send.
    fn replay_safe(&self) -> bool {
        false
    }

    async fn execute(&self, context: &WorkHandlerContext) -> anyhow::Result<WorkOutcome> {
        let input = &context.claim.work_item.input;
        let channel = input
            .get("channel")
            .and_then(serde_json::Value::as_str)
            .context("channel_send requires input.channel")?;
        let session_id = input
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .context("channel_send requires input.session_id")?;
        let text = input
            .get("text")
            .and_then(serde_json::Value::as_str)
            .context("channel_send requires input.text")?;
        ensure!(
            !channel.trim().is_empty() && channel.len() <= 64,
            "channel_send channel must be 1..=64 bytes"
        );
        ensure!(
            !session_id.trim().is_empty() && session_id.len() <= 160,
            "channel_send session_id must be 1..=160 bytes"
        );
        ensure!(
            !text.trim().is_empty() && text.chars().count() <= 8192,
            "channel_send text must be 1..=8192 chars"
        );
        // Same key shape `process_claim` used for the prepared checkpoint:
        // (work item, attempt) is the at-least-once unit of delivery.
        let idempotency_key = format!(
            "{}:{}:v1",
            context.claim.work_item.id, context.claim.attempt.id
        );
        let mut evidence = self
            .sender
            .send(channel, session_id, text, &idempotency_key)
            .await?;
        if let serde_json::Value::Object(map) = &mut evidence {
            map.insert(
                "idempotency_key".to_string(),
                serde_json::Value::String(idempotency_key.clone()),
            );
        }
        Ok(WorkOutcome {
            summary: format!("sent channel message to {channel}:{session_id}"),
            artifact_ids: Vec::new(),
            evidence: serde_json::json!({
                "channel": channel,
                "session_id": session_id,
                "send": evidence,
            }),
            sleep_until_unix: None,
            continuation: None,
        })
    }
}

/// `wait` — a durable timer step. `input.until_unix` sets an absolute
/// deadline; `input.delay_secs` sets one relative to work-item creation.
/// While the deadline is in the future the attempt defers: the work item
/// holds no lease, the wake time is persisted, and a later claim completes
/// the step. Effect-free and always safe to re-run.
struct WaitHandler;

#[async_trait]
impl WorkHandler for WaitHandler {
    fn name(&self) -> &'static str {
        "wait"
    }

    fn effect_class(&self) -> EffectClass {
        EffectClass::Pure
    }

    async fn execute(&self, context: &WorkHandlerContext) -> anyhow::Result<WorkOutcome> {
        let input = &context.claim.work_item.input;
        let until = if let Some(until) = input.get("until_unix").and_then(serde_json::Value::as_i64)
        {
            until
        } else {
            let delay = input
                .get("delay_secs")
                .and_then(serde_json::Value::as_i64)
                .context("wait requires input.until_unix or input.delay_secs")?;
            ensure!(
                (1..=2_592_000).contains(&delay),
                "wait delay_secs must be 1..=2592000"
            );
            // Anchored to item creation so every attempt resolves the same
            // absolute deadline instead of re-arming the clock.
            context
                .claim
                .work_item
                .created_at_unix
                .saturating_add(delay)
        };
        ensure!(until > 0, "wait deadline must be positive");
        if unix_now() < until {
            return Ok(WorkOutcome {
                summary: format!("sleeping until {until}"),
                artifact_ids: Vec::new(),
                evidence: serde_json::json!({"until_unix": until}),
                sleep_until_unix: Some(until),
                continuation: None,
            });
        }
        Ok(WorkOutcome {
            summary: format!("wait deadline {until} reached"),
            artifact_ids: Vec::new(),
            evidence: serde_json::json!({"until_unix": until}),
            sleep_until_unix: None,
            continuation: None,
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
                sleep_until_unix: None,
                continuation: None,
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
            sleep_until_unix: None,
            continuation: None,
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
                sleep_until_unix: None,
                continuation: None,
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
            sleep_until_unix: None,
            continuation: None,
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
            sleep_until_unix: None,
            continuation: None,
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
            sleep_until_unix: None,
            continuation: None,
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
        // Per-phase budget: one step evaluates exactly one case (baseline +
        // candidate), so each phase reserves two provider calls — a crashed
        // attempt no longer burns reservations for the whole suite.
        let mut deadline_at_unix = None;
        for _ in 0..2 {
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
        let step = self
            .evolution_engine
            .run_evaluation_step(
                candidate_id,
                max_response_bytes,
                Some(tokio::time::Instant::now() + Duration::from_secs(remaining_secs)),
            )
            .await?;
        let (evaluation, response_bytes) = match step {
            EvalStepOutcome::Progress {
                completed_cases,
                total_cases,
                ..
            } => {
                return Ok(WorkOutcome {
                    summary: format!("evolution evaluation {completed_cases}/{total_cases} cases"),
                    artifact_ids: Vec::new(),
                    evidence: serde_json::Value::Null,
                    sleep_until_unix: None,
                    continuation: Some(WorkContinuation {
                        phase: "evaluating".to_string(),
                        state: serde_json::json!({
                            "completed_cases": completed_cases,
                            "total_cases": total_cases,
                        }),
                        delay_secs: None,
                    }),
                });
            }
            EvalStepOutcome::Completed {
                evaluation,
                response_bytes,
            } => (*evaluation, response_bytes),
        };
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
                "response_bytes": response_bytes,
            }),
            sleep_until_unix: None,
            continuation: None,
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
