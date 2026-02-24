#!/usr/bin/env python3
"""
Minimal zvec sidecar for xiaomaolv hybrid memory backend.

Endpoints:
- GET  /health
- POST /v1/memory/upsert
- POST /v1/memory/query

This sidecar computes lightweight hash embeddings locally (no external model
required) and stores vectors in zvec.
"""

from __future__ import annotations

import math
import os
import re
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import List, Optional

from fastapi import FastAPI, Header, HTTPException
from pydantic import BaseModel, Field

try:
    import zvec
except Exception as exc:  # pragma: no cover
    raise RuntimeError(
        "zvec is required for this sidecar. install with: pip install zvec fastapi uvicorn"
    ) from exc


EMBED_DIM = int(os.getenv("ZVEC_EMBED_DIM", "256"))
ZVEC_PATH = os.getenv("ZVEC_PATH", "./data/zvec_memory")
DEFAULT_COLLECTION = os.getenv("ZVEC_COLLECTION", "agent_memory_v1")
API_TOKEN = os.getenv("ZVEC_API_TOKEN", "").strip()


class UpsertDoc(BaseModel):
    id: str
    text: str
    role: str
    session_id: str
    user_id: str
    channel: str
    created_at: int
    importance: float = 0.5


class UpsertRequest(BaseModel):
    collection: str = Field(default=DEFAULT_COLLECTION)
    docs: List[UpsertDoc] = Field(default_factory=list)
    items: List[UpsertDoc] = Field(default_factory=list)


class QueryRequest(BaseModel):
    collection: str = Field(default=DEFAULT_COLLECTION)
    text: Optional[str] = None
    query: Optional[str] = None
    q: Optional[str] = None
    user_id: str
    channel: str
    lookback_days: int = 90
    topk: int = 20
    top_k: Optional[int] = None
    topK: Optional[int] = None


class MemoryHit(BaseModel):
    id: str
    score: float
    text: str
    role: str
    session_id: str
    user_id: str
    channel: str
    created_at: int


class QueryResponse(BaseModel):
    hits: List[MemoryHit]


@dataclass
class SidecarState:
    collection_name: str
    collection: zvec.Collection
    lock: threading.Lock


def tokenize(text: str) -> List[str]:
    text = text.strip().lower()
    if not text:
        return []
    # Keep words + single CJK characters.
    return re.findall(r"[a-z0-9_]+|[\u4e00-\u9fff]", text)


def hash_embed(text: str, dim: int) -> List[float]:
    vec = [0.0] * dim
    tokens = tokenize(text)
    if not tokens:
        return vec

    for tok in tokens:
        idx = hash(tok) % dim
        vec[idx] += 1.0

    norm = math.sqrt(sum(v * v for v in vec))
    if norm > 0:
        vec = [v / norm for v in vec]
    return vec


def build_schema(name: str) -> zvec.CollectionSchema:
    return zvec.CollectionSchema(
        name=name,
        fields=[
            zvec.FieldSchema("text", zvec.DataType.STRING),
            zvec.FieldSchema("role", zvec.DataType.STRING),
            zvec.FieldSchema("session_id", zvec.DataType.STRING),
            zvec.FieldSchema("user_id", zvec.DataType.STRING),
            zvec.FieldSchema("channel", zvec.DataType.STRING),
            zvec.FieldSchema("created_at", zvec.DataType.INT64),
        ],
        vectors=[
            zvec.VectorSchema(
                "dense",
                zvec.DataType.VECTOR_FP32,
                EMBED_DIM,
                index_param=zvec.HnswIndexParam(),
            )
        ],
    )


def open_or_create_collection(path: str, name: str) -> zvec.Collection:
    Path(path).parent.mkdir(parents=True, exist_ok=True)
    schema = build_schema(name)
    try:
        return zvec.open(path)
    except Exception:
        return zvec.create_and_open(path, schema)


zvec.init()
state = SidecarState(
    collection_name=DEFAULT_COLLECTION,
    collection=open_or_create_collection(ZVEC_PATH, DEFAULT_COLLECTION),
    lock=threading.Lock(),
)

app = FastAPI(title="xiaomaolv-zvec-sidecar")


@app.get("/health")
def health():
    return {"status": "ok", "collection": state.collection_name, "dim": EMBED_DIM}


@app.post("/v1/memory/upsert")
def upsert_memory(req: UpsertRequest, authorization: Optional[str] = Header(default=None)):
    assert_auth(authorization)
    if req.collection != state.collection_name:
        raise HTTPException(status_code=400, detail="unknown collection")
    input_docs = req.docs if req.docs else req.items
    docs = []
    for doc in input_docs:
        docs.append(
            zvec.Doc(
                id=doc.id,
                vectors={"dense": hash_embed(doc.text, EMBED_DIM)},
                fields={
                    "text": doc.text,
                    "role": doc.role,
                    "session_id": doc.session_id,
                    "user_id": doc.user_id,
                    "channel": doc.channel,
                    "created_at": int(doc.created_at),
                },
            )
        )
    with state.lock:
        state.collection.upsert(docs)
    return {"ok": True, "upserted": len(docs)}


@app.post("/v1/memory/query", response_model=QueryResponse)
def query_memory(req: QueryRequest, authorization: Optional[str] = Header(default=None)):
    return do_query_memory(req, authorization)


@app.post("/v1/memory/search", response_model=QueryResponse)
def search_memory(req: QueryRequest, authorization: Optional[str] = Header(default=None)):
    return do_query_memory(req, authorization)


def do_query_memory(req: QueryRequest, authorization: Optional[str]) -> QueryResponse:
    assert_auth(authorization)
    if req.collection != state.collection_name:
        raise HTTPException(status_code=400, detail="unknown collection")
    raw_text = req.text or req.query or req.q or ""
    query_vector = hash_embed(raw_text, EMBED_DIM)
    lookback_seconds = max(1, req.lookback_days) * 24 * 3600
    min_ts = int(time.time()) - lookback_seconds

    flt = (
        f"user_id == '{req.user_id}' and channel == '{req.channel}' "
        f"and created_at >= {min_ts}"
    )

    topk = req.top_k or req.topK or req.topk

    with state.lock:
        docs = state.collection.query(
            vectors=zvec.VectorQuery("dense", vector=query_vector),
            topk=max(1, topk),
            filter=flt,
            output_fields=[
                "text",
                "role",
                "session_id",
                "user_id",
                "channel",
                "created_at",
            ],
        )

    hits = []
    for doc in docs:
        hits.append(
            MemoryHit(
                id=doc.id,
                score=float(doc.score or 0.0),
                text=str(doc.field("text") or ""),
                role=str(doc.field("role") or "user"),
                session_id=str(doc.field("session_id") or ""),
                user_id=str(doc.field("user_id") or ""),
                channel=str(doc.field("channel") or ""),
                created_at=int(doc.field("created_at") or 0),
            )
        )
    return QueryResponse(hits=hits)


def assert_auth(authorization: Optional[str]) -> None:
    if not API_TOKEN:
        return
    expected = f"Bearer {API_TOKEN}"
    if authorization != expected:
        raise HTTPException(status_code=401, detail="unauthorized")
