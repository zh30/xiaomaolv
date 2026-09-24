use super::*;

pub(super) enum CodeModeCompletion {
    Finished(String),
    PendingStream { text: String, run: AgentRun },
}

impl CodeModeCompletion {
    pub(super) fn into_text(self) -> String {
        match self {
            Self::Finished(text) | Self::PendingStream { text, .. } => text,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CodeModeAttempt {
    Normal,
    Probe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodeModeCircuitChange {
    None,
    Opened,
    Closed,
}

impl MessageService {
    pub(super) async fn complete_with_code_mode(
        &self,
        history: Vec<StoredMessage>,
        tools: &[McpToolInfo],
        runtime: &McpRuntime,
        force_shadow: bool,
        incoming: &IncomingMessage,
        defer_success_finish: bool,
    ) -> anyhow::Result<Option<CodeModeCompletion>> {
        let started_at = Instant::now();
        let planner = self.code_mode_planner.clone();
        let planner_name = planner.name();
        let policy = CodeModePolicy::new(self.agent_code_mode.clone());
        let code_mode_tools = tools
            .iter()
            .filter(|tool| policy.allows_tool(tool))
            .cloned()
            .collect::<Vec<_>>();
        if code_mode_tools.is_empty() {
            let audit = CodeModeAuditRecord::fallback(
                planner_name,
                "no code mode tools allowed by capability policy",
            );
            emit_code_mode_audit(&audit, self.agent_code_mode.normalized_timeout_warn_ratio());
            self.record_code_mode_counters(&audit, force_shadow, CodeModeCircuitChange::None);
            return Ok(None);
        }

        let plan = planner.build_plan(&history, &code_mode_tools).await?;
        let Some(plan) = plan else {
            let audit = CodeModeAuditRecord::fallback(planner_name, "planner returned no plan");
            emit_code_mode_audit(&audit, self.agent_code_mode.normalized_timeout_warn_ratio());
            self.record_code_mode_counters(&audit, force_shadow, CodeModeCircuitChange::None);
            return Ok(None);
        };

        let planned_calls = plan.calls.len();
        let environment: Box<dyn ExecutionEnvironment> = match self.agent_code_mode.execution_mode {
            CodeModeExecutionMode::Local => {
                Box::new(LocalExecutionEnvironment::new(self.agent_code_mode.clone()))
            }
            CodeModeExecutionMode::Subprocess => Box::new(SubprocessExecutionEnvironment::new(
                self.agent_code_mode.clone(),
            )),
        };
        let execution = environment.execute(runtime, &plan, &code_mode_tools).await;
        let execution = match execution {
            Ok(report) => report,
            Err(err) => {
                let audit = CodeModeAuditRecord {
                    planner: planner_name.to_string(),
                    used: false,
                    fallback: true,
                    reason: Some(format!("{}; isolation={:?}", err, environment.isolation())),
                    planned_calls,
                    executed_calls: 0,
                    failed_calls: 0,
                    timed_out_calls: 0,
                    elapsed_ms: started_at.elapsed().as_millis(),
                };
                emit_code_mode_audit(&audit, self.agent_code_mode.normalized_timeout_warn_ratio());
                self.record_code_mode_counters(&audit, force_shadow, CodeModeCircuitChange::None);
                return Ok(None);
            }
        };

        let mut audit = CodeModeAuditRecord {
            planner: planner_name.to_string(),
            used: !self.agent_code_mode.shadow_mode && !force_shadow,
            fallback: self.agent_code_mode.shadow_mode || force_shadow,
            reason: if self.agent_code_mode.shadow_mode {
                Some("shadow_mode enabled".to_string())
            } else if force_shadow {
                Some("timeout circuit probe".to_string())
            } else {
                None
            },
            planned_calls,
            executed_calls: execution.calls.len(),
            failed_calls: execution.failed_calls,
            timed_out_calls: execution.timed_out_calls,
            elapsed_ms: started_at.elapsed().as_millis(),
        };
        let circuit_change = self.update_code_mode_timeout_circuit(&audit);
        if matches!(circuit_change, CodeModeCircuitChange::Opened) {
            audit.used = false;
            audit.fallback = true;
            audit.reason = Some("timeout auto shadow circuit opened".to_string());
        } else if matches!(circuit_change, CodeModeCircuitChange::Closed) && force_shadow {
            audit.reason = Some("timeout circuit probe succeeded; circuit closed".to_string());
        }
        emit_code_mode_audit(&audit, self.agent_code_mode.normalized_timeout_warn_ratio());
        self.record_code_mode_counters(&audit, force_shadow, circuit_change);

        if self.agent_code_mode.shadow_mode
            || force_shadow
            || matches!(circuit_change, CodeModeCircuitChange::Opened)
        {
            return Ok(None);
        }

        let model = self.provider.model_name().unwrap_or("unknown").to_string();
        let mut run = AgentRun::start(AgentRunStart {
            logger: self.trajectory_logger.clone(),
            metrics: self.trajectory_metrics.clone(),
            session_id: incoming.session_id.clone(),
            channel: incoming.channel.clone(),
            user_id: incoming.user_id.clone(),
            model,
        })
        .await;
        let mut tool_calls = Vec::new();
        for (iteration, call) in execution.calls.iter().enumerate() {
            let record = run
                .record_tool_call(code_mode_tool_call_record(call, iteration))
                .await;
            tool_calls.push(record);
        }

        let mut next_history = history;
        next_history.push(StoredMessage {
            role: MessageRole::System,
            content: format!(
                "CODE_MODE_TOOL_RESULT_JSON:\n{}",
                serde_json::to_string(&execution).unwrap_or_else(|_| {
                    "{\"calls\":[],\"failed_calls\":0,\"timed_out_calls\":0}".to_string()
                })
            ),
        });
        let reply = match self
            .complete_provider_for_run(
                &mut run,
                CompletionRequest {
                    messages: next_history.clone(),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(reply) => reply,
            Err(err) => {
                run.finish(AgentRunExit::InternalError).await;
                return Err(err).context("provider completion failed after code mode execution");
            }
        };
        let reply = match self
            .verify_final_answer(&next_history, &incoming.channel, reply, &tool_calls)
            .await
        {
            Ok(reply) => reply,
            Err(err) => {
                run.finish(AgentRunExit::InternalError).await;
                return Err(err).context("output verification failed after code mode execution");
            }
        };
        run.observe_iteration(0);
        if defer_success_finish {
            Ok(Some(CodeModeCompletion::PendingStream { text: reply, run }))
        } else {
            run.finish(AgentRunExit::FinalAnswer(reply.clone())).await;
            Ok(Some(CodeModeCompletion::Finished(reply)))
        }
    }

    pub(super) fn next_code_mode_attempt(&self) -> Option<CodeModeAttempt> {
        if !self.is_code_mode_timeout_circuit_open() {
            return Some(CodeModeAttempt::Normal);
        }

        let probe_count = self
            .code_mode_timeout_probe_counter
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        if should_probe_code_mode_timeout_circuit(
            probe_count,
            self.agent_code_mode.timeout_auto_shadow_probe_every,
        ) {
            return Some(CodeModeAttempt::Probe);
        }
        None
    }

    fn update_code_mode_timeout_circuit(
        &self,
        record: &CodeModeAuditRecord,
    ) -> CodeModeCircuitChange {
        if !self.agent_code_mode.timeout_auto_shadow_enabled || self.agent_code_mode.shadow_mode {
            return CodeModeCircuitChange::None;
        }

        let timeout_warn_ratio = self.agent_code_mode.normalized_timeout_warn_ratio();
        let current = self.code_mode_timeout_alert_streak.load(Ordering::Relaxed);
        let next = next_code_mode_timeout_streak(current, record, timeout_warn_ratio);
        self.code_mode_timeout_alert_streak
            .store(next, Ordering::Relaxed);

        if should_open_code_mode_timeout_circuit(next, &self.agent_code_mode) {
            let was_open = self
                .code_mode_timeout_circuit_open
                .swap(true, Ordering::SeqCst);
            if !was_open {
                warn!(
                    streak = next,
                    threshold = self.agent_code_mode.timeout_auto_shadow_streak.max(1),
                    timeout_warn_ratio,
                    "code mode timeout auto shadow circuit opened"
                );
                return CodeModeCircuitChange::Opened;
            }
            return CodeModeCircuitChange::None;
        }

        let is_timeout_alert = should_warn_code_mode_timeouts(record, timeout_warn_ratio);
        if self.is_code_mode_timeout_circuit_open() && !is_timeout_alert {
            let was_open = self
                .code_mode_timeout_circuit_open
                .swap(false, Ordering::SeqCst);
            if was_open {
                self.code_mode_timeout_alert_streak
                    .store(0, Ordering::Relaxed);
                self.code_mode_timeout_probe_counter
                    .store(0, Ordering::Relaxed);
                info!(
                    timeout_warn_ratio,
                    "code mode timeout auto shadow circuit closed"
                );
                return CodeModeCircuitChange::Closed;
            }
        }
        CodeModeCircuitChange::None
    }
}

fn emit_code_mode_audit(record: &CodeModeAuditRecord, timeout_warn_ratio: f64) {
    info!(
        planner = %record.planner,
        used = record.used,
        fallback = record.fallback,
        reason = %record.reason.as_deref().unwrap_or(""),
        planned_calls = record.planned_calls,
        executed_calls = record.executed_calls,
        failed_calls = record.failed_calls,
        timed_out_calls = record.timed_out_calls,
        elapsed_ms = record.elapsed_ms,
        "code mode audit"
    );

    if should_warn_code_mode_timeouts(record, timeout_warn_ratio)
        && let Some(timeout_ratio) = code_mode_timeout_ratio(record)
    {
        warn!(
            planner = %record.planner,
            used = record.used,
            fallback = record.fallback,
            executed_calls = record.executed_calls,
            timed_out_calls = record.timed_out_calls,
            timeout_ratio = timeout_ratio,
            timeout_ratio_pct = timeout_ratio * 100.0,
            threshold = timeout_warn_ratio,
            "code mode timeout ratio is high"
        );
    }
}

pub(crate) fn code_mode_timeout_ratio(record: &CodeModeAuditRecord) -> Option<f64> {
    if record.executed_calls == 0 {
        return None;
    }
    Some(record.timed_out_calls as f64 / record.executed_calls as f64)
}

pub(crate) fn should_warn_code_mode_timeouts(
    record: &CodeModeAuditRecord,
    timeout_warn_ratio: f64,
) -> bool {
    let Some(ratio) = code_mode_timeout_ratio(record) else {
        return false;
    };
    record.timed_out_calls > 0 && ratio >= timeout_warn_ratio
}

pub(crate) fn next_code_mode_timeout_streak(
    current: usize,
    record: &CodeModeAuditRecord,
    timeout_warn_ratio: f64,
) -> usize {
    if should_warn_code_mode_timeouts(record, timeout_warn_ratio) {
        return current.saturating_add(1);
    }
    0
}

pub(crate) fn should_probe_code_mode_timeout_circuit(
    probe_count: usize,
    probe_every: usize,
) -> bool {
    let interval = probe_every.max(1);
    probe_count.is_multiple_of(interval)
}

pub(crate) fn should_open_code_mode_timeout_circuit(
    timeout_streak: usize,
    settings: &AgentCodeModeSettings,
) -> bool {
    if !settings.timeout_auto_shadow_enabled || settings.shadow_mode {
        return false;
    }
    timeout_streak >= settings.timeout_auto_shadow_streak.max(1)
}

fn code_mode_tool_call_record(call: &CodeModeCallResult, iteration: usize) -> ToolCallRecord {
    let result = match (&call.result, &call.error) {
        (Some(result), _) => result.clone(),
        (None, Some(error)) => serde_json::json!({ "error": error }),
        (None, None) => serde_json::json!({ "error": "code mode call returned no result" }),
    };

    ToolCallRecord {
        call_index: 0,
        server: call.server.clone(),
        tool: call.tool.clone(),
        arguments: serde_json::json!({}),
        result,
        ok: call.ok,
        duration_ms: 0,
        iteration,
    }
}
