# Zvec + SQLite Agent Memory Design (Rust-first)

Date: 2026-02-23

## 1. Goals

- Keep current Rust gateway simple and fast.
- Upgrade memory from "recent window only" to "short-term + long-term semantic recall".
- Preserve low hardware requirement and low ops burden.
- Keep extension model plugin-friendly (same style as Provider/Channel plugin API).

## 2. Current Baseline (in this repo)

- `src/memory.rs` stores conversation turns in SQLite table `messages`.
- `src/service.rs` loads `max_history` recent turns per `session_id`.
- No semantic retrieval, no memory summarization, no cross-session recall.

This is stable and cheap, but context quality drops when sessions get long or fragmented.

## 3. Key Decision

Use a **hybrid memory architecture**:

1. **SQLite = source of truth + transactional log**
2. **Zvec = semantic index for long-term recall**
3. **Memory Orchestrator (Rust) = fusion + budgeting + ranking**

Why:

- SQLite keeps write path robust and simple.
- Zvec gives fast semantic recall and scalar filter capability.
- Orchestrator keeps business logic provider-agnostic and easy to evolve.

## 4. Important Compatibility Note

Based on upstream layout today:

- `alibaba/zvec` repository exposes core C++ engine + Python binding (`pyproject.toml`, `src/binding/python`).
- There is no official Rust binding surface in the upstream tree at this moment.

So MVP should use a **Zvec sidecar adapter**:

- Rust main process stays pure Rust.
- Python sidecar embeds zvec SDK.
- Rust talks to sidecar over Unix Domain Socket (preferred) or localhost HTTP.

When official Rust binding is mature, swap adapter only (no memory domain rewrite).

## 5. Architecture

### 5.1 Components

- `MemoryBackend` (trait, Rust)
  - `SqliteOnlyMemoryBackend`
  - `HybridSqliteZvecMemoryBackend`
- `EmbeddingProvider` (trait, Rust)
  - remote embedding API provider(s)
  - optional local embedding provider
- `MemoryOrchestrator` (Rust service)
  - write pipeline
  - retrieval fusion
  - prompt budget control
- `ZvecAdapter` (trait, Rust)
  - `ZvecSidecarAdapter` (MVP)
  - `ZvecNativeAdapter` (future)

### 5.2 Plugin API Sketch

```rust
#[async_trait]
pub trait MemoryBackend: Send + Sync {
    async fn append_turn(&self, turn: StoredTurn) -> anyhow::Result<()>;
    async fn build_context(&self, req: ContextRequest) -> anyhow::Result<Vec<StoredMessage>>;
    async fn compact(&self, req: CompactRequest) -> anyhow::Result<CompactStats>;
}

#[async_trait]
pub trait ZvecAdapter: Send + Sync {
    async fn upsert(&self, docs: Vec<VectorDoc>) -> anyhow::Result<()>;
    async fn query(&self, req: VectorQueryReq) -> anyhow::Result<Vec<VectorHit>>;
}
```

## 6. Data Model

### 6.1 SQLite tables (authoritative)

- `messages`
  - `id INTEGER PK`
  - `session_id TEXT`
  - `user_id TEXT`
  - `channel TEXT`
  - `role TEXT`
  - `content TEXT`
  - `created_at INTEGER`
- `memory_chunks`
  - `chunk_id TEXT PK`
  - `message_id INTEGER`
  - `session_id TEXT`
  - `user_id TEXT`
  - `channel TEXT`
  - `text TEXT`
  - `importance REAL DEFAULT 0.5`
  - `created_at INTEGER`
  - `archived INTEGER DEFAULT 0`
- `memory_jobs`
  - `job_id INTEGER PK`
  - `chunk_id TEXT`
  - `status TEXT` (`pending|running|done|failed`)
  - `retry_count INTEGER`
  - `next_retry_at INTEGER`
  - `last_error TEXT`

Indexes:

- `idx_messages_session_created(session_id, created_at DESC)`
- `idx_chunks_user_created(user_id, created_at DESC)`
- `idx_jobs_status_next(status, next_retry_at)`

### 6.2 Zvec collection schema (semantic layer)

Collection: `agent_memory_v1`

- Scalar fields:
  - `chunk_id` (primary string)
  - `session_id`
  - `user_id`
  - `channel`
  - `created_at` (int64)
  - `importance` (float)
  - `token_len` (int32)
- Vector fields:
  - `dense` (VECTOR_FP32, dim depends on embedding model)
  - optional `sparse` (for future hybrid dense+sparse rerank)

## 7. Write Path

1. Receive user turn.
2. In one SQLite transaction:
   - append raw turn to `messages`
   - chunk and write `memory_chunks`
   - enqueue `memory_jobs`
3. Background worker pulls jobs (bounded concurrency).
4. Generate embedding.
5. Upsert into zvec (`chunk_id` as id).
6. Mark job `done`, failures use exponential backoff.

Design rule:

- **Never block user response on vector indexing**.
- If zvec unavailable, system still works with SQLite recent context.

## 8. Read Path (Context Building)

Given incoming query `q`:

1. `recent_ctx`: load latest N turns from SQLite (`session_id`).
2. `semantic_hits`: zvec query by embedding(q), with scalar filter:
   - same `user_id`
   - optionally same `channel`
   - optional recency window (e.g., last 90 days)
3. `rerank`:
   - final_score = `0.70 * sim + 0.20 * recency + 0.10 * importance`
4. dedupe by `chunk_id` and semantic near-duplicate hash.
5. token budget packer:
   - reserve system + tool budget
   - fill with recent first, then top semantic hits.

Output prompt context order:

- system memory summary
- high-priority semantic memories
- latest dialogue turns

## 9. Performance & Resource Defaults

- SQLite:
  - `WAL` mode
  - `synchronous=NORMAL`
  - batched writes for memory jobs
- Worker:
  - embed concurrency `2~4` (start with 2)
  - query topk `20`, final inject `6~10`
- Zvec:
  - start with `HNSW` + FP16 quantization when acceptable recall
  - enable mmap in collection option for lower RAM pressure

## 10. Failure Strategy

- Sidecar down:
  - degrade to SQLite-only memory
  - mark health flag + emit warning metric
- Embedding API timeout:
  - retry with backoff
  - dead-letter after max retries
- Corrupted chunk/vector mismatch:
  - periodic reconciler scans SQLite vs zvec by `chunk_id`

## 11. Config (TOML shape proposal)

```toml
[memory]
backend = "hybrid-sqlite-zvec"   # sqlite-only | hybrid-sqlite-zvec
max_recent_turns = 16
max_semantic_memories = 8
semantic_lookback_days = 90
enable_compaction = true

[memory.embedding]
provider = "openai-compatible"
base_url = "${EMBED_BASE_URL}"
api_key = "${EMBED_API_KEY}"
model = "text-embedding-3-small"
timeout_secs = 20

[memory.zvec]
mode = "sidecar"                 # sidecar | native
endpoint = "unix:///tmp/zvec.sock"
collection = "agent_memory_v1"
query_topk = 20
upsert_batch = 64
```

## 12. Rollout Plan

Phase 1 (safe):

- Implement `MemoryBackend` abstraction.
- Keep default `sqlite-only`.
- Add `hybrid-sqlite-zvec` behind feature/config flag.

Phase 2:

- Add async indexing worker + sidecar adapter.
- Add semantic retrieval fusion.

Phase 3:

- Add compaction/summarization and reconciliation jobs.
- Add metrics dashboard (queue lag, recall hit rate, degradation events).

## 13. Test Plan

- Unit:
  - chunking
  - score/rerank
  - budget packer
- Integration:
  - sqlite-only fallback path
  - sidecar unavailable degradation
  - zvec query + filter behavior
- Load:
  - 1k QPS write/read mixed with bounded memory

## 14. Why this design is "high-efficiency + concise"

- Minimal moving parts in critical path (SQLite transaction + async enqueue).
- Semantic layer is optional at runtime, not mandatory for correctness.
- Pluginized boundaries let community replace embedding/zvec adapter without touching main service logic.
- Works now with current upstream zvec packaging constraints, and ready for future native Rust binding.
