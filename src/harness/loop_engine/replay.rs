use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use crate::domain::StoredMessage;

use super::domain::hash_serializable;
use super::store::new_loop_id;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrajectoryFrameCapture {
    Full,
    Redacted,
    Truncated,
}

impl TrajectoryFrameCapture {
    fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Redacted => "redacted",
            Self::Truncated => "truncated",
        }
    }

    fn from_db(value: &str) -> anyhow::Result<Self> {
        match value {
            "full" => Ok(Self::Full),
            "redacted" => Ok(Self::Redacted),
            "truncated" => Ok(Self::Truncated),
            other => bail!("unknown trajectory frame capture: {other}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrajectoryFrameDraft {
    pub trajectory_id: String,
    pub call_index: u32,
    pub model: String,
    pub provider_fingerprint: String,
    pub request_messages: Vec<StoredMessage>,
    pub request_was_json: bool,
    pub response: String,
    pub capture: TrajectoryFrameCapture,
}

impl TrajectoryFrameDraft {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            !self.trajectory_id.trim().is_empty() && self.trajectory_id.len() <= 160,
            "trajectory id must be 1..=160 bytes"
        );
        ensure!(
            self.call_index <= 1024,
            "provider call index cannot exceed 1024"
        );
        ensure!(
            !self.model.trim().is_empty() && self.model.len() <= 200,
            "model must be 1..=200 bytes"
        );
        ensure!(
            !self.provider_fingerprint.trim().is_empty() && self.provider_fingerprint.len() <= 300,
            "provider fingerprint must be 1..=300 bytes"
        );
        ensure!(
            !self.request_messages.is_empty() && self.request_messages.len() <= 256,
            "trajectory frame must contain 1..=256 request messages"
        );
        ensure!(
            serde_json::to_vec(&self.request_messages)?.len() <= 131_072,
            "trajectory frame request cannot exceed 131072 bytes"
        );
        ensure!(
            self.response.len() <= 262_144,
            "trajectory frame response cannot exceed 262144 bytes"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrajectoryFrame {
    pub id: String,
    pub trajectory_id: String,
    pub call_index: u32,
    pub model: String,
    pub provider_fingerprint: String,
    pub request_messages: Vec<StoredMessage>,
    pub request_was_json: bool,
    pub request_hash: String,
    pub response: String,
    pub response_hash: String,
    pub capture: TrajectoryFrameCapture,
    pub created_at_unix: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayMode {
    Structural,
    ShadowComparative,
}

impl ReplayMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Structural => "structural",
            Self::ShadowComparative => "shadow_comparative",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayStatus {
    Passed,
    Failed,
    Comparative,
}

impl ReplayStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Comparative => "comparative",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayCaseResult {
    pub frame_id: String,
    pub call_index: u32,
    pub passed: bool,
    pub details: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayRun {
    pub id: String,
    pub trajectory_id: String,
    pub mode: ReplayMode,
    pub status: ReplayStatus,
    pub exact_replayable: bool,
    pub live_tools_executed: u32,
    pub cases: Vec<ReplayCaseResult>,
    pub created_at_unix: i64,
}

pub(crate) async fn initialize_replay_schema(pool: &SqlitePool) -> anyhow::Result<()> {
    for statement in [
        "CREATE TABLE IF NOT EXISTS harness_trajectory_frames (
            id TEXT PRIMARY KEY,
            trajectory_id TEXT NOT NULL,
            call_index INTEGER NOT NULL,
            model TEXT NOT NULL,
            provider_fingerprint TEXT NOT NULL,
            request_messages_json TEXT NOT NULL,
            request_was_json INTEGER NOT NULL,
            request_hash TEXT NOT NULL,
            response TEXT NOT NULL,
            response_hash TEXT NOT NULL,
            capture TEXT NOT NULL,
            created_by TEXT NOT NULL,
            created_at INTEGER NOT NULL DEFAULT (unixepoch()),
            UNIQUE(trajectory_id, call_index)
        )",
        "CREATE INDEX IF NOT EXISTS idx_harness_trajectory_frames_trajectory
         ON harness_trajectory_frames(trajectory_id, call_index)",
        "CREATE TABLE IF NOT EXISTS harness_replay_runs (
            id TEXT PRIMARY KEY,
            trajectory_id TEXT NOT NULL,
            mode TEXT NOT NULL,
            status TEXT NOT NULL,
            exact_replayable INTEGER NOT NULL,
            live_tools_executed INTEGER NOT NULL,
            created_by TEXT NOT NULL,
            created_at INTEGER NOT NULL DEFAULT (unixepoch())
        )",
        "CREATE INDEX IF NOT EXISTS idx_harness_replay_runs_trajectory_created
         ON harness_replay_runs(trajectory_id, created_at DESC, id)",
        "CREATE TABLE IF NOT EXISTS harness_replay_case_results (
            run_id TEXT NOT NULL REFERENCES harness_replay_runs(id),
            case_index INTEGER NOT NULL,
            frame_id TEXT NOT NULL,
            call_index INTEGER NOT NULL,
            passed INTEGER NOT NULL,
            details TEXT NOT NULL,
            PRIMARY KEY (run_id, case_index)
        )",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .context("failed to initialize session replay schema")?;
    }
    Ok(())
}

pub(crate) async fn record_trajectory_frame(
    pool: &SqlitePool,
    draft: TrajectoryFrameDraft,
    actor: &str,
) -> anyhow::Result<TrajectoryFrame> {
    let request_hash = request_hash(&draft.request_messages, draft.request_was_json)?;
    let response_hash = hash_serializable(&draft.response)?;
    let mut tx = pool.begin().await?;
    let existing = sqlx::query(
        "SELECT id, trajectory_id, call_index, model, provider_fingerprint,
                request_messages_json, request_was_json, request_hash, response,
                response_hash, capture, created_at
         FROM harness_trajectory_frames WHERE trajectory_id = ?1 AND call_index = ?2",
    )
    .bind(&draft.trajectory_id)
    .bind(i64::from(draft.call_index))
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(row) = existing {
        let existing = decode_frame(row)?;
        ensure!(
            existing.request_hash == request_hash && existing.response_hash == response_hash,
            "provider call index was already recorded with different content"
        );
        tx.commit().await?;
        return Ok(existing);
    }
    let id = new_loop_id("frame");
    sqlx::query(
        "INSERT INTO harness_trajectory_frames
         (id, trajectory_id, call_index, model, provider_fingerprint,
          request_messages_json, request_was_json, request_hash, response,
          response_hash, capture, created_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )
    .bind(&id)
    .bind(&draft.trajectory_id)
    .bind(i64::from(draft.call_index))
    .bind(&draft.model)
    .bind(&draft.provider_fingerprint)
    .bind(serde_json::to_string(&draft.request_messages)?)
    .bind(i64::from(draft.request_was_json))
    .bind(&request_hash)
    .bind(&draft.response)
    .bind(&response_hash)
    .bind(draft.capture.as_str())
    .bind(actor)
    .execute(&mut *tx)
    .await?;
    let row = sqlx::query(
        "SELECT id, trajectory_id, call_index, model, provider_fingerprint,
                request_messages_json, request_was_json, request_hash, response,
                response_hash, capture, created_at
         FROM harness_trajectory_frames WHERE id = ?1",
    )
    .bind(&id)
    .fetch_one(&mut *tx)
    .await?;
    let frame = decode_frame(row)?;
    tx.commit().await?;
    Ok(frame)
}

pub(crate) async fn list_frames(
    pool: &SqlitePool,
    trajectory_id: &str,
) -> anyhow::Result<Vec<TrajectoryFrame>> {
    sqlx::query(
        "SELECT id, trajectory_id, call_index, model, provider_fingerprint,
                request_messages_json, request_was_json, request_hash, response,
                response_hash, capture, created_at
         FROM harness_trajectory_frames WHERE trajectory_id = ?1 ORDER BY call_index",
    )
    .bind(trajectory_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(decode_frame)
    .collect()
}

pub(crate) async fn run_structural_replay(
    pool: &SqlitePool,
    trajectory_id: &str,
    actor: &str,
) -> anyhow::Result<ReplayRun> {
    let frames = list_frames(pool, trajectory_id).await?;
    ensure!(!frames.is_empty(), "trajectory has no provider frames");
    let fingerprint = frames[0].provider_fingerprint.as_str();
    let exact_replayable = frames.iter().all(|frame| {
        frame.capture == TrajectoryFrameCapture::Full && frame.provider_fingerprint == fingerprint
    });
    let mut cases = Vec::with_capacity(frames.len());
    for (expected_index, frame) in frames.iter().enumerate() {
        let expected_index = u32::try_from(expected_index).context("too many replay frames")?;
        let request_matches =
            request_hash(&frame.request_messages, frame.request_was_json)? == frame.request_hash;
        let response_matches = hash_serializable(&frame.response)? == frame.response_hash;
        let ordered = frame.call_index == expected_index;
        let passed = ordered && request_matches && response_matches;
        cases.push(ReplayCaseResult {
            frame_id: frame.id.clone(),
            call_index: frame.call_index,
            passed,
            details: if passed {
                "frame hashes and order are intact; no live tools executed".to_string()
            } else {
                format!(
                    "ordered={ordered}, request_hash_valid={request_matches}, response_hash_valid={response_matches}"
                )
            },
        });
    }
    let status = if cases.iter().all(|case| case.passed) {
        ReplayStatus::Passed
    } else {
        ReplayStatus::Failed
    };
    let run = ReplayRun {
        id: new_loop_id("replay"),
        trajectory_id: trajectory_id.to_string(),
        mode: ReplayMode::Structural,
        status,
        exact_replayable,
        live_tools_executed: 0,
        cases,
        created_at_unix: unix_now(),
    };
    persist_replay(pool, &run, actor).await?;
    Ok(run)
}

fn decode_frame(row: sqlx::sqlite::SqliteRow) -> anyhow::Result<TrajectoryFrame> {
    let call_index: i64 = row.try_get("call_index")?;
    let request_json: String = row.try_get("request_messages_json")?;
    Ok(TrajectoryFrame {
        id: row.try_get("id")?,
        trajectory_id: row.try_get("trajectory_id")?,
        call_index: u32::try_from(call_index).context("invalid provider call index")?,
        model: row.try_get("model")?,
        provider_fingerprint: row.try_get("provider_fingerprint")?,
        request_messages: serde_json::from_str(&request_json)
            .context("invalid trajectory frame request")?,
        request_was_json: row.try_get::<i64, _>("request_was_json")? != 0,
        request_hash: row.try_get("request_hash")?,
        response: row.try_get("response")?,
        response_hash: row.try_get("response_hash")?,
        capture: TrajectoryFrameCapture::from_db(row.try_get("capture")?)?,
        created_at_unix: row.try_get("created_at")?,
    })
}

async fn persist_replay(pool: &SqlitePool, run: &ReplayRun, actor: &str) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO harness_replay_runs
         (id, trajectory_id, mode, status, exact_replayable, live_tools_executed,
          created_by, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )
    .bind(&run.id)
    .bind(&run.trajectory_id)
    .bind(run.mode.as_str())
    .bind(run.status.as_str())
    .bind(i64::from(run.exact_replayable))
    .bind(i64::from(run.live_tools_executed))
    .bind(actor)
    .bind(run.created_at_unix)
    .execute(&mut *tx)
    .await?;
    for (index, case) in run.cases.iter().enumerate() {
        sqlx::query(
            "INSERT INTO harness_replay_case_results
             (run_id, case_index, frame_id, call_index, passed, details)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(&run.id)
        .bind(i64::try_from(index).context("too many replay cases")?)
        .bind(&case.frame_id)
        .bind(i64::from(case.call_index))
        .bind(i64::from(case.passed))
        .bind(&case.details)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

fn request_hash(messages: &[StoredMessage], request_was_json: bool) -> anyhow::Result<String> {
    hash_serializable(&(messages, request_was_json))
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
