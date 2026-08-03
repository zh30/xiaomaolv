# Engineering Quality Guide

This document defines the engineering baseline for `xiaomaolv`.

## 1. Quality Gates

Every change must pass these checks:

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --release
```

Do not merge changes that skip lint or test verification.

## 2. Runtime Architecture Boundaries

- `src/http.rs`: composition root and API wiring.
- `src/http/harness_control.rs`: Loop Engineering transport, auth scopes, and SSE only.
- `src/service.rs`: orchestration only (message flow, MCP loop, scheduler intent parsing).
- `src/provider.rs`: model provider transport and retry policy.
- `src/memory.rs`: storage/recall abstraction and SQLite/zvec implementations.
- `src/channel.rs`: channel adapters and protocol-specific behavior (Telegram).
- `src/scheduler.rs`: pure scheduler domain logic (state machine and schedule math).
- `src/harness/`: agent run lifecycle, tool/output protocol, trajectories, compaction, verification, and prompt evolution.
- `src/harness/loop_engine/`: durable Goal/workflow domain, persistence, worker/recovery, signals, self-tests, replay, and artifacts.

Message-flow rules stay in `service.rs`; scheduler rules stay in `scheduler.rs`; Loop Engineering
rules stay in `harness/loop_engine`. Transport adapters must not own state transitions or approval
logic.

### Loop Engineering invariants

- Planning is non-executing; approval binds the exact goal revision, workflow/effect manifest,
  acceptance criteria, and execution budget.
- Provider/tool calls occur outside SQLite transactions. Claims are at-least-once and guarded by
  lease tokens plus monotonically increasing fencing tokens.
- Checkpoint phase/outcome transitions (`prepared`, `committed`, `reconciled`) are durable and
  fenced; recovery reconciles committed outcomes without replaying effects.
- Only registered handlers execute. `external_write` is rejected until a separately reviewed
  idempotency/confirmation contract exists.
- Production self-tests are read-only; structural replay executes zero live tools.
- External Signal credentials cannot access operator routes or assert `internal` trust.
- Prompt approval/activation/rollback remains exclusively in the existing Evolution control plane.

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
- Bound workflow nodes/edges/payloads, provider calls, response bytes, deadlines, list limits, and
  replay captures before accepting new Loop Engineering surfaces.

## 5. Documentation Requirements

When adding or changing subsystem behavior:

1. Update README/README.zh if user-facing behavior changes.
2. Update this guide if quality gates or performance strategy change.
3. Keep `docs/` references accurate and minimal.
