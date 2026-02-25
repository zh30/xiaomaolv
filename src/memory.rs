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

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS telegram_group_aliases (
                channel TEXT NOT NULL,
                chat_id INTEGER NOT NULL,
                alias TEXT NOT NULL,
                learned_at INTEGER NOT NULL DEFAULT (unixepoch()),
                PRIMARY KEY (channel, chat_id, alias)
            );",
        )
        .execute(&pool)
        .await
        .context("failed to initialize telegram_group_aliases table")?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS telegram_group_user_profiles (
                channel TEXT NOT NULL,
                chat_id INTEGER NOT NULL,
                user_id INTEGER NOT NULL,
                preferred_name TEXT NOT NULL,
                username TEXT,
                updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
                PRIMARY KEY (channel, chat_id, user_id)
            );",
        )
        .execute(&pool)
        .await
        .context("failed to initialize telegram_group_user_profiles table")?;

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

    pub async fn upsert_group_aliases(
        &self,
        channel: &str,
        chat_id: i64,
        aliases: &[String],
    ) -> anyhow::Result<()> {
        for alias in aliases {
            let normalized = alias.trim();
            if normalized.is_empty() {
                continue;
            }
            sqlx::query(
                "INSERT OR IGNORE INTO telegram_group_aliases
                 (channel, chat_id, alias, learned_at)
                 VALUES (?1, ?2, ?3, unixepoch())",
            )
            .bind(channel)
            .bind(chat_id)
            .bind(normalized)
            .execute(&self.pool)
            .await
            .context("failed to upsert telegram group alias")?;
        }
        Ok(())
    }

    pub async fn load_group_aliases(
        &self,
        channel: &str,
        chat_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT alias FROM telegram_group_aliases
             WHERE channel = ?1 AND chat_id = ?2
             ORDER BY learned_at DESC
             LIMIT ?3",
        )
        .bind(channel)
        .bind(chat_id)
        .bind(limit.max(1) as i64)
        .fetch_all(&self.pool)
        .await
        .context("failed to load telegram group aliases")?;

        Ok(rows
            .into_iter()
            .map(|row| row.get::<String, _>("alias"))
            .collect())
    }

    pub async fn upsert_group_user_profile(
        &self,
        channel: &str,
        chat_id: i64,
        user_id: i64,
        preferred_name: &str,
        username: Option<&str>,
    ) -> anyhow::Result<()> {
        let normalized_name = preferred_name.trim();
        if normalized_name.is_empty() {
            return Ok(());
        }
        let normalized_username = username
            .map(|v| v.trim().trim_start_matches('@'))
            .filter(|v| !v.is_empty());

        sqlx::query(
            "INSERT INTO telegram_group_user_profiles
             (channel, chat_id, user_id, preferred_name, username, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, unixepoch())
             ON CONFLICT(channel, chat_id, user_id) DO UPDATE SET
               preferred_name=excluded.preferred_name,
               username=COALESCE(excluded.username, telegram_group_user_profiles.username),
               updated_at=unixepoch()",
        )
        .bind(channel)
        .bind(chat_id)
        .bind(user_id)
        .bind(normalized_name)
        .bind(normalized_username)
        .execute(&self.pool)
        .await
        .context("failed to upsert telegram group user profile")?;

        Ok(())
    }

    pub async fn load_group_user_profiles(
        &self,
        channel: &str,
        chat_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<GroupUserProfileRecord>> {
        let rows = sqlx::query(
            "SELECT user_id, preferred_name, username, updated_at
             FROM telegram_group_user_profiles
             WHERE channel = ?1 AND chat_id = ?2
             ORDER BY updated_at DESC
             LIMIT ?3",
        )
        .bind(channel)
        .bind(chat_id)
        .bind(limit.max(1) as i64)
        .fetch_all(&self.pool)
        .await
        .context("failed to load telegram group user profiles")?;

        Ok(rows
            .into_iter()
            .map(|row| GroupUserProfileRecord {
                user_id: row.get("user_id"),
                preferred_name: row.get("preferred_name"),
                username: row.get("username"),
                updated_at: row.get("updated_at"),
            })
            .collect())
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

#[derive(Debug, Clone)]
pub struct GroupAliasUpsertRequest {
    pub channel: String,
    pub chat_id: i64,
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct GroupAliasLoadRequest {
    pub channel: String,
    pub chat_id: i64,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupUserProfileRecord {
    pub user_id: i64,
    pub preferred_name: String,
    pub username: Option<String>,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct GroupUserProfileUpsertRequest {
    pub channel: String,
    pub chat_id: i64,
    pub user_id: i64,
    pub preferred_name: String,
    pub username: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GroupUserProfileLoadRequest {
    pub channel: String,
    pub chat_id: i64,
    pub limit: usize,
}

#[async_trait]
pub trait MemoryBackend: Send + Sync {
    async fn append(&self, req: MemoryWriteRequest) -> anyhow::Result<()>;
    async fn load_context(&self, req: MemoryContextRequest) -> anyhow::Result<Vec<StoredMessage>>;
    async fn upsert_group_aliases(&self, _req: GroupAliasUpsertRequest) -> anyhow::Result<()> {
        Ok(())
    }
    async fn load_group_aliases(&self, _req: GroupAliasLoadRequest) -> anyhow::Result<Vec<String>> {
        Ok(vec![])
    }
    async fn upsert_group_user_profile(
        &self,
        _req: GroupUserProfileUpsertRequest,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn load_group_user_profiles(
        &self,
        _req: GroupUserProfileLoadRequest,
    ) -> anyhow::Result<Vec<GroupUserProfileRecord>> {
        Ok(vec![])
    }
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

    async fn upsert_group_aliases(&self, req: GroupAliasUpsertRequest) -> anyhow::Result<()> {
        self.store
            .upsert_group_aliases(&req.channel, req.chat_id, &req.aliases)
            .await
    }

    async fn load_group_aliases(&self, req: GroupAliasLoadRequest) -> anyhow::Result<Vec<String>> {
        self.store
            .load_group_aliases(&req.channel, req.chat_id, req.limit)
            .await
    }

    async fn upsert_group_user_profile(
        &self,
        req: GroupUserProfileUpsertRequest,
    ) -> anyhow::Result<()> {
        self.store
            .upsert_group_user_profile(
                &req.channel,
                req.chat_id,
                req.user_id,
                &req.preferred_name,
                req.username.as_deref(),
            )
            .await
    }

    async fn load_group_user_profiles(
        &self,
        req: GroupUserProfileLoadRequest,
    ) -> anyhow::Result<Vec<GroupUserProfileRecord>> {
        self.store
            .load_group_user_profiles(&req.channel, req.chat_id, req.limit)
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

    async fn upsert_group_aliases(&self, req: GroupAliasUpsertRequest) -> anyhow::Result<()> {
        self.store
            .upsert_group_aliases(&req.channel, req.chat_id, &req.aliases)
            .await
    }

    async fn load_group_aliases(&self, req: GroupAliasLoadRequest) -> anyhow::Result<Vec<String>> {
        self.store
            .load_group_aliases(&req.channel, req.chat_id, req.limit)
            .await
    }

    async fn upsert_group_user_profile(
        &self,
        req: GroupUserProfileUpsertRequest,
    ) -> anyhow::Result<()> {
        self.store
            .upsert_group_user_profile(
                &req.channel,
                req.chat_id,
                req.user_id,
                &req.preferred_name,
                req.username.as_deref(),
            )
            .await
    }

    async fn load_group_user_profiles(
        &self,
        req: GroupUserProfileLoadRequest,
    ) -> anyhow::Result<Vec<GroupUserProfileRecord>> {
        self.store
            .load_group_user_profiles(&req.channel, req.chat_id, req.limit)
            .await
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
