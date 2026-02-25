# Rust Architecture Hardening Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Raise code quality, maintainability, and runtime robustness without changing external behavior.

**Architecture:** Keep message flow and scheduler logic stable while tightening module boundaries, reducing function coupling, and hardening runtime defaults. Refactors are behavior-preserving and validation-driven (fmt, clippy, full tests).

**Tech Stack:** Rust (Tokio, Axum, SQLx/SQLite, Reqwest, Clap, Tracing)

---

### Task 1: Baseline and structural lint debt removal

**Files:**
- Modify: `src/channel.rs`
- Modify: `src/config.rs`

**Step 1: Run strict lint to capture baseline errors**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: lint failures identify refactor targets.

**Step 2: Refactor long-argument functions into context structs**

- Introduce request/operation context structs in `channel.rs`.
- Migrate call sites and preserve behavior.

**Step 3: Remove panic-prone and readability anti-patterns**

- Replace checked `expect` with safe control flow.
- Collapse nested conditionals and clean redundant borrowing.

**Step 4: Re-run strict lint**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: previous lint failures resolved.

### Task 2: Runtime performance and resilience hardening

**Files:**
- Modify: `src/memory.rs`
- Modify: `src/provider.rs`

**Step 1: Improve SQLite operational defaults and hot-path indexes**

- Enable busy timeout.
- For file DBs, use WAL + NORMAL sync settings.
- Add indexes for high-frequency reads.

**Step 2: Improve provider retry semantics and error observability**

- Define attempts as `max_retries + 1`.
- Backoff only between attempts.
- Include response body snippets for non-2xx failures.

**Step 3: Add unit tests for retry/parsing helper behavior**

- Add focused tests in `provider.rs` for deterministic helper logic.

### Task 3: Engineering docs and release posture

**Files:**
- Modify: `Cargo.toml`
- Modify: `README.md`
- Modify: `README.zh.md`
- Create: `docs/engineering-quality.md`

**Step 1: Tune release profile**

- Add thin LTO, single codegen unit, symbol stripping.

**Step 2: Document quality gates and architecture boundaries**

- Add engineering guide under `docs/`.
- Link guide from README and README.zh.

**Step 3: Add explicit pre-merge verification commands**

- `fmt --check`, strict `clippy`, all-target tests.

### Task 4: Verification

**Files:**
- Test: `src/*`, `tests/*`

**Step 1: Format check**

Run: `cargo fmt --all`
Expected: no formatting diffs afterward.

**Step 2: Strict lint check**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: pass with zero warnings.

**Step 3: Full behavior regression check**

Run: `cargo test --all-targets`
Expected: all tests pass.
