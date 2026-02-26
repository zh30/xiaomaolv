use super::{
    GroupDecisionKind, GroupSignalInput, McpCommandAccess, PrivateChatAccess,
    TELEGRAM_MAX_TEXT_CHARS, TelegramCommandSettings, TelegramGroupTriggerMode,
    TelegramGroupUserProfile, TelegramReplyMessage, TelegramSchedulerSettings,
    TelegramSlashCommand, TelegramUser, build_draft_message_id,
    build_group_member_identity_context, detect_scheduler_job_operation_keyword,
    evaluate_group_decision, evaluate_mcp_command_access, evaluate_private_chat_access,
    extract_dynamic_alias_candidates, extract_realtime_name_correction, extract_scheduler_job_id,
    is_reply_to_bot_message, looks_like_scheduler_list_text, message_mentions_bot,
    parse_admin_user_ids, parse_telegram_slash_command, render_telegram_text_parts,
    short_description_payload, telegram_help_text, telegram_registered_commands,
    telegram_session_id, text_implies_latest_target, truncate_chars, typing_action_payload,
};

fn test_scheduler_settings() -> TelegramSchedulerSettings {
    TelegramSchedulerSettings {
        enabled: true,
        tick_secs: 2,
        batch_size: 8,
        lease_secs: 30,
        default_timezone: "Asia/Shanghai".to_string(),
        nl_enabled: true,
        nl_min_confidence: 0.78,
        require_confirm: true,
        max_jobs_per_owner: 64,
    }
}

#[test]
fn think_block_is_stripped_and_only_body_is_sent() {
    let raw = "<think>内部推理A\n内部推理B</think>\n\n最终回答";
    let parts = render_telegram_text_parts(raw, TELEGRAM_MAX_TEXT_CHARS);
    assert_eq!(parts.len(), 1);
    let rendered = &parts[0];
    assert_eq!(rendered.parse_mode, Some("MarkdownV2"));
    assert_eq!(rendered.text, "最终回答");
}

#[test]
fn plain_text_keeps_original_mode() {
    let raw = "你好，这里是正文。";
    let parts = render_telegram_text_parts(raw, TELEGRAM_MAX_TEXT_CHARS);
    let rendered = &parts[0];
    assert_eq!(rendered.parse_mode, Some("MarkdownV2"));
    assert_eq!(rendered.text, raw);
}

#[test]
fn markdownv2_renderer_supports_common_markdown_features() {
    let raw = "# 标题\n\n**加粗** `code` [链接](https://example.com)\n- 条目";
    let parts = render_telegram_text_parts(raw, TELEGRAM_MAX_TEXT_CHARS);
    assert_eq!(parts.len(), 1);
    let rendered = &parts[0];
    assert_eq!(rendered.parse_mode, Some("MarkdownV2"));
    assert!(rendered.text.contains("*标题*"));
    assert!(rendered.text.contains("*加粗*"));
    assert!(rendered.text.contains("`code`"));
    assert!(rendered.text.contains("[链接](https://example.com)"));
    assert!(rendered.text.contains("• 条目"));
}

#[test]
fn markdownv2_renderer_escapes_special_chars() {
    let raw = "a+b=c.";
    let parts = render_telegram_text_parts(raw, TELEGRAM_MAX_TEXT_CHARS);
    assert_eq!(parts.len(), 1);
    let rendered = &parts[0];
    assert_eq!(rendered.parse_mode, Some("MarkdownV2"));
    assert_eq!(rendered.text, "a\\+b\\=c\\.");
}

#[test]
fn unclosed_think_is_suppressed() {
    let raw = "<think>这段还没闭合";
    let parts = render_telegram_text_parts(raw, TELEGRAM_MAX_TEXT_CHARS);
    assert!(parts.is_empty());
}

#[test]
fn draft_message_id_format_is_stable() {
    let draft_id = build_draft_message_id(-100123, Some(88));
    assert!(draft_id.starts_with("xm-"));
    assert!(draft_id.contains("-100123-88-"));
    assert!(draft_id.len() < 128);
}

#[test]
fn typing_payload_includes_action_and_chat_id() {
    let payload = typing_action_payload(123, None);
    assert_eq!(payload["chat_id"], 123);
    assert_eq!(payload["action"], "typing");
    assert!(payload.get("message_thread_id").is_none());
}

#[test]
fn typing_payload_includes_thread_id_when_present() {
    let payload = typing_action_payload(-100, Some(7));
    assert_eq!(payload["chat_id"], -100);
    assert_eq!(payload["action"], "typing");
    assert_eq!(payload["message_thread_id"], 7);
}

#[test]
fn short_description_payload_has_expected_shape() {
    let payload = short_description_payload("online");
    assert_eq!(payload["short_description"], "online");
}

#[test]
fn truncate_chars_respects_limit() {
    assert_eq!(truncate_chars("abcdef", 3), "abc".to_string());
    assert_eq!(truncate_chars("你好世界", 2), "你好".to_string());
}

#[test]
fn mention_detection_matches_username_boundaries() {
    assert!(message_mentions_bot(
        "hello @xiaomaolv_bot",
        "xiaomaolv_bot"
    ));
    assert!(message_mentions_bot(
        "@xiaomaolv_bot，请回复",
        "xiaomaolv_bot"
    ));
    assert!(!message_mentions_bot(
        "not-mention@xiaomaolv_botx",
        "xiaomaolv_bot"
    ));
    assert!(!message_mentions_bot("hello @other_bot", "xiaomaolv_bot"));
}

#[test]
fn reply_to_bot_detection_uses_reply_from_is_bot() {
    let reply_from_bot = TelegramReplyMessage {
        message_id: 1,
        from: Some(TelegramUser {
            id: 42,
            is_bot: Some(true),
            username: None,
            first_name: None,
            last_name: None,
        }),
    };
    let reply_from_user = TelegramReplyMessage {
        message_id: 2,
        from: Some(TelegramUser {
            id: 7,
            is_bot: Some(false),
            username: None,
            first_name: None,
            last_name: None,
        }),
    };
    let reply_without_from = TelegramReplyMessage {
        message_id: 3,
        from: None,
    };

    assert!(is_reply_to_bot_message(Some(&reply_from_bot), None));
    assert!(!is_reply_to_bot_message(Some(&reply_from_user), None));
    assert!(!is_reply_to_bot_message(Some(&reply_without_from), None));
    assert!(!is_reply_to_bot_message(None, None));
}

#[test]
fn reply_to_bot_detection_falls_back_to_bot_user_id_when_is_bot_missing() {
    let reply_without_flag = TelegramReplyMessage {
        message_id: 7,
        from: Some(TelegramUser {
            id: 12345,
            is_bot: None,
            username: None,
            first_name: None,
            last_name: None,
        }),
    };
    assert!(is_reply_to_bot_message(
        Some(&reply_without_flag),
        Some(12345)
    ));
    assert!(!is_reply_to_bot_message(
        Some(&reply_without_flag),
        Some(99999)
    ));
}

#[test]
fn extract_dynamic_alias_candidates_learns_prefix_name() {
    let aliases =
        extract_dynamic_alias_candidates("小绿，@myxiaomaolvbot 你看看这个", "myxiaomaolvbot");
    assert!(aliases.iter().any(|v| v == "小绿"));
}

#[test]
fn extract_realtime_name_correction_supports_jiaowo_pattern() {
    let corrected = extract_realtime_name_correction("请叫我阿青");
    assert_eq!(corrected, Some("阿青".to_string()));
}

#[test]
fn extract_realtime_name_correction_supports_wojiao_pattern() {
    let corrected = extract_realtime_name_correction("我叫小陈");
    assert_eq!(corrected, Some("小陈".to_string()));
}

#[test]
fn extract_realtime_name_correction_ignores_non_name_sentence() {
    let corrected = extract_realtime_name_correction("我是来问一个问题的");
    assert_eq!(corrected, None);
}

#[test]
fn group_member_identity_context_includes_sender_and_roster_mapping() {
    let sender_profile = TelegramGroupUserProfile {
        preferred_name: "阿青".to_string(),
        username: Some("aqing_99".to_string()),
        updated_at: 1700000000,
    };
    let roster = vec![
        (
            1001_i64,
            TelegramGroupUserProfile {
                preferred_name: "阿青".to_string(),
                username: Some("aqing_99".to_string()),
                updated_at: 1700000000,
            },
        ),
        (
            1002_i64,
            TelegramGroupUserProfile {
                preferred_name: "小米".to_string(),
                username: Some("xiaomi".to_string()),
                updated_at: 1699999999,
            },
        ),
    ];

    let context = build_group_member_identity_context(
        "我们今天要评审需求",
        1001,
        &sender_profile,
        &roster,
        6,
    );

    assert!(context.contains("当前发言账号: uid=1001"));
    assert!(context.contains("当前发言称呼: 阿青"));
    assert!(context.contains("uid=1001 -> 阿青"));
    assert!(context.contains("uid=1002 -> 小米"));
    assert!(context.contains("原始消息: 我们今天要评审需求"));
}

#[test]
fn telegram_session_id_uses_thread_then_reply_then_chat() {
    assert_eq!(
        telegram_session_id(-1001, Some(88), Some(9)),
        "tg:-1001:thread:88".to_string()
    );
    assert_eq!(
        telegram_session_id(-1001, None, Some(9)),
        "tg:-1001:reply:9".to_string()
    );
    assert_eq!(
        telegram_session_id(-1001, None, None),
        "tg:-1001".to_string()
    );
}

#[test]
fn long_plain_text_is_split_into_multiple_messages() {
    let raw = "a".repeat(TELEGRAM_MAX_TEXT_CHARS + 37);
    let parts = render_telegram_text_parts(&raw, TELEGRAM_MAX_TEXT_CHARS);
    assert!(parts.len() >= 2);
    for part in &parts {
        assert!(part.text.chars().count() <= TELEGRAM_MAX_TEXT_CHARS);
        assert_eq!(part.parse_mode, Some("MarkdownV2"));
    }
}

#[test]
fn long_think_text_only_keeps_final_body() {
    let think = "推理".repeat(TELEGRAM_MAX_TEXT_CHARS);
    let raw = format!("<think>{think}</think>\n\n最终结论");
    let parts = render_telegram_text_parts(&raw, TELEGRAM_MAX_TEXT_CHARS);
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].parse_mode, Some("MarkdownV2"));
    assert_eq!(parts[0].text, "最终结论");
    for part in &parts {
        assert!(part.text.chars().count() <= TELEGRAM_MAX_TEXT_CHARS);
    }
}

#[test]
fn parse_slash_command_supports_bot_username_suffix() {
    let parsed = parse_telegram_slash_command(
        "/mcp@xiaomaolv_bot ls --scope merged",
        Some("xiaomaolv_bot"),
    );
    assert_eq!(
        parsed,
        Some(TelegramSlashCommand::Mcp {
            tail: "ls --scope merged".to_string()
        })
    );
}

#[test]
fn parse_slash_command_ignores_other_bot_username() {
    let parsed = parse_telegram_slash_command("/mcp@other_bot ls", Some("xiaomaolv_bot"));
    assert_eq!(parsed, None);
}

#[test]
fn parse_slash_command_supports_whoami() {
    let parsed = parse_telegram_slash_command("/whoami", Some("xiaomaolv_bot"));
    assert_eq!(parsed, Some(TelegramSlashCommand::WhoAmI));
}

#[test]
fn parse_slash_command_supports_task() {
    let parsed = parse_telegram_slash_command(
        "/task add 2026-03-01T09:00:00+08:00 | 会议提醒",
        Some("xiaomaolv_bot"),
    );
    assert_eq!(
        parsed,
        Some(TelegramSlashCommand::Task {
            tail: "add 2026-03-01T09:00:00+08:00 | 会议提醒".to_string()
        })
    );
}

#[test]
fn parse_slash_command_supports_skill_alias() {
    let parsed = parse_telegram_slash_command("/skill ls --scope merged", Some("xiaomaolv_bot"));
    assert_eq!(
        parsed,
        Some(TelegramSlashCommand::Skills {
            tail: "ls --scope merged".to_string()
        })
    );
}

#[test]
fn telegram_help_and_registered_commands_include_task_and_skills() {
    let help = telegram_help_text();
    assert!(help.contains("/task"));
    assert!(help.contains("/skills"));
    let commands = telegram_registered_commands();
    assert!(commands.iter().any(|(name, _)| *name == "task"));
    assert!(commands.iter().any(|(name, _)| *name == "skills"));
}

#[test]
fn parse_task_schedule_and_payload_supports_pipe_format() {
    let (schedule, payload) =
        super::parse_task_schedule_and_payload("2026-03-01 09:00 | 提醒开会").expect("parse");
    assert_eq!(schedule, "2026-03-01 09:00");
    assert_eq!(payload, "提醒开会");
}

#[test]
fn parse_scheduler_once_time_supports_default_timezone_local_format() {
    let unix =
        super::parse_scheduler_once_time("2026-03-01 09:00", "Asia/Shanghai").expect("parse time");
    assert!(unix > 0);
}

#[test]
fn detect_scheduler_management_keywords_and_job_id() {
    let text = "请帮我暂停任务 task-100-200-300";
    let op = detect_scheduler_job_operation_keyword(text).expect("op");
    assert_eq!(op, super::SchedulerJobOperation::Pause);
    let job_id = extract_scheduler_job_id(text).expect("job id");
    assert_eq!(job_id, "task-100-200-300");
}

#[test]
fn looks_like_scheduler_list_text_supports_cn_and_en() {
    assert!(looks_like_scheduler_list_text("帮我看看任务列表"));
    assert!(looks_like_scheduler_list_text("list tasks"));
}

#[test]
fn text_implies_latest_target_supports_cn_and_en() {
    assert!(text_implies_latest_target("暂停最近那个任务"));
    assert!(text_implies_latest_target("delete latest task"));
}

#[test]
fn parse_admin_user_ids_supports_csv() {
    let ids = parse_admin_user_ids("123, 456 ,789").expect("ids");
    assert_eq!(ids, vec![123, 456, 789]);
}

#[test]
fn mcp_command_access_requires_private_chat_first() {
    let settings = TelegramCommandSettings {
        enabled: true,
        private_only: true,
        admin_user_ids: vec![42],
        scheduler: test_scheduler_settings(),
    };
    let access = evaluate_mcp_command_access(false, Some(42), &settings);
    assert_eq!(access, McpCommandAccess::RequirePrivateChat);
}

#[test]
fn mcp_command_access_rejects_non_admin() {
    let settings = TelegramCommandSettings {
        enabled: true,
        private_only: true,
        admin_user_ids: vec![42],
        scheduler: test_scheduler_settings(),
    };
    let access = evaluate_mcp_command_access(true, Some(7), &settings);
    assert_eq!(access, McpCommandAccess::Unauthorized);
}

#[test]
fn private_chat_access_allows_admin() {
    let access = evaluate_private_chat_access(Some(42), &[42, 99]);
    assert_eq!(access, PrivateChatAccess::Allowed);
}

#[test]
fn private_chat_access_rejects_non_admin() {
    let access = evaluate_private_chat_access(Some(7), &[42, 99]);
    assert_eq!(access, PrivateChatAccess::Unauthorized);
}

#[test]
fn private_chat_access_rejects_when_allowlist_missing() {
    let access = evaluate_private_chat_access(Some(7), &[]);
    assert_eq!(access, PrivateChatAccess::MissingAdminAllowlist);
}

#[test]
fn group_decision_strict_only_allows_mention_or_reply() {
    let decision = evaluate_group_decision(
        TelegramGroupTriggerMode::Strict,
        &GroupSignalInput {
            mentioned: false,
            replied_to_bot: false,
            recent_bot_participation: true,
            alias_hit: true,
            has_question_marker: true,
            points_to_other_bot: false,
            low_signal_noise: false,
            cooldown_active: false,
        },
        70,
    );
    assert_eq!(decision.kind, GroupDecisionKind::Ignore);
}

#[test]
fn group_decision_strict_allows_explicit_mention() {
    let decision = evaluate_group_decision(
        TelegramGroupTriggerMode::Strict,
        &GroupSignalInput {
            mentioned: true,
            replied_to_bot: false,
            recent_bot_participation: false,
            alias_hit: false,
            has_question_marker: false,
            points_to_other_bot: false,
            low_signal_noise: false,
            cooldown_active: true,
        },
        70,
    );
    assert_eq!(decision.kind, GroupDecisionKind::Respond);
}

#[test]
fn group_decision_strict_allows_reply_to_bot() {
    let decision = evaluate_group_decision(
        TelegramGroupTriggerMode::Strict,
        &GroupSignalInput {
            mentioned: false,
            replied_to_bot: true,
            recent_bot_participation: false,
            alias_hit: false,
            has_question_marker: false,
            points_to_other_bot: false,
            low_signal_noise: false,
            cooldown_active: true,
        },
        70,
    );
    assert_eq!(decision.kind, GroupDecisionKind::Respond);
}

#[test]
fn group_decision_smart_promotes_contextual_question_to_observe_or_respond() {
    let decision = evaluate_group_decision(
        TelegramGroupTriggerMode::Smart,
        &GroupSignalInput {
            mentioned: false,
            replied_to_bot: false,
            recent_bot_participation: true,
            alias_hit: true,
            has_question_marker: true,
            points_to_other_bot: false,
            low_signal_noise: false,
            cooldown_active: false,
        },
        70,
    );
    assert_eq!(decision.kind, GroupDecisionKind::Respond);
}

#[test]
fn group_decision_smart_suppresses_other_bot_target() {
    let decision = evaluate_group_decision(
        TelegramGroupTriggerMode::Smart,
        &GroupSignalInput {
            mentioned: false,
            replied_to_bot: false,
            recent_bot_participation: false,
            alias_hit: false,
            has_question_marker: true,
            points_to_other_bot: true,
            low_signal_noise: false,
            cooldown_active: false,
        },
        70,
    );
    assert_eq!(decision.kind, GroupDecisionKind::Ignore);
}
