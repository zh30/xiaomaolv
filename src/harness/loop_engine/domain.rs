use std::collections::{BTreeMap, BTreeSet, VecDeque};

use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Proposed,
    Planning,
    ReviewReady,
    Approved,
    Active,
    Verifying,
    Achieved,
    Rejected,
    Canceled,
    Paused,
    Blocked,
    Failed,
    Replan,
}

impl GoalStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Planning => "planning",
            Self::ReviewReady => "review_ready",
            Self::Approved => "approved",
            Self::Active => "active",
            Self::Verifying => "verifying",
            Self::Achieved => "achieved",
            Self::Rejected => "rejected",
            Self::Canceled => "canceled",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::Replan => "replan",
        }
    }

    pub(crate) fn from_db(value: &str) -> anyhow::Result<Self> {
        match value {
            "proposed" => Ok(Self::Proposed),
            "planning" => Ok(Self::Planning),
            "review_ready" => Ok(Self::ReviewReady),
            "approved" => Ok(Self::Approved),
            "active" => Ok(Self::Active),
            "verifying" => Ok(Self::Verifying),
            "achieved" => Ok(Self::Achieved),
            "rejected" => Ok(Self::Rejected),
            "canceled" => Ok(Self::Canceled),
            "paused" => Ok(Self::Paused),
            "blocked" => Ok(Self::Blocked),
            "failed" => Ok(Self::Failed),
            "replan" => Ok(Self::Replan),
            other => bail!("unknown goal status: {other}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemStatus {
    Pending,
    Ready,
    Running,
    WaitingConfirmation,
    Succeeded,
    Failed,
    Canceled,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    Running,
    Committing,
    Succeeded,
    Failed,
    Abandoned,
    WaitingConfirmation,
}

impl AttemptStatus {
    pub(crate) fn from_db(value: &str) -> anyhow::Result<Self> {
        match value {
            "running" => Ok(Self::Running),
            "committing" => Ok(Self::Committing),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "abandoned" => Ok(Self::Abandoned),
            "waiting_confirmation" => Ok(Self::WaitingConfirmation),
            other => bail!("unknown attempt status: {other}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointPhase {
    Prepared,
    Committed,
    Reconciled,
}

impl CheckpointPhase {
    pub(crate) fn from_db(value: &str) -> anyhow::Result<Self> {
        match value {
            "prepared" => Ok(Self::Prepared),
            "committed" => Ok(Self::Committed),
            "reconciled" => Ok(Self::Reconciled),
            other => bail!("unknown checkpoint phase: {other}"),
        }
    }
}

impl WorkItemStatus {
    pub(crate) fn from_db(value: &str) -> anyhow::Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "ready" => Ok(Self::Ready),
            "running" => Ok(Self::Running),
            "waiting_confirmation" => Ok(Self::WaitingConfirmation),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "canceled" => Ok(Self::Canceled),
            "blocked" => Ok(Self::Blocked),
            other => bail!("unknown work item status: {other}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    Pure,
    Read,
    LocalWrite,
    ExternalWrite,
}

impl EffectClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Pure => "pure",
            Self::Read => "read",
            Self::LocalWrite => "local_write",
            Self::ExternalWrite => "external_write",
        }
    }

    pub(crate) fn from_db(value: &str) -> anyhow::Result<Self> {
        match value {
            "pure" => Ok(Self::Pure),
            "read" => Ok(Self::Read),
            "local_write" => Ok(Self::LocalWrite),
            "external_write" => Ok(Self::ExternalWrite),
            other => bail!("unknown effect class: {other}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub max_attempts: u8,
    pub backoff_secs: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionBudget {
    pub max_provider_calls: u32,
    pub deadline_secs: u32,
    pub max_response_bytes: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowStep {
    pub id: String,
    pub handler: String,
    pub effect: EffectClass,
    #[serde(default)]
    pub input: serde_json::Value,
    pub retry: RetryPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowEdge {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowSpec {
    pub steps: Vec<WorkflowStep>,
    #[serde(default)]
    pub edges: Vec<WorkflowEdge>,
    pub budget: ExecutionBudget,
}

impl WorkflowSpec {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            !self.steps.is_empty() && self.steps.len() <= 32,
            "workflow must contain 1..=32 steps"
        );
        ensure!(self.edges.len() <= 64, "workflow cannot exceed 64 edges");
        ensure!(
            self.budget.max_provider_calls <= 64,
            "provider call budget cannot exceed 64"
        );
        ensure!(
            (1..=86_400).contains(&self.budget.deadline_secs),
            "workflow deadline must be 1..=86400 seconds"
        );
        ensure!(
            (1..=10_485_760).contains(&self.budget.max_response_bytes),
            "response byte budget must be 1..=10485760"
        );

        let allowed_handlers = BTreeSet::from([
            "goal_planner",
            "provider_analysis",
            "session_replay",
            "self_test_suite",
            "evolution_evaluate",
            "manual_gate",
        ]);
        let mut step_ids = BTreeSet::new();
        for step in &self.steps {
            ensure!(
                !step.id.trim().is_empty() && step.id.len() <= 96,
                "workflow step id must be 1..=96 bytes"
            );
            ensure!(
                step_ids.insert(step.id.as_str()),
                "duplicate workflow step id"
            );
            ensure!(
                allowed_handlers.contains(step.handler.as_str()),
                "unregistered workflow handler: {}",
                step.handler
            );
            ensure!(
                step.effect != EffectClass::ExternalWrite,
                "external_write handlers are not enabled in this release"
            );
            ensure!(
                (1..=10).contains(&step.retry.max_attempts),
                "retry max_attempts must be 1..=10"
            );
            ensure!(
                step.retry.backoff_secs <= 3600,
                "retry backoff cannot exceed 3600 seconds"
            );
            ensure!(
                serde_json::to_vec(&step.input)?.len() <= 8192,
                "workflow step input cannot exceed 8192 bytes"
            );
        }

        let mut indegree = BTreeMap::from_iter(step_ids.iter().map(|id| (*id, 0_usize)));
        let mut outgoing: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        let mut unique_edges = BTreeSet::new();
        for edge in &self.edges {
            ensure!(
                step_ids.contains(edge.from.as_str()),
                "edge source does not exist"
            );
            ensure!(
                step_ids.contains(edge.to.as_str()),
                "edge target does not exist"
            );
            ensure!(
                edge.from != edge.to,
                "workflow step cannot depend on itself"
            );
            ensure!(
                unique_edges.insert((edge.from.as_str(), edge.to.as_str())),
                "duplicate workflow edge"
            );
            *indegree
                .get_mut(edge.to.as_str())
                .expect("validated target") += 1;
            outgoing
                .entry(edge.from.as_str())
                .or_default()
                .push(edge.to.as_str());
        }
        let mut queue = VecDeque::from_iter(
            indegree
                .iter()
                .filter_map(|(id, degree)| (*degree == 0).then_some(*id)),
        );
        let mut visited = 0;
        while let Some(id) = queue.pop_front() {
            visited += 1;
            for target in outgoing.get(id).into_iter().flatten() {
                let degree = indegree.get_mut(target).expect("validated target");
                *degree -= 1;
                if *degree == 0 {
                    queue.push_back(target);
                }
            }
        }
        ensure!(
            visited == self.steps.len(),
            "workflow graph must be acyclic"
        );
        ensure!(
            serde_json::to_vec(self)?.len() <= 32_768,
            "workflow cannot exceed 32768 serialized bytes"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AcceptanceCriterion {
    SelfTestSuite { suite: String },
    ArtifactExists { artifact_type: String },
    ManualApproval { label: String },
}

impl AcceptanceCriterion {
    fn validate(&self) -> anyhow::Result<()> {
        let value = match self {
            Self::SelfTestSuite { suite } => suite,
            Self::ArtifactExists { artifact_type } => artifact_type,
            Self::ManualApproval { label } => label,
        };
        ensure!(
            !value.trim().is_empty() && value.len() <= 160,
            "acceptance criterion value must be 1..=160 bytes"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanGoalRequest {
    pub workflow: WorkflowSpec,
    pub acceptance_criteria: Vec<AcceptanceCriterion>,
}

impl PlanGoalRequest {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        self.workflow.validate()?;
        ensure!(
            !self.acceptance_criteria.is_empty() && self.acceptance_criteria.len() <= 16,
            "plan must contain 1..=16 acceptance criteria"
        );
        for criterion in &self.acceptance_criteria {
            criterion.validate()?;
        }
        Ok(())
    }

    pub(crate) fn plan_hash(&self) -> anyhow::Result<String> {
        hash_serializable(self)
    }

    pub(crate) fn effect_manifest(&self) -> Vec<EffectClass> {
        self.workflow
            .steps
            .iter()
            .map(|step| step.effect)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApproveGoalRequest {
    pub expected_goal_revision: u32,
    pub expected_plan_hash: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannedGoal {
    pub goal: GoalRecord,
    pub plan_hash: String,
    pub workflow: WorkflowSpec,
    pub acceptance_criteria: Vec<AcceptanceCriterion>,
    pub effect_manifest: Vec<EffectClass>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateGoalRequest {
    pub objective: String,
    #[serde(default)]
    pub source_signal_ids: Vec<String>,
}

impl CreateGoalRequest {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        let objective = self.objective.trim();
        ensure!(!objective.is_empty(), "goal objective must not be empty");
        ensure!(
            objective.chars().count() <= 4096,
            "goal objective must not exceed 4096 characters"
        );
        ensure!(
            self.source_signal_ids.len() <= 64,
            "goal cannot reference more than 64 source signals"
        );
        for signal_id in &self.source_signal_ids {
            ensure!(
                !signal_id.trim().is_empty() && signal_id.len() <= 160,
                "source signal id must be 1..=160 bytes"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalRecord {
    pub id: String,
    pub objective: String,
    pub status: GoalStatus,
    pub revision: u32,
    pub source_signal_ids: Vec<String>,
    pub created_by: String,
    pub created_at_unix: i64,
    pub updated_at_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkItemRecord {
    pub id: String,
    pub goal_id: String,
    pub status: WorkItemStatus,
    pub step_id: String,
    pub handler: String,
    pub effect: EffectClass,
    pub input: serde_json::Value,
    pub max_attempts: u8,
    pub ordinal: u32,
    pub dependency_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub id: String,
    pub goal_id: String,
    pub work_item_id: String,
    pub attempt_number: u8,
    pub status: AttemptStatus,
    pub worker_id: String,
    pub lease_token: String,
    pub fencing_token: u64,
    pub started_at_unix: i64,
    pub finished_at_unix: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkClaim {
    pub work_item: WorkItemRecord,
    pub attempt: AttemptRecord,
    pub lease_until_unix: i64,
    pub budget: ExecutionBudget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkOutcome {
    pub summary: String,
    #[serde(default)]
    pub artifact_ids: Vec<String>,
    #[serde(default)]
    pub evidence: serde_json::Value,
}

impl WorkOutcome {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.summary.chars().count() <= 8192,
            "work outcome summary cannot exceed 8192 characters"
        );
        ensure!(
            self.artifact_ids.len() <= 64,
            "work outcome cannot reference more than 64 artifacts"
        );
        for artifact_id in &self.artifact_ids {
            ensure!(
                !artifact_id.trim().is_empty() && artifact_id.len() <= 160,
                "artifact id must be 1..=160 bytes"
            );
        }
        ensure!(
            serde_json::to_vec(&self.evidence)?.len() <= 16_384,
            "work outcome evidence cannot exceed 16384 bytes"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointRecord {
    pub id: String,
    pub goal_id: String,
    pub work_item_id: String,
    pub attempt_id: String,
    pub phase: CheckpointPhase,
    pub idempotency_key: String,
    pub outcome: Option<WorkOutcome>,
    pub created_at_unix: i64,
    pub updated_at_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResumeReport {
    pub goal: GoalRecord,
    pub work_items: Vec<WorkItemRecord>,
    pub attempts: Vec<AttemptRecord>,
    pub latest_checkpoint: Option<CheckpointRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoopEventRecord {
    pub sequence: i64,
    pub goal_id: Option<String>,
    pub event_type: String,
    pub actor: String,
    pub details: serde_json::Value,
    pub created_at_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalVerificationReport {
    pub goal: GoalRecord,
    pub achieved: bool,
    pub unmet_criteria: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderBudgetReservation {
    pub deadline_at_unix: i64,
    pub remaining_provider_calls: u32,
    pub remaining_response_bytes: u32,
}

pub(crate) fn validate_actor(actor: &str) -> anyhow::Result<()> {
    ensure!(
        !actor.trim().is_empty() && actor.len() <= 160,
        "actor must be 1..=160 bytes"
    );
    Ok(())
}

pub(crate) fn hash_serializable<T: Serialize>(value: &T) -> anyhow::Result<String> {
    let encoded = serde_json::to_vec(value).context("failed to encode hash input")?;
    let digest = Sha256::digest(encoded);
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!("sha256:{hex}"))
}
