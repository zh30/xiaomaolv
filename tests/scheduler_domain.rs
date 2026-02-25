use xiaomaolv::scheduler::{ScheduleSpec, compute_next_run_at_unix, compute_retry_backoff_secs};

#[test]
fn scheduler_domain_once_returns_none_when_expired() {
    let now = 1_800_000_000_i64;
    let next = compute_next_run_at_unix(
        &ScheduleSpec::Once {
            run_at_unix: now - 1,
        },
        now,
        "Asia/Shanghai",
    )
    .expect("compute once");
    assert_eq!(next, None);
}

#[test]
fn scheduler_domain_once_returns_original_when_future() {
    let now = 1_800_000_000_i64;
    let run_at = now + 120;
    let next = compute_next_run_at_unix(
        &ScheduleSpec::Once {
            run_at_unix: run_at,
        },
        now,
        "UTC",
    )
    .expect("compute once");
    assert_eq!(next, Some(run_at));
}

#[test]
fn scheduler_domain_cron_computes_future_fire_time() {
    let now = 1_800_000_000_i64;
    let next = compute_next_run_at_unix(
        &ScheduleSpec::Cron {
            expr: "0 9 * * *".to_string(),
        },
        now,
        "Asia/Shanghai",
    )
    .expect("compute cron")
    .expect("next exists");
    assert!(next > now);
}

#[test]
fn scheduler_domain_retry_backoff_is_capped() {
    assert_eq!(compute_retry_backoff_secs(0), 1);
    assert_eq!(compute_retry_backoff_secs(1), 2);
    assert_eq!(compute_retry_backoff_secs(5), 32);
    assert_eq!(compute_retry_backoff_secs(16), 300);
}
