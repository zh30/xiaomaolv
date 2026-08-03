use std::collections::{BTreeMap, HashSet};
use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;

use anyhow::{Context, bail, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, RwLock};

use crate::domain::{MessageRole, StoredMessage};
use crate::harness::store::{EvolutionStore, HarnessStore, new_evolution_id};
use crate::harness::trajectory::{TrajectoryExitReason, TrajectoryFilter, TrajectoryRecord};
use crate::provider::{ChatProvider, CompletionRequest};

pub const ABSOLUTE_MAX_PROMPT_PATCH_CHARS: usize = 16_000;
pub const MAX_ENABLED_EVOLUTION_EVAL_CASES: usize = 50;
const MAX_EVAL_OUTPUT_EXCERPT_CHARS: usize = 2_000;
const MAX_EVAL_CASE_INPUT_CHARS: usize = 8_000;
const MAX_PROPOSAL_RESPONSE_CHARS: usize = 20_000;
const MAX_CANDIDATE_RATIONALE_CHARS: usize = 4_000;

const RESERVED_HARNESS_MARKERS: &[&str] = &[
    "MCP_TOOL_RESULT_JSON",
    "MCP_TOOL_VERIFICATION_FAILED_JSON",
    "CODE_MODE_TOOL_RESULT_JSON",
    "MCP_TOOLS_JSON",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct PromptPatch(String);

impl PromptPatch {
    pub fn new(value: impl Into<String>, max_chars: usize) -> anyhow::Result<Self> {
        let value = value.into();
        let trimmed = value.trim();
        if trimmed.is_empty() {
            bail!("prompt patch cannot be empty");
        }

        let effective_max = max_chars.min(ABSOLUTE_MAX_PROMPT_PATCH_CHARS);
        let char_count = trimmed.chars().count();
        if char_count > effective_max {
            bail!("prompt patch exceeds the configured limit of {effective_max} characters");
        }

        if let Some(marker) = RESERVED_HARNESS_MARKERS
            .iter()
            .find(|marker| trimmed.contains(**marker))
        {
            bail!("prompt patch contains reserved harness marker '{marker}'");
        }

        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct EvolutionCaseAssertions {
    #[serde(default)]
    pub required_substrings: Vec<String>,
    #[serde(default)]
    pub forbidden_substrings: Vec<String>,
    #[serde(default)]
    pub require_json: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvolutionEvalCase {
    pub id: String,
    pub name: String,
    pub input: String,
    pub assertions: EvolutionCaseAssertions,
    pub weight: f64,
    pub enabled: bool,
}

impl EvolutionEvalCase {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.id.trim().is_empty() {
            bail!("eval case id cannot be empty");
        }
        if self.id.len() > 128 {
            bail!("eval case id cannot exceed 128 bytes");
        }
        if self.name.trim().is_empty() {
            bail!("eval case name cannot be empty");
        }
        if self.name.chars().count() > 256 {
            bail!("eval case name cannot exceed 256 characters");
        }
        if self.input.trim().is_empty() {
            bail!("eval case input cannot be empty");
        }
        if self.input.chars().count() > MAX_EVAL_CASE_INPUT_CHARS {
            bail!("eval case input cannot exceed {MAX_EVAL_CASE_INPUT_CHARS} characters");
        }
        if !self.weight.is_finite() || self.weight <= 0.0 || self.weight > 100.0 {
            bail!("eval case weight must be finite, greater than zero, and at most 100");
        }
        if self.assertions.required_substrings.is_empty()
            && self.assertions.forbidden_substrings.is_empty()
            && !self.assertions.require_json
        {
            bail!("eval case must define at least one assertion");
        }
        let assertion_count =
            self.assertions.required_substrings.len() + self.assertions.forbidden_substrings.len();
        if assertion_count > 64 {
            bail!("eval case cannot define more than 64 substring assertions");
        }
        if self
            .assertions
            .required_substrings
            .iter()
            .chain(self.assertions.forbidden_substrings.iter())
            .any(|value| value.trim().is_empty() || value.chars().count() > 512)
        {
            bail!("eval case substrings must contain between 1 and 512 characters");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvolutionCaseResult {
    pub case_id: String,
    pub case_name: String,
    pub input: String,
    pub weight: f64,
    pub assertions: EvolutionCaseAssertions,
    pub baseline_passed: bool,
    pub candidate_passed: bool,
    pub baseline_issues: Vec<String>,
    pub candidate_issues: Vec<String>,
    pub baseline_output_excerpt: String,
    pub candidate_output_excerpt: String,
    pub baseline_output_sha256: String,
    pub candidate_output_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvolutionScorecard {
    pub baseline_score: f64,
    pub candidate_score: f64,
    pub score_delta: f64,
    pub regressions: usize,
    pub baseline_passed_cases: usize,
    pub candidate_passed_cases: usize,
    pub total_cases: usize,
    pub case_results: Vec<EvolutionCaseResult>,
}

pub struct EvolutionScorer;

impl EvolutionScorer {
    pub fn score(
        cases: &[EvolutionEvalCase],
        baseline_outputs: &BTreeMap<String, String>,
        candidate_outputs: &BTreeMap<String, String>,
    ) -> anyhow::Result<EvolutionScorecard> {
        let enabled_cases = cases.iter().filter(|case| case.enabled).collect::<Vec<_>>();
        if enabled_cases.is_empty() {
            bail!("at least one enabled eval case is required");
        }
        if enabled_cases.len() > MAX_ENABLED_EVOLUTION_EVAL_CASES {
            bail!("enabled eval cases exceed the maximum of {MAX_ENABLED_EVOLUTION_EVAL_CASES}");
        }

        let mut baseline_passed_weight = 0.0;
        let mut candidate_passed_weight = 0.0;
        let mut total_weight = 0.0;
        let mut baseline_passed_cases = 0;
        let mut candidate_passed_cases = 0;
        let mut regressions = 0;
        let mut case_results = Vec::with_capacity(enabled_cases.len());

        for case in enabled_cases {
            case.validate()
                .with_context(|| format!("invalid eval case '{}'", case.id))?;
            let baseline_output = baseline_outputs
                .get(&case.id)
                .with_context(|| format!("missing baseline output for eval case '{}'", case.id))?;
            let candidate_output = candidate_outputs
                .get(&case.id)
                .with_context(|| format!("missing candidate output for eval case '{}'", case.id))?;

            let baseline_issues = evaluate_assertions(&case.assertions, baseline_output);
            let candidate_issues = evaluate_assertions(&case.assertions, candidate_output);
            let baseline_passed = baseline_issues.is_empty();
            let candidate_passed = candidate_issues.is_empty();

            total_weight += case.weight;
            if baseline_passed {
                baseline_passed_weight += case.weight;
                baseline_passed_cases += 1;
            }
            if candidate_passed {
                candidate_passed_weight += case.weight;
                candidate_passed_cases += 1;
            }
            if baseline_passed && !candidate_passed {
                regressions += 1;
            }

            case_results.push(EvolutionCaseResult {
                case_id: case.id.clone(),
                case_name: case.name.clone(),
                input: case.input.clone(),
                weight: case.weight,
                assertions: case.assertions.clone(),
                baseline_passed,
                candidate_passed,
                baseline_issues,
                candidate_issues,
                baseline_output_excerpt: truncate_chars(
                    baseline_output,
                    MAX_EVAL_OUTPUT_EXCERPT_CHARS,
                ),
                candidate_output_excerpt: truncate_chars(
                    candidate_output,
                    MAX_EVAL_OUTPUT_EXCERPT_CHARS,
                ),
                baseline_output_sha256: sha256_hex(baseline_output.as_bytes()),
                candidate_output_sha256: sha256_hex(candidate_output.as_bytes()),
            });
        }

        let baseline_score = baseline_passed_weight / total_weight;
        let candidate_score = candidate_passed_weight / total_weight;
        Ok(EvolutionScorecard {
            baseline_score,
            candidate_score,
            score_delta: candidate_score - baseline_score,
            regressions,
            baseline_passed_cases,
            candidate_passed_cases,
            total_cases: case_results.len(),
            case_results,
        })
    }
}

fn evaluate_assertions(assertions: &EvolutionCaseAssertions, output: &str) -> Vec<String> {
    let mut issues = Vec::new();
    for required in &assertions.required_substrings {
        if !output.contains(required) {
            issues.push(format!("missing required substring '{required}'"));
        }
    }
    for forbidden in &assertions.forbidden_substrings {
        if output.contains(forbidden) {
            issues.push(format!("contains forbidden substring '{forbidden}'"));
        }
    }
    if assertions.require_json && serde_json::from_str::<serde_json::Value>(output).is_err() {
        issues.push("output is not valid JSON".to_string());
    }
    issues
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvolutionGateConfig {
    pub min_eval_cases: usize,
    pub min_candidate_score: f64,
    pub min_score_delta: f64,
    pub max_regressions: usize,
    pub max_prompt_patch_chars: usize,
    pub require_human_approval: bool,
}

impl Default for EvolutionGateConfig {
    fn default() -> Self {
        Self {
            min_eval_cases: 3,
            min_candidate_score: 0.8,
            min_score_delta: 0.05,
            max_regressions: 0,
            max_prompt_patch_chars: 4_000,
            require_human_approval: true,
        }
    }
}

impl EvolutionGateConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.min_eval_cases == 0 {
            bail!("min_eval_cases must be greater than zero");
        }
        if self.min_eval_cases > MAX_ENABLED_EVOLUTION_EVAL_CASES {
            bail!("min_eval_cases cannot exceed {MAX_ENABLED_EVOLUTION_EVAL_CASES}");
        }
        validate_unit_interval("min_candidate_score", self.min_candidate_score)?;
        if !self.min_score_delta.is_finite() || !(-1.0..=1.0).contains(&self.min_score_delta) {
            bail!("min_score_delta must be finite and between -1 and 1");
        }
        if self.max_prompt_patch_chars == 0
            || self.max_prompt_patch_chars > ABSOLUTE_MAX_PROMPT_PATCH_CHARS
        {
            bail!("max_prompt_patch_chars must be between 1 and {ABSOLUTE_MAX_PROMPT_PATCH_CHARS}");
        }
        Ok(())
    }

    pub fn decide(&self, scorecard: &EvolutionScorecard) -> EvolutionPromotionDecision {
        let mut reasons = Vec::new();
        if scorecard.total_cases < self.min_eval_cases {
            reasons.push(format!(
                "minimum eval cases not met: {} < {}",
                scorecard.total_cases, self.min_eval_cases
            ));
        }
        if scorecard.candidate_score < self.min_candidate_score {
            reasons.push(format!(
                "candidate score {:.6} is below minimum {:.6}",
                scorecard.candidate_score, self.min_candidate_score
            ));
        }
        if scorecard.score_delta < self.min_score_delta {
            reasons.push(format!(
                "score delta {:.6} is below minimum {:.6}",
                scorecard.score_delta, self.min_score_delta
            ));
        }
        if scorecard.regressions > self.max_regressions {
            reasons.push(format!(
                "regressions {} exceed maximum {}",
                scorecard.regressions, self.max_regressions
            ));
        }

        if reasons.is_empty() {
            EvolutionPromotionDecision::Ready
        } else {
            EvolutionPromotionDecision::Rejected { reasons }
        }
    }
}

fn validate_unit_interval(name: &str, value: f64) -> anyhow::Result<()> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        bail!("{name} must be finite and between 0 and 1");
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum EvolutionPromotionDecision {
    Ready,
    Rejected { reasons: Vec<String> },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvolutionCandidateStatus {
    Draft,
    Evaluating,
    Ready,
    Rejected,
    Approved,
    Failed,
}

impl EvolutionCandidateStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Evaluating => "evaluating",
            Self::Ready => "ready",
            Self::Rejected => "rejected",
            Self::Approved => "approved",
            Self::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "draft" => Ok(Self::Draft),
            "evaluating" => Ok(Self::Evaluating),
            "ready" => Ok(Self::Ready),
            "rejected" => Ok(Self::Rejected),
            "approved" => Ok(Self::Approved),
            "failed" => Ok(Self::Failed),
            other => bail!("unsupported evolution candidate status '{other}'"),
        }
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (
                Self::Draft,
                Self::Evaluating | Self::Rejected | Self::Failed
            ) | (
                Self::Evaluating,
                Self::Ready | Self::Rejected | Self::Failed
            ) | (
                Self::Ready,
                Self::Approved | Self::Rejected | Self::Evaluating
            ) | (
                Self::Rejected | Self::Failed | Self::Approved,
                Self::Evaluating
            )
        )
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EvolutionCandidateDraft {
    pub id: String,
    pub parent_candidate_id: Option<String>,
    pub evidence_fingerprint: Option<String>,
    pub prompt_patch: PromptPatch,
    pub rationale: String,
    pub source_trajectory_ids: Vec<String>,
}

impl EvolutionCandidateDraft {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.id.trim().is_empty() {
            bail!("candidate id cannot be empty");
        }
        if self.rationale.trim().is_empty() {
            bail!("candidate rationale cannot be empty");
        }
        if self.rationale.trim().chars().count() > MAX_CANDIDATE_RATIONALE_CHARS {
            bail!("candidate rationale cannot exceed {MAX_CANDIDATE_RATIONALE_CHARS} characters");
        }
        if self
            .parent_candidate_id
            .as_ref()
            .is_some_and(|id| id.trim().is_empty())
        {
            bail!("parent candidate id cannot be empty");
        }
        if self.evidence_fingerprint.as_deref().is_some_and(|value| {
            value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            bail!("evidence fingerprint must be a 64-character SHA-256 hex digest");
        }
        if self
            .source_trajectory_ids
            .iter()
            .any(|id| id.trim().is_empty())
        {
            bail!("source trajectory ids cannot contain empty values");
        }
        if self.source_trajectory_ids.len() > 100 {
            bail!("candidate cannot reference more than 100 source trajectories");
        }
        if self.source_trajectory_ids.iter().any(|id| id.len() > 256) {
            bail!("source trajectory ids cannot exceed 256 bytes");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EvolutionCandidateRecord {
    pub id: String,
    pub parent_candidate_id: Option<String>,
    pub evidence_fingerprint: Option<String>,
    pub prompt_patch: PromptPatch,
    pub rationale: String,
    pub source_trajectory_ids: Vec<String>,
    pub status: EvolutionCandidateStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EvolutionEvaluationDraft {
    pub id: String,
    pub candidate_id: String,
    pub baseline_candidate_id: Option<String>,
    pub scorecard: EvolutionScorecard,
    pub decision: EvolutionPromotionDecision,
    pub gate_config: EvolutionGateConfig,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EvolutionEvaluationRecord {
    pub id: String,
    pub candidate_id: String,
    pub baseline_candidate_id: Option<String>,
    pub scorecard: EvolutionScorecard,
    pub decision: EvolutionPromotionDecision,
    pub gate_config: EvolutionGateConfig,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvolutionFeedbackDraft {
    pub trajectory_id: String,
    pub score: f64,
    #[serde(default)]
    pub tags: Vec<String>,
    pub comment: Option<String>,
}

impl EvolutionFeedbackDraft {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.trajectory_id.trim().is_empty() {
            bail!("feedback trajectory id cannot be empty");
        }
        if self.trajectory_id.len() > 256 {
            bail!("feedback trajectory id cannot exceed 256 bytes");
        }
        if !self.score.is_finite() || !(-1.0..=1.0).contains(&self.score) {
            bail!("feedback score must be finite and between -1 and 1");
        }
        if self.tags.len() > 16 {
            bail!("feedback cannot contain more than 16 tags");
        }
        for tag in &self.tags {
            let tag = tag.trim();
            if tag.is_empty() || tag.chars().count() > 64 {
                bail!("feedback tags must contain between 1 and 64 characters");
            }
        }
        if self
            .comment
            .as_deref()
            .is_some_and(|comment| comment.chars().count() > 2_000)
        {
            bail!("feedback comment cannot exceed 2000 characters");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EvolutionFeedbackRecord {
    pub id: i64,
    pub trajectory_id: String,
    pub score: f64,
    pub tags: Vec<String>,
    pub comment: Option<String>,
    pub actor: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EvolutionDeploymentRecord {
    pub id: String,
    pub candidate_id: String,
    pub previous_deployment_id: Option<String>,
    pub activated_by: String,
    pub reason: String,
    pub activated_at: DateTime<Utc>,
    pub rolled_back_at: Option<DateTime<Utc>>,
    pub rolled_back_by: Option<String>,
    pub rollback_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ActiveEvolutionPolicy {
    pub deployment_id: String,
    pub candidate_id: String,
    pub prompt_patch: PromptPatch,
}

pub fn render_evolution_policy_message(
    candidate_id: &str,
    deployment_id: &str,
    prompt_patch: &PromptPatch,
) -> String {
    format!(
        "ACTIVE_EVOLUTION_POLICY\ncandidate_id={candidate_id}\ndeployment_id={deployment_id}\nInstructions:\n{}",
        prompt_patch.as_str()
    )
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EvolutionRollbackResult {
    pub rolled_back_deployment_id: String,
    pub rolled_back_candidate_id: String,
    pub restored_policy: Option<ActiveEvolutionPolicy>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EvolutionAuditEvent {
    pub id: i64,
    pub candidate_id: Option<String>,
    pub deployment_id: Option<String>,
    pub event_type: String,
    pub actor: String,
    pub details: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum EvolutionActor {
    Human(String),
    System(String),
}

impl EvolutionActor {
    fn audit_label(&self) -> anyhow::Result<String> {
        let (prefix, id) = match self {
            Self::Human(id) => ("human", id),
            Self::System(id) => ("system", id),
        };
        let id = id.trim();
        if id.is_empty() {
            bail!("evolution actor id cannot be empty");
        }
        if id.len() > 128 || id.chars().any(char::is_control) {
            bail!("evolution actor id cannot exceed 128 bytes or contain control characters");
        }
        Ok(format!("{prefix}:{id}"))
    }

    fn is_human(&self) -> bool {
        matches!(self, Self::Human(_))
    }
}

#[derive(Clone, Default)]
pub struct EvolutionPolicyRuntime {
    active: Arc<RwLock<Option<ActiveEvolutionPolicy>>>,
}

impl EvolutionPolicyRuntime {
    pub fn new(active: Option<ActiveEvolutionPolicy>) -> Self {
        Self {
            active: Arc::new(RwLock::new(active)),
        }
    }

    pub async fn active(&self) -> Option<ActiveEvolutionPolicy> {
        self.active.read().await.clone()
    }

    async fn replace(&self, active: Option<ActiveEvolutionPolicy>) {
        *self.active.write().await = active;
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EvolutionCycleResult {
    pub candidate: EvolutionCandidateRecord,
    pub evaluation: EvolutionEvaluationRecord,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct EvolutionCycleStatus {
    pub running: bool,
    pub last_started_at: Option<DateTime<Utc>>,
    pub last_finished_at: Option<DateTime<Utc>>,
    pub last_candidate_id: Option<String>,
    pub last_outcome: Option<String>,
    pub last_skip_reason: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug)]
pub struct EvolutionCycleSkipped {
    reason: String,
}

impl EvolutionCycleSkipped {
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for EvolutionCycleSkipped {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl StdError for EvolutionCycleSkipped {}

pub fn evolution_cycle_skip_reason(error: &anyhow::Error) -> Option<&str> {
    error
        .downcast_ref::<EvolutionCycleSkipped>()
        .map(EvolutionCycleSkipped::reason)
}

pub struct EvolutionEngine {
    store: Arc<dyn EvolutionStore>,
    harness_store: Option<Arc<dyn HarnessStore>>,
    provider: Arc<dyn ChatProvider>,
    gate_config: EvolutionGateConfig,
    policy_runtime: EvolutionPolicyRuntime,
    max_source_trajectories: usize,
    max_evidence_chars: usize,
    cycle_lock: Arc<Mutex<()>>,
    evaluation_lock: Arc<Mutex<()>>,
    cycle_status: Arc<RwLock<EvolutionCycleStatus>>,
}

impl EvolutionEngine {
    pub async fn new(
        store: Arc<dyn EvolutionStore>,
        provider: Arc<dyn ChatProvider>,
        gate_config: EvolutionGateConfig,
    ) -> anyhow::Result<Self> {
        gate_config.validate()?;
        let active = store.active_policy().await?;
        Ok(Self {
            store,
            harness_store: None,
            provider,
            gate_config,
            policy_runtime: EvolutionPolicyRuntime::new(active),
            max_source_trajectories: 20,
            max_evidence_chars: 8_000,
            cycle_lock: Arc::new(Mutex::new(())),
            evaluation_lock: Arc::new(Mutex::new(())),
            cycle_status: Arc::new(RwLock::new(EvolutionCycleStatus::default())),
        })
    }

    pub fn with_harness_store(mut self, store: Arc<dyn HarnessStore>) -> Self {
        self.harness_store = Some(store);
        self
    }

    pub fn with_evidence_limits(
        mut self,
        max_source_trajectories: usize,
        max_evidence_chars: usize,
    ) -> Self {
        self.max_source_trajectories = max_source_trajectories.clamp(1, 100);
        self.max_evidence_chars = max_evidence_chars.clamp(512, 32_000);
        self
    }

    pub fn policy_runtime(&self) -> EvolutionPolicyRuntime {
        self.policy_runtime.clone()
    }

    pub async fn cycle_status(&self) -> EvolutionCycleStatus {
        self.cycle_status.read().await.clone()
    }

    pub fn gate_config(&self) -> &EvolutionGateConfig {
        &self.gate_config
    }

    pub async fn active_policy(&self) -> Option<ActiveEvolutionPolicy> {
        self.policy_runtime.active().await
    }

    pub async fn get_candidate(
        &self,
        candidate_id: &str,
    ) -> anyhow::Result<Option<EvolutionCandidateRecord>> {
        self.store.get_candidate(candidate_id).await
    }

    pub async fn list_candidates(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<EvolutionCandidateRecord>> {
        self.store.list_candidates(limit).await
    }

    pub async fn upsert_eval_case(
        &self,
        eval_case: EvolutionEvalCase,
        actor: &EvolutionActor,
    ) -> anyhow::Result<()> {
        self.store
            .upsert_eval_case(eval_case, &actor.audit_label()?)
            .await
    }

    pub async fn list_eval_cases(
        &self,
        enabled_only: bool,
    ) -> anyhow::Result<Vec<EvolutionEvalCase>> {
        self.store.list_eval_cases(enabled_only).await
    }

    pub async fn latest_evaluation(
        &self,
        candidate_id: &str,
    ) -> anyhow::Result<Option<EvolutionEvaluationRecord>> {
        self.store.latest_evaluation(candidate_id).await
    }

    pub async fn list_audit_events(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<EvolutionAuditEvent>> {
        self.store.list_audit_events(limit).await
    }

    pub async fn record_feedback(
        &self,
        draft: EvolutionFeedbackDraft,
        actor: &EvolutionActor,
    ) -> anyhow::Result<EvolutionFeedbackRecord> {
        self.store
            .record_feedback(draft, &actor.audit_label()?)
            .await
    }

    pub async fn list_feedback(
        &self,
        negative_only: bool,
        limit: usize,
    ) -> anyhow::Result<Vec<EvolutionFeedbackRecord>> {
        self.store.list_feedback(negative_only, limit).await
    }

    pub async fn create_candidate(
        &self,
        prompt_patch: impl Into<String>,
        rationale: impl Into<String>,
        source_trajectory_ids: Vec<String>,
        actor: &EvolutionActor,
    ) -> anyhow::Result<EvolutionCandidateRecord> {
        self.create_candidate_with_fingerprint(
            prompt_patch,
            rationale,
            source_trajectory_ids,
            None,
            actor,
        )
        .await
    }

    async fn create_candidate_with_fingerprint(
        &self,
        prompt_patch: impl Into<String>,
        rationale: impl Into<String>,
        source_trajectory_ids: Vec<String>,
        evidence_fingerprint: Option<String>,
        actor: &EvolutionActor,
    ) -> anyhow::Result<EvolutionCandidateRecord> {
        let actor = actor.audit_label()?;
        let active = self.policy_runtime.active().await;
        let mut source_trajectory_ids = source_trajectory_ids
            .into_iter()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect::<Vec<_>>();
        source_trajectory_ids.sort();
        source_trajectory_ids.dedup();
        if source_trajectory_ids.len() > self.max_source_trajectories {
            bail!(
                "candidate source trajectories exceed the configured limit of {}",
                self.max_source_trajectories
            );
        }
        self.store
            .create_candidate(
                EvolutionCandidateDraft {
                    id: new_evolution_id("candidate"),
                    parent_candidate_id: active.map(|policy| policy.candidate_id),
                    evidence_fingerprint,
                    prompt_patch: PromptPatch::new(
                        prompt_patch,
                        self.gate_config.max_prompt_patch_chars,
                    )?,
                    rationale: rationale.into(),
                    source_trajectory_ids,
                },
                &actor,
            )
            .await
    }

    pub async fn evaluate_candidate(
        &self,
        candidate_id: &str,
    ) -> anyhow::Result<EvolutionEvaluationRecord> {
        self.evaluate_candidate_with_limit(candidate_id, usize::MAX, None)
            .await
            .map(|(evaluation, _)| evaluation)
    }

    pub async fn evaluate_candidate_bounded(
        &self,
        candidate_id: &str,
        max_response_bytes: usize,
    ) -> anyhow::Result<(EvolutionEvaluationRecord, usize)> {
        ensure!(
            max_response_bytes > 0,
            "evolution response byte budget must be positive"
        );
        self.evaluate_candidate_with_limit(candidate_id, max_response_bytes, None)
            .await
    }

    pub async fn evaluate_candidate_bounded_until(
        &self,
        candidate_id: &str,
        max_response_bytes: usize,
        deadline: tokio::time::Instant,
    ) -> anyhow::Result<(EvolutionEvaluationRecord, usize)> {
        ensure!(
            max_response_bytes > 0,
            "evolution response byte budget must be positive"
        );
        self.evaluate_candidate_with_limit(candidate_id, max_response_bytes, Some(deadline))
            .await
    }

    async fn evaluate_candidate_with_limit(
        &self,
        candidate_id: &str,
        max_response_bytes: usize,
        deadline: Option<tokio::time::Instant>,
    ) -> anyhow::Result<(EvolutionEvaluationRecord, usize)> {
        let _evaluation_guard = self.evaluation_lock.lock().await;
        let candidate = self
            .store
            .get_candidate(candidate_id)
            .await?
            .with_context(|| format!("evolution candidate '{candidate_id}' does not exist"))?;
        let expected_status = match candidate.status {
            EvolutionCandidateStatus::Draft
            | EvolutionCandidateStatus::Ready
            | EvolutionCandidateStatus::Rejected
            | EvolutionCandidateStatus::Failed
            | EvolutionCandidateStatus::Approved => candidate.status,
            status => bail!(
                "candidate '{candidate_id}' cannot be evaluated from status '{}'",
                status.as_str()
            ),
        };
        self.store
            .transition_candidate(
                candidate_id,
                expected_status,
                EvolutionCandidateStatus::Evaluating,
                "system:evolution-engine",
                serde_json::json!({"model": self.provider.model_name()}),
            )
            .await?;

        let result = self
            .evaluate_candidate_inner(&candidate, max_response_bytes, deadline)
            .await;
        match result {
            Ok(evaluation) => Ok(evaluation),
            Err(err) => {
                let _ = self
                    .store
                    .transition_candidate(
                        candidate_id,
                        EvolutionCandidateStatus::Evaluating,
                        EvolutionCandidateStatus::Failed,
                        "system:evolution-engine",
                        serde_json::json!({
                            "error": "shadow_evaluation_failed",
                            "retryable": true,
                        }),
                    )
                    .await;
                Err(err)
            }
        }
    }

    async fn evaluate_candidate_inner(
        &self,
        candidate: &EvolutionCandidateRecord,
        max_response_bytes: usize,
        deadline: Option<tokio::time::Instant>,
    ) -> anyhow::Result<(EvolutionEvaluationRecord, usize)> {
        let cases = self.store.list_eval_cases(true).await?;
        let active = self.policy_runtime.active().await;
        let mut baseline_outputs = BTreeMap::new();
        let mut candidate_outputs = BTreeMap::new();
        let mut response_bytes = 0_usize;

        for case in &cases {
            let baseline_policy = active.as_ref().map(|policy| {
                (
                    policy.candidate_id.as_str(),
                    policy.deployment_id.as_str(),
                    &policy.prompt_patch,
                )
            });
            let baseline = self
                .shadow_complete_with_deadline(case, baseline_policy, deadline)
                .await
                .with_context(|| format!("baseline eval case '{}' failed", case.id))?;
            response_bytes = response_bytes
                .checked_add(baseline.len())
                .context("evolution response byte count overflowed")?;
            ensure!(
                response_bytes <= max_response_bytes,
                "evolution response byte budget exceeded"
            );
            let candidate_output = self
                .shadow_complete_with_deadline(
                    case,
                    Some((
                        candidate.id.as_str(),
                        "shadow-evaluation",
                        &candidate.prompt_patch,
                    )),
                    deadline,
                )
                .await
                .with_context(|| format!("candidate eval case '{}' failed", case.id))?;
            response_bytes = response_bytes
                .checked_add(candidate_output.len())
                .context("evolution response byte count overflowed")?;
            ensure!(
                response_bytes <= max_response_bytes,
                "evolution response byte budget exceeded"
            );
            baseline_outputs.insert(case.id.clone(), baseline);
            candidate_outputs.insert(case.id.clone(), candidate_output);
        }

        let scorecard = EvolutionScorer::score(&cases, &baseline_outputs, &candidate_outputs)?;
        let decision = self.gate_config.decide(&scorecard);
        let evaluation = self
            .store
            .record_evaluation(EvolutionEvaluationDraft {
                id: new_evolution_id("evaluation"),
                candidate_id: candidate.id.clone(),
                baseline_candidate_id: active.map(|policy| policy.candidate_id),
                scorecard,
                decision: decision.clone(),
                gate_config: self.gate_config.clone(),
            })
            .await?;

        let (next, details) = match &decision {
            EvolutionPromotionDecision::Ready => (
                EvolutionCandidateStatus::Ready,
                serde_json::json!({
                    "evaluation_id": evaluation.id,
                    "candidate_score": evaluation.scorecard.candidate_score,
                    "score_delta": evaluation.scorecard.score_delta,
                }),
            ),
            EvolutionPromotionDecision::Rejected { reasons } => (
                EvolutionCandidateStatus::Rejected,
                serde_json::json!({
                    "evaluation_id": evaluation.id,
                    "reasons": reasons,
                }),
            ),
        };
        self.store
            .transition_candidate(
                &candidate.id,
                EvolutionCandidateStatus::Evaluating,
                next,
                "system:evolution-engine",
                details,
            )
            .await?;
        Ok((evaluation, response_bytes))
    }

    async fn shadow_complete_with_deadline(
        &self,
        eval_case: &EvolutionEvalCase,
        policy: Option<(&str, &str, &PromptPatch)>,
        deadline: Option<tokio::time::Instant>,
    ) -> anyhow::Result<String> {
        let completion = self.shadow_complete(eval_case, policy);
        if let Some(deadline) = deadline {
            tokio::time::timeout_at(deadline, completion)
                .await
                .context("evolution evaluation deadline exceeded")?
        } else {
            completion.await
        }
    }

    async fn shadow_complete(
        &self,
        eval_case: &EvolutionEvalCase,
        policy: Option<(&str, &str, &PromptPatch)>,
    ) -> anyhow::Result<String> {
        let mut messages = Vec::with_capacity(2);
        if let Some((candidate_id, deployment_id, prompt_patch)) = policy {
            messages.push(StoredMessage {
                role: MessageRole::System,
                content: render_evolution_policy_message(candidate_id, deployment_id, prompt_patch),
            });
        }
        messages.push(StoredMessage {
            role: MessageRole::User,
            content: eval_case.input.clone(),
        });
        self.provider
            .complete(CompletionRequest::from_messages(messages))
            .await
    }

    pub async fn approve_candidate(
        &self,
        candidate_id: &str,
        actor: &EvolutionActor,
        reason: &str,
    ) -> anyhow::Result<EvolutionCandidateRecord> {
        self.require_human_actor(actor)?;
        validate_operator_reason("approval", reason)?;
        let latest = self
            .store
            .latest_evaluation(candidate_id)
            .await?
            .with_context(|| format!("candidate '{candidate_id}' has no evaluation"))?;
        if !matches!(latest.decision, EvolutionPromotionDecision::Ready) {
            bail!("candidate '{candidate_id}' latest evaluation is not ready");
        }
        let active_candidate_id = self
            .policy_runtime
            .active()
            .await
            .map(|policy| policy.candidate_id);
        if latest.baseline_candidate_id != active_candidate_id {
            bail!(
                "candidate '{candidate_id}' was evaluated against a stale baseline; re-evaluate it before approval"
            );
        }
        self.store
            .transition_candidate(
                candidate_id,
                EvolutionCandidateStatus::Ready,
                EvolutionCandidateStatus::Approved,
                &actor.audit_label()?,
                serde_json::json!({
                    "reason": reason.trim(),
                    "evaluation_id": latest.id,
                }),
            )
            .await
    }

    pub async fn abandon_evaluation(
        &self,
        candidate_id: &str,
        actor: &EvolutionActor,
        reason: &str,
    ) -> anyhow::Result<EvolutionCandidateRecord> {
        self.require_human_actor(actor)?;
        validate_operator_reason("abandon", reason)?;
        self.store
            .transition_candidate(
                candidate_id,
                EvolutionCandidateStatus::Evaluating,
                EvolutionCandidateStatus::Failed,
                &actor.audit_label()?,
                serde_json::json!({
                    "reason": reason.trim(),
                    "recovery": "operator_abandoned_stale_evaluation",
                }),
            )
            .await
    }

    pub async fn activate_candidate(
        &self,
        candidate_id: &str,
        actor: &EvolutionActor,
        reason: &str,
    ) -> anyhow::Result<EvolutionDeploymentRecord> {
        self.require_human_actor(actor)?;
        let candidate = self
            .store
            .get_candidate(candidate_id)
            .await?
            .with_context(|| format!("evolution candidate '{candidate_id}' does not exist"))?;
        let deployment = self
            .store
            .activate_candidate(candidate_id, &actor.audit_label()?, reason)
            .await?;
        self.policy_runtime
            .replace(Some(ActiveEvolutionPolicy {
                deployment_id: deployment.id.clone(),
                candidate_id: candidate.id,
                prompt_patch: candidate.prompt_patch,
            }))
            .await;
        Ok(deployment)
    }

    pub async fn rollback_active(
        &self,
        actor: &EvolutionActor,
        reason: &str,
    ) -> anyhow::Result<EvolutionRollbackResult> {
        self.require_human_actor(actor)?;
        let rollback = self
            .store
            .rollback_active(&actor.audit_label()?, reason)
            .await?;
        self.policy_runtime
            .replace(rollback.restored_policy.clone())
            .await;
        Ok(rollback)
    }

    fn require_human_actor(&self, actor: &EvolutionActor) -> anyhow::Result<()> {
        if self.gate_config.require_human_approval && !actor.is_human() {
            bail!("a human actor is required by the evolution promotion policy");
        }
        Ok(())
    }

    pub async fn propose_from_trajectories(&self) -> anyhow::Result<EvolutionCandidateRecord> {
        let harness_store = self
            .harness_store
            .as_ref()
            .context("trajectory evidence source is not configured")?;
        let feedback = self
            .store
            .list_feedback(true, self.max_source_trajectories.saturating_mul(5))
            .await?;
        let mut feedback_by_trajectory = BTreeMap::<String, Vec<EvolutionFeedbackRecord>>::new();
        for record in feedback {
            feedback_by_trajectory
                .entry(record.trajectory_id.clone())
                .or_default()
                .push(record);
        }

        let mut failures = Vec::new();
        let mut seen_trajectory_ids = HashSet::new();
        for trajectory_id in feedback_by_trajectory.keys() {
            if failures.len() >= self.max_source_trajectories {
                break;
            }
            if let Some(trajectory) = harness_store.get_trajectory(trajectory_id).await? {
                seen_trajectory_ids.insert(trajectory.id.clone());
                failures.push(trajectory);
            }
        }

        let trajectories = harness_store
            .query_trajectories(TrajectoryFilter {
                session_id: None,
                channel: None,
                user_id: None,
                exit_reason: None,
                has_tool_errors: None,
                limit: self.max_source_trajectories.saturating_mul(5),
            })
            .await?;
        for trajectory in trajectories {
            if failures.len() >= self.max_source_trajectories {
                break;
            }
            if trajectory_has_failure_signal(&trajectory)
                && seen_trajectory_ids.insert(trajectory.id.clone())
            {
                failures.push(trajectory);
            }
        }
        if failures.is_empty() {
            return Err(EvolutionCycleSkipped::new(
                "no failed trajectories are available for an evolution proposal",
            )
            .into());
        }

        failures.sort_by(|left, right| left.id.cmp(&right.id));
        let mut source_trajectory_ids = failures
            .iter()
            .map(|trajectory| trajectory.id.clone())
            .collect::<Vec<_>>();
        source_trajectory_ids.sort();
        let active_parent = self
            .policy_runtime
            .active()
            .await
            .map(|policy| policy.candidate_id);
        let evidence =
            build_failure_evidence(&failures, &feedback_by_trajectory, self.max_evidence_chars);
        let fingerprint = evidence_fingerprint(&evidence);
        if let Some(existing) = self.store.find_candidate_by_evidence(&fingerprint).await? {
            if existing.status == EvolutionCandidateStatus::Draft {
                self.store
                    .record_audit_event(
                        Some(&existing.id),
                        None,
                        "proposal_resumed",
                        "system:evolution-engine",
                        serde_json::json!({"evidence_fingerprint": fingerprint}),
                    )
                    .await?;
                return Ok(existing);
            }
            return Err(EvolutionCycleSkipped::new(
                "no new failure evidence is available since the last evolution proposal",
            )
            .into());
        }

        let audit_actor = "system:evolution-engine";
        self.store
            .record_audit_event(
                None,
                None,
                "proposal_started",
                audit_actor,
                serde_json::json!({
                    "parent_candidate_id": active_parent,
                    "evidence_fingerprint": fingerprint,
                    "source_trajectory_ids": source_trajectory_ids,
                }),
            )
            .await?;

        let reply = match self
            .provider
            .complete(CompletionRequest::json(vec![
                StoredMessage {
                    role: MessageRole::System,
                    content: [
                        "SELF_EVOLUTION_PROPOSAL_JSON",
                        "Propose one bounded replacement system-prompt patch that addresses the observed failures.",
                        "Return only JSON with schema: {\"prompt_patch\":\"string\",\"rationale\":\"string\"}.",
                        "Do not include MCP/code-mode control markers, secrets, tool permissions, or instructions to edit files.",
                    ]
                    .join("\n"),
                },
                StoredMessage {
                    role: MessageRole::User,
                    content: evidence,
                },
            ]))
            .await
        {
            Ok(reply) => reply,
            Err(err) => {
                self.record_proposal_failure(&fingerprint, "provider", audit_actor)
                    .await?;
                return Err(err).context("evolution proposal completion failed");
            }
        };
        if reply.chars().count() > MAX_PROPOSAL_RESPONSE_CHARS {
            self.record_proposal_failure(&fingerprint, "oversized_response", audit_actor)
                .await?;
            bail!("evolution proposal response exceeds {MAX_PROPOSAL_RESPONSE_CHARS} characters");
        }
        let proposal: EvolutionProposal = match serde_json::from_str(&reply) {
            Ok(proposal) => proposal,
            Err(err) => {
                self.record_proposal_failure(&fingerprint, "invalid_response", audit_actor)
                    .await?;
                return Err(err).context("evolution proposal was not valid JSON");
            }
        };
        match self
            .create_candidate_with_fingerprint(
                proposal.prompt_patch,
                proposal.rationale,
                source_trajectory_ids,
                Some(fingerprint.clone()),
                &EvolutionActor::System("evolution-engine".to_string()),
            )
            .await
        {
            Ok(candidate) => Ok(candidate),
            Err(err) => {
                if let Some(existing) = self.store.find_candidate_by_evidence(&fingerprint).await? {
                    self.store
                        .record_audit_event(
                            Some(&existing.id),
                            None,
                            "proposal_deduplicated",
                            audit_actor,
                            serde_json::json!({"evidence_fingerprint": fingerprint}),
                        )
                        .await?;
                    return Err(EvolutionCycleSkipped::new(format!(
                        "the failure evidence was claimed by candidate '{}'",
                        existing.id
                    ))
                    .into());
                }
                self.record_proposal_failure(
                    &fingerprint,
                    "candidate_validation_or_persistence",
                    audit_actor,
                )
                .await?;
                Err(err)
            }
        }
    }

    async fn record_proposal_failure(
        &self,
        fingerprint: &str,
        stage: &str,
        actor: &str,
    ) -> anyhow::Result<()> {
        self.store
            .record_audit_event(
                None,
                None,
                "proposal_failed",
                actor,
                serde_json::json!({
                    "evidence_fingerprint": fingerprint,
                    "stage": stage,
                }),
            )
            .await
    }

    pub async fn run_cycle(&self) -> anyhow::Result<EvolutionCycleResult> {
        let _cycle_guard = self.cycle_lock.lock().await;
        {
            let mut status = self.cycle_status.write().await;
            status.running = true;
            status.last_started_at = Some(Utc::now());
            status.last_finished_at = None;
            status.last_candidate_id = None;
            status.last_outcome = None;
            status.last_skip_reason = None;
            status.last_error = None;
        }
        let result = self.run_cycle_inner().await;
        {
            let mut status = self.cycle_status.write().await;
            status.running = false;
            status.last_finished_at = Some(Utc::now());
            match &result {
                Ok(cycle) => {
                    status.last_candidate_id = Some(cycle.candidate.id.clone());
                    status.last_outcome = Some(cycle.candidate.status.as_str().to_string());
                    status.last_skip_reason = None;
                    status.last_error = None;
                }
                Err(err) => {
                    if let Some(reason) = evolution_cycle_skip_reason(err) {
                        status.last_outcome = Some("skipped".to_string());
                        status.last_skip_reason = Some(truncate_chars(reason, 1_000));
                        status.last_error = None;
                    } else {
                        status.last_outcome = Some("failed".to_string());
                        status.last_skip_reason = None;
                        status.last_error = Some(truncate_chars(&err.to_string(), 1_000));
                    }
                }
            }
        }
        result
    }

    async fn run_cycle_inner(&self) -> anyhow::Result<EvolutionCycleResult> {
        let enabled_eval_cases = self.store.list_eval_cases(true).await?.len();
        if enabled_eval_cases > MAX_ENABLED_EVOLUTION_EVAL_CASES {
            bail!("enabled eval cases exceed the maximum of {MAX_ENABLED_EVOLUTION_EVAL_CASES}");
        }
        if enabled_eval_cases < self.gate_config.min_eval_cases {
            return Err(EvolutionCycleSkipped::new(format!(
                "evolution eval suite is not ready: {enabled_eval_cases} enabled cases, {} required",
                self.gate_config.min_eval_cases
            ))
            .into());
        }
        let candidate = self.propose_from_trajectories().await?;
        let evaluation = self.evaluate_candidate(&candidate.id).await?;
        let candidate = self
            .store
            .get_candidate(&candidate.id)
            .await?
            .context("evolution cycle candidate disappeared")?;
        Ok(EvolutionCycleResult {
            candidate,
            evaluation,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvolutionProposal {
    prompt_patch: String,
    rationale: String,
}

fn evidence_fingerprint(evidence: &str) -> String {
    sha256_hex(evidence.as_bytes())
}

fn sha256_hex(value: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value);
    format!("{:x}", hasher.finalize())
}

fn validate_operator_reason(action: &str, reason: &str) -> anyhow::Result<()> {
    let reason = reason.trim();
    if reason.is_empty() {
        bail!("{action} reason cannot be empty");
    }
    if reason.chars().count() > 2_000 {
        bail!("{action} reason cannot exceed 2000 characters");
    }
    Ok(())
}

fn trajectory_has_failure_signal(trajectory: &TrajectoryRecord) -> bool {
    trajectory.exit_reason != TrajectoryExitReason::FinalAnswer
        || trajectory.tool_calls.iter().any(|call| {
            !call.ok
                || call
                    .result
                    .get("verification_failed")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
        })
        || trajectory
            .final_answer
            .as_ref()
            .is_none_or(|answer| answer.trim().is_empty())
}

fn build_failure_evidence(
    trajectories: &[TrajectoryRecord],
    feedback_by_trajectory: &BTreeMap<String, Vec<EvolutionFeedbackRecord>>,
    max_chars: usize,
) -> String {
    let rows = trajectories
        .iter()
        .map(|trajectory| {
            let failed_tools = trajectory
                .tool_calls
                .iter()
                .filter(|call| !call.ok)
                .map(|call| {
                    serde_json::json!({
                        "server": call.server,
                        "tool": call.tool,
                        "arguments": truncate_chars(&call.arguments.to_string(), 500),
                        "result": truncate_chars(&call.result.to_string(), 500),
                    })
                })
                .collect::<Vec<_>>();
            serde_json::json!({
                "trajectory_id": trajectory.id,
                "channel": trajectory.channel,
                "model": trajectory.model,
                "exit_reason": trajectory.exit_reason.as_str(),
                "failed_tools": failed_tools,
                "feedback": feedback_by_trajectory
                    .get(&trajectory.id)
                    .map(|records| records.iter().map(|record| serde_json::json!({
                        "score": record.score,
                        "tags": record.tags,
                        "comment": record.comment.as_deref().map(|value| truncate_chars(value, 500)),
                    })).collect::<Vec<_>>())
                    .unwrap_or_default(),
                "final_answer": trajectory
                    .final_answer
                    .as_deref()
                    .map(|answer| truncate_chars(answer, 500)),
            })
        })
        .collect::<Vec<_>>();
    truncate_chars(
        &serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_string()),
        max_chars,
    )
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let mut out = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}
