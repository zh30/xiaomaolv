# Agent Harness Prioritized Roadmap

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the current agent harness from MVP scaffolding into a reliable, observable, and safer execution layer.

**Architecture:** Work from highest-risk semantics toward optimization: first make safety/config behavior honest, then make trajectory and metrics complete, then add verification feedback, context compaction, and stronger MCP/tool protocols. Each task should be independently shippable with focused tests.

**Tech Stack:** Rust, Tokio, SQLx/SQLite, Prometheus, Tracing, existing MCP runtime, existing `MessageService` pipeline.

---

## Priority Overview

| Priority | Task | Why First | Done When |
|---|---|---|---|
| P0 | Code Mode safety semantics | Current `allow_*` flags can mislead operators | Config either enforced or renamed/documented honestly |
| P1 | Trajectory lifecycle completeness | Observability data is currently partial and can be left unfinished | Every MCP/Code Mode path has started/finished trajectory records |
| P1 | Harness metrics wiring | Metrics module exists but is not wired into runtime startup | Harness metrics are exported and increment during real requests |
| P2 | Tool and output verification | Verification only warns on slow tools today | Bad tool/output states are captured and can influence retry/fallback |
| P2 | Context compaction correctness | Current compaction is mostly head-tail and runs after budget trimming | Compaction happens before destructive trimming and can persist summaries |
| P3 | MCP loop protocol hardening | Tool calls depend on prompt JSON parsing | Tool calls are schema-validated, bounded, and test-covered |
| P3 | Trajectory query/API hardening | Query path has N+1 loading and weak limits | APIs have bounded limits, efficient queries, and auth/rate-limit tests |
| P4 | Config/docs/test alignment | Example/default behavior diverges in places | Docs, examples, and tests describe the same runtime behavior |
| P4 | Regression/eval harness | No stable harness quality benchmark | A small eval suite tracks tool use, compaction, and verification regressions |

---

## Current Baseline

Files that already exist and should be evolved instead of replaced:

- `src/service.rs`: Main message pipeline, MCP loop, Code Mode fallback, compaction hook, trajectory hook.
- `src/code_mode.rs`: Code Mode planning, policy, execution, subprocess wrapper.
- `src/harness/trajectory.rs`: Trajectory record types and logger.
- `src/harness/observability.rs`: Prometheus metric structs for trajectories.
- `src/harness/verifier.rs`: Timing/schema/semantic verifier scaffolding.
- `src/harness/compactor.rs`: Head-tail compaction scaffold.
- `src/memory.rs`: SQLite storage for memory, swarm audit, and trajectory tables.
- `src/http.rs`: Runtime construction and trajectory API endpoints.
- `config/xiaomaolv.example.toml`: Public configuration example.
- `tests/harness_*.rs`, `tests/http_api.rs`, `tests/config_bootstrap.rs`: Existing test entry points.

Useful verification commands:

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

---

### Task 1: Make Code Mode Safety Semantics Honest

**Priority:** P0

**Problem:** `AgentCodeModeSettings` exposes `allow_network`, `allow_filesystem`, and `allow_env`, but `CodeModePolicy::validate_plan` does not enforce them. In subprocess mode, the child process is not an OS sandbox; it rebuilds MCP runtime from local config and then applies only tool allow-list, max calls, parallelism, runtime, timeout, and output budget.

**Files:**
- Modify: `src/code_mode.rs`
- Modify: `src/config.rs`
- Modify: `config/xiaomaolv.example.toml`
- Modify: `README.md`
- Test: `tests/config_bootstrap.rs`
- Test: add or extend `src/code_mode.rs` unit tests

- [x] Decide the product stance: enforce `allow_*` as hard policy, or rename them to `requested_allow_*` / remove from public config until enforcement exists.
- [x] If enforcing, add policy metadata to `McpToolInfo` or MCP server config so tools can be classified as network/filesystem/env capable.
- [x] Reject Code Mode plans that call tools disallowed by `allow_network`, `allow_filesystem`, or `allow_env`.
- [x] In subprocess mode, pass only the filtered allowed tool list and fail closed when a tool has unknown capability metadata.
- [x] Add tests where a fake filesystem/network/env tool is rejected with all `allow_* = false`.
- [x] Add tests where the same tool is accepted when the relevant flag is true.
- [x] Update docs to state exactly what subprocess mode does and does not isolate.
- [x] Run `cargo test --all-targets code_mode -- --nocapture`.
- [x] Run full format, lint, and tests.

**Acceptance Criteria:**
- Operators cannot enable Code Mode while believing unavailable sandbox guarantees exist.
- The docs and runtime policy agree.
- A denied capability fails before tool execution.

---

### Task 2: Complete Trajectory Lifecycle Coverage

**Priority:** P1

**Problem:** Trajectory logging currently covers MCP JSON loop happy/max-iteration paths, but model is `"unknown"`, token fields are empty, Code Mode success bypasses trajectory logging, and provider/tool errors can leave unfinished trajectories.

**Files:**
- Modify: `src/service.rs`
- Modify: `src/harness/trajectory.rs`
- Modify: `src/memory.rs`
- Test: `tests/harness_trajectory.rs`
- Test: add service-level coverage in `tests/service_pipeline.rs` or a new `tests/service_harness_trajectory.rs`

- [x] Introduce a small RAII-style or explicit `TrajectoryRun` helper that starts once and finishes exactly once.
- [x] Ensure MCP non-streaming and streaming loops finish trajectory on final answer, max iterations, provider error, tool error policy exit, and internal error.
- [x] Add trajectory coverage for Code Mode direct-success path.
- [x] Store provider/model name instead of hardcoded `"unknown"` where available.
- [x] Add `total_tokens` only if provider usage is available; otherwise leave `None` but document why.
- [x] Replace silent ignored logger failures with structured warnings that include `trajectory_id`.
- [x] Add tests proving failed provider completion records `InternalError` or equivalent finish state.
- [x] Add tests proving Code Mode direct success records a trajectory.
- [x] Run `cargo test --test harness_trajectory -- --nocapture`.
- [x] Run full format, lint, and tests.

**Acceptance Criteria:**
- No started trajectory is intentionally left unfinished.
- Code Mode and MCP loop behavior are both visible in trajectory data.
- Error exits are distinguishable from final answers.

---

### Task 3: Wire Harness Metrics Into Runtime

**Priority:** P1

**Problem:** `TrajectoryMetrics` exists, but runtime construction does not instantiate or attach it. Tool-call metric methods are guarded by `self.trajectory_metrics`, which is `None` in the normal HTTP startup path.

**Files:**
- Modify: `src/http.rs`
- Modify: `src/service.rs`
- Modify: `src/harness/observability.rs`
- Test: `tests/harness_observability.rs`
- Test: `tests/http_api.rs`

- [x] Decide endpoint shape: reuse `/v1/code-mode/metrics`, create `/v1/harness/metrics`, or expose a combined `/metrics`.
- [x] Instantiate `TrajectoryMetrics` during runtime build when harness trajectory or verification is enabled.
- [x] Add `.with_trajectory_metrics(...)` to the normal service construction path.
- [x] Record completed trajectory duration and iteration count, not only per-tool duration.
- [x] Add labels for `ok` or `status` if cardinality remains bounded.
- [x] Add an HTTP test that sends a tool-using request and confirms trajectory metric counters increment.
- [x] Add a test confirming metrics endpoint requires the same auth/rate-limit behavior as diagnostics.
- [x] Run `cargo test --test harness_observability -- --nocapture`.
- [x] Run full format, lint, and tests.

**Acceptance Criteria:**
- Metrics visible in tests are produced by real service flow, not only direct unit calls.
- Completed trajectory count, tool-call count, duration, and iteration metrics move during requests.

---

### Task 4: Add Tool Result Verification That Can Affect Behavior

**Priority:** P2

**Problem:** Current verification mostly logs slow calls. Schema verification is ineffective because `ToolCallRecord.result` is already a `serde_json::Value`, and semantic verification is a placeholder.

**Files:**
- Modify: `src/harness/verifier.rs`
- Modify: `src/service.rs`
- Modify: `src/config.rs`
- Modify: `config/xiaomaolv.example.toml`
- Test: `tests/harness_verifier.rs`
- Test: add MCP loop behavior tests in `tests/service_pipeline.rs` or `tests/service_mcp_reload.rs`

- [x] Replace generic `SchemaVerifier` with `ToolSchemaVerifier` that validates `arguments` against `McpToolInfo.input_schema` before execution.
- [x] Add result-shape checks for common MCP response conventions: error objects, empty results, oversized truncation, and unexpected null.
- [x] Add `verification_mode = "observe|retry|block"` config with default `observe`.
- [x] In `retry` mode, allow one model retry when verification fails before feeding result back.
- [x] In `block` mode, stop the loop and ask the provider for a safe final answer explaining the tool failure.
- [x] Persist verification issues into trajectory tool-call metadata or a new related table.
- [x] Add tests for observe mode not changing output.
- [x] Add tests for retry/block mode changing the loop behavior.
- [x] Run `cargo test --test harness_verifier -- --nocapture`.
- [x] Run full format, lint, and tests.

**Acceptance Criteria:**
- Verification can be configured as passive observation or active guardrail.
- Tool argument schema failures are caught before tool execution.
- Verification issues are visible in trajectory/API output.

---

### Task 5: Add Final Answer Verification

**Priority:** P2

**Problem:** The harness does not verify whether the final answer is consistent with tool results, whether a tool failure was hidden, or whether required output format was followed.

**Files:**
- Modify: `src/harness/verifier.rs`
- Modify: `src/service.rs`
- Modify: `src/config.rs`
- Test: new `tests/harness_output_verifier.rs`
- Test: service integration tests in `tests/service_pipeline.rs`

- [x] Define `OutputVerificationRequest` containing final answer, recent history, tool calls, and channel.
- [x] Define `OutputVerificationResult` with `passed`, `confidence`, `issues`, and `suggested_revision`.
- [x] Add a deterministic verifier for basic issues: empty answer, unresolved JSON tool call emitted to user, hidden tool error, and required format mismatch.
- [x] Add optional LLM self-check only behind config, with strict max prompt/result size.
- [x] Add `output_verification_mode = "off|observe|revise_once|block"` with default `off`.
- [x] In `revise_once`, call provider once with verification issues and require a final answer.
- [x] Record output verification result on the trajectory.
- [x] Add tests for hidden tool error detection.
- [x] Add tests for revise-once path.
- [x] Run full format, lint, and tests.

**Acceptance Criteria:**
- Bad final answers can be detected without relying only on logs.
- Optional revision is bounded to one pass.
- Verification outcome is queryable after the request.

---

### Task 6: Make Context Compaction Correct and Persistent

**Priority:** P2

**Problem:** `AgeBased` and `BudgetBased` are no-ops. Head-tail compaction runs after `apply_context_budget`, so important history may already be removed before it can be summarized. Summaries are not persisted.

**Files:**
- Modify: `src/harness/compactor.rs`
- Modify: `src/service.rs`
- Modify: `src/memory.rs`
- Modify: `src/config.rs`
- Test: `tests/harness_compactor.rs`
- Test: add memory tests in `tests/memory_store.rs`

- [x] Move compaction before destructive budget trimming, while still preserving the final `apply_context_budget` safety pass.
- [x] Add timestamp-aware stored context or a memory lookup method that can support `AgeBased`.
- [x] Implement `BudgetBased` using the same token estimator as `apply_context_budget`.
- [x] Add a persisted summary table or memory record type with source message IDs, strategy, created_at, and invalidation metadata.
- [x] Reuse existing persisted summaries when the source window has not changed.
- [x] Add summary quality guardrails: max chars, no empty summary, and no raw tool JSON leakage.
- [x] Add tests proving budget-based compaction reduces tokens before budget trimming.
- [x] Add tests proving persisted summary is reused.
- [x] Add tests proving recent messages are never compacted below `context_min_recent_messages`.
- [x] Run `cargo test --test harness_compactor -- --nocapture`.
- [x] Run full format, lint, and tests.

**Acceptance Criteria:**
- Long conversations are summarized before losing context.
- Compaction reduces repeated LLM summarization work across turns.
- Head, tail, recent minimum, and memory-score priorities remain predictable.

---

### Task 7: Harden MCP Tool Loop Protocol

**Priority:** P3

**Problem:** The loop asks the model to emit raw JSON. Invalid JSON becomes a final answer; there is no local schema validation of `arguments`, no multi-tool support in normal MCP loop, and no explicit parse-error recovery.

**Files:**
- Modify: `src/service.rs`
- Modify: `src/mcp.rs`
- Modify: `src/harness/verifier.rs`
- Test: service MCP tests in `tests/service_pipeline.rs` or a new `tests/service_mcp_loop.rs`

- [x] Add local validation that requested `server/tool` exists in the listed tool set.
- [x] Validate `arguments` against `input_schema` before `runtime.call_tool`.
- [x] Treat malformed tool-call JSON that looks like an attempted tool call as a parse error, not final answer.
- [x] Add one bounded retry prompt for parse errors.
- [x] Support array tool-call responses only if each call validates and `max_parallel` policy allows it.
- [x] Keep single-call behavior as the default until multi-call policy is configured.
- [x] Add tests for invalid JSON recovery.
- [x] Add tests for unknown tool rejection.
- [x] Add tests for schema-invalid arguments.
- [x] Run full format, lint, and tests.

**Acceptance Criteria:**
- Tool-loop failures are deterministic and visible.
- The model cannot call unavailable tools by string alone.
- Parse recovery is bounded and does not loop indefinitely.

---

### Task 8: Harden Trajectory Storage and Query APIs

**Priority:** P3

**Problem:** Querying trajectories loads tool calls per row, limits are not clamped, and `UNIQUE(trajectory_id, iteration, server, tool)` can overwrite repeated calls in the same iteration.

**Files:**
- Modify: `src/memory.rs`
- Modify: `src/harness/trajectory.rs`
- Modify: `src/http.rs`
- Test: `tests/harness_trajectory.rs`
- Test: `tests/http_api.rs`

- [x] Add a stable `call_index` or unique call ID to each tool call record.
- [x] Replace `INSERT OR REPLACE` with insert-only behavior unless an explicit update is required.
- [x] Clamp trajectory API `limit` to a safe maximum such as 500.
- [x] Batch-load tool calls for list queries to avoid N+1 queries.
- [x] Add filters for `exit_reason` and `has_tool_errors` if needed by dashboard workflows.
- [x] Add API tests for auth, rate limit, limit clamp, and detail lookup.
- [x] Add tests with repeated same server/tool calls in one trajectory.
- [x] Run full format, lint, and tests.

**Acceptance Criteria:**
- List queries stay bounded and efficient.
- Repeated tool calls are not overwritten.
- API behavior is tested at the HTTP layer.

---

### Task 9: Align Config, Examples, and README

**Priority:** P4

**Problem:** Some defaults and examples are easy to misread, especially `enable_trajectory = true` in the example while Rust defaults false, and hardcoded compaction strategy values.

**Files:**
- Modify: `src/config.rs`
- Modify: `config/xiaomaolv.example.toml`
- Modify: `README.md`
- Modify: `README.zh.md`
- Test: `tests/config_bootstrap.rs`

- [x] Decide whether example config should show production-recommended values or Rust defaults.
- [x] Add comments distinguishing "default" from "recommended".
- [x] Expose non-hardcoded values for age-based days and budget-based max tokens, or remove these strategies from config until implemented.
- [x] Add config tests for every harness field.
- [x] Document which harness features are passive and which can affect model output.
- [x] Run config bootstrap tests.
- [x] Run full format, lint, and tests.

**Acceptance Criteria:**
- A new operator can predict runtime behavior from config docs.
- Unsupported or no-op config values are not advertised as real features.

---

### Task 10: Build a Small Regression/Eval Harness

**Priority:** P4

**Problem:** There is no repeatable way to tell whether harness changes improve or regress tool use quality, trajectory completeness, compaction quality, or verification behavior.

**Files:**
- Create: `tests/harness_eval.rs`
- Create: `docs/agent-harness-eval.md`
- Modify: `src/service.rs` only if a test seam is needed

- [x] Define 5 deterministic fake-provider scenarios: no tool needed, valid tool call, malformed tool JSON, tool error, max iterations.
- [x] Define 3 long-context scenarios: no compaction needed, head-tail compaction, budget-based compaction.
- [x] Define 3 verification scenarios: observe warning, retry once, block.
- [x] Assert trajectory exit reason, tool call count, final answer, and verification issues for each scenario.
- [x] Keep eval tests deterministic with fake providers and fake MCP runtime.
- [x] Document how to run the eval subset.
- [x] Run `cargo test --test harness_eval -- --nocapture`.
- [x] Run full format, lint, and tests.

**Acceptance Criteria:**
- Future harness work has a stable regression suite.
- The suite exercises behavior, not only isolated structs.

---

## Suggested Development Order

1. Task 1: Make Code Mode Safety Semantics Honest
2. Task 2: Complete Trajectory Lifecycle Coverage
3. Task 3: Wire Harness Metrics Into Runtime
4. Task 4: Add Tool Result Verification That Can Affect Behavior
5. Task 5: Add Final Answer Verification
6. Task 6: Make Context Compaction Correct and Persistent
7. Task 7: Harden MCP Tool Loop Protocol
8. Task 8: Harden Trajectory Storage and Query APIs
9. Task 9: Align Config, Examples, and README
10. Task 10: Build a Small Regression/Eval Harness

This order intentionally puts trust and observability before optimization. A harness that records incomplete data or advertises unenforced safety semantics will make later improvements hard to evaluate.

---

## Per-Task Completion Checklist

Use this checklist before marking any task complete:

- [ ] New or changed behavior has at least one failing test first. Regression coverage was added, but red-first evidence was not retained for every incremental edit.
- [x] The implementation is scoped to the files named in the task, unless a new dependency is discovered and documented.
- [x] `cargo fmt --all` passes.
- [x] `cargo clippy --all-targets -- -D warnings` passes.
- [x] `cargo test --all-targets` passes.
- [x] README/config/docs are updated if operator-facing behavior changed.
- [x] Any new harness config has a default, example value, and config bootstrap test.
- [x] Any new trajectory field is covered by storage and HTTP/query tests.

## 2026-06-17 Follow-up

The next optimization phase deepens the harness interfaces:

- `HarnessStore` separates harness persistence from conversation memory.
- `AgentRun` owns start/log/finish lifecycle semantics.
- `ToolProtocol` owns MCP proposal parsing, validation, execution envelopes, and feedback.
- `OutputExit` owns final answer verification and bounded revision/block behavior.
- `ExecutionEnvironment` makes Code Mode isolation explicit.
- `SkillRuntime::select` exposes selected skill metadata before prompt rendering.
