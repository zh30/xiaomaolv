use super::*;

use crate::harness::loop_engine::{
    AcceptanceCriterion, ApproveGoalRequest, ArtifactKind, CreateGoalRequest, EffectClass,
    ExecutionBudget, PlanGoalRequest, PublishArtifactRequest, RetryPolicy, WorkClaim, WorkOutcome,
    WorkflowSpec, WorkflowStep,
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
                .start_swarm_loop_projection(&run_id, &incoming.text)
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
        let run_result = timeout(
            max_run_timeout,
            self.execute_swarm_node(history.to_vec(), root_spec, 0, None, shared.clone()),
        )
        .await;

        let finished_at_unix = current_unix_timestamp_i64();
        let (reply_text, run_status, run_error) = match run_result {
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
                (Some(root.answer), status, None)
            }
            Ok(Err(err)) => (None, AgentSwarmRunStatus::Failed, Some(err.to_string())),
            Err(_) => (
                None,
                AgentSwarmRunStatus::Failed,
                Some("swarm run timeout".to_string()),
            ),
        };

        if let Err(err) = self
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
    ) -> Option<SwarmLoopProjection> {
        let engine = self.loop_engine.clone()?;
        match self.create_swarm_goal(&engine, run_id, root_task).await {
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
    ) -> anyhow::Result<String> {
        let goal = engine
            .create_goal(
                CreateGoalRequest {
                    objective: format!(
                        "agent swarm run {run_id}: {}",
                        truncate_swarm_text(root_task, 480)
                    ),
                    source_signal_ids: Vec::new(),
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
                            handler: SWARM_NODE_HANDLER.to_string(),
                            effect: EffectClass::LocalWrite,
                            input: serde_json::json!({
                                "swarm_root": true,
                                "max_nodes": max_nodes,
                                "swarm_run_id": run_id,
                                "task": truncate_swarm_text(root_task, 1024),
                                "role_name": "orchestrator",
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
            let checkpoint = engine
                .prepare_checkpoint(claim, &idempotency_key, SWARM_LOOP_ACTOR)
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
