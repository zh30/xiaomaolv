use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::code_mode::{
    AgentCodeModeSettings, CodeModeAuditRecord, CodeModeExecutionMode, CodeModeExecutor,
    CodeModePlanner, DisabledCodeModePlanner, execute_plan_via_subprocess,
};
use crate::domain::{IncomingMessage, MessageRole, OutgoingMessage, StoredMessage};
use crate::mcp::{McpRuntime, McpToolInfo};
use crate::memory::{
    ClaimDueTelegramSchedulerJobsRequest, CompleteTelegramSchedulerJobRunRequest,
    CreateTelegramSchedulerJobRequest, FailTelegramSchedulerJobRunRequest, GroupAliasLoadRequest,
    GroupAliasUpsertRequest, GroupUserProfileLoadRequest, GroupUserProfileRecord,
    GroupUserProfileUpsertRequest, MemoryBackend, MemoryContextRequest, MemoryWriteRequest,
    SqliteMemoryBackend, SqliteMemoryStore, TelegramSchedulerJobListRequest,
    TelegramSchedulerJobRecord, TelegramSchedulerJobStatus, TelegramSchedulerPendingIntentRecord,
    TelegramSchedulerStats, TelegramSchedulerStatsRequest, UpdateTelegramSchedulerJobStatusRequest,
    UpsertTelegramSchedulerPendingIntentRequest,
};
use crate::provider::{ChatProvider, CompletionRequest, StreamSink};
use crate::skills::{SkillRegistry, SkillRuntime, SkillRuntimeSelectionSettings};

#[derive(Debug, Clone)]
pub struct AgentMcpSettings {
    pub enabled: bool,
    pub max_iterations: usize,
    pub max_tool_result_chars: usize,
}

#[derive(Debug, Clone)]
pub struct AgentSkillsSettings {
    pub enabled: bool,
    pub max_selected: usize,
    pub max_prompt_chars: usize,
    pub match_min_score: f32,
    pub llm_rerank_enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CodeModeDiagnostics {
    pub policy: CodeModeDiagnosticsPolicy,
    pub runtime: CodeModeDiagnosticsRuntime,
}

#[derive(Debug, Clone, Serialize)]
pub struct CodeModeDiagnosticsPolicy {
    pub enabled: bool,
    pub shadow_mode: bool,
    pub execution_mode: CodeModeExecutionMode,
    pub timeout_warn_ratio: f64,
    pub timeout_auto_shadow_enabled: bool,
    pub timeout_auto_shadow_streak: usize,
    pub timeout_auto_shadow_probe_every: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct CodeModeDiagnosticsRuntime {
    pub circuit_open: bool,
    pub timeout_alert_streak: usize,
    pub probe_counter: usize,
    pub counters: CodeModeDiagnosticsCounters,
}

#[derive(Debug, Clone, Serialize)]
pub struct CodeModeDiagnosticsCounters {
    pub attempts_total: usize,
    pub used_total: usize,
    pub fallback_total: usize,
    pub timed_out_calls_total: usize,
    pub failed_calls_total: usize,
    pub probe_attempt_total: usize,
    pub circuit_open_total: usize,
    pub circuit_close_total: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TelegramSchedulerIntent {
    pub action: String,
    pub confidence: f32,
    pub task_kind: Option<String>,
    pub payload: Option<String>,
    pub schedule_kind: Option<String>,
    pub run_at: Option<String>,
    pub cron_expr: Option<String>,
    pub timezone: Option<String>,
    pub job_id: Option<String>,
    pub job_operation: Option<String>,
}

impl Default for AgentMcpSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_iterations: 4,
            max_tool_result_chars: 4000,
        }
    }
}

impl Default for AgentSkillsSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_selected: 3,
            max_prompt_chars: 8000,
            match_min_score: 0.45,
            llm_rerank_enabled: false,
        }
    }
}

#[derive(Clone)]
pub struct MessageService {
    provider: Arc<dyn ChatProvider>,
    memory: Arc<dyn MemoryBackend>,
    mcp_runtime: Option<Arc<RwLock<McpRuntime>>>,
    code_mode_planner: Arc<dyn CodeModePlanner>,
    skills_runtime: Option<Arc<RwLock<SkillRuntime>>>,
    agent_mcp: AgentMcpSettings,
    agent_code_mode: AgentCodeModeSettings,
    agent_skills: AgentSkillsSettings,
    code_mode_timeout_alert_streak: Arc<AtomicUsize>,
    code_mode_timeout_circuit_open: Arc<AtomicBool>,
    code_mode_timeout_probe_counter: Arc<AtomicUsize>,
    code_mode_attempts_total: Arc<AtomicUsize>,
    code_mode_used_total: Arc<AtomicUsize>,
    code_mode_fallback_total: Arc<AtomicUsize>,
    code_mode_timed_out_calls_total: Arc<AtomicUsize>,
    code_mode_failed_calls_total: Arc<AtomicUsize>,
    code_mode_probe_attempt_total: Arc<AtomicUsize>,
    code_mode_circuit_open_total: Arc<AtomicUsize>,
    code_mode_circuit_close_total: Arc<AtomicUsize>,
    max_recent_turns: usize,
    max_semantic_memories: usize,
    semantic_lookback_days: u32,
    context_window_tokens: usize,
    context_reserved_tokens: usize,
    context_memory_budget_ratio: u8,
    context_min_recent_messages: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodeModeAttempt {
    Normal,
    Probe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodeModeCircuitChange {
    None,
    Opened,
    Closed,
}

impl MessageService {
    pub fn new(
        provider: Arc<dyn ChatProvider>,
        store: SqliteMemoryStore,
        max_history: usize,
    ) -> Self {
        Self::new_with_backend(
            provider,
            Arc::new(SqliteMemoryBackend::new(store)),
            None,
            AgentMcpSettings::default(),
            max_history,
            0,
            0,
        )
    }

    pub fn new_with_backend(
        provider: Arc<dyn ChatProvider>,
        memory: Arc<dyn MemoryBackend>,
        mcp_runtime: Option<Arc<RwLock<McpRuntime>>>,
        agent_mcp: AgentMcpSettings,
        max_recent_turns: usize,
        max_semantic_memories: usize,
        semantic_lookback_days: u32,
    ) -> Self {
        Self {
            provider,
            memory,
            mcp_runtime,
            code_mode_planner: Arc::new(DisabledCodeModePlanner),
            skills_runtime: None,
            agent_mcp,
            agent_code_mode: AgentCodeModeSettings::default(),
            agent_skills: AgentSkillsSettings::default(),
            code_mode_timeout_alert_streak: Arc::new(AtomicUsize::new(0)),
            code_mode_timeout_circuit_open: Arc::new(AtomicBool::new(false)),
            code_mode_timeout_probe_counter: Arc::new(AtomicUsize::new(0)),
            code_mode_attempts_total: Arc::new(AtomicUsize::new(0)),
            code_mode_used_total: Arc::new(AtomicUsize::new(0)),
            code_mode_fallback_total: Arc::new(AtomicUsize::new(0)),
            code_mode_timed_out_calls_total: Arc::new(AtomicUsize::new(0)),
            code_mode_failed_calls_total: Arc::new(AtomicUsize::new(0)),
            code_mode_probe_attempt_total: Arc::new(AtomicUsize::new(0)),
            code_mode_circuit_open_total: Arc::new(AtomicUsize::new(0)),
            code_mode_circuit_close_total: Arc::new(AtomicUsize::new(0)),
            max_recent_turns,
            max_semantic_memories,
            semantic_lookback_days,
            context_window_tokens: 200_000,
            context_reserved_tokens: 8_192,
            context_memory_budget_ratio: 35,
            context_min_recent_messages: 8,
        }
    }

    pub fn with_context_budget(
        mut self,
        window_tokens: usize,
        reserved_tokens: usize,
        memory_budget_ratio: u8,
        min_recent_messages: usize,
    ) -> Self {
        self.context_window_tokens = window_tokens;
        self.context_reserved_tokens = reserved_tokens;
        self.context_memory_budget_ratio = memory_budget_ratio;
        self.context_min_recent_messages = min_recent_messages;
        self
    }

    pub fn with_agent_code_mode(mut self, settings: AgentCodeModeSettings) -> Self {
        self.agent_code_mode = settings;
        self.code_mode_timeout_alert_streak
            .store(0, Ordering::Relaxed);
        self.code_mode_timeout_circuit_open
            .store(false, Ordering::Relaxed);
        self.code_mode_timeout_probe_counter
            .store(0, Ordering::Relaxed);
        self.code_mode_attempts_total.store(0, Ordering::Relaxed);
        self.code_mode_used_total.store(0, Ordering::Relaxed);
        self.code_mode_fallback_total.store(0, Ordering::Relaxed);
        self.code_mode_timed_out_calls_total
            .store(0, Ordering::Relaxed);
        self.code_mode_failed_calls_total
            .store(0, Ordering::Relaxed);
        self.code_mode_probe_attempt_total
            .store(0, Ordering::Relaxed);
        self.code_mode_circuit_open_total
            .store(0, Ordering::Relaxed);
        self.code_mode_circuit_close_total
            .store(0, Ordering::Relaxed);
        self
    }

    pub fn with_code_mode_planner(mut self, planner: Arc<dyn CodeModePlanner>) -> Self {
        self.code_mode_planner = planner;
        self
    }

    pub fn code_mode_diagnostics(&self) -> CodeModeDiagnostics {
        CodeModeDiagnostics {
            policy: CodeModeDiagnosticsPolicy {
                enabled: self.agent_code_mode.enabled,
                shadow_mode: self.agent_code_mode.shadow_mode,
                execution_mode: self.agent_code_mode.execution_mode.clone(),
                timeout_warn_ratio: self.agent_code_mode.normalized_timeout_warn_ratio(),
                timeout_auto_shadow_enabled: self.agent_code_mode.timeout_auto_shadow_enabled,
                timeout_auto_shadow_streak: self.agent_code_mode.timeout_auto_shadow_streak.max(1),
                timeout_auto_shadow_probe_every: self
                    .agent_code_mode
                    .timeout_auto_shadow_probe_every
                    .max(1),
            },
            runtime: CodeModeDiagnosticsRuntime {
                circuit_open: self.is_code_mode_timeout_circuit_open(),
                timeout_alert_streak: self.code_mode_timeout_alert_streak.load(Ordering::Relaxed),
                probe_counter: self.code_mode_timeout_probe_counter.load(Ordering::Relaxed),
                counters: CodeModeDiagnosticsCounters {
                    attempts_total: self.code_mode_attempts_total.load(Ordering::Relaxed),
                    used_total: self.code_mode_used_total.load(Ordering::Relaxed),
                    fallback_total: self.code_mode_fallback_total.load(Ordering::Relaxed),
                    timed_out_calls_total: self
                        .code_mode_timed_out_calls_total
                        .load(Ordering::Relaxed),
                    failed_calls_total: self.code_mode_failed_calls_total.load(Ordering::Relaxed),
                    probe_attempt_total: self.code_mode_probe_attempt_total.load(Ordering::Relaxed),
                    circuit_open_total: self.code_mode_circuit_open_total.load(Ordering::Relaxed),
                    circuit_close_total: self.code_mode_circuit_close_total.load(Ordering::Relaxed),
                },
            },
        }
    }

    pub fn code_mode_metrics_prometheus(&self) -> String {
        let diag = self.code_mode_diagnostics();
        let counters = &diag.runtime.counters;
        let mut out = String::new();

        let _ = writeln!(
            out,
            "# HELP xiaomaolv_code_mode_attempts_total Total code mode attempts."
        );
        let _ = writeln!(out, "# TYPE xiaomaolv_code_mode_attempts_total counter");
        let _ = writeln!(
            out,
            "xiaomaolv_code_mode_attempts_total {}",
            counters.attempts_total
        );

        let _ = writeln!(
            out,
            "# HELP xiaomaolv_code_mode_used_total Total code mode attempts that were used directly."
        );
        let _ = writeln!(out, "# TYPE xiaomaolv_code_mode_used_total counter");
        let _ = writeln!(
            out,
            "xiaomaolv_code_mode_used_total {}",
            counters.used_total
        );

        let _ = writeln!(
            out,
            "# HELP xiaomaolv_code_mode_fallback_total Total code mode attempts that fell back."
        );
        let _ = writeln!(out, "# TYPE xiaomaolv_code_mode_fallback_total counter");
        let _ = writeln!(
            out,
            "xiaomaolv_code_mode_fallback_total {}",
            counters.fallback_total
        );

        let _ = writeln!(
            out,
            "# HELP xiaomaolv_code_mode_timed_out_calls_total Total timed out tool calls in code mode."
        );
        let _ = writeln!(
            out,
            "# TYPE xiaomaolv_code_mode_timed_out_calls_total counter"
        );
        let _ = writeln!(
            out,
            "xiaomaolv_code_mode_timed_out_calls_total {}",
            counters.timed_out_calls_total
        );

        let _ = writeln!(
            out,
            "# HELP xiaomaolv_code_mode_failed_calls_total Total failed tool calls in code mode."
        );
        let _ = writeln!(out, "# TYPE xiaomaolv_code_mode_failed_calls_total counter");
        let _ = writeln!(
            out,
            "xiaomaolv_code_mode_failed_calls_total {}",
            counters.failed_calls_total
        );

        let _ = writeln!(
            out,
            "# HELP xiaomaolv_code_mode_probe_attempt_total Total probe attempts when timeout circuit is open."
        );
        let _ = writeln!(
            out,
            "# TYPE xiaomaolv_code_mode_probe_attempt_total counter"
        );
        let _ = writeln!(
            out,
            "xiaomaolv_code_mode_probe_attempt_total {}",
            counters.probe_attempt_total
        );

        let _ = writeln!(
            out,
            "# HELP xiaomaolv_code_mode_circuit_open_total Total times timeout circuit opened."
        );
        let _ = writeln!(out, "# TYPE xiaomaolv_code_mode_circuit_open_total counter");
        let _ = writeln!(
            out,
            "xiaomaolv_code_mode_circuit_open_total {}",
            counters.circuit_open_total
        );

        let _ = writeln!(
            out,
            "# HELP xiaomaolv_code_mode_circuit_close_total Total times timeout circuit closed."
        );
        let _ = writeln!(
            out,
            "# TYPE xiaomaolv_code_mode_circuit_close_total counter"
        );
        let _ = writeln!(
            out,
            "xiaomaolv_code_mode_circuit_close_total {}",
            counters.circuit_close_total
        );

        let _ = writeln!(
            out,
            "# HELP xiaomaolv_code_mode_circuit_open Current timeout circuit open state (1=open, 0=closed)."
        );
        let _ = writeln!(out, "# TYPE xiaomaolv_code_mode_circuit_open gauge");
        let _ = writeln!(
            out,
            "xiaomaolv_code_mode_circuit_open {}",
            if diag.runtime.circuit_open { 1 } else { 0 }
        );

        let _ = writeln!(
            out,
            "# HELP xiaomaolv_code_mode_timeout_alert_streak Current timeout alert streak."
        );
        let _ = writeln!(out, "# TYPE xiaomaolv_code_mode_timeout_alert_streak gauge");
        let _ = writeln!(
            out,
            "xiaomaolv_code_mode_timeout_alert_streak {}",
            diag.runtime.timeout_alert_streak
        );

        let _ = writeln!(
            out,
            "# HELP xiaomaolv_code_mode_probe_counter Current probe counter while circuit is open."
        );
        let _ = writeln!(out, "# TYPE xiaomaolv_code_mode_probe_counter gauge");
        let _ = writeln!(
            out,
            "xiaomaolv_code_mode_probe_counter {}",
            diag.runtime.probe_counter
        );

        let _ = writeln!(
            out,
            "# HELP xiaomaolv_code_mode_enabled Whether code mode is enabled (1=yes, 0=no)."
        );
        let _ = writeln!(out, "# TYPE xiaomaolv_code_mode_enabled gauge");
        let _ = writeln!(
            out,
            "xiaomaolv_code_mode_enabled {}",
            if diag.policy.enabled { 1 } else { 0 }
        );

        let _ = writeln!(
            out,
            "# HELP xiaomaolv_code_mode_shadow_mode Whether code mode shadow mode is enabled (1=yes, 0=no)."
        );
        let _ = writeln!(out, "# TYPE xiaomaolv_code_mode_shadow_mode gauge");
        let _ = writeln!(
            out,
            "xiaomaolv_code_mode_shadow_mode {}",
            if diag.policy.shadow_mode { 1 } else { 0 }
        );

        let _ = writeln!(
            out,
            "# HELP xiaomaolv_code_mode_timeout_warn_ratio Timeout warning ratio threshold."
        );
        let _ = writeln!(out, "# TYPE xiaomaolv_code_mode_timeout_warn_ratio gauge");
        let _ = writeln!(
            out,
            "xiaomaolv_code_mode_timeout_warn_ratio {:.6}",
            diag.policy.timeout_warn_ratio
        );

        let _ = writeln!(
            out,
            "# HELP xiaomaolv_code_mode_timeout_auto_shadow_enabled Whether timeout auto shadow is enabled (1=yes, 0=no)."
        );
        let _ = writeln!(
            out,
            "# TYPE xiaomaolv_code_mode_timeout_auto_shadow_enabled gauge"
        );
        let _ = writeln!(
            out,
            "xiaomaolv_code_mode_timeout_auto_shadow_enabled {}",
            if diag.policy.timeout_auto_shadow_enabled {
                1
            } else {
                0
            }
        );

        let _ = writeln!(
            out,
            "# HELP xiaomaolv_code_mode_timeout_auto_shadow_streak Timeout auto shadow streak threshold."
        );
        let _ = writeln!(
            out,
            "# TYPE xiaomaolv_code_mode_timeout_auto_shadow_streak gauge"
        );
        let _ = writeln!(
            out,
            "xiaomaolv_code_mode_timeout_auto_shadow_streak {}",
            diag.policy.timeout_auto_shadow_streak
        );

        let _ = writeln!(
            out,
            "# HELP xiaomaolv_code_mode_timeout_auto_shadow_probe_every Probe interval when timeout circuit is open."
        );
        let _ = writeln!(
            out,
            "# TYPE xiaomaolv_code_mode_timeout_auto_shadow_probe_every gauge"
        );
        let _ = writeln!(
            out,
            "xiaomaolv_code_mode_timeout_auto_shadow_probe_every {}",
            diag.policy.timeout_auto_shadow_probe_every
        );

        out
    }

    pub async fn handle(&self, incoming: IncomingMessage) -> anyhow::Result<OutgoingMessage> {
        self.memory
            .append(MemoryWriteRequest {
                session_id: incoming.session_id.clone(),
                user_id: incoming.user_id.clone(),
                channel: incoming.channel.clone(),
                message: StoredMessage {
                    role: MessageRole::User,
                    content: incoming.text.clone(),
                },
            })
            .await
            .context("failed to persist user message")?;

        let history = self
            .memory
            .load_context(MemoryContextRequest {
                session_id: incoming.session_id.clone(),
                user_id: incoming.user_id.clone(),
                channel: incoming.channel.clone(),
                query_text: incoming.text.clone(),
                max_recent_turns: self.max_recent_turns,
                max_semantic_memories: self.max_semantic_memories,
                semantic_lookback_days: self.semantic_lookback_days,
            })
            .await
            .context("failed to load history")?;
        let history = apply_context_budget(
            history,
            self.context_window_tokens,
            self.context_reserved_tokens,
            self.context_memory_budget_ratio,
            self.context_min_recent_messages,
        );
        let history = self.apply_skills_prompt(history, &incoming.text).await;

        let text = self.complete_with_optional_mcp(history).await?;

        self.memory
            .append(MemoryWriteRequest {
                session_id: incoming.session_id.clone(),
                user_id: incoming.user_id.clone(),
                channel: incoming.channel.clone(),
                message: StoredMessage {
                    role: MessageRole::Assistant,
                    content: text.clone(),
                },
            })
            .await
            .context("failed to persist assistant message")?;

        Ok(OutgoingMessage {
            channel: incoming.channel,
            session_id: incoming.session_id,
            text,
            reply_target: incoming.reply_target,
        })
    }

    pub async fn handle_stream(
        &self,
        incoming: IncomingMessage,
        sink: &mut dyn StreamSink,
    ) -> anyhow::Result<OutgoingMessage> {
        self.memory
            .append(MemoryWriteRequest {
                session_id: incoming.session_id.clone(),
                user_id: incoming.user_id.clone(),
                channel: incoming.channel.clone(),
                message: StoredMessage {
                    role: MessageRole::User,
                    content: incoming.text.clone(),
                },
            })
            .await
            .context("failed to persist user message")?;

        let history = self
            .memory
            .load_context(MemoryContextRequest {
                session_id: incoming.session_id.clone(),
                user_id: incoming.user_id.clone(),
                channel: incoming.channel.clone(),
                query_text: incoming.text.clone(),
                max_recent_turns: self.max_recent_turns,
                max_semantic_memories: self.max_semantic_memories,
                semantic_lookback_days: self.semantic_lookback_days,
            })
            .await
            .context("failed to load history")?;
        let history = apply_context_budget(
            history,
            self.context_window_tokens,
            self.context_reserved_tokens,
            self.context_memory_budget_ratio,
            self.context_min_recent_messages,
        );
        let history = self.apply_skills_prompt(history, &incoming.text).await;

        // Keep true streaming semantics on channel streaming path.
        // MCP tool loop currently runs on non-stream path only.
        let text = self
            .provider
            .complete_stream(CompletionRequest { messages: history }, sink)
            .await
            .context("provider stream completion failed")?;

        self.memory
            .append(MemoryWriteRequest {
                session_id: incoming.session_id.clone(),
                user_id: incoming.user_id.clone(),
                channel: incoming.channel.clone(),
                message: StoredMessage {
                    role: MessageRole::Assistant,
                    content: text.clone(),
                },
            })
            .await
            .context("failed to persist assistant message")?;

        Ok(OutgoingMessage {
            channel: incoming.channel,
            session_id: incoming.session_id,
            text,
            reply_target: incoming.reply_target,
        })
    }

    pub async fn observe(&self, incoming: IncomingMessage) -> anyhow::Result<()> {
        self.memory
            .append(MemoryWriteRequest {
                session_id: incoming.session_id,
                user_id: incoming.user_id,
                channel: incoming.channel,
                message: StoredMessage {
                    role: MessageRole::User,
                    content: incoming.text,
                },
            })
            .await
            .context("failed to persist observed user message")
    }

    pub async fn upsert_group_aliases(
        &self,
        channel: String,
        chat_id: i64,
        aliases: Vec<String>,
    ) -> anyhow::Result<()> {
        if aliases.is_empty() {
            return Ok(());
        }
        self.memory
            .upsert_group_aliases(GroupAliasUpsertRequest {
                channel,
                chat_id,
                aliases,
            })
            .await
            .context("failed to persist telegram group aliases")
    }

    pub async fn load_group_aliases(
        &self,
        channel: String,
        chat_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<String>> {
        self.memory
            .load_group_aliases(GroupAliasLoadRequest {
                channel,
                chat_id,
                limit,
            })
            .await
            .context("failed to load telegram group aliases")
    }

    pub async fn upsert_group_user_profile(
        &self,
        channel: String,
        chat_id: i64,
        user_id: i64,
        preferred_name: String,
        username: Option<String>,
    ) -> anyhow::Result<()> {
        if preferred_name.trim().is_empty() {
            return Ok(());
        }
        self.memory
            .upsert_group_user_profile(GroupUserProfileUpsertRequest {
                channel,
                chat_id,
                user_id,
                preferred_name,
                username,
            })
            .await
            .context("failed to persist telegram group user profile")
    }

    pub async fn load_group_user_profiles(
        &self,
        channel: String,
        chat_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<GroupUserProfileRecord>> {
        self.memory
            .load_group_user_profiles(GroupUserProfileLoadRequest {
                channel,
                chat_id,
                limit,
            })
            .await
            .context("failed to load telegram group user profiles")
    }

    pub async fn create_telegram_scheduler_job(
        &self,
        req: CreateTelegramSchedulerJobRequest,
    ) -> anyhow::Result<()> {
        self.memory
            .create_telegram_scheduler_job(req)
            .await
            .context("failed to create telegram scheduler job")
    }

    pub async fn list_telegram_scheduler_jobs_by_owner(
        &self,
        channel: String,
        chat_id: i64,
        owner_user_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<TelegramSchedulerJobRecord>> {
        self.memory
            .list_telegram_scheduler_jobs_by_owner(TelegramSchedulerJobListRequest {
                channel,
                chat_id,
                owner_user_id,
                limit,
            })
            .await
            .context("failed to list telegram scheduler jobs")
    }

    pub async fn query_telegram_scheduler_stats(
        &self,
        channel: String,
        now_unix: i64,
    ) -> anyhow::Result<TelegramSchedulerStats> {
        self.memory
            .query_telegram_scheduler_stats(TelegramSchedulerStatsRequest { channel, now_unix })
            .await
            .context("failed to query telegram scheduler stats")
    }

    pub async fn load_telegram_scheduler_job(
        &self,
        channel: String,
        job_id: String,
    ) -> anyhow::Result<Option<TelegramSchedulerJobRecord>> {
        self.memory
            .load_telegram_scheduler_job(channel, job_id)
            .await
            .context("failed to load telegram scheduler job")
    }

    pub async fn update_telegram_scheduler_job_status(
        &self,
        channel: String,
        chat_id: i64,
        owner_user_id: i64,
        job_id: String,
        status: TelegramSchedulerJobStatus,
    ) -> anyhow::Result<bool> {
        self.memory
            .update_telegram_scheduler_job_status(UpdateTelegramSchedulerJobStatusRequest {
                channel,
                chat_id,
                owner_user_id,
                job_id,
                status,
            })
            .await
            .context("failed to update telegram scheduler job status")
    }

    pub async fn claim_due_telegram_scheduler_jobs(
        &self,
        channel: String,
        now_unix: i64,
        limit: usize,
        lease_secs: i64,
        lease_token: String,
    ) -> anyhow::Result<Vec<TelegramSchedulerJobRecord>> {
        self.memory
            .claim_due_telegram_scheduler_jobs(ClaimDueTelegramSchedulerJobsRequest {
                channel,
                now_unix,
                limit,
                lease_secs,
                lease_token,
            })
            .await
            .context("failed to claim due telegram scheduler jobs")
    }

    pub async fn complete_telegram_scheduler_job_run(
        &self,
        req: CompleteTelegramSchedulerJobRunRequest,
    ) -> anyhow::Result<()> {
        self.memory
            .complete_telegram_scheduler_job_run(req)
            .await
            .context("failed to complete telegram scheduler job run")
    }

    pub async fn fail_telegram_scheduler_job_run(
        &self,
        req: FailTelegramSchedulerJobRunRequest,
    ) -> anyhow::Result<()> {
        self.memory
            .fail_telegram_scheduler_job_run(req)
            .await
            .context("failed to fail telegram scheduler job run")
    }

    pub async fn upsert_telegram_scheduler_pending_intent(
        &self,
        req: UpsertTelegramSchedulerPendingIntentRequest,
    ) -> anyhow::Result<()> {
        self.memory
            .upsert_telegram_scheduler_pending_intent(req)
            .await
            .context("failed to upsert telegram scheduler pending intent")
    }

    pub async fn load_telegram_scheduler_pending_intent(
        &self,
        channel: String,
        chat_id: i64,
        owner_user_id: i64,
        now_unix: i64,
    ) -> anyhow::Result<Option<TelegramSchedulerPendingIntentRecord>> {
        self.memory
            .load_telegram_scheduler_pending_intent(channel, chat_id, owner_user_id, now_unix)
            .await
            .context("failed to load telegram scheduler pending intent")
    }

    pub async fn delete_telegram_scheduler_pending_intent(
        &self,
        channel: String,
        chat_id: i64,
        owner_user_id: i64,
    ) -> anyhow::Result<bool> {
        self.memory
            .delete_telegram_scheduler_pending_intent(channel, chat_id, owner_user_id)
            .await
            .context("failed to delete telegram scheduler pending intent")
    }

    pub async fn detect_telegram_scheduler_intent(
        &self,
        text: &str,
        timezone: &str,
        now_unix: i64,
        pending_draft_json: Option<&str>,
    ) -> anyhow::Result<Option<TelegramSchedulerIntent>> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(None);
        }

        let mut input =
            format!("当前时间戳(now_unix): {now_unix}\n默认时区: {timezone}\n用户输入: {text}");
        if let Some(draft) = pending_draft_json
            && !draft.trim().is_empty()
        {
            input.push_str("\n待确认草案(JSON): ");
            input.push_str(draft.trim());
        }

        let reply = self
            .provider
            .complete(CompletionRequest {
                messages: vec![
                    StoredMessage {
                        role: MessageRole::System,
                        content: [
                            "你是 Telegram 定时任务意图解析器。",
                            "只输出 JSON，不要 Markdown，不要解释。",
                            "JSON schema:",
                            "{\"action\":\"create|update|delete|pause|resume|cancel|list|none\",\"confidence\":0..1,\"task_kind\":\"reminder|agent|null\",\"payload\":\"string|null\",\"schedule_kind\":\"once|cron|null\",\"run_at\":\"RFC3339或unix秒字符串|null\",\"cron_expr\":\"string|null\",\"timezone\":\"IANA时区或null\",\"job_id\":\"string|null\",\"job_operation\":\"delete|pause|resume|null\"}",
                            "规则:",
                            "1) 若不是定时任务意图，action=none, confidence<=0.4",
                            "2) 若有明确时间并要求提醒，优先 action=create",
                            "3) 对修改已存在草案可输出 action=update",
                            "4) 若表达暂停/恢复/删除已存在任务，输出 action=pause|resume|delete，并尽量给出 job_id",
                            "4) 只返回一个合法 JSON 对象",
                        ]
                        .join("\n"),
                    },
                    StoredMessage {
                        role: MessageRole::User,
                        content: input,
                    },
                ],
            })
            .await
            .context("failed to detect telegram scheduler intent")?;

        Ok(parse_scheduler_intent_json(&reply))
    }

    async fn complete_with_optional_mcp(
        &self,
        history: Vec<StoredMessage>,
    ) -> anyhow::Result<String> {
        let Some(runtime) = self.snapshot_mcp_runtime().await else {
            return self
                .provider
                .complete(CompletionRequest { messages: history })
                .await
                .context("provider completion failed");
        };
        let tools = match runtime.list_tools(None).await {
            Ok(tools) => tools,
            Err(err) => {
                warn!(error = %err, "failed to list mcp tools, fallback to plain completion");
                return self
                    .provider
                    .complete(CompletionRequest { messages: history })
                    .await
                    .context("provider completion failed");
            }
        };

        if tools.is_empty() {
            return self
                .provider
                .complete(CompletionRequest { messages: history })
                .await
                .context("provider completion failed");
        }

        if self.agent_code_mode.enabled
            && let Some(attempt) = self.next_code_mode_attempt()
        {
            let force_shadow = matches!(attempt, CodeModeAttempt::Probe);
            match self
                .complete_with_code_mode(history.clone(), &tools, &runtime, force_shadow)
                .await
            {
                Ok(Some(reply)) => return Ok(reply),
                Ok(None) => {}
                Err(err) => {
                    warn!(error = %err, "code mode path failed, fallback to mcp json loop");
                }
            }
        }

        self.complete_with_mcp_loop(history, tools, runtime).await
    }

    async fn complete_with_code_mode(
        &self,
        history: Vec<StoredMessage>,
        tools: &[McpToolInfo],
        runtime: &McpRuntime,
        force_shadow: bool,
    ) -> anyhow::Result<Option<String>> {
        let started_at = Instant::now();
        let planner = self.code_mode_planner.clone();
        let planner_name = planner.name();
        let plan = planner.build_plan(&history, tools).await?;
        let Some(plan) = plan else {
            let audit = CodeModeAuditRecord::fallback(planner_name, "planner returned no plan");
            emit_code_mode_audit(&audit, self.agent_code_mode.normalized_timeout_warn_ratio());
            self.record_code_mode_counters(&audit, force_shadow, CodeModeCircuitChange::None);
            return Ok(None);
        };

        let planned_calls = plan.calls.len();
        let execution = match self.agent_code_mode.execution_mode {
            CodeModeExecutionMode::Local => {
                let executor = CodeModeExecutor::new(self.agent_code_mode.clone());
                executor.execute(runtime, &plan, tools).await
            }
            CodeModeExecutionMode::Subprocess => {
                execute_plan_via_subprocess(&plan, tools, &self.agent_code_mode).await
            }
        };
        let execution = match execution {
            Ok(report) => report,
            Err(err) => {
                let audit = CodeModeAuditRecord {
                    planner: planner_name.to_string(),
                    used: false,
                    fallback: true,
                    reason: Some(err.to_string()),
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
        let reply = self
            .provider
            .complete(CompletionRequest {
                messages: next_history,
            })
            .await
            .context("provider completion failed after code mode execution")?;
        Ok(Some(reply))
    }

    async fn complete_with_mcp_loop(
        &self,
        mut history: Vec<StoredMessage>,
        tools: Vec<McpToolInfo>,
        runtime: McpRuntime,
    ) -> anyhow::Result<String> {
        let mut telemetry = McpLoopTelemetry::new(tools.len());
        history.push(StoredMessage {
            role: MessageRole::System,
            content: build_mcp_system_prompt(&tools)?,
        });

        let max_iterations = self.agent_mcp.max_iterations.max(1);
        for _ in 0..max_iterations {
            telemetry.observe_prompt_chars(&history);
            let reply = self
                .provider
                .complete(CompletionRequest {
                    messages: history.clone(),
                })
                .await
                .context("provider completion failed")?;
            telemetry.iterations += 1;

            let Some(tool_call) = parse_mcp_tool_call(&reply) else {
                telemetry.emit("final_answer");
                return Ok(reply);
            };

            let tool_result = runtime
                .call_tool(
                    &tool_call.server,
                    &tool_call.tool,
                    tool_call.arguments.clone(),
                )
                .await;

            let tool_message = match tool_result {
                Ok(value) => {
                    telemetry.tool_calls_total += 1;
                    telemetry.tool_calls_ok += 1;
                    let result = truncate_json_value(&value, self.agent_mcp.max_tool_result_chars);
                    serde_json::json!({
                        "server": tool_call.server,
                        "tool": tool_call.tool,
                        "ok": true,
                        "result": result
                    })
                }
                Err(err) => {
                    telemetry.tool_calls_total += 1;
                    telemetry.tool_calls_err += 1;
                    serde_json::json!({
                        "server": tool_call.server,
                        "tool": tool_call.tool,
                        "ok": false,
                        "error": err.to_string()
                    })
                }
            };

            history.push(StoredMessage {
                role: MessageRole::Assistant,
                content: reply,
            });
            history.push(StoredMessage {
                role: MessageRole::System,
                content: format!(
                    "MCP_TOOL_RESULT_JSON:\n{}",
                    serde_json::to_string(&tool_message)
                        .unwrap_or_else(|_| "{\"ok\":false}".to_string())
                ),
            });
        }

        history.push(StoredMessage {
            role: MessageRole::System,
            content: "MCP tool loop reached max iterations. Give a final answer based on available context."
                .to_string(),
        });
        let final_reply = self
            .provider
            .complete(CompletionRequest { messages: history })
            .await
            .context("provider completion failed")?;
        telemetry.emit("max_iterations");
        Ok(final_reply)
    }

    fn is_mcp_agent_enabled(&self) -> bool {
        self.agent_mcp.enabled && self.mcp_runtime.is_some()
    }

    async fn snapshot_mcp_runtime(&self) -> Option<McpRuntime> {
        if !self.is_mcp_agent_enabled() {
            return None;
        }
        let runtime = self.mcp_runtime.as_ref()?;
        Some(runtime.read().await.clone())
    }

    fn is_code_mode_timeout_circuit_open(&self) -> bool {
        self.code_mode_timeout_circuit_open.load(Ordering::Relaxed)
    }

    fn next_code_mode_attempt(&self) -> Option<CodeModeAttempt> {
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

    fn record_code_mode_counters(
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

    pub async fn reload_skill_runtime_from_registry(
        &self,
        registry: &SkillRegistry,
    ) -> anyhow::Result<()> {
        let Some(runtime) = &self.skills_runtime else {
            return Ok(());
        };
        let next = SkillRuntime::from_registry(registry).await?;
        let mut guard = runtime.write().await;
        *guard = next;
        Ok(())
    }

    pub fn with_agent_skills(
        mut self,
        skills_runtime: Option<Arc<RwLock<SkillRuntime>>>,
        settings: AgentSkillsSettings,
    ) -> Self {
        self.skills_runtime = skills_runtime;
        self.agent_skills = settings;
        self
    }

    fn is_skills_agent_enabled(&self) -> bool {
        self.agent_skills.enabled && self.skills_runtime.is_some()
    }

    async fn snapshot_skill_runtime(&self) -> Option<SkillRuntime> {
        if !self.is_skills_agent_enabled() {
            return None;
        }
        let runtime = self.skills_runtime.as_ref()?;
        Some(runtime.read().await.clone())
    }

    async fn apply_skills_prompt(
        &self,
        mut history: Vec<StoredMessage>,
        query_text: &str,
    ) -> Vec<StoredMessage> {
        let Some(runtime) = self.snapshot_skill_runtime().await else {
            return history;
        };
        let settings = SkillRuntimeSelectionSettings {
            max_selected: self.agent_skills.max_selected,
            max_prompt_chars: self.agent_skills.max_prompt_chars,
            match_min_score: self.agent_skills.match_min_score,
        };
        if let Some(prompt) = runtime.build_system_prompt(query_text, &settings) {
            history.push(StoredMessage {
                role: MessageRole::System,
                content: prompt,
            });
        }
        history
    }
}

#[derive(Debug, Clone)]
struct McpLoopTelemetry {
    started_at: Instant,
    discovered_tools: usize,
    iterations: usize,
    prompt_chars_total: usize,
    tool_calls_total: usize,
    tool_calls_ok: usize,
    tool_calls_err: usize,
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

fn code_mode_timeout_ratio(record: &CodeModeAuditRecord) -> Option<f64> {
    if record.executed_calls == 0 {
        return None;
    }
    Some(record.timed_out_calls as f64 / record.executed_calls as f64)
}

fn should_warn_code_mode_timeouts(record: &CodeModeAuditRecord, timeout_warn_ratio: f64) -> bool {
    let Some(ratio) = code_mode_timeout_ratio(record) else {
        return false;
    };
    record.timed_out_calls > 0 && ratio >= timeout_warn_ratio
}

fn next_code_mode_timeout_streak(
    current: usize,
    record: &CodeModeAuditRecord,
    timeout_warn_ratio: f64,
) -> usize {
    if should_warn_code_mode_timeouts(record, timeout_warn_ratio) {
        return current.saturating_add(1);
    }
    0
}

fn should_probe_code_mode_timeout_circuit(probe_count: usize, probe_every: usize) -> bool {
    let interval = probe_every.max(1);
    probe_count % interval == 0
}

fn should_open_code_mode_timeout_circuit(
    timeout_streak: usize,
    settings: &AgentCodeModeSettings,
) -> bool {
    if !settings.timeout_auto_shadow_enabled || settings.shadow_mode {
        return false;
    }
    timeout_streak >= settings.timeout_auto_shadow_streak.max(1)
}

#[derive(Debug, Clone)]
struct ParsedMcpToolCall {
    server: String,
    tool: String,
    arguments: Value,
}

fn build_mcp_system_prompt(tools: &[McpToolInfo]) -> anyhow::Result<String> {
    let tool_defs = tools
        .iter()
        .take(64)
        .map(|t| {
            serde_json::json!({
                "server": t.server,
                "tool": t.name,
                "description": t.description,
                "input_schema": t.input_schema
            })
        })
        .collect::<Vec<_>>();
    let serialized = serde_json::to_string(&tool_defs).context("failed to encode mcp tool list")?;
    Ok(format!(
        "You can use MCP tools.\nWhen a tool call is needed, reply with ONLY JSON (no markdown, no extra text): {{\"server\":\"<server>\",\"tool\":\"<tool>\",\"arguments\":{{...}}}}.\nAvailable tools: {serialized}\nIf no tool is needed, reply with the final answer directly."
    ))
}

fn parse_mcp_tool_call(reply: &str) -> Option<ParsedMcpToolCall> {
    let json_text = extract_json_payload(reply.trim())?;
    let value: Value = serde_json::from_str(&json_text).ok()?;
    parse_mcp_tool_call_value(&value)
}

fn parse_scheduler_intent_json(reply: &str) -> Option<TelegramSchedulerIntent> {
    let json_text = extract_json_payload(reply.trim())?;
    let mut parsed: TelegramSchedulerIntent = serde_json::from_str(&json_text).ok()?;
    parsed.action = parsed.action.trim().to_ascii_lowercase();
    parsed.confidence = parsed.confidence.clamp(0.0, 1.0);
    parsed.task_kind = parsed
        .task_kind
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty());
    parsed.schedule_kind = parsed
        .schedule_kind
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty());
    parsed.payload = parsed
        .payload
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    parsed.run_at = parsed
        .run_at
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    parsed.cron_expr = parsed
        .cron_expr
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    parsed.timezone = parsed
        .timezone
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    parsed.job_id = parsed
        .job_id
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    parsed.job_operation = parsed
        .job_operation
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty());
    Some(parsed)
}

fn extract_json_payload(text: &str) -> Option<String> {
    if text.starts_with("```") {
        let stripped = text.trim_start_matches("```");
        let stripped = stripped.strip_prefix("json").unwrap_or(stripped).trim();
        let stripped = stripped.strip_suffix("```").unwrap_or(stripped).trim();
        return Some(stripped.to_string());
    }
    Some(text.to_string())
}

fn parse_mcp_tool_call_value(value: &Value) -> Option<ParsedMcpToolCall> {
    if let Some(inner) = value.get("tool_call") {
        return parse_mcp_tool_call_value(inner);
    }
    if let Some(inner) = value.get("mcp_tool_call") {
        return parse_mcp_tool_call_value(inner);
    }
    if let Some(items) = value.as_array() {
        return items.first().and_then(parse_mcp_tool_call_value);
    }

    let obj = value.as_object()?;
    let arguments = obj
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));

    if let (Some(server), Some(tool)) = (
        obj.get("server").and_then(|v| v.as_str()),
        obj.get("tool").and_then(|v| v.as_str()),
    ) {
        return Some(ParsedMcpToolCall {
            server: server.to_string(),
            tool: tool.to_string(),
            arguments,
        });
    }

    let name = obj.get("name").and_then(|v| v.as_str())?;
    if let Some((server, tool)) = name.split_once("::").or_else(|| name.split_once('/')) {
        return Some(ParsedMcpToolCall {
            server: server.to_string(),
            tool: tool.to_string(),
            arguments,
        });
    }
    None
}

fn apply_context_budget(
    messages: Vec<StoredMessage>,
    context_window_tokens: usize,
    context_reserved_tokens: usize,
    memory_budget_ratio: u8,
    min_recent_messages: usize,
) -> Vec<StoredMessage> {
    if messages.is_empty() || context_window_tokens == 0 {
        return messages;
    }

    let input_budget = context_window_tokens.saturating_sub(context_reserved_tokens);
    if input_budget == 0 {
        return messages
            .into_iter()
            .last()
            .map(|m| vec![m])
            .unwrap_or_default();
    }

    let total_tokens = messages.iter().map(estimate_message_tokens).sum::<usize>();
    if total_tokens <= input_budget {
        return messages;
    }

    let mut selected = vec![false; messages.len()];
    let mut replacements: HashMap<usize, StoredMessage> = HashMap::new();
    let mut used = 0usize;
    let capped_ratio = memory_budget_ratio.clamp(0, 80) as usize;
    let memory_budget = input_budget.saturating_mul(capped_ratio) / 100;

    let mut memory_candidates = messages
        .iter()
        .enumerate()
        .filter_map(|(idx, msg)| {
            if msg.role != MessageRole::System {
                return None;
            }
            let score = parse_memory_score(&msg.content)?;
            Some((idx, score, estimate_message_tokens(msg)))
        })
        .collect::<Vec<_>>();
    memory_candidates.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.0.cmp(&a.0))
    });

    for (idx, _score, tokens) in memory_candidates {
        if used + tokens > memory_budget {
            continue;
        }
        selected[idx] = true;
        used += tokens;
    }

    let mut recent_kept = 0usize;
    for idx in (0..messages.len()).rev() {
        if selected[idx] {
            continue;
        }
        let tokens = estimate_message_tokens(&messages[idx]);
        if used + tokens > input_budget {
            if recent_kept < min_recent_messages {
                let remain = input_budget.saturating_sub(used);
                if remain > 12 {
                    let truncated = truncate_message_to_token_budget(&messages[idx], remain);
                    let truncated_tokens = estimate_message_tokens(&truncated);
                    if truncated_tokens > 0 {
                        selected[idx] = true;
                        used = used.saturating_add(truncated_tokens).min(input_budget);
                        replacements.insert(idx, truncated);
                    }
                    recent_kept += 1;
                }
            }
            continue;
        }
        selected[idx] = true;
        used += tokens;
        recent_kept += 1;
        if used >= input_budget {
            break;
        }
    }

    for idx in (0..messages.len()).rev() {
        if used >= input_budget {
            break;
        }
        if selected[idx] {
            continue;
        }
        let tokens = estimate_message_tokens(&messages[idx]);
        if used + tokens > input_budget {
            continue;
        }
        selected[idx] = true;
        used += tokens;
    }

    if !selected.iter().any(|v| *v)
        && let Some(last_idx) = messages.len().checked_sub(1)
    {
        selected[last_idx] = true;
    }

    messages
        .into_iter()
        .enumerate()
        .filter_map(|(idx, msg)| {
            if !selected[idx] {
                return None;
            }
            Some(replacements.remove(&idx).unwrap_or(msg))
        })
        .collect()
}

fn estimate_message_tokens(msg: &StoredMessage) -> usize {
    estimate_text_tokens(&msg.content).saturating_add(4)
}

fn estimate_text_tokens(text: &str) -> usize {
    let chars = text.chars().count();
    (chars.saturating_add(3) / 4).max(1)
}

fn parse_memory_score(content: &str) -> Option<f32> {
    let marker = "[memory score=";
    let start = content.find(marker)? + marker.len();
    let tail = &content[start..];
    let end = tail.find([' ', ']'])?;
    tail[..end].trim().parse::<f32>().ok()
}

fn truncate_message_to_token_budget(msg: &StoredMessage, max_tokens: usize) -> StoredMessage {
    if max_tokens == 0 {
        return StoredMessage {
            role: msg.role.clone(),
            content: String::new(),
        };
    }
    if estimate_message_tokens(msg) <= max_tokens {
        return msg.clone();
    }

    let max_chars = max_tokens.saturating_mul(4).saturating_sub(12);
    let mut content = msg
        .content
        .chars()
        .take(max_chars.max(8))
        .collect::<String>();
    if !content.is_empty() {
        content.push_str(" ...(truncated)");
    }
    StoredMessage {
        role: msg.role.clone(),
        content,
    }
}

fn truncate_json_value(value: &Value, max_chars: usize) -> Value {
    let encoded = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    if encoded.chars().count() <= max_chars {
        return value.clone();
    }
    let mut out = encoded.chars().take(max_chars).collect::<String>();
    out.push_str("...(truncated)");
    serde_json::json!({ "truncated": out })
}

#[cfg(test)]
mod tests {
    use super::{
        CodeModeCircuitChange, MessageService, apply_context_budget, code_mode_timeout_ratio,
        next_code_mode_timeout_streak, parse_mcp_tool_call, parse_scheduler_intent_json,
        should_open_code_mode_timeout_circuit, should_probe_code_mode_timeout_circuit,
        should_warn_code_mode_timeouts, truncate_json_value,
    };
    use crate::code_mode::{AgentCodeModeSettings, CodeModeAuditRecord};
    use crate::domain::{MessageRole, StoredMessage};
    use crate::memory::SqliteMemoryStore;
    use crate::provider::{ChatProvider, CompletionRequest};
    use std::sync::Arc;

    struct FakeProvider;

    #[async_trait::async_trait]
    impl ChatProvider for FakeProvider {
        async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
            Ok("ok".to_string())
        }
    }

    #[test]
    fn parse_tool_call_plain_json() {
        let parsed =
            parse_mcp_tool_call(r#"{"server":"search","tool":"web","arguments":{"q":"rust mcp"}}"#)
                .expect("parsed");
        assert_eq!(parsed.server, "search");
        assert_eq!(parsed.tool, "web");
    }

    #[test]
    fn parse_tool_call_from_fenced_json() {
        let parsed = parse_mcp_tool_call(
            "```json\n{\"tool_call\":{\"server\":\"s1\",\"tool\":\"t1\",\"arguments\":{}}}\n```",
        )
        .expect("parsed");
        assert_eq!(parsed.server, "s1");
        assert_eq!(parsed.tool, "t1");
    }

    #[test]
    fn parse_tool_call_name_compact_format() {
        let parsed =
            parse_mcp_tool_call(r#"{"name":"s2::t2","arguments":{"x":1}}"#).expect("parsed");
        assert_eq!(parsed.server, "s2");
        assert_eq!(parsed.tool, "t2");
    }

    #[test]
    fn truncate_json_value_caps_size() {
        let value = serde_json::json!({ "long": "abcdefghijklmnopqrstuvwxyz" });
        let truncated = truncate_json_value(&value, 12);
        assert!(truncated.get("truncated").is_some());
    }

    #[test]
    fn parse_scheduler_intent_json_normalizes_fields() {
        let parsed = parse_scheduler_intent_json(
            r#"{"action":" CREATE ","confidence":1.2,"task_kind":"Reminder","payload":"  明早开会  ","schedule_kind":"Once","run_at":" 2026-03-01T09:00:00+08:00 ","cron_expr":"","timezone":" Asia/Shanghai ","job_id":" ","job_operation":" DELETE "}"#,
        )
        .expect("parsed");
        assert_eq!(parsed.action, "create");
        assert_eq!(parsed.confidence, 1.0);
        assert_eq!(parsed.task_kind.as_deref(), Some("reminder"));
        assert_eq!(parsed.payload.as_deref(), Some("明早开会"));
        assert_eq!(parsed.schedule_kind.as_deref(), Some("once"));
        assert_eq!(parsed.run_at.as_deref(), Some("2026-03-01T09:00:00+08:00"));
        assert!(parsed.cron_expr.is_none());
        assert_eq!(parsed.timezone.as_deref(), Some("Asia/Shanghai"));
        assert!(parsed.job_id.is_none());
        assert_eq!(parsed.job_operation.as_deref(), Some("delete"));
    }

    #[test]
    fn context_budget_prefers_high_score_memory_and_latest_turns() {
        let messages = vec![
            StoredMessage {
                role: MessageRole::System,
                content: "[memory score=0.950 src=vec] Rust trait object supports dynamic dispatch"
                    .to_string(),
            },
            StoredMessage {
                role: MessageRole::System,
                content: "[memory score=0.120 src=vec]".to_string()
                    + &"irrelevant old memory".repeat(80),
            },
            StoredMessage {
                role: MessageRole::User,
                content: "first question about rust".repeat(20),
            },
            StoredMessage {
                role: MessageRole::Assistant,
                content: "first answer".repeat(20),
            },
            StoredMessage {
                role: MessageRole::User,
                content: "latest question: explain trait object".to_string(),
            },
        ];

        let trimmed = apply_context_budget(messages, 180, 40, 35, 2);

        assert!(
            trimmed
                .iter()
                .any(|m| m.content.contains("score=0.950") && m.role == MessageRole::System)
        );
        assert!(
            trimmed
                .iter()
                .any(|m| m.content.contains("latest question") && m.role == MessageRole::User)
        );
        assert!(
            !trimmed
                .iter()
                .any(|m| m.content.contains("score=0.120") && m.role == MessageRole::System)
        );
    }

    #[test]
    fn code_mode_timeout_ratio_warning_threshold_works() {
        let low = CodeModeAuditRecord {
            planner: "p".to_string(),
            used: true,
            fallback: false,
            reason: None,
            planned_calls: 5,
            executed_calls: 5,
            failed_calls: 1,
            timed_out_calls: 1,
            elapsed_ms: 100,
        };
        assert_eq!(code_mode_timeout_ratio(&low), Some(0.2));
        assert!(!should_warn_code_mode_timeouts(&low, 0.4));

        let high = CodeModeAuditRecord {
            timed_out_calls: 2,
            ..low.clone()
        };
        assert_eq!(code_mode_timeout_ratio(&high), Some(0.4));
        assert!(should_warn_code_mode_timeouts(&high, 0.4));

        let no_exec = CodeModeAuditRecord {
            executed_calls: 0,
            failed_calls: 0,
            timed_out_calls: 0,
            ..low
        };
        assert_eq!(code_mode_timeout_ratio(&no_exec), None);
        assert!(!should_warn_code_mode_timeouts(&no_exec, 0.4));
    }

    #[test]
    fn code_mode_timeout_streak_and_circuit_rules_work() {
        let settings = AgentCodeModeSettings {
            shadow_mode: false,
            timeout_auto_shadow_enabled: true,
            timeout_auto_shadow_streak: 3,
            ..AgentCodeModeSettings::default()
        };
        assert!(!should_open_code_mode_timeout_circuit(2, &settings));
        assert!(should_open_code_mode_timeout_circuit(3, &settings));

        let disabled = AgentCodeModeSettings {
            timeout_auto_shadow_enabled: false,
            timeout_auto_shadow_streak: 1,
            ..AgentCodeModeSettings::default()
        };
        assert!(!should_open_code_mode_timeout_circuit(99, &disabled));

        let forced_shadow = AgentCodeModeSettings {
            shadow_mode: true,
            timeout_auto_shadow_enabled: true,
            timeout_auto_shadow_streak: 1,
            ..AgentCodeModeSettings::default()
        };
        assert!(!should_open_code_mode_timeout_circuit(99, &forced_shadow));

        let high = CodeModeAuditRecord {
            planner: "p".to_string(),
            used: true,
            fallback: false,
            reason: None,
            planned_calls: 5,
            executed_calls: 5,
            failed_calls: 2,
            timed_out_calls: 2,
            elapsed_ms: 100,
        };
        let low = CodeModeAuditRecord {
            timed_out_calls: 0,
            ..high.clone()
        };

        assert_eq!(next_code_mode_timeout_streak(0, &high, 0.4), 1);
        assert_eq!(next_code_mode_timeout_streak(2, &high, 0.4), 3);
        assert_eq!(next_code_mode_timeout_streak(2, &low, 0.4), 0);
    }

    #[test]
    fn code_mode_probe_schedule_works() {
        assert!(!should_probe_code_mode_timeout_circuit(1, 5));
        assert!(!should_probe_code_mode_timeout_circuit(4, 5));
        assert!(should_probe_code_mode_timeout_circuit(5, 5));
        assert!(should_probe_code_mode_timeout_circuit(10, 5));
        assert!(should_probe_code_mode_timeout_circuit(1, 0));
    }

    #[tokio::test]
    async fn code_mode_diagnostics_counters_accumulate() {
        let store = SqliteMemoryStore::new("sqlite::memory:")
            .await
            .expect("store");
        let service = MessageService::new(Arc::new(FakeProvider), store, 8);

        let audit = CodeModeAuditRecord {
            planner: "p".to_string(),
            used: false,
            fallback: true,
            reason: Some("timeout circuit probe".to_string()),
            planned_calls: 2,
            executed_calls: 2,
            failed_calls: 1,
            timed_out_calls: 1,
            elapsed_ms: 42,
        };
        service.record_code_mode_counters(&audit, true, CodeModeCircuitChange::Opened);

        let diag = service.code_mode_diagnostics();
        assert_eq!(diag.runtime.counters.attempts_total, 1);
        assert_eq!(diag.runtime.counters.used_total, 0);
        assert_eq!(diag.runtime.counters.fallback_total, 1);
        assert_eq!(diag.runtime.counters.failed_calls_total, 1);
        assert_eq!(diag.runtime.counters.timed_out_calls_total, 1);
        assert_eq!(diag.runtime.counters.probe_attempt_total, 1);
        assert_eq!(diag.runtime.counters.circuit_open_total, 1);
        assert_eq!(diag.runtime.counters.circuit_close_total, 0);
    }

    #[tokio::test]
    async fn code_mode_prometheus_metrics_render_counter_values() {
        let store = SqliteMemoryStore::new("sqlite::memory:")
            .await
            .expect("store");
        let service = MessageService::new(Arc::new(FakeProvider), store, 8);

        let audit = CodeModeAuditRecord {
            planner: "p".to_string(),
            used: false,
            fallback: true,
            reason: Some("timeout circuit probe".to_string()),
            planned_calls: 2,
            executed_calls: 2,
            failed_calls: 1,
            timed_out_calls: 1,
            elapsed_ms: 42,
        };
        service.record_code_mode_counters(&audit, true, CodeModeCircuitChange::Opened);

        let body = service.code_mode_metrics_prometheus();
        assert!(body.contains("xiaomaolv_code_mode_attempts_total 1"));
        assert!(body.contains("xiaomaolv_code_mode_fallback_total 1"));
        assert!(body.contains("xiaomaolv_code_mode_timed_out_calls_total 1"));
        assert!(body.contains("xiaomaolv_code_mode_circuit_open_total 1"));
        assert!(body.contains("xiaomaolv_code_mode_timeout_warn_ratio 0.400000"));
        assert!(body.contains("xiaomaolv_code_mode_timeout_auto_shadow_probe_every 5"));
    }
}
