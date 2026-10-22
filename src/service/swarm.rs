use super::*;

use crate::harness::loop_engine::{
    AcceptanceCriterion, ApproveGoalRequest, ArtifactKind, CreateGoalRequest, EffectClass,
    ExecutionBudget, GoalStatus, PlanGoalRequest, PublishArtifactRequest, ResumeReport,
    RetryPolicy, WorkClaim, WorkHandlerContext, WorkOutcome, WorkflowSpec, WorkflowStep,
};

/// Actor used for Loop Engine records created by the swarm projection. The
/// `internal:` prefix is required by `extend_workflow` and keeps these writes
/// attributable to the subsystem rather than an operator.
const SWARM_LOOP_ACTOR: &str = "internal:swarm";
/// Loop Engine handler recorded for every projected swarm node. The real
/// execution stays in-process; the handler name documents the work class.
const SWARM_NODE_HANDLER: &str = "provider_analysis";

#[derive(Debug, Clone)]
pub struct AgentSwarmSettings {
    pub enabled: bool,
    pub auto_detect: bool,
    pub max_depth: usize,
    pub max_agents: usize,
    pub max_parallel: usize,
    pub max_node_timeout_ms: u64,
    pub max_run_timeout_ms: u64,
    pub reply_summary_enabled: bool,
    pub audit_retention_days: u32,
    /// Execute nodes as durable loop-engine work items when a LoopEngine is
    /// wired; `false` keeps the legacy in-process runner + mirror projection.
    pub durable_engine: bool,
    /// How long a chat turn waits for the swarm answer before the reply is
    /// deferred (ack now, janitor delivers the final answer later). Serial
    /// per-channel processing means a long sync wait head-blocks updates.
    /// `0` means always defer — the run only ever answers asynchronously.
    pub reply_sync_wait_ms: u64,
    /// Swarm runs are expensive — auto-detect fires only in private chats
    /// unless this is enabled. Group messages keep the normal reply path.
    pub group_enabled: bool,
}

impl Default for AgentSwarmSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_detect: true,
            max_depth: 3,
            max_agents: 16,
            max_parallel: 4,
            max_node_timeout_ms: 20_000,
            max_run_timeout_ms: 90_000,
            reply_summary_enabled: true,
            audit_retention_days: 30,
            durable_engine: true,
            reply_sync_wait_ms: 45_000,
            group_enabled: false,
        }
    }
}

/// Best-effort durable projection of one swarm run into Loop Engine state.
/// One per run, cloned into `SwarmExecutionShared`. Every projection failure
/// is logged and downgraded to `None` so the reply path is never affected.
#[derive(Clone)]
struct SwarmLoopProjection {
    engine: Arc<LoopEngine>,
    goal_id: String,
    worker_id: String,
}

impl std::fmt::Debug for SwarmLoopProjection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SwarmLoopProjection")
            .field("goal_id", &self.goal_id)
            .field("worker_id", &self.worker_id)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
struct SwarmExecutionShared {
    run_id: String,
    channel: String,
    settings: AgentSwarmSettings,
    semaphore: Arc<Semaphore>,
    loop_projection: Option<SwarmLoopProjection>,
    agent_counter: Arc<AtomicUsize>,
    success_count: Arc<AtomicUsize>,
    partial_count: Arc<AtomicUsize>,
    failed_count: Arc<AtomicUsize>,
    timeout_count: Arc<AtomicUsize>,
    skipped_count: Arc<AtomicUsize>,
}

impl SwarmExecutionShared {
    fn next_agent_index(&self) -> usize {
        self.agent_counter.fetch_add(1, Ordering::Relaxed) + 1
    }
}

#[derive(Debug, Clone)]
struct SwarmAgentSpec {
    task: String,
    role_name: String,
    role_definition: String,
    nickname: Option<String>,
}

/// Per-node identity assigned at dispatch: `agent_id` is `{run_id}:{index}`.
#[derive(Debug, Clone)]
struct SwarmNodeIdentity {
    agent_id: String,
    index: usize,
}

#[derive(Debug, Clone)]
struct SwarmNodeOutcome {
    agent_id: String,
    nickname: String,
    role_name: String,
    answer: String,
    status: AgentSwarmNodeExitStatus,
    partial: bool,
    failed: bool,
}

#[derive(Debug)]
enum SwarmModelError {
    Timeout,
    Failure(anyhow::Error),
}

#[derive(Debug, Deserialize)]
struct SwarmActivationDecision {
    use_swarm: bool,
}

#[derive(Debug, Deserialize)]
struct SwarmNodePlanEnvelope {
    mode: Option<String>,
    answer: Option<String>,
    children: Option<Vec<SwarmChildPlan>>,
}

#[derive(Debug, Deserialize, Clone)]
struct SwarmChildPlan {
    task: String,
    role_name: Option<String>,
    role_definition: Option<String>,
    nickname: Option<String>,
}

impl MessageService {
    pub async fn list_agent_swarm_runs(
        &self,
        channel: String,
        user_id: Option<String>,
        limit: usize,
    ) -> anyhow::Result<Vec<AgentSwarmRunRecord>> {
        self.memory
            .list_agent_swarm_runs(AgentSwarmRunListRequest {
                channel,
                user_id,
                limit,
            })
            .await
            .context("failed to list agent swarm runs")
    }

    pub async fn load_agent_swarm_tree(
        &self,
        channel: String,
        run_id: String,
    ) -> anyhow::Result<Vec<AgentSwarmNodeRecord>> {
        self.memory
            .load_agent_swarm_tree(AgentSwarmTreeRequest { channel, run_id })
            .await
            .context("failed to load agent swarm tree")
    }

    pub async fn load_agent_swarm_node(
        &self,
        channel: String,
        agent_id: String,
    ) -> anyhow::Result<Option<AgentSwarmNodeRecord>> {
        self.memory
            .load_agent_swarm_node(AgentSwarmNodeLoadRequest { channel, agent_id })
            .await
            .context("failed to load agent swarm node")
    }

    pub async fn cleanup_agent_swarm_audit(
        &self,
        channel: String,
        before_unix: i64,
    ) -> anyhow::Result<i64> {
        self.memory
            .cleanup_agent_swarm_audit(AgentSwarmCleanupRequest {
                channel,
                before_unix,
            })
            .await
            .context("failed to cleanup agent swarm audit")
    }

    pub(super) async fn try_swarm_reply(
        &self,
        incoming: &IncomingMessage,
        history: &[StoredMessage],
    ) -> anyhow::Result<Option<String>> {
        if !self.agent_swarm.enabled {
            return Ok(None);
        }
        // Group chats stay on the normal reply path by default — a swarm run
        // is multiple provider calls, so group activation is explicit opt-in.
        if incoming.is_group_chat && !self.agent_swarm.group_enabled {
            return Ok(None);
        }

        let should_swarm = if self.agent_swarm.auto_detect {
            self.detect_swarm_activation(&incoming.text, history).await
        } else {
            true
        };
        if !should_swarm {
            return Ok(None);
        }

        let run_id = self.next_swarm_run_id();
        let started_at_unix = current_unix_timestamp_i64();
        self.memory
            .create_agent_swarm_run(CreateAgentSwarmRunRequest {
                run_id: run_id.clone(),
                channel: incoming.channel.clone(),
                session_id: incoming.session_id.clone(),
                user_id: incoming.user_id.clone(),
                root_task: incoming.text.clone(),
                status: AgentSwarmRunStatus::Running,
                started_at_unix,
            })
            .await
            .context("failed to create swarm run audit")?;

        let shared = SwarmExecutionShared {
            run_id: run_id.clone(),
            channel: incoming.channel.clone(),
            settings: self.agent_swarm.clone(),
            semaphore: Arc::new(Semaphore::new(self.agent_swarm.max_parallel.max(1))),
            loop_projection: self
                .start_swarm_loop_projection(&run_id, &incoming.text, history, incoming)
                .await,
            agent_counter: Arc::new(AtomicUsize::new(0)),
            success_count: Arc::new(AtomicUsize::new(0)),
            partial_count: Arc::new(AtomicUsize::new(0)),
            failed_count: Arc::new(AtomicUsize::new(0)),
            timeout_count: Arc::new(AtomicUsize::new(0)),
            skipped_count: Arc::new(AtomicUsize::new(0)),
        };

        let root_spec = SwarmAgentSpec {
            task: incoming.text.clone(),
            role_name: "orchestrator".to_string(),
            role_definition: "负责分解子任务、并行调度并汇总子结果。".to_string(),
            nickname: Some("Orchestrator".to_string()),
        };

        let max_run_timeout = Duration::from_millis(self.agent_swarm.max_run_timeout_ms.max(1));
        let run_result = if self.agent_swarm.durable_engine
            && let Some(projection) = shared.loop_projection.as_ref()
        {
            // Durable execution: the projected goal IS the run — nodes claim
            // work items, delegation extends the DAG, aggregates merge
            // upstream outcomes. Same synchronous reply semantics.
            let goal_id = projection.goal_id.clone();
            let durable = timeout(
                max_run_timeout,
                self.run_swarm_durable(&run_id, incoming, &goal_id, &shared),
            )
            .await;
            match durable {
                Ok((text, status, error)) => Ok((text, status, error)),
                Err(_) => Ok((
                    None,
                    AgentSwarmRunStatus::Failed,
                    Some("swarm run timeout".to_string()),
                )),
            }
        } else {
            match timeout(
                max_run_timeout,
                self.execute_swarm_node(history.to_vec(), root_spec, 0, None, shared.clone()),
            )
            .await
            {
                Ok(Ok(root)) => {
                    let has_failures = shared.failed_count.load(Ordering::Relaxed) > 0
                        || shared.timeout_count.load(Ordering::Relaxed) > 0;
                    let has_partial = shared.partial_count.load(Ordering::Relaxed) > 0
                        || shared.skipped_count.load(Ordering::Relaxed) > 0
                        || root.partial;
                    let status = if root.answer.trim().is_empty() || root.failed {
                        AgentSwarmRunStatus::Failed
                    } else if has_failures || has_partial {
                        AgentSwarmRunStatus::Partial
                    } else {
                        AgentSwarmRunStatus::Completed
                    };
                    Ok((Some(root.answer), status, None))
                }
                Ok(Err(err)) => Ok((None, AgentSwarmRunStatus::Failed, Some(err.to_string()))),
                Err(_) => Err(()),
            }
        };

        let finished_at_unix = current_unix_timestamp_i64();
        let (reply_text, run_status, run_error) = match run_result {
            Ok((text, status, error)) => (text, status, error),
            Err(_) => (
                None,
                AgentSwarmRunStatus::Failed,
                Some("swarm run timeout".to_string()),
            ),
        };

        // A still-running row means the reply was deferred — the janitor
        // finishes the audit record after delivering the answer.
        if run_status != AgentSwarmRunStatus::Running
            && let Err(err) = self
                .memory
                .finish_agent_swarm_run(FinishAgentSwarmRunRequest {
                    channel: incoming.channel.clone(),
                    run_id: run_id.clone(),
                    status: run_status,
                    finished_at_unix,
                    error: run_error.clone(),
                })
                .await
        {
            warn!(error = %err, run_id = %run_id, "failed to finalize swarm run audit");
        }

        self.finish_swarm_loop_projection(&shared, reply_text.as_deref(), run_error.as_deref())
            .await;

        if self.agent_swarm.audit_retention_days > 0 {
            let keep_secs = (self.agent_swarm.audit_retention_days as i64)
                .saturating_mul(86_400)
                .max(0);
            let before_unix = finished_at_unix.saturating_sub(keep_secs);
            if let Err(err) = self
                .cleanup_agent_swarm_audit(incoming.channel.clone(), before_unix)
                .await
            {
                warn!(error = %err, run_id = %run_id, "failed to cleanup swarm audit");
            }
        }

        let Some(mut reply_text) = reply_text else {
            return Ok(None);
        };

        if self.agent_swarm.reply_summary_enabled {
            reply_text.push_str(&self.build_swarm_summary_suffix(&run_id, &shared));
        }
        Ok(Some(reply_text))
    }

    async fn detect_swarm_activation(&self, user_text: &str, history: &[StoredMessage]) -> bool {
        let base_decision = looks_like_swarm_task(user_text);
        let sampled_history = history
            .iter()
            .rev()
            .take(8)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>();
        let history_json =
            serde_json::to_string(&sampled_history).unwrap_or_else(|_| "[]".to_string());
        let prompt = [
            "You are a task routing classifier.",
            "Return ONLY JSON: {\"use_swarm\":true|false}.",
            "Set use_swarm=true when task is broad, multi-domain, parallelizable, or asks for role separation.",
            "Set use_swarm=false for simple Q&A or single-step tasks.",
        ]
        .join("\n");

        let decision_call = self.provider.complete(CompletionRequest::json(vec![
            StoredMessage {
                role: MessageRole::System,
                content: prompt,
            },
            StoredMessage {
                role: MessageRole::User,
                content: format!(
                    "USER_TASK:\n{}\n\nRECENT_CONTEXT_JSON:\n{}",
                    user_text.trim(),
                    history_json
                ),
            },
        ]));
        let call_result = timeout(
            Duration::from_millis(self.agent_swarm.max_node_timeout_ms.max(1)),
            decision_call,
        )
        .await;

        match call_result {
            Ok(Ok(reply)) => {
                if let Some(value) = parse_flexible_json_value(&reply)
                    && let Ok(parsed) = serde_json::from_value::<SwarmActivationDecision>(value)
                {
                    return parsed.use_swarm;
                }
                base_decision
            }
            Ok(Err(err)) => {
                warn!(error = %err, "swarm activation classifier failed, fallback to heuristic");
                base_decision
            }
            Err(_) => {
                warn!("swarm activation classifier timed out, fallback to heuristic");
                base_decision
            }
        }
    }

    /// Executes one swarm node and mirrors its lifecycle into Loop Engine when
    /// a projection is active: the node is appended (non-root) and claimed as
    /// a work item, then its attempt is finished with a committed checkpoint
    /// or failed non-retryably. Projection failures never change the outcome.
    async fn execute_swarm_node(
        &self,
        history: Vec<StoredMessage>,
        spec: SwarmAgentSpec,
        depth: usize,
        parent_agent_id: Option<String>,
        shared: SwarmExecutionShared,
    ) -> anyhow::Result<SwarmNodeOutcome> {
        let idx = shared.next_agent_index();
        let node = SwarmNodeIdentity {
            agent_id: format!("{}:{idx}", shared.run_id),
            index: idx,
        };
        let claim = self
            .claim_swarm_loop_node(
                &shared,
                &spec,
                depth,
                parent_agent_id.as_deref(),
                &node.agent_id,
            )
            .await;
        let result = self
            .execute_swarm_node_inner(history, spec, depth, parent_agent_id, shared.clone(), node)
            .await;
        if let Some(claim) = claim {
            match &result {
                Ok(outcome) => self.commit_swarm_loop_node(&shared, &claim, outcome).await,
                Err(err) => self.fail_swarm_loop_node(&claim, &err.to_string()).await,
            }
        }
        result
    }

    async fn execute_swarm_node_inner(
        &self,
        history: Vec<StoredMessage>,
        spec: SwarmAgentSpec,
        depth: usize,
        parent_agent_id: Option<String>,
        shared: SwarmExecutionShared,
        node: SwarmNodeIdentity,
    ) -> anyhow::Result<SwarmNodeOutcome> {
        let agent_id = node.agent_id;
        let idx = node.index;
        let nickname = spec
            .nickname
            .clone()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| default_swarm_nickname(&spec.role_name, idx));
        let started_at_unix = current_unix_timestamp_i64();

        if idx > shared.settings.max_agents.max(1) {
            let _ = self
                .memory
                .upsert_agent_swarm_node(UpsertAgentSwarmNodeRequest {
                    agent_id: agent_id.clone(),
                    run_id: shared.run_id.clone(),
                    channel: shared.channel.clone(),
                    parent_agent_id,
                    depth,
                    nickname: nickname.clone(),
                    role_name: spec.role_name.clone(),
                    role_definition: spec.role_definition.clone(),
                    task: spec.task.clone(),
                    state: AgentSwarmNodeState::Exited,
                    exit_status: Some(AgentSwarmNodeExitStatus::Skipped),
                    summary: Some("agent budget exceeded".to_string()),
                    error: None,
                    started_at_unix,
                    finished_at_unix: Some(started_at_unix),
                })
                .await;
            shared.skipped_count.fetch_add(1, Ordering::Relaxed);
            return Ok(SwarmNodeOutcome {
                agent_id,
                nickname,
                role_name: spec.role_name,
                answer: String::new(),
                status: AgentSwarmNodeExitStatus::Skipped,
                partial: true,
                failed: false,
            });
        }

        let _ = self
            .memory
            .upsert_agent_swarm_node(UpsertAgentSwarmNodeRequest {
                agent_id: agent_id.clone(),
                run_id: shared.run_id.clone(),
                channel: shared.channel.clone(),
                parent_agent_id: parent_agent_id.clone(),
                depth,
                nickname: nickname.clone(),
                role_name: spec.role_name.clone(),
                role_definition: spec.role_definition.clone(),
                task: spec.task.clone(),
                state: AgentSwarmNodeState::Running,
                exit_status: None,
                summary: None,
                error: None,
                started_at_unix,
                finished_at_unix: None,
            })
            .await;

        let force_answer = depth >= shared.settings.max_depth.max(1);
        let mut direct_answer = None::<String>;
        let mut child_specs = Vec::<SwarmChildPlan>::new();

        if force_answer {
            direct_answer = Some(String::new());
        } else {
            let planner_messages = build_swarm_planner_messages(
                &history,
                &spec.task,
                &spec.role_name,
                &spec.role_definition,
                depth,
                shared.settings.max_depth,
            );
            match self
                .swarm_complete_with_timeout(planner_messages, &shared, true)
                .await
            {
                Ok(reply) => {
                    if let Some(value) = parse_flexible_json_value(&reply)
                        && let Ok(plan) = serde_json::from_value::<SwarmNodePlanEnvelope>(value)
                    {
                        let mode = plan
                            .mode
                            .unwrap_or_else(|| "answer".to_string())
                            .to_ascii_lowercase();
                        if mode == "delegate" {
                            child_specs = plan.children.unwrap_or_default();
                        } else {
                            direct_answer = Some(plan.answer.unwrap_or_default());
                        }
                    } else {
                        direct_answer = Some(String::new());
                    }
                }
                Err(SwarmModelError::Timeout) => {
                    let finished = current_unix_timestamp_i64();
                    let _ = self
                        .memory
                        .upsert_agent_swarm_node(UpsertAgentSwarmNodeRequest {
                            agent_id: agent_id.clone(),
                            run_id: shared.run_id.clone(),
                            channel: shared.channel.clone(),
                            parent_agent_id,
                            depth,
                            nickname: nickname.clone(),
                            role_name: spec.role_name.clone(),
                            role_definition: spec.role_definition.clone(),
                            task: spec.task.clone(),
                            state: AgentSwarmNodeState::Exited,
                            exit_status: Some(AgentSwarmNodeExitStatus::Timeout),
                            summary: Some("planner timeout".to_string()),
                            error: Some("planner timeout".to_string()),
                            started_at_unix,
                            finished_at_unix: Some(finished),
                        })
                        .await;
                    shared.timeout_count.fetch_add(1, Ordering::Relaxed);
                    shared.failed_count.fetch_add(1, Ordering::Relaxed);
                    return Ok(SwarmNodeOutcome {
                        agent_id,
                        nickname,
                        role_name: spec.role_name,
                        answer: String::new(),
                        status: AgentSwarmNodeExitStatus::Timeout,
                        partial: false,
                        failed: true,
                    });
                }
                Err(SwarmModelError::Failure(err)) => {
                    warn!(error = %err, "swarm planner failed, fallback to direct answer");
                    direct_answer = Some(String::new());
                }
            }
        }

        if child_specs.is_empty() {
            let answer_messages = build_swarm_answer_messages(
                &history,
                &spec.task,
                &spec.role_name,
                &spec.role_definition,
            );
            let answer = if let Some(seed) = direct_answer
                && !seed.trim().is_empty()
            {
                seed
            } else {
                match self
                    .swarm_complete_with_timeout(answer_messages, &shared, false)
                    .await
                {
                    Ok(text) => text,
                    Err(SwarmModelError::Timeout) => {
                        let finished = current_unix_timestamp_i64();
                        let _ = self
                            .memory
                            .upsert_agent_swarm_node(UpsertAgentSwarmNodeRequest {
                                agent_id: agent_id.clone(),
                                run_id: shared.run_id.clone(),
                                channel: shared.channel.clone(),
                                parent_agent_id,
                                depth,
                                nickname: nickname.clone(),
                                role_name: spec.role_name.clone(),
                                role_definition: spec.role_definition.clone(),
                                task: spec.task.clone(),
                                state: AgentSwarmNodeState::Exited,
                                exit_status: Some(AgentSwarmNodeExitStatus::Timeout),
                                summary: Some("answer timeout".to_string()),
                                error: Some("answer timeout".to_string()),
                                started_at_unix,
                                finished_at_unix: Some(finished),
                            })
                            .await;
                        shared.timeout_count.fetch_add(1, Ordering::Relaxed);
                        shared.failed_count.fetch_add(1, Ordering::Relaxed);
                        return Ok(SwarmNodeOutcome {
                            agent_id,
                            nickname,
                            role_name: spec.role_name,
                            answer: String::new(),
                            status: AgentSwarmNodeExitStatus::Timeout,
                            partial: false,
                            failed: true,
                        });
                    }
                    Err(SwarmModelError::Failure(err)) => {
                        let finished = current_unix_timestamp_i64();
                        let err_text = err.to_string();
                        let _ = self
                            .memory
                            .upsert_agent_swarm_node(UpsertAgentSwarmNodeRequest {
                                agent_id: agent_id.clone(),
                                run_id: shared.run_id.clone(),
                                channel: shared.channel.clone(),
                                parent_agent_id,
                                depth,
                                nickname: nickname.clone(),
                                role_name: spec.role_name.clone(),
                                role_definition: spec.role_definition.clone(),
                                task: spec.task.clone(),
                                state: AgentSwarmNodeState::Exited,
                                exit_status: Some(AgentSwarmNodeExitStatus::Failed),
                                summary: Some("answer failed".to_string()),
                                error: Some(err_text),
                                started_at_unix,
                                finished_at_unix: Some(finished),
                            })
                            .await;
                        shared.failed_count.fetch_add(1, Ordering::Relaxed);
                        return Ok(SwarmNodeOutcome {
                            agent_id,
                            nickname,
                            role_name: spec.role_name,
                            answer: String::new(),
                            status: AgentSwarmNodeExitStatus::Failed,
                            partial: false,
                            failed: true,
                        });
                    }
                }
            };

            let finished = current_unix_timestamp_i64();
            let _ = self
                .memory
                .upsert_agent_swarm_node(UpsertAgentSwarmNodeRequest {
                    agent_id: agent_id.clone(),
                    run_id: shared.run_id.clone(),
                    channel: shared.channel.clone(),
                    parent_agent_id,
                    depth,
                    nickname: nickname.clone(),
                    role_name: spec.role_name.clone(),
                    role_definition: spec.role_definition.clone(),
                    task: spec.task.clone(),
                    state: AgentSwarmNodeState::Exited,
                    exit_status: Some(AgentSwarmNodeExitStatus::Success),
                    summary: Some(truncate_swarm_text(&answer, 240)),
                    error: None,
                    started_at_unix,
                    finished_at_unix: Some(finished),
                })
                .await;
            shared.success_count.fetch_add(1, Ordering::Relaxed);
            return Ok(SwarmNodeOutcome {
                agent_id,
                nickname,
                role_name: spec.role_name,
                answer,
                status: AgentSwarmNodeExitStatus::Success,
                partial: false,
                failed: false,
            });
        }

        let mut child_futures = Vec::new();
        for child in child_specs {
            if child.task.trim().is_empty() {
                continue;
            }
            let child_spec = SwarmAgentSpec {
                task: child.task,
                role_name: child
                    .role_name
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or_else(|| "general".to_string()),
                role_definition: child
                    .role_definition
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or_else(|| "负责当前子任务并输出可交付结果。".to_string()),
                nickname: child.nickname,
            };
            child_futures.push(Box::pin(self.execute_swarm_node(
                history.clone(),
                child_spec,
                depth + 1,
                Some(agent_id.clone()),
                shared.clone(),
            )));
        }

        // Execute child futures with semaphore-limited concurrency
        let mut child_outcomes = Vec::new();
        let semaphore = shared.semaphore.clone();
        for future in child_futures {
            // Acquire permit before executing (limits concurrent children to max_parallel)
            let permit = match semaphore.clone().acquire_owned().await {
                Ok(p) => p,
                Err(_) => {
                    // Semaphore closed, skip this child
                    continue;
                }
            };
            let result = future.await;
            // Permit is dropped here, releasing the slot for next child
            drop(permit);
            match result {
                Ok(outcome) => child_outcomes.push(outcome),
                Err(err) => warn!(error = %err, "swarm child execution failed"),
            }
        }

        let mut any_failed = false;
        let mut any_partial = false;
        let mut child_json = Vec::new();
        for outcome in &child_outcomes {
            any_failed |= outcome.failed;
            any_partial |= outcome.partial
                || matches!(
                    outcome.status,
                    AgentSwarmNodeExitStatus::Partial | AgentSwarmNodeExitStatus::Skipped
                );
            child_json.push(serde_json::json!({
                "agent_id": outcome.agent_id,
                "nickname": outcome.nickname,
                "role_name": outcome.role_name,
                "status": outcome.status.as_str(),
                "answer": truncate_swarm_text(&outcome.answer, 180),
            }));
        }

        let merged_messages =
            build_swarm_merge_messages(&history, &spec.task, &spec.role_name, &child_json);
        let merged_answer = match self
            .swarm_complete_with_timeout(merged_messages, &shared, false)
            .await
        {
            Ok(text) => text,
            Err(_) => child_outcomes
                .iter()
                .filter(|o| !o.answer.trim().is_empty())
                .map(|o| format!("[{}] {}", o.nickname, o.answer.trim()))
                .collect::<Vec<_>>()
                .join("\n\n"),
        };

        let status = if any_failed {
            AgentSwarmNodeExitStatus::Partial
        } else {
            AgentSwarmNodeExitStatus::Success
        };
        let finished = current_unix_timestamp_i64();
        let _ = self
            .memory
            .upsert_agent_swarm_node(UpsertAgentSwarmNodeRequest {
                agent_id: agent_id.clone(),
                run_id: shared.run_id.clone(),
                channel: shared.channel.clone(),
                parent_agent_id,
                depth,
                nickname: nickname.clone(),
                role_name: spec.role_name.clone(),
                role_definition: spec.role_definition.clone(),
                task: spec.task.clone(),
                state: AgentSwarmNodeState::Exited,
                exit_status: Some(status),
                summary: Some(truncate_swarm_text(&merged_answer, 240)),
                error: None,
                started_at_unix,
                finished_at_unix: Some(finished),
            })
            .await;

        if matches!(status, AgentSwarmNodeExitStatus::Success) && !any_partial {
            shared.success_count.fetch_add(1, Ordering::Relaxed);
        } else {
            shared.partial_count.fetch_add(1, Ordering::Relaxed);
        }

        Ok(SwarmNodeOutcome {
            agent_id,
            nickname,
            role_name: spec.role_name,
            answer: merged_answer,
            status,
            partial: any_failed || any_partial,
            failed: false,
        })
    }

    async fn swarm_complete_with_timeout(
        &self,
        messages: Vec<StoredMessage>,
        shared: &SwarmExecutionShared,
        json_mode: bool,
    ) -> Result<String, SwarmModelError> {
        let _permit = shared
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|err| SwarmModelError::Failure(anyhow::anyhow!(err.to_string())))?;
        let request = if json_mode {
            CompletionRequest::json(messages)
        } else {
            CompletionRequest::from_messages(messages)
        };
        let call = self.provider.complete(request);
        match timeout(
            Duration::from_millis(shared.settings.max_node_timeout_ms.max(1)),
            call,
        )
        .await
        {
            Ok(Ok(text)) => Ok(text),
            Ok(Err(err)) => Err(SwarmModelError::Failure(err)),
            Err(_) => Err(SwarmModelError::Timeout),
        }
    }

    /// Creates the durable Goal for this swarm run: a fixed internal plan with
    /// a single `swarm_root` step declaring the expansion budget, immediately
    /// approved. The approval binds a code-constructed plan (not model
    /// output), which is the T6 temporary internal path; operator approval
    /// flows stay untouched.
    async fn start_swarm_loop_projection(
        &self,
        run_id: &str,
        root_task: &str,
        history: &[StoredMessage],
        incoming: &IncomingMessage,
    ) -> Option<SwarmLoopProjection> {
        let engine = self.loop_engine.clone()?;
        match self
            .create_swarm_goal(&engine, run_id, root_task, history, incoming)
            .await
        {
            Ok(goal_id) => Some(SwarmLoopProjection {
                engine,
                goal_id,
                worker_id: format!("swarm:{run_id}"),
            }),
            Err(err) => {
                warn!(error = %err, run_id = %run_id, "swarm loop projection disabled: goal setup failed");
                None
            }
        }
    }

    async fn create_swarm_goal(
        &self,
        engine: &LoopEngine,
        run_id: &str,
        root_task: &str,
        history: &[StoredMessage],
        incoming: &IncomingMessage,
    ) -> anyhow::Result<String> {
        let goal = engine
            .create_goal(
                CreateGoalRequest {
                    objective: format!(
                        "agent swarm run {run_id}: {}",
                        truncate_swarm_text(root_task, 480)
                    ),
                    source_signal_ids: Vec::new(),
                    // Swarm retries of the same run admit the same goal
                    // instead of forking a second projection.
                    request_id: Some(format!("swarm:{run_id}")),
                    // Mirror-only goals stay out of the dispatch sweep —
                    // their items are claimed in-process. The durable path
                    // keeps sweep visibility so orphaned runs get claimed.
                    internal_only: !self.agent_swarm.durable_engine,
                },
                SWARM_LOOP_ACTOR,
            )
            .await?;
        let max_nodes = self.agent_swarm.max_agents.max(1) as u32;
        let planned = engine
            .plan_goal(
                &goal.id,
                PlanGoalRequest {
                    workflow: WorkflowSpec {
                        steps: vec![WorkflowStep {
                            // The root node's agent_id is `{run_id}:1` because
                            // `next_agent_index` returns 1 for the first node.
                            id: format!("{run_id}:1"),
                            handler: SWARM_NODE_DURABLE_HANDLER.to_string(),
                            effect: EffectClass::LocalWrite,
                            input: serde_json::json!({
                                "swarm_root": true,
                                "max_nodes": max_nodes,
                                "swarm_run_id": run_id,
                                "task": truncate_swarm_text(root_task, 1024),
                                "role_name": "orchestrator",
                                "role_definition": "负责分解子任务、并行调度并汇总子结果。",
                                "nickname": "Orchestrator",
                                "depth": 0,
                                "history": bounded_swarm_input_history(history),
                                "channel": incoming.channel,
                                "user_id": incoming.user_id,
                                "session_id": incoming.session_id,
                            }),
                            retry: RetryPolicy {
                                max_attempts: 1,
                                backoff_secs: 0,
                            },
                        }],
                        edges: Vec::new(),
                        budget: ExecutionBudget {
                            max_provider_calls: (2 * max_nodes).min(64),
                            deadline_secs: ((self.agent_swarm.max_run_timeout_ms / 1000) as u32)
                                .saturating_add(60)
                                .clamp(1, 86_400),
                            max_response_bytes: 1_048_576,
                        },
                    },
                    acceptance_criteria: vec![AcceptanceCriterion::ArtifactExists {
                        artifact_type: "analysis_report".to_string(),
                    }],
                },
                SWARM_LOOP_ACTOR,
            )
            .await?;
        engine
            .approve_goal(
                &goal.id,
                ApproveGoalRequest {
                    expected_goal_revision: planned.goal.revision,
                    expected_plan_hash: planned.plan_hash,
                },
                SWARM_LOOP_ACTOR,
            )
            .await?;
        Ok(goal.id)
    }

    /// Appends the node as a work item (non-root only; the root step already
    /// exists in the approved plan) and claims it under this run's worker id.
    /// Returns `None` when projection is disabled or loses the claim race.
    async fn claim_swarm_loop_node(
        &self,
        shared: &SwarmExecutionShared,
        spec: &SwarmAgentSpec,
        depth: usize,
        parent_agent_id: Option<&str>,
        agent_id: &str,
    ) -> Option<WorkClaim> {
        let projection = shared.loop_projection.as_ref()?;
        if parent_agent_id.is_some() {
            let step = WorkflowStep {
                id: agent_id.to_string(),
                handler: SWARM_NODE_HANDLER.to_string(),
                effect: EffectClass::LocalWrite,
                input: serde_json::json!({
                    "swarm_run_id": shared.run_id,
                    "parent_agent_id": parent_agent_id,
                    "depth": depth,
                    "task": truncate_swarm_text(&spec.task, 1024),
                    "role_name": spec.role_name,
                }),
                retry: RetryPolicy {
                    max_attempts: 1,
                    backoff_secs: 0,
                },
            };
            if let Err(err) = projection
                .engine
                .extend_workflow(&projection.goal_id, vec![step], SWARM_LOOP_ACTOR)
                .await
            {
                warn!(error = %err, agent_id = %agent_id, "swarm node projection skipped: extension rejected");
                return None;
            }
        }
        let lease_secs = ((shared.settings.max_node_timeout_ms / 1000) as u32)
            .saturating_add(30)
            .clamp(1, 3600);
        let claimed = match projection
            .engine
            .claim_work_item(
                &projection.goal_id,
                agent_id,
                &projection.worker_id,
                lease_secs,
                SWARM_LOOP_ACTOR,
            )
            .await
        {
            Ok(claim) => claim,
            Err(err) => {
                warn!(error = %err, agent_id = %agent_id, "swarm node projection claim failed");
                return None;
            }
        };
        if claimed.is_none() {
            // A generic LoopWorker may have won the ready item first; the
            // in-process swarm still runs the node itself.
            warn!(agent_id = %agent_id, "swarm node projection claim lost to another worker");
        }
        claimed
    }

    /// Commits the node outcome. The root node additionally publishes the
    /// `analysis_report` artifact first so its committed outcome references it
    /// and the `ArtifactExists` acceptance criterion can pass.
    async fn commit_swarm_loop_node(
        &self,
        shared: &SwarmExecutionShared,
        claim: &WorkClaim,
        outcome: &SwarmNodeOutcome,
    ) {
        let Some(engine) = self.loop_engine.clone() else {
            return;
        };
        if outcome.failed {
            self.fail_swarm_loop_node(claim, outcome.status.as_str())
                .await;
            return;
        }
        let result = async {
            let is_root = claim
                .work_item
                .input
                .get("swarm_root")
                .is_some_and(|value| value.as_bool() == Some(true));
            let mut artifact_ids = Vec::new();
            if is_root && !outcome.answer.trim().is_empty() {
                let published = engine
                    .publish_artifact(
                        PublishArtifactRequest {
                            kind: ArtifactKind::AnalysisReport,
                            name: format!("swarm-{}", shared.run_id),
                            version: "1".to_string(),
                            content: serde_json::json!({
                                "swarm_run_id": shared.run_id,
                                "channel": shared.channel,
                                "answer": truncate_swarm_text(&outcome.answer, 4096),
                            }),
                            source_goal_id: Some(claim.work_item.goal_id.clone()),
                            parent_artifact_id: None,
                        },
                        SWARM_LOOP_ACTOR,
                    )
                    .await?;
                artifact_ids.push(published.artifact.id);
            }
            let idempotency_key = format!("{}:{}:v1", claim.work_item.id, claim.attempt.id);
            // The real node already executed in-process; this projection is
            // a durable record only, so a crash mid-projection is always
            // safe to re-run — never park for operator confirmation.
            let checkpoint = engine
                .prepare_checkpoint(claim, &idempotency_key, true, SWARM_LOOP_ACTOR)
                .await?;
            engine
                .commit_checkpoint(
                    claim,
                    &checkpoint.id,
                    WorkOutcome {
                        summary: truncate_swarm_text(&outcome.answer, 1024),
                        artifact_ids,
                        evidence: serde_json::json!({
                            "agent_id": outcome.agent_id,
                            "nickname": outcome.nickname,
                            "role_name": outcome.role_name,
                            "status": outcome.status.as_str(),
                            "partial": outcome.partial,
                        }),
                        sleep_until_unix: None,
                        continuation: None,
                    },
                    SWARM_LOOP_ACTOR,
                )
                .await?;
            engine
                .finish_attempt(claim, &checkpoint.id, SWARM_LOOP_ACTOR)
                .await?;
            anyhow::Ok(())
        }
        .await;
        if let Err(err) = result {
            warn!(error = %err, agent_id = %outcome.agent_id, "swarm node projection commit failed");
        }
    }

    async fn fail_swarm_loop_node(&self, claim: &WorkClaim, error: &str) {
        let Some(engine) = self.loop_engine.clone() else {
            return;
        };
        if let Err(err) = engine
            .fail_attempt(claim, error, false, SWARM_LOOP_ACTOR)
            .await
        {
            warn!(error = %err, "swarm node projection fail_attempt failed");
        }
    }

    /// Runs goal verification so `/resume` and goal detail show a closed loop.
    /// The run-level result artifact was already published during the root
    /// node's commit (acceptance criteria read committed outcomes).
    async fn finish_swarm_loop_projection(
        &self,
        shared: &SwarmExecutionShared,
        reply_text: Option<&str>,
        run_error: Option<&str>,
    ) {
        let Some(projection) = shared.loop_projection.as_ref() else {
            return;
        };
        if reply_text.is_none_or(|text| text.trim().is_empty())
            && let Some(error) = run_error
        {
            warn!(run_id = %shared.run_id, error = %error, "swarm run failed; projected goal left unverified");
        }
        if let Err(err) = projection
            .engine
            .verify_goal(&projection.goal_id, SWARM_LOOP_ACTOR)
            .await
        {
            warn!(error = %err, run_id = %shared.run_id, "swarm goal verification failed");
        }
    }

    fn build_swarm_summary_suffix(&self, run_id: &str, shared: &SwarmExecutionShared) -> String {
        format!(
            "\n\n[Agents Summary]\nrun_id: {}\nagents_total: {}\nsuccess: {}\npartial: {}\nfailed: {}\ntimeout: {}\nskipped: {}",
            run_id,
            shared.agent_counter.load(Ordering::Relaxed),
            shared.success_count.load(Ordering::Relaxed),
            shared.partial_count.load(Ordering::Relaxed),
            shared.failed_count.load(Ordering::Relaxed),
            shared.timeout_count.load(Ordering::Relaxed),
            shared.skipped_count.load(Ordering::Relaxed),
        )
    }

    fn next_swarm_run_id(&self) -> String {
        let tick = self.swarm_run_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|v| v.as_millis())
            .unwrap_or(0);
        format!("swarm-{}-{tick}", now_ms)
    }
}

fn parse_flexible_json_value(reply: &str) -> Option<Value> {
    let json_text = extract_json_payload(reply.trim())?;
    serde_json::from_str(&json_text).ok()
}

fn sample_swarm_history(history: &[StoredMessage], max_items: usize) -> Vec<StoredMessage> {
    history
        .iter()
        .rev()
        .take(max_items.max(1))
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
}

fn build_swarm_planner_messages(
    history: &[StoredMessage],
    task: &str,
    role_name: &str,
    role_definition: &str,
    depth: usize,
    max_depth: usize,
) -> Vec<StoredMessage> {
    let mut messages = sample_swarm_history(history, 12);
    messages.push(StoredMessage {
        role: MessageRole::System,
        content: [
            "You are a hierarchical task planner.",
            "Return ONLY JSON.",
            "Schema: {\"mode\":\"answer|delegate\",\"answer\":\"string\",\"children\":[{\"task\":\"string\",\"role_name\":\"string\",\"role_definition\":\"string\",\"nickname\":\"string|optional\"}]}",
            "Rules:",
            "- If task is simple, choose mode=answer and provide answer.",
            "- If task can be split and depth allows, choose mode=delegate with 1-4 children.",
            "- Do not output markdown.",
        ]
        .join("\n"),
    });
    messages.push(StoredMessage {
        role: MessageRole::User,
        content: format!(
            "CURRENT_DEPTH: {depth}\nMAX_DEPTH: {max_depth}\nROLE: {role_name}\nROLE_DEFINITION: {role_definition}\nTASK:\n{}",
            task.trim()
        ),
    });
    messages
}

fn build_swarm_answer_messages(
    history: &[StoredMessage],
    task: &str,
    role_name: &str,
    role_definition: &str,
) -> Vec<StoredMessage> {
    let mut messages = sample_swarm_history(history, 12);
    messages.push(StoredMessage {
        role: MessageRole::System,
        content: format!(
            "You are the '{role_name}' agent.\nRole definition: {role_definition}\nReturn the direct deliverable for your task."
        ),
    });
    messages.push(StoredMessage {
        role: MessageRole::User,
        content: task.trim().to_string(),
    });
    messages
}

fn build_swarm_merge_messages(
    history: &[StoredMessage],
    task: &str,
    role_name: &str,
    child_json: &[Value],
) -> Vec<StoredMessage> {
    let mut messages = sample_swarm_history(history, 12);
    let serialized = serde_json::to_string(child_json).unwrap_or_else(|_| "[]".to_string());
    messages.push(StoredMessage {
        role: MessageRole::System,
        content: format!(
            "You are the '{role_name}' parent agent.\nMerge child results into one coherent final response for the task."
        ),
    });
    messages.push(StoredMessage {
        role: MessageRole::User,
        content: format!(
            "PARENT_TASK:\n{}\n\nCHILD_RESULTS_JSON:\n{}",
            task.trim(),
            serialized
        ),
    });
    messages
}

fn default_swarm_nickname(role_name: &str, idx: usize) -> String {
    let prefix = match role_name.trim().to_ascii_lowercase().as_str() {
        "frontend" => "FE",
        "backend" => "BE",
        "test" | "qa" => "QA",
        "research" => "RS",
        "orchestrator" => "ORCH",
        _ => "AG",
    };
    format!("{prefix}-{idx}")
}

fn truncate_swarm_text(input: &str, max_chars: usize) -> String {
    let trimmed = input.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let mut out = trimmed.chars().take(max_chars.max(1)).collect::<String>();
    out.push_str("...(truncated)");
    out
}

fn looks_like_swarm_task(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    let hints = [
        "并行",
        "拆分",
        "多代理",
        "子代理",
        "swarm",
        "subagent",
        "frontend",
        "backend",
        "前端",
        "后端",
        "分工",
        "协作",
        "pipeline",
        "plan and execute",
        "multi-step",
        "多个步骤",
    ];
    hints.iter().any(|needle| lowered.contains(needle))
}

/* ------------------------------------------------------------------------
 * Durable swarm execution (`swarm_node` handler)
 *
 * Each node's decide -> answer|delegate cycle is one work item. Delegation
 * extends the goal's DAG: child items plus an aggregate item that depends
 * on them; the aggregate merges children through `ctx.upstream`. Model
 * errors never hard-fail an item — they land in outcome evidence as
 * `status:"failed"|"timeout"` so the aggregate still merges partial
 * results (a hard-failed dependency would stall the pending aggregate and
 * the whole goal). A crash mid-run leaves a resumable DAG — a new attempt
 * resumes from committed outcomes, not from scratch.
 * ------------------------------------------------------------------------ */

/// Loop Engine handler that executes one swarm node durably.
const SWARM_NODE_DURABLE_HANDLER: &str = "swarm_node";

/// Work-item input for the durable swarm path.
#[derive(Debug, Deserialize)]
struct SwarmNodeInput {
    task: String,
    #[serde(default)]
    role_name: Option<String>,
    #[serde(default)]
    role_definition: Option<String>,
    #[serde(default)]
    nickname: Option<String>,
    #[serde(default)]
    depth: usize,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    parent_step_id: Option<String>,
    #[serde(default)]
    swarm_run_id: Option<String>,
    #[serde(default)]
    history: Vec<StoredMessage>,
    /// Root step marker — the run's terminal node publishes the
    /// `analysis_report` artifact the goal's acceptance criterion checks.
    #[serde(default)]
    swarm_root: bool,
    /// Set on the aggregate spawned by a `swarm_root` delegation — the
    /// merged answer is the run's terminal deliverable.
    #[serde(default)]
    swarm_terminal: bool,
    /// Provenance carried from the triggering message so node trajectories
    /// land under the real channel/user.
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
}

/// Per-attempt trajectory recorder: one trajectory per work-item attempt
/// (`swarm-node:{work}:{attempt}`), one frame per provider call. All writes
/// are best-effort — recording never blocks a node.
struct SwarmNodeTrajectory<'a> {
    logger: Option<&'a crate::harness::trajectory::TrajectoryLogger>,
    trajectory_id: String,
    session_id: String,
    channel: String,
    user_id: String,
    call_index: u32,
}

impl<'a> SwarmNodeTrajectory<'a> {
    fn new(
        logger: Option<&'a crate::harness::trajectory::TrajectoryLogger>,
        claim: &WorkClaim,
        input: &SwarmNodeInput,
    ) -> Self {
        Self {
            logger,
            trajectory_id: format!("swarm-node:{}:{}", claim.work_item.id, claim.attempt.id),
            session_id: input
                .session_id
                .clone()
                .unwrap_or_else(|| "swarm".to_string()),
            channel: input.channel.clone().unwrap_or_else(|| "swarm".to_string()),
            user_id: input.user_id.clone().unwrap_or_else(|| "swarm".to_string()),
            call_index: 0,
        }
    }

    async fn start(&self, model: &str) {
        if let Some(logger) = self.logger
            && let Err(error) = logger
                .start_trajectory(
                    &self.trajectory_id,
                    &self.session_id,
                    &self.channel,
                    &self.user_id,
                    model,
                )
                .await
        {
            warn!(error = %error, "swarm node trajectory start failed");
        }
    }

    async fn log_frame(
        &mut self,
        request_messages: Vec<StoredMessage>,
        json_mode: bool,
        response: &str,
        model: Option<&str>,
    ) {
        let Some(logger) = self.logger else {
            return;
        };
        let draft = crate::harness::loop_engine::TrajectoryFrameDraft {
            trajectory_id: self.trajectory_id.clone(),
            call_index: self.call_index,
            model: model.unwrap_or("unknown").to_string(),
            provider_fingerprint: String::new(),
            request_messages,
            request_was_json: json_mode,
            response: response.to_string(),
            capture: crate::harness::loop_engine::TrajectoryFrameCapture::Full,
        };
        self.call_index += 1;
        if let Err(error) = logger.log_provider_frame(draft).await {
            warn!(error = %error, "swarm node trajectory frame failed");
        }
    }

    async fn finish(
        self,
        answer: Option<String>,
        exit: crate::harness::trajectory::TrajectoryExitReason,
    ) {
        if let Some(logger) = self.logger
            && let Err(error) = logger
                .finish_trajectory(&self.trajectory_id, answer, exit)
                .await
        {
            warn!(error = %error, "swarm node trajectory finish failed");
        }
    }
}

/// Executes a `swarm_node` work item: decide -> answer|delegate (and the
/// aggregate half when `mode == "aggregate"`).
pub struct SwarmNodeHandler {
    provider: Arc<dyn crate::provider::ChatProvider>,
    settings: AgentSwarmSettings,
    trajectory: Option<crate::harness::trajectory::TrajectoryLogger>,
}

impl SwarmNodeHandler {
    pub fn new(
        provider: Arc<dyn crate::provider::ChatProvider>,
        settings: AgentSwarmSettings,
    ) -> Self {
        Self {
            provider,
            settings,
            trajectory: None,
        }
    }

    /// Wires node-level trajectory recording — each attempt logs one
    /// trajectory and every provider call a frame, so swarm runs feed the
    /// same observation/scorecard loop as ordinary turns.
    pub fn with_trajectory(mut self, logger: crate::harness::trajectory::TrajectoryLogger) -> Self {
        self.trajectory = Some(logger);
        self
    }

    /// Model call under the goal's declared budget: each call reserves one
    /// provider-call slot and caps the node timeout at the goal deadline —
    /// `max_provider_calls`/`deadline_secs`/`max_response_bytes` are real
    /// limits for swarm nodes, not just plan metadata. Budget exhaustion or
    /// deadline overrun surfaces as a node-level failure (the item still
    /// succeeds; the aggregate merges partial results).
    async fn complete(
        &self,
        ctx: &WorkHandlerContext,
        messages: Vec<StoredMessage>,
        json_mode: bool,
        trajectory: &mut SwarmNodeTrajectory<'_>,
    ) -> Result<String, SwarmModelError> {
        let reservation = ctx
            .engine
            .reserve_provider_call(&ctx.claim.attempt.goal_id)
            .await
            .map_err(SwarmModelError::Failure)?;

        let request = if json_mode {
            CompletionRequest::json(messages.clone())
        } else {
            CompletionRequest::from_messages(messages.clone())
        };
        let deadline_ms = (reservation.deadline_at_unix - current_unix_timestamp_i64())
            .max(1)
            .saturating_mul(1000);
        let node_timeout_ms = self
            .settings
            .max_node_timeout_ms
            .max(1)
            .min(deadline_ms.max(1) as u64);

        let result = match timeout(
            Duration::from_millis(node_timeout_ms),
            self.provider.complete(request),
        )
        .await
        {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(err)) => Err(SwarmModelError::Failure(err)),
            Err(_) => Err(SwarmModelError::Timeout),
        };

        if let Ok(reply) = &result {
            if let Err(error) = ctx
                .engine
                .record_provider_response_bytes(
                    &ctx.claim.attempt.goal_id,
                    u32::try_from(reply.len()).unwrap_or(u32::MAX),
                )
                .await
            {
                warn!(error = %error, "swarm node response accounting failed");
            }
            trajectory
                .log_frame(messages, json_mode, reply, self.provider.model_name())
                .await;
        }
        result
    }

    /// The direct-answer leg: use the planner's seeded answer when present,
    /// otherwise ask for one. Model failures surface as failed-status
    /// outcomes, not attempt errors (see module note).
    async fn answer_outcome(
        &self,
        ctx: &WorkHandlerContext,
        input: &SwarmNodeInput,
        role_name: &str,
        role_definition: &str,
        direct_answer: Option<String>,
        traj: &mut SwarmNodeTrajectory<'_>,
    ) -> anyhow::Result<WorkOutcome> {
        let outcome = if let Some(answer) = direct_answer
            && !answer.trim().is_empty()
        {
            swarm_node_outcome(&answer, "success", input)
        } else {
            let messages = build_swarm_answer_messages(
                &input.history,
                &input.task,
                role_name,
                role_definition,
            );
            match self.complete(ctx, messages, false, traj).await {
                Ok(answer) => swarm_node_outcome(&answer, "success", input),
                Err(SwarmModelError::Timeout) => swarm_node_outcome("", "timeout", input),
                Err(SwarmModelError::Failure(err)) => {
                    warn!(error = %err, "swarm node answer call failed");
                    swarm_node_outcome("", "failed", input)
                }
            }
        };
        self.maybe_publish_terminal_artifact(ctx, input, outcome)
            .await
    }

    /// The run's terminal node (direct root answer or root aggregate)
    /// publishes its answer as the `analysis_report` artifact the goal's
    /// `ArtifactExists` criterion verifies — the evidence trail keeps the
    /// deliverable, not just the reply.
    async fn maybe_publish_terminal_artifact(
        &self,
        ctx: &WorkHandlerContext,
        input: &SwarmNodeInput,
        mut outcome: WorkOutcome,
    ) -> anyhow::Result<WorkOutcome> {
        let is_terminal = input.swarm_root || input.swarm_terminal;
        let answer = outcome
            .evidence
            .get("answer")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if !is_terminal || answer.trim().is_empty() {
            return Ok(outcome);
        }
        let run_id = input.swarm_run_id.as_deref().unwrap_or("unknown");
        match ctx
            .engine
            .publish_artifact(
                PublishArtifactRequest {
                    kind: ArtifactKind::AnalysisReport,
                    name: format!("swarm-{run_id}"),
                    version: "1".to_string(),
                    content: serde_json::json!({
                        "swarm_run_id": run_id,
                        "answer": truncate_swarm_text(&answer, 4096),
                    }),
                    source_goal_id: Some(ctx.claim.work_item.goal_id.clone()),
                    parent_artifact_id: None,
                },
                SWARM_LOOP_ACTOR,
            )
            .await
        {
            Ok(published) => outcome.artifact_ids.push(published.artifact.id),
            Err(error) => {
                warn!(error = %error, "swarm terminal artifact publish failed");
            }
        }
        Ok(outcome)
    }

    /// Aggregation leg: merge declared upstream outcomes into one answer.
    async fn aggregate_outcome(
        &self,
        ctx: &WorkHandlerContext,
        input: &SwarmNodeInput,
        role_name: &str,
        traj: &mut SwarmNodeTrajectory<'_>,
    ) -> anyhow::Result<WorkOutcome> {
        let mut any_failed = false;
        let mut child_json = Vec::with_capacity(ctx.upstream.len());
        for (step_id, outcome) in &ctx.upstream {
            let evidence = &outcome.evidence;
            let status = evidence
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("success");
            any_failed |= matches!(status, "failed" | "timeout");
            child_json.push(serde_json::json!({
                "agent_id": step_id,
                "nickname": evidence.get("nickname").and_then(Value::as_str).unwrap_or(step_id),
                "role_name": evidence.get("role_name").and_then(Value::as_str).unwrap_or("general"),
                "status": status,
                "answer": truncate_swarm_text(
                    evidence
                        .get("answer")
                        .and_then(Value::as_str)
                        .unwrap_or(&outcome.summary),
                    180,
                ),
            }));
        }
        let merged = match self
            .complete(
                ctx,
                build_swarm_merge_messages(&input.history, &input.task, role_name, &child_json),
                false,
                traj,
            )
            .await
        {
            Ok(text) => text,
            Err(_) => ctx
                .upstream
                .values()
                .filter_map(|outcome| {
                    let answer = outcome
                        .evidence
                        .get("answer")
                        .and_then(Value::as_str)
                        .unwrap_or(&outcome.summary);
                    (!answer.trim().is_empty()).then(|| answer.trim().to_string())
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
        };
        let status = if any_failed { "partial" } else { "success" };
        let outcome = swarm_node_outcome(&merged, status, input);
        self.maybe_publish_terminal_artifact(ctx, input, outcome)
            .await
    }
}

/// Outcome carrying the node result in `evidence` — aggregates read
/// `answer`/`status`/`nickname`/`role_name` from upstream entries.
fn swarm_node_outcome(answer: &str, status: &str, input: &SwarmNodeInput) -> WorkOutcome {
    WorkOutcome {
        summary: truncate_swarm_text(answer, 240).to_string(),
        artifact_ids: Vec::new(),
        evidence: serde_json::json!({
            "answer": answer,
            "status": status,
            "nickname": input.nickname,
            "role_name": input.role_name,
            "depth": input.depth,
        }),
        sleep_until_unix: None,
        continuation: None,
    }
}

#[async_trait::async_trait]
impl crate::harness::loop_engine::WorkHandler for SwarmNodeHandler {
    fn name(&self) -> &'static str {
        SWARM_NODE_DURABLE_HANDLER
    }

    fn effect_class(&self) -> EffectClass {
        EffectClass::LocalWrite
    }

    /// Model calls only — replaying an attempt after a crash is safe.
    fn replay_safe(&self) -> bool {
        true
    }

    async fn execute(&self, ctx: &WorkHandlerContext) -> anyhow::Result<WorkOutcome> {
        let input: SwarmNodeInput = serde_json::from_value(ctx.claim.work_item.input.clone())
            .context("invalid swarm_node input")?;
        let mut traj = SwarmNodeTrajectory::new(self.trajectory.as_ref(), &ctx.claim, &input);
        traj.start(self.provider.model_name().unwrap_or("swarm"))
            .await;
        let outcome = self.execute_inner(ctx, &input, &mut traj).await;
        let exit = match &outcome {
            Ok(outcome) => match outcome
                .evidence
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("success")
            {
                "timeout" => crate::harness::trajectory::TrajectoryExitReason::Timeout,
                "failed" => crate::harness::trajectory::TrajectoryExitReason::ToolError,
                _ => crate::harness::trajectory::TrajectoryExitReason::FinalAnswer,
            },
            Err(_) => crate::harness::trajectory::TrajectoryExitReason::InternalError,
        };
        let answer = outcome.as_ref().ok().and_then(|outcome| {
            outcome
                .evidence
                .get("answer")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        traj.finish(answer, exit).await;
        outcome
    }
}

impl SwarmNodeHandler {
    async fn execute_inner(
        &self,
        ctx: &WorkHandlerContext,
        input: &SwarmNodeInput,
        traj: &mut SwarmNodeTrajectory<'_>,
    ) -> anyhow::Result<WorkOutcome> {
        let role_name = input
            .role_name
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "general".to_string());
        let role_definition = input
            .role_definition
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "负责当前子任务并输出可交付结果。".to_string());
        if input.mode.as_deref() == Some("aggregate") {
            return self.aggregate_outcome(ctx, input, &role_name, traj).await;
        }

        // Decide leg: planner picks answer|delegate unless depth forces a
        // direct answer.
        let force_answer = input.depth >= self.settings.max_depth.max(1);
        let mut direct_answer = force_answer.then(String::new);
        let mut child_specs = Vec::<SwarmChildPlan>::new();
        if !force_answer {
            let planner_messages = build_swarm_planner_messages(
                &input.history,
                &input.task,
                &role_name,
                &role_definition,
                input.depth,
                self.settings.max_depth,
            );
            match self.complete(ctx, planner_messages, true, traj).await {
                Ok(reply) => {
                    if let Some(value) = parse_flexible_json_value(&reply)
                        && let Ok(plan) = serde_json::from_value::<SwarmNodePlanEnvelope>(value)
                    {
                        match plan
                            .mode
                            .unwrap_or_else(|| "answer".to_string())
                            .to_ascii_lowercase()
                            .as_str()
                        {
                            "delegate" => {
                                child_specs = plan
                                    .children
                                    .unwrap_or_default()
                                    .into_iter()
                                    .filter(|child| !child.task.trim().is_empty())
                                    .take(4)
                                    .collect();
                            }
                            _ => {
                                direct_answer = Some(plan.answer.unwrap_or_default());
                            }
                        }
                    } else {
                        direct_answer = Some(String::new());
                    }
                }
                Err(SwarmModelError::Timeout) => {
                    return Ok(swarm_node_outcome("", "timeout", input));
                }
                Err(SwarmModelError::Failure(err)) => {
                    warn!(error = %err, "swarm planner failed, fallback to direct answer");
                    direct_answer = Some(String::new());
                }
            }
        }
        if child_specs.is_empty() {
            return self
                .answer_outcome(
                    ctx,
                    input,
                    &role_name,
                    &role_definition,
                    direct_answer,
                    traj,
                )
                .await;
        }

        // Delegate leg: extend the goal's DAG — one item per child plus an
        // aggregate item that depends on all of them. The aggregate carries
        // the parent's task/role so the merge happens in the node's voice.
        let own_step = ctx.claim.work_item.step_id.as_str();
        let history = bounded_swarm_input_history(&input.history);
        let mut steps = Vec::with_capacity(child_specs.len() + 1);
        let mut child_step_ids = Vec::with_capacity(child_specs.len());
        for (index, child) in child_specs.iter().enumerate() {
            let child_step_id = format!("{own_step}.c{}", index + 1);
            child_step_ids.push(child_step_id.clone());
            steps.push(WorkflowStep {
                id: child_step_id,
                handler: SWARM_NODE_DURABLE_HANDLER.to_string(),
                effect: EffectClass::LocalWrite,
                input: serde_json::json!({
                    "task": truncate_swarm_text(&child.task, 1024),
                    "role_name": child
                        .role_name
                        .clone()
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or_else(|| "general".to_string()),
                    "role_definition": child
                        .role_definition
                        .clone()
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or_else(|| "负责当前子任务并输出可交付结果。".to_string()),
                    "nickname": child.nickname,
                    "depth": input.depth + 1,
                    "parent_step_id": own_step,
                    "swarm_run_id": input.swarm_run_id,
                    "history": history,
                    "channel": input.channel,
                    "user_id": input.user_id,
                    "session_id": input.session_id,
                }),
                retry: RetryPolicy {
                    max_attempts: 2,
                    backoff_secs: 5,
                },
            });
        }
        let aggregate_step_id = format!("{own_step}.agg");
        steps.push(WorkflowStep {
            id: aggregate_step_id.clone(),
            handler: SWARM_NODE_DURABLE_HANDLER.to_string(),
            effect: EffectClass::LocalWrite,
            input: serde_json::json!({
                "mode": "aggregate",
                "task": truncate_swarm_text(&input.task, 1024),
                "role_name": role_name,
                "depth": input.depth,
                "parent_step_id": own_step,
                "swarm_run_id": input.swarm_run_id,
                "history": history,
                "depends_on": child_step_ids,
                "swarm_terminal": input.swarm_root,
                "channel": input.channel,
                "user_id": input.user_id,
                "session_id": input.session_id,
            }),
            retry: RetryPolicy {
                max_attempts: 2,
                backoff_secs: 5,
            },
        });
        // Under concurrent workers a transient store error can reject the
        // extension — retry once before degrading to a direct answer so a
        // busy seam never silently loses a delegation.
        let mut extension = ctx
            .engine
            .extend_workflow(&ctx.claim.attempt.goal_id, steps.clone(), SWARM_LOOP_ACTOR)
            .await;
        if extension.is_err() {
            tokio::time::sleep(Duration::from_millis(150)).await;
            extension = ctx
                .engine
                .extend_workflow(&ctx.claim.attempt.goal_id, steps, SWARM_LOOP_ACTOR)
                .await;
        }
        match extension {
            Ok(_) => Ok(WorkOutcome {
                summary: format!("delegated to {} children", child_step_ids.len()),
                artifact_ids: Vec::new(),
                evidence: serde_json::json!({
                    "delegated": true,
                    "status": "success",
                    "aggregate_step_id": aggregate_step_id,
                    "children_step_ids": child_step_ids,
                    "nickname": input.nickname,
                    "role_name": input.role_name,
                    "depth": input.depth,
                }),
                sleep_until_unix: None,
                continuation: None,
            }),
            Err(error) => {
                warn!(error = %error, step_id = %own_step, "swarm DAG extension rejected; answering directly");
                self.answer_outcome(ctx, input, &role_name, &role_definition, None, traj)
                    .await
            }
        }
    }
}

/// Sampled history bounded for the 8KiB work-item input limit.
fn bounded_swarm_input_history(history: &[StoredMessage]) -> Vec<StoredMessage> {
    sample_swarm_history(history, 8)
        .into_iter()
        .map(|mut message| {
            message.content = truncate_swarm_text(&message.content, 400).to_string();
            message
        })
        .collect()
}

impl MessageService {
    /// Durable swarm run: the projection's goal IS the execution — nodes
    /// claim work items, delegation extends the tree, aggregates merge
    /// upstream outcomes. Returns (reply, run status, error) like the
    /// in-process runner so the caller's audit/finish path is unchanged.
    async fn run_swarm_durable(
        &self,
        run_id: &str,
        incoming: &IncomingMessage,
        goal_id: &str,
        shared: &SwarmExecutionShared,
    ) -> (Option<String>, AgentSwarmRunStatus, Option<String>) {
        let Some(engine) = self.loop_engine.clone() else {
            return (
                None,
                AgentSwarmRunStatus::Failed,
                Some("loop engine not configured".to_string()),
            );
        };
        let goal_id = goal_id.to_string();
        let mut registry = crate::harness::loop_engine::WorkHandlerRegistry::default();
        let mut handler = SwarmNodeHandler::new(self.provider.clone(), self.agent_swarm.clone());
        if let Some(logger) = &self.trajectory_logger {
            handler = handler.with_trajectory(logger.clone());
        }
        if let Err(error) = registry.register(Arc::new(handler)) {
            return (
                None,
                AgentSwarmRunStatus::Failed,
                Some(format!("handler registration failed: {error}")),
            );
        }
        let lease_secs = ((self.agent_swarm.max_node_timeout_ms / 1000) as u32)
            .saturating_add(30)
            .clamp(1, 3600);
        let worker = match crate::harness::loop_engine::LoopWorker::with_registry(
            engine.clone(),
            registry,
            format!("swarm:{run_id}"),
            lease_secs,
        ) {
            Ok(worker) => worker,
            Err(error) => {
                return (
                    None,
                    AgentSwarmRunStatus::Failed,
                    Some(format!("worker setup failed: {error}")),
                );
            }
        };
        let max_rounds = (self.agent_swarm.max_agents.max(1) * 2 + 8).min(128);
        let run_timeout = Duration::from_millis(self.agent_swarm.max_run_timeout_ms.max(1));
        // The synchronous reply window is capped far below the run timeout:
        // message processing is serial per channel, so a long swarm must not
        // head-block every later update. Past the cap the run keeps executing
        // (spawned workers outlive this call), the run row is flagged
        // `awaiting_reply`, an ack is returned now, and the swarm janitor
        // delivers the real answer once the goal reaches a terminal state.
        let sync_cap = run_timeout.min(Duration::from_millis(
            self.agent_swarm.reply_sync_wait_ms.max(1),
        ));
        let mut deferred_reply = false;
        let run_error = match timeout(
            sync_cap,
            worker.run_goal_concurrent(&goal_id, self.agent_swarm.max_parallel.max(1), max_rounds),
        )
        .await
        {
            Ok(Ok(_)) => None,
            Ok(Err(error)) => Some(format!("goal run failed: {error}")),
            Err(_) if run_timeout <= sync_cap => Some("swarm run timeout".to_string()),
            Err(_) => {
                deferred_reply = true;
                None
            }
        };

        // Root answer: the root aggregate's merged outcome when the root
        // delegated, otherwise the root item's own answer.
        let report = match engine.resume_goal(&goal_id, SWARM_LOOP_ACTOR).await {
            Ok(report) => report,
            Err(error) => {
                return (
                    None,
                    AgentSwarmRunStatus::Failed,
                    Some(format!("resume failed: {error}")),
                );
            }
        };
        let outcome_for = async |step_id: &str| -> Option<WorkOutcome> {
            let item = report
                .work_items
                .iter()
                .find(|item| item.step_id == step_id)?;
            engine.work_item_outcome(&item.id).await.ok().flatten()
        };
        let root_answer = outcome_for(&format!("{run_id}:1.agg")).await;
        let root_answer = match root_answer {
            Some(outcome) => Some(outcome),
            None => outcome_for(&format!("{run_id}:1")).await,
        };

        self.mirror_durable_swarm_audit(run_id, &incoming.channel, &report)
            .await;

        // Summary counters mirror the in-process path's semantics: every
        // non-aggregate item is one agent; its outcome evidence carries the
        // terminal status (aggregates merge but aren't user-visible nodes).
        for item in &report.work_items {
            if item.input.get("mode").and_then(Value::as_str) == Some("aggregate") {
                continue;
            }
            shared.agent_counter.fetch_add(1, Ordering::Relaxed);
            match item.status {
                crate::harness::loop_engine::WorkItemStatus::Succeeded => {
                    let outcome_status = engine
                        .work_item_outcome(&item.id)
                        .await
                        .ok()
                        .flatten()
                        .and_then(|outcome| {
                            outcome
                                .evidence
                                .get("status")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .unwrap_or_else(|| "success".to_string());
                    match outcome_status.as_str() {
                        "timeout" => {
                            shared.timeout_count.fetch_add(1, Ordering::Relaxed);
                            shared.failed_count.fetch_add(1, Ordering::Relaxed);
                        }
                        "failed" => {
                            shared.failed_count.fetch_add(1, Ordering::Relaxed);
                        }
                        "partial" => {
                            shared.partial_count.fetch_add(1, Ordering::Relaxed);
                        }
                        _ => {
                            shared.success_count.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                crate::harness::loop_engine::WorkItemStatus::Failed => {
                    shared.failed_count.fetch_add(1, Ordering::Relaxed);
                }
                _ => {
                    shared.skipped_count.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        let degraded = run_error.is_some()
            || shared.failed_count.load(Ordering::Relaxed) > 0
            || shared.partial_count.load(Ordering::Relaxed) > 0
            || shared.skipped_count.load(Ordering::Relaxed) > 0;
        let answer_text = root_answer
            .as_ref()
            .and_then(|outcome| {
                outcome
                    .evidence
                    .get("answer")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .filter(|answer| !answer.trim().is_empty())
            .or_else(|| root_answer.as_ref().map(|outcome| outcome.summary.clone()));
        if deferred_reply && answer_text.is_none() {
            // Only promise delivery when an outbound sender exists for this
            // channel — otherwise the run still finishes but nobody can be
            // pushed the answer, and `awaiting_reply` would never clear.
            let can_deliver = self
                .outbound_sender
                .as_ref()
                .is_some_and(|sender| sender.supports_channel(&incoming.channel));
            if can_deliver
                && let Err(error) = self
                    .memory
                    .set_swarm_run_awaiting_reply(&incoming.channel, run_id, true)
                    .await
            {
                warn!(error = %error, run_id = %run_id, "failed to flag deferred swarm reply");
            }
            return (
                Some(if can_deliver {
                    "多智能体任务仍在后台运行，完成后我会把结果发给你。".to_string()
                } else {
                    "多智能体任务仍在后台运行。".to_string()
                }),
                AgentSwarmRunStatus::Running,
                None,
            );
        }
        let status = match (&answer_text, degraded) {
            (None, _) => AgentSwarmRunStatus::Failed,
            (Some(_), true) => AgentSwarmRunStatus::Partial,
            (Some(_), false) => AgentSwarmRunStatus::Completed,
        };
        (answer_text, status, run_error)
    }

    /// Reaps swarm runs orphaned by a crash: the run row is still `running`
    /// but its durable goal already reached a terminal status — mirror the
    /// final audit rows and close the run so `/agents` reflects reality.
    pub async fn reap_orphaned_swarm_runs(&self) -> anyhow::Result<usize> {
        let Some(engine) = self.loop_engine.clone() else {
            return Ok(0);
        };
        let mut reaped = 0usize;
        for run in self
            .memory
            .list_running_agent_swarm_runs(32)
            .await
            .context("failed to list running swarm runs")?
        {
            let Some(goal) = engine
                .get_goal_by_request_id(&format!("swarm:{}", run.run_id))
                .await
                .ok()
                .flatten()
            else {
                continue; // projection never landed — leave for TTL/audit
            };
            if !matches!(
                goal.status,
                GoalStatus::Achieved
                    | GoalStatus::Failed
                    | GoalStatus::Blocked
                    | GoalStatus::Canceled
                    | GoalStatus::Rejected
            ) {
                continue; // still dispatchable — a live worker may be mid-run
            }
            let report = match engine.resume_goal(&goal.id, SWARM_LOOP_ACTOR).await {
                Ok(report) => report,
                Err(error) => {
                    warn!(error = %error, run_id = %run.run_id, "orphan swarm resume failed");
                    continue;
                }
            };
            self.mirror_durable_swarm_audit(&run.run_id, &run.channel, &report)
                .await;
            let status = if goal.status == GoalStatus::Achieved {
                AgentSwarmRunStatus::Completed
            } else {
                AgentSwarmRunStatus::Failed
            };
            let _ = self
                .memory
                .finish_agent_swarm_run(FinishAgentSwarmRunRequest {
                    run_id: run.run_id.clone(),
                    channel: run.channel.clone(),
                    status,
                    finished_at_unix: current_unix_timestamp_i64(),
                    error: if goal.status == GoalStatus::Achieved {
                        None
                    } else {
                        Some(format!("process restart: goal {}", goal.status.as_str()))
                    },
                })
                .await;
            reaped += 1;
        }

        // Deferred replies: a run whose sync window elapsed carries
        // `awaiting_reply` — deliver the final answer once the goal is
        // terminal, then clear the flag so it sends exactly once.
        let Some(sender) = self.outbound_sender.clone() else {
            return Ok(reaped);
        };
        for run in self
            .memory
            .list_awaiting_reply_swarm_runs(32)
            .await
            .context("failed to list awaiting-reply swarm runs")?
        {
            let Some(goal) = engine
                .get_goal_by_request_id(&format!("swarm:{}", run.run_id))
                .await
                .ok()
                .flatten()
            else {
                continue;
            };
            if !matches!(
                goal.status,
                GoalStatus::Achieved
                    | GoalStatus::Failed
                    | GoalStatus::Blocked
                    | GoalStatus::Canceled
                    | GoalStatus::Rejected
            ) {
                continue;
            }
            if !sender.supports_channel(&run.channel) {
                // Unsendable channel — clear the flag rather than retrying
                // every sweep forever.
                let _ = self
                    .memory
                    .set_swarm_run_awaiting_reply(&run.channel, &run.run_id, false)
                    .await;
                continue;
            }
            let report = match engine.resume_goal(&goal.id, SWARM_LOOP_ACTOR).await {
                Ok(report) => report,
                Err(error) => {
                    warn!(error = %error, run_id = %run.run_id, "swarm reply resume failed");
                    continue;
                }
            };
            let outcome_for = async |step_id: &str| -> Option<WorkOutcome> {
                let item = report
                    .work_items
                    .iter()
                    .find(|item| item.step_id == step_id)?;
                engine.work_item_outcome(&item.id).await.ok().flatten()
            };
            let answer = outcome_for(&format!("{}:1.agg", run.run_id))
                .await
                .or(outcome_for(&format!("{}:1", run.run_id)).await)
                .and_then(|outcome| {
                    outcome
                        .evidence
                        .get("answer")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .filter(|a| !a.trim().is_empty())
                        .or(Some(outcome.summary))
                });
            let text = match answer {
                Some(answer) if goal.status == GoalStatus::Achieved => answer,
                _ => "多智能体任务结束，但没有产出可送达的结果。".to_string(),
            };
            match sender
                .send(
                    &run.channel,
                    &run.session_id,
                    &text,
                    &format!("swarm-reply:{}", run.run_id),
                )
                .await
            {
                Ok(_) => {
                    let _ = self
                        .memory
                        .set_swarm_run_awaiting_reply(&run.channel, &run.run_id, false)
                        .await;
                }
                Err(error) => {
                    warn!(error = %error, run_id = %run.run_id, "deferred swarm reply send failed");
                    // One dedup'd triage signal — a reply that keeps failing
                    // is operator-visible instead of warn-log silent.
                    if let Some(engine) = &self.loop_engine {
                        let _ = engine
                        .ingest_signal(
                            crate::harness::loop_engine::CreateSignalRequest {
                                kind: crate::harness::loop_engine::SignalKind::Manual,
                                trust: crate::harness::loop_engine::SignalTrust::Internal,
                                source: "internal:swarm-deliver".to_string(),
                                external_id: Some(format!("swarm-deliver:{}", run.run_id)),
                                content: format!(
                                    "swarm run {}: deferred reply delivery failed on channel {}: {error}",
                                    run.run_id, run.channel
                                ),
                                metadata: std::collections::BTreeMap::from([
                                    ("run_id".to_string(), run.run_id.clone()),
                                    ("channel".to_string(), run.channel.clone()),
                                ]),
                            },
                            "internal:swarm",
                        )
                        .await;
                    }
                }
            }
        }
        Ok(reaped)
    }

    /// Rebuilds the `/agents` audit rows from the durable goal tree — each
    /// work item maps to a node row keyed by its step id.
    async fn mirror_durable_swarm_audit(&self, run_id: &str, channel: &str, report: &ResumeReport) {
        for item in &report.work_items {
            let input: SwarmNodeInput = match serde_json::from_value(item.input.clone()) {
                Ok(input) => input,
                Err(_) => continue,
            };
            if input.mode.as_deref() == Some("aggregate") {
                continue; // merge nodes are not user-visible agents
            }
            let exit_status = match item.status {
                crate::harness::loop_engine::WorkItemStatus::Succeeded => {
                    Some(AgentSwarmNodeExitStatus::Success)
                }
                crate::harness::loop_engine::WorkItemStatus::Failed => {
                    Some(AgentSwarmNodeExitStatus::Failed)
                }
                _ => Some(AgentSwarmNodeExitStatus::Skipped),
            };
            let _ = self
                .memory
                .upsert_agent_swarm_node(UpsertAgentSwarmNodeRequest {
                    agent_id: item.step_id.clone(),
                    run_id: run_id.to_string(),
                    channel: channel.to_string(),
                    parent_agent_id: input.parent_step_id.clone(),
                    depth: input.depth,
                    nickname: input.nickname.clone().unwrap_or_else(|| {
                        default_swarm_nickname(
                            &input.role_name.clone().unwrap_or_default(),
                            item.ordinal as usize + 1,
                        )
                    }),
                    role_name: input
                        .role_name
                        .clone()
                        .unwrap_or_else(|| "general".to_string()),
                    role_definition: input
                        .role_definition
                        .clone()
                        .unwrap_or_else(|| "负责当前子任务并输出可交付结果。".to_string()),
                    task: input.task.clone(),
                    state: AgentSwarmNodeState::Exited,
                    exit_status,
                    summary: None,
                    error: None,
                    started_at_unix: item.created_at_unix,
                    finished_at_unix: Some(item.created_at_unix),
                })
                .await;
        }
    }
}
