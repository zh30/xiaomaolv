use std::fs;

use axum_test::TestServer;
use tempfile::TempDir;
use xiaomaolv::http::build_router_with_config_paths;

#[tokio::test]
async fn setup_page_and_state_reflect_first_time() {
    let td = TempDir::new().expect("temp dir");
    let config_path = td.path().join("xiaomaolv.toml");
    let env_path = td.path().join(".env.realtest");

    fs::write(
        &config_path,
        r#"
[app]
bind = "127.0.0.1:0"
default_provider = "openai"

[providers.openai]
kind = "openai-compatible"
base_url = "http://127.0.0.1:9999/v1"
api_key = "${MINIMAX_API_KEY}"
model = "${MINIMAX_MODEL:-MiniMax-M2.5-highspeed}"

[channels.http]
enabled = true

[channels.telegram]
enabled = false
bot_token = "${TELEGRAM_BOT_TOKEN}"
"#,
    )
    .expect("write config");

    let app = build_router_with_config_paths(&config_path, &env_path, "sqlite::memory:", None)
        .await
        .expect("router");
    let server = TestServer::new(app).expect("test server");

    let page = server.get("/setup").await;
    page.assert_status_ok();
    page.assert_text_contains("配置中心");

    let state = server.get("/v1/config/ui/state").await;
    state.assert_status_ok();

    let payload: serde_json::Value = state.json();
    assert_eq!(
        payload.get("first_time").and_then(|v| v.as_bool()),
        Some(true)
    );

    let required = payload
        .get("required_keys")
        .and_then(|v| v.as_array())
        .expect("required_keys array");
    assert!(
        required
            .iter()
            .any(|k| k.as_str() == Some("MINIMAX_API_KEY"))
    );
    assert!(
        required
            .iter()
            .any(|k| k.as_str() == Some("TELEGRAM_BOT_TOKEN"))
    );
}

#[tokio::test]
async fn save_config_persists_env_and_clears_first_time() {
    let td = TempDir::new().expect("temp dir");
    let config_path = td.path().join("xiaomaolv.toml");
    let env_path = td.path().join(".env.realtest");

    fs::write(
        &config_path,
        r#"
[app]
bind = "127.0.0.1:0"
default_provider = "openai"

[providers.openai]
kind = "openai-compatible"
base_url = "http://127.0.0.1:9999/v1"
api_key = "${MINIMAX_API_KEY}"
model = "${MINIMAX_MODEL:-MiniMax-M2.5-highspeed}"

[channels.http]
enabled = true

[channels.telegram]
enabled = false
bot_token = "${TELEGRAM_BOT_TOKEN}"
"#,
    )
    .expect("write config");

    let app = build_router_with_config_paths(&config_path, &env_path, "sqlite::memory:", None)
        .await
        .expect("router");
    let server = TestServer::new(app).expect("test server");

    let save = server
        .post("/v1/config/ui/save")
        .json(&serde_json::json!({
            "values": {
                "MINIMAX_API_KEY": "mm-test-key",
                "TELEGRAM_BOT_TOKEN": "tg-test-token",
                "MINIMAX_MODEL": "MiniMax-M2.5-highspeed"
            },
            "mode": "required"
        }))
        .await;

    save.assert_status_ok();
    let save_payload: serde_json::Value = save.json();
    assert_eq!(
        save_payload.get("saved").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        save_payload
            .get("runtime_reloaded")
            .and_then(|v| v.as_bool()),
        Some(true)
    );

    let saved_env = fs::read_to_string(&env_path).expect("saved env exists");
    assert!(saved_env.contains("MINIMAX_API_KEY=mm-test-key"));
    assert!(saved_env.contains("TELEGRAM_BOT_TOKEN=tg-test-token"));

    let state = server.get("/v1/config/ui/state").await;
    state.assert_status_ok();
    let payload: serde_json::Value = state.json();
    assert_eq!(
        payload.get("first_time").and_then(|v| v.as_bool()),
        Some(false)
    );

    // SAFETY: test cleanup restores process env keys used only in this test.
    unsafe {
        std::env::remove_var("MINIMAX_API_KEY");
        std::env::remove_var("TELEGRAM_BOT_TOKEN");
        std::env::remove_var("MINIMAX_MODEL");
    }
}
