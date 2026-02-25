use std::collections::HashMap;
use std::sync::Arc;

use tempfile::tempdir;
use tokio::sync::RwLock;
use xiaomaolv::mcp::{McpAddSpec, McpConfigPaths, McpRegistry, McpRuntime, McpScope, McpTransport};
use xiaomaolv::memory::{SqliteMemoryBackend, SqliteMemoryStore};
use xiaomaolv::provider::{ChatProvider, CompletionRequest};
use xiaomaolv::service::{AgentMcpSettings, MessageService};

struct FakeProvider;

#[async_trait::async_trait]
impl ChatProvider for FakeProvider {
    async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<String> {
        Ok("ok".to_string())
    }
}

#[tokio::test]
async fn service_can_reload_mcp_runtime_from_registry() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let runtime = Arc::new(RwLock::new(McpRuntime::default()));
    let service = MessageService::new_with_backend(
        Arc::new(FakeProvider),
        Arc::new(SqliteMemoryBackend::new(store)),
        Some(runtime.clone()),
        AgentMcpSettings::default(),
        16,
        0,
        0,
    );

    let tmp = tempdir().expect("tempdir");
    let registry = McpRegistry::new(McpConfigPaths {
        user: tmp.path().join("user.toml"),
        project: tmp.path().join("project.toml"),
    });

    registry
        .add_server(
            McpScope::User,
            "demo",
            McpAddSpec {
                transport: McpTransport::Http,
                command: None,
                args: vec![],
                cwd: None,
                env: HashMap::new(),
                url: Some("http://127.0.0.1:8787/mcp".to_string()),
                headers: HashMap::new(),
                timeout_secs: 10,
                enabled: true,
            },
        )
        .await
        .expect("add server");

    service
        .reload_mcp_runtime_from_registry(&registry)
        .await
        .expect("reload runtime");

    let snapshot = runtime.read().await.clone();
    let servers = snapshot.servers();
    assert!(servers.iter().any(|s| s.name == "demo"));
}
