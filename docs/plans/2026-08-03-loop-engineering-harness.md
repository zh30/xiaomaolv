# Loop Engineering Harness — Phase 2 Design

**Status:** Phase 2 core implemented and verified  
**Date:** 2026-08-03  
**Branch:** `codex/self-evolving-harness`  
**Supersedes:** The execution and evidence scope of
`2026-08-03-self-evolving-harness.md`; its prompt-promotion safety invariants remain active.

## Goal

Evolve xiaomaolv from a prompt-improvement loop into a durable Harness engineering system:

1. **Loop Engineering Setup** — durable goal state, work dispatch, checkpoints, crash recovery,
   and explicit `/goal` and `/resume` operations.
2. **Self-testing** — the product can run bounded maintenance suites, persist results, and feed
   failures back into evolution without mutating production directly.
3. **Multi-source Evolution** — community knowledge, user feedback, developer requests,
   trajectories, replays, and maintenance failures enter one provenance-preserving signal model.

The system may discover, normalize, plan, execute safe work, and test itself. Production
activation remains a separate operator-authorized operation.

## Pre-Phase-2 Baseline (historical)

| Existing component | Reuse | Missing boundary |
|---|---|---|
| Telegram scheduler lease/retry logic | Reuse the short SQLite lease/fencing pattern | Its tables and states remain Telegram-specific |
| Agent Swarm run/node audit | Reuse tree-shaped task decomposition concepts | Runs execute in-process and cannot resume |
| Harness trajectories/tool records | Reuse as evidence and replay source | No request snapshot or safe replay contract |
| Evolution candidates/evals/deployments | Keep prompt driver and human promotion unchanged | Evidence and artifacts are prompt/trajectory-specific |
| MCP/Skills/Provider/Channel registries | Reuse registration pattern for work/artifact drivers | Existing MCP tools lack effect/idempotency metadata |
| HTTP worker supervisor | Host loop and maintenance workers | Current evolution cycle status itself is in memory |

The new design must adapt these components. It must not overload Telegram scheduler tables or
turn Agent Swarm audit rows into a generic queue.

## NOT in Scope

- A complete Desktop GUI. Phase 2 defines a stable HTTP/SSE read model that a later Desktop
  client can consume.
- Arbitrary autonomous code edits, commits, deployments, credential changes, or permission
  changes.
- Live MCP calls during Session Replay.
- A universal exactly-once guarantee for external services.
- External vendor-specific community connectors. Phase 2 provides the scoped ingestion
  contract and source adapter seam.
- A general event-sourcing framework. Current state tables plus append-only domain events are
  sufficient for a solo-operated SQLite service.
- Rewriting the existing prompt evolution implementation. It is connected through adapters.

## Implemented Phase 2 Boundary

The delivered core includes durable Goal/WorkItem/Attempt/Checkpoint state, immutable approval
hashes, lease heartbeat and fencing, committed-outcome reconciliation, multi-source immutable
signals, typed artifacts, bounded provider budgets, read-only self-tests with deduplicated failure
signals, per-completion frames, structural replay with no live tools, bounded prompt-candidate
evaluation through the existing EvolutionEngine, HTTP/SSE collection/detail APIs, Telegram
`/goal`/`/resume`, cooperative reload, schema version 1, configuration, and an operator runbook.

Full Desktop UI, arbitrary external-write handlers, retention/pruning automation, and comparative
live-provider replay remain outside this safe core. The HTTP/SSE model, effect enum, and replay mode
types preserve those extension seams without claiming those effects or replays are currently
available.

## Safety Invariants

1. Signals can be converted only into `proposed` Goals, and only through an operator route.
2. `/goal` starts safe planning only. Effectful dispatch requires an operator approval bound to
   an immutable goal revision, plan/workflow hash, effect manifest, acceptance criteria, and
   execution budget.
3. Work execution is at-least-once. No API claims universal exactly-once behavior.
4. Every attempt owns a lease token and monotonically increasing fencing version.
5. Provider and tool calls never occur inside a SQLite transaction.
6. Registered work may retry only within its approved retry policy; production `external_write`
   workflows and handlers are rejected.
7. Prepared/committed outcomes are durable. Resume may mark a committed checkpoint reconciled and
   creates a new Attempt for retried work; it never erases Attempt history.
8. Structural Replay validates captured provider-frame order and hashes; it never calls live tools.
9. Self-test failures emit deduplicated Signals. They cannot approve a Goal or activate Prompt
   Policy.
10. Only known Artifact kinds with bounded content may be published. Generic Artifact activation
    is not implemented.
11. Existing prompt candidate approval, stale-baseline protection, and rollback remain intact.
12. Payloads, provider-call counts, response bytes, retries, and worker concurrency are bounded.
    Retention and token/currency budgets are not claimed.
13. Production Self-tests are read-only. Mutation/crash suites run only against an ephemeral
    SQLite store with fake handlers and providers.
14. Existing prompt candidates, deployments, active pointers, and runtime cache remain the sole
    source of truth for prompt policy.

## Architecture

```text
community / user / developer / trajectory / replay / self-test
                           |
                           v
                 append-only EvolutionSignal
                 provenance + trust + digest
                           |
                   operator review
                           |
                           v
                       proposed Goal
                           |
             deterministic or supplied plan
                           |
                   review-ready revision
           workflow hash + effects + call budget
                           |
                    operator approve
                           |
                           v
                immutable WorkflowSpec
                compiled WorkItem DAG
                           |
          +----------------+----------------+
          |                                 |
     leased Attempt                    manual gate
          |
    prepared checkpoint
          |
    registered WorkHandler
          |
    committed checkpoint
          |
      verification / reconciliation
          |
          v
      achieved Goal + typed Artifacts

Prompt candidate evaluation is one bounded handler;
promotion/activation/rollback remains in EvolutionEngine.
```

Runtime ownership:

```text
HTTP / Telegram commands
          |
     LoopEngine API
          |
    LoopStore trait
          |
       SQLite

WorkerSupervisor
  ├── LoopWorker        claim -> heartbeat -> execute -> commit
  ├── maintenance timer run read-only core self-test
  └── EvolutionWorker   existing propose/evaluate-only cycle
```

Configuration reload is cooperative: signal the old generation, stop new claims, await bounded
worker shutdown, and then start the replacement generation. If a claim outlives the process,
leases expire naturally and the next worker still relies on fencing and checkpoint reconciliation.

## Durable Domain Model

### Goal

`GoalRecord` is the durable desired outcome, not an execution attempt.

```text
id / revision
objective / status
source_signal_ids[]
created_by / created_at / updated_at
```

The enum preserves these states:

```text
proposed | planning | review_ready | approved | active | verifying | achieved
rejected | canceled | paused | blocked | failed | replan
```

The delivered HTTP/worker path uses `proposed -> review_ready -> approved -> active -> verifying ->
achieved`, with `failed` on exhausted work. The additional states are reserved in the domain; no
pause/cancel/reject HTTP operations are exposed in this phase.

Acceptance criteria are `self_test_suite`, `artifact_exists`, or `manual_approval`. Verification
checks immutable Self-test/Artifact/manual evidence and marks a fully satisfied Goal `achieved`.
Manual evidence requires the explicit verify endpoint.

An operator `/goal <objective>` creates a `proposed` Goal and stores the deterministic recommended
plan; no work executes. A Goal derived from a Signal also starts as `proposed`. Planning stores a
new immutable Workflow revision and approval occurs only after its effects, criteria, and budget
are visible. Approval checks both expected Goal revision and `plan_hash`.

### WorkItem

`WorkItemRecord` is one node in a persisted DAG.

```text
id / goal_id / step_id / ordinal
handler / input / effect / max_attempts
dependency_ids[] / status
```

```text
pending -> ready -> running -> succeeded
                     |   |
                     |   +-> ready (bounded retry)
                     +-----> failed
reserved: waiting_confirmation | canceled | blocked
```

Dependencies must exist in the same goal and the graph must be acyclic. A work item becomes
claimable only when every dependency succeeded. Exhausted attempts fail the WorkItem; downstream
pending nodes become blocked and the Goal becomes failed. Lease/fencing columns remain in SQLite
and are returned through Attempt/Resume reports rather than duplicated in `WorkItemRecord`.

### Attempt and Checkpoint

`AttemptRecord` represents one worker claim. `CheckpointRecord` is durable evidence about a
safe boundary inside that attempt.

```text
Attempt: id / work_item_id / number / lease_token / fencing_version
         worker_id / status / started_at / finished_at / error?

Checkpoint: id / goal_id / work_item_id / attempt_id / phase
            idempotency_key / bounded WorkOutcome? / created_at / updated_at
```

Checkpoint phases share an operation ID and persist a bounded `WorkOutcome`:

```text
prepared -> committed -> reconciled -> work succeeded
    |           |
    |           +-- crash before finish -> recover outcome, do not execute again
    |
lease expiry -> abandon attempt -> bounded retry or fail
```

`/resume <goal-id>` performs a transaction that validates the goal state, expires stale leases,
reconciles committed checkpoints, requeues retryable work, unlocks satisfied dependencies, and
records an event. It does not change the approved Workflow or replay a committed handler effect. A
subsequent claim creates a new Attempt with a higher fencing token.

## Execution Contract

Every registered `WorkHandler` declares:

```text
name()
effect_class(): pure | read | local_write | external_write
retryable(error)
execute(context) -> WorkOutcome
```

Rules:

- The Workflow validator accepts only the six built-in names and rejects `external_write`.
- The runtime registry also rejects any `external_write` handler and duplicate/empty names.
- A handler's declared effect must exactly match the approved WorkItem effect before execution.
- Retry is bounded by the approved step policy and the handler's `retryable` decision.
- A stale fencing version cannot heartbeat, checkpoint, complete, or fail an attempt.
- A lease check and prepared checkpoint precede handler execution; committed outcomes are persisted
  before an attempt is finalized.
- Provider reservations persist call count, absolute deadline, and cumulative response bytes before
  a call; provider I/O happens outside the SQLite transaction.

Production handlers are bounded local/read operations: `goal_planner`, `provider_analysis`,
`session_replay`, `self_test_suite`, `evolution_evaluate`, and `manual_gate`.

## Multi-source Evolution

### EvolutionSignal

All source adapters normalize into one append-only record:

```text
id
kind: trajectory | user_feedback | developer_feedback |
      community | self_test | session_replay | manual
source / external_id?
trust: internal | authenticated | external
content / content_hash / metadata
status: observed | triaged | proposed | ignored
created_at
```

Deduplication uses `(source, external_id)` when supplied and `(source, fingerprint)` for the full
bounded request. Input limits are enforced before persistence: source 160 bytes, external ID 200
bytes, content 16384 characters, and metadata 64 entries/8192 serialized bytes. Callers remain
responsible for not submitting secrets or credentials as Signal content.

External content is persisted as data and never injected as a system instruction by the Loop
Engine. There is no autonomous LLM triage in this phase. An authenticated operator may convert one
Signal into a `proposed` Goal with an explicit objective:

```json
{
  "objective": "Investigate and safely improve recovery visibility"
}
```

The new Goal records the source Signal ID for provenance. Signal ingestion cannot plan, approve,
dispatch, resume, verify, publish Artifacts, or operate Prompt Evolution. Existing trajectory
feedback remains available through the separate Prompt Evolution endpoint; automatic cross-store
normalization is not claimed.

## Dynamic Workflow and Capability Artifacts

### Workflow

`plan_goal_recommended` builds the default deterministic two-step plan (`provider_analysis` then
`self_test_suite`). `POST /goals/{id}/plan` can supply another validated `WorkflowSpec`. Planning
persists the full spec and compiles its steps/dependencies into WorkItems in one transaction; no
provider or handler runs during planning.

```text
steps[]:
  id / handler / effect / input / retry(max_attempts, backoff_secs)
edges[]: from / to
budget: max_provider_calls / deadline_secs / max_response_bytes
acceptance_criteria[]
```

Limits: 32 steps, 64 dependency edges, 32 KiB serialized manifest, acyclic graph, unique step
IDs, registered handlers only, and no `external_write`. The plan hash covers the Workflow plus
acceptance criteria; approval separately persists hashes for the Workflow, effect manifest,
criteria, and budget.

### Artifact registry

`ArtifactRecord` is an immutable, typed output/reference:

```text
id / kind / name / version / content / content_hash
source_goal_id? / parent_artifact_id? / created_by / created_at
```

Kinds are `goal_template`, `dynamic_workflow`, `eval_suite`, `skill_manifest`, `replay_corpus`,
`desktop_view`, `analysis_report`, `self_test_report`, `evolution_evaluation`, and
`prompt_policy_ref`. Content is bounded to 32768 bytes and `(kind, name, version)` is unique.

Artifacts do not have a generic activation pointer in this phase. `prompt_policy_ref` permits only
an `evolution_candidate_id` and optional `deployment_id`; it cannot contain prompt text. Prompt
approval, deployment history, active policy, and rollback remain exclusively in the existing
Evolution tables/API.

## Session Replay

When `agent.harness.enable_trajectory = true`, main plain, Code Mode, and MCP completion paths add
one bounded `TrajectoryFrame` per provider call. One message may therefore produce several ordered
frames. Each frame records:

```text
id / trajectory_id / call_index
model / provider_fingerprint
request_messages / request_was_json / request_hash
response / response_hash
capture: full | redacted | truncated
```

Frames accept at most 256 messages/131072 serialized request bytes and 262144 response bytes.
Runtime captures are conservatively `truncated` because the seed and full provider configuration
are unavailable; operators must treat frame content as sensitive SQLite operational data.

Delivered replay mode:

1. `structural` — validate contiguous call indexes plus request/response hashes entirely from
   recorded frames. It reports `live_tools_executed = 0` and performs no provider/MCP/channel/memory
   action.

Each invocation persists a Replay run and case results. It fails clearly when a trajectory has no
provider frames. `shadow_comparative` exists only as a reserved enum value: there is no endpoint or
live-provider implementation, no recorded-tool substitution, and no automatic Replay-to-Signal
emission in this release.

## Self-testing and Maintenance

The delivered production suite is `core`, a fixed set of read-only SQL integrity checks:

- all 20 Loop Engineering tables exist,
- schema migration version 1 is recorded,
- every WorkItem references its Goal and immutable Workflow,
- committed/reconciled checkpoints retain a WorkOutcome,
- Signal events reference an immutable Signal,
- Goal approvals still resolve to the reviewed Workflow hash.

Each invocation persists a `SelfTestRun` plus case results. A failure emits an internal
`self_test` Signal; the complete failed-case set is fingerprinted so repeated identical failures
do not create a Signal storm. Passing does not approve a Goal, publish/activate Prompt Policy, or
execute a provider/tool.

The operator endpoint can run `core` at any time. Periodic execution is enabled only when the Loop
Engine is enabled and `self_test_interval_secs > 0`; it is independent of `worker_enabled`.
Mutation/crash-window behavior is covered by integration tests with temporary SQLite and fake
handlers/providers, not by production maintenance.

## Control Plane

Operator endpoints use the existing app API key. The caller-supplied actor header remains an audit
label, not identity proof; the fallback is `operator:http`, while Telegram records the verified
admin user ID. Only `POST /v1/harness/signals` uses the separate scoped `ingest_api_key`; it cannot
read Signals or create, plan, approve, dispatch, resume, verify, publish, activate, or roll back
anything.

```text
POST   /v1/harness/signals
GET    /v1/harness/signals
GET    /v1/harness/signals/{id}
POST   /v1/harness/signals/{id}/propose-goal

POST   /v1/harness/goals
GET    /v1/harness/goals
GET    /v1/harness/goals/{id}
POST   /v1/harness/goals/{id}/plan
POST   /v1/harness/goals/{id}/approve
POST   /v1/harness/goals/{id}/resume
POST   /v1/harness/goals/{id}/verify/manual
GET    /v1/harness/goals/{id}/events
GET    /v1/harness/goals/{id}/events/stream

GET    /v1/harness/trajectories/{id}/frames
POST   /v1/harness/trajectories/{id}/replay/structural
POST   /v1/harness/self-tests/{suite}
GET    /v1/harness/self-test-runs/{id}
POST   /v1/harness/artifacts
GET    /v1/harness/artifacts
GET    /v1/harness/artifacts/{id}
```

Telegram admin commands use the same engine:

```text
/goal <objective>
/goal approve <goal-id> <revision> <plan-hash>
/resume <goal-id>
```

The HTTP read model is the future Desktop boundary. Desktop-specific business logic does not
enter the server core.

## Persistence

New SQLite tables:

```text
harness_schema_migrations
harness_signals
harness_signal_events
harness_goals
harness_workflows
harness_goal_approvals
harness_goal_budget_usage
harness_work_items
harness_work_item_dependencies
harness_attempts
harness_checkpoints
harness_events
harness_manual_verifications
harness_artifacts
harness_artifact_events
harness_trajectory_frames
harness_replay_runs
harness_replay_case_results
harness_self_test_runs
harness_self_test_case_results
```

Relevant foreign keys preserve Goal/Workflow/Signal/Artifact provenance. Core plan, approval,
claim, checkpoint, and completion paths use short transactions; events are append-only. No
provider or tool call occurs inside a SQLite transaction.

Important indexes:

- signals: `(kind, created_at, id)` and unique `(source, fingerprint)` / `(source, external_id)`;
- goals: `(status, updated_at, id)`;
- work: `(status, next_attempt_at, lease_until, created_at)`;
- attempts: `(goal_id, started_at, id)` plus unique `(work_item_id, attempt_number)`;
- checkpoints: `(goal_id, sequence)` plus unique `(work_item_id, idempotency_key)`;
- events: `(goal_id, sequence)`;
- replay: `(trajectory_id, created_at, id)`; Self-test: `(suite, started_at, id)`.

## Production Failure Modes

| Failure | Handling | Test | User-visible result |
|---|---|---|---|
| Worker dies with an uncommitted claim | Lease expires; `/resume` abandons/retries within attempt budget | Loop Engine integration | Goal and attempt history remain visible |
| Worker dies after committed checkpoint | Reconcile stored outcome; do not call handler again | Loop Engine crash-window test | Work completes from durable result |
| Stale worker returns after takeover | Lease token/fencing checks reject heartbeat/checkpoint/finish/fail | Loop Engine fencing test | New attempt stays authoritative |
| External-write workflow/handler | Validator and registry reject it before dispatch | Workflow/registry tests | Bad request; no external effect |
| Duplicate community webhook | Source external-ID/fingerprint returns existing Signal | Signal integration | `deduplicated = true` |
| Untrusted Signal content | Scoped caller can only persist external/authenticated data | HTTP auth/scope test | Operator must explicitly propose a Goal |
| Invalid/cyclic/oversized Workflow | Validation fails before WorkItem insertion | Loop Engine boundary tests | Bad request; no partial DAG |
| Replay frames missing | Structural replay returns `trajectory has no provider frames` | Replay integration | Explicit bad request; no approximation |
| Replay attempts live tools | Structural implementation has no tool/provider call path | Replay integration | `live_tools_executed = 0` |
| Provider timeout/output overflow | Goal deadline and reserved response budget bound handler/eval | Evolution/worker tests | Attempt retry/failure remains visible |
| Self-test repeatedly fails identically | Failure-set fingerprint deduplicates internal Signal | Self-test integration | No Signal storm |
| SQLite busy/locked | WAL, five-second busy timeout, and short transactions | Existing store coverage | Error without provider I/O in transaction |
| Payload growth | Pre-persistence size/count limits reject input | Boundary tests | Bad request; retention is not claimed |
| Config reload overlaps generations | Old worker tasks receive shutdown; durable leases/fencing remain authoritative | Runtime behavior | No assumption of exactly-once execution |

## Performance and Retention

- Each claim transaction leases one dependency-ready WorkItem. A worker processes at most eight
  WorkItems for one Goal per dispatch pass; provider and handler calls are outside transactions.
- Loop worker concurrency defaults to 2. Periodic `core` maintenance and structural replay make
  no provider calls; provider-backed handlers share the approved Goal budget and worker bound.
- Lease heartbeat is at most one third of lease duration.
- Signal content is capped at 16,384 characters, workflow/artifact manifests at 32 KiB,
  provider-frame requests at 128 KiB, and responses at 256 KiB. HTTP collection/event queries
  default to 100 results; a defensive hard maximum is a deferred hardening item.
- Every replay and self-test invocation persists an immutable run. Repeated identical Self-test
  failure sets deduplicate the internal Signal by complete failure fingerprint.
- Provider budgets enforce persisted maximum call counts, deadlines, and response bytes.
  Token/currency accounting is not claimed because trusted provider usage is not captured here.
- Retention/pruning is deliberately not automated in this phase; immutable evidence remains until
  a future reference-aware retention job is reviewed and enabled.
- SQLite WAL and the existing five-second busy timeout remain. New write transactions must not
  span an await on a provider, MCP tool, channel, or filesystem operation.

## Implemented Test Coverage

| Test file | Verified behavior |
|---|---|
| `tests/harness_loop_engine.rs` | Approved DAG dispatch, Goal verification, provider budgets, restart/resume, committed-checkpoint reconciliation, stale-worker fencing, approval hash binding, manual acceptance, Signal dedup/proposal, read-only Self-test/failure dedup, structural replay, Artifact/prompt references, and bounded evolution evaluation |
| `tests/http_loop_engine_api.rs` | Operator authentication, scoped Signal ingestion/trust restriction, Goal plan/approve/resume lifecycle, collections, and event cursor |
| `tests/service_harness_trajectory.rs` | Plain and streaming provider frames, Code Mode paths, MCP multi-call order/recovery, and terminal trajectory behavior |
| `tests/harness_evolution_engine.rs` | Cumulative provider response budget, immutable scorecards, human gate, stale baselines, automatic-cycle stop-at-ready, concurrent evidence deduplication, and rollback |
| `tests/config_bootstrap.rs` | Loop Engine defaults, explicit controls, environment placeholders, and template parsing |

All tests use local SQLite and deterministic fake providers/tools. Structural replay asserts
`live_tools_executed = 0`. The repository-level verification completed with formatting, strict
Clippy, all targets, and a release build.

Intentionally uncovered because the feature is not shipped: external-write confirmation,
pause/cancel HTTP operations, comparative live-provider replay, Desktop GUI behavior,
vendor-specific community connectors, and automated retention/pruning.

Known delivered-path test gaps: the SSE streaming handler and disabled-mode `404` guard exist but
do not yet have dedicated integration assertions.

## Implementation Record

1. Added schema version 1, shared Harness IDs, explicit budgets, and cooperative worker reload.
2. Added domain validation, immutable approval binding, DAG/work/attempt/checkpoint persistence,
   lease heartbeat/fencing, and committed-outcome reconciliation.
3. Added Artifact and multi-source Signal registries with bounded payloads and scoped ingestion.
4. Added registered safe handlers and `LoopEngine` claim/dispatch/resume behavior.
5. Added per-provider-completion frames and structural replay; comparative replay remains deferred.
6. Added read-only Self-test persistence and deduplicated failure-to-Signal maintenance.
7. Added HTTP/SSE and Telegram `/goal`/`/resume` control surfaces.
8. Connected bounded `evolution_evaluate` to the existing Prompt Evolution engine without creating
   an alternate approval/activation path.
9. Added configuration templates, operator runbook, focused tests, and full repository gates.

## Implementation Dependency Record

The implementation used these dependency boundaries after the core domain stabilized:

| Step | Modules | Depends on |
|---|---|---|
| Core domain/store | `harness/loop_engine/domain.rs`, `store.rs`, SQLite schema | — |
| Signal ingestion/proposal | `harness/loop_engine/signals.rs` | Core domain/store |
| Replay | trajectory frame capture, `harness/loop_engine/replay.rs` | Core domain/store |
| Self-test | `harness/loop_engine/self_test.rs` | Core domain/store, Replay |
| Artifact registry | `harness/loop_engine/artifacts.rs`, evolution adapter | Core domain/store |
| Control plane | HTTP/Telegram | Engine, Signal, Replay, Self-test |

Signal, Replay, and Artifact Registry were separable after the core; Self-test followed storage
and Replay, and the Control Plane followed the engines. They share schema initialization and module
exports, so the implementation was integrated sequentially in one workspace.

## Implementation Tasks

- [x] **T1 (P1)** — Loop core — Build Goal, WorkItem, Attempt,
  Checkpoint, lease, fencing, events, and recovery.
- [x] **T2 (P1)** — Signals — Add append-only multi-source
  ingestion, provenance, trust, dedup, and operator-created proposed Goals.
- [x] **T3 (P1)** — Planner/verifier — Bind immutable workflow/effects/budgets to approval and
  close the Goal achievement loop.
- [x] **T4 (P1)** — Replay — Capture per-completion frames and provide structural replay without
  live tools. Comparative provider replay remains an explicit future mode, not an exactness claim.
- [x] **T5 (P1)** — Self-test — Persist bounded read-only suites/runs and emit
  deduplicated failure signals.
- [x] **T6 (P2)** — Artifact registry — Add immutable typed/versioned records
  while preserving Prompt activation safeguards.
- [x] **T7 (P1)** — Control plane — Add HTTP/SSE and Telegram goal/recovery
  flows with typed errors, scoped ingestion, operator gates, and event cursors.
- [x] **T8 (P2)** — Operations — Add cooperative workers, persisted provider/deadline/response
  budgets, schema version coverage, configuration, periodic maintenance, and runbook.

## GSTACK REVIEW REPORT

| Review | Trigger | Why | Runs | Status | Findings |
|--------|---------|-----|------|--------|----------|
| CEO Review | `/plan-ceo-review` | Scope & strategy | 0 | — | Not run |
| Codex Review | outside voice | Independent second opinion | 1 | ABSORBED | 18 gaps incorporated into the plan |
| Eng Review | `/plan-eng-review` | Architecture & tests (required) | 1 | CLEAR | 48 issues/test gaps, 0 critical gaps open |
| Design Review | `/plan-design-review` | UI/UX gaps | 0 | — | Desktop UI is not in scope |
| DX Review | `/plan-devex-review` | Developer experience gaps | 0 | — | Not run |

- **CODEX:** Added planner/verifier closure, immutable approval hashes, provider-call frames,
  scoped ingestion, isolated Self-tests, cooperative worker reload, and complete recovery APIs.
- **CROSS-MODEL:** Both reviews agree on a dedicated durable loop, append-only signals,
  at-least-once recovery, typed drivers, and no live tools during replay.
- **VERDICT:** ENG CLEARED — the scoped Phase 2 core was then implemented and verified.

NO UNRESOLVED DECISIONS
