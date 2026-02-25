use std::str::FromStr;

use anyhow::{Context, bail};
use chrono::{Duration, LocalResult, TimeZone, Utc};
use chrono_tz::Tz;
use cron::Schedule;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleSpec {
    Once { run_at_unix: i64 },
    Cron { expr: String },
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

pub fn compute_retry_backoff_secs(attempt: u32) -> i64 {
    if attempt == 0 {
        return 1;
    }
    let exp = 2_i64.saturating_pow(attempt.min(20));
    exp.clamp(1, 300)
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
