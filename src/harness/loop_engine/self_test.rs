use std::time::Instant;

use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use super::store::new_loop_id;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelfTestStatus {
    Passed,
    Failed,
}

impl SelfTestStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
        }
    }

    fn from_db(value: &str) -> anyhow::Result<Self> {
        match value {
            "passed" => Ok(Self::Passed),
            "failed" => Ok(Self::Failed),
            other => bail!("unknown self-test status: {other}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfTestCaseResult {
    pub name: String,
    pub passed: bool,
    pub details: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfTestRun {
    pub id: String,
    pub suite: String,
    pub status: SelfTestStatus,
    pub cases: Vec<SelfTestCaseResult>,
    pub started_at_unix: i64,
    pub finished_at_unix: i64,
}

pub(crate) async fn initialize_self_test_schema(pool: &SqlitePool) -> anyhow::Result<()> {
    for statement in [
        "CREATE TABLE IF NOT EXISTS harness_self_test_runs (
            id TEXT PRIMARY KEY,
            suite TEXT NOT NULL,
            status TEXT NOT NULL,
            started_at INTEGER NOT NULL,
            finished_at INTEGER NOT NULL,
            created_by TEXT NOT NULL
        )",
        "CREATE INDEX IF NOT EXISTS idx_harness_self_test_runs_suite_started
         ON harness_self_test_runs(suite, started_at DESC, id)",
        "CREATE TABLE IF NOT EXISTS harness_self_test_case_results (
            run_id TEXT NOT NULL REFERENCES harness_self_test_runs(id),
            case_index INTEGER NOT NULL,
            name TEXT NOT NULL,
            passed INTEGER NOT NULL,
            details TEXT NOT NULL,
            duration_ms INTEGER NOT NULL,
            PRIMARY KEY (run_id, case_index)
        )",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .context("failed to initialize self-test schema")?;
    }
    Ok(())
}

pub(crate) async fn run_self_tests(
    pool: &SqlitePool,
    suite: &str,
    actor: &str,
) -> anyhow::Result<SelfTestRun> {
    ensure!(suite == "core", "unknown self-test suite: {suite}");
    let started_at = unix_now();
    let cases = vec![
        count_case(
            pool,
            "required_schema",
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name IN (
               'harness_goals', 'harness_workflows', 'harness_goal_approvals',
               'harness_goal_budget_usage', 'harness_work_items',
               'harness_work_item_dependencies', 'harness_attempts',
               'harness_checkpoints', 'harness_events',
               'harness_manual_verifications', 'harness_signals',
               'harness_signal_events', 'harness_self_test_runs',
               'harness_self_test_case_results', 'harness_trajectory_frames',
               'harness_replay_runs', 'harness_replay_case_results',
               'harness_artifacts', 'harness_artifact_events',
               'harness_schema_migrations'
             )",
            20,
            "all durable loop tables are present",
        )
        .await,
        count_case(
            pool,
            "schema_version",
            "SELECT COUNT(*) FROM harness_schema_migrations
             WHERE version = 1 AND name = 'loop_engine_initial_schema'",
            1,
            "loop engine schema version 1 is applied",
        )
        .await,
        count_case(
            pool,
            "work_item_references",
            "SELECT COUNT(*) FROM harness_work_items w
             LEFT JOIN harness_goals g ON g.id = w.goal_id
             LEFT JOIN harness_workflows p ON p.id = w.workflow_id
             WHERE g.id IS NULL OR p.id IS NULL",
            0,
            "all work items reference a goal and immutable workflow",
        )
        .await,
        count_case(
            pool,
            "checkpoint_outcomes",
            "SELECT COUNT(*) FROM harness_checkpoints
             WHERE phase IN ('committed', 'reconciled') AND outcome_json IS NULL",
            0,
            "committed checkpoints always retain their durable outcome",
        )
        .await,
        count_case(
            pool,
            "signal_event_references",
            "SELECT COUNT(*) FROM harness_signal_events e
             LEFT JOIN harness_signals s ON s.id = e.signal_id
             WHERE s.id IS NULL",
            0,
            "append-only signal events always reference immutable signals",
        )
        .await,
        count_case(
            pool,
            "approval_bindings",
            "SELECT COUNT(*) FROM harness_goal_approvals a
             LEFT JOIN harness_workflows p
               ON p.goal_id = a.goal_id AND p.goal_revision = a.goal_revision
             WHERE p.id IS NULL OR p.plan_hash != a.plan_hash",
            0,
            "every approval still resolves to the reviewed workflow hash",
        )
        .await,
    ];
    let status = if cases.iter().all(|case| case.passed) {
        SelfTestStatus::Passed
    } else {
        SelfTestStatus::Failed
    };
    let run = SelfTestRun {
        id: new_loop_id("selftest"),
        suite: suite.to_string(),
        status,
        cases,
        started_at_unix: started_at,
        finished_at_unix: unix_now(),
    };
    persist_run(pool, &run, actor).await?;
    Ok(run)
}

pub(crate) async fn get_self_test_run(
    pool: &SqlitePool,
    run_id: &str,
) -> anyhow::Result<Option<SelfTestRun>> {
    let row = sqlx::query(
        "SELECT id, suite, status, started_at, finished_at
         FROM harness_self_test_runs WHERE id = ?1",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let cases = sqlx::query(
        "SELECT name, passed, details, duration_ms
         FROM harness_self_test_case_results
         WHERE run_id = ?1 ORDER BY case_index",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| {
        let duration_ms: i64 = row.try_get("duration_ms")?;
        Ok(SelfTestCaseResult {
            name: row.try_get("name")?,
            passed: row.try_get::<i64, _>("passed")? != 0,
            details: row.try_get("details")?,
            duration_ms: u64::try_from(duration_ms).context("invalid self-test duration")?,
        })
    })
    .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(Some(SelfTestRun {
        id: row.try_get("id")?,
        suite: row.try_get("suite")?,
        status: SelfTestStatus::from_db(row.try_get("status")?)?,
        cases,
        started_at_unix: row.try_get("started_at")?,
        finished_at_unix: row.try_get("finished_at")?,
    }))
}

async fn count_case(
    pool: &SqlitePool,
    name: &str,
    sql: &str,
    expected: i64,
    success_details: &str,
) -> SelfTestCaseResult {
    let started = Instant::now();
    let (passed, details) = match sqlx::query_scalar::<_, i64>(sql).fetch_one(pool).await {
        Ok(actual) if actual == expected => (true, success_details.to_string()),
        Ok(actual) => (
            false,
            format!("expected count {expected}, observed {actual}"),
        ),
        Err(error) => (false, format!("read-only check failed: {error}")),
    };
    SelfTestCaseResult {
        name: name.to_string(),
        passed,
        details,
        duration_ms: started.elapsed().as_millis() as u64,
    }
}

async fn persist_run(pool: &SqlitePool, run: &SelfTestRun, actor: &str) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO harness_self_test_runs
         (id, suite, status, started_at, finished_at, created_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(&run.id)
    .bind(&run.suite)
    .bind(run.status.as_str())
    .bind(run.started_at_unix)
    .bind(run.finished_at_unix)
    .bind(actor)
    .execute(&mut *tx)
    .await?;
    for (index, case) in run.cases.iter().enumerate() {
        sqlx::query(
            "INSERT INTO harness_self_test_case_results
             (run_id, case_index, name, passed, details, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(&run.id)
        .bind(i64::try_from(index).context("too many self-test cases")?)
        .bind(&case.name)
        .bind(i64::from(case.passed))
        .bind(&case.details)
        .bind(i64::try_from(case.duration_ms).context("self-test duration exceeds sqlite range")?)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
