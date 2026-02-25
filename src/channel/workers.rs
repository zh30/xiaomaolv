use anyhow::{Context, bail};
use std::time::{SystemTime, UNIX_EPOCH};

use super::*;

pub(super) async fn run_telegram_scheduler_loop(
    channel_name: String,
    service: Arc<MessageService>,
    sender: TelegramSender,
    scheduler_settings: TelegramSchedulerSettings,
    mut shutdown: watch::Receiver<bool>,
    scheduler_diag_state: Arc<tokio::sync::Mutex<TelegramSchedulerDiagState>>,
) {
    info!(
        channel = %channel_name,
        tick_secs = scheduler_settings.tick_secs,
        batch_size = scheduler_settings.batch_size,
        lease_secs = scheduler_settings.lease_secs,
        "telegram scheduler worker started"
    );
    {
        let mut diag = scheduler_diag_state.lock().await;
        diag.worker_started_at_unix = Some(current_unix_timestamp());
    }

    loop {
        if *shutdown.borrow() {
            break;
        }

        let now_unix = current_unix_timestamp_i64();
        let lease_token = format!(
            "lease-{}-{}-{}",
            channel_name,
            now_unix,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );

        let claimed = match service
            .claim_due_telegram_scheduler_jobs(
                channel_name.clone(),
                now_unix,
                scheduler_settings.batch_size,
                scheduler_settings.lease_secs as i64,
                lease_token.clone(),
            )
            .await
        {
            Ok(items) => items,
            Err(err) => {
                let err_text = format!("{err:#}");
                {
                    let mut diag = scheduler_diag_state.lock().await;
                    diag.last_tick_at_unix = Some(current_unix_timestamp());
                    diag.last_error = Some(err_text.clone());
                    diag.total_runs_err += 1;
                    diag.last_run_err_at_unix = Some(current_unix_timestamp());
                }
                warn!(
                    channel = %channel_name,
                    error = %err_text,
                    "telegram scheduler claim failed"
                );
                if wait_for_shutdown_or_timeout(
                    &mut shutdown,
                    Duration::from_secs(scheduler_settings.tick_secs),
                )
                .await
                {
                    break;
                }
                continue;
            }
        };

        {
            let mut diag = scheduler_diag_state.lock().await;
            diag.last_tick_at_unix = Some(current_unix_timestamp());
            diag.last_claimed_jobs = claimed.len();
            diag.total_claimed_jobs += claimed.len() as u64;
        }

        match service
            .query_telegram_scheduler_stats(channel_name.clone(), now_unix)
            .await
        {
            Ok(stats) => {
                let mut diag = scheduler_diag_state.lock().await;
                diag.jobs_total = stats.jobs_total;
                diag.jobs_active = stats.jobs_active;
                diag.jobs_paused = stats.jobs_paused;
                diag.jobs_completed = stats.jobs_completed;
                diag.jobs_canceled = stats.jobs_canceled;
                diag.jobs_due = stats.jobs_due;
            }
            Err(err) => {
                let err_text = format!("{err:#}");
                {
                    let mut diag = scheduler_diag_state.lock().await;
                    diag.last_error = Some(err_text.clone());
                }
                warn!(
                    channel = %channel_name,
                    error = %err_text,
                    "telegram scheduler stats query failed"
                );
            }
        }

        for job in claimed {
            let started_at_unix = current_unix_timestamp_i64();
            let execution: anyhow::Result<()> = async {
                let policy =
                    resolve_scheduler_policy_for_job(&job, &scheduler_settings.default_timezone)?;
                let outbound_text = match job.task_kind {
                    TelegramSchedulerTaskKind::Reminder => job.payload.clone(),
                    TelegramSchedulerTaskKind::Agent => {
                        let session_id = format!("tg:{}:scheduler:{}", job.chat_id, job.job_id);
                        let user_id = format!("scheduler:{}", job.owner_user_id);
                        let reply = service
                            .handle(IncomingMessage {
                                channel: channel_name.clone(),
                                session_id,
                                user_id,
                                text: job.payload.clone(),
                                reply_target: Some(ReplyTarget::Telegram {
                                    chat_id: job.chat_id,
                                    message_thread_id: job.message_thread_id,
                                }),
                            })
                            .await
                            .context("failed to generate scheduler agent output")?;
                        reply.text
                    }
                };

                sender
                    .send_message(job.chat_id, job.message_thread_id, None, &outbound_text)
                    .await
                    .context("failed to send scheduler telegram message")?;

                let finished_at_unix = current_unix_timestamp_i64();
                let run_count_after = job.run_count.saturating_add(1);
                let success_plan = plan_success_transition(
                    &policy,
                    finished_at_unix,
                    run_count_after,
                    job.max_runs,
                )
                .context("failed to plan scheduler success transition")?;

                service
                    .complete_telegram_scheduler_job_run(CompleteTelegramSchedulerJobRunRequest {
                        channel: channel_name.clone(),
                        chat_id: job.chat_id,
                        job_id: job.job_id.clone(),
                        lease_token: lease_token.clone(),
                        started_at_unix,
                        finished_at_unix,
                        next_run_at_unix: success_plan.next_run_at_unix,
                        mark_completed: success_plan.mark_completed,
                    })
                    .await
                    .context("failed to mark scheduler job success")?;
                Ok(())
            }
            .await;

            match execution {
                Ok(()) => {
                    let mut diag = scheduler_diag_state.lock().await;
                    diag.total_runs_ok += 1;
                    diag.last_run_ok_at_unix = Some(current_unix_timestamp());
                    diag.last_error = None;
                }
                Err(err) => {
                    let err_text = format!("{err:#}");
                    let finished_at_unix = current_unix_timestamp_i64();
                    let failure_plan = match resolve_scheduler_policy_for_job(
                        &job,
                        &scheduler_settings.default_timezone,
                    )
                    .and_then(|policy| {
                        let failure_streak_after = job.failure_streak.saturating_add(1);
                        plan_failure_transition(&policy, finished_at_unix, failure_streak_after)
                    }) {
                        Ok(plan) => plan,
                        Err(plan_err) => {
                            let attempt = (job.failure_streak.max(0) as u32).saturating_add(1);
                            let backoff = compute_retry_backoff_secs(attempt);
                            warn!(
                                channel = %channel_name,
                                job_id = %job.job_id,
                                error = %format!("{plan_err:#}"),
                                "scheduler policy invalid, fallback to legacy retry policy"
                            );
                            if attempt >= 8 {
                                crate::scheduler::SchedulerFailurePlan {
                                    next_state: SchedulerExecutionState::Paused,
                                    pause_job: true,
                                    next_run_at_unix: None,
                                    backoff_secs: None,
                                }
                            } else {
                                crate::scheduler::SchedulerFailurePlan {
                                    next_state: SchedulerExecutionState::Active,
                                    pause_job: false,
                                    next_run_at_unix: Some(finished_at_unix + backoff),
                                    backoff_secs: Some(backoff),
                                }
                            }
                        }
                    };
                    let pause_job =
                        matches!(failure_plan.next_state, SchedulerExecutionState::Paused)
                            || failure_plan.pause_job;
                    let next_run_at_unix = if pause_job {
                        None
                    } else {
                        failure_plan.next_run_at_unix
                    };

                    if let Err(mark_err) = service
                        .fail_telegram_scheduler_job_run(FailTelegramSchedulerJobRunRequest {
                            channel: channel_name.clone(),
                            chat_id: job.chat_id,
                            job_id: job.job_id.clone(),
                            lease_token: lease_token.clone(),
                            started_at_unix,
                            finished_at_unix,
                            next_run_at_unix,
                            pause_job,
                            error: err_text.clone(),
                        })
                        .await
                    {
                        warn!(
                            channel = %channel_name,
                            job_id = %job.job_id,
                            error = %format!("{mark_err:#}"),
                            "failed to mark scheduler job failure"
                        );
                    }

                    {
                        let mut diag = scheduler_diag_state.lock().await;
                        diag.total_runs_err += 1;
                        diag.last_run_err_at_unix = Some(current_unix_timestamp());
                        diag.last_error = Some(err_text.clone());
                    }
                    warn!(
                        channel = %channel_name,
                        job_id = %job.job_id,
                        error = %err_text,
                        "telegram scheduler job execution failed"
                    );
                }
            }
        }

        if wait_for_shutdown_or_timeout(
            &mut shutdown,
            Duration::from_secs(scheduler_settings.tick_secs),
        )
        .await
        {
            break;
        }
    }

    info!(channel = %channel_name, "telegram scheduler worker stopped");
}

pub(super) async fn fetch_telegram_updates(
    client: &Client,
    bot_token: &str,
    offset: i64,
    timeout_secs: u64,
) -> anyhow::Result<Vec<TelegramPollUpdate>> {
    let url = format!("https://api.telegram.org/bot{bot_token}/getUpdates");
    let response = client
        .post(url)
        .json(&serde_json::json!({
            "offset": offset,
            "timeout": timeout_secs,
            "allowed_updates": ["message"]
        }))
        .send()
        .await
        .context("failed to call telegram getUpdates")?;

    let status = response.status();
    let raw = response
        .bytes()
        .await
        .context("failed to read telegram getUpdates payload")?;
    if !status.is_success() {
        let raw_text = String::from_utf8_lossy(&raw);
        let preview = truncate_chars(raw_text.trim(), 800);
        bail!("telegram getUpdates returned error: http_status={status} body={preview}");
    }

    let body: TelegramApiResponse<Vec<TelegramPollUpdate>> =
        serde_json::from_slice(&raw).context("failed to decode telegram getUpdates payload")?;

    if !body.ok {
        let retry_after = body.parameters.as_ref().and_then(|p| p.retry_after);
        let description = body
            .description
            .unwrap_or_else(|| "unknown error".to_string());
        if let Some(seconds) = retry_after {
            bail!("telegram getUpdates returned ok=false: {description} (retry_after={seconds}s)");
        }
        bail!("telegram getUpdates returned ok=false: {}", description);
    }

    Ok(body.result)
}

pub(super) async fn delete_telegram_webhook(
    client: &Client,
    bot_token: &str,
) -> anyhow::Result<()> {
    let url = format!("https://api.telegram.org/bot{bot_token}/deleteWebhook");
    client
        .post(url)
        .json(&serde_json::json!({ "drop_pending_updates": false }))
        .send()
        .await
        .context("failed to call telegram deleteWebhook")?
        .error_for_status()
        .context("telegram deleteWebhook returned error")?;
    Ok(())
}

pub(super) async fn wait_for_shutdown_or_timeout(
    shutdown: &mut watch::Receiver<bool>,
    timeout: Duration,
) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(timeout) => false,
        changed = shutdown.changed() => {
            if changed.is_err() {
                true
            } else {
                *shutdown.borrow()
            }
        }
    }
}
