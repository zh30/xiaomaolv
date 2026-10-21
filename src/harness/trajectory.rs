use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tracing::warn;

use crate::domain::StoredMessage;
use crate::harness::loop_engine::{TrajectoryFrameCapture, TrajectoryFrameDraft};
use crate::harness::observability::TrajectoryMetrics;
use crate::harness::store::HarnessStore;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    #[serde(default)]
    pub call_index: usize,
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
    /// Provider completions currently return text only; keep this empty until usage metadata exists.
    pub total_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// Deterministic fitness score for a finished trajectory. The scorecard is
/// the measurable half of the self-improvement loop: proposals and
/// deployments are judged against the same persisted numbers, so a rollout
/// can be compared to its own baseline rather than to a binary failure flag.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunScorecard {
    pub trajectory_id: String,
    /// Composite score in `0..=100` — higher is better.
    pub score: i64,
    /// Exit-reason weight: `final_answer` = 1.0, `max_iterations` = 0.4,
    /// `tool_error`/`timeout` = 0.1, `internal_error` = 0.0.
    pub completion: f64,
    /// 1.0 when a non-empty final answer was produced.
    pub answer_quality: f64,
    /// Share of tool calls that succeeded (1.0 when no tools ran).
    pub tool_health: f64,
    /// Wall-clock band: <10s = 1.0, <60s = 0.6, <300s = 0.3, otherwise 0.0.
    pub speed: f64,
    pub tool_calls: i64,
    pub failed_tool_calls: i64,
    pub verification_failures: i64,
    pub iterations: i64,
    pub latency_ms: i64,
    pub computed_at_unix: i64,
}

#[derive(Debug, Clone, Default)]
pub struct RunScorecardFilter {
    pub channel: Option<String>,
    pub model: Option<String>,
    pub since_unix: Option<i64>,
    pub until_unix: Option<i64>,
    pub min_score: Option<i64>,
    pub max_score: Option<i64>,
    /// Most recent first when false.
    pub ascending: bool,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScorecardSummary {
    pub window_start_unix: i64,
    pub window_end_unix: i64,
    pub run_count: i64,
    pub mean_score: Option<f64>,
    /// Distribution of runs across 0-49, 50-79 and 80-100 score bands.
    pub low_count: i64,
    pub mid_count: i64,
    pub high_count: i64,
    pub failed_tool_calls: i64,
    pub verification_failures: i64,
    pub exit_reasons: BTreeMap<String, i64>,
}

/// The composite is a documented contract: it must stay deterministic and
/// explainable because deployment reviews and proposal evidence consume it.
/// `60 * completion + 10 * answer + 15 * tool_health + 15 * speed
///  - 4 * failed_tool_calls (cap 20) - 5 * verification_failures (cap 15)`,
/// clamped to `0..=100`.
pub fn compute_run_scorecard(record: &TrajectoryRecord, computed_at_unix: i64) -> RunScorecard {
    let completion = match record.exit_reason {
        TrajectoryExitReason::FinalAnswer => 1.0,
        TrajectoryExitReason::MaxIterations => 0.4,
        TrajectoryExitReason::ToolError | TrajectoryExitReason::Timeout => 0.1,
        TrajectoryExitReason::InternalError => 0.0,
    };
    let answer_quality = f64::from(
        record
            .final_answer
            .as_deref()
            .is_some_and(|answer| !answer.trim().is_empty()),
    );
    let tool_calls = record.tool_calls.len() as i64;
    let failed_tool_calls = record.tool_calls.iter().filter(|call| !call.ok).count() as i64;
    let verification_failures = record
        .tool_calls
        .iter()
        .filter(|call| {
            call.result
                .get("verification_failed")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        })
        .count() as i64;
    let tool_health = if tool_calls == 0 {
        1.0
    } else {
        ((tool_calls - failed_tool_calls) as f64 / tool_calls as f64).clamp(0.0, 1.0)
    };
    let latency_ms = record
        .finished_at
        .map(|finished| (finished - record.started_at).num_milliseconds().max(0))
        .unwrap_or(0);
    let speed = if latency_ms < 10_000 {
        1.0
    } else if latency_ms < 60_000 {
        0.6
    } else if latency_ms < 300_000 {
        0.3
    } else {
        0.0
    };
    let iterations = record
        .tool_calls
        .iter()
        .map(|call| call.iteration)
        .max()
        .map_or(0, |max| max.saturating_add(1)) as i64;
    let raw = completion * 60.0 + answer_quality * 10.0 + tool_health * 15.0 + speed * 15.0
        - (failed_tool_calls.min(5) as f64) * 4.0
        - (verification_failures.min(3) as f64) * 5.0;
    RunScorecard {
        trajectory_id: record.id.clone(),
        score: raw.round().clamp(0.0, 100.0) as i64,
        completion,
        answer_quality,
        tool_health,
        speed,
        tool_calls,
        failed_tool_calls,
        verification_failures,
        iterations,
        latency_ms,
        computed_at_unix,
    }
}

#[derive(Debug, Clone)]
pub struct TrajectoryFilter {
    pub session_id: Option<String>,
    pub channel: Option<String>,
    pub user_id: Option<String>,
    pub exit_reason: Option<TrajectoryExitReason>,
    pub has_tool_errors: Option<bool>,
    pub limit: usize,
}

/// Runs scoring below this composite value are treated as self-reported
/// failures: they surface as triage signals and as proposal evidence.
pub const LOW_RUN_SCORE: i64 = 40;

pub const DEFAULT_TRAJECTORY_QUERY_LIMIT: usize = 100;
pub const MAX_TRAJECTORY_QUERY_LIMIT: usize = 500;

pub fn clamp_trajectory_query_limit(limit: usize) -> usize {
    if limit == 0 {
        DEFAULT_TRAJECTORY_QUERY_LIMIT
    } else {
        limit.min(MAX_TRAJECTORY_QUERY_LIMIT)
    }
}

#[derive(Clone)]
pub struct TrajectoryLogger {
    store: Arc<dyn HarnessStore>,
    enabled: bool,
}

impl TrajectoryLogger {
    pub fn new(store: Arc<dyn HarnessStore>, enabled: bool) -> Self {
        Self { store, enabled }
    }

    pub fn store(&self) -> Arc<dyn HarnessStore> {
        self.store.clone()
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
        self.store
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
        self.store
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
        self.store
            .finish_trajectory(trajectory_id, final_answer, exit_reason)
            .await
    }

    pub async fn log_provider_frame(&self, draft: TrajectoryFrameDraft) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        self.store.record_provider_frame(draft).await
    }

    pub async fn query_trajectories(
        &self,
        filter: TrajectoryFilter,
    ) -> anyhow::Result<Vec<TrajectoryRecord>> {
        self.store.query_trajectories(filter).await
    }
}

pub struct TrajectoryRun {
    logger: Option<TrajectoryLogger>,
    metrics: Option<TrajectoryMetrics>,
    id: String,
    started_at: Instant,
    started: bool,
    finished: bool,
    tool_calls: usize,
    max_iteration_seen: Option<usize>,
    provider_calls: u32,
    model: String,
}

impl TrajectoryRun {
    pub async fn start(
        logger: Option<TrajectoryLogger>,
        metrics: Option<TrajectoryMetrics>,
        session_id: &str,
        channel: &str,
        user_id: &str,
        model: &str,
    ) -> Self {
        let id = new_trajectory_id();
        let mut started = false;

        if let Some(logger) = &logger {
            match logger
                .start_trajectory(&id, session_id, channel, user_id, model)
                .await
            {
                Ok(()) => started = true,
                Err(err) => {
                    warn!(
                        trajectory_id = %id,
                        error = %err,
                        "failed to start trajectory"
                    );
                }
            }
        }

        Self {
            logger,
            metrics,
            id,
            started_at: Instant::now(),
            started,
            finished: false,
            tool_calls: 0,
            max_iteration_seen: None,
            provider_calls: 0,
            model: model.to_string(),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn observe_iteration(&mut self, iteration: usize) {
        self.max_iteration_seen = Some(
            self.max_iteration_seen
                .map_or(iteration, |current| current.max(iteration)),
        );
    }

    pub async fn log_tool_call(&mut self, mut record: ToolCallRecord) -> ToolCallRecord {
        self.observe_iteration(record.iteration);
        record.call_index = self.tool_calls;
        self.tool_calls = self.tool_calls.saturating_add(1);
        if let Some(metrics) = &self.metrics {
            metrics.record_tool_call(record.duration_ms, &record.server, &record.tool, record.ok);
        }
        let logged_record = record.clone();

        if !self.started {
            return logged_record;
        }

        if let Some(logger) = &self.logger
            && let Err(err) = logger.log_tool_call(&self.id, record).await
        {
            warn!(
                trajectory_id = %self.id,
                error = %err,
                "failed to log trajectory tool call"
            );
        }
        logged_record
    }

    pub async fn log_provider_call(
        &mut self,
        messages: &[StoredMessage],
        request_was_json: bool,
        response: &str,
    ) {
        let Some(logger) = &self.logger else {
            return;
        };
        if !self.started {
            return;
        }
        let call_index = self.provider_calls;
        self.provider_calls = self.provider_calls.saturating_add(1);
        let bounded_messages = bounded_replay_messages(messages, 128 * 1024);
        let bounded_response = truncate_utf8(response, 256 * 1024);
        let draft = TrajectoryFrameDraft {
            trajectory_id: self.id.clone(),
            call_index,
            model: self.model.clone(),
            provider_fingerprint: format!("model:{};seed:unknown;config:unavailable", self.model),
            request_messages: bounded_messages,
            request_was_json,
            response: bounded_response,
            capture: TrajectoryFrameCapture::Truncated,
        };
        if let Err(error) = logger.log_provider_frame(draft).await {
            warn!(trajectory_id = %self.id, %error, "failed to record provider trajectory frame");
        }
    }

    pub async fn finish(
        &mut self,
        final_answer: Option<String>,
        exit_reason: TrajectoryExitReason,
    ) {
        if self.finished {
            return;
        }
        self.finished = true;

        if let Some(metrics) = &self.metrics {
            let iterations = self
                .max_iteration_seen
                .map_or(0, |idx| idx.saturating_add(1));
            metrics.record_trajectory(
                self.started_at.elapsed().as_secs_f64(),
                iterations,
                self.tool_calls,
                exit_reason.as_str(),
            );
        }

        if !self.started {
            return;
        }

        if let Some(logger) = &self.logger
            && let Err(err) = logger
                .finish_trajectory(&self.id, final_answer, exit_reason)
                .await
        {
            warn!(
                trajectory_id = %self.id,
                error = %err,
                "failed to finish trajectory"
            );
        }
    }
}

/// Create a new trajectory ID
pub fn new_trajectory_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    static TRAJECTORY_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    let sequence = TRAJECTORY_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("traj-{now_ns:x}-{sequence:x}")
}

fn bounded_replay_messages(messages: &[StoredMessage], max_bytes: usize) -> Vec<StoredMessage> {
    let mut selected = Vec::new();
    let mut used = 2_usize;
    for message in messages.iter().rev() {
        let remaining = max_bytes.saturating_sub(used);
        if remaining < 64 {
            break;
        }
        let content = truncate_utf8(&message.content, remaining.saturating_sub(48));
        used = used.saturating_add(content.len()).saturating_add(48);
        selected.push(StoredMessage {
            role: message.role.clone(),
            content,
        });
    }
    selected.reverse();
    if selected.is_empty() {
        selected.push(StoredMessage {
            role: crate::domain::MessageRole::System,
            content: "[trajectory request omitted by capture budget]".to_string(),
        });
    }
    selected
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value[..boundary].to_string()
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
        let ids = (0..1_000)
            .map(|_| new_trajectory_id())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(ids.len(), 1_000);
        assert!(ids.iter().all(|id| id.starts_with("traj-")));
    }

    #[test]
    fn test_tool_call_record_serde() {
        let record = ToolCallRecord {
            call_index: 0,
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
