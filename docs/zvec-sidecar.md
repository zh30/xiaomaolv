# Zvec Sidecar Integration (Hybrid Memory)

Date: 2026-02-23

## Overview

`xiaomaolv` supports two memory backends:

- `sqlite-only` (default)
- `hybrid-sqlite-zvec` (SQLite + zvec semantic recall via sidecar)

Hybrid mode keeps SQLite as source-of-truth and calls sidecar for semantic memory retrieval.

## 1. Enable Hybrid Memory

In config:

```toml
[memory]
backend = "hybrid-sqlite-zvec"
max_recent_turns = 0
max_semantic_memories = 8
semantic_lookback_days = 90

[memory.zvec]
endpoint = "http://127.0.0.1:3711"
collection = "agent_memory_v1"
query_topk = 20
request_timeout_secs = 3
upsert_path = "/v1/memory/upsert"
query_path = "/v1/memory/query"
# auth_bearer_token = "optional-token"
```

`max_recent_turns = 0` means fallback to `app.max_history`.

## 2. Run Sidecar

```bash
cd /Users/henry/code/xiaomaolv
./scripts/run_zvec_sidecar.sh
```

Optional bearer auth for sidecar:

```bash
export ZVEC_SIDECAR_TOKEN=your_token
```

Health check:

```bash
curl -sS http://127.0.0.1:3711/health
```

## 3. Sidecar API Contract

### Upsert

`POST /v1/memory/upsert`

Request:

```json
{
  "collection": "agent_memory_v1",
  "docs": [
    {
      "id": "mem-xxx",
      "text": "user message",
      "role": "user",
      "session_id": "s1",
      "user_id": "u1",
      "channel": "telegram",
      "created_at": 1730000000,
      "importance": 0.5
    }
  ]
}
```

### Query

`POST /v1/memory/query`

Legacy-compatible alias:

`POST /v1/memory/search`

Request:

```json
{
  "collection": "agent_memory_v1",
  "text": "current user question",
  "user_id": "u1",
  "channel": "telegram",
  "lookback_days": 90,
  "topk": 20
}
```

The sidecar also accepts:

- `text` or `query` or `q`
- `topk` or `top_k` or `topK`
- upsert payload key `docs` or `items`

Response:

```json
{
  "hits": [
    {
      "id": "mem-xxx",
      "score": 0.91,
      "text": "historical memory text",
      "role": "user",
      "session_id": "s1",
      "user_id": "u1",
      "channel": "telegram",
      "created_at": 1730000000
    }
  ]
}
```

## 4. Fallback Behavior

If sidecar is unavailable or query fails:

- request does not fail
- service falls back to SQLite recent context only

This behavior is covered by:

- `/Users/henry/code/xiaomaolv/tests/hybrid_memory_sidecar.rs`
