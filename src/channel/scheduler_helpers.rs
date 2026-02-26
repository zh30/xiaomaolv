use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;

use crate::memory::{
    CreateTelegramSchedulerJobRequest, TelegramSchedulerJobRecord, TelegramSchedulerJobStatus,
    TelegramSchedulerScheduleKind, TelegramSchedulerTaskKind,
};
use crate::scheduler::{
    ScheduleSpec, SchedulerPolicy, SchedulerPolicyTrigger, compute_next_run_at_unix,
    default_scheduler_policy, encode_scheduler_policy_json, parse_scheduler_policy_json,
};

use super::{
    ChannelContext, SchedulerJobOperation, TelegramSchedulerIntent, TelegramSchedulerIntentDraft,
    TelegramSchedulerSettings, current_unix_timestamp_i64, truncate_chars,
};

pub(super) fn describe_scheduler_draft(draft: &TelegramSchedulerIntentDraft) -> String {
    let schedule = match draft.schedule_kind {
        TelegramSchedulerScheduleKind::Once => {
            format!(
                "一次性 @ {}",
                format_scheduler_time(draft.run_at_unix, &draft.timezone)
            )
        }
        TelegramSchedulerScheduleKind::Cron => format!(
            "周期 cron({}) next @ {}",
            draft.cron_expr.clone().unwrap_or_else(|| "-".to_string()),
            format_scheduler_time(draft.run_at_unix, &draft.timezone)
        ),
    };
    format!(
        "类型: {}\n时区: {}\n内容: {}\n原始: {}",
        schedule, draft.timezone, draft.payload, draft.source_text
    )
}

pub(super) fn is_scheduler_confirm_text(text: &str) -> bool {
    matches!(
        text.trim().to_ascii_lowercase().as_str(),
        "确认" | "确定" | "yes" | "y" | "ok" | "okay" | "confirm"
    )
}

pub(super) fn is_scheduler_cancel_text(text: &str) -> bool {
    matches!(
        text.trim().to_ascii_lowercase().as_str(),
        "取消" | "不用了" | "算了" | "no" | "n" | "cancel" | "stop"
    )
}

pub(super) fn looks_like_scheduler_list_text(text: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    let zh_hit = ["任务列表", "查看任务", "列出任务", "有哪些任务", "看看任务"]
        .iter()
        .any(|k| text.contains(k));
    if zh_hit {
        return true;
    }
    ["list task", "list tasks", "show tasks", "show task"]
        .iter()
        .any(|k| lower.contains(k))
}

pub(super) fn detect_scheduler_job_operation_keyword(text: &str) -> Option<SchedulerJobOperation> {
    let lower = text.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    let pause_hit = ["暂停任务", "停用任务", "pause task", "pause任务"]
        .iter()
        .any(|k| text.contains(k) || lower.contains(k));
    if pause_hit {
        return Some(SchedulerJobOperation::Pause);
    }
    let resume_hit = [
        "恢复任务",
        "继续任务",
        "启用任务",
        "resume task",
        "start task",
    ]
    .iter()
    .any(|k| text.contains(k) || lower.contains(k));
    if resume_hit {
        return Some(SchedulerJobOperation::Resume);
    }
    let delete_hit = [
        "删除任务",
        "删掉任务",
        "取消任务",
        "delete task",
        "remove task",
        "cancel task",
    ]
    .iter()
    .any(|k| text.contains(k) || lower.contains(k));
    if delete_hit {
        return Some(SchedulerJobOperation::Delete);
    }
    None
}

pub(super) fn extract_scheduler_job_id(text: &str) -> Option<String> {
    if let Some(found) = normalize_scheduler_job_id(text) {
        return Some(found);
    }
    for (start, _) in text.match_indices("task-") {
        let mut end = start;
        for (idx, ch) in text[start..].char_indices() {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':') {
                end = start + idx + ch.len_utf8();
            } else {
                break;
            }
        }
        if end > start {
            let candidate = &text[start..end];
            if let Some(normalized) = normalize_scheduler_job_id(candidate) {
                return Some(normalized);
            }
        }
    }
    None
}

pub(super) fn normalize_scheduler_job_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_matches(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '`' | '"' | '\'' | ',' | '，' | '.' | '。' | ';' | '；' | ')' | '(' | '[' | ']'
            )
    });
    if !trimmed.starts_with("task-") || trimmed.len() <= 5 {
        return None;
    }
    if !trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':'))
    {
        return None;
    }
    Some(trimmed.to_string())
}

pub(super) fn resolve_scheduler_job_operation_from_intent(
    intent: &TelegramSchedulerIntent,
) -> Option<SchedulerJobOperation> {
    if let Some(op) = intent.job_operation.as_deref() {
        match op {
            "pause" => return Some(SchedulerJobOperation::Pause),
            "resume" => return Some(SchedulerJobOperation::Resume),
            "delete" => return Some(SchedulerJobOperation::Delete),
            _ => {}
        }
    }
    match intent.action.as_str() {
        "pause" => Some(SchedulerJobOperation::Pause),
        "resume" => Some(SchedulerJobOperation::Resume),
        "delete" => Some(SchedulerJobOperation::Delete),
        _ => None,
    }
}

pub(super) fn text_implies_latest_target(text: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    [
        "最新",
        "最近",
        "刚刚",
        "上一个",
        "上次",
        "latest",
        "recent",
        "last",
    ]
    .iter()
    .any(|k| text.contains(k) || lower.contains(k))
}

pub(super) fn format_scheduler_operation_hint(op: SchedulerJobOperation) -> &'static str {
    match op {
        SchedulerJobOperation::Pause => "pause",
        SchedulerJobOperation::Resume => "resume",
        SchedulerJobOperation::Delete => "del",
    }
}

pub(super) fn format_scheduler_operation_candidates(
    options: &[TelegramSchedulerJobRecord],
    timezone: &str,
) -> String {
    if options.is_empty() {
        return "-".to_string();
    }
    let mut lines = vec!["候选任务:".to_string()];
    for item in options.iter().take(5) {
        let schedule = match item.schedule_kind {
            TelegramSchedulerScheduleKind::Once => {
                format!(
                    "once@{}",
                    format_scheduler_time(item.next_run_at_unix, timezone)
                )
            }
            TelegramSchedulerScheduleKind::Cron => format!(
                "cron({}) next@{}",
                item.cron_expr.clone().unwrap_or_else(|| "-".to_string()),
                format_scheduler_time(item.next_run_at_unix, timezone)
            ),
        };
        lines.push(format!(
            "- {} [{}] {} | {}",
            item.job_id,
            item.status.as_str(),
            schedule,
            truncate_chars(item.payload.trim(), 60)
        ));
    }
    lines.join("\n")
}

pub(super) fn build_scheduler_draft_from_intent(
    intent: &TelegramSchedulerIntent,
    source_text: &str,
    default_timezone: &str,
    now_unix: i64,
) -> anyhow::Result<Option<TelegramSchedulerIntentDraft>> {
    let schedule_kind = match intent
        .schedule_kind
        .as_deref()
        .map(|v| v.trim().to_ascii_lowercase())
    {
        Some(v) if v == "cron" => TelegramSchedulerScheduleKind::Cron,
        Some(v) if v == "once" => TelegramSchedulerScheduleKind::Once,
        _ => return Ok(None),
    };
    let task_kind = match intent
        .task_kind
        .as_deref()
        .map(|v| v.trim().to_ascii_lowercase())
    {
        Some(v) if v == "agent" => TelegramSchedulerTaskKind::Agent,
        _ => TelegramSchedulerTaskKind::Reminder,
    };
    let payload = match intent.payload.as_deref().map(str::trim) {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => return Ok(None),
    };
    let timezone = intent
        .timezone
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(default_timezone)
        .to_string();

    let (run_at_unix, cron_expr) = match schedule_kind {
        TelegramSchedulerScheduleKind::Once => {
            let Some(run_at_raw) = intent.run_at.as_deref() else {
                return Ok(None);
            };
            let run_at_unix = parse_scheduler_once_time(run_at_raw, timezone.as_str())?;
            if run_at_unix <= now_unix {
                return Ok(None);
            }
            (Some(run_at_unix), None)
        }
        TelegramSchedulerScheduleKind::Cron => {
            let Some(expr) = intent.cron_expr.as_deref().map(str::trim) else {
                return Ok(None);
            };
            if expr.is_empty() {
                return Ok(None);
            }
            let next = compute_next_run_at_unix(
                &ScheduleSpec::Cron {
                    expr: expr.to_string(),
                },
                now_unix,
                timezone.as_str(),
            )?;
            let Some(next_run) = next else {
                return Ok(None);
            };
            (Some(next_run), Some(expr.to_string()))
        }
    };

    Ok(Some(TelegramSchedulerIntentDraft {
        task_kind,
        schedule_kind,
        payload,
        timezone,
        run_at_unix,
        cron_expr,
        source_text: source_text.trim().to_string(),
    }))
}

pub(super) fn build_scheduler_policy_from_draft(
    draft: &TelegramSchedulerIntentDraft,
) -> anyhow::Result<SchedulerPolicy> {
    let trigger = match draft.schedule_kind {
        TelegramSchedulerScheduleKind::Once => {
            let run_at_unix = draft.run_at_unix.context("once draft missing run_at")?;
            SchedulerPolicyTrigger::Once {
                run_at_unix,
                timezone: draft.timezone.clone(),
            }
        }
        TelegramSchedulerScheduleKind::Cron => {
            let expr = draft
                .cron_expr
                .clone()
                .context("cron draft missing cron_expr")?;
            SchedulerPolicyTrigger::Cron {
                expr,
                timezone: draft.timezone.clone(),
            }
        }
    };
    Ok(default_scheduler_policy(trigger))
}

pub(super) fn resolve_scheduler_policy_for_job(
    job: &TelegramSchedulerJobRecord,
    default_timezone: &str,
) -> anyhow::Result<SchedulerPolicy> {
    if let Some(raw) = job.policy_json.as_deref()
        && !raw.trim().is_empty()
    {
        return parse_scheduler_policy_json(raw);
    }

    let timezone = if job.timezone.trim().is_empty() {
        default_timezone.to_string()
    } else {
        job.timezone.clone()
    };
    let trigger = match job.schedule_kind {
        TelegramSchedulerScheduleKind::Once => {
            let run_at_unix = job
                .run_at_unix
                .or(job.next_run_at_unix)
                .context("once scheduler job missing run_at")?;
            SchedulerPolicyTrigger::Once {
                run_at_unix,
                timezone,
            }
        }
        TelegramSchedulerScheduleKind::Cron => {
            let expr = job
                .cron_expr
                .clone()
                .context("cron scheduler job missing cron_expr")?;
            SchedulerPolicyTrigger::Cron { expr, timezone }
        }
    };
    Ok(default_scheduler_policy(trigger))
}

pub(super) async fn create_scheduler_job_from_draft(
    ctx: &ChannelContext,
    chat_id: i64,
    message_thread_id: Option<i64>,
    owner_user_id: i64,
    scheduler_settings: &TelegramSchedulerSettings,
    draft: &TelegramSchedulerIntentDraft,
) -> anyhow::Result<(String, i64)> {
    enforce_scheduler_job_quota(
        ctx,
        chat_id,
        owner_user_id,
        scheduler_settings.max_jobs_per_owner,
    )
    .await?;

    let policy = build_scheduler_policy_from_draft(draft)?;
    let policy_json = encode_scheduler_policy_json(&policy)?;
    let next_run_at_unix = match draft.schedule_kind {
        TelegramSchedulerScheduleKind::Once => {
            draft.run_at_unix.context("once draft missing run_at")?
        }
        TelegramSchedulerScheduleKind::Cron => {
            let expr = draft
                .cron_expr
                .clone()
                .context("cron draft missing cron_expr")?;
            compute_next_run_at_unix(
                &ScheduleSpec::Cron { expr },
                current_unix_timestamp_i64(),
                draft.timezone.as_str(),
            )?
            .context("failed to compute next run time for cron draft")?
        }
    };
    let job_id = build_scheduler_job_id(chat_id, owner_user_id);

    ctx.service
        .create_telegram_scheduler_job(CreateTelegramSchedulerJobRequest {
            job_id: job_id.clone(),
            channel: ctx.channel_name.clone(),
            chat_id,
            message_thread_id,
            owner_user_id,
            status: TelegramSchedulerJobStatus::Active,
            task_kind: draft.task_kind,
            payload: draft.payload.clone(),
            schedule_kind: draft.schedule_kind,
            timezone: draft.timezone.clone(),
            run_at_unix: if matches!(draft.schedule_kind, TelegramSchedulerScheduleKind::Once) {
                Some(next_run_at_unix)
            } else {
                None
            },
            cron_expr: draft.cron_expr.clone(),
            policy_json: Some(policy_json),
            next_run_at_unix: Some(next_run_at_unix),
            max_runs: if matches!(draft.schedule_kind, TelegramSchedulerScheduleKind::Once) {
                Some(1)
            } else {
                None
            },
        })
        .await?;

    Ok((job_id, next_run_at_unix))
}

pub(super) async fn enforce_scheduler_job_quota(
    ctx: &ChannelContext,
    chat_id: i64,
    owner_user_id: i64,
    max_jobs_per_owner: usize,
) -> anyhow::Result<()> {
    let jobs = ctx
        .service
        .list_telegram_scheduler_jobs_by_owner(
            ctx.channel_name.clone(),
            chat_id,
            owner_user_id,
            max_jobs_per_owner.saturating_add(1),
        )
        .await?;
    let used = jobs
        .iter()
        .filter(|job| {
            matches!(
                job.status,
                TelegramSchedulerJobStatus::Active | TelegramSchedulerJobStatus::Paused
            )
        })
        .count();
    if used >= max_jobs_per_owner {
        bail!("任务数量已达上限({max_jobs_per_owner})，请先删除/暂停部分任务后再创建。");
    }
    Ok(())
}

pub(super) fn try_build_relative_reminder_draft_from_text(
    text: &str,
    default_timezone: &str,
    now_unix: i64,
) -> Option<TelegramSchedulerIntentDraft> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let delay_secs = parse_relative_delay_secs(trimmed)?;
    let payload = extract_relative_reminder_payload(trimmed)?;
    let run_at_unix = now_unix.saturating_add(delay_secs);

    Some(TelegramSchedulerIntentDraft {
        task_kind: TelegramSchedulerTaskKind::Reminder,
        schedule_kind: TelegramSchedulerScheduleKind::Once,
        payload,
        timezone: default_timezone.to_string(),
        run_at_unix: Some(run_at_unix),
        cron_expr: None,
        source_text: trimmed.to_string(),
    })
}

fn parse_relative_delay_secs(text: &str) -> Option<i64> {
    let mut cursor = 0usize;
    while cursor < text.len() {
        let mut digit_start = None;
        for (off, ch) in text[cursor..].char_indices() {
            if ch.is_ascii_digit() {
                digit_start = Some(cursor + off);
                break;
            }
        }
        let Some(start) = digit_start else {
            return None;
        };
        let mut end = start;
        for (off, ch) in text[start..].char_indices() {
            if ch.is_ascii_digit() {
                end = start + off + ch.len_utf8();
            } else {
                break;
            }
        }
        let value = text[start..end].parse::<i64>().ok()?;
        if value <= 0 {
            cursor = end;
            continue;
        }

        let rest = text[end..].trim_start();
        let Some((unit_secs, consumed)) = parse_duration_unit(rest) else {
            cursor = end;
            continue;
        };
        let after_unit = rest[consumed..].trim_start();
        let has_relative_suffix = after_unit.starts_with('后')
            || after_unit.starts_with("之后")
            || after_unit.starts_with("以后")
            || after_unit.starts_with("later")
            || after_unit.starts_with("from now")
            || after_unit.starts_with("提醒")
            || after_unit.starts_with("叫")
            || after_unit.starts_with("喊");
        if !has_relative_suffix {
            cursor = end;
            continue;
        }

        return Some(value.saturating_mul(unit_secs));
    }
    None
}

fn parse_duration_unit(text: &str) -> Option<(i64, usize)> {
    let unit = [
        ("minutes", 60),
        ("minute", 60),
        ("mins", 60),
        ("min", 60),
        ("hours", 3600),
        ("hour", 3600),
        ("days", 86400),
        ("day", 86400),
        ("分钟", 60),
        ("分", 60),
        ("小时", 3600),
        ("秒钟", 1),
        ("秒", 1),
        ("天", 86400),
    ];
    for (token, secs) in unit {
        if text.starts_with(token) {
            return Some((secs, token.len()));
        }
    }
    None
}

fn extract_relative_reminder_payload(text: &str) -> Option<String> {
    for marker in [
        "提醒一下我",
        "提醒下我",
        "提醒我",
        "提醒一下",
        "提醒下",
        "提醒",
        "叫一下我",
        "叫我",
        "叫一下",
        "叫",
        "喊我",
        "喊",
    ] {
        let Some(start) = text.find(marker) else {
            continue;
        };
        let tail = text[start + marker.len()..].trim_matches(|c: char| {
            c.is_whitespace() || matches!(c, ',' | '，' | '.' | '。' | '!' | '！' | '?' | '？' | ':' | '：' | ';' | '；')
        });
        if !tail.is_empty() {
            return Some(tail.to_string());
        }
        return Some("你设置的提醒".to_string());
    }
    None
}

pub(super) fn parse_task_schedule_and_payload(raw: &str) -> anyhow::Result<(String, String)> {
    let (left, right) = raw
        .split_once('|')
        .context("格式错误，缺少 `|` 分隔符。示例: /task add 2026-03-01 09:00 | 提醒我开会")?;
    let schedule = left.trim();
    let payload = right.trim();
    if schedule.is_empty() || payload.is_empty() {
        bail!("格式错误，时间和内容都不能为空。");
    }
    Ok((schedule.to_string(), payload.to_string()))
}

pub(super) fn parse_scheduler_once_time(raw: &str, default_timezone: &str) -> anyhow::Result<i64> {
    let input = raw.trim();
    if input.is_empty() {
        bail!("缺少执行时间。");
    }
    if let Ok(unix) = input.parse::<i64>() {
        return Ok(unix);
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(input) {
        return Ok(dt.timestamp());
    }

    let timezone = default_timezone
        .parse::<Tz>()
        .with_context(|| format!("invalid timezone '{default_timezone}'"))?;
    for fmt in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y/%m/%d %H:%M:%S",
        "%Y/%m/%d %H:%M",
    ] {
        if let Ok(local_naive) = NaiveDateTime::parse_from_str(input, fmt)
            && let Some(local_dt) = timezone.from_local_datetime(&local_naive).single()
        {
            return Ok(local_dt.with_timezone(&Utc).timestamp());
        }
    }

    bail!(
        "不支持的时间格式。支持: Unix 秒时间戳 / RFC3339 / YYYY-MM-DD HH:MM（默认时区 {}）",
        default_timezone
    );
}

pub(super) fn build_scheduler_job_id(chat_id: i64, owner_user_id: i64) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("task-{chat_id}-{owner_user_id}-{millis}")
}

pub(super) fn format_scheduler_time(unix: Option<i64>, timezone: &str) -> String {
    let Some(unix) = unix else {
        return "-".to_string();
    };
    let tz: Tz = timezone.parse().unwrap_or(chrono_tz::UTC);
    match tz.timestamp_opt(unix, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S %Z").to_string(),
        None => unix.to_string(),
    }
}

pub(super) fn telegram_task_list_text(
    jobs: &[TelegramSchedulerJobRecord],
    timezone: &str,
) -> String {
    if jobs.is_empty() {
        return "当前没有定时任务。".to_string();
    }
    let mut lines = vec!["你的定时任务:".to_string()];
    for job in jobs.iter().take(64) {
        let schedule_text = match job.schedule_kind {
            TelegramSchedulerScheduleKind::Once => {
                format!(
                    "once@{}",
                    format_scheduler_time(job.next_run_at_unix, timezone)
                )
            }
            TelegramSchedulerScheduleKind::Cron => {
                format!(
                    "cron({}) next@{}",
                    job.cron_expr.clone().unwrap_or_else(|| "-".to_string()),
                    format_scheduler_time(job.next_run_at_unix, timezone)
                )
            }
        };
        lines.push(format!(
            "- {} [{}] {} | {}",
            job.job_id,
            job.status.as_str(),
            schedule_text,
            truncate_chars(job.payload.trim(), 80)
        ));
    }
    lines.join("\n")
}
