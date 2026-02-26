use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TelegramSlashCommand {
    Start,
    Help,
    WhoAmI,
    Mcp { tail: String },
    Skills { tail: String },
    Task { tail: String },
    Unknown { name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum McpCommandAccess {
    Allowed,
    RequirePrivateChat,
    MissingAdminAllowlist,
    Unauthorized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PrivateChatAccess {
    Allowed,
    MissingAdminAllowlist,
    Unauthorized,
}

pub(super) async fn maybe_handle_telegram_command(
    ctx: &ChannelContext,
    sender: &TelegramSender,
    message: &TelegramMessage,
    text: &str,
    bot_username: Option<&str>,
    command_settings: &TelegramCommandSettings,
) -> anyhow::Result<bool> {
    let Some(command) = parse_telegram_slash_command(text, bot_username) else {
        return Ok(false);
    };
    let is_private_chat = is_private_chat_type(message.chat.kind.as_deref());

    match command {
        TelegramSlashCommand::Start => {
            send_telegram_command_reply(sender, message, &telegram_start_text()).await?;
            Ok(true)
        }
        TelegramSlashCommand::Help => {
            send_telegram_command_reply(sender, message, &telegram_help_text()).await?;
            Ok(true)
        }
        TelegramSlashCommand::WhoAmI => {
            send_telegram_command_reply(sender, message, &telegram_whoami_text(message)).await?;
            Ok(true)
        }
        TelegramSlashCommand::Mcp { tail } => {
            if !command_settings.enabled {
                send_telegram_command_reply(sender, message, "命令功能未启用。").await?;
                return Ok(true);
            }
            match evaluate_mcp_command_access(
                is_private_chat,
                message.from.as_ref().map(|u| u.id),
                command_settings,
            ) {
                McpCommandAccess::Allowed => {}
                McpCommandAccess::RequirePrivateChat => {
                    send_telegram_command_reply(sender, message, "请私聊使用 /mcp 管理命令。")
                        .await?;
                    return Ok(true);
                }
                McpCommandAccess::MissingAdminAllowlist => {
                    send_telegram_command_reply(
                        sender,
                        message,
                        &format!(
                            "管理员白名单未配置，命令不可用。\n{}\n\n请在 .env.realtest 中配置 TELEGRAM_ADMIN_USER_IDS，然后重启服务。",
                            telegram_whoami_hint(message)
                        ),
                    )
                    .await?;
                    return Ok(true);
                }
                McpCommandAccess::Unauthorized => {
                    send_telegram_command_reply(
                        sender,
                        message,
                        &format!(
                            "无权限执行该命令。\n{}\n\n请将你的 ID 加到 TELEGRAM_ADMIN_USER_IDS。",
                            telegram_whoami_hint(message)
                        ),
                    )
                    .await?;
                    return Ok(true);
                }
            }

            if tail.trim().is_empty() {
                send_telegram_command_reply(sender, message, &mcp_help_text()).await?;
                return Ok(true);
            }

            let parsed = match parse_telegram_mcp_command(&tail) {
                Ok(command) => command,
                Err(err) => {
                    send_telegram_command_reply(
                        sender,
                        message,
                        &format!("命令参数错误: {err}\n\n{}", mcp_help_text()),
                    )
                    .await?;
                    return Ok(true);
                }
            };

            let registry = match discover_mcp_registry() {
                Ok(registry) => registry,
                Err(err) => {
                    send_telegram_command_reply(
                        sender,
                        message,
                        &format!("无法加载 MCP 配置路径: {err}"),
                    )
                    .await?;
                    return Ok(true);
                }
            };

            match execute_mcp_command(&registry, parsed).await {
                Ok(output) => {
                    let mut out = if output.text.trim().is_empty() {
                        "命令执行完成。".to_string()
                    } else {
                        output.text
                    };
                    if output.reload_runtime {
                        match ctx
                            .service
                            .reload_mcp_runtime_from_registry(&registry)
                            .await
                        {
                            Ok(()) => {
                                out.push_str("\n\nMCP runtime 已热重载。");
                            }
                            Err(err) => {
                                out.push_str(&format!("\n\nMCP runtime 热重载失败: {err}"));
                            }
                        }
                    }
                    send_telegram_command_reply(sender, message, &out).await?;
                }
                Err(err) => {
                    send_telegram_command_reply(sender, message, &format!("命令执行失败: {err}"))
                        .await?;
                }
            }
            Ok(true)
        }
        TelegramSlashCommand::Skills { tail } => {
            if !command_settings.enabled {
                send_telegram_command_reply(sender, message, "命令功能未启用。").await?;
                return Ok(true);
            }
            match evaluate_mcp_command_access(
                is_private_chat,
                message.from.as_ref().map(|u| u.id),
                command_settings,
            ) {
                McpCommandAccess::Allowed => {}
                McpCommandAccess::RequirePrivateChat => {
                    send_telegram_command_reply(sender, message, "请私聊使用 /skills 管理命令。")
                        .await?;
                    return Ok(true);
                }
                McpCommandAccess::MissingAdminAllowlist => {
                    send_telegram_command_reply(
                        sender,
                        message,
                        &format!(
                            "管理员白名单未配置，命令不可用。\n{}\n\n请在 .env.realtest 中配置 TELEGRAM_ADMIN_USER_IDS，然后重启服务。",
                            telegram_whoami_hint(message)
                        ),
                    )
                    .await?;
                    return Ok(true);
                }
                McpCommandAccess::Unauthorized => {
                    send_telegram_command_reply(
                        sender,
                        message,
                        &format!(
                            "无权限执行该命令。\n{}\n\n请将你的 ID 加到 TELEGRAM_ADMIN_USER_IDS。",
                            telegram_whoami_hint(message)
                        ),
                    )
                    .await?;
                    return Ok(true);
                }
            }

            if tail.trim().is_empty() {
                send_telegram_command_reply(sender, message, &skills_help_text()).await?;
                return Ok(true);
            }

            let parsed = match parse_telegram_skills_command(&tail) {
                Ok(command) => command,
                Err(err) => {
                    send_telegram_command_reply(
                        sender,
                        message,
                        &format!("命令参数错误: {err}\n\n{}", skills_help_text()),
                    )
                    .await?;
                    return Ok(true);
                }
            };

            let registry = match discover_skill_registry() {
                Ok(registry) => registry,
                Err(err) => {
                    send_telegram_command_reply(
                        sender,
                        message,
                        &format!("无法加载 skills 配置路径: {err}"),
                    )
                    .await?;
                    return Ok(true);
                }
            };

            match execute_skills_command(&registry, parsed).await {
                Ok(output) => {
                    let mut out = if output.text.trim().is_empty() {
                        "命令执行完成。".to_string()
                    } else {
                        output.text
                    };
                    if output.reload_runtime {
                        match ctx
                            .service
                            .reload_skill_runtime_from_registry(&registry)
                            .await
                        {
                            Ok(()) => {
                                out.push_str("\n\nSkills runtime 已热重载。");
                            }
                            Err(err) => {
                                out.push_str(&format!("\n\nSkills runtime 热重载失败: {err}"));
                            }
                        }
                    }
                    send_telegram_command_reply(sender, message, &out).await?;
                }
                Err(err) => {
                    send_telegram_command_reply(sender, message, &format!("命令执行失败: {err}"))
                        .await?;
                }
            }
            Ok(true)
        }
        TelegramSlashCommand::Task { tail } => {
            handle_telegram_task_command(
                ctx,
                sender,
                message,
                tail.as_str(),
                is_private_chat,
                command_settings,
            )
            .await?;
            Ok(true)
        }
        TelegramSlashCommand::Unknown { name } => {
            if is_private_chat {
                send_telegram_command_reply(
                    sender,
                    message,
                    &format!("未知命令 /{name}。可用命令见 /help。"),
                )
                .await?;
                Ok(true)
            } else {
                Ok(false)
            }
        }
    }
}

pub(super) async fn handle_telegram_task_command(
    ctx: &ChannelContext,
    sender: &TelegramSender,
    message: &TelegramMessage,
    tail: &str,
    is_private_chat: bool,
    command_settings: &TelegramCommandSettings,
) -> anyhow::Result<()> {
    if !command_settings.scheduler.enabled {
        send_telegram_command_reply(sender, message, "定时任务功能未启用。").await?;
        return Ok(());
    }

    if !is_private_chat {
        send_telegram_command_reply(sender, message, "请私聊使用 /task 定时任务命令。").await?;
        return Ok(());
    }

    match evaluate_private_chat_access(
        message.from.as_ref().map(|u| u.id),
        &command_settings.admin_user_ids,
    ) {
        PrivateChatAccess::Allowed => {}
        PrivateChatAccess::MissingAdminAllowlist => {
            send_telegram_command_reply(
                sender,
                message,
                &format!(
                    "管理员白名单未配置，定时任务不可用。\n{}\n\n请在 .env.realtest 中配置 TELEGRAM_ADMIN_USER_IDS。",
                    telegram_whoami_hint(message)
                ),
            )
            .await?;
            return Ok(());
        }
        PrivateChatAccess::Unauthorized => {
            send_telegram_command_reply(
                sender,
                message,
                &format!(
                    "无权限管理定时任务。\n{}\n\n如需开通，请让管理员把你的 ID 加入 TELEGRAM_ADMIN_USER_IDS。",
                    telegram_whoami_hint(message)
                ),
            )
            .await?;
            return Ok(());
        }
    }

    let owner_user_id = message
        .from
        .as_ref()
        .map(|u| u.id)
        .unwrap_or(message.chat.id);
    let chat_id = message.chat.id;
    let tail = tail.trim();
    if tail.is_empty() {
        send_telegram_command_reply(
            sender,
            message,
            &telegram_task_help_text(&command_settings.scheduler.default_timezone),
        )
        .await?;
        return Ok(());
    }

    let mut tokens = tail.split_whitespace();
    let action = tokens
        .next()
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_default();
    let rest = tail[action.len()..].trim();

    match action.as_str() {
        "list" => {
            let jobs = ctx
                .service
                .list_telegram_scheduler_jobs_by_owner(
                    ctx.channel_name.clone(),
                    chat_id,
                    owner_user_id,
                    64,
                )
                .await?;
            let text = telegram_task_list_text(&jobs, &command_settings.scheduler.default_timezone);
            send_telegram_command_reply(sender, message, &text).await?;
        }
        "add" => {
            let (when_raw, payload) = parse_task_schedule_and_payload(rest)?;
            let run_at_unix = parse_scheduler_once_time(
                when_raw.as_str(),
                &command_settings.scheduler.default_timezone,
            )?;
            let now = current_unix_timestamp_i64();
            if run_at_unix <= now {
                send_telegram_command_reply(sender, message, "目标时间已过，请设置未来时间。")
                    .await?;
                return Ok(());
            }
            let draft = TelegramSchedulerIntentDraft {
                task_kind: TelegramSchedulerTaskKind::Reminder,
                schedule_kind: TelegramSchedulerScheduleKind::Once,
                payload: payload.to_string(),
                timezone: command_settings.scheduler.default_timezone.clone(),
                run_at_unix: Some(run_at_unix),
                cron_expr: None,
                source_text: tail.to_string(),
            };
            let (job_id, next_run_at_unix) = create_scheduler_job_from_draft(
                ctx,
                chat_id,
                message.message_thread_id,
                owner_user_id,
                &command_settings.scheduler,
                &draft,
            )
            .await?;
            let time_text = format_scheduler_time(
                Some(next_run_at_unix),
                &command_settings.scheduler.default_timezone,
            );
            send_telegram_command_reply(
                sender,
                message,
                &format!(
                    "已创建一次性任务。\nID: {job_id}\n执行时间: {time_text}\n内容: {}",
                    payload
                ),
            )
            .await?;
        }
        "every" => {
            let (cron_raw, payload) = parse_task_schedule_and_payload(rest)?;
            let draft = TelegramSchedulerIntentDraft {
                task_kind: TelegramSchedulerTaskKind::Reminder,
                schedule_kind: TelegramSchedulerScheduleKind::Cron,
                payload: payload.to_string(),
                timezone: command_settings.scheduler.default_timezone.clone(),
                run_at_unix: None,
                cron_expr: Some(cron_raw.to_string()),
                source_text: tail.to_string(),
            };
            let (job_id, next_run_at_unix) = create_scheduler_job_from_draft(
                ctx,
                chat_id,
                message.message_thread_id,
                owner_user_id,
                &command_settings.scheduler,
                &draft,
            )
            .await?;
            let next_time = format_scheduler_time(
                Some(next_run_at_unix),
                &command_settings.scheduler.default_timezone,
            );
            send_telegram_command_reply(
                sender,
                message,
                &format!(
                    "已创建周期任务。\nID: {job_id}\nCron: {cron_raw}\n下次执行: {next_time}\n内容: {}",
                    payload
                ),
            )
            .await?;
        }
        "pause" | "resume" | "del" => {
            let job_id = rest.trim();
            if job_id.is_empty() {
                send_telegram_command_reply(
                    sender,
                    message,
                    "缺少任务 ID。\n示例: /task pause <job_id>",
                )
                .await?;
                return Ok(());
            }
            let op = match action.as_str() {
                "pause" => SchedulerJobOperation::Pause,
                "resume" => SchedulerJobOperation::Resume,
                _ => SchedulerJobOperation::Delete,
            };
            let op_ctx = SchedulerJobOperationContext {
                ctx,
                sender,
                message,
                scheduler_settings: &command_settings.scheduler,
                chat_id,
                owner_user_id,
            };
            execute_scheduler_job_operation(&op_ctx, op, job_id, false).await?;
        }
        _ => {
            send_telegram_command_reply(
                sender,
                message,
                &format!(
                    "未知 task 子命令: {action}\n\n{}",
                    telegram_task_help_text(&command_settings.scheduler.default_timezone)
                ),
            )
            .await?;
        }
    }

    Ok(())
}

pub(super) async fn maybe_handle_telegram_scheduler_natural_language(
    ctx: &ChannelContext,
    sender: &TelegramSender,
    message: &TelegramMessage,
    text: &str,
    scheduler_settings: &TelegramSchedulerSettings,
) -> anyhow::Result<bool> {
    let owner_user_id = message
        .from
        .as_ref()
        .map(|u| u.id)
        .unwrap_or(message.chat.id);
    let chat_id = message.chat.id;
    let now_unix = current_unix_timestamp_i64();
    let channel_name = ctx.channel_name.clone();

    let pending = ctx
        .service
        .load_telegram_scheduler_pending_intent(
            channel_name.clone(),
            chat_id,
            owner_user_id,
            now_unix,
        )
        .await?;

    if let Some(pending) = pending {
        if is_scheduler_confirm_text(text) {
            let draft: TelegramSchedulerIntentDraft =
                match serde_json::from_str(pending.draft_json.as_str()) {
                    Ok(v) => v,
                    Err(err) => {
                        let _ = ctx
                            .service
                            .delete_telegram_scheduler_pending_intent(
                                channel_name.clone(),
                                chat_id,
                                owner_user_id,
                            )
                            .await;
                        send_telegram_command_reply(
                            sender,
                            message,
                            &format!("待确认任务草案已损坏，已清理。请重新描述任务。\n错误: {err}"),
                        )
                        .await?;
                        return Ok(true);
                    }
                };

            match create_scheduler_job_from_draft(
                ctx,
                chat_id,
                message.message_thread_id,
                owner_user_id,
                scheduler_settings,
                &draft,
            )
            .await
            {
                Ok((job_id, next_run_at_unix)) => {
                    let _ = ctx
                        .service
                        .delete_telegram_scheduler_pending_intent(
                            channel_name.clone(),
                            chat_id,
                            owner_user_id,
                        )
                        .await;
                    let next_time = format_scheduler_time(Some(next_run_at_unix), &draft.timezone);
                    send_telegram_command_reply(
                        sender,
                        message,
                        &format!(
                            "已确认并创建任务。\nID: {job_id}\n类型: {}\n下次执行: {next_time}\n内容: {}",
                            draft.schedule_kind.as_str(),
                            draft.payload
                        ),
                    )
                    .await?;
                }
                Err(err) => {
                    send_telegram_command_reply(
                        sender,
                        message,
                        &format!("确认失败: {err}\n你可以修改后重试，或回复“取消”丢弃草案。"),
                    )
                    .await?;
                }
            }
            return Ok(true);
        }

        if is_scheduler_cancel_text(text) {
            let _ = ctx
                .service
                .delete_telegram_scheduler_pending_intent(
                    channel_name.clone(),
                    chat_id,
                    owner_user_id,
                )
                .await;
            send_telegram_command_reply(sender, message, "已取消当前待确认定时任务。").await?;
            return Ok(true);
        }

        if try_handle_scheduler_management_text(
            ctx,
            sender,
            message,
            text,
            scheduler_settings,
            chat_id,
            owner_user_id,
        )
        .await?
        {
            return Ok(true);
        }

        if let Some(intent) = ctx
            .service
            .detect_telegram_scheduler_intent(
                text,
                scheduler_settings.default_timezone.as_str(),
                now_unix,
                Some(pending.draft_json.as_str()),
            )
            .await?
            && intent.confidence >= scheduler_settings.nl_min_confidence
            && matches!(intent.action.as_str(), "create" | "update")
            && let Some(updated_draft) = build_scheduler_draft_from_intent(
                &intent,
                text,
                scheduler_settings.default_timezone.as_str(),
                now_unix,
            )?
        {
            upsert_scheduler_pending_intent(
                ctx,
                chat_id,
                owner_user_id,
                &updated_draft,
                now_unix + TELEGRAM_SCHEDULER_PENDING_TTL_SECS,
            )
            .await?;
            send_telegram_command_reply(
                sender,
                message,
                &format!(
                    "已更新任务草案:\n{}\n\n回复“确认”创建任务，回复“取消”放弃草案。",
                    describe_scheduler_draft(&updated_draft)
                ),
            )
            .await?;
            return Ok(true);
        }
    }

    if try_handle_scheduler_management_text(
        ctx,
        sender,
        message,
        text,
        scheduler_settings,
        chat_id,
        owner_user_id,
    )
    .await?
    {
        return Ok(true);
    }

    let Some(intent) = ctx
        .service
        .detect_telegram_scheduler_intent(
            text,
            scheduler_settings.default_timezone.as_str(),
            now_unix,
            None,
        )
        .await?
    else {
        return Ok(false);
    };

    if intent.confidence < scheduler_settings.nl_min_confidence {
        return Ok(false);
    }

    match intent.action.as_str() {
        "list" => {
            let jobs = ctx
                .service
                .list_telegram_scheduler_jobs_by_owner(channel_name, chat_id, owner_user_id, 64)
                .await?;
            let text = telegram_task_list_text(&jobs, &scheduler_settings.default_timezone);
            send_telegram_command_reply(sender, message, &text).await?;
            Ok(true)
        }
        "delete" | "pause" | "resume" | "cancel" => {
            let op = resolve_scheduler_job_operation_from_intent(&intent).or_else(|| {
                if intent.action == "cancel" {
                    Some(SchedulerJobOperation::Delete)
                } else {
                    None
                }
            });
            let Some(op) = op else {
                return Ok(false);
            };
            let explicit_job_id = intent
                .job_id
                .as_deref()
                .and_then(normalize_scheduler_job_id)
                .or_else(|| extract_scheduler_job_id(text));
            let resolved = resolve_scheduler_job_target(
                ctx,
                chat_id,
                owner_user_id,
                op,
                explicit_job_id,
                text,
            )
            .await?;
            let op_ctx = SchedulerJobOperationContext {
                ctx,
                sender,
                message,
                scheduler_settings,
                chat_id,
                owner_user_id,
            };
            execute_scheduler_job_operation_with_resolution(&op_ctx, op, resolved).await?;
            Ok(true)
        }
        "create" | "update" => {
            let Some(draft) = build_scheduler_draft_from_intent(
                &intent,
                text,
                scheduler_settings.default_timezone.as_str(),
                now_unix,
            )?
            else {
                return Ok(false);
            };

            if scheduler_settings.require_confirm {
                upsert_scheduler_pending_intent(
                    ctx,
                    chat_id,
                    owner_user_id,
                    &draft,
                    now_unix + TELEGRAM_SCHEDULER_PENDING_TTL_SECS,
                )
                .await?;
                send_telegram_command_reply(
                    sender,
                    message,
                    &format!(
                        "识别到定时任务草案:\n{}\n\n回复“确认”创建任务，回复“取消”放弃草案。",
                        describe_scheduler_draft(&draft)
                    ),
                )
                .await?;
                Ok(true)
            } else {
                let (job_id, next_run_at_unix) = create_scheduler_job_from_draft(
                    ctx,
                    chat_id,
                    message.message_thread_id,
                    owner_user_id,
                    scheduler_settings,
                    &draft,
                )
                .await?;
                send_telegram_command_reply(
                    sender,
                    message,
                    &format!(
                        "已创建任务。\nID: {job_id}\n下次执行: {}\n内容: {}",
                        format_scheduler_time(Some(next_run_at_unix), &draft.timezone),
                        draft.payload
                    ),
                )
                .await?;
                Ok(true)
            }
        }
        _ => Ok(false),
    }
}

pub(super) async fn try_handle_scheduler_management_text(
    ctx: &ChannelContext,
    sender: &TelegramSender,
    message: &TelegramMessage,
    text: &str,
    scheduler_settings: &TelegramSchedulerSettings,
    chat_id: i64,
    owner_user_id: i64,
) -> anyhow::Result<bool> {
    if looks_like_scheduler_list_text(text) {
        let jobs = ctx
            .service
            .list_telegram_scheduler_jobs_by_owner(
                ctx.channel_name.clone(),
                chat_id,
                owner_user_id,
                64,
            )
            .await?;
        let text = telegram_task_list_text(&jobs, &scheduler_settings.default_timezone);
        send_telegram_command_reply(sender, message, &text).await?;
        return Ok(true);
    }

    let Some(op) = detect_scheduler_job_operation_keyword(text) else {
        return Ok(false);
    };
    let explicit_job_id = extract_scheduler_job_id(text);
    let resolved =
        resolve_scheduler_job_target(ctx, chat_id, owner_user_id, op, explicit_job_id, text)
            .await?;
    let op_ctx = SchedulerJobOperationContext {
        ctx,
        sender,
        message,
        scheduler_settings,
        chat_id,
        owner_user_id,
    };
    execute_scheduler_job_operation_with_resolution(&op_ctx, op, resolved).await?;
    Ok(true)
}

pub(super) async fn resolve_scheduler_job_target(
    ctx: &ChannelContext,
    chat_id: i64,
    owner_user_id: i64,
    op: SchedulerJobOperation,
    explicit_job_id: Option<String>,
    raw_text: &str,
) -> anyhow::Result<SchedulerJobTargetResolution> {
    if let Some(job_id) = explicit_job_id {
        return Ok(SchedulerJobTargetResolution::Selected {
            job_id,
            inferred: false,
        });
    }

    let jobs = ctx
        .service
        .list_telegram_scheduler_jobs_by_owner(ctx.channel_name.clone(), chat_id, owner_user_id, 64)
        .await?;

    let mut candidates = jobs
        .into_iter()
        .filter(|job| match op {
            SchedulerJobOperation::Pause => job.status == TelegramSchedulerJobStatus::Active,
            SchedulerJobOperation::Resume => job.status == TelegramSchedulerJobStatus::Paused,
            SchedulerJobOperation::Delete => {
                matches!(
                    job.status,
                    TelegramSchedulerJobStatus::Active | TelegramSchedulerJobStatus::Paused
                )
            }
        })
        .collect::<Vec<_>>();

    if candidates.is_empty() {
        return Ok(SchedulerJobTargetResolution::Empty);
    }
    if candidates.len() == 1 {
        return Ok(SchedulerJobTargetResolution::Selected {
            job_id: candidates.remove(0).job_id,
            inferred: true,
        });
    }

    if text_implies_latest_target(raw_text)
        || matches!(
            op,
            SchedulerJobOperation::Pause | SchedulerJobOperation::Resume
        )
    {
        return Ok(SchedulerJobTargetResolution::Selected {
            job_id: candidates.remove(0).job_id,
            inferred: true,
        });
    }

    Ok(SchedulerJobTargetResolution::Ambiguous {
        options: candidates.into_iter().take(5).collect(),
    })
}

pub(super) async fn execute_scheduler_job_operation_with_resolution(
    op_ctx: &SchedulerJobOperationContext<'_>,
    op: SchedulerJobOperation,
    resolved: SchedulerJobTargetResolution,
) -> anyhow::Result<()> {
    match resolved {
        SchedulerJobTargetResolution::Empty => {
            send_telegram_command_reply(
                op_ctx.sender,
                op_ctx.message,
                "没有找到可操作的任务。可先发送 `/task list` 查看。",
            )
            .await?;
        }
        SchedulerJobTargetResolution::Ambiguous { options } => {
            let hint = format_scheduler_operation_hint(op);
            let choices = format_scheduler_operation_candidates(
                &options,
                &op_ctx.scheduler_settings.default_timezone,
            );
            send_telegram_command_reply(
                op_ctx.sender,
                op_ctx.message,
                &format!(
                    "匹配到多个任务，暂未自动执行 {}。\n{}\n\n请补充任务 ID（例如：`{} task-...`）。",
                    hint,
                    choices,
                    hint
                ),
            )
            .await?;
        }
        SchedulerJobTargetResolution::Selected { job_id, inferred } => {
            execute_scheduler_job_operation(op_ctx, op, &job_id, inferred).await?;
        }
    }
    Ok(())
}

pub(super) async fn execute_scheduler_job_operation(
    op_ctx: &SchedulerJobOperationContext<'_>,
    op: SchedulerJobOperation,
    job_id: &str,
    inferred: bool,
) -> anyhow::Result<()> {
    let text_prefix = match op {
        SchedulerJobOperation::Pause => "任务已暂停",
        SchedulerJobOperation::Resume => "任务已恢复",
        SchedulerJobOperation::Delete => "任务已删除",
    };

    let loaded = op_ctx
        .ctx
        .service
        .load_telegram_scheduler_job(op_ctx.ctx.channel_name.clone(), job_id.to_string())
        .await?;
    let Some(job) = loaded else {
        send_telegram_command_reply(
            op_ctx.sender,
            op_ctx.message,
            &format!("未找到可更新的任务: {job_id}"),
        )
        .await?;
        return Ok(());
    };
    if job.chat_id != op_ctx.chat_id || job.owner_user_id != op_ctx.owner_user_id {
        send_telegram_command_reply(
            op_ctx.sender,
            op_ctx.message,
            &format!("未找到可更新的任务: {job_id}"),
        )
        .await?;
        return Ok(());
    }

    let current_state = scheduler_state_from_job_status(job.status);
    let event = op.as_event();
    let next_state = match apply_scheduler_state_transition(current_state, event) {
        Ok(state) => state,
        Err(_) => {
            send_telegram_command_reply(
                op_ctx.sender,
                op_ctx.message,
                &format!(
                    "任务当前状态为 `{}`，不支持执行该操作。",
                    current_state.as_str()
                ),
            )
            .await?;
            return Ok(());
        }
    };
    if next_state == current_state {
        send_telegram_command_reply(
            op_ctx.sender,
            op_ctx.message,
            &format!("任务 `{job_id}` 当前已是 `{}`。", current_state.as_str()),
        )
        .await?;
        return Ok(());
    }
    let status = scheduler_state_to_job_status(next_state);

    let updated = op_ctx
        .ctx
        .service
        .update_telegram_scheduler_job_status(
            op_ctx.ctx.channel_name.clone(),
            op_ctx.chat_id,
            op_ctx.owner_user_id,
            job_id.to_string(),
            status,
        )
        .await?;

    let text = if updated {
        if inferred {
            format!("{text_prefix}: {job_id}\n已自动定位目标任务。")
        } else {
            format!("{text_prefix}: {job_id}")
        }
    } else {
        format!("未找到可更新的任务: {job_id}")
    };
    send_telegram_command_reply(op_ctx.sender, op_ctx.message, &text).await?;
    Ok(())
}

pub(super) async fn upsert_scheduler_pending_intent(
    ctx: &ChannelContext,
    chat_id: i64,
    owner_user_id: i64,
    draft: &TelegramSchedulerIntentDraft,
    expires_at_unix: i64,
) -> anyhow::Result<()> {
    let intent_id = format!("pending:{}:{}:{}", ctx.channel_name, chat_id, owner_user_id);
    let draft_json = serde_json::to_string(draft).context("failed to encode scheduler draft")?;
    ctx.service
        .upsert_telegram_scheduler_pending_intent(UpsertTelegramSchedulerPendingIntentRequest {
            intent_id,
            channel: ctx.channel_name.clone(),
            chat_id,
            owner_user_id,
            draft_json,
            expires_at_unix,
        })
        .await
}

pub(super) fn evaluate_mcp_command_access(
    is_private_chat: bool,
    caller_user_id: Option<i64>,
    command_settings: &TelegramCommandSettings,
) -> McpCommandAccess {
    if command_settings.private_only && !is_private_chat {
        return McpCommandAccess::RequirePrivateChat;
    }
    if command_settings.admin_user_ids.is_empty() {
        return McpCommandAccess::MissingAdminAllowlist;
    }
    if caller_user_id.is_none_or(|id| !command_settings.admin_user_ids.contains(&id)) {
        return McpCommandAccess::Unauthorized;
    }
    McpCommandAccess::Allowed
}

pub(super) fn evaluate_private_chat_access(
    caller_user_id: Option<i64>,
    admin_user_ids: &[i64],
) -> PrivateChatAccess {
    if admin_user_ids.is_empty() {
        return PrivateChatAccess::MissingAdminAllowlist;
    }
    if caller_user_id.is_none_or(|id| !admin_user_ids.contains(&id)) {
        return PrivateChatAccess::Unauthorized;
    }
    PrivateChatAccess::Allowed
}

pub(super) fn parse_telegram_slash_command(
    text: &str,
    bot_username: Option<&str>,
) -> Option<TelegramSlashCommand> {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    let split_idx = trimmed
        .char_indices()
        .find_map(|(idx, ch)| ch.is_whitespace().then_some(idx))
        .unwrap_or(trimmed.len());
    let head = &trimmed[..split_idx];
    let tail = trimmed[split_idx..].trim().to_string();
    let raw = head.trim_start_matches('/');
    if raw.is_empty() {
        return None;
    }

    let (command, at_target) = raw.split_once('@').unwrap_or((raw, ""));
    if !at_target.is_empty()
        && bot_username.is_some_and(|v| {
            let expect = v.trim().trim_start_matches('@');
            !expect.is_empty() && !at_target.eq_ignore_ascii_case(expect)
        })
    {
        return None;
    }

    let command = command.to_ascii_lowercase();
    match command.as_str() {
        "start" => Some(TelegramSlashCommand::Start),
        "help" => Some(TelegramSlashCommand::Help),
        "whoami" => Some(TelegramSlashCommand::WhoAmI),
        "mcp" => Some(TelegramSlashCommand::Mcp { tail }),
        "skills" | "skill" => Some(TelegramSlashCommand::Skills { tail }),
        "task" => Some(TelegramSlashCommand::Task { tail }),
        _ => Some(TelegramSlashCommand::Unknown { name: command }),
    }
}

pub(super) fn telegram_start_text() -> String {
    [
        "你好，我是 xiaomaolv Telegram 机器人。",
        "输入 /help 查看可用命令。",
    ]
    .join("\n")
}

pub(super) fn telegram_help_text() -> String {
    [
        "可用命令:",
        "/start - 查看欢迎信息",
        "/help - 查看帮助",
        "/whoami - 查看你的 Telegram 用户 ID",
        "/mcp - 管理 MCP 服务器（仅私聊管理员）",
        "/skills - 管理 Skills（仅私聊管理员）",
        "/task - 管理定时任务（仅私聊管理员）",
    ]
    .join("\n")
}

pub(super) fn telegram_task_help_text(default_timezone: &str) -> String {
    [
        "定时任务命令:",
        "/task list",
        "/task add <时间> | <内容>",
        "/task every <cron> | <内容>",
        "/task pause <job_id>",
        "/task resume <job_id>",
        "/task del <job_id>",
        "",
        "时间支持:",
        "- Unix 秒时间戳",
        "- RFC3339，例如 2026-03-01T09:00:00+08:00",
        &format!("- YYYY-MM-DD HH:MM（默认时区 {default_timezone}）"),
        "",
        "示例:",
        "/task add 2026-03-01 09:00 | 提醒我参加晨会",
        "/task every 0 9 * * * | 每天提醒我写日报",
    ]
    .join("\n")
}

pub(super) fn telegram_whoami_text(message: &TelegramMessage) -> String {
    match message.from.as_ref().map(|u| u.id) {
        Some(user_id) => format!(
            "你的 Telegram 用户 ID: {user_id}\n建议配置:\nTELEGRAM_ADMIN_USER_IDS={user_id}"
        ),
        None => "无法识别当前用户 ID（message.from 缺失）。请使用普通用户账号私聊 bot 再试一次。"
            .to_string(),
    }
}

pub(super) fn telegram_whoami_hint(message: &TelegramMessage) -> String {
    match message.from.as_ref().map(|u| u.id) {
        Some(user_id) => format!("你的当前用户 ID: {user_id}（也可发送 /whoami 查看）"),
        None => "可发送 /whoami 查看你的 Telegram 用户 ID。".to_string(),
    }
}

pub(super) fn telegram_registered_commands() -> Vec<(&'static str, &'static str)> {
    vec![
        ("start", "启动与介绍"),
        ("help", "查看帮助"),
        ("whoami", "查看当前用户ID"),
        ("mcp", "管理 MCP 服务器"),
        ("skills", "管理 Skills"),
        ("task", "管理定时任务"),
    ]
}

pub(super) async fn send_telegram_command_reply(
    sender: &TelegramSender,
    message: &TelegramMessage,
    text: &str,
) -> anyhow::Result<()> {
    sender
        .send_message(
            message.chat.id,
            message.message_thread_id,
            Some(message.message_id),
            text,
        )
        .await
}

pub(super) fn is_private_chat_type(kind: Option<&str>) -> bool {
    matches!(
        kind.map(|v| v.trim().to_ascii_lowercase()),
        Some(v) if v == "private"
    )
}
