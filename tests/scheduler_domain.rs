use xiaomaolv::scheduler::{
    ScheduleSpec, SchedulerBackoffPolicy, SchedulerExecutionEvent, SchedulerExecutionState,
    SchedulerPolicyTrigger, apply_scheduler_state_transition, compute_next_run_at_unix,
    compute_retry_backoff_secs, compute_retry_backoff_secs_with_policy, default_scheduler_policy,
    encode_scheduler_policy_json, parse_scheduler_policy_json, plan_failure_transition,
    plan_success_transition, scheduler_policy_json_schema,
};

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

#[test]
fn scheduler_policy_schema_has_core_sections() {
    let schema = scheduler_policy_json_schema();
    assert_eq!(schema["type"], "object");
    assert!(schema["properties"]["trigger"].is_object());
    assert!(schema["properties"]["execution"].is_object());
}

#[test]
fn scheduler_policy_roundtrip_and_plans_work() {
    let policy = default_scheduler_policy(SchedulerPolicyTrigger::Cron {
        expr: "0 */2 * * *".to_string(),
        timezone: "Asia/Shanghai".to_string(),
    });
    let encoded = encode_scheduler_policy_json(&policy).expect("encode");
    let parsed = parse_scheduler_policy_json(&encoded).expect("parse");
    assert_eq!(parsed.version, "1.0");

    let now = 1_800_000_000_i64;
    let success_plan = plan_success_transition(&parsed, now, 1, None).expect("success plan");
    assert!(!success_plan.mark_completed);
    assert_eq!(success_plan.next_state, SchedulerExecutionState::Active);
    assert!(success_plan.next_run_at_unix.is_some());

    let failure_plan = plan_failure_transition(&parsed, now, 1).expect("failure plan");
    assert!(!failure_plan.pause_job);
    assert_eq!(failure_plan.next_state, SchedulerExecutionState::Active);
    assert!(failure_plan.next_run_at_unix.is_some());
}

#[test]
fn scheduler_policy_failure_plan_pauses_when_threshold_reached() {
    let policy_json = r#"{
      "version":"1.0",
      "trigger":{"kind":"cron","expr":"0 9 * * *","timezone":"Asia/Shanghai"},
      "execution":{
        "max_failure_streak_before_pause":2,
        "backoff":{"strategy":"fixed","base_secs":5,"max_secs":30}
      }
    }"#;
    let policy = parse_scheduler_policy_json(policy_json).expect("parse policy");
    let plan = plan_failure_transition(&policy, 1_800_000_000, 2).expect("plan");
    assert!(plan.pause_job);
    assert_eq!(plan.next_state, SchedulerExecutionState::Paused);
    assert!(plan.next_run_at_unix.is_none());
}

#[test]
fn scheduler_state_machine_rejects_invalid_resume_from_completed() {
    let result = apply_scheduler_state_transition(
        SchedulerExecutionState::Completed,
        SchedulerExecutionEvent::ManualResume,
    );
    assert!(result.is_err());
}

#[test]
fn scheduler_retry_backoff_policy_supports_fixed_strategy() {
    let policy = SchedulerBackoffPolicy {
        strategy: "fixed".to_string(),
        base_secs: 7,
        max_secs: 30,
    };
    assert_eq!(compute_retry_backoff_secs_with_policy(&policy, 1), 7);
    assert_eq!(compute_retry_backoff_secs_with_policy(&policy, 20), 7);
}
