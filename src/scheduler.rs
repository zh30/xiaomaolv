use std::str::FromStr;

use anyhow::{Context, bail};
use chrono::{Duration, LocalResult, TimeZone, Utc};
use chrono_tz::Tz;
use cron::Schedule;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleSpec {
    Once { run_at_unix: i64 },
    Cron { expr: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchedulerPolicy {
    #[serde(default = "default_scheduler_policy_version_string")]
    pub version: String,
    pub trigger: SchedulerPolicyTrigger,
    #[serde(default)]
    pub execution: SchedulerExecutionPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SchedulerPolicyTrigger {
    Once { run_at_unix: i64, timezone: String },
    Cron { expr: String, timezone: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchedulerExecutionPolicy {
    #[serde(default = "default_max_failure_streak_before_pause")]
    pub max_failure_streak_before_pause: u32,
    #[serde(default)]
    pub backoff: SchedulerBackoffPolicy,
}

impl Default for SchedulerExecutionPolicy {
    fn default() -> Self {
        Self {
            max_failure_streak_before_pause: default_max_failure_streak_before_pause(),
            backoff: SchedulerBackoffPolicy::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchedulerBackoffPolicy {
    #[serde(default = "default_backoff_strategy_string")]
    pub strategy: String,
    #[serde(default = "default_backoff_base_secs")]
    pub base_secs: i64,
    #[serde(default = "default_backoff_max_secs")]
    pub max_secs: i64,
}

impl Default for SchedulerBackoffPolicy {
    fn default() -> Self {
        Self {
            strategy: default_backoff_strategy().to_string(),
            base_secs: default_backoff_base_secs(),
            max_secs: default_backoff_max_secs(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerExecutionState {
    Active,
    Paused,
    Completed,
    Canceled,
}

impl SchedulerExecutionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Canceled => "canceled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerExecutionEvent {
    ManualPause,
    ManualResume,
    ManualCancel,
    RunSucceededKeepActive,
    RunSucceededComplete,
    RunFailedRetry,
    RunFailedPause,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerSuccessPlan {
    pub next_state: SchedulerExecutionState,
    pub mark_completed: bool,
    pub next_run_at_unix: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerFailurePlan {
    pub next_state: SchedulerExecutionState,
    pub pause_job: bool,
    pub next_run_at_unix: Option<i64>,
    pub backoff_secs: Option<i64>,
}

pub fn default_scheduler_policy(trigger: SchedulerPolicyTrigger) -> SchedulerPolicy {
    SchedulerPolicy {
        version: default_scheduler_policy_version().to_string(),
        trigger,
        execution: SchedulerExecutionPolicy::default(),
    }
}

pub fn scheduler_policy_json_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "SchedulerPolicy",
        "type": "object",
        "additionalProperties": false,
        "required": ["version", "trigger", "execution"],
        "properties": {
            "version": {
                "type": "string",
                "description": "Schema version for policy compatibility.",
                "enum": ["1.0"]
            },
            "trigger": {
                "oneOf": [
                    {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["kind", "run_at_unix", "timezone"],
                        "properties": {
                            "kind": { "const": "once" },
                            "run_at_unix": {
                                "type": "integer",
                                "description": "Unix timestamp in seconds."
                            },
                            "timezone": {
                                "type": "string",
                                "description": "IANA timezone name."
                            }
                        }
                    },
                    {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["kind", "expr", "timezone"],
                        "properties": {
                            "kind": { "const": "cron" },
                            "expr": {
                                "type": "string",
                                "description": "Cron expression (5 or 6 fields)."
                            },
                            "timezone": {
                                "type": "string",
                                "description": "IANA timezone name."
                            }
                        }
                    }
                ]
            },
            "execution": {
                "type": "object",
                "additionalProperties": false,
                "required": ["max_failure_streak_before_pause", "backoff"],
                "properties": {
                    "max_failure_streak_before_pause": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 1024
                    },
                    "backoff": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["strategy", "base_secs", "max_secs"],
                        "properties": {
                            "strategy": {
                                "type": "string",
                                "enum": ["exponential", "fixed"]
                            },
                            "base_secs": {
                                "type": "integer",
                                "minimum": 1
                            },
                            "max_secs": {
                                "type": "integer",
                                "minimum": 1
                            }
                        }
                    }
                }
            }
        }
    })
}

pub fn parse_scheduler_policy_json(raw: &str) -> anyhow::Result<SchedulerPolicy> {
    let mut policy: SchedulerPolicy =
        serde_json::from_str(raw).context("failed to decode scheduler policy json")?;
    normalize_policy(&mut policy);
    validate_scheduler_policy(&policy)?;
    Ok(policy)
}

pub fn encode_scheduler_policy_json(policy: &SchedulerPolicy) -> anyhow::Result<String> {
    validate_scheduler_policy(policy)?;
    serde_json::to_string(policy).context("failed to encode scheduler policy json")
}

pub fn validate_scheduler_policy(policy: &SchedulerPolicy) -> anyhow::Result<()> {
    if policy.version.trim() != default_scheduler_policy_version() {
        bail!(
            "unsupported scheduler policy version '{}', expected {}",
            policy.version,
            default_scheduler_policy_version()
        );
    }
    if policy.execution.max_failure_streak_before_pause == 0 {
        bail!("execution.max_failure_streak_before_pause must be >= 1");
    }
    if policy.execution.max_failure_streak_before_pause > 1024 {
        bail!("execution.max_failure_streak_before_pause must be <= 1024");
    }
    validate_backoff_policy(&policy.execution.backoff)?;
    validate_policy_trigger(&policy.trigger)?;
    Ok(())
}

pub fn compute_next_run_from_policy(
    policy: &SchedulerPolicy,
    now_unix: i64,
) -> anyhow::Result<Option<i64>> {
    validate_scheduler_policy(policy)?;
    match &policy.trigger {
        SchedulerPolicyTrigger::Once {
            run_at_unix,
            timezone,
        } => compute_next_run_at_unix(
            &ScheduleSpec::Once {
                run_at_unix: *run_at_unix,
            },
            now_unix,
            timezone,
        ),
        SchedulerPolicyTrigger::Cron { expr, timezone } => compute_next_run_at_unix(
            &ScheduleSpec::Cron { expr: expr.clone() },
            now_unix,
            timezone,
        ),
    }
}

pub fn compute_retry_backoff_secs_with_policy(
    policy: &SchedulerBackoffPolicy,
    attempt: u32,
) -> i64 {
    let normalized = normalize_backoff_policy(policy.clone());
    let base = normalized.base_secs.max(1);
    let max = normalized.max_secs.max(base);
    match normalized.strategy.as_str() {
        "fixed" => base.clamp(1, max),
        _ => {
            if attempt == 0 {
                return 1_i64.clamp(1, max);
            }
            let exp = 2_i64.saturating_pow(attempt.min(20));
            base.saturating_mul(exp).clamp(1, max)
        }
    }
}

pub fn compute_retry_backoff_secs(attempt: u32) -> i64 {
    let policy = SchedulerBackoffPolicy::default();
    compute_retry_backoff_secs_with_policy(&policy, attempt)
}

pub fn apply_scheduler_state_transition(
    current: SchedulerExecutionState,
    event: SchedulerExecutionEvent,
) -> anyhow::Result<SchedulerExecutionState> {
    let next = match (current, event) {
        (SchedulerExecutionState::Active, SchedulerExecutionEvent::ManualPause) => {
            SchedulerExecutionState::Paused
        }
        (SchedulerExecutionState::Paused, SchedulerExecutionEvent::ManualResume) => {
            SchedulerExecutionState::Active
        }
        (SchedulerExecutionState::Active, SchedulerExecutionEvent::ManualCancel)
        | (SchedulerExecutionState::Paused, SchedulerExecutionEvent::ManualCancel) => {
            SchedulerExecutionState::Canceled
        }
        (SchedulerExecutionState::Active, SchedulerExecutionEvent::RunSucceededKeepActive) => {
            SchedulerExecutionState::Active
        }
        (SchedulerExecutionState::Active, SchedulerExecutionEvent::RunSucceededComplete) => {
            SchedulerExecutionState::Completed
        }
        (SchedulerExecutionState::Active, SchedulerExecutionEvent::RunFailedRetry) => {
            SchedulerExecutionState::Active
        }
        (SchedulerExecutionState::Active, SchedulerExecutionEvent::RunFailedPause) => {
            SchedulerExecutionState::Paused
        }
        _ => {
            bail!(
                "invalid scheduler transition {} + {:?}",
                current.as_str(),
                event
            );
        }
    };
    Ok(next)
}

pub fn plan_success_transition(
    policy: &SchedulerPolicy,
    now_unix: i64,
    run_count_after: i64,
    max_runs: Option<i64>,
) -> anyhow::Result<SchedulerSuccessPlan> {
    validate_scheduler_policy(policy)?;
    let max_runs_reached = max_runs.map(|max| run_count_after >= max).unwrap_or(false);
    let once = matches!(policy.trigger, SchedulerPolicyTrigger::Once { .. });
    if once || max_runs_reached {
        let next_state = apply_scheduler_state_transition(
            SchedulerExecutionState::Active,
            SchedulerExecutionEvent::RunSucceededComplete,
        )?;
        return Ok(SchedulerSuccessPlan {
            next_state,
            mark_completed: true,
            next_run_at_unix: None,
        });
    }
    let next_run_at_unix = compute_next_run_from_policy(policy, now_unix)?;
    if next_run_at_unix.is_none() {
        let next_state = apply_scheduler_state_transition(
            SchedulerExecutionState::Active,
            SchedulerExecutionEvent::RunSucceededComplete,
        )?;
        return Ok(SchedulerSuccessPlan {
            next_state,
            mark_completed: true,
            next_run_at_unix: None,
        });
    }
    let next_state = apply_scheduler_state_transition(
        SchedulerExecutionState::Active,
        SchedulerExecutionEvent::RunSucceededKeepActive,
    )?;
    Ok(SchedulerSuccessPlan {
        next_state,
        mark_completed: false,
        next_run_at_unix,
    })
}

pub fn plan_failure_transition(
    policy: &SchedulerPolicy,
    now_unix: i64,
    failure_streak_after: i64,
) -> anyhow::Result<SchedulerFailurePlan> {
    validate_scheduler_policy(policy)?;
    let threshold = policy.execution.max_failure_streak_before_pause.max(1) as i64;
    if failure_streak_after >= threshold {
        let next_state = apply_scheduler_state_transition(
            SchedulerExecutionState::Active,
            SchedulerExecutionEvent::RunFailedPause,
        )?;
        return Ok(SchedulerFailurePlan {
            next_state,
            pause_job: true,
            next_run_at_unix: None,
            backoff_secs: None,
        });
    }
    let backoff_secs = compute_retry_backoff_secs_with_policy(
        &policy.execution.backoff,
        failure_streak_after.max(0) as u32,
    )
    .max(1);
    let next_state = apply_scheduler_state_transition(
        SchedulerExecutionState::Active,
        SchedulerExecutionEvent::RunFailedRetry,
    )?;
    Ok(SchedulerFailurePlan {
        next_state,
        pause_job: false,
        next_run_at_unix: Some(now_unix + backoff_secs),
        backoff_secs: Some(backoff_secs),
    })
}

pub fn compute_next_run_at_unix(
    spec: &ScheduleSpec,
    now_unix: i64,
    timezone: &str,
) -> anyhow::Result<Option<i64>> {
    let tz: Tz = timezone
        .parse()
        .with_context(|| format!("invalid timezone '{timezone}'"))?;

    match spec {
        ScheduleSpec::Once { run_at_unix } => {
            if *run_at_unix <= now_unix {
                Ok(None)
            } else {
                Ok(Some(*run_at_unix))
            }
        }
        ScheduleSpec::Cron { expr } => {
            let normalized_expr = normalize_cron_expr(expr);
            let schedule = Schedule::from_str(&normalized_expr)
                .with_context(|| format!("invalid cron expression '{expr}'"))?;
            let base = match tz.timestamp_opt(now_unix, 0) {
                LocalResult::Single(v) => v,
                _ => bail!("invalid local timestamp for timezone '{timezone}'"),
            };
            let start = base + Duration::seconds(1);
            Ok(schedule
                .after(&start)
                .next()
                .map(|dt| dt.with_timezone(&Utc).timestamp()))
        }
    }
}

fn default_scheduler_policy_version() -> &'static str {
    "1.0"
}

fn default_scheduler_policy_version_string() -> String {
    default_scheduler_policy_version().to_string()
}

fn default_max_failure_streak_before_pause() -> u32 {
    8
}

fn default_backoff_strategy() -> &'static str {
    "exponential"
}

fn default_backoff_strategy_string() -> String {
    default_backoff_strategy().to_string()
}

fn default_backoff_base_secs() -> i64 {
    1
}

fn default_backoff_max_secs() -> i64 {
    300
}

fn normalize_policy(policy: &mut SchedulerPolicy) {
    if policy.version.trim().is_empty() {
        policy.version = default_scheduler_policy_version().to_string();
    } else {
        policy.version = policy.version.trim().to_string();
    }
    policy.execution.backoff = normalize_backoff_policy(policy.execution.backoff.clone());
    match &mut policy.trigger {
        SchedulerPolicyTrigger::Once { timezone, .. } => {
            *timezone = timezone.trim().to_string();
        }
        SchedulerPolicyTrigger::Cron { expr, timezone } => {
            *expr = expr.trim().to_string();
            *timezone = timezone.trim().to_string();
        }
    }
}

fn normalize_backoff_policy(mut policy: SchedulerBackoffPolicy) -> SchedulerBackoffPolicy {
    policy.strategy = if policy.strategy.trim().is_empty() {
        default_backoff_strategy().to_string()
    } else {
        policy.strategy.trim().to_ascii_lowercase()
    };
    policy.base_secs = policy.base_secs.max(1);
    if policy.max_secs < policy.base_secs {
        policy.max_secs = policy.base_secs;
    }
    policy
}

fn validate_policy_trigger(trigger: &SchedulerPolicyTrigger) -> anyhow::Result<()> {
    match trigger {
        SchedulerPolicyTrigger::Once {
            run_at_unix,
            timezone,
        } => {
            if *run_at_unix <= 0 {
                bail!("trigger.run_at_unix must be positive");
            }
            let _: Tz = timezone
                .parse()
                .with_context(|| format!("invalid timezone '{timezone}'"))?;
        }
        SchedulerPolicyTrigger::Cron { expr, timezone } => {
            if expr.trim().is_empty() {
                bail!("trigger.expr cannot be empty");
            }
            let _: Tz = timezone
                .parse()
                .with_context(|| format!("invalid timezone '{timezone}'"))?;
            let _ = Schedule::from_str(&normalize_cron_expr(expr))
                .with_context(|| format!("invalid cron expression '{expr}'"))?;
        }
    }
    Ok(())
}

fn validate_backoff_policy(policy: &SchedulerBackoffPolicy) -> anyhow::Result<()> {
    let strategy = policy.strategy.trim().to_ascii_lowercase();
    if !matches!(strategy.as_str(), "exponential" | "fixed") {
        bail!(
            "unsupported backoff strategy '{}', expected exponential|fixed",
            policy.strategy
        );
    }
    if policy.base_secs <= 0 {
        bail!("backoff.base_secs must be >= 1");
    }
    if policy.max_secs <= 0 {
        bail!("backoff.max_secs must be >= 1");
    }
    if policy.max_secs < policy.base_secs {
        bail!("backoff.max_secs must be >= backoff.base_secs");
    }
    Ok(())
}

fn normalize_cron_expr(raw: &str) -> String {
    let trimmed = raw.trim();
    let fields = trimmed.split_whitespace().count();
    if fields == 5 {
        format!("0 {trimmed}")
    } else {
        trimmed.to_string()
    }
}
