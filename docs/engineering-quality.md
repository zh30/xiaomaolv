# Engineering Quality Guide

This document defines the engineering baseline for `xiaomaolv`.

## 1. Quality Gates

Every change must pass these checks:

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Do not merge changes that skip lint or test verification.

## 2. Runtime Architecture Boundaries

- `src/http.rs`: composition root and API wiring.
- `src/service.rs`: orchestration only (message flow, MCP loop, scheduler intent parsing).
- `src/provider.rs`: model provider transport and retry policy.
- `src/memory.rs`: storage/recall abstraction and SQLite/zvec implementations.
- `src/channel.rs`: channel adapters and protocol-specific behavior (Telegram).
- `src/scheduler.rs`: pure scheduler domain logic (state machine and schedule math).

Business rules should stay in `service.rs` or `scheduler.rs`, not in transport adapters.

## 3. Performance Baseline

### Build profile

Release profile in `Cargo.toml`:

- `codegen-units = 1`
- `lto = "thin"`
- `opt-level = 3`
- `strip = "symbols"`

### SQLite

`SqliteMemoryStore` uses:

- `busy_timeout = 5s`
- WAL mode for file-based databases
- `synchronous = NORMAL` for better write throughput
- hot-path indexes for session history and memory chunk scans

### Provider retries

Retry behavior for OpenAI-compatible provider:

- total attempts = `max_retries + 1`
- backoff only happens between attempts
- non-2xx responses include trimmed body details for diagnosis

## 4. Coding Standards

- Avoid broad functions with long positional argument lists; use context structs.
- Avoid panicking APIs (`expect`/`unwrap`) on runtime paths.
- Keep transport-level errors contextual and actionable.
- Prefer small pure helpers for parsing/normalization and add unit tests for them.

## 5. Documentation Requirements

When adding or changing subsystem behavior:

1. Update README/README.zh if user-facing behavior changes.
2. Update this guide if quality gates or performance strategy change.
3. Keep `docs/` references accurate and minimal.
