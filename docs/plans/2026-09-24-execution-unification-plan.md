# Execution Unification Plan — Pipeline Decomposition, Swarm Convergence, Controlled Writes

**Status:** In progress  
**Date:** 2026-09-24  
**Direction:** `docs/direction.md` (read first — this plan implements that north star)  
**Supersedes:** Nothing. `2026-08-03-loop-engineering-harness.md` Phase 2 invariants remain active.

## Goal

Move xiaomaolv from "a chat gateway plus a separate durable harness" toward one durable, gated
execution fabric:

1. **Decompose `src/service.rs`** into a module tree with named pipeline stages (no behavior
   change).
2. **Project swarm runs into loop-engine terms** so multi-agent work becomes durable and
   resumable instead of in-process only.
3. **Enable `external_write` behind gates** (config flag, handler allowlist, approval,
   idempotency, reconciliation) so the harness can act, not only observe.
4. **Deepen the gates** (eval scorecards, signal-to-goal polish) — the differentiator.

**Architecture note:** `src/service.rs` uses the Rust 2018 `foo.rs` + `foo/` layout already used
by `src/channel.rs` + `src/channel/` and `src/http.rs` + `src/http/`. Submodules of
`crate::service` are descendants of the module that defines `MessageService`, so they can hold
`impl MessageService` blocks and access private fields/methods without widening visibility.
Each decomposition task is therefore a pure code move plus a `pub use` where an external path
must be preserved.

## Current baseline (measured)

`src/service.rs` = 5,575 lines total, ~4,680 non-test:

| Region | Lines | Contents |
|---|---|---|
| settings + `MessageService` + swarm types | ~1-360 | `AgentSwarmSettings`, `SwarmExecutionShared`, etc. |
| constructors / builders / diagnostics | ~361-830 | `new*`, `with_*`, metrics accessors |
| `verify_final_answer`, `handle`, `handle_stream` | ~832-1080 | two near-identical ~110-line pipelines |
| storage proxies (scheduler/swarm audit/trajectory) | ~1082-1390 | thin `self.memory.*` forwards |
| swarm execution | ~1388-2016 | activation, recursive node executor, merge |
| scheduler intent | ~2017-2066 | `detect_telegram_scheduler_intent` |
| completion paths | ~2068-3190 | plain / code-mode / MCP loop, buffered + streaming |
| helpers + free functions | ~3187-4687 | circuit breaker, compaction, time fast path, swarm prompts |
| tests | ~4688-5575 | ~890 lines of unit tests |

## NOT in scope

- Any behavior change in Phases A. Message handling, swarm semantics, and prompts are frozen;
  decomposition moves code, nothing else.
- Executing swarm nodes through `LoopWorker` claims (Phase B keeps the in-process executor;
  only the *record* becomes durable).
- Exactly-once guarantees for external writes. The contract is at-least-once with approval,
  idempotency keys, and reconcile-without-replay.
- New channels, providers, memory backends, or multi-tenant anything.
- Desktop UI (Phase E in the direction doc; the HTTP/SSE contract is already stable).

## Priority overview

| Priority | Task | Why first | Done when |
|---|---|---|---|
| P1 | T1 shared `prepare_turn` stage | Kills ~110 duplicated lines; creates the stage seam everything else hangs off | `handle`/`handle_stream` share one prelude; suite green |
| P1 | T2 swarm -> `service/swarm.rs` | Largest self-contained block (~800 lines incl. helpers); frees the file | swarm code moved; `crate::service::*` paths unchanged; suite green |
| P1 | T3 completion paths -> `service/completion*.rs` | ~1,120 lines of MCP loop / code mode / plain paths | completion code moved; suite green |
| P1 | T4 delegates + streaming adapters -> submodules | ~400 lines of proxies and sink adapters | service.rs is orchestration only |
| P1 | T5 tests -> `service/tests.rs` | Keeps the root file readable | `service.rs` <= ~1,500 lines |
| P1 | T6 durable swarm projection | First real convergence: swarm runs survive crashes | run/node records land in loop-engine tables; resume reports swarm state |
| P2 | T7 internal auto-approval policy | Required for any dynamic/non-interactive workflow growth | `internal:auto` actor bounded to effect <= `read` + budget cap; documented |
| P2 | T8 message-turn durability decision | Decide whether trajectory frames suffice for chat turns | decision recorded; minimal impl if needed |
| P2 | T9 `external_write` enablement | Converts harness from analyst to actor | config flag + allowlist + `channel_send` handler + idempotency + reconcile semantics; default off |
| P2 | T10 eval scorecards -> evolution gate | Raises evolution ceiling | benchmark suite emits scorecards consumed by shadow eval |
| P3 | T11 signal->goal polish | Dedup/review UX improvements | operator can review/convert signals end-to-end |
| P3 | T12 Desktop control plane | Only worth it after Phase C | out of scope this plan; placeholder |

## Implementation tasks

### T1 (P1) — Extract the shared `prepare_turn` stage

**Problem:** `handle` (859-966) and `handle_stream` (968-1080) duplicate ~110 lines:
persist user message -> time-query fast path -> load context -> compaction -> budget ->
skills -> evolution policy -> builtin time context -> swarm dispatch. They diverge only at the
tail (buffered vs streamed completion) and in how early answers are emitted.

**Files:**
- Modify: `src/service.rs`

**Steps:**
- [x] Add `enum PreparedTurn { Immediate { text: String }, Ready { history: Vec<StoredMessage> } }`
  near `CompletionOutcome`. `Immediate` carries an already-verified final answer (fast-path or
  swarm); `Ready` carries the fully-prepared history.
- [x] Add `async fn prepare_turn(&self, incoming: &IncomingMessage) -> anyhow::Result<PreparedTurn>`
  performing: user-message persist -> fast-path check (verify, return `Immediate`) -> context
  load -> compaction -> budget -> skills -> evolution -> time context -> swarm dispatch
  (verify, return `Immediate`) -> `Ready`.
- [x] Rewrite `handle` as `match self.prepare_turn(&incoming).await?` — `Immediate`: persist +
  return; `Ready`: `complete_with_optional_mcp` -> conditional verify -> persist -> return.
- [x] Rewrite `handle_stream` identically except `Immediate` emits via `sink.on_delta` (only when
  non-empty) before persist, and `Ready` calls `complete_with_optional_mcp_stream`.
- [x] Verify: `cargo test --test service_pipeline --test harness_eval` plus
  `cargo test --lib service` (fast-path, swarm, streaming tests must pass unchanged).

### T2 (P1) — Move swarm execution into `src/service/swarm.rs`

**Problem:** ~800 lines of swarm code (types, audit proxies, recursive executor, prompt
builders, heuristics) sit inside `service.rs`. It is the most self-contained subsystem and the
Phase-B convergence target — it needs its own home before it grows a loop-engine projection.

**Files:**
- Create: `src/service/swarm.rs`
- Modify: `src/service.rs` (`mod swarm;` + `pub use` for moved pub types + deletions)

**Steps:**
- [x] Move `AgentSwarmSettings` (keep `pub use swarm::AgentSwarmSettings` in `service.rs` so
  `crate::service::AgentSwarmSettings` keeps working for `config.rs`, `channel.rs`, `http.rs`,
  and the eight test files importing it).
- [x] Move private swarm types: `SwarmExecutionShared`, `SwarmAgentSpec`, `SwarmNodeOutcome`,
  `SwarmModelError`, `SwarmActivationDecision`, `SwarmNodePlanEnvelope`, `SwarmChildPlan`.
- [x] Move methods into `impl MessageService` in the submodule: `try_swarm_reply` (as
  `pub(super)`), `detect_swarm_activation`, `execute_swarm_node`, `swarm_complete_with_timeout`,
  `build_swarm_summary_suffix`, `next_swarm_run_id`, and the four audit proxies
  (`list_agent_swarm_runs`, `load_agent_swarm_tree`, `load_agent_swarm_node`,
  `cleanup_agent_swarm_audit`).
- [x] Move free helpers used only by swarm: `parse_flexible_json_value` (swarm-only — moved),
  `sample_swarm_history`, `build_swarm_planner_messages`, `build_swarm_answer_messages`,
  `build_swarm_merge_messages`, `default_swarm_nickname`, `truncate_swarm_text`,
  `looks_like_swarm_task`.
- [x] New file starts with `use super::*;` (deliberate: the move must not re-litigate import
  lists; tightening imports is a separate cleanup).
- [x] Verify: `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test --all-targets` — the agent-swarm tests (`tests/agent_swarm_store.rs`, swarm unit
  tests) unchanged and green.

### T3 (P1) — Move completion paths into `src/service/completion*.rs`

**Problem:** `complete_with_optional_mcp`, `complete_plain_provider`, `complete_*_for_run`,
`complete_with_code_mode`, `complete_with_mcp_loop`, `complete_with_mcp_loop_stream` (~2068-3190)
plus their free helpers (`build_mcp_system_prompt`, `parse_mcp_tool_call`, stream replay
chunker, code-mode audit/circuit functions) are ~1,500 lines of provider-call machinery.

**Steps:**
- [x] Create `src/service/completion/` split as `mod.rs` (dispatch + stream sinks + telemetry),
  `mcp_loop.rs` (both MCP loop variants + prompt builder), and `code_mode_path.rs` (Code Mode
  path + timeout circuit) — the single-file form exceeded the ~900-line threshold.
- [x] Move the completion methods into `impl MessageService` in the submodule(s) plus the free
  helpers they own. `CompletionOutcome` stays in `service.rs` (consumed by `handle`);
  `CodeModeCompletion`, `CodeModeAttempt`, `CodeModeCircuitChange`, `McpLoopTelemetry`, and the
  stream sink adapters moved into `completion/`. Test-only imports reach them via
  `pub(super)`/`pub(crate)` re-exports in `completion/mod.rs`.
- [x] Keep `code_mode_diagnostics`/`code_mode_metrics_prometheus` accessors in `service.rs`
  (public API) — smallest diff.
- [x] Verify: full `cargo test --all-targets` — MCP loop and code-mode tests must pass
  unchanged.

### T4 (P1) — Move delegates and streaming adapters into submodules

**Steps:**
- [x] Create `src/service/delegates.rs`: the thin storage proxies (`observe`, scheduler job
  CRUD/claim/complete/fail, pending-intent, trajectory queries, group alias/profile proxies)
  and `detect_telegram_scheduler_intent` + `parse_scheduler_intent_json`. Swarm audit reads
  already live in `swarm.rs`.
- [x] ~~Create `src/service/streaming.rs`~~ — satisfied by T3: `StreamSink` adapter structs,
  `replay_text`, `resolve_provider_stream_reply`, `chunk_text_for_stream_replay` all moved into
  `src/service/completion/mod.rs` with the completion paths that use them.
- [x] Verify: `cargo test --all-targets`.

### T5 (P1) — Move tests into `src/service/tests.rs`

**Steps:**
- [x] Move `#[cfg(test)] mod tests` (and `#[cfg(test)]` free helpers like `truncate_json_value`,
  `parse_mcp_tool_call` if only tests use them) into `src/service/tests.rs` — `parse_mcp_tool_call`
  moved earlier into `completion/mcp_loop.rs` and is reached via re-export.
- [x] Additionally split `src/service/time_query.rs` (time fast-path free helpers) so the root
  file meets its size target without mixing unrelated domains.
- [x] Target: `service.rs` is now ~1,470 lines containing settings structs, `MessageService`,
  builders, `handle`/`handle_stream` orchestration, `prepare_turn`, and stage helpers.
- [x] Verify: `cargo test --all-targets`, `cargo clippy --all-targets -- -D warnings`.

### T6 (P1) — Durable swarm projection

**Problem:** swarm runs execute in-process; a crash loses the tree. Loop engine already has the
record model (Goal/WorkItem/Attempt/Checkpoint) and `Replan` for dynamic DAG growth.

**Design sketch (validate in implementation):** when `try_swarm_reply` activates, create a Goal
(`created_by = "internal:swarm"`, objective = root task) and immediately approve+activate it via
the internal auto-approval seam (T7) — for T6 the projection may reuse an existing approval
path internally. Each `execute_swarm_node` call maps to a WorkItem keyed by `agent_id`; node
plan/answer/children persist as Attempt checkpoints. Recursive `delegate` plans append steps by
committing a new workflow revision (the `Replan` path) rather than mutating the immutable
revision. Swarm audit tables stay the Telegram-facing view; loop-engine tables become the
durable system of record.

**Design (validated against the code, 2026-09-24):**

*Mapping table*

| Swarm concept | Loop Engine record |
|---|---|
| run (`run_id`) | `GoalRecord` — `objective` = root task, `created_by` = `internal:swarm`; `run_id` carried in every step's `input.swarm_run_id` |
| node (`agent_id` = `{run_id}:{idx}`) | `WorkItemRecord` — `step_id` = `agent_id`, `handler` = `provider_analysis`, `effect` = `local_write` (writes swarm audit + memory locally) |
| parent→child delegation | `input.parent_agent_id` on the child step (see edge note below) |
| node execution | `AttemptRecord` claimed by `worker_id = "swarm:{run_id}"` via a targeted claim |
| node result | `CheckpointRecord` prepared→committed; `WorkOutcome.summary` = answer excerpt, `evidence` = `{exit_status, depth, role_name, child_agent_ids}` |
| node failure/timeout | `fail_attempt(retryable = false)` — swarm keeps its own timeout semantics, no engine retry |
| merged reply | `publish_artifact` (`analysis_report`, name `swarm-{run_id}`) during the root node's commit so its `artifact_ids` satisfy the `ArtifactExists` criterion → `verify_goal` → `achieved`; a run with no answer publishes nothing and the goal stays non-terminal |

*Edge note:* the sketch's parent→child `WorkflowEdge` mapping was rejected during
implementation review. Edges gate `ready` promotion — a child item would stay `pending` until
the parent's attempt finishes, but swarm children execute *inside* the parent's in-process
await, so edge-gated children could never be claimed. Parent linkage is recorded in
`input.parent_agent_id` instead; appended steps start `ready`.

*New primitives required (validated gaps):*

1. `LoopStore::extend_workflow(goal_id, steps, actor)` — append steps to an `approved`/`active`
   goal. Guards: `actor` must start with `internal:`; the approved workflow must contain a step
   whose input carries `"swarm_root": true` plus `"max_nodes": N` (the approval-bound expansion
   budget); total items ≤ `min(32, max_nodes)`; handlers must be registered; appended effect
   classes ⊆ the approved effect manifest. Emits a `workflow.extended` `LoopEventRecord`; the
   goal revision is unchanged — appends are execution records under the approved plan's
   declared expansion budget, not a new plan. (`GoalStatus::Replan` exists but has no writer;
   this method is the T6-sized subset of that path.)
2. `LoopStore::claim_work_item(goal_id, step_id, worker_id, lease_secs, actor)` — targeted
   claim: same lease/fencing/attempt mechanics as `claim_goal_work`, but selects the work item
   with the given `step_id` instead of the lowest-ordinal ready item. `claim_goal_work` cannot
   be reused: parallel siblings make its ordinal order unpredictable.
3. `MessageService::with_loop_engine(Option<Arc<LoopEngine>>)` — wired in `http.rs` where the
   engine is already built before the service.

*Concurrency semantics:* the swarm appends a child's step and immediately claims it — the
ready-but-unclaimed window is microseconds, while the generic `LoopWorker` polls on a seconds
scale. If the worker wins a race anyway it executes `provider_analysis` on the node's input,
which produces a generic answer (degraded but consistent: the durable record still shows the
node ran); the swarm then sees `claim_work_item → None`, logs a warning, and still completes
the node in-process for the parent merge. Accepted limitation for T6: an operator-facing
`internal:` dispatch exclusion is deferred to T7 if hijacks prove noisy.

*Crash semantics:* a process death mid-run leaves leased `running` attempts that expire and get
reconciled by `recover_goal_state` on the next resume/claim. The swarm audit tables remain the
Telegram-facing view; the goal record shows exactly which nodes finished. Operator `/resume`
(`run_goal_until_idle` bypasses `list_dispatchable_goal_ids`) can drive leftover items to
completion with generic handlers — degraded but durable. In-request swarm resumption is out of
scope for T6.

*Budget honesty:* the projected spec's `ExecutionBudget` mirrors swarm limits
(`max_provider_calls = min(2 × max_agents, 64)`, `deadline_secs = max_run_timeout + margin`);
per-call `reserve_provider_call` enforcement stays out of T6 because node provider calls happen
inside swarm logic, not inside a `WorkHandler`.

**Steps:**
- [x] Write the design section into this doc before coding: projection mapping table
  (run->Goal, node->WorkItem, node outcome->Attempt+Checkpoint) and how `run_id`/`agent_id`
  correlate.
- [x] Add a `LoopStore`-backed projection inside `execute_swarm_node` (behind
  `agent_swarm.enabled && loop_engine.enabled`); failures to project must not fail the reply
  (warn + continue, matching existing audit best-effort semantics). *Done: `extend_workflow`,
  `claim_work_item`, and `with_loop_engine` added; `execute_swarm_node` wraps the inner body
  with claim → inner → commit/fail.*
- [x] `/resume` and the goal-detail HTTP resource must show swarm goals with their node tree.
  *Done automatically: projected goals are ordinary `harness_goals` rows; `resume_goal` and
  the goal-detail/SSE endpoints read them without further work.*
- [x] Tests: new `tests/harness_loop_engine.rs` cases — swarm run produces Goal + WorkItems +
  committed checkpoints; simulated crash mid-run leaves resumable state.
  *Done: `swarm_run_projects_a_durable_goal_tree_and_verifies_it`,
  `swarm_crash_leaves_resumable_durable_state`, and
  `extend_workflow_enforces_internal_actor_manifest_and_budget`.*

### T7 (P2) — Internal auto-approval policy

**Problem:** T6's projection and any future dynamic workflow need approval without a human in
the loop, which today does not exist.

**Steps:**
- [x] Add `internal:auto` actor + policy: auto-approval binds only plans whose effect manifest
  is a subset of `{pure, read}`, whose budget is under configured caps, and whose actor is
  internal. External-write or over-budget plans still require the operator route.
  *Done: `InternalApprovalPolicy` enforced inside `approve_goal` when `actor ==
  "internal:auto"`; subsystem actors (`internal:swarm`) approve only their own code-built
  plan shapes and stay governed by domain validation.*
- [x] Config: `[agent.harness.loop_engine] internal_auto_approve_max_effect = "read"`,
  `internal_auto_approve_max_provider_calls = 16` (defaults; hard ceiling enforced in domain
  validation, not only config). *Done; both parsed and range-checked at startup in `http.rs`.*
- [x] Audit: auto-approvals emit a `LoopEventRecord` with actor `internal:auto` and the bound
  plan hash. *Done: the existing approval event records the actor verbatim.*
- [x] Tests: auto-approve accepts a read-only swarm plan; rejects `local_write`/`external_write`
  plans and over-budget plans; events recorded.
  *Done: `internal_auto_approval_is_bounded_by_policy`.*

### T8 (P2) — Message-turn durability decision

- [x] Evaluate whether per-completion trajectory frames already give chat turns enough durable
  record, or whether `handle` should also emit a loop-engine Attempt. Record the decision in
  `docs/direction.md`; implement only if the gap is real (e.g., mid-turn crash leaves memory
  with a user message and no assistant reply — acceptable or not?).
  *Decided 2026-09-24: no per-turn Attempt — see "Decisions" in `docs/direction.md`. The
  durable record (message + frames + reply) suffices; real recovery needs Phase C's delivery
  gate anyway. Revisit at `channel_send`.*

### T9 (P2) — Controlled `external_write`

**Problem:** the harness can prove safety but cannot act. Enable writes through the gates.

**Steps:**
- [x] Config: `[agent.harness.loop_engine] external_write_enabled = false` (default off) +
  `external_write_handlers = ["channel_send"]` allowlist.
- [x] `WorkflowSpec::validate` gains a policy parameter (or validation moves to an engine-side
  `validate_plan(spec, policy)`) so `external_write` steps are rejected unless the handler is in
  the allowlist and the flag is on; keep the unconditional rejection as the default path.
  *As built:* `ExternalWritePolicy { enabled, allowed_handlers }` lives on `LoopEngine`;
  `WorkflowSpec::validate_with_policy` gates plan, `approve_goal` re-checks the *current*
  policy, `extend_workflow` gates appended steps, and the worker re-checks `enabled` at
  dispatch time — so a flag flip between plan and execution fails closed.
- [x] `WorkHandlerRegistry::register` rejects `external_write` handlers not in the allowlist.
  *As built:* the registry carries `external_write_allowlist` seeded from the engine policy;
  registering a `channel_send` sender without the allowlist entry is a startup error.
- [x] New handler `channel_send`: input `{channel, session_id, text}`; writes a `prepared`
  checkpoint carrying an idempotency key derived from `(goal_id, step_id, attempt_number)`
  *before* sending; sends through the registered channel; records evidence `{sent_at,
  channel}`; on resume, `reconciled` marks committed sends without re-sending (document
  at-least-once honestly).
  *As built:* `OutboundSender` is the injected delivery capability
  (`ChannelOutboundSender` maps `channel="telegram"` onto `TelegramSender`, parsing
  `tg:{chat_id}[:thread:{id}|:reply:{id}]` session ids); the prepared checkpoint is written by
  `process_claim` before `execute` under key `{work_item.id}:{attempt.id}:v1`; a crash between
  send and commit parks the item in `waiting_confirmation` — never resent.
- [x] Approval surface (HTTP detail + Telegram `/goal` review) renders the effect manifest so
  an operator sees `external_write` before approving.
  *As built:* `GET /v1/harness/goals/{id}` now embeds the latest plan (`plan_hash`, `workflow`,
  `acceptance_criteria`, `effect_manifest`) via a new `LoopStore::latest_plan` accessor;
  Telegram `/goal` review prints the manifest and per-step `handler [effect]`, and `/resume`
  item lines show the effect class.
- [x] Tests: flag off -> rejection; flag on + allowlisted -> executes once; crash between
  prepared and committed -> resume reconciles without a duplicate send (assert via fake channel
  sink counting sends).

### T10 (P2) — Eval scorecards feed the evolution gate

- [x] Extend `tests/harness_eval.rs` scenarios into a versioned benchmark producing a scorecard
  artifact (tool-use success, compaction correctness, verification blocks, latency budget).
  Landed as `src/harness/benchmark.rs` (`core@2026-09-24.1`): five prompt-level probes derived
  from the deterministic eval scenarios (direct answer, JSON contract, no internal leak,
  bounded verbosity, bounded summary), plus a new `max_output_chars` assertion so response-
  budget expectations are real checks.
- [x] Wire scorecard artifacts into `EvolutionEngine` shadow eval as additional scored evidence.
  `EvolutionBenchmarkSuite` (`id@version`, 1..=16 validated unique-id cases) attaches via
  `with_benchmark_suite`; `EvolutionScorer::score_with_benchmark` scores operator + benchmark
  cases together, marks provenance per result, and records `benchmark_regressions` and the
  suite label on the persisted scorecard. Operator/benchmark case-id collisions fail closed.
- [x] Gate: a prompt candidate cannot reach `ready` if any benchmark scenario regresses —
  `benchmark_regressions > 0` rejects regardless of `max_regressions`.

### T11 (P3) — Signal->goal polish

- [x] Dedup quality pass on `signals.rs`: three source-scoped layers — exact `(external_id |
  fingerprint)`, `normalized_hash` column (case/whitespace/punctuation collapsed per kind,
  backfilled), and token-set near-duplicates (Jaccard >= 0.9 over the last 128 same-source/kind
  signals).
- [x] Operator review path: `?status=` filter on `GET /v1/harness/signals`,
  `POST /v1/harness/signals/{id}/ignore` (reason required; `proposed`/`ignored` are terminal
  in both directions), and Telegram `/signals` + `/signal <id> [ignore|goal]` surface.

### T12 (P3) — Desktop control plane (placeholder)

- [ ] Deferred until Phase C lands. Consume existing collection/detail/SSE contract; no new
  backend work expected.

## Verification commands

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo test --test harness_loop_engine -- --nocapture
cargo test --test agent_swarm_store --test service_pipeline --test harness_eval -- --nocapture
```

## Progress log

- 2026-09-24 — Plan created. **T1 done:** `PreparedTurn` + `prepare_turn` extracted;
  `handle`/`handle_stream` are now thin tails (~110 duplicate lines removed). **T2 done:**
  swarm subsystem moved to `src/service/swarm.rs` (917 lines); `service.rs` 5,575 -> 4,605.
  `fmt`/`clippy -D warnings`/`cargo test --all-targets` all green, zero behavior change.
- 2026-09-24 — **T3 done:** completion paths split into `service/completion/` (mod 527,
  mcp_loop 728, code_mode_path 357). **T4 done:** storage delegates + scheduler intent moved to
  `service/delegates.rs` (348). **T5 done:** tests -> `service/tests.rs` (899), time-query
  helpers -> `service/time_query.rs` (325); `service.rs` lands at 1,469 lines (<= 1,500).
- 2026-09-24 — **T6 done:** swarm runs project into durable Goal/WorkItem/Attempt/Checkpoint
  via `claim_work_item` (targeted claim) + `extend_workflow` (internal-actor, manifest/budget
  bounded); projection is best-effort and never alters reply behavior.
- 2026-09-24 — **T7 done:** `internal:auto` approvals bounded by `InternalApprovalPolicy`
  (effect ceiling + provider-call cap); subsystem actors keep their code-built plan path.
- 2026-09-24 — **T8 done:** decision recorded — ordinary chat turns do not emit Loop Engine
  Attempts; reply recovery waits for the `channel_send` gate.
- 2026-09-24 — **T9 done:** controlled `external_write` behind `external_write_enabled` +
  handler allowlist enforced at plan/approve/register/dispatch; `channel_send` handler via
  `OutboundSender`; prepared->committed->reconciled checkpoints; crash between send and commit
  parks in `waiting_confirmation`; approval surfaces render effect manifests over HTTP and
  Telegram. 41 test binaries green, `harness_loop_engine` 20/20.
- 2026-09-24 — **T10 done:** versioned benchmark suite (`core@2026-09-24.1`,
  `src/harness/benchmark.rs`) joins every shadow evaluation via `with_benchmark_suite`;
  scorecards record benchmark provenance and `benchmark_regressions`, which are always fatal
  to promotion independent of the operator regression budget; new `max_output_chars` assertion
  makes response-budget scenarios enforceable; operator/benchmark case-id collisions fail
  closed. 41 test binaries green.
- 2026-09-24 — **T11 done:** signal dedup gains normalized-hash and bounded Jaccard
  near-duplicate layers; triage lifecycle closes (`observed`/`triaged` -> `proposed`|`ignored`,
  both terminal); operator review works end-to-end via `?status=` + `POST /ignore` on HTTP and
  `/signals` + `/signal <id> ignore|goal` on Telegram. 41 test binaries green.
