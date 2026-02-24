use std::collections::{HashSet, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use async_trait::async_trait;
use reqwest::Client;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Row, SqlitePool};
use tracing::warn;

use crate::domain::{MessageRole, StoredMessage};

#[derive(Clone)]
pub struct SqliteMemoryStore {
    pool: SqlitePool,
}

impl SqliteMemoryStore {
    pub async fn new(database_url: &str) -> anyhow::Result<Self> {
        let options = SqliteConnectOptions::from_str(database_url)
            .with_context(|| format!("failed to parse sqlite database URL: {database_url}"))?
            .create_if_missing(true);

        let pool = SqlitePool::connect_with(options)
            .await
            .with_context(|| format!("failed to connect sqlite database: {database_url}"))?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at INTEGER NOT NULL DEFAULT (unixepoch())
            );",
        )
        .execute(&pool)
        .await
        .context("failed to initialize messages table")?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS memory_chunks (
                chunk_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                channel TEXT NOT NULL,
                role TEXT NOT NULL,
                text TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );",
        )
        .execute(&pool)
        .await
        .context("failed to initialize memory_chunks table")?;

        Ok(Self { pool })
    }

    pub async fn append(&self, session_id: &str, message: StoredMessage) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO messages (session_id, role, content) VALUES (?1, ?2, ?3)")
            .bind(session_id)
            .bind(message.role.as_str())
            .bind(message.content)
            .execute(&self.pool)
            .await
            .context("failed to insert message")?;
        Ok(())
    }

    pub async fn load_recent(
        &self,
        session_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<StoredMessage>> {
        let mut rows = sqlx::query(
            "SELECT role, content FROM messages
             WHERE session_id = ?1
             ORDER BY id DESC
             LIMIT ?2",
        )
        .bind(session_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("failed to load recent messages")?
        .into_iter()
        .map(|row| {
            let role: String = row.get("role");
            let content: String = row.get("content");
            StoredMessage {
                role: MessageRole::from_db(&role),
                content,
            }
        })
        .collect::<Vec<_>>();

        rows.reverse();
        Ok(rows)
    }

    pub async fn append_chunk(&self, chunk: MemoryChunkRecord) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO memory_chunks
             (chunk_id, session_id, user_id, channel, role, text, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(chunk.chunk_id)
        .bind(chunk.session_id)
        .bind(chunk.user_id)
        .bind(chunk.channel)
        .bind(chunk.role.as_str())
        .bind(chunk.text)
        .bind(chunk.created_at)
        .execute(&self.pool)
        .await
        .context("failed to upsert memory chunk")?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct MemoryChunkRecord {
    pub chunk_id: String,
    pub session_id: String,
    pub user_id: String,
    pub channel: String,
    pub role: MessageRole,
    pub text: String,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct MemoryWriteRequest {
    pub session_id: String,
    pub user_id: String,
    pub channel: String,
    pub message: StoredMessage,
}

#[derive(Debug, Clone)]
pub struct MemoryContextRequest {
    pub session_id: String,
    pub user_id: String,
    pub channel: String,
    pub query_text: String,
    pub max_recent_turns: usize,
    pub max_semantic_memories: usize,
    pub semantic_lookback_days: u32,
}

#[async_trait]
pub trait MemoryBackend: Send + Sync {
    async fn append(&self, req: MemoryWriteRequest) -> anyhow::Result<()>;
    async fn load_context(&self, req: MemoryContextRequest) -> anyhow::Result<Vec<StoredMessage>>;
}

#[derive(Clone)]
pub struct SqliteMemoryBackend {
    store: SqliteMemoryStore,
}

impl SqliteMemoryBackend {
    pub fn new(store: SqliteMemoryStore) -> Self {
        Self { store }
    }
}

#[async_trait]
impl MemoryBackend for SqliteMemoryBackend {
    async fn append(&self, req: MemoryWriteRequest) -> anyhow::Result<()> {
        self.store.append(&req.session_id, req.message).await
    }

    async fn load_context(&self, req: MemoryContextRequest) -> anyhow::Result<Vec<StoredMessage>> {
        self.store
            .load_recent(&req.session_id, req.max_recent_turns)
            .await
    }
}

#[derive(Debug, Clone)]
pub struct ZvecSidecarConfig {
    pub endpoint: String,
    pub collection: String,
    pub query_topk: usize,
    pub request_timeout_secs: u64,
    pub upsert_path: String,
    pub query_path: String,
    pub auth_bearer_token: Option<String>,
}

#[derive(Clone)]
pub struct ZvecSidecarClient {
    client: Client,
    cfg: ZvecSidecarConfig,
}

impl ZvecSidecarClient {
    pub fn new(cfg: ZvecSidecarConfig) -> Self {
        Self {
            client: Client::new(),
            cfg,
        }
    }

    async fn upsert(&self, doc: SidecarMemoryDoc) -> anyhow::Result<()> {
        let request = SidecarUpsertRequest {
            collection: self.cfg.collection.clone(),
            docs: vec![doc],
        };

        self.request(self.cfg.upsert_path.as_str(), &request)
            .await
            .context("failed to call zvec sidecar upsert")?
            .error_for_status()
            .context("zvec sidecar upsert returned error")?;
        Ok(())
    }

    async fn query(&self, req: &MemoryContextRequest) -> anyhow::Result<Vec<SidecarMemoryHit>> {
        let request = SidecarQueryRequest {
            collection: self.cfg.collection.clone(),
            text: req.query_text.clone(),
            user_id: req.user_id.clone(),
            channel: req.channel.clone(),
            lookback_days: req.semantic_lookback_days,
            topk: req.max_semantic_memories.max(self.cfg.query_topk),
        };

        let query_path = self.cfg.query_path.as_str();
        let response = self
            .request(query_path, &request)
            .await
            .context("failed to call zvec sidecar query")?;

        let response = if response.status().is_success() {
            response
        } else if response.status() == StatusCode::NOT_FOUND && query_path != "/v1/memory/search" {
            let legacy = self
                .request("/v1/memory/search", &request)
                .await
                .context("failed to call zvec sidecar legacy search endpoint")?;
            legacy
                .error_for_status()
                .context("zvec sidecar legacy search returned error")?
        } else {
            response
                .error_for_status()
                .context("zvec sidecar query returned error")?
        };

        let body: SidecarQueryResponse = response
            .json()
            .await
            .context("failed to decode zvec sidecar query response")?;
        Ok(body.into_hits())
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.cfg.endpoint.trim_end_matches('/'), path)
    }

    async fn request<T: Serialize>(
        &self,
        path: &str,
        payload: &T,
    ) -> anyhow::Result<reqwest::Response> {
        let mut req = self
            .client
            .post(self.url(path))
            .timeout(std::time::Duration::from_secs(
                self.cfg.request_timeout_secs.max(1),
            ))
            .json(payload);

        if let Some(token) = &self.cfg.auth_bearer_token
            && !token.trim().is_empty()
        {
            req = req.bearer_auth(token);
        }

        req.send().await.context("sidecar HTTP request failed")
    }
}

#[derive(Clone)]
pub struct HybridSqliteZvecMemoryBackend {
    store: SqliteMemoryStore,
    sidecar: ZvecSidecarClient,
}

impl HybridSqliteZvecMemoryBackend {
    pub fn new(store: SqliteMemoryStore, sidecar: ZvecSidecarClient) -> Self {
        Self { store, sidecar }
    }
}

#[async_trait]
impl MemoryBackend for HybridSqliteZvecMemoryBackend {
    async fn append(&self, req: MemoryWriteRequest) -> anyhow::Result<()> {
        self.store
            .append(&req.session_id, req.message.clone())
            .await
            .context("failed to append message in hybrid backend")?;

        if req.message.content.trim().is_empty() {
            return Ok(());
        }

        let created_at = unix_ts();
        let chunk_id = stable_chunk_id(&req, created_at);

        self.store
            .append_chunk(MemoryChunkRecord {
                chunk_id: chunk_id.clone(),
                session_id: req.session_id.clone(),
                user_id: req.user_id.clone(),
                channel: req.channel.clone(),
                role: req.message.role.clone(),
                text: req.message.content.clone(),
                created_at,
            })
            .await
            .context("failed to append memory chunk in hybrid backend")?;

        let sidecar = self.sidecar.clone();
        let doc = SidecarMemoryDoc {
            id: chunk_id,
            text: req.message.content,
            role: req.message.role.as_str().to_string(),
            session_id: req.session_id,
            user_id: req.user_id,
            channel: req.channel,
            created_at,
            importance: 0.5,
        };

        tokio::spawn(async move {
            if let Err(err) = sidecar.upsert(doc).await {
                warn!(error = %err, "failed to upsert memory into zvec sidecar");
            }
        });

        Ok(())
    }

    async fn load_context(&self, req: MemoryContextRequest) -> anyhow::Result<Vec<StoredMessage>> {
        let recent = self
            .store
            .load_recent(&req.session_id, req.max_recent_turns)
            .await
            .context("failed to load recent messages in hybrid backend")?;

        if req.max_semantic_memories == 0 || req.query_text.trim().is_empty() {
            return Ok(recent);
        }

        let hits = match self.sidecar.query(&req).await {
            Ok(hits) => hits,
            Err(err) => {
                warn!(error = %err, "zvec sidecar query failed, fallback to recent context");
                return Ok(recent);
            }
        };

        let mut seen = HashSet::new();
        for msg in &recent {
            seen.insert(msg.content.clone());
        }

        let mut merged = Vec::new();
        for hit in hits.into_iter().take(req.max_semantic_memories) {
            let Some(text) = hit.resolved_text() else {
                continue;
            };
            let text = text.trim();
            if text.is_empty() || seen.contains(text) {
                continue;
            }
            seen.insert(text.to_string());
            merged.push(StoredMessage {
                role: MessageRole::System,
                content: format!("[memory score={:.3}] {}", hit.score, text),
            });
        }
        merged.extend(recent);
        Ok(merged)
    }
}

#[derive(Debug, Clone, Serialize)]
struct SidecarUpsertRequest {
    collection: String,
    docs: Vec<SidecarMemoryDoc>,
}

#[derive(Debug, Clone, Serialize)]
struct SidecarMemoryDoc {
    id: String,
    text: String,
    role: String,
    session_id: String,
    user_id: String,
    channel: String,
    created_at: i64,
    importance: f32,
}

#[derive(Debug, Clone, Serialize)]
struct SidecarQueryRequest {
    collection: String,
    text: String,
    user_id: String,
    channel: String,
    lookback_days: u32,
    topk: usize,
}

#[derive(Debug, Clone, Deserialize)]
struct SidecarQueryResponse {
    #[serde(default)]
    hits: Vec<SidecarMemoryHit>,
    #[serde(default)]
    result: Option<SidecarQueryNestedHits>,
    #[serde(default)]
    data: Option<Vec<SidecarMemoryHit>>,
}

#[derive(Debug, Clone, Deserialize)]
struct SidecarMemoryHit {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    score: f32,
}

#[derive(Debug, Clone, Deserialize)]
struct SidecarQueryNestedHits {
    #[serde(default)]
    hits: Vec<SidecarMemoryHit>,
}

impl SidecarQueryResponse {
    fn into_hits(self) -> Vec<SidecarMemoryHit> {
        if !self.hits.is_empty() {
            return self.hits;
        }
        if let Some(result) = self.result
            && !result.hits.is_empty()
        {
            return result.hits;
        }
        self.data.unwrap_or_default()
    }
}

impl SidecarMemoryHit {
    fn resolved_text(&self) -> Option<&str> {
        self.text
            .as_deref()
            .or(self.content.as_deref())
            .or(self.message.as_deref())
    }
}

fn unix_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn unix_ts_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn stable_chunk_id(req: &MemoryWriteRequest, ts_secs: i64) -> String {
    let mut hasher = DefaultHasher::new();
    req.session_id.hash(&mut hasher);
    req.user_id.hash(&mut hasher);
    req.channel.hash(&mut hasher);
    req.message.role.as_str().hash(&mut hasher);
    req.message.content.hash(&mut hasher);
    ts_secs.hash(&mut hasher);
    unix_ts_nanos().hash(&mut hasher);
    format!("mem-{:x}", hasher.finish())
}
