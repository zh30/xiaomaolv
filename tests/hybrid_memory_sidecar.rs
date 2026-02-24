use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use xiaomaolv::domain::{IncomingMessage, MessageRole};
use xiaomaolv::memory::{
    HybridSqliteZvecMemoryBackend, SqliteMemoryStore, ZvecSidecarClient, ZvecSidecarConfig,
};
use xiaomaolv::provider::{ChatProvider, CompletionRequest};
use xiaomaolv::service::MessageService;

struct CaptureProvider;

#[async_trait::async_trait]
impl ChatProvider for CaptureProvider {
    async fn complete(&self, req: CompletionRequest) -> anyhow::Result<String> {
        let system_memory = req
            .messages
            .iter()
            .find(|m| m.role == MessageRole::System)
            .map(|m| m.content.clone())
            .unwrap_or_else(|| "no-memory".to_string());
        Ok(system_memory)
    }
}

struct CountProvider;

#[async_trait::async_trait]
impl ChatProvider for CountProvider {
    async fn complete(&self, req: CompletionRequest) -> anyhow::Result<String> {
        Ok(format!("ctx:{}", req.messages.len()))
    }
}

#[tokio::test]
async fn hybrid_backend_merges_sidecar_memories_into_context() {
    let app = Router::new()
        .route("/v1/memory/upsert", post(mock_upsert))
        .route("/v1/memory/query", post(mock_query_with_hit));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let sidecar = ZvecSidecarClient::new(ZvecSidecarConfig {
        endpoint: format!("http://{addr}"),
        collection: "agent_memory_v1".to_string(),
        query_topk: 20,
        request_timeout_secs: 3,
        upsert_path: "/v1/memory/upsert".to_string(),
        query_path: "/v1/memory/query".to_string(),
        auth_bearer_token: None,
    });

    let backend = Arc::new(HybridSqliteZvecMemoryBackend::new(store, sidecar));
    let service = MessageService::new_with_backend(Arc::new(CaptureProvider), backend, 16, 4, 90);

    let out = service
        .handle(IncomingMessage {
            channel: "telegram".to_string(),
            session_id: "s1".to_string(),
            user_id: "u1".to_string(),
            text: "给我一个 rust 记忆".to_string(),
            reply_target: None,
        })
        .await
        .expect("service handle");

    assert!(out.text.contains("remember-rust"));

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn hybrid_backend_supports_legacy_search_endpoint_and_content_field() {
    let app = Router::new()
        .route("/v1/memory/upsert", post(mock_upsert))
        .route("/v1/memory/search", post(mock_search_with_content_hit));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let sidecar = ZvecSidecarClient::new(ZvecSidecarConfig {
        endpoint: format!("http://{addr}"),
        collection: "agent_memory_v1".to_string(),
        query_topk: 20,
        request_timeout_secs: 3,
        upsert_path: "/v1/memory/upsert".to_string(),
        query_path: "/v1/memory/query".to_string(),
        auth_bearer_token: None,
    });

    let backend = Arc::new(HybridSqliteZvecMemoryBackend::new(store, sidecar));
    let service = MessageService::new_with_backend(Arc::new(CaptureProvider), backend, 16, 4, 90);

    let out = service
        .handle(IncomingMessage {
            channel: "telegram".to_string(),
            session_id: "s-legacy".to_string(),
            user_id: "u-legacy".to_string(),
            text: "legacy memory".to_string(),
            reply_target: None,
        })
        .await
        .expect("service handle");

    assert!(out.text.contains("legacy-content-memory"));

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn hybrid_backend_can_send_bearer_token_to_sidecar() {
    let app = Router::new()
        .route("/v1/memory/upsert", post(mock_upsert))
        .route("/v1/memory/query", post(mock_query_requires_auth))
        .with_state("Bearer s3cr3t".to_string());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let sidecar = ZvecSidecarClient::new(ZvecSidecarConfig {
        endpoint: format!("http://{addr}"),
        collection: "agent_memory_v1".to_string(),
        query_topk: 20,
        request_timeout_secs: 3,
        upsert_path: "/v1/memory/upsert".to_string(),
        query_path: "/v1/memory/query".to_string(),
        auth_bearer_token: Some("s3cr3t".to_string()),
    });

    let backend = Arc::new(HybridSqliteZvecMemoryBackend::new(store, sidecar));
    let service = MessageService::new_with_backend(Arc::new(CaptureProvider), backend, 16, 4, 90);

    let out = service
        .handle(IncomingMessage {
            channel: "telegram".to_string(),
            session_id: "s-auth".to_string(),
            user_id: "u-auth".to_string(),
            text: "auth memory".to_string(),
            reply_target: None,
        })
        .await
        .expect("service handle");

    assert!(out.text.contains("auth-memory-hit"));

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn hybrid_backend_falls_back_to_recent_context_when_sidecar_unavailable() {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    let sidecar = ZvecSidecarClient::new(ZvecSidecarConfig {
        endpoint: "http://127.0.0.1:1".to_string(),
        collection: "agent_memory_v1".to_string(),
        query_topk: 20,
        request_timeout_secs: 1,
        upsert_path: "/v1/memory/upsert".to_string(),
        query_path: "/v1/memory/query".to_string(),
        auth_bearer_token: None,
    });

    let backend = Arc::new(HybridSqliteZvecMemoryBackend::new(store, sidecar));
    let service = MessageService::new_with_backend(Arc::new(CountProvider), backend, 16, 4, 90);

    let out = service
        .handle(IncomingMessage {
            channel: "telegram".to_string(),
            session_id: "s2".to_string(),
            user_id: "u2".to_string(),
            text: "hello".to_string(),
            reply_target: None,
        })
        .await
        .expect("service handle");

    assert_eq!(out.text, "ctx:1");
}

async fn mock_upsert(Json(_payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

async fn mock_query_with_hit(Json(_payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "hits": [
            {
                "id": "m1",
                "score": 0.98,
                "text": "remember-rust",
                "role": "user"
            }
        ]
    }))
}

async fn mock_search_with_content_hit(
    Json(_payload): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "result": {
            "hits": [
                {
                    "id": "legacy-1",
                    "score": 0.83,
                    "content": "legacy-content-memory",
                    "role": "user"
                }
            ]
        }
    }))
}

async fn mock_query_requires_auth(
    State(expected_auth): State<String>,
    headers: HeaderMap,
    Json(_payload): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let got = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if got != expected_auth {
        return Err((
            StatusCode::UNAUTHORIZED,
            "missing or invalid auth".to_string(),
        ));
    }

    Ok(Json(serde_json::json!({
        "hits": [
            {
                "id": "auth-1",
                "score": 0.95,
                "text": "auth-memory-hit",
                "role": "user"
            }
        ]
    })))
}
