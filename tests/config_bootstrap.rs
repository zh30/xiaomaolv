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
}
