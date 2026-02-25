use std::collections::HashMap;
use std::sync::Arc;

use axum_test::TestServer;
use xiaomaolv::channel::{
    ChannelContext, ChannelFactory, ChannelInbound, ChannelPlugin, ChannelPluginConfig,
    ChannelPluginError, ChannelRegistry, ChannelResponse,
};
use xiaomaolv::config::{
    AppConfig, AppSettings, ChannelsConfig, HttpChannelConfig, ProviderConfig,
};
use xiaomaolv::domain::{IncomingMessage, MessageRole};
use xiaomaolv::http::build_router_with_registries;
use xiaomaolv::provider::{ChatProvider, CompletionRequest, ProviderRegistry};

struct FakeProvider;

#[async_trait::async_trait]
impl ChatProvider for FakeProvider {
    async fn complete(&self, req: CompletionRequest) -> anyhow::Result<String> {
        let user = req
            .messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::User)
            .map(|m| m.content.as_str())
            .unwrap_or("");
        Ok(format!("ack:{user}"))
    }
}

struct JsonWebhookChannel;

#[async_trait::async_trait]
impl ChannelPlugin for JsonWebhookChannel {
    async fn handle_inbound(
        &self,
        ctx: ChannelContext,
        inbound: ChannelInbound,
    ) -> Result<ChannelResponse, ChannelPluginError> {
        let payload = inbound.payload;
        let session_id = payload
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ChannelPluginError::BadRequest("missing session_id".to_string()))?;
        let user_id = payload
            .get("user_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ChannelPluginError::BadRequest("missing user_id".to_string()))?;
        let text = payload
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ChannelPluginError::BadRequest("missing text".to_string()))?;

        let out = ctx
            .service
            .handle(IncomingMessage {
                channel: ctx.channel_name,
                session_id: session_id.to_string(),
                user_id: user_id.to_string(),
                text: text.to_string(),
                reply_target: None,
            })
            .await
            .map_err(ChannelPluginError::internal)?;

        Ok(ChannelResponse::json(
            serde_json::json!({ "reply": out.text }),
        ))
    }
}

struct JsonWebhookFactory;

impl ChannelFactory for JsonWebhookFactory {
    fn kind(&self) -> &'static str {
        "json-webhook"
    }

    fn create(
        &self,
        _name: &str,
        _config: &ChannelPluginConfig,
    ) -> anyhow::Result<Arc<dyn ChannelPlugin>> {
        Ok(Arc::new(JsonWebhookChannel))
    }
}

#[tokio::test]
async fn custom_channel_plugin_can_receive_messages_via_unified_endpoint() {
    let cfg = AppConfig {
        app: AppSettings {
            bind: "127.0.0.1:0".to_string(),
            default_provider: "openai".to_string(),
            max_history: 16,
            concurrency_limit: 32,
        },
        providers: std::iter::once((
            "openai".to_string(),
            ProviderConfig {
                kind: "openai-compatible".to_string(),
                base_url: Some("http://127.0.0.1:9999/v1".to_string()),
                api_key: Some("x".to_string()),
                model: Some("m".to_string()),
                timeout_secs: 30,
                max_retries: 0,
                options: HashMap::new(),
            },
        ))
        .collect(),
        channels: ChannelsConfig {
            http: HttpChannelConfig { enabled: true },
            telegram: None,
            plugins: std::iter::once((
                "webhook1".to_string(),
                ChannelPluginConfig {
                    kind: "json-webhook".to_string(),
                    enabled: true,
                    settings: HashMap::new(),
                },
            ))
            .collect(),
        },
        memory: Default::default(),
        agent: Default::default(),
    };

    let mut channel_registry = ChannelRegistry::new();
    channel_registry
        .register(Arc::new(JsonWebhookFactory))
        .expect("register channel factory");

    let app = build_router_with_registries(
        cfg,
        "sqlite::memory:",
        Some(Arc::new(FakeProvider)),
        ProviderRegistry::with_defaults(),
        channel_registry,
    )
    .await
    .expect("router");

    let server = TestServer::new(app).expect("test server");

    let response = server
        .post("/v1/channels/webhook1/inbound")
        .json(&serde_json::json!({
            "session_id": "s-plugin",
            "user_id": "u-plugin",
            "text": "hello"
        }))
        .await;

    response.assert_status_ok();
}
