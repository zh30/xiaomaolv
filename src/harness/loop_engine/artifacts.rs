use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use super::domain::hash_serializable;
use super::store::new_loop_id;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    GoalTemplate,
    DynamicWorkflow,
    EvalSuite,
    SkillManifest,
    ReplayCorpus,
    DesktopView,
    AnalysisReport,
    SelfTestReport,
    EvolutionEvaluation,
    PromptPolicyRef,
}

impl ArtifactKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::GoalTemplate => "goal_template",
            Self::DynamicWorkflow => "dynamic_workflow",
            Self::EvalSuite => "eval_suite",
            Self::SkillManifest => "skill_manifest",
            Self::ReplayCorpus => "replay_corpus",
            Self::DesktopView => "desktop_view",
            Self::AnalysisReport => "analysis_report",
            Self::SelfTestReport => "self_test_report",
            Self::EvolutionEvaluation => "evolution_evaluation",
            Self::PromptPolicyRef => "prompt_policy_ref",
        }
    }

    fn from_db(value: &str) -> anyhow::Result<Self> {
        match value {
            "goal_template" => Ok(Self::GoalTemplate),
            "dynamic_workflow" => Ok(Self::DynamicWorkflow),
            "eval_suite" => Ok(Self::EvalSuite),
            "skill_manifest" => Ok(Self::SkillManifest),
            "replay_corpus" => Ok(Self::ReplayCorpus),
            "desktop_view" => Ok(Self::DesktopView),
            "analysis_report" => Ok(Self::AnalysisReport),
            "self_test_report" => Ok(Self::SelfTestReport),
            "evolution_evaluation" => Ok(Self::EvolutionEvaluation),
            "prompt_policy_ref" => Ok(Self::PromptPolicyRef),
            other => bail!("unknown artifact kind: {other}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishArtifactRequest {
    pub kind: ArtifactKind,
    pub name: String,
    pub version: String,
    pub content: serde_json::Value,
    pub source_goal_id: Option<String>,
    pub parent_artifact_id: Option<String>,
}

impl PublishArtifactRequest {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            !self.name.trim().is_empty() && self.name.len() <= 160,
            "artifact name must be 1..=160 bytes"
        );
        ensure!(
            !self.version.trim().is_empty() && self.version.len() <= 80,
            "artifact version must be 1..=80 bytes"
        );
        ensure!(
            serde_json::to_vec(&self.content)?.len() <= 32_768,
            "artifact content cannot exceed 32768 bytes"
        );
        for (value, label) in [
            (self.source_goal_id.as_deref(), "source goal id"),
            (self.parent_artifact_id.as_deref(), "parent artifact id"),
        ] {
            if let Some(value) = value {
                ensure!(
                    !value.trim().is_empty() && value.len() <= 160,
                    "{label} must be 1..=160 bytes"
                );
            }
        }
        if self.kind == ArtifactKind::PromptPolicyRef {
            let object = self
                .content
                .as_object()
                .context("prompt_policy_ref content must be an object")?;
            ensure!(
                object
                    .keys()
                    .all(|key| matches!(key.as_str(), "evolution_candidate_id" | "deployment_id")),
                "prompt_policy_ref may only contain evolution lifecycle references"
            );
            let candidate_id = object
                .get("evolution_candidate_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            ensure!(
                !candidate_id.trim().is_empty() && candidate_id.len() <= 160,
                "prompt_policy_ref requires evolution_candidate_id"
            );
            if let Some(deployment_id) = object
                .get("deployment_id")
                .and_then(serde_json::Value::as_str)
            {
                ensure!(
                    !deployment_id.trim().is_empty() && deployment_id.len() <= 160,
                    "deployment_id must be 1..=160 bytes"
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub id: String,
    pub kind: ArtifactKind,
    pub name: String,
    pub version: String,
    pub content: serde_json::Value,
    pub content_hash: String,
    pub source_goal_id: Option<String>,
    pub parent_artifact_id: Option<String>,
    pub created_by: String,
    pub created_at_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactPublishResult {
    pub artifact: ArtifactRecord,
    pub deduplicated: bool,
}

pub(crate) async fn initialize_artifact_schema(pool: &SqlitePool) -> anyhow::Result<()> {
    for statement in [
        "CREATE TABLE IF NOT EXISTS harness_artifacts (
            id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            name TEXT NOT NULL,
            version TEXT NOT NULL,
            content_json TEXT NOT NULL,
            content_hash TEXT NOT NULL,
            source_goal_id TEXT,
            parent_artifact_id TEXT,
            created_by TEXT NOT NULL,
            created_at INTEGER NOT NULL DEFAULT (unixepoch()),
            UNIQUE(kind, name, version)
        )",
        "CREATE INDEX IF NOT EXISTS idx_harness_artifacts_kind_created
         ON harness_artifacts(kind, created_at DESC, id)",
        "CREATE TABLE IF NOT EXISTS harness_artifact_events (
            sequence INTEGER PRIMARY KEY AUTOINCREMENT,
            artifact_id TEXT NOT NULL REFERENCES harness_artifacts(id),
            event_type TEXT NOT NULL,
            actor TEXT NOT NULL,
            details_json TEXT NOT NULL,
            created_at INTEGER NOT NULL DEFAULT (unixepoch())
        )",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .context("failed to initialize artifact registry schema")?;
    }
    Ok(())
}

pub(crate) async fn publish_artifact(
    pool: &SqlitePool,
    request: PublishArtifactRequest,
    actor: &str,
) -> anyhow::Result<ArtifactPublishResult> {
    let content_hash = hash_serializable(&request.content)?;
    let mut tx = pool.begin().await?;
    if let Some(source_goal_id) = &request.source_goal_id {
        let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM harness_goals WHERE id = ?1")
            .bind(source_goal_id)
            .fetch_one(&mut *tx)
            .await?;
        ensure!(exists == 1, "source goal not found");
    }
    if let Some(parent_id) = &request.parent_artifact_id {
        let exists: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM harness_artifacts WHERE id = ?1")
                .bind(parent_id)
                .fetch_one(&mut *tx)
                .await?;
        ensure!(exists == 1, "parent artifact not found");
    }
    if let Some(row) = artifact_identity_query(&mut tx, &request).await? {
        let existing = decode_artifact(row)?;
        ensure!(
            existing.content_hash == content_hash,
            "artifact version already exists with different content"
        );
        tx.commit().await?;
        return Ok(ArtifactPublishResult {
            artifact: existing,
            deduplicated: true,
        });
    }
    let id = new_loop_id("artifact");
    sqlx::query(
        "INSERT INTO harness_artifacts
         (id, kind, name, version, content_json, content_hash, source_goal_id,
          parent_artifact_id, created_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(&id)
    .bind(request.kind.as_str())
    .bind(request.name.trim())
    .bind(request.version.trim())
    .bind(serde_json::to_string(&request.content)?)
    .bind(&content_hash)
    .bind(&request.source_goal_id)
    .bind(&request.parent_artifact_id)
    .bind(actor)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO harness_artifact_events
         (artifact_id, event_type, actor, details_json)
         VALUES (?1, 'artifact.published', ?2, ?3)",
    )
    .bind(&id)
    .bind(actor)
    .bind(serde_json::json!({"content_hash": content_hash}).to_string())
    .execute(&mut *tx)
    .await?;
    let artifact = load_artifact_tx(&mut tx, &id).await?;
    tx.commit().await?;
    Ok(ArtifactPublishResult {
        artifact,
        deduplicated: false,
    })
}

pub(crate) async fn get_artifact(
    pool: &SqlitePool,
    artifact_id: &str,
) -> anyhow::Result<Option<ArtifactRecord>> {
    let row = sqlx::query(
        "SELECT id, kind, name, version, content_json, content_hash, source_goal_id,
                parent_artifact_id, created_by, created_at
         FROM harness_artifacts WHERE id = ?1",
    )
    .bind(artifact_id)
    .fetch_optional(pool)
    .await?;
    row.map(decode_artifact).transpose()
}

pub(crate) async fn list_artifacts(
    pool: &SqlitePool,
    limit: usize,
) -> anyhow::Result<Vec<ArtifactRecord>> {
    let limit = limit.clamp(1, 500);
    sqlx::query(
        "SELECT id, kind, name, version, content_json, content_hash, source_goal_id,
                parent_artifact_id, created_by, created_at
         FROM harness_artifacts ORDER BY created_at DESC, id DESC LIMIT ?1",
    )
    .bind(i64::try_from(limit).context("artifact limit exceeds sqlite range")?)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(decode_artifact)
    .collect()
}

pub(crate) async fn find_artifact(
    pool: &SqlitePool,
    kind: ArtifactKind,
    name: &str,
    version: &str,
) -> anyhow::Result<Option<ArtifactRecord>> {
    let row = sqlx::query(
        "SELECT id, kind, name, version, content_json, content_hash, source_goal_id,
                parent_artifact_id, created_by, created_at
         FROM harness_artifacts WHERE kind = ?1 AND name = ?2 AND version = ?3",
    )
    .bind(kind.as_str())
    .bind(name)
    .bind(version)
    .fetch_optional(pool)
    .await?;
    row.map(decode_artifact).transpose()
}

async fn artifact_identity_query(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    request: &PublishArtifactRequest,
) -> anyhow::Result<Option<sqlx::sqlite::SqliteRow>> {
    Ok(sqlx::query(
        "SELECT id, kind, name, version, content_json, content_hash, source_goal_id,
                parent_artifact_id, created_by, created_at
         FROM harness_artifacts WHERE kind = ?1 AND name = ?2 AND version = ?3",
    )
    .bind(request.kind.as_str())
    .bind(request.name.trim())
    .bind(request.version.trim())
    .fetch_optional(&mut **tx)
    .await?)
}

async fn load_artifact_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    artifact_id: &str,
) -> anyhow::Result<ArtifactRecord> {
    let row = sqlx::query(
        "SELECT id, kind, name, version, content_json, content_hash, source_goal_id,
                parent_artifact_id, created_by, created_at
         FROM harness_artifacts WHERE id = ?1",
    )
    .bind(artifact_id)
    .fetch_one(&mut **tx)
    .await?;
    decode_artifact(row)
}

fn decode_artifact(row: sqlx::sqlite::SqliteRow) -> anyhow::Result<ArtifactRecord> {
    let content_json: String = row.try_get("content_json")?;
    Ok(ArtifactRecord {
        id: row.try_get("id")?,
        kind: ArtifactKind::from_db(row.try_get("kind")?)?,
        name: row.try_get("name")?,
        version: row.try_get("version")?,
        content: serde_json::from_str(&content_json).context("invalid artifact content")?,
        content_hash: row.try_get("content_hash")?,
        source_goal_id: row.try_get("source_goal_id")?,
        parent_artifact_id: row.try_get("parent_artifact_id")?,
        created_by: row.try_get("created_by")?,
        created_at_unix: row.try_get("created_at")?,
    })
}
