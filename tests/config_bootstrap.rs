use xiaomaolv::code_mode::CodeModeExecutionMode;
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
    assert_eq!(cfg.app.locale, "en-US");
    assert!(cfg.channels.http.enabled);
    assert!(cfg.channels.http.diag_bearer_token.is_none());
    assert_eq!(cfg.channels.http.diag_rate_limit_per_minute, 120);
    assert!(cfg.agent.mcp_enabled);
    assert_eq!(cfg.agent.mcp_max_iterations, 4);
    assert!(cfg.agent.skills_enabled);
    assert_eq!(cfg.agent.skills_max_selected, 3);
    assert_eq!(cfg.agent.skills_max_prompt_chars, 8000);
    assert_eq!(cfg.agent.skills_match_min_score, 0.45);
    assert!(!cfg.agent.skills_llm_rerank_enabled);
    assert!(cfg.agent.swarm.enabled);
    assert!(cfg.agent.swarm.auto_detect);
    assert_eq!(cfg.agent.swarm.max_depth, 3);
    assert_eq!(cfg.agent.swarm.max_agents, 16);
    assert_eq!(cfg.agent.swarm.max_parallel, 4);
    assert_eq!(cfg.agent.swarm.max_node_timeout_ms, 20_000);
    assert_eq!(cfg.agent.swarm.max_run_timeout_ms, 90_000);
    assert!(cfg.agent.swarm.reply_summary_enabled);
    assert_eq!(cfg.agent.swarm.audit_retention_days, 30);
    assert!(!cfg.agent.code_mode.enabled);
    assert!(cfg.agent.code_mode.shadow_mode);
    assert_eq!(cfg.agent.code_mode.max_calls, 6);
    assert_eq!(cfg.agent.code_mode.max_parallel, 2);
    assert_eq!(cfg.agent.code_mode.max_runtime_ms, 2500);
    assert_eq!(cfg.agent.code_mode.max_call_timeout_ms, 1200);
    assert_eq!(cfg.agent.code_mode.timeout_warn_ratio, 0.4);
    assert!(!cfg.agent.code_mode.timeout_auto_shadow_enabled);
    assert_eq!(cfg.agent.code_mode.timeout_auto_shadow_probe_every, 5);
    assert_eq!(cfg.agent.code_mode.timeout_auto_shadow_streak, 3);
    assert_eq!(cfg.agent.code_mode.max_result_chars, 12000);
    assert_eq!(
        cfg.agent.code_mode.execution_mode,
        CodeModeExecutionMode::Local
    );
    assert_eq!(cfg.agent.code_mode.subprocess_timeout_secs, 8);
    assert!(!cfg.agent.code_mode.allow_network);
    assert!(!cfg.agent.code_mode.allow_filesystem);
    assert!(!cfg.agent.code_mode.allow_env);
}

#[test]
fn config_bootstrap_resolves_app_locale_from_env_fallback() {
    let toml = r#"
[app]
bind = "127.0.0.1:8080"
default_provider = "openai"
locale = "${XIAOMAOLV_LOCALE:-zh-CN}"

[providers.openai]
kind = "openai-compatible"
base_url = "https://api.openai.com/v1"
api_key = "test-key"
model = "gpt-4o-mini"

[channels.http]
enabled = true
"#;

    let cfg = AppConfig::from_toml(toml).expect("config should parse");
    assert_eq!(cfg.app.locale, "zh-CN");
}

#[test]
fn config_bootstrap_parses_http_diag_bearer_token() {
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
diag_bearer_token = "${HTTP_DIAG_BEARER_TOKEN:-diag-secret}"
diag_rate_limit_per_minute = 7
"#;

    let cfg = AppConfig::from_toml(toml).expect("config should parse");
    assert_eq!(
        cfg.channels.http.diag_bearer_token.as_deref(),
        Some("diag-secret")
    );
    assert_eq!(cfg.channels.http.diag_rate_limit_per_minute, 7);
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
    assert!(tg.scheduler_enabled);
    assert_eq!(tg.scheduler_tick_secs, 2);
    assert_eq!(tg.scheduler_batch_size, 8);
    assert_eq!(tg.scheduler_lease_secs, 30);
    assert_eq!(tg.scheduler_default_timezone, "Asia/Shanghai");
    assert!(tg.scheduler_nl_enabled);
    assert_eq!(tg.scheduler_nl_min_confidence, 0.78);
    assert!(tg.scheduler_require_confirm);
    assert_eq!(tg.scheduler_max_jobs_per_owner, 64);
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

#[test]
fn config_bootstrap_parses_telegram_scheduler_fields() {
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
scheduler_enabled = false
scheduler_tick_secs = 5
scheduler_batch_size = 20
scheduler_lease_secs = 45
scheduler_default_timezone = "${TELEGRAM_SCHEDULER_DEFAULT_TIMEZONE:-Asia/Tokyo}"
scheduler_nl_enabled = false
scheduler_nl_min_confidence = 0.9
scheduler_require_confirm = false
scheduler_max_jobs_per_owner = 120
"#;

    let cfg = AppConfig::from_toml(toml).expect("config should parse");
    let tg = cfg.channels.telegram.expect("telegram config");
    assert!(!tg.scheduler_enabled);
    assert_eq!(tg.scheduler_tick_secs, 5);
    assert_eq!(tg.scheduler_batch_size, 20);
    assert_eq!(tg.scheduler_lease_secs, 45);
    assert_eq!(tg.scheduler_default_timezone, "Asia/Tokyo");
    assert!(!tg.scheduler_nl_enabled);
    assert_eq!(tg.scheduler_nl_min_confidence, 0.9);
    assert!(!tg.scheduler_require_confirm);
    assert_eq!(tg.scheduler_max_jobs_per_owner, 120);
}

#[test]
fn config_bootstrap_parses_memory_budget_and_hybrid_keyword_fields() {
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

[memory]
backend = "hybrid-sqlite-zvec"
max_recent_turns = 18
max_semantic_memories = 6
semantic_lookback_days = 45
context_window_tokens = 120000
context_reserved_tokens = 6000
hybrid_keyword_enabled = true
hybrid_keyword_topk = 12
hybrid_keyword_candidate_limit = 300
hybrid_memory_snippet_max_chars = 360
hybrid_min_score = 0.22
context_memory_budget_ratio = 42
context_min_recent_messages = 10
"#;

    let cfg = AppConfig::from_toml(toml).expect("config should parse");
    assert_eq!(cfg.memory.backend, "hybrid-sqlite-zvec");
    assert_eq!(cfg.memory.max_recent_turns, 18);
    assert_eq!(cfg.memory.max_semantic_memories, 6);
    assert_eq!(cfg.memory.semantic_lookback_days, 45);
    assert_eq!(cfg.memory.context_window_tokens, 120000);
    assert_eq!(cfg.memory.context_reserved_tokens, 6000);
    assert!(cfg.memory.hybrid_keyword_enabled);
    assert_eq!(cfg.memory.hybrid_keyword_topk, 12);
    assert_eq!(cfg.memory.hybrid_keyword_candidate_limit, 300);
    assert_eq!(cfg.memory.hybrid_memory_snippet_max_chars, 360);
    assert_eq!(cfg.memory.hybrid_min_score, 0.22);
    assert_eq!(cfg.memory.context_memory_budget_ratio, 42);
    assert_eq!(cfg.memory.context_min_recent_messages, 10);
}

#[test]
fn config_bootstrap_parses_agent_skills_fields() {
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

[agent]
mcp_enabled = false
mcp_max_iterations = 2
mcp_max_tool_result_chars = 1000
skills_enabled = false
skills_max_selected = 5
skills_max_prompt_chars = 12000
skills_match_min_score = 0.66
skills_llm_rerank_enabled = true

[agent.swarm]
enabled = false
auto_detect = false
max_depth = 5
max_agents = 32
max_parallel = 8
max_node_timeout_ms = 30000
max_run_timeout_ms = 180000
reply_summary_enabled = false
audit_retention_days = 7

[agent.code_mode]
enabled = true
shadow_mode = false
max_calls = 10
max_parallel = 4
max_runtime_ms = 4000
max_call_timeout_ms = 900
timeout_warn_ratio = 0.6
timeout_auto_shadow_enabled = true
timeout_auto_shadow_probe_every = 7
timeout_auto_shadow_streak = 5
max_result_chars = 20000
execution_mode = "subprocess"
subprocess_timeout_secs = 15
allow_network = false
allow_filesystem = false
allow_env = false
"#;

    let cfg = AppConfig::from_toml(toml).expect("config should parse");
    assert!(!cfg.agent.mcp_enabled);
    assert_eq!(cfg.agent.mcp_max_iterations, 2);
    assert_eq!(cfg.agent.mcp_max_tool_result_chars, 1000);
    assert!(!cfg.agent.skills_enabled);
    assert_eq!(cfg.agent.skills_max_selected, 5);
    assert_eq!(cfg.agent.skills_max_prompt_chars, 12000);
    assert_eq!(cfg.agent.skills_match_min_score, 0.66);
    assert!(cfg.agent.skills_llm_rerank_enabled);
    assert!(!cfg.agent.swarm.enabled);
    assert!(!cfg.agent.swarm.auto_detect);
    assert_eq!(cfg.agent.swarm.max_depth, 5);
    assert_eq!(cfg.agent.swarm.max_agents, 32);
    assert_eq!(cfg.agent.swarm.max_parallel, 8);
    assert_eq!(cfg.agent.swarm.max_node_timeout_ms, 30_000);
    assert_eq!(cfg.agent.swarm.max_run_timeout_ms, 180_000);
    assert!(!cfg.agent.swarm.reply_summary_enabled);
    assert_eq!(cfg.agent.swarm.audit_retention_days, 7);
    assert!(cfg.agent.code_mode.enabled);
    assert!(!cfg.agent.code_mode.shadow_mode);
    assert_eq!(cfg.agent.code_mode.max_calls, 10);
    assert_eq!(cfg.agent.code_mode.max_parallel, 4);
    assert_eq!(cfg.agent.code_mode.max_runtime_ms, 4000);
    assert_eq!(cfg.agent.code_mode.max_call_timeout_ms, 900);
    assert_eq!(cfg.agent.code_mode.timeout_warn_ratio, 0.6);
    assert!(cfg.agent.code_mode.timeout_auto_shadow_enabled);
    assert_eq!(cfg.agent.code_mode.timeout_auto_shadow_probe_every, 7);
    assert_eq!(cfg.agent.code_mode.timeout_auto_shadow_streak, 5);
    assert_eq!(cfg.agent.code_mode.max_result_chars, 20000);
    assert_eq!(
        cfg.agent.code_mode.execution_mode,
        CodeModeExecutionMode::Subprocess
    );
    assert_eq!(cfg.agent.code_mode.subprocess_timeout_secs, 15);
    assert!(!cfg.agent.code_mode.allow_network);
    assert!(!cfg.agent.code_mode.allow_filesystem);
    assert!(!cfg.agent.code_mode.allow_env);
}
