use crate::harness::observability::TrajectoryMetrics;
use crate::harness::trajectory::{
    ToolCallRecord, TrajectoryExitReason, TrajectoryLogger, TrajectoryRun,
};

pub struct AgentRunStart {
    pub logger: Option<TrajectoryLogger>,
    pub metrics: Option<TrajectoryMetrics>,
    pub session_id: String,
    pub channel: String,
    pub user_id: String,
    pub model: String,
}

pub enum AgentRunExit {
    FinalAnswer(String),
    MaxIterations(String),
    ToolError(String),
    Timeout,
    InternalError,
}

pub struct AgentRun {
    trajectory: TrajectoryRun,
    finished: bool,
}

impl AgentRun {
    pub async fn start(start: AgentRunStart) -> Self {
        let trajectory = TrajectoryRun::start(
            start.logger,
            start.metrics,
            &start.session_id,
            &start.channel,
            &start.user_id,
            &start.model,
        )
        .await;

        Self {
            trajectory,
            finished: false,
        }
    }

    pub fn id(&self) -> &str {
        self.trajectory.id()
    }

    pub fn observe_iteration(&mut self, iteration: usize) {
        self.trajectory.observe_iteration(iteration);
    }

    pub async fn record_tool_call(&mut self, record: ToolCallRecord) -> ToolCallRecord {
        self.trajectory.log_tool_call(record).await
    }

    pub async fn finish(&mut self, exit: AgentRunExit) {
        if self.finished {
            return;
        }
        self.finished = true;

        let (answer, reason) = match exit {
            AgentRunExit::FinalAnswer(answer) => (Some(answer), TrajectoryExitReason::FinalAnswer),
            AgentRunExit::MaxIterations(answer) => {
                (Some(answer), TrajectoryExitReason::MaxIterations)
            }
            AgentRunExit::ToolError(answer) => (Some(answer), TrajectoryExitReason::ToolError),
            AgentRunExit::Timeout => (None, TrajectoryExitReason::Timeout),
            AgentRunExit::InternalError => (None, TrajectoryExitReason::InternalError),
        };

        self.trajectory.finish(answer, reason).await;
    }
}
