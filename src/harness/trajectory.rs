use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::memory::MemoryBackend;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub server: String,
    pub tool: String,
    pub arguments: serde_json::Value,
    pub result: serde_json::Value,
    pub ok: bool,
    pub duration_ms: u64,
    pub iteration: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryRecord {
    pub id: String,
    pub session_id: String,
    pub channel: String,
    pub user_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub tool_calls: Vec<ToolCallRecord>,
    pub final_answer: Option<String>,
    pub exit_reason: TrajectoryExitReason,
    pub model: String,
    pub total_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TrajectoryExitReason {
    FinalAnswer,
    MaxIterations,
    ToolError,
    Timeout,
    InternalError,
}

impl TrajectoryExitReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FinalAnswer => "final_answer",
            Self::MaxIterations => "max_iterations",
            Self::ToolError => "tool_error",
            Self::Timeout => "timeout",
            Self::InternalError => "internal_error",
        }
    }

    pub fn from_db(value: &str) -> Self {
        match value {
            "max_iterations" => Self::MaxIterations,
            "tool_error" => Self::ToolError,
            "timeout" => Self::Timeout,
            "internal_error" => Self::InternalError,
            _ => Self::FinalAnswer,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TrajectoryFilter {
    pub session_id: Option<String>,
    pub channel: Option<String>,
    pub user_id: Option<String>,
    pub limit: usize,
}

#[derive(Clone)]
pub struct TrajectoryLogger {
    memory: Arc<dyn MemoryBackend>,
    enabled: bool,
}

impl TrajectoryLogger {
    pub fn new(memory: Arc<dyn MemoryBackend>, enabled: bool) -> Self {
        Self { memory, enabled }
    }

    pub async fn start_trajectory(
        &self,
        trajectory_id: &str,
        session_id: &str,
        channel: &str,
        user_id: &str,
        model: &str,
    ) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        self.memory
            .start_trajectory(trajectory_id, session_id, channel, user_id, model)
            .await
    }

    pub async fn log_tool_call(
        &self,
        trajectory_id: &str,
        record: ToolCallRecord,
    ) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        self.memory
            .insert_trajectory_tool_call(trajectory_id, record)
            .await
    }

    pub async fn finish_trajectory(
        &self,
        trajectory_id: &str,
        final_answer: Option<String>,
        exit_reason: TrajectoryExitReason,
    ) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        self.memory
            .finish_trajectory(trajectory_id, final_answer, exit_reason)
            .await
    }

    pub async fn query_trajectories(
        &self,
        filter: TrajectoryFilter,
    ) -> anyhow::Result<Vec<TrajectoryRecord>> {
        self.memory.query_trajectories(filter).await
    }
}

/// Create a new trajectory ID
pub fn new_trajectory_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|v| v.as_millis())
        .unwrap_or(0);
    let rand: u16 = (now_ms % 65536) as u16;
    format!("traj-{now_ms}-{rand}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trajectory_exit_reason_serialization() {
        assert_eq!(TrajectoryExitReason::FinalAnswer.as_str(), "final_answer");
        assert_eq!(
            TrajectoryExitReason::MaxIterations.as_str(),
            "max_iterations"
        );
        assert_eq!(TrajectoryExitReason::ToolError.as_str(), "tool_error");
        assert_eq!(TrajectoryExitReason::Timeout.as_str(), "timeout");
        assert_eq!(
            TrajectoryExitReason::InternalError.as_str(),
            "internal_error"
        );
    }

    #[test]
    fn test_trajectory_exit_reason_from_db() {
        assert!(matches!(
            TrajectoryExitReason::from_db("final_answer"),
            TrajectoryExitReason::FinalAnswer
        ));
        assert!(matches!(
            TrajectoryExitReason::from_db("max_iterations"),
            TrajectoryExitReason::MaxIterations
        ));
        assert!(matches!(
            TrajectoryExitReason::from_db("tool_error"),
            TrajectoryExitReason::ToolError
        ));
        assert!(matches!(
            TrajectoryExitReason::from_db("timeout"),
            TrajectoryExitReason::Timeout
        ));
        assert!(matches!(
            TrajectoryExitReason::from_db("internal_error"),
            TrajectoryExitReason::InternalError
        ));
    }

    #[test]
    fn test_new_trajectory_id() {
        let id1 = new_trajectory_id();
        // Add small delay to ensure different timestamp
        std::thread::sleep(std::time::Duration::from_millis(2));
        let id2 = new_trajectory_id();
        assert!(id1.starts_with("traj-"));
        assert!(id2.starts_with("traj-"));
        // IDs should be unique-ish (different timestamps or rand)
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_tool_call_record_serde() {
        let record = ToolCallRecord {
            server: "test-server".to_string(),
            tool: "test-tool".to_string(),
            arguments: serde_json::json!({"arg": "value"}),
            result: serde_json::json!({"result": "ok"}),
            ok: true,
            duration_ms: 100,
            iteration: 1,
        };
        let serialized = serde_json::to_string(&record).unwrap();
        let deserialized: ToolCallRecord = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.server, "test-server");
        assert_eq!(deserialized.tool, "test-tool");
        assert!(deserialized.ok);
    }
}
