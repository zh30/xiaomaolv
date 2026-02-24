use std::collections::HashMap;
use std::sync::Arc;

use xiaomaolv::config::ProviderConfig;
use xiaomaolv::domain::{MessageRole, StoredMessage};
use xiaomaolv::provider::{
    ChatProvider, CompletionRequest, ProviderFactory, ProviderRegistry, StreamSink,
};

struct PrefixProvider {
    prefix: String,
}

#[async_trait::async_trait]
impl ChatProvider for PrefixProvider {
    async fn complete(&self, req: CompletionRequest) -> anyhow::Result<String> {
        let user = req
            .messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::User)
            .map(|m| m.content.as_str())
            .unwrap_or("");
        Ok(format!("{}:{}", self.prefix, user))
    }
}

struct PrefixFactory;

impl ProviderFactory for PrefixFactory {
    fn kind(&self) -> &'static str {
        "prefix"
    }

    fn create(
        &self,
        provider_name: &str,
        config: &ProviderConfig,
    ) -> anyhow::Result<Arc<dyn ChatProvider>> {
        let prefix = config
            .options
            .get("prefix")
            .cloned()
            .unwrap_or_else(|| provider_name.to_string());
        Ok(Arc::new(PrefixProvider { prefix }))
    }
}

#[tokio::test]
async fn custom_provider_can_be_registered_and_built() {
    let mut registry = ProviderRegistry::new();
    registry
        .register(Arc::new(PrefixFactory))
        .expect("register factory");

    let cfg = ProviderConfig {
        kind: "prefix".to_string(),
        base_url: None,
        api_key: None,
        model: None,
        timeout_secs: 10,
        max_retries: 0,
        options: HashMap::from([("prefix".to_string(), "custom".to_string())]),
    };

    let provider = registry.build("my-provider", &cfg).expect("build provider");
    let output = provider
        .complete(CompletionRequest {
            messages: vec![StoredMessage {
                role: MessageRole::User,
                content: "hello".to_string(),
            }],
        })
        .await
        .expect("complete");

    assert_eq!(output, "custom:hello");
}

#[derive(Default)]
struct ChunkCollector {
    deltas: Vec<String>,
}

#[async_trait::async_trait]
impl StreamSink for ChunkCollector {
    async fn on_delta(&mut self, delta: &str) -> anyhow::Result<()> {
        self.deltas.push(delta.to_string());
        Ok(())
    }
}

#[tokio::test]
async fn custom_provider_without_stream_impl_uses_default_stream_fallback() {
    let mut registry = ProviderRegistry::new();
    registry
        .register(Arc::new(PrefixFactory))
        .expect("register factory");

    let cfg = ProviderConfig {
        kind: "prefix".to_string(),
        base_url: None,
        api_key: None,
        model: None,
        timeout_secs: 10,
        max_retries: 0,
        options: HashMap::from([("prefix".to_string(), "stream".to_string())]),
    };

    let provider = registry.build("my-provider", &cfg).expect("build provider");
    let mut sink = ChunkCollector::default();
    let output = provider
        .complete_stream(
            CompletionRequest {
                messages: vec![StoredMessage {
                    role: MessageRole::User,
                    content: "hello".to_string(),
                }],
            },
            &mut sink,
        )
        .await
        .expect("complete stream");

    assert_eq!(output, "stream:hello");
    assert_eq!(sink.deltas, vec!["stream:hello"]);
}
