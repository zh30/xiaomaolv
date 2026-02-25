use xiaomaolv::config::AppConfig;

#[test]
fn config_bootstrap_parses_minimal_toml() {
    let toml = r#"
[app]
bind = "127.0.0.1:8080"
default_provider = "openai"

[providers.openai]
kind = "openai-compatible"
base_url = "https://api.openai.com/v1"
api_key = "test-key"
model = "gpt-4o-mini"

[channels.http]
enabled = true
"#;

    let cfg = AppConfig::from_toml(toml).expect("config should parse");
    assert_eq!(cfg.app.default_provider, "openai");
    assert!(cfg.channels.http.enabled);
    assert!(cfg.agent.mcp_enabled);
    assert_eq!(cfg.agent.mcp_max_iterations, 4);
}

#[test]
fn config_bootstrap_parses_telegram_streaming_fields() {
    let toml = r#"
[app]
bind = "127.0.0.1:8080"
default_provider = "openai"

[providers.openai]
kind = "openai-compatible"
base_url = "https://api.openai.com/v1"
api_key = "test-key"
model = "gpt-4o-mini"

[channels.http]
enabled = true

[channels.telegram]
enabled = true
bot_token = "t"
mode = "polling"
streaming_enabled = false
streaming_edit_interval_ms = 1500
streaming_prefer_draft = false
"#;

    let cfg = AppConfig::from_toml(toml).expect("config should parse");
    let tg = cfg.channels.telegram.expect("telegram config");
    assert!(!tg.streaming_enabled);
    assert_eq!(tg.streaming_edit_interval_ms, 1500);
    assert!(!tg.streaming_prefer_draft);
    assert!(!tg.startup_online_enabled);
    assert_eq!(tg.startup_online_text, "online");
    assert!(tg.commands_enabled);
    assert!(tg.commands_auto_register);
    assert!(tg.commands_private_only);
    assert!(tg.admin_user_ids.to_csv().is_empty());
    assert_eq!(tg.group_trigger_mode, "smart");
    assert_eq!(tg.group_followup_window_secs, 180);
    assert_eq!(tg.group_cooldown_secs, 20);
    assert_eq!(tg.group_rule_min_score, 70);
    assert!(!tg.group_llm_gate_enabled);
}

#[test]
fn config_bootstrap_parses_telegram_command_fields() {
    let toml = r#"
[app]
bind = "127.0.0.1:8080"
default_provider = "openai"

[providers.openai]
kind = "openai-compatible"
base_url = "https://api.openai.com/v1"
api_key = "test-key"
model = "gpt-4o-mini"

[channels.http]
enabled = true

[channels.telegram]
enabled = true
bot_token = "t"
commands_enabled = true
commands_auto_register = false
commands_private_only = true
admin_user_ids = [101, 202]
"#;

    let cfg = AppConfig::from_toml(toml).expect("config should parse");
    let tg = cfg.channels.telegram.expect("telegram config");
    assert!(tg.commands_enabled);
    assert!(!tg.commands_auto_register);
    assert!(tg.commands_private_only);
    assert_eq!(tg.admin_user_ids.to_csv(), "101,202".to_string());
}

#[test]
fn config_bootstrap_parses_telegram_group_trigger_fields() {
    let toml = r#"
[app]
bind = "127.0.0.1:8080"
default_provider = "openai"

[providers.openai]
kind = "openai-compatible"
base_url = "https://api.openai.com/v1"
api_key = "test-key"
model = "gpt-4o-mini"

[channels.http]
enabled = true

[channels.telegram]
enabled = true
bot_token = "t"
group_trigger_mode = "smart"
group_followup_window_secs = 240
group_cooldown_secs = 45
group_rule_min_score = 66
group_llm_gate_enabled = true
"#;

    let cfg = AppConfig::from_toml(toml).expect("config should parse");
    let tg = cfg.channels.telegram.expect("telegram config");
    assert_eq!(tg.group_trigger_mode, "smart");
    assert_eq!(tg.group_followup_window_secs, 240);
    assert_eq!(tg.group_cooldown_secs, 45);
    assert_eq!(tg.group_rule_min_score, 66);
    assert!(tg.group_llm_gate_enabled);
}

#[test]
fn config_bootstrap_supports_telegram_admin_ids_from_env_csv() {
    let toml = r#"
[app]
bind = "127.0.0.1:8080"
default_provider = "openai"

[providers.openai]
kind = "openai-compatible"
base_url = "https://api.openai.com/v1"
api_key = "test-key"
model = "gpt-4o-mini"

[channels.http]
enabled = true

[channels.telegram]
enabled = true
bot_token = "t"
admin_user_ids = "${TELEGRAM_ADMIN_USER_IDS:-}"
"#;

    // Keep test deterministic by using fallback value syntax.
    let cfg = AppConfig::from_toml(toml).expect("config should parse");
    let tg = cfg.channels.telegram.expect("telegram config");
    assert_eq!(tg.admin_user_ids.to_csv(), "".to_string());
}

#[test]
fn config_bootstrap_resolves_group_trigger_mode_from_env_fallback() {
    let toml = r#"
[app]
bind = "127.0.0.1:8080"
default_provider = "openai"

[providers.openai]
kind = "openai-compatible"
base_url = "https://api.openai.com/v1"
api_key = "test-key"
model = "gpt-4o-mini"

[channels.http]
enabled = true

[channels.telegram]
enabled = true
bot_token = "t"
group_trigger_mode = "${TELEGRAM_GROUP_TRIGGER_MODE:-smart}"
"#;

    let cfg = AppConfig::from_toml(toml).expect("config should parse");
    let tg = cfg.channels.telegram.expect("telegram config");
    assert_eq!(tg.group_trigger_mode, "smart");
}
