mod artifacts;
mod domain;
mod engine;
mod replay;
mod self_test;
mod signals;
mod store;
mod worker;

pub use artifacts::{ArtifactKind, ArtifactPublishResult, ArtifactRecord, PublishArtifactRequest};
pub use domain::{
    AcceptanceCriterion, ApproveGoalRequest, AttemptRecord, AttemptStatus, CheckpointPhase,
    CheckpointRecord, CreateGoalRequest, EffectClass, ExecutionBudget, GoalRecord, GoalStatus,
    GoalVerificationReport, LoopEventRecord, PlanGoalRequest, PlannedGoal,
    ProviderBudgetReservation, ResumeReport, RetryPolicy, WorkClaim, WorkItemRecord,
    WorkItemStatus, WorkOutcome, WorkflowEdge, WorkflowSpec, WorkflowStep,
};
pub use engine::LoopEngine;
pub use replay::{
    ReplayCaseResult, ReplayMode, ReplayRun, ReplayStatus, TrajectoryFrame, TrajectoryFrameCapture,
    TrajectoryFrameDraft,
};
pub use self_test::{SelfTestCaseResult, SelfTestRun, SelfTestStatus};
pub use signals::{
    CreateSignalRequest, SignalIngestResult, SignalKind, SignalRecord, SignalStatus, SignalTrust,
};
pub use store::{LoopStore, SqliteLoopStore, initialize_loop_engine_schema};
pub use worker::{LoopWorker, WorkHandler, WorkHandlerContext, WorkHandlerRegistry};
