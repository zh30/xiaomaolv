use anyhow::Context;

use super::*;

pub(super) enum GroupProcessOutcome {
    Continue { model_input_text: String },
    Stop,
}

struct GroupPreparation {
    sender_user_id: i64,
    sender_profile: TelegramGroupUserProfile,
    model_input_text: String,
    now: u64,
}

pub(super) struct GroupProcessContext<'a> {
    pub(super) ctx: &'a ChannelContext,
    pub(super) message: &'a TelegramMessage,
    pub(super) text: &'a str,
    pub(super) chat_id: i64,
    pub(super) message_id: i64,
    pub(super) message_thread_id: Option<i64>,
    pub(super) reply_to_message_id: Option<i64>,
    pub(super) replied_to_bot: bool,
    pub(super) group_mention_bot_username: Option<&'a str>,
    pub(super) group_trigger_settings: &'a TelegramGroupTriggerSettings,
    pub(super) group_runtime_state: &'a Arc<tokio::sync::Mutex<TelegramGroupRuntimeState>>,
    pub(super) diag_state: &'a Arc<tokio::sync::Mutex<TelegramPollingDiagState>>,
    pub(super) mention_entity_count: usize,
}

pub(super) async fn process_group_message(
    input: GroupProcessContext<'_>,
) -> anyhow::Result<GroupProcessOutcome> {
    let GroupProcessContext {
        ctx,
        message,
        text,
        chat_id,
        message_id,
        message_thread_id,
        reply_to_message_id,
        replied_to_bot,
        group_mention_bot_username,
        group_trigger_settings,
        group_runtime_state,
        diag_state,
        mention_entity_count,
    } = input;

    bootstrap_group_runtime_state(ctx, chat_id, group_runtime_state).await;

    let prep =
        build_group_identity_context(ctx, message, text, chat_id, message_id, group_runtime_state)
            .await;

    let bot_username = group_mention_bot_username.unwrap_or("");
    let mentioned = if bot_username.is_empty() {
        false
    } else {
        message_mentions_bot(text, bot_username)
    };
    let has_question_marker = message_has_question_marker(text);

    if (mentioned || replied_to_bot) && !bot_username.is_empty() {
        learn_group_aliases(ctx, text, chat_id, bot_username, group_runtime_state).await;
    }

    let (
        recent_bot_participation,
        cooldown_active,
        seconds_since_last_bot_response,
        mut learned_aliases,
    ) = load_group_runtime_signals(
        chat_id,
        prep.now,
        group_trigger_settings,
        group_runtime_state,
    )
    .await;

    let points_to_other_bot = if bot_username.is_empty() {
        false
    } else {
        message_mentions_other_handle(text, bot_username)
    };
    let low_signal_noise = is_low_signal_group_noise(text);
    let vocative_alias_candidate = if bot_username.is_empty() {
        None
    } else {
        detect_vocative_learned_alias_prefix(text, &learned_aliases)
            .or_else(|| extract_vocative_alias_candidate(text, bot_username))
    };
    let vocative_alias_known = vocative_alias_candidate
        .as_ref()
        .map(|candidate| learned_aliases.iter().any(|v| v == candidate))
        .unwrap_or(false);
    let vocative_alias_hit = matches!(group_trigger_settings.mode, TelegramGroupTriggerMode::Smart)
        && !points_to_other_bot
        && vocative_alias_candidate.is_some()
        && (vocative_alias_known || has_question_marker);

    if vocative_alias_hit
        && let Some(candidate) = vocative_alias_candidate.as_ref()
        && !vocative_alias_known
    {
        learn_group_alias_tokens(ctx, chat_id, vec![candidate.clone()], group_runtime_state).await;
        learned_aliases.push(candidate.clone());
    }

    let alias_hit = message_contains_any_alias(text, &learned_aliases);

    let decision = evaluate_group_decision(
        group_trigger_settings.mode,
        &GroupSignalInput {
            mentioned,
            replied_to_bot,
            recent_bot_participation,
            alias_hit,
            vocative_alias_hit,
            has_question_marker,
            points_to_other_bot,
            low_signal_noise,
            cooldown_active,
        },
        group_trigger_settings.rule_min_score,
    );

    record_group_decision_metrics(diag_state, decision.kind).await;

    debug!(
        chat_id,
        message_id = message.message_id,
        bot_username = %bot_username,
        mentioned,
        replied_to_bot,
        alias_hit,
        vocative_alias_hit,
        vocative_alias_candidate = ?vocative_alias_candidate,
        aliases_count = learned_aliases.len(),
        has_question_marker,
        recent_bot_participation,
        cooldown_active,
        seconds_since_last_bot_response = ?seconds_since_last_bot_response,
        decision = %decision.kind.as_str(),
        decision_score = decision.score,
        reasons = ?decision.reasons,
        text_len = text.chars().count(),
        sender_user_id = prep.sender_user_id,
        sender_preferred_name = %prep.sender_profile.preferred_name,
        "telegram group mention check"
    );

    info!(
        chat_id,
        message_id,
        bot_username = %bot_username,
        mentioned,
        replied_to_bot,
        alias_hit,
        vocative_alias_hit,
        vocative_alias_candidate = ?vocative_alias_candidate,
        aliases_count = learned_aliases.len(),
        has_question_marker,
        recent_bot_participation,
        cooldown_active,
        seconds_since_last_bot_response = ?seconds_since_last_bot_response,
        decision = %decision.kind.as_str(),
        decision_score = decision.score,
        reasons = ?decision.reasons,
        mention_entity_count,
        sender_user_id = prep.sender_user_id,
        sender_preferred_name = %prep.sender_profile.preferred_name,
        "telegram group trigger decision"
    );

    match decision.kind {
        GroupDecisionKind::Respond => Ok(GroupProcessOutcome::Continue {
            model_input_text: prep.model_input_text,
        }),
        GroupDecisionKind::ObserveOnly => {
            let user_id = message
                .from
                .as_ref()
                .map(|u| u.id.to_string())
                .unwrap_or_else(|| chat_id.to_string());
            let observe_session_id = format!(
                "{}:observe",
                telegram_session_id(chat_id, message_thread_id, reply_to_message_id)
            );
            ctx.service
                .observe(IncomingMessage {
                    channel: ctx.channel_name.clone(),
                    session_id: observe_session_id.clone(),
                    user_id,
                    text: prep.model_input_text,
                    reply_target: None,
                })
                .await
                .with_context(|| {
                    format!(
                        "failed to persist telegram observe-only message (session={observe_session_id})"
                    )
                })?;
            info!(
                chat_id,
                message_id,
                observe_session_id = %observe_session_id,
                "telegram group message observed silently: decision=observe_only"
            );
            Ok(GroupProcessOutcome::Stop)
        }
        GroupDecisionKind::Ignore => {
            if bot_username.is_empty() {
                info!(
                    chat_id,
                    message_id,
                    "telegram group message skipped: bot username unavailable and message is not a reply to bot"
                );
            }
            info!(
                chat_id,
                message_id, "telegram group message skipped by trigger decision"
            );
            Ok(GroupProcessOutcome::Stop)
        }
    }
}

async fn bootstrap_group_runtime_state(
    ctx: &ChannelContext,
    chat_id: i64,
    group_runtime_state: &Arc<tokio::sync::Mutex<TelegramGroupRuntimeState>>,
) {
    let (needs_alias_bootstrap, needs_profile_bootstrap) = {
        let state = group_runtime_state.lock().await;
        (
            !state.loaded_aliases_chats.contains(&chat_id),
            !state.loaded_profile_chats.contains(&chat_id),
        )
    };

    if needs_alias_bootstrap {
        match ctx
            .service
            .load_group_aliases(ctx.channel_name.clone(), chat_id, 64)
            .await
        {
            Ok(stored_aliases) => {
                let mut state = group_runtime_state.lock().await;
                let chat_aliases = state.learned_aliases_by_chat.entry(chat_id).or_default();
                for alias in stored_aliases {
                    let normalized = normalize_alias_token(&alias);
                    if normalized.is_empty() || chat_aliases.len() >= 24 {
                        continue;
                    }
                    chat_aliases.insert(normalized);
                }
                state.loaded_aliases_chats.insert(chat_id);
            }
            Err(err) => {
                warn!(
                    chat_id,
                    error = %format!("{err:#}"),
                    "failed to bootstrap learned telegram group aliases"
                );
                let mut state = group_runtime_state.lock().await;
                state.loaded_aliases_chats.insert(chat_id);
            }
        }
    }

    if needs_profile_bootstrap {
        match ctx
            .service
            .load_group_user_profiles(ctx.channel_name.clone(), chat_id, 128)
            .await
        {
            Ok(stored_profiles) => {
                let mut state = group_runtime_state.lock().await;
                let chat_profiles = state.user_profiles_by_chat.entry(chat_id).or_default();
                for profile in stored_profiles {
                    if profile.preferred_name.trim().is_empty() || chat_profiles.len() >= 256 {
                        continue;
                    }
                    chat_profiles.insert(
                        profile.user_id,
                        TelegramGroupUserProfile {
                            preferred_name: profile.preferred_name,
                            username: normalize_telegram_username(profile.username.as_deref()),
                            updated_at: profile.updated_at.max(0) as u64,
                        },
                    );
                }
                state.loaded_profile_chats.insert(chat_id);
            }
            Err(err) => {
                warn!(
                    chat_id,
                    error = %format!("{err:#}"),
                    "failed to bootstrap telegram group user profiles"
                );
                let mut state = group_runtime_state.lock().await;
                state.loaded_profile_chats.insert(chat_id);
            }
        }
    }
}

async fn build_group_identity_context(
    ctx: &ChannelContext,
    message: &TelegramMessage,
    text: &str,
    chat_id: i64,
    message_id: i64,
    group_runtime_state: &Arc<tokio::sync::Mutex<TelegramGroupRuntimeState>>,
) -> GroupPreparation {
    let sender_user_id = message.from.as_ref().map(|u| u.id).unwrap_or(chat_id);
    let sender_username =
        normalize_telegram_username(message.from.as_ref().and_then(|u| u.username.as_deref()));
    let sender_observed_name = derive_telegram_user_display_name(message.from.as_ref())
        .unwrap_or_else(|| format!("用户{sender_user_id}"));
    let corrected_name = extract_realtime_name_correction(text);
    let now = current_unix_timestamp();

    let mut profile_persist_payload: Option<(i64, String, Option<String>)> = None;
    let (sender_profile, roster_entries) = {
        let mut state = group_runtime_state.lock().await;
        let chat_profiles = state.user_profiles_by_chat.entry(chat_id).or_default();
        let profile =
            chat_profiles
                .entry(sender_user_id)
                .or_insert_with(|| TelegramGroupUserProfile {
                    preferred_name: sender_observed_name.clone(),
                    username: sender_username.clone(),
                    updated_at: now,
                });

        let mut changed = false;
        if profile.preferred_name.trim().is_empty() {
            profile.preferred_name = sender_observed_name.clone();
            changed = true;
        }
        if profile.username.is_none() && sender_username.is_some() {
            profile.username = sender_username.clone();
            changed = true;
        }
        if let Some(name) = corrected_name.as_ref()
            && profile.preferred_name != *name
        {
            profile.preferred_name = name.clone();
            changed = true;
        }
        profile.updated_at = now;
        if changed {
            profile_persist_payload = Some((
                sender_user_id,
                profile.preferred_name.clone(),
                profile.username.clone(),
            ));
        }

        let sender_profile = profile.clone();
        let mut roster = chat_profiles
            .iter()
            .map(|(uid, p)| (*uid, p.clone()))
            .collect::<Vec<_>>();
        roster.sort_by(|a, b| {
            b.1.updated_at
                .cmp(&a.1.updated_at)
                .then_with(|| a.0.cmp(&b.0))
        });
        (sender_profile, roster)
    };

    if let Some((profile_user_id, preferred_name, profile_username)) = profile_persist_payload
        && let Err(err) = ctx
            .service
            .upsert_group_user_profile(
                ctx.channel_name.clone(),
                chat_id,
                profile_user_id,
                preferred_name.clone(),
                profile_username.clone(),
            )
            .await
    {
        warn!(
            chat_id,
            user_id = profile_user_id,
            preferred_name = %preferred_name,
            username = ?profile_username,
            error = %format!("{err:#}"),
            "failed to persist telegram group user profile"
        );
    }

    if let Some(name) = corrected_name.as_ref() {
        info!(
            chat_id,
            message_id,
            user_id = sender_user_id,
            corrected_name = %name,
            "telegram group sender preferred name corrected in real time"
        );
    }

    let model_input_text = build_group_member_identity_context(
        text,
        message.reply_to_message.as_ref(),
        sender_user_id,
        &sender_profile,
        &roster_entries,
        8,
    );

    GroupPreparation {
        sender_user_id,
        sender_profile,
        model_input_text,
        now,
    }
}

async fn learn_group_aliases(
    ctx: &ChannelContext,
    text: &str,
    chat_id: i64,
    bot_username: &str,
    group_runtime_state: &Arc<tokio::sync::Mutex<TelegramGroupRuntimeState>>,
) {
    let learned = extract_dynamic_alias_candidates(text, bot_username);
    if learned.is_empty() {
        return;
    }

    learn_group_alias_tokens(ctx, chat_id, learned, group_runtime_state).await;
}

async fn learn_group_alias_tokens(
    ctx: &ChannelContext,
    chat_id: i64,
    aliases: Vec<String>,
    group_runtime_state: &Arc<tokio::sync::Mutex<TelegramGroupRuntimeState>>,
) {
    if aliases.is_empty() {
        return;
    }
    let mut newly_added = Vec::new();
    let mut state = group_runtime_state.lock().await;
    let chat_aliases = state.learned_aliases_by_chat.entry(chat_id).or_default();
    for alias in aliases {
        if chat_aliases.len() >= 24 {
            break;
        }
        if chat_aliases.insert(alias.clone()) {
            newly_added.push(alias);
        }
    }
    drop(state);

    if !newly_added.is_empty()
        && let Err(err) = ctx
            .service
            .upsert_group_aliases(ctx.channel_name.clone(), chat_id, newly_added.clone())
            .await
    {
        warn!(
            chat_id,
            aliases = ?newly_added,
            error = %format!("{err:#}"),
            "failed to persist learned telegram group aliases"
        );
    }
}

async fn load_group_runtime_signals(
    chat_id: i64,
    now: u64,
    group_trigger_settings: &TelegramGroupTriggerSettings,
    group_runtime_state: &Arc<tokio::sync::Mutex<TelegramGroupRuntimeState>>,
) -> (bool, bool, Option<u64>, Vec<String>) {
    let state = group_runtime_state.lock().await;
    let elapsed = state
        .last_bot_response_unix_by_chat
        .get(&chat_id)
        .map(|last| now.saturating_sub(*last));
    let recent = elapsed
        .map(|secs| secs <= group_trigger_settings.followup_window_secs)
        .unwrap_or(false);
    let cooldown = elapsed
        .map(|secs| secs <= group_trigger_settings.cooldown_secs)
        .unwrap_or(false);
    let aliases = state
        .learned_aliases_by_chat
        .get(&chat_id)
        .map(|items| items.iter().cloned().collect::<Vec<_>>())
        .unwrap_or_default();

    (recent, cooldown, elapsed, aliases)
}

async fn record_group_decision_metrics(
    diag_state: &Arc<tokio::sync::Mutex<TelegramPollingDiagState>>,
    decision_kind: GroupDecisionKind,
) {
    let mut diag = diag_state.lock().await;
    match decision_kind {
        GroupDecisionKind::Respond => diag.group_decision_respond_total += 1,
        GroupDecisionKind::ObserveOnly => diag.group_decision_observe_total += 1,
        GroupDecisionKind::Ignore => diag.group_decision_ignore_total += 1,
    }
}
