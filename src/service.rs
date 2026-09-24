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
mod swarm;

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

    pub async fn get_trajectory_detail(
        &self,
        trajectory_id: String,
    ) -> anyhow::Result<Option<crate::harness::trajectory::TrajectoryRecord>> {
        let Some(store) = &self.harness_store else {
            return Ok(None);
        };
        store.get_trajectory(&trajectory_id).await
    }

    pub async fn query_trajectories(
        &self,
        filter: crate::harness::trajectory::TrajectoryFilter,
    ) -> anyhow::Result<Vec<crate::harness::trajectory::TrajectoryRecord>> {
        let Some(store) = &self.harness_store else {
            return Ok(Vec::new());
        };
        store.query_trajectories(filter).await
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
            .complete(CompletionRequest::json(vec![
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
                ]))
            .await
            .context("failed to detect telegram scheduler intent")?;

        Ok(parse_scheduler_intent_json(&reply))
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

#[derive(Debug, Clone, Copy)]
struct TimezoneHint {
    timezone: &'static str,
    display: &'static str,
    source: &'static str,
    note: Option<&'static str>,
}

fn extract_primary_user_text(text: &str) -> &str {
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(raw) = trimmed.strip_prefix("原始消息:") {
            let raw = raw.trim();
            if !raw.is_empty() {
                return raw;
            }
        }
    }
    text.trim()
}

fn looks_like_time_query(text: &str) -> bool {
    let trimmed = extract_primary_user_text(text).trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    if [
        "what time",
        "current time",
        "time now",
        "today",
        "date",
        "year",
        "weekday",
        "zodiac",
        "now",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
    {
        return true;
    }
    [
        "几点", "时间", "现在", "今天", "日期", "几号", "今年", "年份", "星期", "周几", "生肖",
        "蛇年", "龙年", "马年", "羊年", "猴年", "鸡年", "狗年", "猪年", "鼠年", "牛年", "虎年",
        "兔年",
    ]
    .iter()
    .any(|kw| trimmed.contains(kw))
}

fn is_explicit_current_time_query(text: &str) -> bool {
    let trimmed = extract_primary_user_text(text).trim();
    if trimmed.is_empty() || !looks_like_time_query(trimmed) {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    if [
        "提醒",
        "分钟后",
        "小时后",
        "之后",
        "定时",
        "闹钟",
        "every ",
        "cron",
        "remind",
        "schedule",
    ]
    .iter()
    .any(|kw| trimmed.contains(kw) || lower.contains(kw))
    {
        return false;
    }

    let explicit_cn = [
        "几点",
        "星期几",
        "周几",
        "几号",
        "日期",
        "哪年",
        "今年",
        "生肖",
    ];
    let explicit_en = [
        "what time",
        "time is it",
        "current time",
        "what date",
        "what day",
        "weekday",
        "what year",
        "zodiac",
    ];
    if explicit_cn.iter().any(|kw| trimmed.contains(kw))
        || explicit_en.iter().any(|kw| lower.contains(kw))
    {
        return true;
    }

    (trimmed.contains("现在") || lower.contains("now"))
        && (trimmed.contains("时间") || lower.contains("time"))
        && (trimmed.contains('？') || trimmed.contains('?') || trimmed.ends_with('吗'))
}

fn infer_timezone_from_time_query(text: &str) -> Option<TimezoneHint> {
    let trimmed = extract_primary_user_text(text).trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();

    if ["美西", "洛杉矶", "pacific", "los angeles", "pst", "pdt"]
        .iter()
        .any(|kw| trimmed.contains(kw) || lower.contains(kw))
    {
        return Some(TimezoneHint {
            timezone: "America/Los_Angeles",
            display: "美国西部时间",
            source: "us_west",
            note: None,
        });
    }
    if ["美东", "纽约", "eastern", "new york", "est", "edt"]
        .iter()
        .any(|kw| trimmed.contains(kw) || lower.contains(kw))
    {
        return Some(TimezoneHint {
            timezone: "America/New_York",
            display: "美国东部时间",
            source: "us_east",
            note: None,
        });
    }
    if [
        "美国",
        "美利坚",
        "america",
        "usa",
        "us time",
        "united states",
    ]
    .iter()
    .any(|kw| trimmed.contains(kw) || lower.contains(kw))
    {
        return Some(TimezoneHint {
            timezone: "America/New_York",
            display: "美国东部时间",
            source: "us_default",
            note: Some("美国有多个时区，当前默认按美国东部时间。"),
        });
    }
    if ["中国", "国内", "北京时间", "北京", "china", "beijing"]
        .iter()
        .any(|kw| trimmed.contains(kw) || lower.contains(kw))
    {
        return Some(TimezoneHint {
            timezone: "Asia/Shanghai",
            display: "北京时间",
            source: "china",
            note: None,
        });
    }
    if ["日本", "东京", "japan", "tokyo", "jst"]
        .iter()
        .any(|kw| trimmed.contains(kw) || lower.contains(kw))
    {
        return Some(TimezoneHint {
            timezone: "Asia/Tokyo",
            display: "日本时间",
            source: "japan",
            note: None,
        });
    }
    if ["韩国", "首尔", "korea", "seoul", "kst"]
        .iter()
        .any(|kw| trimmed.contains(kw) || lower.contains(kw))
    {
        return Some(TimezoneHint {
            timezone: "Asia/Seoul",
            display: "韩国时间",
            source: "korea",
            note: None,
        });
    }
    if ["英国", "伦敦", "uk", "britain", "london"]
        .iter()
        .any(|kw| trimmed.contains(kw) || lower.contains(kw))
    {
        return Some(TimezoneHint {
            timezone: "Europe/London",
            display: "英国时间",
            source: "uk",
            note: None,
        });
    }
    if ["utc", "gmt"].iter().any(|kw| lower.contains(kw)) {
        return Some(TimezoneHint {
            timezone: "UTC",
            display: "UTC",
            source: "utc",
            note: None,
        });
    }

    None
}

fn build_builtin_time_tool_arguments(timezone_hint: Option<TimezoneHint>) -> Value {
    match timezone_hint {
        Some(hint) => serde_json::json!({ "timezone": hint.timezone }),
        None => serde_json::json!({}),
    }
}

fn render_fast_time_answer(
    query_text: &str,
    tool_result: &Value,
    timezone_hint: Option<TimezoneHint>,
) -> String {
    let query = extract_primary_user_text(query_text).trim();
    let lower = query.to_ascii_lowercase();
    let local_time = tool_result
        .get("local_time")
        .and_then(|v| v.as_str())
        .unwrap_or("--:--:--");
    let local_date = tool_result
        .get("local_date")
        .and_then(|v| v.as_str())
        .unwrap_or("----/--/--");
    let local_weekday = tool_result
        .get("local_weekday")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown");
    let weekday_zh = weekday_to_chinese(local_weekday);
    let local_year = tool_result
        .get("local_year")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let zodiac = zodiac_for_year(local_year as i32);
    let timezone = tool_result
        .get("timezone")
        .and_then(|v| v.as_str())
        .or_else(|| timezone_hint.map(|hint| hint.timezone))
        .unwrap_or("local");
    let timezone_display = timezone_hint.map(|hint| hint.display).unwrap_or("本地时间");

    let wants_time = ["几点", "时间", "what time", "time now", "current time"]
        .iter()
        .any(|kw| query.contains(kw) || lower.contains(kw));
    let wants_date = ["几号", "日期", "today", "date"]
        .iter()
        .any(|kw| query.contains(kw) || lower.contains(kw));
    let wants_weekday = ["星期", "周几", "weekday", "what day"]
        .iter()
        .any(|kw| query.contains(kw) || lower.contains(kw));
    let wants_year = ["今年", "年份", "哪年", "year"]
        .iter()
        .any(|kw| query.contains(kw) || lower.contains(kw));
    let wants_zodiac = [
        "生肖", "zodiac", "蛇年", "龙年", "马年", "羊年", "猴年", "鸡年", "狗年", "猪年", "鼠年",
        "牛年", "虎年", "兔年",
    ]
    .iter()
    .any(|kw| query.contains(kw) || lower.contains(kw));

    let mut parts = Vec::new();
    if wants_time || (!wants_date && !wants_weekday && !wants_year && !wants_zodiac) {
        parts.push(format!(
            "当前{}（{}）是 {}",
            timezone_display, timezone, local_time
        ));
    }
    if wants_date {
        parts.push(format!("当前日期是 {}", local_date));
    }
    if wants_weekday {
        parts.push(format!("今天是星期{}", weekday_zh));
    }
    if wants_year || wants_zodiac {
        parts.push(format!("当前年份是 {} 年（{}）", local_year, zodiac));
    }
    if parts.is_empty() {
        parts.push(format!(
            "当前{}（{}）是 {} {}，星期{}",
            timezone_display, timezone, local_date, local_time, weekday_zh
        ));
    }

    let mut answer = parts.join("；");
    if let Some(note) = timezone_hint.and_then(|hint| hint.note) {
        answer.push('。');
        answer.push_str(note);
    }
    answer
}

fn weekday_to_chinese(weekday_en: &str) -> &'static str {
    match weekday_en {
        "Monday" => "一",
        "Tuesday" => "二",
        "Wednesday" => "三",
        "Thursday" => "四",
        "Friday" => "五",
        "Saturday" => "六",
        "Sunday" => "日",
        _ => "?",
    }
}

fn zodiac_for_year(year: i32) -> &'static str {
    const SIGNS: [&str; 12] = [
        "鼠年", "牛年", "虎年", "兔年", "龙年", "蛇年", "马年", "羊年", "猴年", "鸡年", "狗年",
        "猪年",
    ];
    if year <= 0 {
        return "未知生肖";
    }
    let idx = (year - 4).rem_euclid(12) as usize;
    SIGNS[idx]
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
fn truncate_json_value(value: &Value, max_chars: usize) -> Value {
    crate::harness::tool_protocol::truncate_json_value(value, max_chars)
}

#[cfg(test)]
mod tests {
    use super::completion::{
        CodeModeCircuitChange, build_mcp_system_prompt, chunk_text_for_stream_replay,
        code_mode_timeout_ratio, next_code_mode_timeout_streak, parse_mcp_tool_call,
        should_open_code_mode_timeout_circuit, should_probe_code_mode_timeout_circuit,
        should_warn_code_mode_timeouts,
    };
    use super::{
        AgentCompactionSettings, AgentMcpSettings, BUILTIN_MCP_SERVER_NAME,
        BUILTIN_MCP_TOOL_CURRENT_TIME, MessageService, apply_context_budget,
        build_compaction_source_key, extract_primary_user_text, infer_timezone_from_time_query,
        is_explicit_current_time_query, looks_like_time_query, parse_scheduler_intent_json,
        render_fast_time_answer, truncate_json_value, weekday_to_chinese, zodiac_for_year,
    };
    use crate::code_mode::{AgentCodeModeSettings, CodeModeAuditRecord};
    use crate::domain::{MessageRole, StoredMessage};
    use crate::harness::compactor::{CompactionMessageMetadata, CompactionStrategy};
    use crate::mcp::McpRuntime;
    use crate::memory::{MemoryBackend, SqliteMemoryBackend, SqliteMemoryStore};
    use crate::provider::{ChatProvider, CompletionRequest, StreamSink};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::sync::RwLock;

    struct FakeProvider;

    #[async_trait::async_trait]
    impl ChatProvider for FakeProvider {
        async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
            Ok("ok".to_string())
        }
    }

    struct CountingSummaryProvider {
        summary_calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ChatProvider for CountingSummaryProvider {
        async fn complete(&self, req: CompletionRequest) -> anyhow::Result<String> {
            if req
                .messages
                .first()
                .is_some_and(|message| message.content.starts_with("Summarize the following"))
            {
                self.summary_calls.fetch_add(1, Ordering::SeqCst);
                return Ok("cached reusable summary".to_string());
            }
            Ok("ok".to_string())
        }
    }

    struct NeverProvider;

    #[async_trait::async_trait]
    impl ChatProvider for NeverProvider {
        async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
            anyhow::bail!("provider should not be called in time fast path")
        }

        async fn complete_stream(
            &self,
            _req: CompletionRequest,
            _sink: &mut dyn StreamSink,
        ) -> anyhow::Result<String> {
            anyhow::bail!("provider stream should not be called in time fast path")
        }
    }

    struct SequenceStreamProvider {
        replies: Arc<Mutex<Vec<String>>>,
    }

    impl SequenceStreamProvider {
        fn next_reply(&self) -> String {
            let mut guard = self.replies.lock().expect("reply lock");
            if guard.is_empty() {
                String::new()
            } else {
                guard.remove(0)
            }
        }
    }

    #[async_trait::async_trait]
    impl ChatProvider for SequenceStreamProvider {
        async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
            Ok(self.next_reply())
        }

        async fn complete_stream(
            &self,
            _req: CompletionRequest,
            sink: &mut dyn StreamSink,
        ) -> anyhow::Result<String> {
            let reply = self.next_reply();
            let chars = reply.chars().collect::<Vec<_>>();
            for chunk in chars.chunks(6) {
                let delta = chunk.iter().collect::<String>();
                sink.on_delta(&delta).await?;
            }
            Ok(reply)
        }
    }

    struct SequenceNoDeltaStreamProvider {
        replies: Arc<Mutex<Vec<String>>>,
    }

    impl SequenceNoDeltaStreamProvider {
        fn next_reply(&self) -> String {
            let mut guard = self.replies.lock().expect("reply lock");
            if guard.is_empty() {
                String::new()
            } else {
                guard.remove(0)
            }
        }
    }

    #[async_trait::async_trait]
    impl ChatProvider for SequenceNoDeltaStreamProvider {
        async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
            Ok(self.next_reply())
        }

        async fn complete_stream(
            &self,
            _req: CompletionRequest,
            _sink: &mut dyn StreamSink,
        ) -> anyhow::Result<String> {
            Ok(self.next_reply())
        }
    }

    #[derive(Default)]
    struct CaptureSink {
        text: String,
    }

    #[async_trait::async_trait]
    impl StreamSink for CaptureSink {
        async fn on_delta(&mut self, delta: &str) -> anyhow::Result<()> {
            self.text.push_str(delta);
            Ok(())
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
    fn stream_replay_chunker_preserves_text_and_bounds_chunk_size() {
        let text = "abcdefghijklmnopqrstuvwxyz";
        let chunks = chunk_text_for_stream_replay(text, 5);
        assert_eq!(chunks.concat(), text);
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 5));
        assert!(chunks.len() > 1);
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
    fn mcp_system_prompt_includes_builtin_time_rule() {
        let prompt = build_mcp_system_prompt(&[]).expect("prompt");
        assert!(prompt.contains(BUILTIN_MCP_SERVER_NAME));
        assert!(prompt.contains(BUILTIN_MCP_TOOL_CURRENT_TIME));
        assert!(prompt.contains("Time rule"));
    }

    #[test]
    fn looks_like_time_query_detects_cn_and_en_intents() {
        assert!(looks_like_time_query("现在几点了？"));
        assert!(looks_like_time_query("今天几号"));
        assert!(looks_like_time_query("今年是什么生肖"));
        assert!(looks_like_time_query("what time is it now"));
    }

    #[test]
    fn looks_like_time_query_ignores_regular_reminder_text() {
        assert!(!looks_like_time_query("5分钟后提醒我喝水"));
        assert!(!looks_like_time_query("驴哥，帮我总结一下上面的讨论"));
    }

    #[test]
    fn extract_primary_user_text_reads_group_wrapped_message() {
        let wrapped = "[群成员身份映射]\n当前发言账号: uid=1\n原始消息: 现在美国时间是几点？\n[/群成员身份映射]";
        assert_eq!(extract_primary_user_text(wrapped), "现在美国时间是几点？");
    }

    #[test]
    fn time_query_timezone_inference_defaults_us_to_eastern() {
        let inferred = infer_timezone_from_time_query("现在美国时间是几点？").expect("tz");
        assert_eq!(inferred.timezone, "America/New_York");
        assert_eq!(inferred.source, "us_default");
    }

    #[test]
    fn explicit_time_query_filter_ignores_scheduler_sentence() {
        assert!(!is_explicit_current_time_query("2分钟后提醒我看看时间"));
        assert!(is_explicit_current_time_query("今天是星期几？"));
    }

    #[test]
    fn render_fast_time_answer_formats_weekday_and_year() {
        let answer = render_fast_time_answer(
            "今天是星期几，今年什么生肖？",
            &serde_json::json!({
                "local_date": "2026-02-26",
                "local_time": "09:32:00",
                "local_weekday": "Thursday",
                "local_year": 2026,
                "timezone": "Asia/Shanghai"
            }),
            None,
        );
        assert!(answer.contains("星期四"));
        assert!(answer.contains("2026"));
        assert!(answer.contains("马年"));
    }

    #[test]
    fn weekday_and_zodiac_helpers_return_expected_values() {
        assert_eq!(weekday_to_chinese("Monday"), "一");
        assert_eq!(weekday_to_chinese("Thursday"), "四");
        assert_eq!(zodiac_for_year(2025), "蛇年");
        assert_eq!(zodiac_for_year(2026), "马年");
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
    fn parse_scheduler_intent_json_extracts_object_from_wrapped_text() {
        let parsed = parse_scheduler_intent_json(
            "好的，已解析如下：\n```json\n{\"action\":\"create\",\"confidence\":0.92,\"task_kind\":\"reminder\",\"payload\":\"喝水\",\"schedule_kind\":\"once\",\"run_at\":\"1772067600\",\"cron_expr\":null,\"timezone\":\"Asia/Shanghai\",\"job_id\":null,\"job_operation\":null}\n```\n请确认",
        )
        .expect("parsed");
        assert_eq!(parsed.action, "create");
        assert_eq!(parsed.payload.as_deref(), Some("喝水"));
    }

    #[test]
    fn parse_mcp_tool_call_extracts_object_from_wrapped_text() {
        let parsed = parse_mcp_tool_call(
            "我准备调用工具：\n{\"server\":\"builtin\",\"tool\":\"current_time\",\"arguments\":{\"timezone\":\"Asia/Shanghai\"}}\n然后返回结果",
        )
        .expect("parsed");
        assert_eq!(parsed.server, "builtin");
        assert_eq!(parsed.tool, "current_time");
        assert_eq!(parsed.arguments["timezone"], "Asia/Shanghai");
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

    #[tokio::test]
    async fn compaction_reuses_persisted_summary_for_same_source_window() {
        let store = SqliteMemoryStore::new("sqlite::memory:")
            .await
            .expect("init store");
        for idx in 0..12 {
            store
                .append(
                    "session-compact-cache",
                    StoredMessage {
                        role: if idx % 2 == 0 {
                            MessageRole::User
                        } else {
                            MessageRole::Assistant
                        },
                        content: format!("Message {idx} {}", "long context ".repeat(24)),
                    },
                )
                .await
                .expect("append message");
        }

        let summary_calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(CountingSummaryProvider {
            summary_calls: summary_calls.clone(),
        });
        let memory: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(store.clone()));
        let service = MessageService::new_with_backend(
            provider,
            memory,
            None,
            AgentMcpSettings::default(),
            12,
            0,
            0,
        )
        .with_context_budget(4000, 512, 35, 4)
        .with_compaction(AgentCompactionSettings {
            enabled: true,
            strategy: CompactionStrategy::HeadTail {
                head_count: 1,
                tail_count: 1,
            },
            head_count: 1,
            tail_count: 1,
            message_count_threshold: 4,
        });
        let history = store
            .load_recent("session-compact-cache", 12)
            .await
            .expect("load history");

        let first = service
            .apply_compaction(history.clone(), "session-compact-cache")
            .await
            .expect("first compaction");
        let second = service
            .apply_compaction(history, "session-compact-cache")
            .await
            .expect("second compaction");

        assert_eq!(summary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(first, second);
        assert!(
            second
                .iter()
                .any(|message| message.content.contains("cached reusable summary"))
        );
    }

    #[test]
    fn compaction_source_key_uses_stable_sha256_digest() {
        let history = vec![
            StoredMessage {
                role: MessageRole::User,
                content: "first".to_string(),
            },
            StoredMessage {
                role: MessageRole::Assistant,
                content: "second".to_string(),
            },
        ];
        let metadata = vec![
            CompactionMessageMetadata {
                source_id: Some(10),
                created_at: Some(1000),
            },
            CompactionMessageMetadata {
                source_id: Some(11),
                created_at: Some(1001),
            },
        ];

        let key = build_compaction_source_key(
            &history,
            &CompactionStrategy::HeadTail {
                head_count: 1,
                tail_count: 2,
            },
            &metadata,
        );

        assert_eq!(
            key.source_hash,
            "391ded3c62b5ac3c864f424fbf622c15bd2381556a366ef0e88e312731cccc3e"
        );
        assert_eq!(key.source_message_ids, vec![10, 11]);
        assert_eq!(key.source_first_message_id, Some(10));
        assert_eq!(key.source_last_message_id, Some(11));
        assert_eq!(key.source_message_count, 2);
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

    #[tokio::test]
    async fn handle_stream_supports_mcp_tool_loop() {
        let provider = Arc::new(SequenceStreamProvider {
            replies: Arc::new(Mutex::new(vec![
                format!(
                    r#"{{"server":"{BUILTIN_MCP_SERVER_NAME}","tool":"{BUILTIN_MCP_TOOL_CURRENT_TIME}","arguments":{{"timezone":"Asia/Shanghai"}}}}"#
                ),
                "现在是正确的当前时间。".to_string(),
            ])),
        });
        let store = SqliteMemoryStore::new("sqlite::memory:")
            .await
            .expect("store");
        let memory: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(store));
        let service = MessageService::new_with_backend(
            provider,
            memory,
            Some(Arc::new(RwLock::new(McpRuntime::default()))),
            AgentMcpSettings::default(),
            8,
            0,
            0,
        );

        let mut sink = CaptureSink::default();
        let out = service
            .handle_stream(
                crate::domain::IncomingMessage {
                    channel: "telegram".to_string(),
                    session_id: "tg:test:stream".to_string(),
                    user_id: "u1".to_string(),
                    text: "请调用工具并给我最终结论".to_string(),
                    reply_target: None,
                },
                &mut sink,
            )
            .await
            .expect("handle stream with mcp loop");

        assert_eq!(out.text, "现在是正确的当前时间。");
        assert_eq!(sink.text, "现在是正确的当前时间。");
        assert!(!sink.text.contains("\"server\""));
    }

    #[tokio::test]
    async fn handle_stream_supports_multi_iteration_mcp_tool_loop() {
        let provider = Arc::new(SequenceStreamProvider {
            replies: Arc::new(Mutex::new(vec![
                format!(
                    r#"{{"server":"{BUILTIN_MCP_SERVER_NAME}","tool":"{BUILTIN_MCP_TOOL_CURRENT_TIME}","arguments":{{"timezone":"Asia/Shanghai"}}}}"#
                ),
                format!(
                    r#"{{"server":"{BUILTIN_MCP_SERVER_NAME}","tool":"{BUILTIN_MCP_TOOL_CURRENT_TIME}","arguments":{{"timezone":"America/New_York"}}}}"#
                ),
                "这是多轮工具调用后的最终回答。".to_string(),
            ])),
        });
        let store = SqliteMemoryStore::new("sqlite::memory:")
            .await
            .expect("store");
        let memory: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(store));
        let service = MessageService::new_with_backend(
            provider,
            memory,
            Some(Arc::new(RwLock::new(McpRuntime::default()))),
            AgentMcpSettings::default(),
            8,
            0,
            0,
        );

        let mut sink = CaptureSink::default();
        let out = service
            .handle_stream(
                crate::domain::IncomingMessage {
                    channel: "telegram".to_string(),
                    session_id: "tg:test:stream-multi-loop".to_string(),
                    user_id: "u1".to_string(),
                    text: "请连续调用两次工具后再回答".to_string(),
                    reply_target: None,
                },
                &mut sink,
            )
            .await
            .expect("handle stream with multi mcp loop");

        assert_eq!(out.text, "这是多轮工具调用后的最终回答。");
        assert_eq!(sink.text, "这是多轮工具调用后的最终回答。");
        assert!(!sink.text.contains("\"server\""));
    }

    #[tokio::test]
    async fn handle_stream_supports_fenced_json_tool_call() {
        let provider = Arc::new(SequenceStreamProvider {
            replies: Arc::new(Mutex::new(vec![
                format!(
                    "```json\n{{\"tool_call\":{{\"server\":\"{BUILTIN_MCP_SERVER_NAME}\",\"tool\":\"{BUILTIN_MCP_TOOL_CURRENT_TIME}\",\"arguments\":{{\"timezone\":\"Asia/Shanghai\"}}}}}}\n```"
                ),
                "fenced json tool call 已成功执行。".to_string(),
            ])),
        });
        let store = SqliteMemoryStore::new("sqlite::memory:")
            .await
            .expect("store");
        let memory: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(store));
        let service = MessageService::new_with_backend(
            provider,
            memory,
            Some(Arc::new(RwLock::new(McpRuntime::default()))),
            AgentMcpSettings::default(),
            8,
            0,
            0,
        );

        let mut sink = CaptureSink::default();
        let out = service
            .handle_stream(
                crate::domain::IncomingMessage {
                    channel: "telegram".to_string(),
                    session_id: "tg:test:stream-fenced".to_string(),
                    user_id: "u1".to_string(),
                    text: "请用 fenced json 发起工具调用".to_string(),
                    reply_target: None,
                },
                &mut sink,
            )
            .await
            .expect("handle stream with fenced json tool call");

        assert_eq!(out.text, "fenced json tool call 已成功执行。");
        assert_eq!(sink.text, "fenced json tool call 已成功执行。");
        assert!(!sink.text.contains("\"tool_call\""));
    }

    #[tokio::test]
    async fn handle_stream_continues_after_mcp_tool_error() {
        let provider = Arc::new(SequenceStreamProvider {
            replies: Arc::new(Mutex::new(vec![
                format!(
                    r#"{{"server":"{BUILTIN_MCP_SERVER_NAME}","tool":"no_such_tool","arguments":{{}}}}"#
                ),
                "我捕获到了工具调用失败，并继续给出最终回答。".to_string(),
            ])),
        });
        let store = SqliteMemoryStore::new("sqlite::memory:")
            .await
            .expect("store");
        let memory: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(store));
        let service = MessageService::new_with_backend(
            provider,
            memory,
            Some(Arc::new(RwLock::new(McpRuntime::default()))),
            AgentMcpSettings::default(),
            8,
            0,
            0,
        );

        let mut sink = CaptureSink::default();
        let out = service
            .handle_stream(
                crate::domain::IncomingMessage {
                    channel: "telegram".to_string(),
                    session_id: "tg:test:stream-tool-error".to_string(),
                    user_id: "u1".to_string(),
                    text: "请调用一个不存在的工具后继续回答".to_string(),
                    reply_target: None,
                },
                &mut sink,
            )
            .await
            .expect("handle stream should continue after tool error");

        assert_eq!(out.text, "我捕获到了工具调用失败，并继续给出最终回答。");
        assert_eq!(sink.text, "我捕获到了工具调用失败，并继续给出最终回答。");
    }

    #[tokio::test]
    async fn handle_stream_parses_tool_call_even_when_provider_emits_no_deltas() {
        let provider = Arc::new(SequenceNoDeltaStreamProvider {
            replies: Arc::new(Mutex::new(vec![
                format!(
                    r#"{{"server":"{BUILTIN_MCP_SERVER_NAME}","tool":"{BUILTIN_MCP_TOOL_CURRENT_TIME}","arguments":{{"timezone":"Asia/Shanghai"}}}}"#
                ),
                "无delta流式也能完成工具链路。".to_string(),
            ])),
        });
        let store = SqliteMemoryStore::new("sqlite::memory:")
            .await
            .expect("store");
        let memory: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(store));
        let service = MessageService::new_with_backend(
            provider,
            memory,
            Some(Arc::new(RwLock::new(McpRuntime::default()))),
            AgentMcpSettings::default(),
            8,
            0,
            0,
        );

        let mut sink = CaptureSink::default();
        let out = service
            .handle_stream(
                crate::domain::IncomingMessage {
                    channel: "telegram".to_string(),
                    session_id: "tg:test:stream-no-delta".to_string(),
                    user_id: "u1".to_string(),
                    text: "即使没有delta也要完成工具调用".to_string(),
                    reply_target: None,
                },
                &mut sink,
            )
            .await
            .expect("handle stream should parse tool call from fallback reply text");

        assert_eq!(out.text, "无delta流式也能完成工具链路。");
        assert_eq!(sink.text, "无delta流式也能完成工具链路。");
    }

    #[tokio::test]
    async fn handle_uses_builtin_time_fast_path_without_provider_call() {
        let store = SqliteMemoryStore::new("sqlite::memory:")
            .await
            .expect("store");
        let memory: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(store));
        let service = MessageService::new_with_backend(
            Arc::new(NeverProvider),
            memory,
            Some(Arc::new(RwLock::new(McpRuntime::default()))),
            AgentMcpSettings::default(),
            8,
            0,
            0,
        );

        let out = service
            .handle(crate::domain::IncomingMessage {
                channel: "telegram".to_string(),
                session_id: "tg:test:fast-time".to_string(),
                user_id: "u1".to_string(),
                text: "现在美国时间是几点？".to_string(),
                reply_target: None,
            })
            .await
            .expect("fast time query should bypass provider");

        assert!(out.text.contains("美国东部时间"));
        assert!(out.text.contains("America/New_York"));
    }

    #[tokio::test]
    async fn handle_stream_uses_builtin_time_fast_path_without_provider_call() {
        let store = SqliteMemoryStore::new("sqlite::memory:")
            .await
            .expect("store");
        let memory: Arc<dyn MemoryBackend> = Arc::new(SqliteMemoryBackend::new(store));
        let service = MessageService::new_with_backend(
            Arc::new(NeverProvider),
            memory,
            Some(Arc::new(RwLock::new(McpRuntime::default()))),
            AgentMcpSettings::default(),
            8,
            0,
            0,
        );

        let mut sink = CaptureSink::default();
        let out = service
            .handle_stream(
                crate::domain::IncomingMessage {
                    channel: "telegram".to_string(),
                    session_id: "tg:test:fast-time-stream".to_string(),
                    user_id: "u1".to_string(),
                    text: "今天星期几？".to_string(),
                    reply_target: None,
                },
                &mut sink,
            )
            .await
            .expect("stream time query should bypass provider");

        assert_eq!(sink.text, out.text);
        assert!(out.text.contains("星期"));
    }
}
