use xiaomaolv::memory::{
    CompleteTelegramSchedulerJobRunRequest, CreateTelegramSchedulerJobRequest, SqliteMemoryStore,
    TelegramSchedulerJobStatus, TelegramSchedulerScheduleKind, TelegramSchedulerTaskKind,
    UpsertTelegramSchedulerPendingIntentRequest,
};

#[tokio::test]
async fn scheduler_store_create_list_and_status_update() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("init store");

    store
        .create_telegram_scheduler_job(CreateTelegramSchedulerJobRequest {
            job_id: "job-1".to_string(),
            channel: "telegram".to_string(),
            chat_id: -10001,
            message_thread_id: None,
            owner_user_id: 42,
            status: TelegramSchedulerJobStatus::Active,
            task_kind: TelegramSchedulerTaskKind::Reminder,
            payload: "提醒我开会".to_string(),
            schedule_kind: TelegramSchedulerScheduleKind::Once,
            timezone: "Asia/Shanghai".to_string(),
            run_at_unix: Some(1_900_000_000),
            cron_expr: None,
            policy_json: None,
            next_run_at_unix: Some(1_900_000_000),
            max_runs: Some(1),
        })
        .await
        .expect("create job");

    let jobs = store
        .list_telegram_scheduler_jobs_by_owner("telegram", -10001, 42, 20)
        .await
        .expect("list jobs");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].job_id, "job-1");
    assert_eq!(jobs[0].status, TelegramSchedulerJobStatus::Active);

    let paused = store
        .update_telegram_scheduler_job_status(
            "telegram",
            -10001,
            42,
            "job-1",
            TelegramSchedulerJobStatus::Paused,
        )
        .await
        .expect("pause job");
    assert!(paused);

    let resumed = store
        .update_telegram_scheduler_job_status(
            "telegram",
            -10001,
            42,
            "job-1",
            TelegramSchedulerJobStatus::Active,
        )
        .await
        .expect("resume job");
    assert!(resumed);

    let canceled = store
        .update_telegram_scheduler_job_status(
            "telegram",
            -10001,
            42,
            "job-1",
            TelegramSchedulerJobStatus::Canceled,
        )
        .await
        .expect("cancel job");
    assert!(canceled);
}

#[tokio::test]
async fn scheduler_store_claims_due_jobs_and_completes_once_job() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("init store");
    let now = 1_800_000_000_i64;

    store
        .create_telegram_scheduler_job(CreateTelegramSchedulerJobRequest {
            job_id: "due-once".to_string(),
            channel: "telegram".to_string(),
            chat_id: -10008,
            message_thread_id: Some(99),
            owner_user_id: 101,
            status: TelegramSchedulerJobStatus::Active,
            task_kind: TelegramSchedulerTaskKind::Reminder,
            payload: "到点提醒".to_string(),
            schedule_kind: TelegramSchedulerScheduleKind::Once,
            timezone: "Asia/Shanghai".to_string(),
            run_at_unix: Some(now - 10),
            cron_expr: None,
            policy_json: None,
            next_run_at_unix: Some(now - 10),
            max_runs: Some(1),
        })
        .await
        .expect("create due job");

    store
        .create_telegram_scheduler_job(CreateTelegramSchedulerJobRequest {
            job_id: "future".to_string(),
            channel: "telegram".to_string(),
            chat_id: -10008,
            message_thread_id: None,
            owner_user_id: 101,
            status: TelegramSchedulerJobStatus::Active,
            task_kind: TelegramSchedulerTaskKind::Reminder,
            payload: "未来提醒".to_string(),
            schedule_kind: TelegramSchedulerScheduleKind::Once,
            timezone: "Asia/Shanghai".to_string(),
            run_at_unix: Some(now + 600),
            cron_expr: None,
            policy_json: None,
            next_run_at_unix: Some(now + 600),
            max_runs: Some(1),
        })
        .await
        .expect("create future job");

    let lease_token = "lease-abc";
    let claimed = store
        .claim_due_telegram_scheduler_jobs("telegram", now, 10, 30, lease_token)
        .await
        .expect("claim due jobs");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].job_id, "due-once");

    store
        .complete_telegram_scheduler_job_run(CompleteTelegramSchedulerJobRunRequest {
            channel: "telegram".to_string(),
            chat_id: claimed[0].chat_id,
            job_id: claimed[0].job_id.clone(),
            lease_token: lease_token.to_string(),
            started_at_unix: now,
            finished_at_unix: now + 1,
            next_run_at_unix: None,
            mark_completed: true,
        })
        .await
        .expect("complete run");

    let done = store
        .load_telegram_scheduler_job("telegram", "due-once")
        .await
        .expect("load done job")
        .expect("done job exists");
    assert_eq!(done.status, TelegramSchedulerJobStatus::Completed);
    assert_eq!(done.run_count, 1);
    assert!(done.lease_token.is_none());

    let future = store
        .load_telegram_scheduler_job("telegram", "future")
        .await
        .expect("load future job")
        .expect("future job exists");
    assert_eq!(future.status, TelegramSchedulerJobStatus::Active);
    assert_eq!(future.next_run_at_unix, Some(now + 600));
}

#[tokio::test]
async fn scheduler_store_upserts_and_clears_pending_intent() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("init store");
    let now = 1_800_000_000_i64;

    store
        .upsert_telegram_scheduler_pending_intent(UpsertTelegramSchedulerPendingIntentRequest {
            intent_id: "pending:telegram:-10001:42".to_string(),
            channel: "telegram".to_string(),
            chat_id: -10001,
            owner_user_id: 42,
            draft_json: r#"{"schedule_kind":"once"}"#.to_string(),
            expires_at_unix: now + 600,
        })
        .await
        .expect("upsert pending");

    let loaded = store
        .load_telegram_scheduler_pending_intent("telegram", -10001, 42, now)
        .await
        .expect("load pending")
        .expect("pending exists");
    assert_eq!(loaded.owner_user_id, 42);
    assert!(loaded.draft_json.contains("once"));

    let deleted = store
        .delete_telegram_scheduler_pending_intent("telegram", -10001, 42)
        .await
        .expect("delete pending");
    assert!(deleted);
}

#[tokio::test]
async fn scheduler_store_queries_stats_with_due_jobs() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("init store");
    let now = 1_800_000_000_i64;

    let create =
        |job_id: &str, status: TelegramSchedulerJobStatus, next_run_at_unix: Option<i64>| {
            CreateTelegramSchedulerJobRequest {
                job_id: job_id.to_string(),
                channel: "telegram".to_string(),
                chat_id: -10066,
                message_thread_id: None,
                owner_user_id: 9527,
                status,
                task_kind: TelegramSchedulerTaskKind::Reminder,
                payload: format!("payload-{job_id}"),
                schedule_kind: TelegramSchedulerScheduleKind::Once,
                timezone: "Asia/Shanghai".to_string(),
                run_at_unix: next_run_at_unix,
                cron_expr: None,
                policy_json: None,
                next_run_at_unix,
                max_runs: Some(1),
            }
        };

    store
        .create_telegram_scheduler_job(create(
            "active-due",
            TelegramSchedulerJobStatus::Active,
            Some(now - 5),
        ))
        .await
        .expect("create active due");
    store
        .create_telegram_scheduler_job(create(
            "active-future",
            TelegramSchedulerJobStatus::Active,
            Some(now + 3600),
        ))
        .await
        .expect("create active future");
    store
        .create_telegram_scheduler_job(create(
            "paused",
            TelegramSchedulerJobStatus::Paused,
            Some(now - 10),
        ))
        .await
        .expect("create paused");
    store
        .create_telegram_scheduler_job(create(
            "completed",
            TelegramSchedulerJobStatus::Completed,
            None,
        ))
        .await
        .expect("create completed");
    store
        .create_telegram_scheduler_job(create(
            "canceled",
            TelegramSchedulerJobStatus::Canceled,
            None,
        ))
        .await
        .expect("create canceled");

    let stats = store
        .query_telegram_scheduler_stats("telegram", now)
        .await
        .expect("query stats");
    assert_eq!(stats.jobs_total, 5);
    assert_eq!(stats.jobs_active, 2);
    assert_eq!(stats.jobs_paused, 1);
    assert_eq!(stats.jobs_completed, 1);
    assert_eq!(stats.jobs_canceled, 1);
    assert_eq!(stats.jobs_due, 1);
}
