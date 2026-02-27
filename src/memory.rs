use std::collections::{HashMap, HashSet, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use async_trait::async_trait;
use reqwest::Client;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
use sqlx::{Row, SqlitePool};
use tracing::warn;

use crate::domain::{MessageRole, StoredMessage};

#[derive(Clone)]
pub struct SqliteMemoryStore {
    pool: SqlitePool,
}

impl SqliteMemoryStore {
    pub async fn new(database_url: &str) -> anyhow::Result<Self> {
        let mut options = SqliteConnectOptions::from_str(database_url)
            .with_context(|| format!("failed to parse sqlite database URL: {database_url}"))?
            .create_if_missing(true)
            .busy_timeout(Duration::from_secs(5));

        if !is_in_memory_sqlite_url(database_url) {
            options = options
                .journal_mode(SqliteJournalMode::Wal)
                .synchronous(SqliteSynchronous::Normal);
        }

        let pool = SqlitePool::connect_with(options)
            .await
            .with_context(|| format!("failed to connect sqlite database: {database_url}"))?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at INTEGER NOT NULL DEFAULT (unixepoch())
            );",
        )
        .execute(&pool)
        .await
        .context("failed to initialize messages table")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_messages_session_id_id_desc
             ON messages(session_id, id DESC);",
        )
        .execute(&pool)
        .await
        .context("failed to initialize messages session index")?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS memory_chunks (
                chunk_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                channel TEXT NOT NULL,
                role TEXT NOT NULL,
                text TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );",
        )
        .execute(&pool)
        .await
        .context("failed to initialize memory_chunks table")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_memory_chunks_session_created
             ON memory_chunks(session_id, created_at DESC);",
        )
        .execute(&pool)
        .await
        .context("failed to initialize memory_chunks session index")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_memory_chunks_scope_created
             ON memory_chunks(user_id, channel, created_at DESC);",
        )
        .execute(&pool)
        .await
        .context("failed to initialize memory_chunks scope index")?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS telegram_group_aliases (
                channel TEXT NOT NULL,
                chat_id INTEGER NOT NULL,
                alias TEXT NOT NULL,
                learned_at INTEGER NOT NULL DEFAULT (unixepoch()),
                PRIMARY KEY (channel, chat_id, alias)
            );",
        )
        .execute(&pool)
        .await
        .context("failed to initialize telegram_group_aliases table")?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS telegram_group_user_profiles (
                channel TEXT NOT NULL,
                chat_id INTEGER NOT NULL,
                user_id INTEGER NOT NULL,
                preferred_name TEXT NOT NULL,
                username TEXT,
                updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
                PRIMARY KEY (channel, chat_id, user_id)
            );",
        )
        .execute(&pool)
        .await
        .context("failed to initialize telegram_group_user_profiles table")?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS telegram_scheduler_jobs (
                job_id TEXT PRIMARY KEY,
                channel TEXT NOT NULL,
                chat_id INTEGER NOT NULL,
                message_thread_id INTEGER,
                owner_user_id INTEGER NOT NULL,
                status TEXT NOT NULL,
                task_kind TEXT NOT NULL,
                payload TEXT NOT NULL,
                schedule_kind TEXT NOT NULL,
                timezone TEXT NOT NULL,
                run_at_unix INTEGER,
                cron_expr TEXT,
                policy_json TEXT,
                next_run_at_unix INTEGER,
                last_run_at_unix INTEGER,
                last_error TEXT,
                run_count INTEGER NOT NULL DEFAULT 0,
                failure_streak INTEGER NOT NULL DEFAULT 0,
                max_runs INTEGER,
                lease_token TEXT,
                lease_until_unix INTEGER,
                created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                updated_at INTEGER NOT NULL DEFAULT (unixepoch())
            );",
        )
        .execute(&pool)
        .await
        .context("failed to initialize telegram_scheduler_jobs table")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_scheduler_jobs_due
             ON telegram_scheduler_jobs(channel, status, next_run_at_unix, lease_until_unix);",
        )
        .execute(&pool)
        .await
        .context("failed to initialize telegram_scheduler_jobs due index")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_scheduler_jobs_owner
             ON telegram_scheduler_jobs(channel, chat_id, owner_user_id, created_at);",
        )
        .execute(&pool)
        .await
        .context("failed to initialize telegram_scheduler_jobs owner index")?;

        maybe_add_scheduler_jobs_column(&pool, "policy_json TEXT")
            .await
            .context("failed to add telegram_scheduler_jobs.policy_json")?;
        maybe_add_scheduler_jobs_column(&pool, "failure_streak INTEGER NOT NULL DEFAULT 0")
            .await
            .context("failed to add telegram_scheduler_jobs.failure_streak")?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS telegram_scheduler_runs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                job_id TEXT NOT NULL,
                channel TEXT NOT NULL,
                chat_id INTEGER NOT NULL,
                started_at_unix INTEGER NOT NULL,
                finished_at_unix INTEGER NOT NULL,
                ok INTEGER NOT NULL,
                error TEXT
            );",
        )
        .execute(&pool)
        .await
        .context("failed to initialize telegram_scheduler_runs table")?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS telegram_scheduler_pending_intents (
                intent_id TEXT PRIMARY KEY,
                channel TEXT NOT NULL,
                chat_id INTEGER NOT NULL,
                owner_user_id INTEGER NOT NULL,
                draft_json TEXT NOT NULL,
                expires_at_unix INTEGER NOT NULL,
                created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                updated_at INTEGER NOT NULL DEFAULT (unixepoch())
            );",
        )
        .execute(&pool)
        .await
        .context("failed to initialize telegram_scheduler_pending_intents table")?;

        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_scheduler_pending_owner
             ON telegram_scheduler_pending_intents(channel, chat_id, owner_user_id);",
        )
        .execute(&pool)
        .await
        .context("failed to initialize telegram_scheduler_pending_intents owner index")?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS agent_swarm_runs (
                run_id TEXT PRIMARY KEY,
                channel TEXT NOT NULL,
                session_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                root_task TEXT NOT NULL,
                status TEXT NOT NULL,
                started_at_unix INTEGER NOT NULL,
                finished_at_unix INTEGER,
                error TEXT,
                created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                updated_at INTEGER NOT NULL DEFAULT (unixepoch())
            );",
        )
        .execute(&pool)
        .await
        .context("failed to initialize agent_swarm_runs table")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_agent_swarm_runs_channel_user_started
             ON agent_swarm_runs(channel, user_id, started_at_unix DESC);",
        )
        .execute(&pool)
        .await
        .context("failed to initialize agent_swarm_runs user index")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_agent_swarm_runs_channel_started
             ON agent_swarm_runs(channel, started_at_unix DESC);",
        )
        .execute(&pool)
        .await
        .context("failed to initialize agent_swarm_runs started index")?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS agent_swarm_nodes (
                agent_id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL,
                channel TEXT NOT NULL,
                parent_agent_id TEXT,
                depth INTEGER NOT NULL,
                nickname TEXT NOT NULL,
                role_name TEXT NOT NULL,
                role_definition TEXT NOT NULL,
                task TEXT NOT NULL,
                state TEXT NOT NULL,
                exit_status TEXT,
                summary TEXT,
                error TEXT,
                started_at_unix INTEGER NOT NULL,
                finished_at_unix INTEGER,
                created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                updated_at INTEGER NOT NULL DEFAULT (unixepoch())
            );",
        )
        .execute(&pool)
        .await
        .context("failed to initialize agent_swarm_nodes table")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_agent_swarm_nodes_channel_run
             ON agent_swarm_nodes(channel, run_id, depth, started_at_unix);",
        )
        .execute(&pool)
        .await
        .context("failed to initialize agent_swarm_nodes run index")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_agent_swarm_nodes_channel_started
             ON agent_swarm_nodes(channel, started_at_unix);",
        )
        .execute(&pool)
        .await
        .context("failed to initialize agent_swarm_nodes started index")?;

        Ok(Self { pool })
    }

    pub async fn append(&self, session_id: &str, message: StoredMessage) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO messages (session_id, role, content) VALUES (?1, ?2, ?3)")
            .bind(session_id)
            .bind(message.role.as_str())
            .bind(message.content)
            .execute(&self.pool)
            .await
            .context("failed to insert message")?;
        Ok(())
    }

    pub async fn load_recent(
        &self,
        session_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<StoredMessage>> {
        let mut rows = sqlx::query(
            "SELECT role, content FROM messages
             WHERE session_id = ?1
             ORDER BY id DESC
             LIMIT ?2",
        )
        .bind(session_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("failed to load recent messages")?
        .into_iter()
        .map(|row| {
            let role: String = row.get("role");
            let content: String = row.get("content");
            StoredMessage {
                role: MessageRole::from_db(&role),
                content,
            }
        })
        .collect::<Vec<_>>();

        rows.reverse();
        Ok(rows)
    }

    pub async fn append_chunk(&self, chunk: MemoryChunkRecord) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO memory_chunks
             (chunk_id, session_id, user_id, channel, role, text, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(chunk.chunk_id)
        .bind(chunk.session_id)
        .bind(chunk.user_id)
        .bind(chunk.channel)
        .bind(chunk.role.as_str())
        .bind(chunk.text)
        .bind(chunk.created_at)
        .execute(&self.pool)
        .await
        .context("failed to upsert memory chunk")?;
        Ok(())
    }

    pub async fn load_chunk_candidates(
        &self,
        user_id: &str,
        channel: &str,
        min_created_at: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MemoryChunkCandidateRecord>> {
        let rows = sqlx::query(
            "SELECT role, text, created_at
             FROM memory_chunks
             WHERE user_id = ?1
               AND channel = ?2
               AND created_at >= ?3
             ORDER BY created_at DESC
             LIMIT ?4",
        )
        .bind(user_id)
        .bind(channel)
        .bind(min_created_at.max(0))
        .bind(limit.max(1) as i64)
        .fetch_all(&self.pool)
        .await
        .context("failed to load memory chunk candidates")?;

        Ok(rows
            .into_iter()
            .map(|row| {
                let role: String = row.get("role");
                MemoryChunkCandidateRecord {
                    role: MessageRole::from_db(&role),
                    text: row.get("text"),
                    created_at: row.get("created_at"),
                }
            })
            .collect())
    }

    pub async fn upsert_group_aliases(
        &self,
        channel: &str,
        chat_id: i64,
        aliases: &[String],
    ) -> anyhow::Result<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .context("failed to begin telegram group alias tx")?;

        for alias in aliases {
            let normalized = alias.trim();
            if normalized.is_empty() {
                continue;
            }
            sqlx::query(
                "INSERT OR IGNORE INTO telegram_group_aliases
                 (channel, chat_id, alias, learned_at)
                 VALUES (?1, ?2, ?3, unixepoch())",
            )
            .bind(channel)
            .bind(chat_id)
            .bind(normalized)
            .execute(&mut *tx)
            .await
            .context("failed to upsert telegram group alias")?;
        }

        tx.commit()
            .await
            .context("failed to commit telegram group alias tx")?;
        Ok(())
    }

    pub async fn load_group_aliases(
        &self,
        channel: &str,
        chat_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT alias FROM telegram_group_aliases
             WHERE channel = ?1 AND chat_id = ?2
             ORDER BY learned_at DESC
             LIMIT ?3",
        )
        .bind(channel)
        .bind(chat_id)
        .bind(limit.max(1) as i64)
        .fetch_all(&self.pool)
        .await
        .context("failed to load telegram group aliases")?;

        Ok(rows
            .into_iter()
            .map(|row| row.get::<String, _>("alias"))
            .collect())
    }

    pub async fn upsert_group_user_profile(
        &self,
        channel: &str,
        chat_id: i64,
        user_id: i64,
        preferred_name: &str,
        username: Option<&str>,
    ) -> anyhow::Result<()> {
        let normalized_name = preferred_name.trim();
        if normalized_name.is_empty() {
            return Ok(());
        }
        let normalized_username = username
            .map(|v| v.trim().trim_start_matches('@'))
            .filter(|v| !v.is_empty());

        sqlx::query(
            "INSERT INTO telegram_group_user_profiles
             (channel, chat_id, user_id, preferred_name, username, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, unixepoch())
             ON CONFLICT(channel, chat_id, user_id) DO UPDATE SET
               preferred_name=excluded.preferred_name,
               username=COALESCE(excluded.username, telegram_group_user_profiles.username),
               updated_at=unixepoch()",
        )
        .bind(channel)
        .bind(chat_id)
        .bind(user_id)
        .bind(normalized_name)
        .bind(normalized_username)
        .execute(&self.pool)
        .await
        .context("failed to upsert telegram group user profile")?;

        Ok(())
    }

    pub async fn load_group_user_profiles(
        &self,
        channel: &str,
        chat_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<GroupUserProfileRecord>> {
        let rows = sqlx::query(
            "SELECT user_id, preferred_name, username, updated_at
             FROM telegram_group_user_profiles
             WHERE channel = ?1 AND chat_id = ?2
             ORDER BY updated_at DESC
             LIMIT ?3",
        )
        .bind(channel)
        .bind(chat_id)
        .bind(limit.max(1) as i64)
        .fetch_all(&self.pool)
        .await
        .context("failed to load telegram group user profiles")?;

        Ok(rows
            .into_iter()
            .map(|row| GroupUserProfileRecord {
                user_id: row.get("user_id"),
                preferred_name: row.get("preferred_name"),
                username: row.get("username"),
                updated_at: row.get("updated_at"),
            })
            .collect())
    }

    pub async fn create_telegram_scheduler_job(
        &self,
        req: CreateTelegramSchedulerJobRequest,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO telegram_scheduler_jobs
             (job_id, channel, chat_id, message_thread_id, owner_user_id, status, task_kind,
              payload, schedule_kind, timezone, run_at_unix, cron_expr, policy_json, next_run_at_unix, max_runs,
              run_count, failure_streak, lease_token, lease_until_unix, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, 0, 0, NULL, NULL, unixepoch(), unixepoch())",
        )
        .bind(req.job_id)
        .bind(req.channel)
        .bind(req.chat_id)
        .bind(req.message_thread_id)
        .bind(req.owner_user_id)
        .bind(req.status.as_str())
        .bind(req.task_kind.as_str())
        .bind(req.payload)
        .bind(req.schedule_kind.as_str())
        .bind(req.timezone)
        .bind(req.run_at_unix)
        .bind(req.cron_expr)
        .bind(req.policy_json)
        .bind(req.next_run_at_unix)
        .bind(req.max_runs)
        .execute(&self.pool)
        .await
        .context("failed to create telegram scheduler job")?;
        Ok(())
    }

    pub async fn list_telegram_scheduler_jobs_by_owner(
        &self,
        channel: &str,
        chat_id: i64,
        owner_user_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<TelegramSchedulerJobRecord>> {
        let rows = sqlx::query(
            "SELECT job_id, channel, chat_id, message_thread_id, owner_user_id, status, task_kind,
                    payload, schedule_kind, timezone, run_at_unix, cron_expr, policy_json, next_run_at_unix,
                    last_run_at_unix, last_error, run_count, failure_streak, max_runs, lease_token,
                    lease_until_unix, created_at, updated_at
             FROM telegram_scheduler_jobs
             WHERE channel = ?1 AND chat_id = ?2 AND owner_user_id = ?3
             ORDER BY created_at DESC
             LIMIT ?4",
        )
        .bind(channel)
        .bind(chat_id)
        .bind(owner_user_id)
        .bind(limit.max(1) as i64)
        .fetch_all(&self.pool)
        .await
        .context("failed to list telegram scheduler jobs by owner")?;

        rows.into_iter()
            .map(telegram_scheduler_job_from_row)
            .collect()
    }

    pub async fn query_telegram_scheduler_stats(
        &self,
        channel: &str,
        now_unix: i64,
    ) -> anyhow::Result<TelegramSchedulerStats> {
        let row = sqlx::query(
            "SELECT
                COUNT(*) AS jobs_total,
                SUM(CASE WHEN status = 'active' THEN 1 ELSE 0 END) AS jobs_active,
                SUM(CASE WHEN status = 'paused' THEN 1 ELSE 0 END) AS jobs_paused,
                SUM(CASE WHEN status = 'completed' THEN 1 ELSE 0 END) AS jobs_completed,
                SUM(CASE WHEN status = 'canceled' THEN 1 ELSE 0 END) AS jobs_canceled,
                SUM(CASE
                    WHEN status = 'active'
                     AND next_run_at_unix IS NOT NULL
                     AND next_run_at_unix <= ?2
                     AND (lease_until_unix IS NULL OR lease_until_unix <= ?2)
                    THEN 1 ELSE 0 END
                ) AS jobs_due
             FROM telegram_scheduler_jobs
             WHERE channel = ?1",
        )
        .bind(channel)
        .bind(now_unix)
        .fetch_one(&self.pool)
        .await
        .context("failed to query telegram scheduler stats")?;

        Ok(TelegramSchedulerStats {
            jobs_total: row.get::<i64, _>("jobs_total"),
            jobs_active: row.get::<Option<i64>, _>("jobs_active").unwrap_or(0),
            jobs_paused: row.get::<Option<i64>, _>("jobs_paused").unwrap_or(0),
            jobs_completed: row.get::<Option<i64>, _>("jobs_completed").unwrap_or(0),
            jobs_canceled: row.get::<Option<i64>, _>("jobs_canceled").unwrap_or(0),
            jobs_due: row.get::<Option<i64>, _>("jobs_due").unwrap_or(0),
        })
    }

    pub async fn load_telegram_scheduler_job(
        &self,
        channel: &str,
        job_id: &str,
    ) -> anyhow::Result<Option<TelegramSchedulerJobRecord>> {
        let row = sqlx::query(
            "SELECT job_id, channel, chat_id, message_thread_id, owner_user_id, status, task_kind,
                    payload, schedule_kind, timezone, run_at_unix, cron_expr, policy_json, next_run_at_unix,
                    last_run_at_unix, last_error, run_count, failure_streak, max_runs, lease_token,
                    lease_until_unix, created_at, updated_at
             FROM telegram_scheduler_jobs
             WHERE channel = ?1 AND job_id = ?2
             LIMIT 1",
        )
        .bind(channel)
        .bind(job_id)
        .fetch_optional(&self.pool)
        .await
        .context("failed to load telegram scheduler job")?;

        row.map(telegram_scheduler_job_from_row).transpose()
    }

    pub async fn update_telegram_scheduler_job_status(
        &self,
        channel: &str,
        chat_id: i64,
        owner_user_id: i64,
        job_id: &str,
        status: TelegramSchedulerJobStatus,
    ) -> anyhow::Result<bool> {
        let updated = sqlx::query(
            "UPDATE telegram_scheduler_jobs
             SET status = ?1,
                 lease_token = NULL,
                 lease_until_unix = NULL,
                 updated_at = unixepoch()
             WHERE channel = ?2
               AND chat_id = ?3
               AND owner_user_id = ?4
               AND job_id = ?5
               AND status NOT IN ('completed', 'canceled')",
        )
        .bind(status.as_str())
        .bind(channel)
        .bind(chat_id)
        .bind(owner_user_id)
        .bind(job_id)
        .execute(&self.pool)
        .await
        .context("failed to update telegram scheduler job status")?;

        Ok(updated.rows_affected() > 0)
    }

    pub async fn claim_due_telegram_scheduler_jobs(
        &self,
        channel: &str,
        now_unix: i64,
        limit: usize,
        lease_secs: i64,
        lease_token: &str,
    ) -> anyhow::Result<Vec<TelegramSchedulerJobRecord>> {
        let mut tx = self
            .pool
            .begin()
            .await
            .context("failed to begin scheduler tx")?;
        let rows = sqlx::query(
            "SELECT job_id, channel, chat_id, message_thread_id, owner_user_id, status, task_kind,
                    payload, schedule_kind, timezone, run_at_unix, cron_expr, policy_json, next_run_at_unix,
                    last_run_at_unix, last_error, run_count, failure_streak, max_runs, lease_token,
                    lease_until_unix, created_at, updated_at
             FROM telegram_scheduler_jobs
             WHERE channel = ?1
               AND status = 'active'
               AND next_run_at_unix IS NOT NULL
               AND next_run_at_unix <= ?2
               AND (lease_until_unix IS NULL OR lease_until_unix <= ?2)
             ORDER BY next_run_at_unix ASC
             LIMIT ?3",
        )
        .bind(channel)
        .bind(now_unix)
        .bind(limit.max(1) as i64)
        .fetch_all(&mut *tx)
        .await
        .context("failed to query due telegram scheduler jobs")?;

        let lease_until_unix = now_unix + lease_secs.max(1);
        let mut claimed = Vec::new();
        for row in rows {
            let job = telegram_scheduler_job_from_row(row)
                .context("failed to decode due telegram scheduler job")?;
            let updated = sqlx::query(
                "UPDATE telegram_scheduler_jobs
                 SET lease_token = ?1,
                     lease_until_unix = ?2,
                     updated_at = unixepoch()
                 WHERE channel = ?3
                   AND job_id = ?4
                   AND status = 'active'
                   AND (lease_until_unix IS NULL OR lease_until_unix <= ?5)",
            )
            .bind(lease_token)
            .bind(lease_until_unix)
            .bind(channel)
            .bind(&job.job_id)
            .bind(now_unix)
            .execute(&mut *tx)
            .await
            .context("failed to claim telegram scheduler job")?;

            if updated.rows_affected() == 0 {
                continue;
            }

            claimed.push(TelegramSchedulerJobRecord {
                lease_token: Some(lease_token.to_string()),
                lease_until_unix: Some(lease_until_unix),
                ..job
            });
        }

        tx.commit()
            .await
            .context("failed to commit scheduler claim tx")?;
        Ok(claimed)
    }

    pub async fn complete_telegram_scheduler_job_run(
        &self,
        req: CompleteTelegramSchedulerJobRunRequest,
    ) -> anyhow::Result<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .context("failed to begin scheduler complete tx")?;
        sqlx::query(
            "INSERT INTO telegram_scheduler_runs
             (job_id, channel, chat_id, started_at_unix, finished_at_unix, ok, error)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, NULL)",
        )
        .bind(&req.job_id)
        .bind(&req.channel)
        .bind(req.chat_id)
        .bind(req.started_at_unix)
        .bind(req.finished_at_unix)
        .execute(&mut *tx)
        .await
        .context("failed to insert scheduler success run")?;

        let next_status = if req.mark_completed {
            TelegramSchedulerJobStatus::Completed
        } else {
            TelegramSchedulerJobStatus::Active
        };
        let updated = sqlx::query(
            "UPDATE telegram_scheduler_jobs
             SET status = ?1,
                 next_run_at_unix = ?2,
                 last_run_at_unix = ?3,
                 last_error = NULL,
                 run_count = run_count + 1,
                 failure_streak = 0,
                 lease_token = NULL,
                 lease_until_unix = NULL,
                 updated_at = unixepoch()
             WHERE channel = ?4
               AND chat_id = ?5
               AND job_id = ?6
               AND lease_token = ?7",
        )
        .bind(next_status.as_str())
        .bind(req.next_run_at_unix)
        .bind(req.finished_at_unix)
        .bind(&req.channel)
        .bind(req.chat_id)
        .bind(&req.job_id)
        .bind(&req.lease_token)
        .execute(&mut *tx)
        .await
        .context("failed to update scheduler job after success")?;
        if updated.rows_affected() == 0 {
            bail!("scheduler job completion rejected due to missing lease");
        }

        tx.commit()
            .await
            .context("failed to commit scheduler complete tx")?;
        Ok(())
    }

    pub async fn fail_telegram_scheduler_job_run(
        &self,
        req: FailTelegramSchedulerJobRunRequest,
    ) -> anyhow::Result<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .context("failed to begin scheduler fail tx")?;
        sqlx::query(
            "INSERT INTO telegram_scheduler_runs
             (job_id, channel, chat_id, started_at_unix, finished_at_unix, ok, error)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)",
        )
        .bind(&req.job_id)
        .bind(&req.channel)
        .bind(req.chat_id)
        .bind(req.started_at_unix)
        .bind(req.finished_at_unix)
        .bind(&req.error)
        .execute(&mut *tx)
        .await
        .context("failed to insert scheduler failure run")?;

        let status = if req.pause_job {
            TelegramSchedulerJobStatus::Paused
        } else {
            TelegramSchedulerJobStatus::Active
        };
        let updated = sqlx::query(
            "UPDATE telegram_scheduler_jobs
             SET status = ?1,
                 next_run_at_unix = ?2,
                 last_run_at_unix = ?3,
                 last_error = ?4,
                 run_count = run_count + 1,
                 failure_streak = failure_streak + 1,
                 lease_token = NULL,
                 lease_until_unix = NULL,
                 updated_at = unixepoch()
             WHERE channel = ?5
               AND chat_id = ?6
               AND job_id = ?7
               AND lease_token = ?8",
        )
        .bind(status.as_str())
        .bind(req.next_run_at_unix)
        .bind(req.finished_at_unix)
        .bind(&req.error)
        .bind(&req.channel)
        .bind(req.chat_id)
        .bind(&req.job_id)
        .bind(&req.lease_token)
        .execute(&mut *tx)
        .await
        .context("failed to update scheduler job after failure")?;
        if updated.rows_affected() == 0 {
            bail!("scheduler job failure update rejected due to missing lease");
        }

        tx.commit()
            .await
            .context("failed to commit scheduler fail tx")?;
        Ok(())
    }

    pub async fn upsert_telegram_scheduler_pending_intent(
        &self,
        req: UpsertTelegramSchedulerPendingIntentRequest,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO telegram_scheduler_pending_intents
             (intent_id, channel, chat_id, owner_user_id, draft_json, expires_at_unix, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, unixepoch(), unixepoch())
             ON CONFLICT(channel, chat_id, owner_user_id) DO UPDATE SET
               intent_id=excluded.intent_id,
               draft_json=excluded.draft_json,
               expires_at_unix=excluded.expires_at_unix,
               updated_at=unixepoch()",
        )
        .bind(req.intent_id)
        .bind(req.channel)
        .bind(req.chat_id)
        .bind(req.owner_user_id)
        .bind(req.draft_json)
        .bind(req.expires_at_unix)
        .execute(&self.pool)
        .await
        .context("failed to upsert telegram scheduler pending intent")?;
        Ok(())
    }

    pub async fn load_telegram_scheduler_pending_intent(
        &self,
        channel: &str,
        chat_id: i64,
        owner_user_id: i64,
        now_unix: i64,
    ) -> anyhow::Result<Option<TelegramSchedulerPendingIntentRecord>> {
        let row = sqlx::query(
            "SELECT intent_id, channel, chat_id, owner_user_id, draft_json, expires_at_unix, created_at, updated_at
             FROM telegram_scheduler_pending_intents
             WHERE channel = ?1 AND chat_id = ?2 AND owner_user_id = ?3 AND expires_at_unix > ?4
             LIMIT 1",
        )
        .bind(channel)
        .bind(chat_id)
        .bind(owner_user_id)
        .bind(now_unix)
        .fetch_optional(&self.pool)
        .await
        .context("failed to load telegram scheduler pending intent")?;

        Ok(row.map(|row| TelegramSchedulerPendingIntentRecord {
            intent_id: row.get("intent_id"),
            channel: row.get("channel"),
            chat_id: row.get("chat_id"),
            owner_user_id: row.get("owner_user_id"),
            draft_json: row.get("draft_json"),
            expires_at_unix: row.get("expires_at_unix"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        }))
    }

    pub async fn delete_telegram_scheduler_pending_intent(
        &self,
        channel: &str,
        chat_id: i64,
        owner_user_id: i64,
    ) -> anyhow::Result<bool> {
        let deleted = sqlx::query(
            "DELETE FROM telegram_scheduler_pending_intents
             WHERE channel = ?1 AND chat_id = ?2 AND owner_user_id = ?3",
        )
        .bind(channel)
        .bind(chat_id)
        .bind(owner_user_id)
        .execute(&self.pool)
        .await
        .context("failed to delete telegram scheduler pending intent")?;

        Ok(deleted.rows_affected() > 0)
    }

    pub async fn create_agent_swarm_run(
        &self,
        req: CreateAgentSwarmRunRequest,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO agent_swarm_runs
             (run_id, channel, session_id, user_id, root_task, status, started_at_unix, finished_at_unix, error, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, unixepoch(), unixepoch())",
        )
        .bind(req.run_id)
        .bind(req.channel)
        .bind(req.session_id)
        .bind(req.user_id)
        .bind(req.root_task)
        .bind(req.status.as_str())
        .bind(req.started_at_unix)
        .execute(&self.pool)
        .await
        .context("failed to create agent swarm run")?;
        Ok(())
    }

    pub async fn finish_agent_swarm_run(
        &self,
        req: FinishAgentSwarmRunRequest,
    ) -> anyhow::Result<bool> {
        let updated = sqlx::query(
            "UPDATE agent_swarm_runs
             SET status = ?1,
                 finished_at_unix = ?2,
                 error = ?3,
                 updated_at = unixepoch()
             WHERE channel = ?4 AND run_id = ?5",
        )
        .bind(req.status.as_str())
        .bind(req.finished_at_unix)
        .bind(req.error)
        .bind(req.channel)
        .bind(req.run_id)
        .execute(&self.pool)
        .await
        .context("failed to finish agent swarm run")?;
        Ok(updated.rows_affected() > 0)
    }

    pub async fn upsert_agent_swarm_node(
        &self,
        req: UpsertAgentSwarmNodeRequest,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO agent_swarm_nodes
             (agent_id, run_id, channel, parent_agent_id, depth, nickname, role_name, role_definition,
              task, state, exit_status, summary, error, started_at_unix, finished_at_unix, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, unixepoch(), unixepoch())
             ON CONFLICT(agent_id) DO UPDATE SET
               run_id=excluded.run_id,
               channel=excluded.channel,
               parent_agent_id=excluded.parent_agent_id,
               depth=excluded.depth,
               nickname=excluded.nickname,
               role_name=excluded.role_name,
               role_definition=excluded.role_definition,
               task=excluded.task,
               state=excluded.state,
               exit_status=excluded.exit_status,
               summary=excluded.summary,
               error=excluded.error,
               started_at_unix=excluded.started_at_unix,
               finished_at_unix=excluded.finished_at_unix,
               updated_at=unixepoch()",
        )
        .bind(req.agent_id)
        .bind(req.run_id)
        .bind(req.channel)
        .bind(req.parent_agent_id)
        .bind(req.depth as i64)
        .bind(req.nickname)
        .bind(req.role_name)
        .bind(req.role_definition)
        .bind(req.task)
        .bind(req.state.as_str())
        .bind(req.exit_status.map(|v| v.as_str().to_string()))
        .bind(req.summary)
        .bind(req.error)
        .bind(req.started_at_unix)
        .bind(req.finished_at_unix)
        .execute(&self.pool)
        .await
        .context("failed to upsert agent swarm node")?;
        Ok(())
    }

    pub async fn list_agent_swarm_runs(
        &self,
        channel: &str,
        user_id: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<AgentSwarmRunRecord>> {
        let rows = sqlx::query(
            "SELECT r.run_id, r.channel, r.session_id, r.user_id, r.root_task, r.status,
                    r.started_at_unix, r.finished_at_unix, r.error, r.created_at, r.updated_at,
                    (SELECT COUNT(*)
                       FROM agent_swarm_nodes n
                      WHERE n.channel = r.channel AND n.run_id = r.run_id) AS agent_count
             FROM agent_swarm_runs r
             WHERE r.channel = ?1
               AND (?2 IS NULL OR r.user_id = ?2)
             ORDER BY r.started_at_unix DESC
             LIMIT ?3",
        )
        .bind(channel)
        .bind(user_id)
        .bind(limit.max(1) as i64)
        .fetch_all(&self.pool)
        .await
        .context("failed to list agent swarm runs")?;

        rows.into_iter().map(agent_swarm_run_from_row).collect()
    }

    pub async fn load_agent_swarm_nodes_by_run(
        &self,
        channel: &str,
        run_id: &str,
    ) -> anyhow::Result<Vec<AgentSwarmNodeRecord>> {
        let rows = sqlx::query(
            "SELECT agent_id, run_id, channel, parent_agent_id, depth, nickname, role_name, role_definition,
                    task, state, exit_status, summary, error, started_at_unix, finished_at_unix, created_at, updated_at
             FROM agent_swarm_nodes
             WHERE channel = ?1 AND run_id = ?2
             ORDER BY depth ASC, started_at_unix ASC, agent_id ASC",
        )
        .bind(channel)
        .bind(run_id)
        .fetch_all(&self.pool)
        .await
        .context("failed to load agent swarm nodes by run")?;

        rows.into_iter().map(agent_swarm_node_from_row).collect()
    }

    pub async fn load_agent_swarm_node(
        &self,
        channel: &str,
        agent_id: &str,
    ) -> anyhow::Result<Option<AgentSwarmNodeRecord>> {
        let row = sqlx::query(
            "SELECT agent_id, run_id, channel, parent_agent_id, depth, nickname, role_name, role_definition,
                    task, state, exit_status, summary, error, started_at_unix, finished_at_unix, created_at, updated_at
             FROM agent_swarm_nodes
             WHERE channel = ?1 AND agent_id = ?2
             LIMIT 1",
        )
        .bind(channel)
        .bind(agent_id)
        .fetch_optional(&self.pool)
        .await
        .context("failed to load agent swarm node")?;

        row.map(agent_swarm_node_from_row).transpose()
    }

    pub async fn cleanup_agent_swarm_audit(
        &self,
        channel: &str,
        before_unix: i64,
    ) -> anyhow::Result<i64> {
        let mut tx = self
            .pool
            .begin()
            .await
            .context("failed to begin agent swarm cleanup tx")?;

        sqlx::query(
            "DELETE FROM agent_swarm_nodes
             WHERE channel = ?1
               AND run_id IN (
                   SELECT run_id
                   FROM agent_swarm_runs
                   WHERE channel = ?1 AND started_at_unix < ?2
               )",
        )
        .bind(channel)
        .bind(before_unix)
        .execute(&mut *tx)
        .await
        .context("failed to delete agent swarm nodes in cleanup")?;

        let deleted = sqlx::query(
            "DELETE FROM agent_swarm_runs
             WHERE channel = ?1 AND started_at_unix < ?2",
        )
        .bind(channel)
        .bind(before_unix)
        .execute(&mut *tx)
        .await
        .context("failed to delete agent swarm runs in cleanup")?;

        tx.commit()
            .await
            .context("failed to commit agent swarm cleanup tx")?;
        Ok(deleted.rows_affected() as i64)
    }
}

async fn maybe_add_scheduler_jobs_column(
    pool: &SqlitePool,
    column_def: &str,
) -> anyhow::Result<()> {
    let sql = format!("ALTER TABLE telegram_scheduler_jobs ADD COLUMN {column_def}");
    match sqlx::query(&sql).execute(pool).await {
        Ok(_) => Ok(()),
        Err(err) => {
            let lower = err.to_string().to_ascii_lowercase();
            if lower.contains("duplicate column name") {
                Ok(())
            } else {
                Err(err).context("failed to alter telegram_scheduler_jobs table")
            }
        }
    }
}

fn is_in_memory_sqlite_url(database_url: &str) -> bool {
    database_url.contains(":memory:") || database_url.to_ascii_lowercase().contains("mode=memory")
}

#[derive(Debug, Clone)]
pub struct MemoryChunkRecord {
    pub chunk_id: String,
    pub session_id: String,
    pub user_id: String,
    pub channel: String,
    pub role: MessageRole,
    pub text: String,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct MemoryChunkCandidateRecord {
    pub role: MessageRole,
    pub text: String,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct MemoryWriteRequest {
    pub session_id: String,
    pub user_id: String,
    pub channel: String,
    pub message: StoredMessage,
}

#[derive(Debug, Clone)]
pub struct MemoryContextRequest {
    pub session_id: String,
    pub user_id: String,
    pub channel: String,
    pub query_text: String,
    pub max_recent_turns: usize,
    pub max_semantic_memories: usize,
    pub semantic_lookback_days: u32,
}

#[derive(Debug, Clone)]
pub struct GroupAliasUpsertRequest {
    pub channel: String,
    pub chat_id: i64,
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct GroupAliasLoadRequest {
    pub channel: String,
    pub chat_id: i64,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupUserProfileRecord {
    pub user_id: i64,
    pub preferred_name: String,
    pub username: Option<String>,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct GroupUserProfileUpsertRequest {
    pub channel: String,
    pub chat_id: i64,
    pub user_id: i64,
    pub preferred_name: String,
    pub username: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GroupUserProfileLoadRequest {
    pub channel: String,
    pub chat_id: i64,
    pub limit: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TelegramSchedulerJobStatus {
    Active,
    Paused,
    Completed,
    Canceled,
}

impl TelegramSchedulerJobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Canceled => "canceled",
        }
    }

    fn from_db(value: &str) -> Self {
        match value {
            "paused" => Self::Paused,
            "completed" => Self::Completed,
            "canceled" => Self::Canceled,
            _ => Self::Active,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TelegramSchedulerTaskKind {
    Reminder,
    Agent,
}

impl TelegramSchedulerTaskKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Reminder => "reminder",
            Self::Agent => "agent",
        }
    }

    fn from_db(value: &str) -> Self {
        match value {
            "agent" => Self::Agent,
            _ => Self::Reminder,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TelegramSchedulerScheduleKind {
    Once,
    Cron,
}

impl TelegramSchedulerScheduleKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Cron => "cron",
        }
    }

    fn from_db(value: &str) -> Self {
        match value {
            "cron" => Self::Cron,
            _ => Self::Once,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelegramSchedulerJobRecord {
    pub job_id: String,
    pub channel: String,
    pub chat_id: i64,
    pub message_thread_id: Option<i64>,
    pub owner_user_id: i64,
    pub status: TelegramSchedulerJobStatus,
    pub task_kind: TelegramSchedulerTaskKind,
    pub payload: String,
    pub schedule_kind: TelegramSchedulerScheduleKind,
    pub timezone: String,
    pub run_at_unix: Option<i64>,
    pub cron_expr: Option<String>,
    pub policy_json: Option<String>,
    pub next_run_at_unix: Option<i64>,
    pub last_run_at_unix: Option<i64>,
    pub last_error: Option<String>,
    pub run_count: i64,
    pub failure_streak: i64,
    pub max_runs: Option<i64>,
    pub lease_token: Option<String>,
    pub lease_until_unix: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct CreateTelegramSchedulerJobRequest {
    pub job_id: String,
    pub channel: String,
    pub chat_id: i64,
    pub message_thread_id: Option<i64>,
    pub owner_user_id: i64,
    pub status: TelegramSchedulerJobStatus,
    pub task_kind: TelegramSchedulerTaskKind,
    pub payload: String,
    pub schedule_kind: TelegramSchedulerScheduleKind,
    pub timezone: String,
    pub run_at_unix: Option<i64>,
    pub cron_expr: Option<String>,
    pub policy_json: Option<String>,
    pub next_run_at_unix: Option<i64>,
    pub max_runs: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct TelegramSchedulerJobListRequest {
    pub channel: String,
    pub chat_id: i64,
    pub owner_user_id: i64,
    pub limit: usize,
}

#[derive(Debug, Clone)]
pub struct TelegramSchedulerStatsRequest {
    pub channel: String,
    pub now_unix: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelegramSchedulerStats {
    pub jobs_total: i64,
    pub jobs_active: i64,
    pub jobs_paused: i64,
    pub jobs_completed: i64,
    pub jobs_canceled: i64,
    pub jobs_due: i64,
}

#[derive(Debug, Clone)]
pub struct UpdateTelegramSchedulerJobStatusRequest {
    pub channel: String,
    pub chat_id: i64,
    pub owner_user_id: i64,
    pub job_id: String,
    pub status: TelegramSchedulerJobStatus,
}

#[derive(Debug, Clone)]
pub struct ClaimDueTelegramSchedulerJobsRequest {
    pub channel: String,
    pub now_unix: i64,
    pub limit: usize,
    pub lease_secs: i64,
    pub lease_token: String,
}

#[derive(Debug, Clone)]
pub struct CompleteTelegramSchedulerJobRunRequest {
    pub channel: String,
    pub chat_id: i64,
    pub job_id: String,
    pub lease_token: String,
    pub started_at_unix: i64,
    pub finished_at_unix: i64,
    pub next_run_at_unix: Option<i64>,
    pub mark_completed: bool,
}

#[derive(Debug, Clone)]
pub struct FailTelegramSchedulerJobRunRequest {
    pub channel: String,
    pub chat_id: i64,
    pub job_id: String,
    pub lease_token: String,
    pub started_at_unix: i64,
    pub finished_at_unix: i64,
    pub next_run_at_unix: Option<i64>,
    pub pause_job: bool,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelegramSchedulerPendingIntentRecord {
    pub intent_id: String,
    pub channel: String,
    pub chat_id: i64,
    pub owner_user_id: i64,
    pub draft_json: String,
    pub expires_at_unix: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct UpsertTelegramSchedulerPendingIntentRequest {
    pub intent_id: String,
    pub channel: String,
    pub chat_id: i64,
    pub owner_user_id: i64,
    pub draft_json: String,
    pub expires_at_unix: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentSwarmRunStatus {
    Running,
    Completed,
    Partial,
    Failed,
}

impl AgentSwarmRunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Partial => "partial",
            Self::Failed => "failed",
        }
    }

    fn from_db(value: &str) -> Self {
        match value {
            "completed" => Self::Completed,
            "partial" => Self::Partial,
            "failed" => Self::Failed,
            _ => Self::Running,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentSwarmNodeState {
    Running,
    Exited,
}

impl AgentSwarmNodeState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Exited => "exited",
        }
    }

    fn from_db(value: &str) -> Self {
        match value {
            "exited" => Self::Exited,
            _ => Self::Running,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentSwarmNodeExitStatus {
    Success,
    Failed,
    Timeout,
    Partial,
    Skipped,
}

impl AgentSwarmNodeExitStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Timeout => "timeout",
            Self::Partial => "partial",
            Self::Skipped => "skipped",
        }
    }

    fn from_db(value: &str) -> Self {
        match value {
            "failed" => Self::Failed,
            "timeout" => Self::Timeout,
            "partial" => Self::Partial,
            "skipped" => Self::Skipped,
            _ => Self::Success,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSwarmRunRecord {
    pub run_id: String,
    pub channel: String,
    pub session_id: String,
    pub user_id: String,
    pub root_task: String,
    pub status: AgentSwarmRunStatus,
    pub started_at_unix: i64,
    pub finished_at_unix: Option<i64>,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub agent_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSwarmNodeRecord {
    pub agent_id: String,
    pub run_id: String,
    pub channel: String,
    pub parent_agent_id: Option<String>,
    pub depth: usize,
    pub nickname: String,
    pub role_name: String,
    pub role_definition: String,
    pub task: String,
    pub state: AgentSwarmNodeState,
    pub exit_status: Option<AgentSwarmNodeExitStatus>,
    pub summary: Option<String>,
    pub error: Option<String>,
    pub started_at_unix: i64,
    pub finished_at_unix: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct CreateAgentSwarmRunRequest {
    pub run_id: String,
    pub channel: String,
    pub session_id: String,
    pub user_id: String,
    pub root_task: String,
    pub status: AgentSwarmRunStatus,
    pub started_at_unix: i64,
}

#[derive(Debug, Clone)]
pub struct FinishAgentSwarmRunRequest {
    pub channel: String,
    pub run_id: String,
    pub status: AgentSwarmRunStatus,
    pub finished_at_unix: i64,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UpsertAgentSwarmNodeRequest {
    pub agent_id: String,
    pub run_id: String,
    pub channel: String,
    pub parent_agent_id: Option<String>,
    pub depth: usize,
    pub nickname: String,
    pub role_name: String,
    pub role_definition: String,
    pub task: String,
    pub state: AgentSwarmNodeState,
    pub exit_status: Option<AgentSwarmNodeExitStatus>,
    pub summary: Option<String>,
    pub error: Option<String>,
    pub started_at_unix: i64,
    pub finished_at_unix: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct AgentSwarmRunListRequest {
    pub channel: String,
    pub user_id: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone)]
pub struct AgentSwarmTreeRequest {
    pub channel: String,
    pub run_id: String,
}

#[derive(Debug, Clone)]
pub struct AgentSwarmNodeLoadRequest {
    pub channel: String,
    pub agent_id: String,
}

#[derive(Debug, Clone)]
pub struct AgentSwarmCleanupRequest {
    pub channel: String,
    pub before_unix: i64,
}

fn telegram_scheduler_job_from_row(
    row: sqlx::sqlite::SqliteRow,
) -> anyhow::Result<TelegramSchedulerJobRecord> {
    let status: String = row.get("status");
    let task_kind: String = row.get("task_kind");
    let schedule_kind: String = row.get("schedule_kind");
    Ok(TelegramSchedulerJobRecord {
        job_id: row.get("job_id"),
        channel: row.get("channel"),
        chat_id: row.get("chat_id"),
        message_thread_id: row.get("message_thread_id"),
        owner_user_id: row.get("owner_user_id"),
        status: TelegramSchedulerJobStatus::from_db(&status),
        task_kind: TelegramSchedulerTaskKind::from_db(&task_kind),
        payload: row.get("payload"),
        schedule_kind: TelegramSchedulerScheduleKind::from_db(&schedule_kind),
        timezone: row.get("timezone"),
        run_at_unix: row.get("run_at_unix"),
        cron_expr: row.get("cron_expr"),
        policy_json: row.get("policy_json"),
        next_run_at_unix: row.get("next_run_at_unix"),
        last_run_at_unix: row.get("last_run_at_unix"),
        last_error: row.get("last_error"),
        run_count: row.get("run_count"),
        failure_streak: row.get("failure_streak"),
        max_runs: row.get("max_runs"),
        lease_token: row.get("lease_token"),
        lease_until_unix: row.get("lease_until_unix"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

fn agent_swarm_run_from_row(row: sqlx::sqlite::SqliteRow) -> anyhow::Result<AgentSwarmRunRecord> {
    let status: String = row.get("status");
    Ok(AgentSwarmRunRecord {
        run_id: row.get("run_id"),
        channel: row.get("channel"),
        session_id: row.get("session_id"),
        user_id: row.get("user_id"),
        root_task: row.get("root_task"),
        status: AgentSwarmRunStatus::from_db(&status),
        started_at_unix: row.get("started_at_unix"),
        finished_at_unix: row.get("finished_at_unix"),
        error: row.get("error"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        agent_count: row.get::<Option<i64>, _>("agent_count").unwrap_or(0),
    })
}

fn agent_swarm_node_from_row(row: sqlx::sqlite::SqliteRow) -> anyhow::Result<AgentSwarmNodeRecord> {
    let state: String = row.get("state");
    let exit_status = row
        .get::<Option<String>, _>("exit_status")
        .map(|v| AgentSwarmNodeExitStatus::from_db(&v));
    Ok(AgentSwarmNodeRecord {
        agent_id: row.get("agent_id"),
        run_id: row.get("run_id"),
        channel: row.get("channel"),
        parent_agent_id: row.get("parent_agent_id"),
        depth: (row.get::<i64, _>("depth")).max(0) as usize,
        nickname: row.get("nickname"),
        role_name: row.get("role_name"),
        role_definition: row.get("role_definition"),
        task: row.get("task"),
        state: AgentSwarmNodeState::from_db(&state),
        exit_status,
        summary: row.get("summary"),
        error: row.get("error"),
        started_at_unix: row.get("started_at_unix"),
        finished_at_unix: row.get("finished_at_unix"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

#[async_trait]
pub trait MemoryBackend: Send + Sync {
    async fn append(&self, req: MemoryWriteRequest) -> anyhow::Result<()>;
    async fn load_context(&self, req: MemoryContextRequest) -> anyhow::Result<Vec<StoredMessage>>;
    async fn upsert_group_aliases(&self, _req: GroupAliasUpsertRequest) -> anyhow::Result<()> {
        Ok(())
    }
    async fn load_group_aliases(&self, _req: GroupAliasLoadRequest) -> anyhow::Result<Vec<String>> {
        Ok(vec![])
    }
    async fn upsert_group_user_profile(
        &self,
        _req: GroupUserProfileUpsertRequest,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn load_group_user_profiles(
        &self,
        _req: GroupUserProfileLoadRequest,
    ) -> anyhow::Result<Vec<GroupUserProfileRecord>> {
        Ok(vec![])
    }
    async fn create_telegram_scheduler_job(
        &self,
        _req: CreateTelegramSchedulerJobRequest,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn list_telegram_scheduler_jobs_by_owner(
        &self,
        _req: TelegramSchedulerJobListRequest,
    ) -> anyhow::Result<Vec<TelegramSchedulerJobRecord>> {
        Ok(vec![])
    }
    async fn query_telegram_scheduler_stats(
        &self,
        _req: TelegramSchedulerStatsRequest,
    ) -> anyhow::Result<TelegramSchedulerStats> {
        Ok(TelegramSchedulerStats::default())
    }
    async fn update_telegram_scheduler_job_status(
        &self,
        _req: UpdateTelegramSchedulerJobStatusRequest,
    ) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn load_telegram_scheduler_job(
        &self,
        _channel: String,
        _job_id: String,
    ) -> anyhow::Result<Option<TelegramSchedulerJobRecord>> {
        Ok(None)
    }
    async fn claim_due_telegram_scheduler_jobs(
        &self,
        _req: ClaimDueTelegramSchedulerJobsRequest,
    ) -> anyhow::Result<Vec<TelegramSchedulerJobRecord>> {
        Ok(vec![])
    }
    async fn complete_telegram_scheduler_job_run(
        &self,
        _req: CompleteTelegramSchedulerJobRunRequest,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn fail_telegram_scheduler_job_run(
        &self,
        _req: FailTelegramSchedulerJobRunRequest,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn upsert_telegram_scheduler_pending_intent(
        &self,
        _req: UpsertTelegramSchedulerPendingIntentRequest,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn load_telegram_scheduler_pending_intent(
        &self,
        _channel: String,
        _chat_id: i64,
        _owner_user_id: i64,
        _now_unix: i64,
    ) -> anyhow::Result<Option<TelegramSchedulerPendingIntentRecord>> {
        Ok(None)
    }
    async fn delete_telegram_scheduler_pending_intent(
        &self,
        _channel: String,
        _chat_id: i64,
        _owner_user_id: i64,
    ) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn create_agent_swarm_run(&self, _req: CreateAgentSwarmRunRequest) -> anyhow::Result<()> {
        Ok(())
    }
    async fn finish_agent_swarm_run(
        &self,
        _req: FinishAgentSwarmRunRequest,
    ) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn upsert_agent_swarm_node(
        &self,
        _req: UpsertAgentSwarmNodeRequest,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn list_agent_swarm_runs(
        &self,
        _req: AgentSwarmRunListRequest,
    ) -> anyhow::Result<Vec<AgentSwarmRunRecord>> {
        Ok(vec![])
    }
    async fn load_agent_swarm_tree(
        &self,
        _req: AgentSwarmTreeRequest,
    ) -> anyhow::Result<Vec<AgentSwarmNodeRecord>> {
        Ok(vec![])
    }
    async fn load_agent_swarm_node(
        &self,
        _req: AgentSwarmNodeLoadRequest,
    ) -> anyhow::Result<Option<AgentSwarmNodeRecord>> {
        Ok(None)
    }
    async fn cleanup_agent_swarm_audit(
        &self,
        _req: AgentSwarmCleanupRequest,
    ) -> anyhow::Result<i64> {
        Ok(0)
    }
}

#[derive(Clone)]
pub struct SqliteMemoryBackend {
    store: SqliteMemoryStore,
}

impl SqliteMemoryBackend {
    pub fn new(store: SqliteMemoryStore) -> Self {
        Self { store }
    }
}

#[async_trait]
impl MemoryBackend for SqliteMemoryBackend {
    async fn append(&self, req: MemoryWriteRequest) -> anyhow::Result<()> {
        self.store
            .append(&req.session_id, req.message.clone())
            .await
            .context("failed to append message in sqlite backend")?;

        if req.message.content.trim().is_empty() {
            return Ok(());
        }

        let created_at = unix_ts();
        let chunk_id = stable_chunk_id(&req, created_at);
        self.store
            .append_chunk(MemoryChunkRecord {
                chunk_id,
                session_id: req.session_id,
                user_id: req.user_id,
                channel: req.channel,
                role: req.message.role,
                text: req.message.content,
                created_at,
            })
            .await
            .context("failed to append memory chunk in sqlite backend")
    }

    async fn load_context(&self, req: MemoryContextRequest) -> anyhow::Result<Vec<StoredMessage>> {
        let recent = self
            .store
            .load_recent(&req.session_id, req.max_recent_turns)
            .await
            .context("failed to load recent messages in sqlite backend")?;
        if req.max_semantic_memories == 0 || req.query_text.trim().is_empty() {
            return Ok(recent);
        }

        let keyword_limit = req.max_semantic_memories.max(1);
        let keyword_candidates = collect_local_keyword_candidates(
            &self.store,
            &req,
            keyword_limit,
            keyword_limit.saturating_mul(24).clamp(64, 512),
            0.18,
        )
        .await
        .context("failed to collect local keyword candidates in sqlite backend")?;

        Ok(merge_memory_candidates_into_context(
            recent,
            Vec::new(),
            keyword_candidates,
            req.max_semantic_memories,
            420,
            0.18,
        ))
    }

    async fn upsert_group_aliases(&self, req: GroupAliasUpsertRequest) -> anyhow::Result<()> {
        self.store
            .upsert_group_aliases(&req.channel, req.chat_id, &req.aliases)
            .await
    }

    async fn load_group_aliases(&self, req: GroupAliasLoadRequest) -> anyhow::Result<Vec<String>> {
        self.store
            .load_group_aliases(&req.channel, req.chat_id, req.limit)
            .await
    }

    async fn upsert_group_user_profile(
        &self,
        req: GroupUserProfileUpsertRequest,
    ) -> anyhow::Result<()> {
        self.store
            .upsert_group_user_profile(
                &req.channel,
                req.chat_id,
                req.user_id,
                &req.preferred_name,
                req.username.as_deref(),
            )
            .await
    }

    async fn load_group_user_profiles(
        &self,
        req: GroupUserProfileLoadRequest,
    ) -> anyhow::Result<Vec<GroupUserProfileRecord>> {
        self.store
            .load_group_user_profiles(&req.channel, req.chat_id, req.limit)
            .await
    }

    async fn create_telegram_scheduler_job(
        &self,
        req: CreateTelegramSchedulerJobRequest,
    ) -> anyhow::Result<()> {
        self.store.create_telegram_scheduler_job(req).await
    }

    async fn list_telegram_scheduler_jobs_by_owner(
        &self,
        req: TelegramSchedulerJobListRequest,
    ) -> anyhow::Result<Vec<TelegramSchedulerJobRecord>> {
        self.store
            .list_telegram_scheduler_jobs_by_owner(
                &req.channel,
                req.chat_id,
                req.owner_user_id,
                req.limit,
            )
            .await
    }

    async fn query_telegram_scheduler_stats(
        &self,
        req: TelegramSchedulerStatsRequest,
    ) -> anyhow::Result<TelegramSchedulerStats> {
        self.store
            .query_telegram_scheduler_stats(&req.channel, req.now_unix)
            .await
    }

    async fn update_telegram_scheduler_job_status(
        &self,
        req: UpdateTelegramSchedulerJobStatusRequest,
    ) -> anyhow::Result<bool> {
        self.store
            .update_telegram_scheduler_job_status(
                &req.channel,
                req.chat_id,
                req.owner_user_id,
                &req.job_id,
                req.status,
            )
            .await
    }

    async fn load_telegram_scheduler_job(
        &self,
        channel: String,
        job_id: String,
    ) -> anyhow::Result<Option<TelegramSchedulerJobRecord>> {
        self.store
            .load_telegram_scheduler_job(&channel, &job_id)
            .await
    }

    async fn claim_due_telegram_scheduler_jobs(
        &self,
        req: ClaimDueTelegramSchedulerJobsRequest,
    ) -> anyhow::Result<Vec<TelegramSchedulerJobRecord>> {
        self.store
            .claim_due_telegram_scheduler_jobs(
                &req.channel,
                req.now_unix,
                req.limit,
                req.lease_secs,
                &req.lease_token,
            )
            .await
    }

    async fn complete_telegram_scheduler_job_run(
        &self,
        req: CompleteTelegramSchedulerJobRunRequest,
    ) -> anyhow::Result<()> {
        self.store.complete_telegram_scheduler_job_run(req).await
    }

    async fn fail_telegram_scheduler_job_run(
        &self,
        req: FailTelegramSchedulerJobRunRequest,
    ) -> anyhow::Result<()> {
        self.store.fail_telegram_scheduler_job_run(req).await
    }

    async fn upsert_telegram_scheduler_pending_intent(
        &self,
        req: UpsertTelegramSchedulerPendingIntentRequest,
    ) -> anyhow::Result<()> {
        self.store
            .upsert_telegram_scheduler_pending_intent(req)
            .await
    }

    async fn load_telegram_scheduler_pending_intent(
        &self,
        channel: String,
        chat_id: i64,
        owner_user_id: i64,
        now_unix: i64,
    ) -> anyhow::Result<Option<TelegramSchedulerPendingIntentRecord>> {
        self.store
            .load_telegram_scheduler_pending_intent(&channel, chat_id, owner_user_id, now_unix)
            .await
    }

    async fn delete_telegram_scheduler_pending_intent(
        &self,
        channel: String,
        chat_id: i64,
        owner_user_id: i64,
    ) -> anyhow::Result<bool> {
        self.store
            .delete_telegram_scheduler_pending_intent(&channel, chat_id, owner_user_id)
            .await
    }

    async fn create_agent_swarm_run(&self, req: CreateAgentSwarmRunRequest) -> anyhow::Result<()> {
        self.store.create_agent_swarm_run(req).await
    }

    async fn finish_agent_swarm_run(
        &self,
        req: FinishAgentSwarmRunRequest,
    ) -> anyhow::Result<bool> {
        self.store.finish_agent_swarm_run(req).await
    }

    async fn upsert_agent_swarm_node(
        &self,
        req: UpsertAgentSwarmNodeRequest,
    ) -> anyhow::Result<()> {
        self.store.upsert_agent_swarm_node(req).await
    }

    async fn list_agent_swarm_runs(
        &self,
        req: AgentSwarmRunListRequest,
    ) -> anyhow::Result<Vec<AgentSwarmRunRecord>> {
        self.store
            .list_agent_swarm_runs(&req.channel, req.user_id.as_deref(), req.limit)
            .await
    }

    async fn load_agent_swarm_tree(
        &self,
        req: AgentSwarmTreeRequest,
    ) -> anyhow::Result<Vec<AgentSwarmNodeRecord>> {
        self.store
            .load_agent_swarm_nodes_by_run(&req.channel, &req.run_id)
            .await
    }

    async fn load_agent_swarm_node(
        &self,
        req: AgentSwarmNodeLoadRequest,
    ) -> anyhow::Result<Option<AgentSwarmNodeRecord>> {
        self.store
            .load_agent_swarm_node(&req.channel, &req.agent_id)
            .await
    }

    async fn cleanup_agent_swarm_audit(
        &self,
        req: AgentSwarmCleanupRequest,
    ) -> anyhow::Result<i64> {
        self.store
            .cleanup_agent_swarm_audit(&req.channel, req.before_unix)
            .await
    }
}

#[derive(Debug, Clone)]
pub struct ZvecSidecarConfig {
    pub endpoint: String,
    pub collection: String,
    pub query_topk: usize,
    pub request_timeout_secs: u64,
    pub upsert_path: String,
    pub query_path: String,
    pub auth_bearer_token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HybridRetrievalOptions {
    pub keyword_enabled: bool,
    pub keyword_topk: usize,
    pub keyword_candidate_limit: usize,
    pub memory_snippet_max_chars: usize,
    pub min_score: f32,
}

impl Default for HybridRetrievalOptions {
    fn default() -> Self {
        Self {
            keyword_enabled: true,
            keyword_topk: 8,
            keyword_candidate_limit: 256,
            memory_snippet_max_chars: 420,
            min_score: 0.18,
        }
    }
}

#[derive(Clone)]
pub struct ZvecSidecarClient {
    client: Client,
    cfg: ZvecSidecarConfig,
}

impl ZvecSidecarClient {
    pub fn new(cfg: ZvecSidecarConfig) -> Self {
        Self {
            client: Client::new(),
            cfg,
        }
    }

    async fn upsert(&self, doc: SidecarMemoryDoc) -> anyhow::Result<()> {
        let request = SidecarUpsertRequest {
            collection: self.cfg.collection.clone(),
            docs: vec![doc],
        };

        self.request(self.cfg.upsert_path.as_str(), &request)
            .await
            .context("failed to call zvec sidecar upsert")?
            .error_for_status()
            .context("zvec sidecar upsert returned error")?;
        Ok(())
    }

    async fn query(&self, req: &MemoryContextRequest) -> anyhow::Result<Vec<SidecarMemoryHit>> {
        let request = SidecarQueryRequest {
            collection: self.cfg.collection.clone(),
            text: req.query_text.clone(),
            user_id: req.user_id.clone(),
            channel: req.channel.clone(),
            lookback_days: req.semantic_lookback_days,
            topk: req.max_semantic_memories.max(self.cfg.query_topk),
        };

        let query_path = self.cfg.query_path.as_str();
        let response = self
            .request(query_path, &request)
            .await
            .context("failed to call zvec sidecar query")?;

        let response = if response.status().is_success() {
            response
        } else if response.status() == StatusCode::NOT_FOUND && query_path != "/v1/memory/search" {
            let legacy = self
                .request("/v1/memory/search", &request)
                .await
                .context("failed to call zvec sidecar legacy search endpoint")?;
            legacy
                .error_for_status()
                .context("zvec sidecar legacy search returned error")?
        } else {
            response
                .error_for_status()
                .context("zvec sidecar query returned error")?
        };

        let body: SidecarQueryResponse = response
            .json()
            .await
            .context("failed to decode zvec sidecar query response")?;
        Ok(body.into_hits())
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.cfg.endpoint.trim_end_matches('/'), path)
    }

    async fn request<T: Serialize>(
        &self,
        path: &str,
        payload: &T,
    ) -> anyhow::Result<reqwest::Response> {
        let mut req = self
            .client
            .post(self.url(path))
            .timeout(std::time::Duration::from_secs(
                self.cfg.request_timeout_secs.max(1),
            ))
            .json(payload);

        if let Some(token) = &self.cfg.auth_bearer_token
            && !token.trim().is_empty()
        {
            req = req.bearer_auth(token);
        }

        req.send().await.context("sidecar HTTP request failed")
    }
}

#[derive(Clone)]
pub struct HybridSqliteZvecMemoryBackend {
    store: SqliteMemoryStore,
    sidecar: ZvecSidecarClient,
    options: HybridRetrievalOptions,
}

impl HybridSqliteZvecMemoryBackend {
    pub fn new(store: SqliteMemoryStore, sidecar: ZvecSidecarClient) -> Self {
        Self::with_options(store, sidecar, HybridRetrievalOptions::default())
    }

    pub fn with_options(
        store: SqliteMemoryStore,
        sidecar: ZvecSidecarClient,
        options: HybridRetrievalOptions,
    ) -> Self {
        Self {
            store,
            sidecar,
            options,
        }
    }
}

#[async_trait]
impl MemoryBackend for HybridSqliteZvecMemoryBackend {
    async fn append(&self, req: MemoryWriteRequest) -> anyhow::Result<()> {
        self.store
            .append(&req.session_id, req.message.clone())
            .await
            .context("failed to append message in hybrid backend")?;

        if req.message.content.trim().is_empty() {
            return Ok(());
        }

        let created_at = unix_ts();
        let chunk_id = stable_chunk_id(&req, created_at);

        self.store
            .append_chunk(MemoryChunkRecord {
                chunk_id: chunk_id.clone(),
                session_id: req.session_id.clone(),
                user_id: req.user_id.clone(),
                channel: req.channel.clone(),
                role: req.message.role.clone(),
                text: req.message.content.clone(),
                created_at,
            })
            .await
            .context("failed to append memory chunk in hybrid backend")?;

        let sidecar = self.sidecar.clone();
        let doc = SidecarMemoryDoc {
            id: chunk_id,
            text: req.message.content,
            role: req.message.role.as_str().to_string(),
            session_id: req.session_id,
            user_id: req.user_id,
            channel: req.channel,
            created_at,
            importance: 0.5,
        };

        tokio::spawn(async move {
            if let Err(err) = sidecar.upsert(doc).await {
                warn!(error = %err, "failed to upsert memory into zvec sidecar");
            }
        });

        Ok(())
    }

    async fn load_context(&self, req: MemoryContextRequest) -> anyhow::Result<Vec<StoredMessage>> {
        let recent = self
            .store
            .load_recent(&req.session_id, req.max_recent_turns)
            .await
            .context("failed to load recent messages in hybrid backend")?;

        if req.max_semantic_memories == 0 || req.query_text.trim().is_empty() {
            return Ok(recent);
        }

        let semantic_candidates = match self.sidecar.query(&req).await {
            Ok(hits) => hits
                .into_iter()
                .filter_map(|hit| {
                    let text = hit.resolved_text()?.trim().to_string();
                    if text.is_empty() {
                        return None;
                    }
                    let score = normalize_memory_score(hit.score);
                    if score < self.options.min_score {
                        return None;
                    }
                    Some(RankedMemoryCandidate {
                        text,
                        score,
                        source: "vec".to_string(),
                        created_at: unix_ts(),
                    })
                })
                .collect::<Vec<_>>(),
            Err(err) => {
                warn!(
                    error = %err,
                    "zvec sidecar query failed, fallback to local keyword retrieval"
                );
                Vec::new()
            }
        };

        let keyword_candidates = if self.options.keyword_enabled {
            collect_local_keyword_candidates(
                &self.store,
                &req,
                self.options.keyword_topk.max(req.max_semantic_memories),
                self.options.keyword_candidate_limit,
                self.options.min_score,
            )
            .await
            .unwrap_or_else(|err| {
                warn!(error = %err, "local keyword retrieval failed");
                Vec::new()
            })
        } else {
            Vec::new()
        };

        Ok(merge_memory_candidates_into_context(
            recent,
            semantic_candidates,
            keyword_candidates,
            req.max_semantic_memories,
            self.options.memory_snippet_max_chars,
            self.options.min_score,
        ))
    }

    async fn upsert_group_aliases(&self, req: GroupAliasUpsertRequest) -> anyhow::Result<()> {
        self.store
            .upsert_group_aliases(&req.channel, req.chat_id, &req.aliases)
            .await
    }

    async fn load_group_aliases(&self, req: GroupAliasLoadRequest) -> anyhow::Result<Vec<String>> {
        self.store
            .load_group_aliases(&req.channel, req.chat_id, req.limit)
            .await
    }

    async fn upsert_group_user_profile(
        &self,
        req: GroupUserProfileUpsertRequest,
    ) -> anyhow::Result<()> {
        self.store
            .upsert_group_user_profile(
                &req.channel,
                req.chat_id,
                req.user_id,
                &req.preferred_name,
                req.username.as_deref(),
            )
            .await
    }

    async fn load_group_user_profiles(
        &self,
        req: GroupUserProfileLoadRequest,
    ) -> anyhow::Result<Vec<GroupUserProfileRecord>> {
        self.store
            .load_group_user_profiles(&req.channel, req.chat_id, req.limit)
            .await
    }

    async fn create_telegram_scheduler_job(
        &self,
        req: CreateTelegramSchedulerJobRequest,
    ) -> anyhow::Result<()> {
        self.store.create_telegram_scheduler_job(req).await
    }

    async fn list_telegram_scheduler_jobs_by_owner(
        &self,
        req: TelegramSchedulerJobListRequest,
    ) -> anyhow::Result<Vec<TelegramSchedulerJobRecord>> {
        self.store
            .list_telegram_scheduler_jobs_by_owner(
                &req.channel,
                req.chat_id,
                req.owner_user_id,
                req.limit,
            )
            .await
    }

    async fn query_telegram_scheduler_stats(
        &self,
        req: TelegramSchedulerStatsRequest,
    ) -> anyhow::Result<TelegramSchedulerStats> {
        self.store
            .query_telegram_scheduler_stats(&req.channel, req.now_unix)
            .await
    }

    async fn update_telegram_scheduler_job_status(
        &self,
        req: UpdateTelegramSchedulerJobStatusRequest,
    ) -> anyhow::Result<bool> {
        self.store
            .update_telegram_scheduler_job_status(
                &req.channel,
                req.chat_id,
                req.owner_user_id,
                &req.job_id,
                req.status,
            )
            .await
    }

    async fn load_telegram_scheduler_job(
        &self,
        channel: String,
        job_id: String,
    ) -> anyhow::Result<Option<TelegramSchedulerJobRecord>> {
        self.store
            .load_telegram_scheduler_job(&channel, &job_id)
            .await
    }

    async fn claim_due_telegram_scheduler_jobs(
        &self,
        req: ClaimDueTelegramSchedulerJobsRequest,
    ) -> anyhow::Result<Vec<TelegramSchedulerJobRecord>> {
        self.store
            .claim_due_telegram_scheduler_jobs(
                &req.channel,
                req.now_unix,
                req.limit,
                req.lease_secs,
                &req.lease_token,
            )
            .await
    }

    async fn complete_telegram_scheduler_job_run(
        &self,
        req: CompleteTelegramSchedulerJobRunRequest,
    ) -> anyhow::Result<()> {
        self.store.complete_telegram_scheduler_job_run(req).await
    }

    async fn fail_telegram_scheduler_job_run(
        &self,
        req: FailTelegramSchedulerJobRunRequest,
    ) -> anyhow::Result<()> {
        self.store.fail_telegram_scheduler_job_run(req).await
    }

    async fn upsert_telegram_scheduler_pending_intent(
        &self,
        req: UpsertTelegramSchedulerPendingIntentRequest,
    ) -> anyhow::Result<()> {
        self.store
            .upsert_telegram_scheduler_pending_intent(req)
            .await
    }

    async fn load_telegram_scheduler_pending_intent(
        &self,
        channel: String,
        chat_id: i64,
        owner_user_id: i64,
        now_unix: i64,
    ) -> anyhow::Result<Option<TelegramSchedulerPendingIntentRecord>> {
        self.store
            .load_telegram_scheduler_pending_intent(&channel, chat_id, owner_user_id, now_unix)
            .await
    }

    async fn delete_telegram_scheduler_pending_intent(
        &self,
        channel: String,
        chat_id: i64,
        owner_user_id: i64,
    ) -> anyhow::Result<bool> {
        self.store
            .delete_telegram_scheduler_pending_intent(&channel, chat_id, owner_user_id)
            .await
    }

    async fn create_agent_swarm_run(&self, req: CreateAgentSwarmRunRequest) -> anyhow::Result<()> {
        self.store.create_agent_swarm_run(req).await
    }

    async fn finish_agent_swarm_run(
        &self,
        req: FinishAgentSwarmRunRequest,
    ) -> anyhow::Result<bool> {
        self.store.finish_agent_swarm_run(req).await
    }

    async fn upsert_agent_swarm_node(
        &self,
        req: UpsertAgentSwarmNodeRequest,
    ) -> anyhow::Result<()> {
        self.store.upsert_agent_swarm_node(req).await
    }

    async fn list_agent_swarm_runs(
        &self,
        req: AgentSwarmRunListRequest,
    ) -> anyhow::Result<Vec<AgentSwarmRunRecord>> {
        self.store
            .list_agent_swarm_runs(&req.channel, req.user_id.as_deref(), req.limit)
            .await
    }

    async fn load_agent_swarm_tree(
        &self,
        req: AgentSwarmTreeRequest,
    ) -> anyhow::Result<Vec<AgentSwarmNodeRecord>> {
        self.store
            .load_agent_swarm_nodes_by_run(&req.channel, &req.run_id)
            .await
    }

    async fn load_agent_swarm_node(
        &self,
        req: AgentSwarmNodeLoadRequest,
    ) -> anyhow::Result<Option<AgentSwarmNodeRecord>> {
        self.store
            .load_agent_swarm_node(&req.channel, &req.agent_id)
            .await
    }

    async fn cleanup_agent_swarm_audit(
        &self,
        req: AgentSwarmCleanupRequest,
    ) -> anyhow::Result<i64> {
        self.store
            .cleanup_agent_swarm_audit(&req.channel, req.before_unix)
            .await
    }
}

#[derive(Debug, Clone)]
struct RankedMemoryCandidate {
    text: String,
    score: f32,
    source: String,
    created_at: i64,
}

async fn collect_local_keyword_candidates(
    store: &SqliteMemoryStore,
    req: &MemoryContextRequest,
    topk: usize,
    candidate_limit: usize,
    min_score: f32,
) -> anyhow::Result<Vec<RankedMemoryCandidate>> {
    if topk == 0 || req.query_text.trim().is_empty() {
        return Ok(vec![]);
    }

    let lookback_secs = (req.semantic_lookback_days.max(1) as i64).saturating_mul(86_400);
    let min_created_at = unix_ts().saturating_sub(lookback_secs);

    let rows = store
        .load_chunk_candidates(
            &req.user_id,
            &req.channel,
            min_created_at,
            candidate_limit.max(topk),
        )
        .await?;

    Ok(rank_local_keyword_candidates(
        &req.query_text,
        rows,
        topk,
        min_score,
    ))
}

fn rank_local_keyword_candidates(
    query_text: &str,
    rows: Vec<MemoryChunkCandidateRecord>,
    topk: usize,
    min_score: f32,
) -> Vec<RankedMemoryCandidate> {
    let terms = keyword_terms(query_text);
    if terms.is_empty() {
        return vec![];
    }

    let now = unix_ts();
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    for row in rows {
        let text = row.text.trim();
        if text.is_empty() || !seen.insert(text.to_string()) {
            continue;
        }

        let lexical = keyword_overlap_score(text, &terms);
        if lexical <= 0.0 {
            continue;
        }

        let role_weight = match row.role {
            MessageRole::User => 1.0,
            MessageRole::Assistant => 0.9,
            MessageRole::System => 0.7,
        };
        let age_secs = now.saturating_sub(row.created_at.max(0)) as f32;
        let age_days = age_secs / 86_400.0;
        let recency = 1.0 / (1.0 + age_days / 7.0);
        let score = normalize_memory_score((lexical * 0.82 + recency * 0.18) * role_weight);
        if score < min_score {
            continue;
        }

        out.push(RankedMemoryCandidate {
            text: text.to_string(),
            score,
            source: "kw".to_string(),
            created_at: row.created_at,
        });
    }

    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.created_at.cmp(&a.created_at))
    });
    out.truncate(topk);
    out
}

fn keyword_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut ascii_buf = String::new();
    let mut cjk_buf = String::new();

    for ch in query.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            if !cjk_buf.is_empty() {
                push_cjk_terms(&mut terms, &cjk_buf);
                cjk_buf.clear();
            }
            ascii_buf.push(ch.to_ascii_lowercase());
            continue;
        }

        if is_cjk_char(ch) {
            if !ascii_buf.is_empty() {
                if ascii_buf.chars().count() >= 2 {
                    terms.push(ascii_buf.clone());
                }
                ascii_buf.clear();
            }
            cjk_buf.push(ch);
            continue;
        }

        if !ascii_buf.is_empty() {
            if ascii_buf.chars().count() >= 2 {
                terms.push(ascii_buf.clone());
            }
            ascii_buf.clear();
        }
        if !cjk_buf.is_empty() {
            push_cjk_terms(&mut terms, &cjk_buf);
            cjk_buf.clear();
        }
    }

    if !ascii_buf.is_empty() && ascii_buf.chars().count() >= 2 {
        terms.push(ascii_buf);
    }
    if !cjk_buf.is_empty() {
        push_cjk_terms(&mut terms, &cjk_buf);
    }

    let mut dedup = Vec::new();
    let mut seen = HashSet::new();
    for term in terms {
        if seen.insert(term.clone()) {
            dedup.push(term);
        }
        if dedup.len() >= 40 {
            break;
        }
    }
    dedup
}

fn push_cjk_terms(out: &mut Vec<String>, text: &str) {
    let chars = text.chars().collect::<Vec<_>>();
    if chars.is_empty() {
        return;
    }
    if chars.len() == 1 {
        out.push(chars[0].to_string());
        return;
    }
    for window in chars.windows(2) {
        let mut s = String::new();
        s.push(window[0]);
        s.push(window[1]);
        out.push(s);
    }
}

fn is_cjk_char(ch: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&ch)
        || ('\u{3400}'..='\u{4DBF}').contains(&ch)
        || ('\u{20000}'..='\u{2A6DF}').contains(&ch)
}

fn keyword_overlap_score(text: &str, terms: &[String]) -> f32 {
    if terms.is_empty() {
        return 0.0;
    }
    let hay = text.to_lowercase();
    let mut hit = 0.0f32;
    let mut total = 0.0f32;
    for term in terms {
        let len = term.chars().count();
        let weight = if len >= 6 {
            2.0
        } else if len >= 4 {
            1.4
        } else if len >= 2 {
            1.0
        } else {
            0.6
        };
        total += weight;
        if hay.contains(term) {
            hit += weight;
        }
    }
    if total <= 0.0 {
        0.0
    } else {
        (hit / total).clamp(0.0, 1.0)
    }
}

fn merge_memory_candidates_into_context(
    recent: Vec<StoredMessage>,
    semantic_candidates: Vec<RankedMemoryCandidate>,
    keyword_candidates: Vec<RankedMemoryCandidate>,
    max_semantic_memories: usize,
    memory_snippet_max_chars: usize,
    min_score: f32,
) -> Vec<StoredMessage> {
    if max_semantic_memories == 0 {
        return recent;
    }

    let mut recent_seen = HashSet::new();
    for msg in &recent {
        let key = normalize_memory_text_key(&msg.content);
        if !key.is_empty() {
            recent_seen.insert(key);
        }
    }

    let mut merged_by_text: HashMap<String, RankedMemoryCandidate> = HashMap::new();
    for candidate in semantic_candidates
        .into_iter()
        .chain(keyword_candidates.into_iter())
    {
        if candidate.score < min_score {
            continue;
        }
        let key = normalize_memory_text_key(&candidate.text);
        if key.is_empty() || recent_seen.contains(&key) {
            continue;
        }

        match merged_by_text.get_mut(&key) {
            Some(existing) => {
                if candidate.score > existing.score {
                    existing.score = candidate.score;
                }
                if candidate.created_at > existing.created_at {
                    existing.created_at = candidate.created_at;
                }
                existing.source = merge_source_tag(&existing.source, &candidate.source);
                if candidate.text.chars().count() > existing.text.chars().count() {
                    existing.text = candidate.text;
                }
            }
            None => {
                merged_by_text.insert(key, candidate);
            }
        }
    }

    let mut merged = merged_by_text.into_values().collect::<Vec<_>>();
    merged.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.created_at.cmp(&a.created_at))
    });
    merged.truncate(max_semantic_memories);

    let mut out = Vec::with_capacity(merged.len() + recent.len());
    for candidate in merged {
        let snippet = truncate_chars(&candidate.text, memory_snippet_max_chars.max(120));
        out.push(StoredMessage {
            role: MessageRole::System,
            content: format!(
                "[memory score={:.3} src={}] {}",
                normalize_memory_score(candidate.score),
                candidate.source,
                snippet
            ),
        });
    }
    out.extend(recent);
    out
}

fn normalize_memory_score(score: f32) -> f32 {
    if score.is_finite() {
        score.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn normalize_memory_text_key(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_lowercase()
}

fn merge_source_tag(current: &str, next: &str) -> String {
    if current == next {
        return current.to_string();
    }
    if current.contains(next) {
        return current.to_string();
    }
    if next.contains(current) {
        return next.to_string();
    }
    let mut tags = current
        .split('+')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    if !tags.iter().any(|v| v == next) {
        tags.push(next.to_string());
    }
    tags.join("+")
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut out = String::new();
    for (idx, ch) in input.chars().enumerate() {
        if idx >= max_chars {
            break;
        }
        out.push(ch);
    }
    out
}

#[derive(Debug, Clone, Serialize)]
struct SidecarUpsertRequest {
    collection: String,
    docs: Vec<SidecarMemoryDoc>,
}

#[derive(Debug, Clone, Serialize)]
struct SidecarMemoryDoc {
    id: String,
    text: String,
    role: String,
    session_id: String,
    user_id: String,
    channel: String,
    created_at: i64,
    importance: f32,
}

#[derive(Debug, Clone, Serialize)]
struct SidecarQueryRequest {
    collection: String,
    text: String,
    user_id: String,
    channel: String,
    lookback_days: u32,
    topk: usize,
}

#[derive(Debug, Clone, Deserialize)]
struct SidecarQueryResponse {
    #[serde(default)]
    hits: Vec<SidecarMemoryHit>,
    #[serde(default)]
    result: Option<SidecarQueryNestedHits>,
    #[serde(default)]
    data: Option<Vec<SidecarMemoryHit>>,
}

#[derive(Debug, Clone, Deserialize)]
struct SidecarMemoryHit {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    score: f32,
}

#[derive(Debug, Clone, Deserialize)]
struct SidecarQueryNestedHits {
    #[serde(default)]
    hits: Vec<SidecarMemoryHit>,
}

impl SidecarQueryResponse {
    fn into_hits(self) -> Vec<SidecarMemoryHit> {
        if !self.hits.is_empty() {
            return self.hits;
        }
        if let Some(result) = self.result
            && !result.hits.is_empty()
        {
            return result.hits;
        }
        self.data.unwrap_or_default()
    }
}

impl SidecarMemoryHit {
    fn resolved_text(&self) -> Option<&str> {
        self.text
            .as_deref()
            .or(self.content.as_deref())
            .or(self.message.as_deref())
    }
}

fn unix_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn unix_ts_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn stable_chunk_id(req: &MemoryWriteRequest, ts_secs: i64) -> String {
    let mut hasher = DefaultHasher::new();
    req.session_id.hash(&mut hasher);
    req.user_id.hash(&mut hasher);
    req.channel.hash(&mut hasher);
    req.message.role.as_str().hash(&mut hasher);
    req.message.content.hash(&mut hasher);
    ts_secs.hash(&mut hasher);
    unix_ts_nanos().hash(&mut hasher);
    format!("mem-{:x}", hasher.finish())
}
