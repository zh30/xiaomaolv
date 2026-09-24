use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::{RwLock, Semaphore};
use tokio::time::timeout;
use tracing::{info, warn};

use crate::code_mode::{
    AgentCodeModeSettings, CodeModeAuditRecord, CodeModeCallResult, CodeModeExecutionMode,
    CodeModePlanner, CodeModePolicy, DisabledCodeModePlanner,
};
use crate::config::{AgentHarnessConfig, OutputVerificationMode, ToolVerificationMode};
use crate::domain::{IncomingMessage, MessageRole, OutgoingMessage, StoredMessage};
use crate::harness::compactor::{
    CompactionMessageMetadata, CompactionRequest, CompactionStrategy, Compactor,
};
use crate::harness::evolution::{EvolutionPolicyRuntime, render_evolution_policy_message};
use crate::harness::execution_environment::{
    ExecutionEnvironment, LocalExecutionEnvironment, SubprocessExecutionEnvironment,
};
use crate::harness::observability::TrajectoryMetrics;
use crate::harness::output_exit::{OutputExit, OutputExitRequest};
use crate::harness::run::{AgentRun, AgentRunExit, AgentRunStart};
use crate::harness::store::{HarnessStore, SqliteHarnessStore};
use crate::harness::tool_protocol::{
    ToolProposal, ToolProtocol, annotate_record_with_verification_failure,
    verification_failure_record, verification_feedback_message,
};
use crate::harness::trajectory::{ToolCallRecord, TrajectoryLogger};
use crate::harness::verifier::{
    CompositeVerifier, DeterministicOutputVerifier, ResultShapeVerifier, TimingVerifier,
    ToolCallVerifier, ToolSchemaVerifier, VerificationIssue, VerificationResult,
};
use crate::json_utils::extract_json_payload;
use crate::mcp::{BUILTIN_MCP_SERVER_NAME, BUILTIN_MCP_TOOL_CURRENT_TIME, McpRuntime, McpToolInfo};
use crate::memory::{
    AgentSwarmCleanupRequest, AgentSwarmNodeExitStatus, AgentSwarmNodeLoadRequest,
    AgentSwarmNodeRecord, AgentSwarmNodeState, AgentSwarmRunListRequest, AgentSwarmRunRecord,
    AgentSwarmRunStatus, AgentSwarmTreeRequest, ClaimDueTelegramSchedulerJobsRequest,
    CompactionSummaryLoadRequest, CompactionSummaryUpsertRequest,
    CompleteTelegramSchedulerJobRunRequest, CreateAgentSwarmRunRequest,
    CreateTelegramSchedulerJobRequest, FailTelegramSchedulerJobRunRequest,
    FinishAgentSwarmRunRequest, GroupAliasLoadRequest, GroupAliasUpsertRequest,
    GroupUserProfileLoadRequest, GroupUserProfileRecord, GroupUserProfileUpsertRequest,
    MemoryBackend, MemoryContextRequest, MemoryWriteRequest, RecentMessageRecordsRequest,
    SqliteMemoryBackend, SqliteMemoryStore, StoredMessageRecord, TelegramSchedulerJobListRequest,
    TelegramSchedulerJobRecord, TelegramSchedulerJobStatus, TelegramSchedulerPendingIntentRecord,
    TelegramSchedulerStats, TelegramSchedulerStatsRequest, UpdateTelegramSchedulerJobStatusRequest,
    UpsertAgentSwarmNodeRequest, UpsertTelegramSchedulerPendingIntentRequest,
};
use crate::provider::{ChatProvider, CompletionRequest, StreamSink};
use crate::skills::{SkillRegistry, SkillRuntime, SkillRuntimeSelectionSettings};

mod completion;
mod delegates;
mod swarm;
mod time_query;

use time_query::{
    build_builtin_time_tool_arguments, extract_primary_user_text, infer_timezone_from_time_query,
    is_explicit_current_time_query, looks_like_time_query, render_fast_time_answer,
};

pub use swarm::AgentSwarmSettings;

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

#[derive(Debug, Clone)]
pub struct AgentCompactionSettings {
    pub enabled: bool,
    pub strategy: CompactionStrategy,
    pub head_count: usize,
    pub tail_count: usize,
    pub message_count_threshold: usize,
}

impl Default for AgentCompactionSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            strategy: CompactionStrategy::HeadTail {
                head_count: 2,
                tail_count: 2,
            },
            head_count: 2,
            tail_count: 2,
            message_count_threshold: 20,
        }
    }
}

#[derive(Clone)]
pub struct MessageService {
    provider: Arc<dyn ChatProvider>,
    memory: Arc<dyn MemoryBackend>,
    harness_store: Option<Arc<dyn HarnessStore>>,
    mcp_runtime: Option<Arc<RwLock<McpRuntime>>>,
    code_mode_planner: Arc<dyn CodeModePlanner>,
    skills_runtime: Option<Arc<RwLock<SkillRuntime>>>,
    agent_mcp: AgentMcpSettings,
    agent_code_mode: AgentCodeModeSettings,
    agent_skills: AgentSkillsSettings,
    agent_swarm: AgentSwarmSettings,
    agent_compaction: AgentCompactionSettings,
    compactor: Option<Compactor>,
    swarm_run_counter: Arc<AtomicUsize>,
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
    trajectory_logger: Option<TrajectoryLogger>,
    trajectory_metrics: Option<TrajectoryMetrics>,
    tool_verifier: Option<Arc<dyn ToolCallVerifier>>,
    tool_schema_verifier: Option<ToolSchemaVerifier>,
    tool_verification_mode: ToolVerificationMode,
    output_verifier: Option<DeterministicOutputVerifier>,
    output_verification_mode: OutputVerificationMode,
    output_verification_llm_enabled: bool,
    output_verification_max_prompt_chars: usize,
    output_verification_max_result_chars: usize,
    evolution_policy_runtime: Option<EvolutionPolicyRuntime>,
}

struct CompletionOutcome {
    text: String,
    output_verified: bool,
}

/// Result of the shared turn-preparation stage used by both `handle` and
/// `handle_stream`. `Immediate` carries a final answer that is already verified
/// (time fast-path or swarm reply); `Ready` carries the fully prepared history
/// for the completion stage.
enum PreparedTurn {
    Immediate { text: String },
    Ready { history: Vec<StoredMessage> },
}

impl CompletionOutcome {
    fn unverified(text: String) -> Self {
        Self {
            text,
            output_verified: false,
        }
    }

    fn verified(text: String) -> Self {
        Self {
            text,
            output_verified: true,
        }
    }
}

impl MessageService {
    pub fn new(
        provider: Arc<dyn ChatProvider>,
        store: SqliteMemoryStore,
        max_history: usize,
    ) -> Self {
        let memory: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(store.clone()));
        let harness_store: Arc<dyn HarnessStore> = Arc::new(SqliteHarnessStore::new(store));
        Self::new_with_backend(
            provider,
            memory,
            None,
            AgentMcpSettings::default(),
            max_history,
            0,
            0,
        )
        .with_harness_store(harness_store)
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
            harness_store: None,
            mcp_runtime,
            code_mode_planner: Arc::new(DisabledCodeModePlanner),
            skills_runtime: None,
            agent_mcp,
            agent_code_mode: AgentCodeModeSettings::default(),
            agent_skills: AgentSkillsSettings::default(),
            agent_swarm: AgentSwarmSettings::default(),
            agent_compaction: AgentCompactionSettings::default(),
            compactor: None,
            swarm_run_counter: Arc::new(AtomicUsize::new(0)),
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
            trajectory_logger: None,
            trajectory_metrics: None,
            tool_verifier: None,
            tool_schema_verifier: None,
            tool_verification_mode: ToolVerificationMode::Observe,
            output_verifier: None,
            output_verification_mode: OutputVerificationMode::Off,
            output_verification_llm_enabled: false,
            output_verification_max_prompt_chars: 6000,
            output_verification_max_result_chars: 2000,
            evolution_policy_runtime: None,
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

    pub fn with_trajectory_logger(mut self, logger: TrajectoryLogger) -> Self {
        self.harness_store = Some(logger.store());
        self.trajectory_logger = Some(logger);
        self
    }

    pub fn with_harness_store(mut self, store: Arc<dyn HarnessStore>) -> Self {
        self.harness_store = Some(store);
        self
    }

    pub fn with_evolution_policy_runtime(mut self, runtime: EvolutionPolicyRuntime) -> Self {
        self.evolution_policy_runtime = Some(runtime);
        self
    }

    pub fn with_trajectory_metrics(mut self, metrics: TrajectoryMetrics) -> Self {
        self.trajectory_metrics = Some(metrics);
        self
    }

    pub fn with_tool_verifier(mut self, verifier: Arc<dyn ToolCallVerifier>) -> Self {
        self.tool_verifier = Some(verifier);
        self
    }

    pub fn with_compaction(mut self, settings: AgentCompactionSettings) -> Self {
        self.agent_compaction = settings;
        self.compactor = Some(Compactor::new(self.agent_compaction.enabled));
        self
    }

    /// Apply harness configuration from AgentHarnessConfig
    pub fn with_harness_config(mut self, config: &AgentHarnessConfig) -> Self {
        // Set up trajectory logger if enabled
        if config.enable_trajectory {
            if let Some(store) = &self.harness_store {
                self.trajectory_logger = Some(TrajectoryLogger::new(store.clone(), true));
            } else {
                warn!("trajectory logging requested but no harness store is configured");
            }
        }

        // Set up compactor if enabled
        if config.enable_compaction {
            let strategy = match config.compaction_strategy.as_str() {
                "head_tail" => CompactionStrategy::HeadTail {
                    head_count: config.compaction_head_count,
                    tail_count: config.compaction_tail_count,
                },
                "age_based" => CompactionStrategy::AgeBased {
                    max_age_days: config.compaction_age_max_days,
                },
                "budget_based" => CompactionStrategy::BudgetBased {
                    max_tokens: config.compaction_budget_max_tokens,
                },
                _ => CompactionStrategy::HeadTail {
                    head_count: config.compaction_head_count,
                    tail_count: config.compaction_tail_count,
                },
            };
            self.agent_compaction = AgentCompactionSettings {
                enabled: true,
                strategy,
                head_count: config.compaction_head_count,
                tail_count: config.compaction_tail_count,
                message_count_threshold: config.compaction_message_threshold,
            };
            self.compactor = Some(Compactor::new(true));
        }

        // Set up tool verifier if enabled
        if config.enable_verification {
            self.tool_verification_mode = config.verification_mode;
            self.tool_schema_verifier = Some(ToolSchemaVerifier::new());
            self.tool_verifier = Some(Arc::new(
                CompositeVerifier::new()
                    .add(TimingVerifier::new(
                        config.verification_max_tool_duration_ms,
                        config.verification_warn_ratio,
                    ))
                    .add(ResultShapeVerifier::new()),
            ) as Arc<dyn ToolCallVerifier>);
        }

        self.output_verification_mode = config.output_verification_mode;
        self.output_verification_llm_enabled = config.output_verification_llm_enabled;
        self.output_verification_max_prompt_chars = config.output_verification_max_prompt_chars;
        self.output_verification_max_result_chars = config.output_verification_max_result_chars;
        if !matches!(config.output_verification_mode, OutputVerificationMode::Off) {
            self.output_verifier = Some(DeterministicOutputVerifier::new());
        }

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

    pub fn harness_metrics_prometheus(&self) -> Option<String> {
        self.trajectory_metrics
            .as_ref()
            .map(TrajectoryMetrics::render_prometheus)
    }

    async fn verify_final_answer(
        &self,
        history: &[StoredMessage],
        channel: &str,
        final_answer: String,
        tool_calls: &[ToolCallRecord],
    ) -> anyhow::Result<String> {
        let exit = OutputExit::new(
            self.provider.clone(),
            self.output_verifier.clone(),
            self.output_verification_mode,
            self.output_verification_llm_enabled,
            self.output_verification_max_prompt_chars,
            self.output_verification_max_result_chars,
        );
        let result = exit
            .finalize(OutputExitRequest {
                history,
                channel,
                final_answer,
                tool_calls,
                required_format: None,
            })
            .await?;
        Ok(result.text)
    }

    /// Shared prelude for `handle` and `handle_stream`: persists the user
    /// message, resolves fast-path/swarm early answers, and prepares the
    /// history (context load -> compaction -> budget -> skills -> evolution ->
    /// builtin time context). The completion tail differs per caller.
    async fn prepare_turn(&self, incoming: &IncomingMessage) -> anyhow::Result<PreparedTurn> {
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

        if let Some(text) = self.try_answer_time_query_fast_path(&incoming.text).await {
            let history = vec![StoredMessage {
                role: MessageRole::User,
                content: incoming.text.clone(),
            }];
            let text = self
                .verify_final_answer(&history, &incoming.channel, text, &[])
                .await?;
            return Ok(PreparedTurn::Immediate { text });
        }

        let mut history = self
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

        // Apply compaction before destructive budget trimming, then keep a final budget safety pass.
        if self.agent_compaction.enabled
            && history.len() >= self.agent_compaction.message_count_threshold
            && self.compactor.is_some()
        {
            match self
                .apply_compaction(history.clone(), &incoming.session_id)
                .await
            {
                Ok(compacted) => {
                    history = compacted;
                }
                Err(err) => {
                    warn!(error = %err, "compaction failed, using original history");
                }
            }
        }
        let history = apply_context_budget(
            history,
            self.context_window_tokens,
            self.context_reserved_tokens,
            self.context_memory_budget_ratio,
            self.context_min_recent_messages,
        );

        let history = self.apply_skills_prompt(history, &incoming.text).await;
        let history = self.apply_evolution_policy(history).await;
        let history = self
            .append_builtin_time_context_if_needed(history, &incoming.text)
            .await;

        if let Some(swarm_text) = self.try_swarm_reply(incoming, &history).await? {
            let text = self
                .verify_final_answer(&history, &incoming.channel, swarm_text, &[])
                .await?;
            return Ok(PreparedTurn::Immediate { text });
        }

        Ok(PreparedTurn::Ready { history })
    }

    pub async fn handle(&self, incoming: IncomingMessage) -> anyhow::Result<OutgoingMessage> {
        let history = match self.prepare_turn(&incoming).await? {
            PreparedTurn::Immediate { text } => {
                self.persist_assistant_reply(&incoming, &text).await?;
                return Ok(OutgoingMessage {
                    channel: incoming.channel,
                    session_id: incoming.session_id,
                    text,
                    reply_target: incoming.reply_target,
                });
            }
            PreparedTurn::Ready { history } => history,
        };

        let completion = self
            .complete_with_optional_mcp(history.clone(), &incoming)
            .await?;
        let text = if completion.output_verified {
            completion.text
        } else {
            self.verify_final_answer(&history, &incoming.channel, completion.text, &[])
                .await?
        };

        self.persist_assistant_reply(&incoming, &text).await?;

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
        let history = match self.prepare_turn(&incoming).await? {
            PreparedTurn::Immediate { text } => {
                if !text.is_empty() {
                    sink.on_delta(&text).await?;
                }
                self.persist_assistant_reply(&incoming, &text).await?;
                return Ok(OutgoingMessage {
                    channel: incoming.channel,
                    session_id: incoming.session_id,
                    text,
                    reply_target: incoming.reply_target,
                });
            }
            PreparedTurn::Ready { history } => history,
        };

        let text = self
            .complete_with_optional_mcp_stream(history, sink, &incoming)
            .await
            .context("failed to process streaming completion with optional mcp")?;

        self.persist_assistant_reply(&incoming, &text).await?;

        Ok(OutgoingMessage {
            channel: incoming.channel,
            session_id: incoming.session_id,
            text,
            reply_target: incoming.reply_target,
        })
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

    pub fn with_agent_swarm(mut self, settings: AgentSwarmSettings) -> Self {
        self.agent_swarm = settings;
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

    async fn apply_compaction(
        &self,
        history: Vec<StoredMessage>,
        session_id: &str,
    ) -> anyhow::Result<Vec<StoredMessage>> {
        // Return early if compaction is not enabled or not needed
        if !self.agent_compaction.enabled {
            return Ok(history);
        }

        if history.len() < self.agent_compaction.message_count_threshold {
            return Ok(history);
        }

        let Some(compactor) = &self.compactor else {
            return Ok(history);
        };

        let strategy = self.agent_compaction.strategy.clone();
        let metadata = self
            .load_compaction_metadata(session_id, &history)
            .await
            .unwrap_or_else(|err| {
                warn!(error = %err, "failed to load compaction metadata");
                vec![CompactionMessageMetadata::default(); history.len()]
            });
        let source_key = build_compaction_source_key(&history, &strategy, &metadata);
        let request = CompactionRequest {
            messages: history,
            strategy: strategy.clone(),
            metadata,
            now_unix: Some(current_unix_timestamp_i64()),
            min_recent_messages: self.context_min_recent_messages,
        };

        if let Some(cached) = self
            .memory
            .load_compaction_summary(CompactionSummaryLoadRequest {
                session_id: session_id.to_string(),
                strategy: source_key.strategy.clone(),
                source_hash: source_key.source_hash.clone(),
            })
            .await
            .unwrap_or_else(|err| {
                warn!(error = %err, "failed to load cached compaction summary");
                None
            })
        {
            let result = compactor.compact_with_summary(request, cached.summary)?;
            return Ok(result.compacted_messages);
        }

        let result = compactor.compact(request, self.provider.clone()).await?;
        if !result.summary.trim().is_empty() && result.tokens_saved > 0 {
            let req = CompactionSummaryUpsertRequest {
                session_id: session_id.to_string(),
                strategy: source_key.strategy,
                source_hash: source_key.source_hash,
                source_message_ids: source_key.source_message_ids,
                source_first_message_id: source_key.source_first_message_id,
                source_last_message_id: source_key.source_last_message_id,
                source_message_count: source_key.source_message_count,
                summary: result.summary.clone(),
                tokens_saved: result.tokens_saved,
            };
            if let Err(err) = self.memory.upsert_compaction_summary(req).await {
                warn!(error = %err, "failed to persist compaction summary");
            }
        }
        Ok(result.compacted_messages)
    }

    async fn load_compaction_metadata(
        &self,
        session_id: &str,
        history: &[StoredMessage],
    ) -> anyhow::Result<Vec<CompactionMessageMetadata>> {
        let recent_records = self
            .memory
            .load_recent_records(RecentMessageRecordsRequest {
                session_id: session_id.to_string(),
                limit: self.max_recent_turns,
            })
            .await?;
        Ok(align_compaction_metadata(history, recent_records))
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
        let selection = runtime.select(query_text, &settings);
        if !selection.skills.is_empty() {
            info!(
                skills = ?selection
                    .skills
                    .iter()
                    .map(|skill| skill.id.as_str())
                    .collect::<Vec<_>>(),
                "selected skills for agent run"
            );
        }
        if let Some(prompt) =
            SkillRuntime::render_system_prompt(&selection, settings.max_prompt_chars)
        {
            history.push(StoredMessage {
                role: MessageRole::System,
                content: prompt,
            });
        }
        history
    }

    async fn apply_evolution_policy(&self, mut history: Vec<StoredMessage>) -> Vec<StoredMessage> {
        let Some(runtime) = &self.evolution_policy_runtime else {
            return history;
        };
        let Some(active) = runtime.active().await else {
            return history;
        };
        history.push(StoredMessage {
            role: MessageRole::System,
            content: render_evolution_policy_message(
                &active.candidate_id,
                &active.deployment_id,
                &active.prompt_patch,
            ),
        });
        history
    }

    async fn append_builtin_time_context_if_needed(
        &self,
        mut history: Vec<StoredMessage>,
        query_text: &str,
    ) -> Vec<StoredMessage> {
        let user_text = extract_primary_user_text(query_text);
        if !looks_like_time_query(user_text) {
            return history;
        }
        let Some(runtime) = self.snapshot_mcp_runtime().await else {
            return history;
        };
        let timezone_hint = infer_timezone_from_time_query(user_text);
        match runtime
            .call_tool(
                BUILTIN_MCP_SERVER_NAME,
                BUILTIN_MCP_TOOL_CURRENT_TIME,
                build_builtin_time_tool_arguments(timezone_hint),
            )
            .await
        {
            Ok(value) => {
                let tool_message = serde_json::json!({
                    "server": BUILTIN_MCP_SERVER_NAME,
                    "tool": BUILTIN_MCP_TOOL_CURRENT_TIME,
                    "ok": true,
                    "result": value
                });
                history.push(StoredMessage {
                    role: MessageRole::System,
                    content: format!(
                        "MCP_TOOL_RESULT_JSON:\n{}",
                        serde_json::to_string(&tool_message)
                            .unwrap_or_else(|_| "{\"ok\":false}".to_string())
                    ),
                });
                info!(
                    timezone = ?timezone_hint.map(|hint| hint.timezone),
                    timezone_source = ?timezone_hint.map(|hint| hint.source),
                    "prefetched builtin mcp time context"
                );
            }
            Err(err) => {
                warn!(
                    error = %err,
                    "failed to call builtin mcp time tool for time-related query"
                );
            }
        }
        history
    }

    async fn persist_assistant_reply(
        &self,
        incoming: &IncomingMessage,
        text: &str,
    ) -> anyhow::Result<()> {
        self.memory
            .append(MemoryWriteRequest {
                session_id: incoming.session_id.clone(),
                user_id: incoming.user_id.clone(),
                channel: incoming.channel.clone(),
                message: StoredMessage {
                    role: MessageRole::Assistant,
                    content: text.to_string(),
                },
            })
            .await
            .context("failed to persist assistant message")
    }

    async fn try_answer_time_query_fast_path(&self, query_text: &str) -> Option<String> {
        let user_text = extract_primary_user_text(query_text);
        if !is_explicit_current_time_query(user_text) {
            return None;
        }
        let runtime = self.snapshot_mcp_runtime().await?;
        let timezone_hint = infer_timezone_from_time_query(user_text);
        match runtime
            .call_tool(
                BUILTIN_MCP_SERVER_NAME,
                BUILTIN_MCP_TOOL_CURRENT_TIME,
                build_builtin_time_tool_arguments(timezone_hint),
            )
            .await
        {
            Ok(value) => {
                let answer = render_fast_time_answer(user_text, &value, timezone_hint);
                info!(
                    timezone = ?timezone_hint.map(|hint| hint.timezone),
                    timezone_source = ?timezone_hint.map(|hint| hint.source),
                    "time query served by builtin mcp fast path"
                );
                Some(answer)
            }
            Err(err) => {
                warn!(
                    error = %err,
                    "builtin mcp fast path failed for time query, fallback to model pipeline"
                );
                None
            }
        }
    }
}

fn current_unix_timestamp_i64() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
struct CompactionSourceKey {
    strategy: String,
    source_hash: String,
    source_message_ids: Vec<i64>,
    source_first_message_id: Option<i64>,
    source_last_message_id: Option<i64>,
    source_message_count: usize,
}

fn align_compaction_metadata(
    history: &[StoredMessage],
    mut recent_records: Vec<StoredMessageRecord>,
) -> Vec<CompactionMessageMetadata> {
    history
        .iter()
        .map(|message| {
            let Some(idx) = recent_records.iter().position(|record| {
                record.message.role == message.role && record.message.content == message.content
            }) else {
                return CompactionMessageMetadata::default();
            };
            let record = recent_records.remove(idx);
            CompactionMessageMetadata {
                source_id: Some(record.id),
                created_at: Some(record.created_at),
            }
        })
        .collect()
}

fn build_compaction_source_key(
    history: &[StoredMessage],
    strategy: &CompactionStrategy,
    metadata: &[CompactionMessageMetadata],
) -> CompactionSourceKey {
    let strategy = compaction_strategy_name(strategy);
    let mut hasher = Sha256::new();
    update_compaction_source_hash(&mut hasher, "strategy", &strategy);
    update_compaction_source_hash(&mut hasher, "history_len", &history.len().to_string());
    for message in history {
        update_compaction_source_hash(&mut hasher, "message.role", message.role.as_str());
        update_compaction_source_hash(&mut hasher, "message.content", &message.content);
    }
    for meta in metadata {
        update_compaction_source_hash(
            &mut hasher,
            "metadata.source_id",
            &meta
                .source_id
                .map(|value| value.to_string())
                .unwrap_or_default(),
        );
        update_compaction_source_hash(
            &mut hasher,
            "metadata.created_at",
            &meta
                .created_at
                .map(|value| value.to_string())
                .unwrap_or_default(),
        );
    }
    let source_message_ids = metadata
        .iter()
        .filter_map(|meta| meta.source_id)
        .collect::<Vec<_>>();
    let source_first_message_id = source_message_ids.first().copied();
    let source_last_message_id = source_message_ids.last().copied();

    CompactionSourceKey {
        strategy,
        source_hash: format!("{:x}", hasher.finalize()),
        source_message_ids,
        source_first_message_id,
        source_last_message_id,
        source_message_count: history.len(),
    }
}

fn update_compaction_source_hash(hasher: &mut Sha256, label: &str, value: &str) {
    hasher.update(label.as_bytes());
    hasher.update([0]);
    hasher.update(value.len().to_string().as_bytes());
    hasher.update([0]);
    hasher.update(value.as_bytes());
    hasher.update([0]);
}

fn compaction_strategy_name(strategy: &CompactionStrategy) -> String {
    match strategy {
        CompactionStrategy::HeadTail {
            head_count,
            tail_count,
        } => format!("head_tail:{head_count}:{tail_count}"),
        CompactionStrategy::AgeBased { max_age_days } => format!("age_based:{max_age_days}"),
        CompactionStrategy::BudgetBased { max_tokens } => format!("budget_based:{max_tokens}"),
    }
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

#[cfg(test)]
mod tests;
