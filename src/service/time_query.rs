use super::*;

#[derive(Debug, Clone, Copy)]
pub(super) struct TimezoneHint {
    pub(super) timezone: &'static str,
    pub(super) display: &'static str,
    pub(super) source: &'static str,
    pub(super) note: Option<&'static str>,
}

pub(super) fn extract_primary_user_text(text: &str) -> &str {
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(raw) = trimmed.strip_prefix("原始消息:") {
            let raw = raw.trim();
            if !raw.is_empty() {
                return raw;
            }
        }
    }
    text.trim()
}

pub(super) fn looks_like_time_query(text: &str) -> bool {
    let trimmed = extract_primary_user_text(text).trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    if [
        "what time",
        "current time",
        "time now",
        "today",
        "date",
        "year",
        "weekday",
        "zodiac",
        "now",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
    {
        return true;
    }
    [
        "几点", "时间", "现在", "今天", "日期", "几号", "今年", "年份", "星期", "周几", "生肖",
        "蛇年", "龙年", "马年", "羊年", "猴年", "鸡年", "狗年", "猪年", "鼠年", "牛年", "虎年",
        "兔年",
    ]
    .iter()
    .any(|kw| trimmed.contains(kw))
}

pub(super) fn is_explicit_current_time_query(text: &str) -> bool {
    let trimmed = extract_primary_user_text(text).trim();
    if trimmed.is_empty() || !looks_like_time_query(trimmed) {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    if [
        "提醒",
        "分钟后",
        "小时后",
        "之后",
        "定时",
        "闹钟",
        "every ",
        "cron",
        "remind",
        "schedule",
    ]
    .iter()
    .any(|kw| trimmed.contains(kw) || lower.contains(kw))
    {
        return false;
    }

    let explicit_cn = [
        "几点",
        "星期几",
        "周几",
        "几号",
        "日期",
        "哪年",
        "今年",
        "生肖",
    ];
    let explicit_en = [
        "what time",
        "time is it",
        "current time",
        "what date",
        "what day",
        "weekday",
        "what year",
        "zodiac",
    ];
    if explicit_cn.iter().any(|kw| trimmed.contains(kw))
        || explicit_en.iter().any(|kw| lower.contains(kw))
    {
        return true;
    }

    (trimmed.contains("现在") || lower.contains("now"))
        && (trimmed.contains("时间") || lower.contains("time"))
        && (trimmed.contains('？') || trimmed.contains('?') || trimmed.ends_with('吗'))
}

pub(super) fn infer_timezone_from_time_query(text: &str) -> Option<TimezoneHint> {
    let trimmed = extract_primary_user_text(text).trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();

    if ["美西", "洛杉矶", "pacific", "los angeles", "pst", "pdt"]
        .iter()
        .any(|kw| trimmed.contains(kw) || lower.contains(kw))
    {
        return Some(TimezoneHint {
            timezone: "America/Los_Angeles",
            display: "美国西部时间",
            source: "us_west",
            note: None,
        });
    }
    if ["美东", "纽约", "eastern", "new york", "est", "edt"]
        .iter()
        .any(|kw| trimmed.contains(kw) || lower.contains(kw))
    {
        return Some(TimezoneHint {
            timezone: "America/New_York",
            display: "美国东部时间",
            source: "us_east",
            note: None,
        });
    }
    if [
        "美国",
        "美利坚",
        "america",
        "usa",
        "us time",
        "united states",
    ]
    .iter()
    .any(|kw| trimmed.contains(kw) || lower.contains(kw))
    {
        return Some(TimezoneHint {
            timezone: "America/New_York",
            display: "美国东部时间",
            source: "us_default",
            note: Some("美国有多个时区，当前默认按美国东部时间。"),
        });
    }
    if ["中国", "国内", "北京时间", "北京", "china", "beijing"]
        .iter()
        .any(|kw| trimmed.contains(kw) || lower.contains(kw))
    {
        return Some(TimezoneHint {
            timezone: "Asia/Shanghai",
            display: "北京时间",
            source: "china",
            note: None,
        });
    }
    if ["日本", "东京", "japan", "tokyo", "jst"]
        .iter()
        .any(|kw| trimmed.contains(kw) || lower.contains(kw))
    {
        return Some(TimezoneHint {
            timezone: "Asia/Tokyo",
            display: "日本时间",
            source: "japan",
            note: None,
        });
    }
    if ["韩国", "首尔", "korea", "seoul", "kst"]
        .iter()
        .any(|kw| trimmed.contains(kw) || lower.contains(kw))
    {
        return Some(TimezoneHint {
            timezone: "Asia/Seoul",
            display: "韩国时间",
            source: "korea",
            note: None,
        });
    }
    if ["英国", "伦敦", "uk", "britain", "london"]
        .iter()
        .any(|kw| trimmed.contains(kw) || lower.contains(kw))
    {
        return Some(TimezoneHint {
            timezone: "Europe/London",
            display: "英国时间",
            source: "uk",
            note: None,
        });
    }
    if ["utc", "gmt"].iter().any(|kw| lower.contains(kw)) {
        return Some(TimezoneHint {
            timezone: "UTC",
            display: "UTC",
            source: "utc",
            note: None,
        });
    }

    None
}

pub(super) fn build_builtin_time_tool_arguments(timezone_hint: Option<TimezoneHint>) -> Value {
    match timezone_hint {
        Some(hint) => serde_json::json!({ "timezone": hint.timezone }),
        None => serde_json::json!({}),
    }
}

pub(super) fn render_fast_time_answer(
    query_text: &str,
    tool_result: &Value,
    timezone_hint: Option<TimezoneHint>,
) -> String {
    let query = extract_primary_user_text(query_text).trim();
    let lower = query.to_ascii_lowercase();
    let local_time = tool_result
        .get("local_time")
        .and_then(|v| v.as_str())
        .unwrap_or("--:--:--");
    let local_date = tool_result
        .get("local_date")
        .and_then(|v| v.as_str())
        .unwrap_or("----/--/--");
    let local_weekday = tool_result
        .get("local_weekday")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown");
    let weekday_zh = weekday_to_chinese(local_weekday);
    let local_year = tool_result
        .get("local_year")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let zodiac = zodiac_for_year(local_year as i32);
    let timezone = tool_result
        .get("timezone")
        .and_then(|v| v.as_str())
        .or_else(|| timezone_hint.map(|hint| hint.timezone))
        .unwrap_or("local");
    let timezone_display = timezone_hint.map(|hint| hint.display).unwrap_or("本地时间");

    let wants_time = ["几点", "时间", "what time", "time now", "current time"]
        .iter()
        .any(|kw| query.contains(kw) || lower.contains(kw));
    let wants_date = ["几号", "日期", "today", "date"]
        .iter()
        .any(|kw| query.contains(kw) || lower.contains(kw));
    let wants_weekday = ["星期", "周几", "weekday", "what day"]
        .iter()
        .any(|kw| query.contains(kw) || lower.contains(kw));
    let wants_year = ["今年", "年份", "哪年", "year"]
        .iter()
        .any(|kw| query.contains(kw) || lower.contains(kw));
    let wants_zodiac = [
        "生肖", "zodiac", "蛇年", "龙年", "马年", "羊年", "猴年", "鸡年", "狗年", "猪年", "鼠年",
        "牛年", "虎年", "兔年",
    ]
    .iter()
    .any(|kw| query.contains(kw) || lower.contains(kw));

    let mut parts = Vec::new();
    if wants_time || (!wants_date && !wants_weekday && !wants_year && !wants_zodiac) {
        parts.push(format!(
            "当前{}（{}）是 {}",
            timezone_display, timezone, local_time
        ));
    }
    if wants_date {
        parts.push(format!("当前日期是 {}", local_date));
    }
    if wants_weekday {
        parts.push(format!("今天是星期{}", weekday_zh));
    }
    if wants_year || wants_zodiac {
        parts.push(format!("当前年份是 {} 年（{}）", local_year, zodiac));
    }
    if parts.is_empty() {
        parts.push(format!(
            "当前{}（{}）是 {} {}，星期{}",
            timezone_display, timezone, local_date, local_time, weekday_zh
        ));
    }

    let mut answer = parts.join("；");
    if let Some(note) = timezone_hint.and_then(|hint| hint.note) {
        answer.push('。');
        answer.push_str(note);
    }
    answer
}

pub(super) fn weekday_to_chinese(weekday_en: &str) -> &'static str {
    match weekday_en {
        "Monday" => "一",
        "Tuesday" => "二",
        "Wednesday" => "三",
        "Thursday" => "四",
        "Friday" => "五",
        "Saturday" => "六",
        "Sunday" => "日",
        _ => "?",
    }
}

pub(super) fn zodiac_for_year(year: i32) -> &'static str {
    const SIGNS: [&str; 12] = [
        "鼠年", "牛年", "虎年", "兔年", "龙年", "蛇年", "马年", "羊年", "猴年", "鸡年", "狗年",
        "猪年",
    ];
    if year <= 0 {
        return "未知生肖";
    }
    let idx = (year - 4).rem_euclid(12) as usize;
    SIGNS[idx]
}
