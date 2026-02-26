use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use super::{
    GroupDecision, GroupDecisionKind, GroupSignalInput, TelegramGroupTriggerMode,
    TelegramGroupUserProfile, TelegramReplyMessage, TelegramUser, truncate_chars,
};

pub(super) fn message_contains_any_alias(text: &str, aliases: &[String]) -> bool {
    if aliases.is_empty() {
        return false;
    }
    let lowered = text.to_lowercase();
    aliases.iter().any(|alias| {
        let trimmed = alias.trim().to_lowercase();
        if trimmed.is_empty() {
            return false;
        }
        lowered.contains(&trimmed)
    })
}

pub(super) fn message_has_question_marker(text: &str) -> bool {
    if text.contains('?') || text.contains('？') {
        return true;
    }
    let tail = text
        .trim()
        .trim_end_matches(|ch: char| "，,。.!！？?;；:：)）]】}>》\"'`~|/\\ ".contains(ch));
    tail.ends_with('吗') || tail.ends_with('么')
}

pub(super) fn message_mentions_other_handle(text: &str, bot_username: &str) -> bool {
    let own = bot_username.trim().trim_start_matches('@').to_lowercase();
    if own.is_empty() {
        return false;
    }
    text.split(|ch: char| ch.is_whitespace() || ",.!?，。！？;:()[]{}<>\"'".contains(ch))
        .filter_map(|token| token.strip_prefix('@'))
        .map(|name| {
            name.trim_matches(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                .to_lowercase()
        })
        .any(|name| !name.is_empty() && name != own)
}

pub(super) fn is_low_signal_group_noise(text: &str) -> bool {
    let compact = text.trim();
    if compact.is_empty() {
        return true;
    }
    let len = compact.chars().count();
    len <= 2
}

pub(super) fn evaluate_group_decision(
    mode: TelegramGroupTriggerMode,
    input: &GroupSignalInput,
    min_score: u32,
) -> GroupDecision {
    if input.mentioned {
        return GroupDecision {
            kind: GroupDecisionKind::Respond,
            score: 100,
            reasons: vec!["explicit_mention"],
        };
    }
    if input.replied_to_bot {
        return GroupDecision {
            kind: GroupDecisionKind::Respond,
            score: 100,
            reasons: vec!["reply_to_bot"],
        };
    }
    if matches!(mode, TelegramGroupTriggerMode::Strict) {
        return GroupDecision {
            kind: GroupDecisionKind::Ignore,
            score: 0,
            reasons: vec!["strict_no_explicit_trigger"],
        };
    }

    let mut score: i32 = 0;
    let mut reasons = Vec::new();
    if input.recent_bot_participation {
        score += 30;
        reasons.push("recent_bot_participation");
    }
    if input.alias_hit {
        score += 25;
        reasons.push("alias_hit");
    }
    if input.vocative_alias_hit {
        score += 75;
        reasons.push("vocative_alias_hit");
    }
    if input.has_question_marker {
        score += 20;
        reasons.push("question_marker");
    }
    if input.points_to_other_bot {
        score -= 35;
        reasons.push("points_to_other_bot");
    }
    if input.low_signal_noise {
        score -= 25;
        reasons.push("low_signal_noise");
    }

    let min = min_score.clamp(1, 100) as i32;
    let observe_min = (min / 2).max(30);
    let mut kind = if score >= min {
        GroupDecisionKind::Respond
    } else if score >= observe_min {
        GroupDecisionKind::ObserveOnly
    } else {
        GroupDecisionKind::Ignore
    };
    if input.cooldown_active
        && matches!(kind, GroupDecisionKind::Respond)
        && !input.vocative_alias_hit
    {
        kind = GroupDecisionKind::ObserveOnly;
        reasons.push("cooldown_active");
    }

    GroupDecision {
        kind,
        score,
        reasons,
    }
}

pub(super) fn is_group_chat_type(kind: Option<&str>) -> bool {
    matches!(
        kind.map(|v| v.trim().to_ascii_lowercase()),
        Some(v) if v == "group" || v == "supergroup"
    )
}

pub(super) fn is_reply_to_bot_message(
    reply: Option<&TelegramReplyMessage>,
    bot_user_id: Option<i64>,
) -> bool {
    reply
        .and_then(|msg| msg.from.as_ref())
        .map(|from| {
            from.is_bot.unwrap_or(false)
                || bot_user_id.map(|bot_id| bot_id == from.id).unwrap_or(false)
        })
        .unwrap_or(false)
}

pub(super) fn normalize_telegram_username(raw: Option<&str>) -> Option<String> {
    raw.map(|v| v.trim().trim_start_matches('@').to_string())
        .filter(|v| !v.is_empty())
}

pub(super) fn normalize_display_name(raw: &str) -> Option<String> {
    let normalized = raw
        .trim()
        .trim_start_matches('@')
        .trim_matches(|ch: char| ",，。.!！？?;；:：()[]{}<>\"'`~|/\\ ".contains(ch))
        .trim();
    if normalized.is_empty() {
        return None;
    }
    let len = normalized.chars().count();
    if !(1..=24).contains(&len) {
        return None;
    }
    if normalized.contains('\n') || normalized.contains('\r') || normalized.contains('\t') {
        return None;
    }
    let lowered = normalized.to_ascii_lowercase();
    if matches!(
        lowered.as_str(),
        "me" | "myself" | "bot" | "robot" | "assistant" | "question" | "reply" | "answer" | "sleep"
    ) {
        return None;
    }
    if matches!(
        normalized,
        "我" | "我们"
            | "你"
            | "你们"
            | "他"
            | "她"
            | "它"
            | "大家"
            | "群友"
            | "机器人"
            | "助手"
            | "问题"
            | "一下"
            | "起床"
            | "看看"
            | "帮忙"
            | "分析"
            | "解释"
            | "回复"
            | "回答"
            | "处理"
            | "来问"
            | "问下"
    ) {
        return None;
    }
    if [
        "来问", "请教", "问题", "帮忙", "看看", "分析", "解释", "回复", "回答", "处理",
    ]
    .iter()
    .any(|kw| normalized.contains(kw))
    {
        return None;
    }
    Some(normalized.to_string())
}

pub(super) fn derive_telegram_user_display_name(user: Option<&TelegramUser>) -> Option<String> {
    let user = user?;

    let first = user.first_name.as_deref().unwrap_or_default().trim();
    let last = user.last_name.as_deref().unwrap_or_default().trim();
    if !first.is_empty() || !last.is_empty() {
        let full = if !first.is_empty() && !last.is_empty() {
            format!("{first} {last}")
        } else if !first.is_empty() {
            first.to_string()
        } else {
            last.to_string()
        };
        if let Some(name) = normalize_display_name(&full) {
            return Some(name);
        }
    }

    if let Some(username) = normalize_telegram_username(user.username.as_deref()) {
        return normalize_display_name(&username);
    }

    None
}

pub(super) fn extract_name_candidate_from_tail(tail: &str) -> Option<String> {
    let trimmed = tail.trim_start_matches(|ch: char| {
        ch.is_whitespace() || "，,。.!！？?;；:：()[]{}<>\"'`~|/\\-=".contains(ch)
    });
    if trimmed.is_empty() {
        return None;
    }

    let end = trimmed
        .char_indices()
        .find_map(|(idx, ch)| {
            (ch.is_whitespace() || "，,。.!！？?;；:：()[]{}<>\"'`~|/\\=".contains(ch))
                .then_some(idx)
        })
        .unwrap_or(trimmed.len());
    normalize_display_name(&trimmed[..end])
}

pub(super) fn extract_realtime_name_correction(text: &str) -> Option<String> {
    let compact = text.trim();
    if compact.is_empty() {
        return None;
    }

    let markers = [
        ("请叫我", true),
        ("叫我", true),
        ("我叫", false),
        ("我是", false),
    ];
    for (marker, strong_signal) in markers {
        let Some(start) = compact.rfind(marker) else {
            continue;
        };
        if !strong_signal {
            let allow_weak = compact.chars().count() <= 20
                || compact.contains("不是")
                || compact.contains("别叫")
                || compact.contains("叫错");
            if !allow_weak {
                continue;
            }
        }
        let tail = &compact[start + marker.len()..];
        if let Some(candidate) = extract_name_candidate_from_tail(tail) {
            return Some(candidate);
        }
    }
    None
}

pub(super) fn build_group_member_identity_context(
    raw_text: &str,
    reply_to_message: Option<&TelegramReplyMessage>,
    sender_user_id: i64,
    sender_profile: &TelegramGroupUserProfile,
    roster: &[(i64, TelegramGroupUserProfile)],
    max_roster: usize,
) -> String {
    let mut lines = Vec::new();
    lines.push("[群成员身份映射]".to_string());
    lines.push(format!("当前发言账号: uid={sender_user_id}"));
    lines.push(format!("当前发言称呼: {}", sender_profile.preferred_name));
    if let Some(username) = sender_profile.username.as_deref() {
        lines.push(format!("当前发言用户名: @{username}"));
    }
    lines.push("账号称呼映射:".to_string());

    for (uid, profile) in roster.iter().take(max_roster.max(1)) {
        if let Some(username) = profile.username.as_deref() {
            lines.push(format!(
                "uid={uid} -> {} (@{username})",
                profile.preferred_name
            ));
        } else {
            lines.push(format!("uid={uid} -> {}", profile.preferred_name));
        }
    }
    lines.push(
        "规则: 回复点名时，称呼必须和 uid 对应；若用户纠正称呼，立即使用最新称呼。".to_string(),
    );
    if let Some(reply) = reply_to_message {
        lines.push("[被回复消息]".to_string());
        lines.push(format!("被回复消息ID: {}", reply.message_id));
        if let Some(from) = reply.from.as_ref() {
            if let Some(name) = derive_telegram_user_display_name(Some(from)) {
                lines.push(format!("被回复消息发送者: {}", name));
            } else {
                lines.push(format!("被回复消息发送者uid: {}", from.id));
            }
        }
        let quoted = reply
            .text
            .as_deref()
            .or(reply.caption.as_deref())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|v| truncate_chars(v, 220));
        if let Some(quoted) = quoted {
            lines.push(format!("被回复消息内容: {}", quoted));
        } else {
            lines.push("被回复消息内容: <不可用>".to_string());
        }
        lines.push("[/被回复消息]".to_string());
    }
    lines.push(format!("原始消息: {}", raw_text.trim()));
    lines.push("[/群成员身份映射]".to_string());
    lines.join("\n")
}

pub(super) fn normalize_alias_token(raw: &str) -> String {
    raw.trim()
        .trim_matches(|ch: char| ch.is_whitespace() || ",，:：;；.!?！？()[]{}<>\"'@".contains(ch))
        .to_lowercase()
}

pub(super) fn extract_dynamic_alias_candidates(text: &str, bot_username: &str) -> Vec<String> {
    let own = bot_username.trim().trim_start_matches('@').to_lowercase();
    if own.is_empty() {
        return vec![];
    }
    let tokens = text
        .split(|ch: char| ch.is_whitespace() || ",，:：;；.!?！？()[]{}<>\"'".contains(ch))
        .map(normalize_alias_token)
        .filter(|v| !v.is_empty())
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return vec![];
    }

    let mut out = Vec::new();
    for (idx, token) in tokens.iter().enumerate() {
        if token != &own {
            continue;
        }
        if idx > 0 {
            let candidate = &tokens[idx - 1];
            if let Some(normalized) = normalize_dynamic_alias_candidate(candidate, &own)
                && !out.contains(&normalized)
            {
                out.push(normalized);
            }
        }
        if idx + 1 < tokens.len() {
            let candidate = &tokens[idx + 1];
            if let Some(normalized) = normalize_dynamic_alias_candidate(candidate, &own)
                && !out.contains(&normalized)
            {
                out.push(normalized);
            }
        }
    }

    if out.is_empty() {
        let first = &tokens[0];
        if let Some(normalized) = normalize_dynamic_alias_candidate(first, &own) {
            out.push(normalized);
        }
    }
    out
}

fn normalize_dynamic_alias_candidate(candidate: &str, bot_username: &str) -> Option<String> {
    let normalized = normalize_alias_token(candidate);
    if normalized.is_empty() {
        return None;
    }

    if let Some(idx) = normalized.rfind("叫")
        && idx + "叫".len() < normalized.len()
    {
        let tail = &normalized[idx + "叫".len()..];
        if let Some(name) = extract_name_candidate_from_tail(tail)
            && is_dynamic_alias_candidate(&name, bot_username)
        {
            return Some(name);
        }
    }

    if is_dynamic_alias_candidate(&normalized, bot_username) {
        return Some(normalized);
    }
    None
}

pub(super) fn extract_vocative_alias_candidate(text: &str, bot_username: &str) -> Option<String> {
    let own = bot_username.trim().trim_start_matches('@').to_lowercase();
    if own.is_empty() {
        return None;
    }
    let compact = text.trim_start();
    if compact.is_empty() {
        return None;
    }

    let mut split: Option<(usize, char)> = None;
    for (idx, ch) in compact.char_indices() {
        if ch.is_whitespace()
            || "，,。.!！？?;；:：()[]{}<>\"'`~|/\\-=".contains(ch)
            || (idx > 0 && is_vocative_follow_char(ch))
        {
            split = Some((idx, ch));
            break;
        }
    }
    let (idx, sep) = split?;
    if idx == 0 {
        return None;
    }
    if !matches!(sep, '，' | ',' | '：' | ':' | '！' | '!' | '？' | '?')
        && !is_vocative_follow_char(sep)
    {
        return None;
    }

    let candidate = normalize_alias_token(&compact[..idx]);
    if candidate.is_empty() {
        return None;
    }
    if normalize_display_name(&candidate).is_none() {
        return None;
    }
    if !is_dynamic_alias_candidate(&candidate, &own) {
        return None;
    }
    Some(candidate)
}

pub(super) fn detect_vocative_learned_alias_prefix(
    text: &str,
    aliases: &[String],
) -> Option<String> {
    if aliases.is_empty() {
        return None;
    }
    let compact = text.trim_start();
    if compact.is_empty() {
        return None;
    }
    let compact_lowered = compact.to_lowercase();

    let mut candidates = aliases
        .iter()
        .map(|v| v.trim().to_lowercase())
        .filter(|v| !v.is_empty())
        .collect::<Vec<_>>();
    candidates.sort_by_key(|v| std::cmp::Reverse(v.chars().count()));

    for alias in candidates {
        if !compact_lowered.starts_with(&alias) {
            continue;
        }
        let next = compact.chars().nth(alias.chars().count());
        if next.is_none_or(|ch| {
            ch.is_whitespace()
                || "，,。.!！？?;；:：()[]{}<>\"'`~|/\\-=".contains(ch)
                || is_vocative_follow_char(ch)
        }) {
            return Some(alias);
        }
    }
    None
}

fn is_vocative_follow_char(ch: char) -> bool {
    if ch.is_ascii_digit() {
        return true;
    }
    matches!(
        ch,
        '你' | '您'
            | '在'
            | '来'
            | '给'
            | '帮'
            | '说'
            | '讲'
            | '聊'
            | '能'
            | '可'
            | '会'
            | '要'
            | '别'
            | '快'
            | '咋'
            | '怎'
            | '么'
            | '吗'
            | '呢'
            | '呀'
            | '啊'
            | '哈'
            | '吧'
            | '啦'
            | '喽'
    )
}

pub(super) fn is_dynamic_alias_candidate(candidate: &str, bot_username: &str) -> bool {
    if candidate.is_empty() || candidate.starts_with('@') || candidate == bot_username {
        return false;
    }
    let len = candidate.chars().count();
    if !(1..=16).contains(&len) {
        return false;
    }
    let has_alpha = candidate
        .chars()
        .any(|ch| ch.is_alphabetic() || ('\u{4e00}'..='\u{9fff}').contains(&ch));
    if !has_alpha {
        return false;
    }
    !matches!(
        candidate,
        "hi" | "hello"
            | "hey"
            | "你好"
            | "您好"
            | "请问"
            | "请教"
            | "bot"
            | "机器人"
            | "助手"
            | "我擦"
            | "卧槽"
            | "我去"
            | "wc"
            | "woc"
    )
}

pub(super) fn message_mentions_bot(text: &str, bot_username: &str) -> bool {
    let username = bot_username
        .trim()
        .trim_start_matches('@')
        .to_ascii_lowercase();
    if username.is_empty() {
        return false;
    }
    let needle = format!("@{username}");
    let hay = text.to_ascii_lowercase();
    let mut start = 0usize;
    while let Some(found) = hay[start..].find(&needle) {
        let idx = start + found;
        let after_idx = idx + needle.len();
        let before = idx
            .checked_sub(1)
            .and_then(|i| hay.as_bytes().get(i))
            .copied();
        let after = hay.as_bytes().get(after_idx).copied();
        if is_mention_boundary(before) && is_mention_boundary(after) {
            return true;
        }
        start = after_idx;
    }
    false
}

fn is_mention_boundary(ch: Option<u8>) -> bool {
    match ch {
        None => true,
        Some(v) => {
            let c = v as char;
            !(c.is_ascii_alphanumeric() || c == '_')
        }
    }
}

pub(super) fn telegram_session_id(
    chat_id: i64,
    message_thread_id: Option<i64>,
    reply_to_message_id: Option<i64>,
) -> String {
    if let Some(thread_id) = message_thread_id {
        return format!("tg:{chat_id}:thread:{thread_id}");
    }
    if let Some(reply_id) = reply_to_message_id {
        return format!("tg:{chat_id}:reply:{reply_id}");
    }
    format!("tg:{chat_id}")
}

pub(super) fn typing_action_payload(chat_id: i64, message_thread_id: Option<i64>) -> Value {
    let mut payload = serde_json::json!({
        "chat_id": chat_id,
        "action": "typing"
    });
    if let Some(thread_id) = message_thread_id {
        payload["message_thread_id"] = serde_json::json!(thread_id);
    }
    payload
}

pub(super) fn short_description_payload(short_description: &str) -> Value {
    serde_json::json!({
        "short_description": short_description
    })
}

pub(super) fn current_unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(super) fn current_unix_timestamp_i64() -> i64 {
    current_unix_timestamp() as i64
}

pub(super) fn build_draft_message_id(chat_id: i64, message_thread_id: Option<i64>) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let thread = message_thread_id.unwrap_or(0);
    format!("xm-{chat_id}-{thread}-{millis}")
}
