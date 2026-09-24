use std::collections::BTreeMap;

use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use super::domain::hash_serializable;
use super::store::new_loop_id;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalKind {
    Trajectory,
    UserFeedback,
    DeveloperFeedback,
    Community,
    SelfTest,
    SessionReplay,
    Manual,
}

impl SignalKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Trajectory => "trajectory",
            Self::UserFeedback => "user_feedback",
            Self::DeveloperFeedback => "developer_feedback",
            Self::Community => "community",
            Self::SelfTest => "self_test",
            Self::SessionReplay => "session_replay",
            Self::Manual => "manual",
        }
    }

    fn from_db(value: &str) -> anyhow::Result<Self> {
        match value {
            "trajectory" => Ok(Self::Trajectory),
            "user_feedback" => Ok(Self::UserFeedback),
            "developer_feedback" => Ok(Self::DeveloperFeedback),
            "community" => Ok(Self::Community),
            "self_test" => Ok(Self::SelfTest),
            "session_replay" => Ok(Self::SessionReplay),
            "manual" => Ok(Self::Manual),
            other => bail!("unknown signal kind: {other}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalTrust {
    Internal,
    Authenticated,
    External,
}

impl SignalTrust {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::Authenticated => "authenticated",
            Self::External => "external",
        }
    }

    fn from_db(value: &str) -> anyhow::Result<Self> {
        match value {
            "internal" => Ok(Self::Internal),
            "authenticated" => Ok(Self::Authenticated),
            "external" => Ok(Self::External),
            other => bail!("unknown signal trust: {other}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalStatus {
    Observed,
    Triaged,
    Proposed,
    Ignored,
}

impl SignalStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Observed => "observed",
            Self::Triaged => "triaged",
            Self::Proposed => "proposed",
            Self::Ignored => "ignored",
        }
    }

    fn from_db(value: &str) -> anyhow::Result<Self> {
        match value {
            "observed" => Ok(Self::Observed),
            "triaged" => Ok(Self::Triaged),
            "proposed" => Ok(Self::Proposed),
            "ignored" => Ok(Self::Ignored),
            other => bail!("unknown signal status: {other}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSignalRequest {
    pub kind: SignalKind,
    pub trust: SignalTrust,
    pub source: String,
    pub external_id: Option<String>,
    pub content: String,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

impl CreateSignalRequest {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            !self.source.trim().is_empty() && self.source.len() <= 160,
            "signal source must be 1..=160 bytes"
        );
        if let Some(external_id) = &self.external_id {
            ensure!(
                !external_id.trim().is_empty() && external_id.len() <= 200,
                "signal external id must be 1..=200 bytes"
            );
        }
        ensure!(
            !self.content.trim().is_empty() && self.content.chars().count() <= 16_384,
            "signal content must be 1..=16384 characters"
        );
        ensure!(
            self.metadata.len() <= 64,
            "signal metadata cannot exceed 64 entries"
        );
        for (key, value) in &self.metadata {
            ensure!(
                !key.trim().is_empty() && key.len() <= 96 && value.len() <= 1024,
                "signal metadata key/value exceeds limits"
            );
        }
        ensure!(
            serde_json::to_vec(&self.metadata)?.len() <= 8192,
            "signal metadata cannot exceed 8192 bytes"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalRecord {
    pub id: String,
    pub kind: SignalKind,
    pub trust: SignalTrust,
    pub status: SignalStatus,
    pub source: String,
    pub external_id: Option<String>,
    pub content: String,
    pub content_hash: String,
    pub metadata: BTreeMap<String, String>,
    pub created_at_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalIngestResult {
    pub signal: SignalRecord,
    pub deduplicated: bool,
}

/// Token-set Jaccard similarity at or above which two signals from the same
/// source and kind are treated as near-duplicates.
const NEAR_DUPLICATE_SIMILARITY: f64 = 0.9;
/// How many recent same-source signals are scanned for near-duplicates.
const NEAR_DUPLICATE_SCAN_LIMIT: i64 = 128;

/// Collapses case, whitespace, and punctuation differences so trivially
/// reworded duplicates share one hash. Falls back to trimmed raw content when
/// normalization erases everything (e.g. punctuation-only content).
fn normalized_signal_hash(kind: SignalKind, content: &str) -> String {
    let normalized = normalize_for_tokens(content);
    let hash_input = if normalized.is_empty() {
        content.trim().to_string()
    } else {
        normalized
    };
    hash_serializable(&(kind.as_str(), hash_input)).unwrap_or_else(|_| String::new())
}

fn signal_tokens(content: &str) -> std::collections::BTreeSet<String> {
    normalize_for_tokens(content)
        .split(' ')
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect()
}

fn normalize_for_tokens(content: &str) -> String {
    let mut normalized = String::with_capacity(content.len());
    let mut pending_space = false;
    for ch in content.trim().chars() {
        if ch.is_alphanumeric() {
            if pending_space && !normalized.is_empty() {
                normalized.push(' ');
            }
            pending_space = false;
            normalized.extend(ch.to_lowercase());
        } else {
            pending_space = true;
        }
    }
    normalized
}

fn jaccard_similarity(
    left: &std::collections::BTreeSet<String>,
    right: &std::collections::BTreeSet<String>,
) -> f64 {
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let intersection = left.intersection(right).count() as f64;
    let union = left.union(right).count() as f64;
    intersection / union
}

pub(crate) async fn initialize_signal_schema(pool: &SqlitePool) -> anyhow::Result<()> {
    for statement in [
        "CREATE TABLE IF NOT EXISTS harness_signals (
            id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            trust TEXT NOT NULL,
            source TEXT NOT NULL,
            external_id TEXT,
            content TEXT NOT NULL,
            content_hash TEXT NOT NULL,
            fingerprint TEXT NOT NULL,
            normalized_hash TEXT,
            metadata_json TEXT NOT NULL,
            created_by TEXT NOT NULL,
            created_at INTEGER NOT NULL DEFAULT (unixepoch()),
            UNIQUE(source, fingerprint),
            UNIQUE(source, external_id)
        )",
        "CREATE INDEX IF NOT EXISTS idx_harness_signals_kind_created
         ON harness_signals(kind, created_at DESC, id)",
        "CREATE TABLE IF NOT EXISTS harness_signal_events (
            sequence INTEGER PRIMARY KEY AUTOINCREMENT,
            signal_id TEXT NOT NULL REFERENCES harness_signals(id),
            status TEXT NOT NULL,
            actor TEXT NOT NULL,
            details_json TEXT NOT NULL,
            created_at INTEGER NOT NULL DEFAULT (unixepoch())
        )",
        "CREATE INDEX IF NOT EXISTS idx_harness_signal_events_signal_sequence
         ON harness_signal_events(signal_id, sequence DESC)",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .context("failed to initialize evolution signal schema")?;
    }
    // Existing databases predate the normalized_hash column; add and backfill.
    let has_normalized_hash = sqlx::query(
        "SELECT 1 FROM pragma_table_info('harness_signals') WHERE name = 'normalized_hash'",
    )
    .fetch_optional(pool)
    .await?
    .is_some();
    if !has_normalized_hash {
        sqlx::query("ALTER TABLE harness_signals ADD COLUMN normalized_hash TEXT")
            .execute(pool)
            .await
            .context("failed to add signal normalized_hash column")?;
    }
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_harness_signals_source_normhash
         ON harness_signals(source, normalized_hash)",
    )
    .execute(pool)
    .await
    .context("failed to index signal normalized hashes")?;
    let pending: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT id, kind, content FROM harness_signals WHERE normalized_hash IS NULL",
    )
    .fetch_all(pool)
    .await?;
    for (id, kind, content) in pending {
        let kind = SignalKind::from_db(&kind)?;
        sqlx::query("UPDATE harness_signals SET normalized_hash = ?1 WHERE id = ?2")
            .bind(normalized_signal_hash(kind, &content))
            .bind(&id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

pub(crate) async fn ingest_signal(
    pool: &SqlitePool,
    request: CreateSignalRequest,
    actor: &str,
) -> anyhow::Result<SignalIngestResult> {
    let fingerprint = hash_serializable(&request)?;
    let content_hash = hash_serializable(&request.content)?;
    let normalized_hash = normalized_signal_hash(request.kind, &request.content);
    let mut tx = pool.begin().await?;
    let existing_id = if let Some(external_id) = &request.external_id {
        sqlx::query_scalar::<_, String>(
            "SELECT id FROM harness_signals
             WHERE source = ?1 AND (external_id = ?2 OR fingerprint = ?3)
             ORDER BY created_at LIMIT 1",
        )
        .bind(request.source.trim())
        .bind(external_id)
        .bind(&fingerprint)
        .fetch_optional(&mut *tx)
        .await?
    } else {
        sqlx::query_scalar::<_, String>(
            "SELECT id FROM harness_signals WHERE source = ?1 AND fingerprint = ?2",
        )
        .bind(request.source.trim())
        .bind(&fingerprint)
        .fetch_optional(&mut *tx)
        .await?
    };
    // Layer two: normalization-stable duplicates (case/whitespace/punctuation
    // variants of the same report from the same source and kind).
    let existing_id = existing_id.or({
        sqlx::query_scalar::<_, String>(
            "SELECT id FROM harness_signals
             WHERE source = ?1 AND normalized_hash = ?2
             ORDER BY created_at LIMIT 1",
        )
        .bind(request.source.trim())
        .bind(&normalized_hash)
        .fetch_optional(&mut *tx)
        .await?
    });
    // Layer three: near-duplicates — token-set Jaccard similarity against the
    // most recent signals from the same source and kind.
    let existing_id = match existing_id {
        Some(id) => Some(id),
        None => {
            let tokens = signal_tokens(&request.content);
            if tokens.is_empty() {
                None
            } else {
                let recent: Vec<(String, String)> = sqlx::query_as(
                    "SELECT id, content FROM harness_signals
                     WHERE source = ?1 AND kind = ?2
                     ORDER BY created_at DESC, id DESC LIMIT ?3",
                )
                .bind(request.source.trim())
                .bind(request.kind.as_str())
                .bind(NEAR_DUPLICATE_SCAN_LIMIT)
                .fetch_all(&mut *tx)
                .await?;
                recent
                    .into_iter()
                    .map(|(id, content)| {
                        (id, jaccard_similarity(&tokens, &signal_tokens(&content)))
                    })
                    .filter(|(_, score)| *score >= NEAR_DUPLICATE_SIMILARITY)
                    .max_by(|left, right| left.1.total_cmp(&right.1))
                    .map(|(id, _)| id)
            }
        }
    };
    if let Some(existing_id) = existing_id {
        tx.commit().await?;
        let signal = get_signal(pool, &existing_id)
            .await?
            .context("deduplicated signal disappeared")?;
        return Ok(SignalIngestResult {
            signal,
            deduplicated: true,
        });
    }

    let id = new_loop_id("signal");
    sqlx::query(
        "INSERT INTO harness_signals
         (id, kind, trust, source, external_id, content, content_hash, fingerprint,
          normalized_hash, metadata_json, created_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )
    .bind(&id)
    .bind(request.kind.as_str())
    .bind(request.trust.as_str())
    .bind(request.source.trim())
    .bind(&request.external_id)
    .bind(request.content.trim())
    .bind(&content_hash)
    .bind(&fingerprint)
    .bind(&normalized_hash)
    .bind(serde_json::to_string(&request.metadata)?)
    .bind(actor)
    .execute(&mut *tx)
    .await?;
    insert_signal_event_tx(
        &mut tx,
        &id,
        SignalStatus::Observed,
        actor,
        serde_json::json!({"fingerprint": fingerprint}),
    )
    .await?;
    tx.commit().await?;
    let signal = get_signal(pool, &id)
        .await?
        .context("created signal disappeared")?;
    Ok(SignalIngestResult {
        signal,
        deduplicated: false,
    })
}

pub(crate) async fn get_signal(
    pool: &SqlitePool,
    signal_id: &str,
) -> anyhow::Result<Option<SignalRecord>> {
    let row = sqlx::query(
        "SELECT s.id, s.kind, s.trust, s.source, s.external_id, s.content,
                s.content_hash, s.metadata_json, s.created_at,
                COALESCE((SELECT e.status FROM harness_signal_events e
                          WHERE e.signal_id = s.id ORDER BY e.sequence DESC LIMIT 1),
                         'observed') AS status
         FROM harness_signals s WHERE s.id = ?1",
    )
    .bind(signal_id)
    .fetch_optional(pool)
    .await?;
    row.map(decode_signal).transpose()
}

pub(crate) async fn list_signals(
    pool: &SqlitePool,
    limit: usize,
    status: Option<SignalStatus>,
) -> anyhow::Result<Vec<SignalRecord>> {
    let limit = i64::try_from(limit.clamp(1, 500)).context("signal limit exceeds sqlite range")?;
    let rows = if let Some(status) = status {
        sqlx::query(
            "SELECT s.id, s.kind, s.trust, s.source, s.external_id, s.content,
                    s.content_hash, s.metadata_json, s.created_at,
                    COALESCE((SELECT e.status FROM harness_signal_events e
                              WHERE e.signal_id = s.id ORDER BY e.sequence DESC LIMIT 1),
                             'observed') AS status
             FROM harness_signals s
             WHERE COALESCE((SELECT e.status FROM harness_signal_events e
                             WHERE e.signal_id = s.id ORDER BY e.sequence DESC LIMIT 1),
                            'observed') = ?2
             ORDER BY s.created_at DESC, s.id DESC LIMIT ?1",
        )
        .bind(limit)
        .bind(status.as_str())
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query(
            "SELECT s.id, s.kind, s.trust, s.source, s.external_id, s.content,
                    s.content_hash, s.metadata_json, s.created_at,
                    COALESCE((SELECT e.status FROM harness_signal_events e
                              WHERE e.signal_id = s.id ORDER BY e.sequence DESC LIMIT 1),
                             'observed') AS status
             FROM harness_signals s ORDER BY s.created_at DESC, s.id DESC LIMIT ?1",
        )
        .bind(limit)
        .fetch_all(pool)
        .await?
    };
    rows.into_iter().map(decode_signal).collect()
}

/// Marks a pending signal as ignored with an operator reason. `proposed` and
/// already-`ignored` signals are terminal for this transition.
pub(crate) async fn ignore_signal(
    pool: &SqlitePool,
    signal_id: &str,
    reason: &str,
    actor: &str,
) -> anyhow::Result<SignalRecord> {
    ensure!(
        !reason.trim().is_empty() && reason.chars().count() <= 512,
        "signal ignore reason must be 1..=512 characters"
    );
    let signal = get_signal(pool, signal_id)
        .await?
        .context("signal not found")?;
    ensure!(
        matches!(
            signal.status,
            SignalStatus::Observed | SignalStatus::Triaged
        ),
        "signal cannot be ignored from status '{}'",
        signal.status.as_str()
    );
    let mut tx = pool.begin().await?;
    insert_signal_event_tx(
        &mut tx,
        signal_id,
        SignalStatus::Ignored,
        actor,
        serde_json::json!({"reason": reason.trim()}),
    )
    .await?;
    tx.commit().await?;
    get_signal(pool, signal_id)
        .await?
        .context("ignored signal disappeared")
}

pub(crate) async fn mark_signal_proposed(
    pool: &SqlitePool,
    signal_id: &str,
    goal_id: &str,
    actor: &str,
) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM harness_signals WHERE id = ?1")
        .bind(signal_id)
        .fetch_one(&mut *tx)
        .await?;
    ensure!(exists == 1, "signal not found");
    insert_signal_event_tx(
        &mut tx,
        signal_id,
        SignalStatus::Proposed,
        actor,
        serde_json::json!({"goal_id": goal_id}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn insert_signal_event_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    signal_id: &str,
    status: SignalStatus,
    actor: &str,
    details: serde_json::Value,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO harness_signal_events (signal_id, status, actor, details_json)
         VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(signal_id)
    .bind(status.as_str())
    .bind(actor)
    .bind(details.to_string())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn decode_signal(row: sqlx::sqlite::SqliteRow) -> anyhow::Result<SignalRecord> {
    let metadata_json: String = row.try_get("metadata_json")?;
    Ok(SignalRecord {
        id: row.try_get("id")?,
        kind: SignalKind::from_db(row.try_get("kind")?)?,
        trust: SignalTrust::from_db(row.try_get("trust")?)?,
        status: SignalStatus::from_db(row.try_get("status")?)?,
        source: row.try_get("source")?,
        external_id: row.try_get("external_id")?,
        content: row.try_get("content")?,
        content_hash: row.try_get("content_hash")?,
        metadata: serde_json::from_str(&metadata_json).context("invalid signal metadata")?,
        created_at_unix: row.try_get("created_at")?,
    })
}
