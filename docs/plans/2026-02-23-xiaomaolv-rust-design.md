# xiaomaolv Rust Core Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build a production-style, high-performance Rust implementation of xiaomaolv core capabilities where users only configure AI provider and message channel.

**Architecture:** Single Rust service with clean module boundaries: config loading, provider abstraction, channel adapters, conversation memory, and HTTP gateway. Use async-first I/O, bounded concurrency, and SQLite persistence to keep hardware requirements low while supporting concurrent sessions.

**Tech Stack:** Rust, Tokio, Axum, Reqwest, SQLx (SQLite), Serde, Tracing, Clap.

---

### Task 1: Project Baseline

**Files:**
- Create: `Cargo.toml`
- Create: `src/lib.rs`
- Create: `src/main.rs`

**Step 1: Write the failing test**
- Create integration test for config bootstrap and service startup contract.

**Step 2: Run test to verify it fails**
- Run: `cargo test bootstrap -- --nocapture`

**Step 3: Write minimal implementation**
- Add crate dependencies and compile-ready module exports.

**Step 4: Run test to verify it passes**
- Run: `cargo test bootstrap -- --nocapture`

### Task 2: Configuration + Domain Contracts

**Files:**
- Create: `src/config.rs`
- Create: `src/domain.rs`
- Test: `tests/config_bootstrap.rs`

**Step 1: Write failing tests**
- Validate config parsing from TOML with minimal provider/channel fields.

**Step 2: Verify failing**
- Run: `cargo test config_bootstrap -- --nocapture`

**Step 3: Implement**
- Add serde-based config model and loader.

**Step 4: Verify passing**
- Run: `cargo test config_bootstrap -- --nocapture`

### Task 3: Memory Store (SQLite)

**Files:**
- Create: `src/memory.rs`
- Test: `tests/memory_store.rs`

**Step 1: Write failing tests**
- Save/load ordered conversation history.

**Step 2: Verify failing**
- Run: `cargo test memory_store -- --nocapture`

**Step 3: Implement**
- Create async SQLite store with schema bootstrap.

**Step 4: Verify passing**
- Run: `cargo test memory_store -- --nocapture`

### Task 4: Provider + Service Pipeline

**Files:**
- Create: `src/provider.rs`
- Create: `src/service.rs`
- Test: `tests/service_pipeline.rs`

**Step 1: Write failing tests**
- Incoming user message creates assistant reply and persists both messages.

**Step 2: Verify failing**
- Run: `cargo test service_pipeline -- --nocapture`

**Step 3: Implement**
- Add provider trait, OpenAI-compatible client, and orchestration service.

**Step 4: Verify passing**
- Run: `cargo test service_pipeline -- --nocapture`

### Task 5: Channel Adapters + HTTP API

**Files:**
- Create: `src/channel.rs`
- Create: `src/http.rs`
- Modify: `src/main.rs`
- Test: `tests/http_api.rs`

**Step 1: Write failing tests**
- POST `/v1/messages` returns assistant response.
- Telegram webhook route parses update and triggers pipeline.

**Step 2: Verify failing**
- Run: `cargo test http_api -- --nocapture`

**Step 3: Implement**
- Axum router, request validation, bounded semaphore for concurrency, optional Telegram sender.

**Step 4: Verify passing**
- Run: `cargo test http_api -- --nocapture`

### Task 6: Ops and Developer Experience

**Files:**
- Create: `README.md`
- Create: `config/xiaomaolv.example.toml`
- Create: `.gitignore`

**Step 1: Write failing test/check**
- Sanity-check startup command in docs.

**Step 2: Verify failing**
- Run: `cargo run -- --help`

**Step 3: Implement**
- Add simple setup and configuration docs.

**Step 4: Verify passing**
- Run: `cargo run -- --help`
