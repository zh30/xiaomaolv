use super::*;

mod code_mode_path;
mod mcp_loop;

pub(super) use code_mode_path::CodeModeCircuitChange;
use code_mode_path::{CodeModeAttempt, CodeModeCompletion};
#[cfg(test)]
pub(super) use code_mode_path::{
    code_mode_timeout_ratio, next_code_mode_timeout_streak, should_open_code_mode_timeout_circuit,
    should_probe_code_mode_timeout_circuit, should_warn_code_mode_timeouts,
};
#[cfg(test)]
pub(super) use mcp_loop::{build_mcp_system_prompt, parse_mcp_tool_call};

struct McpLoopTelemetry {
    started_at: Instant,
    discovered_tools: usize,
    iterations: usize,
    prompt_chars_total: usize,
    tool_calls_total: usize,
    tool_calls_ok: usize,
    tool_calls_err: usize,
}

#[derive(Default)]
struct BufferedStreamSink {
    deltas: Vec<String>,
}

struct RecordingStreamSink<'a> {
    inner: &'a mut dyn StreamSink,
    deltas: Vec<String>,
}

const STREAM_REPLAY_CHUNK_CHARS: usize = 96;
const STREAM_REPLAY_DELAY_MS: u64 = 220;

#[async_trait::async_trait]
impl StreamSink for BufferedStreamSink {
    async fn on_delta(&mut self, delta: &str) -> anyhow::Result<()> {
        if !delta.is_empty() {
            self.deltas.push(delta.to_string());
        }
        Ok(())
    }
}

impl<'a> RecordingStreamSink<'a> {
    fn new(inner: &'a mut dyn StreamSink) -> Self {
        Self {
            inner,
            deltas: Vec::new(),
        }
    }

    fn resolved_text(&self, provider_reply: String) -> String {
        let streamed = self.deltas.concat();
        if streamed.trim().is_empty() {
            provider_reply
        } else {
            streamed
        }
    }
}

#[async_trait::async_trait]
impl StreamSink for RecordingStreamSink<'_> {
    async fn on_delta(&mut self, delta: &str) -> anyhow::Result<()> {
        if !delta.is_empty() {
            self.deltas.push(delta.to_string());
        }
        self.inner.on_delta(delta).await
    }
}

impl BufferedStreamSink {
    fn rendered_text(&self) -> String {
        self.deltas.concat()
    }

    async fn replay_text(text: &str, sink: &mut dyn StreamSink) -> anyhow::Result<()> {
        if text.is_empty() {
            return Ok(());
        }

        let chunks = chunk_text_for_stream_replay(text, STREAM_REPLAY_CHUNK_CHARS);
        for (idx, chunk) in chunks.iter().enumerate() {
            sink.on_delta(chunk).await?;
            if idx + 1 < chunks.len() {
                tokio::time::sleep(Duration::from_millis(STREAM_REPLAY_DELAY_MS)).await;
            }
        }
        Ok(())
    }
}

impl McpLoopTelemetry {
    fn new(discovered_tools: usize) -> Self {
        Self {
            started_at: Instant::now(),
            discovered_tools,
            iterations: 0,
            prompt_chars_total: 0,
            tool_calls_total: 0,
            tool_calls_ok: 0,
            tool_calls_err: 0,
        }
    }

    fn observe_prompt_chars(&mut self, history: &[StoredMessage]) {
        self.prompt_chars_total += history
            .iter()
            .map(|m| m.content.chars().count())
            .sum::<usize>();
    }

    fn emit(&self, stop_reason: &str) {
        info!(
            stop_reason,
            discovered_tools = self.discovered_tools,
            iterations = self.iterations,
            prompt_chars_total = self.prompt_chars_total,
            tool_calls_total = self.tool_calls_total,
            tool_calls_ok = self.tool_calls_ok,
            tool_calls_err = self.tool_calls_err,
            elapsed_ms = self.started_at.elapsed().as_millis(),
            "mcp loop baseline"
        );
    }
}

impl MessageService {
    pub(super) async fn complete_with_optional_mcp(
        &self,
        history: Vec<StoredMessage>,
        incoming: &IncomingMessage,
    ) -> anyhow::Result<CompletionOutcome> {
        let Some(runtime) = self.snapshot_mcp_runtime().await else {
            return self.complete_plain_provider(history, incoming).await;
        };
        let tools = match runtime.list_tools(None).await {
            Ok(tools) => tools,
            Err(err) => {
                warn!(error = %err, "failed to list mcp tools, fallback to plain completion");
                return self.complete_plain_provider(history, incoming).await;
            }
        };

        if tools.is_empty() {
            return self.complete_plain_provider(history, incoming).await;
        }

        if self.agent_code_mode.enabled
            && let Some(attempt) = self.next_code_mode_attempt()
        {
            let force_shadow = matches!(attempt, CodeModeAttempt::Probe);
            match self
                .complete_with_code_mode(
                    history.clone(),
                    &tools,
                    &runtime,
                    force_shadow,
                    incoming,
                    false,
                )
                .await
            {
                Ok(Some(reply)) => return Ok(CompletionOutcome::verified(reply.into_text())),
                Ok(None) => {}
                Err(err) => {
                    warn!(error = %err, "code mode path failed, fallback to mcp json loop");
                }
            }
        }

        self.complete_with_mcp_loop(history, tools, runtime, incoming)
            .await
            .map(CompletionOutcome::verified)
    }

    async fn complete_plain_provider(
        &self,
        history: Vec<StoredMessage>,
        incoming: &IncomingMessage,
    ) -> anyhow::Result<CompletionOutcome> {
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
        let request = CompletionRequest {
            messages: history,
            ..Default::default()
        };
        let reply = match self.complete_provider_for_run(&mut run, request).await {
            Ok(reply) => reply,
            Err(error) => {
                run.finish(AgentRunExit::InternalError).await;
                return Err(error).context("provider completion failed");
            }
        };
        run.finish(AgentRunExit::FinalAnswer(reply.clone())).await;
        Ok(CompletionOutcome::unverified(reply))
    }

    async fn complete_provider_for_run(
        &self,
        run: &mut AgentRun,
        request: CompletionRequest,
    ) -> anyhow::Result<String> {
        let reply = self.provider.complete(request.clone()).await?;
        run.record_provider_call(&request.messages, request.response_format.is_some(), &reply)
            .await;
        Ok(reply)
    }

    async fn complete_buffered_provider_for_run(
        &self,
        run: &mut AgentRun,
        request: CompletionRequest,
        sink: &mut BufferedStreamSink,
    ) -> anyhow::Result<String> {
        let provider_reply = self.provider.complete_stream(request.clone(), sink).await?;
        let resolved_reply = resolve_provider_stream_reply(provider_reply, sink);
        run.record_provider_call(
            &request.messages,
            request.response_format.is_some(),
            &resolved_reply,
        )
        .await;
        Ok(resolved_reply)
    }

    pub(super) async fn complete_with_optional_mcp_stream(
        &self,
        history: Vec<StoredMessage>,
        sink: &mut dyn StreamSink,
        incoming: &IncomingMessage,
    ) -> anyhow::Result<String> {
        let Some(runtime) = self.snapshot_mcp_runtime().await else {
            return self
                .complete_plain_provider_stream(history, sink, incoming)
                .await;
        };
        let tools = match runtime.list_tools(None).await {
            Ok(tools) => tools,
            Err(err) => {
                warn!(
                    error = %err,
                    "failed to list mcp tools, fallback to plain stream completion"
                );
                return self
                    .complete_plain_provider_stream(history, sink, incoming)
                    .await;
            }
        };

        if tools.is_empty() {
            return self
                .complete_plain_provider_stream(history, sink, incoming)
                .await;
        }

        if self.agent_code_mode.enabled
            && let Some(attempt) = self.next_code_mode_attempt()
        {
            let force_shadow = matches!(attempt, CodeModeAttempt::Probe);
            match self
                .complete_with_code_mode(
                    history.clone(),
                    &tools,
                    &runtime,
                    force_shadow,
                    incoming,
                    true,
                )
                .await
            {
                Ok(Some(CodeModeCompletion::PendingStream { text, mut run })) => {
                    if let Err(err) = BufferedStreamSink::replay_text(&text, sink).await {
                        run.finish(AgentRunExit::InternalError).await;
                        return Err(err);
                    }
                    run.finish(AgentRunExit::FinalAnswer(text.clone())).await;
                    return Ok(text);
                }
                Ok(Some(CodeModeCompletion::Finished(reply))) => {
                    if !reply.is_empty() {
                        BufferedStreamSink::replay_text(&reply, sink).await?;
                    }
                    return Ok(reply);
                }
                Ok(None) => {}
                Err(err) => {
                    warn!(error = %err, "code mode path failed, fallback to mcp json loop");
                }
            }
        }

        self.complete_with_mcp_loop_stream(history, tools, runtime, sink, incoming)
            .await
    }

    async fn complete_plain_provider_stream(
        &self,
        history: Vec<StoredMessage>,
        sink: &mut dyn StreamSink,
        incoming: &IncomingMessage,
    ) -> anyhow::Result<String> {
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
        let request = CompletionRequest {
            messages: history.clone(),
            ..Default::default()
        };
        if matches!(self.output_verification_mode, OutputVerificationMode::Off) {
            let mut recording_sink = RecordingStreamSink::new(sink);
            let provider_reply = match self
                .provider
                .complete_stream(request.clone(), &mut recording_sink)
                .await
            {
                Ok(reply) => reply,
                Err(error) => {
                    run.finish(AgentRunExit::InternalError).await;
                    return Err(error).context("provider stream completion failed");
                }
            };
            let resolved_reply = recording_sink.resolved_text(provider_reply);
            run.record_provider_call(
                &request.messages,
                request.response_format.is_some(),
                &resolved_reply,
            )
            .await;
            run.finish(AgentRunExit::FinalAnswer(resolved_reply.clone()))
                .await;
            return Ok(resolved_reply);
        }

        let mut buffered_sink = BufferedStreamSink::default();
        let reply = match self
            .provider
            .complete_stream(request.clone(), &mut buffered_sink)
            .await
        {
            Ok(reply) => reply,
            Err(error) => {
                run.finish(AgentRunExit::InternalError).await;
                return Err(error).context("provider stream completion failed");
            }
        };
        let resolved_reply = resolve_provider_stream_reply(reply, &buffered_sink);
        run.record_provider_call(
            &request.messages,
            request.response_format.is_some(),
            &resolved_reply,
        )
        .await;
        let resolved_reply = match self
            .verify_final_answer(&history, &incoming.channel, resolved_reply, &[])
            .await
        {
            Ok(reply) => reply,
            Err(error) => {
                run.finish(AgentRunExit::InternalError).await;
                return Err(error).context("output verification failed");
            }
        };
        if let Err(error) = BufferedStreamSink::replay_text(&resolved_reply, sink).await {
            run.finish(AgentRunExit::InternalError).await;
            return Err(error);
        }
        run.finish(AgentRunExit::FinalAnswer(resolved_reply.clone()))
            .await;
        Ok(resolved_reply)
    }

    fn is_mcp_agent_enabled(&self) -> bool {
        self.agent_mcp.enabled && self.mcp_runtime.is_some()
    }

    pub(super) async fn snapshot_mcp_runtime(&self) -> Option<McpRuntime> {
        if !self.is_mcp_agent_enabled() {
            return None;
        }
        let runtime = self.mcp_runtime.as_ref()?;
        Some(runtime.read().await.clone())
    }

    pub(super) fn is_code_mode_timeout_circuit_open(&self) -> bool {
        self.code_mode_timeout_circuit_open.load(Ordering::Relaxed)
    }

    pub(super) fn record_code_mode_counters(
        &self,
        record: &CodeModeAuditRecord,
        force_shadow: bool,
        circuit_change: CodeModeCircuitChange,
    ) {
        self.code_mode_attempts_total
            .fetch_add(1, Ordering::Relaxed);
        if record.used {
            self.code_mode_used_total.fetch_add(1, Ordering::Relaxed);
        }
        if record.fallback {
            self.code_mode_fallback_total
                .fetch_add(1, Ordering::Relaxed);
        }
        if record.timed_out_calls > 0 {
            self.code_mode_timed_out_calls_total
                .fetch_add(record.timed_out_calls, Ordering::Relaxed);
        }
        if record.failed_calls > 0 {
            self.code_mode_failed_calls_total
                .fetch_add(record.failed_calls, Ordering::Relaxed);
        }
        if force_shadow {
            self.code_mode_probe_attempt_total
                .fetch_add(1, Ordering::Relaxed);
        }
        match circuit_change {
            CodeModeCircuitChange::Opened => {
                self.code_mode_circuit_open_total
                    .fetch_add(1, Ordering::Relaxed);
            }
            CodeModeCircuitChange::Closed => {
                self.code_mode_circuit_close_total
                    .fetch_add(1, Ordering::Relaxed);
            }
            CodeModeCircuitChange::None => {}
        }
    }

    pub async fn reload_mcp_runtime_from_registry(
        &self,
        registry: &crate::mcp::McpRegistry,
    ) -> anyhow::Result<()> {
        let Some(runtime) = &self.mcp_runtime else {
            return Ok(());
        };
        let next = McpRuntime::from_registry(registry).await?;
        let mut guard = runtime.write().await;
        *guard = next;
        Ok(())
    }
}

fn resolve_provider_stream_reply(
    provider_reply: String,
    buffered_sink: &BufferedStreamSink,
) -> String {
    let streamed_reply = buffered_sink.rendered_text();
    if streamed_reply.trim().is_empty() {
        provider_reply
    } else {
        streamed_reply
    }
}

async fn replay_text_or_finish_internal_error(
    run: &mut AgentRun,
    text: &str,
    sink: &mut dyn StreamSink,
) -> anyhow::Result<()> {
    if let Err(err) = BufferedStreamSink::replay_text(text, sink).await {
        run.finish(AgentRunExit::InternalError).await;
        return Err(err);
    }
    Ok(())
}

pub(super) fn chunk_text_for_stream_replay(text: &str, max_chars: usize) -> Vec<String> {
    let limit = max_chars.max(1);
    if text.is_empty() {
        return Vec::new();
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0usize;
    for ch in text.chars() {
        current.push(ch);
        current_chars += 1;
        if current_chars >= limit {
            chunks.push(current);
            current = String::new();
            current_chars = 0;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn warn_verification_failure(verification: &VerificationResult) {
    let issues = verification
        .issues
        .iter()
        .map(verification_issue_summary)
        .collect::<Vec<_>>();
    warn!(?issues, "Tool call verification failed");
}

fn verification_issue_summary(issue: &VerificationIssue) -> String {
    format!(
        "{}:{}:{}",
        issue.severity.as_str(),
        issue.code,
        issue.message
    )
}
