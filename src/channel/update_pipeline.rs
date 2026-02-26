use std::time::Duration;

use anyhow::Context;

use super::group_pipeline::{GroupProcessContext, GroupProcessOutcome, process_group_message};
use super::*;

struct TelegramInboundMeta {
    chat_id: i64,
    message_id: i64,
    chat_kind: String,
    message_thread_id: Option<i64>,
    reply_to_message_id: Option<i64>,
    replied_to_bot: bool,
    is_group: bool,
    is_private_chat: bool,
    from_id: Option<i64>,
    from_is_bot: bool,
    mention_entity_count: usize,
}

struct ReplyDispatchConfig<'a> {
    ctx: &'a ChannelContext,
    sender: &'a TelegramSender,
    message: &'a TelegramMessage,
    meta: &'a TelegramInboundMeta,
    model_input_text: String,
    streaming_enabled: bool,
    streaming_edit_interval_ms: u64,
    streaming_prefer_draft: bool,
}

impl TelegramInboundMeta {
    fn from_message(message: &TelegramMessage, bot_user_id: Option<i64>) -> Self {
        let from_id = message.from.as_ref().map(|u| u.id);
        let from_is_bot = message
            .from
            .as_ref()
            .and_then(|u| u.is_bot)
            .unwrap_or(false)
            || bot_user_id
                .zip(from_id)
                .map(|(bot_id, uid)| bot_id == uid)
                .unwrap_or(false);

        let mention_entity_count = message
            .entities
            .as_ref()
            .map(|items| {
                items
                    .iter()
                    .filter(|e| e.kind == "mention" || e.kind == "text_mention")
                    .count()
            })
            .unwrap_or(0);

        Self {
            chat_id: message.chat.id,
            message_id: message.message_id,
            chat_kind: message
                .chat
                .kind
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
            message_thread_id: message.message_thread_id,
            reply_to_message_id: message.reply_to_message.as_ref().map(|m| m.message_id),
            replied_to_bot: is_reply_to_bot_message(message.reply_to_message.as_ref(), bot_user_id),
            is_group: is_group_chat_type(message.chat.kind.as_deref()),
            is_private_chat: is_private_chat_type(message.chat.kind.as_deref()),
            from_id,
            from_is_bot,
            mention_entity_count,
        }
    }
}

pub(super) async fn process_telegram_update(
    ctx: ChannelContext,
    sender: &TelegramSender,
    update: TelegramUpdate,
    runtime: TelegramUpdateProcessingContext,
) -> anyhow::Result<()> {
    let TelegramUpdateProcessingContext {
        streaming_enabled,
        streaming_edit_interval_ms,
        streaming_prefer_draft,
        group_mention_bot_username,
        bot_user_id,
        command_settings,
        group_trigger_settings,
        group_runtime_state,
        diag_state,
    } = runtime;

    if let Some(callback) = update.callback_query.as_ref() {
        log_inbound_callback_query(callback);
        if maybe_handle_telegram_callback_query(&ctx, sender, callback, &command_settings).await? {
            return Ok(());
        }
    }

    let Some(message) = update.message else {
        return Ok(());
    };
    let Some(text) = message.text.as_deref() else {
        return Ok(());
    };

    let meta = TelegramInboundMeta::from_message(&message, bot_user_id);
    log_inbound_message(text, &meta, bot_user_id);

    if meta.is_group {
        let mut diag = diag_state.lock().await;
        diag.group_messages_total += 1;
    }

    if meta.is_group && meta.from_is_bot {
        let mut diag = diag_state.lock().await;
        diag.group_decision_ignore_total += 1;
        debug!(
            chat_id = meta.chat_id,
            message_id = message.message_id,
            "skip telegram group message from bot account"
        );
        info!(
            chat_id = meta.chat_id,
            message_id = meta.message_id,
            "telegram group message skipped: sender is bot"
        );
        return Ok(());
    }

    if meta.is_private_chat
        && enforce_private_chat_access(sender, &message, &command_settings)
            .await
            .context("failed to enforce telegram private chat access")?
    {
        return Ok(());
    }

    if maybe_handle_telegram_command(
        &ctx,
        sender,
        &message,
        text.trim(),
        group_mention_bot_username.as_deref(),
        &command_settings,
    )
    .await?
    {
        return Ok(());
    }

    if meta.is_private_chat
        && should_send_initial_typing_before_scheduler_nl(&meta, &command_settings.scheduler)
    {
        send_initial_typing_hint(sender, &meta, "private_scheduler_nl");
    }

    if meta.is_private_chat
        && should_send_initial_typing_before_scheduler_nl(&meta, &command_settings.scheduler)
        && maybe_handle_telegram_scheduler_natural_language(
            &ctx,
            sender,
            &message,
            text.trim(),
            &command_settings.scheduler,
        )
        .await?
    {
        return Ok(());
    }

    if should_send_early_group_typing_hint(
        text,
        &meta,
        group_mention_bot_username.as_deref(),
        group_trigger_settings.mode,
    ) {
        send_initial_typing_hint(sender, &meta, "group_pretrigger");
    }

    let model_input_text = if meta.is_group {
        match process_group_message(GroupProcessContext {
            ctx: &ctx,
            message: &message,
            text,
            chat_id: meta.chat_id,
            message_id: meta.message_id,
            message_thread_id: meta.message_thread_id,
            reply_to_message_id: meta.reply_to_message_id,
            replied_to_bot: meta.replied_to_bot,
            group_mention_bot_username: group_mention_bot_username.as_deref(),
            group_trigger_settings: &group_trigger_settings,
            group_runtime_state: &group_runtime_state,
            diag_state: &diag_state,
            mention_entity_count: meta.mention_entity_count,
        })
        .await?
        {
            GroupProcessOutcome::Continue { model_input_text } => model_input_text,
            GroupProcessOutcome::Stop => return Ok(()),
        }
    } else {
        text.to_string()
    };

    if meta.is_group
        && should_send_initial_typing_before_scheduler_nl(&meta, &command_settings.scheduler)
    {
        send_initial_typing_hint(sender, &meta, "group_scheduler_nl");
    }

    if meta.is_group
        && should_send_initial_typing_before_scheduler_nl(&meta, &command_settings.scheduler)
        && maybe_handle_telegram_scheduler_natural_language(
            &ctx,
            sender,
            &message,
            text.trim(),
            &command_settings.scheduler,
        )
        .await?
    {
        let mut state = group_runtime_state.lock().await;
        state
            .last_bot_response_unix_by_chat
            .insert(meta.chat_id, current_unix_timestamp());
        return Ok(());
    }

    dispatch_telegram_reply(ReplyDispatchConfig {
        ctx: &ctx,
        sender,
        message: &message,
        meta: &meta,
        model_input_text,
        streaming_enabled,
        streaming_edit_interval_ms,
        streaming_prefer_draft,
    })
    .await?;

    if meta.is_group {
        let mut state = group_runtime_state.lock().await;
        state
            .last_bot_response_unix_by_chat
            .insert(meta.chat_id, current_unix_timestamp());
    }

    Ok(())
}

fn log_inbound_message(text: &str, meta: &TelegramInboundMeta, bot_user_id: Option<i64>) {
    let text_preview = truncate_chars(text.trim(), 120).replace('\n', " ");
    info!(
        chat_id = meta.chat_id,
        message_id = meta.message_id,
        chat_type = %meta.chat_kind,
        is_group = meta.is_group,
        from_is_bot = meta.from_is_bot,
        bot_user_id = ?bot_user_id,
        from_id = ?meta.from_id,
        message_thread_id = ?meta.message_thread_id,
        reply_to_message_id = ?meta.reply_to_message_id,
        replied_to_bot = meta.replied_to_bot,
        mention_entity_count = meta.mention_entity_count,
        text_len = text.chars().count(),
        text_preview = %text_preview,
        "telegram inbound message"
    );
}

fn log_inbound_callback_query(callback: &TelegramCallbackQuery) {
    let data_preview = callback
        .data
        .as_deref()
        .map(|v| truncate_chars(v.trim(), 120).replace('\n', " "))
        .unwrap_or_default();
    let chat_id = callback.message.as_ref().map(|m| m.chat.id);
    let message_id = callback.message.as_ref().map(|m| m.message_id);
    info!(
        callback_query_id = %callback.id,
        from_id = callback.from.id,
        chat_id = ?chat_id,
        message_id = ?message_id,
        data_len = callback.data.as_ref().map(|v| v.chars().count()).unwrap_or(0),
        data_preview = %data_preview,
        "telegram inbound callback_query"
    );
}

fn should_send_initial_typing_before_scheduler_nl(
    meta: &TelegramInboundMeta,
    scheduler_settings: &TelegramSchedulerSettings,
) -> bool {
    scheduler_settings.enabled
        && scheduler_settings.nl_enabled
        && (meta.is_group || meta.is_private_chat)
}

fn should_send_early_group_typing_hint(
    text: &str,
    meta: &TelegramInboundMeta,
    group_mention_bot_username: Option<&str>,
    group_trigger_mode: TelegramGroupTriggerMode,
) -> bool {
    if !meta.is_group {
        return false;
    }
    if meta.replied_to_bot {
        return true;
    }

    let Some(bot_username) = group_mention_bot_username else {
        return false;
    };
    if message_mentions_bot(text, bot_username) {
        return true;
    }

    matches!(group_trigger_mode, TelegramGroupTriggerMode::Smart)
        && message_has_question_marker(text)
        && extract_vocative_alias_candidate(text, bot_username).is_some()
}

fn send_initial_typing_hint(
    sender: &TelegramSender,
    meta: &TelegramInboundMeta,
    stage: &'static str,
) {
    let sender = sender.clone();
    let chat_id = meta.chat_id;
    let message_id = meta.message_id;
    let message_thread_id = meta.message_thread_id;
    debug!(
        chat_id,
        message_id,
        message_thread_id = ?message_thread_id,
        stage,
        "telegram initial typing hint scheduled"
    );
    tokio::spawn(async move {
        if let Err(err) = sender
            .send_chat_action_typing(chat_id, message_thread_id)
            .await
        {
            warn!(
                error = %err,
                chat_id,
                message_id,
                stage,
                "telegram initial typing hint failed"
            );
        }
    });
}

async fn enforce_private_chat_access(
    sender: &TelegramSender,
    message: &TelegramMessage,
    command_settings: &TelegramCommandSettings,
) -> anyhow::Result<bool> {
    match evaluate_private_chat_access(
        message.from.as_ref().map(|u| u.id),
        &command_settings.admin_user_ids,
    ) {
        PrivateChatAccess::Allowed => Ok(false),
        PrivateChatAccess::MissingAdminAllowlist => {
            send_telegram_command_reply(
                sender,
                message,
                &format!(
                    "管理员白名单未配置，私聊功能未启用。\n{}\n\n请在 .env.realtest 中配置 TELEGRAM_ADMIN_USER_IDS，然后重启服务。",
                    telegram_whoami_hint(message)
                ),
            )
            .await
            .context("failed to send telegram private-chat deny message")?;
            Ok(true)
        }
        PrivateChatAccess::Unauthorized => {
            send_telegram_command_reply(
                sender,
                message,
                &format!(
                    "无权限使用该机器人。\n私聊仅开放给管理员。\n{}\n\n如需开通，请让管理员把你的 ID 加入 TELEGRAM_ADMIN_USER_IDS。",
                    telegram_whoami_hint(message)
                ),
            )
            .await
            .context("failed to send telegram private-chat unauthorized message")?;
            Ok(true)
        }
    }
}

async fn dispatch_telegram_reply(config: ReplyDispatchConfig<'_>) -> anyhow::Result<()> {
    let ReplyDispatchConfig {
        ctx,
        sender,
        message,
        meta,
        model_input_text,
        streaming_enabled,
        streaming_edit_interval_ms,
        streaming_prefer_draft,
    } = config;
    let user_id = message
        .from
        .as_ref()
        .map(|u| u.id.to_string())
        .unwrap_or_else(|| meta.chat_id.to_string());

    let session_id = if meta.is_group {
        telegram_session_id(
            meta.chat_id,
            meta.message_thread_id,
            meta.reply_to_message_id,
        )
    } else {
        format!("tg:{}", meta.chat_id)
    };

    let outbound_reply_to_message_id = if meta.is_group && meta.replied_to_bot {
        Some(meta.message_id)
    } else {
        None
    };

    info!(
        chat_id = meta.chat_id,
        message_id = meta.message_id,
        session_id = %session_id,
        outbound_reply_to_message_id = ?outbound_reply_to_message_id,
        "telegram message accepted for processing"
    );

    let incoming = IncomingMessage {
        channel: ctx.channel_name.clone(),
        session_id,
        user_id,
        text: model_input_text,
        reply_target: Some(ReplyTarget::Telegram {
            chat_id: meta.chat_id,
            message_thread_id: meta.message_thread_id,
        }),
    };

    let typing_sender = sender.clone();
    let typing_chat_id = meta.chat_id;
    let typing_thread_id = meta.message_thread_id;
    let typing_task = tokio::spawn(async move {
        loop {
            if let Err(err) = typing_sender
                .send_chat_action_typing(typing_chat_id, typing_thread_id)
                .await
            {
                warn!(error = %err, chat_id = typing_chat_id, "telegram typing heartbeat failed");
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    let result: anyhow::Result<()> = if streaming_enabled {
        let mut sink = TelegramStreamSink::new(
            sender,
            meta.chat_id,
            meta.message_thread_id,
            outbound_reply_to_message_id,
            streaming_prefer_draft,
            Duration::from_millis(streaming_edit_interval_ms),
        );
        match ctx
            .service
            .handle_stream(incoming, &mut sink)
            .await
            .context("failed to process telegram streaming message")
        {
            Ok(reply) => sink.finalize(&reply.text).await,
            Err(err) => Err(err),
        }
    } else {
        match ctx
            .service
            .handle(incoming)
            .await
            .context("failed to process telegram message")
        {
            Ok(reply) => sender
                .send_message(
                    meta.chat_id,
                    meta.message_thread_id,
                    outbound_reply_to_message_id,
                    &reply.text,
                )
                .await
                .context("failed to send telegram response"),
            Err(err) => Err(err),
        }
    };

    typing_task.abort();
    let _ = typing_task.await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scheduler_settings(enabled: bool, nl_enabled: bool) -> TelegramSchedulerSettings {
        TelegramSchedulerSettings {
            enabled,
            tick_secs: 2,
            batch_size: 8,
            lease_secs: 30,
            default_timezone: "Asia/Shanghai".to_string(),
            nl_enabled,
            nl_min_confidence: 0.78,
            require_confirm: true,
            max_jobs_per_owner: 64,
        }
    }

    fn inbound_meta(is_group: bool, is_private_chat: bool) -> TelegramInboundMeta {
        TelegramInboundMeta {
            chat_id: -100,
            message_id: 1,
            chat_kind: "group".to_string(),
            message_thread_id: None,
            reply_to_message_id: None,
            replied_to_bot: false,
            is_group,
            is_private_chat,
            from_id: Some(42),
            from_is_bot: false,
            mention_entity_count: 0,
        }
    }

    #[test]
    fn pretyping_is_enabled_for_group_scheduler_nl_flow() {
        let meta = inbound_meta(true, false);
        let settings = scheduler_settings(true, true);
        assert!(should_send_initial_typing_before_scheduler_nl(
            &meta, &settings
        ));
    }

    #[test]
    fn pretyping_is_disabled_when_scheduler_nl_is_off() {
        let meta = inbound_meta(true, false);
        let settings = scheduler_settings(true, false);
        assert!(!should_send_initial_typing_before_scheduler_nl(
            &meta, &settings
        ));
    }

    #[test]
    fn pretyping_is_enabled_for_private_scheduler_nl_flow() {
        let meta = inbound_meta(false, true);
        let settings = scheduler_settings(true, true);
        assert!(should_send_initial_typing_before_scheduler_nl(
            &meta, &settings
        ));
    }

    #[test]
    fn early_group_pretyping_triggers_on_reply_to_bot() {
        let meta = inbound_meta(true, false);
        let result = should_send_early_group_typing_hint(
            "随便说一句",
            &TelegramInboundMeta {
                replied_to_bot: true,
                ..meta
            },
            Some("myxiaomaolvbot"),
            TelegramGroupTriggerMode::Strict,
        );
        assert!(result);
    }

    #[test]
    fn early_group_pretyping_triggers_on_explicit_mention() {
        let meta = inbound_meta(true, false);
        let result = should_send_early_group_typing_hint(
            "@myxiaomaolvbot 你在吗",
            &meta,
            Some("myxiaomaolvbot"),
            TelegramGroupTriggerMode::Strict,
        );
        assert!(result);
    }

    #[test]
    fn early_group_pretyping_triggers_on_smart_vocative_question() {
        let meta = inbound_meta(true, false);
        let result = should_send_early_group_typing_hint(
            "驴哥你在吗？",
            &meta,
            Some("myxiaomaolvbot"),
            TelegramGroupTriggerMode::Smart,
        );
        assert!(result);
    }

    #[test]
    fn early_group_pretyping_does_not_trigger_for_strict_vocative_question() {
        let meta = inbound_meta(true, false);
        let result = should_send_early_group_typing_hint(
            "驴哥你在吗？",
            &meta,
            Some("myxiaomaolvbot"),
            TelegramGroupTriggerMode::Strict,
        );
        assert!(!result);
    }
}
