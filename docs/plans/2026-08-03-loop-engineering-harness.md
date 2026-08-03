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

## What Already Exists

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

1. External and model-derived signals can create only `proposed` goals.
2. `/goal` starts safe planning only. Effectful dispatch requires an operator approval bound to
   an immutable goal revision, workflow hash, effect manifest, and execution budget.
3. Work execution is at-least-once. No API claims universal exactly-once behavior.
4. Every attempt owns a lease token and monotonically increasing fencing version.
5. Provider and tool calls never occur inside a SQLite transaction.
6. Pure/read work may retry automatically. Unknown external writes require confirmation.
7. Checkpoints are immutable. Resume creates a new attempt; it never rewrites history.
8. Replay uses captured prompts and recorded tool results; it never calls live tools.
9. Self-test failures emit signals. They cannot approve or activate an artifact.
10. Only registered, schema-versioned artifact drivers may validate, test, activate, or roll
    back a capability.
11. Existing prompt candidate approval, stale-baseline protection, and rollback remain intact.
12. Payloads, provider-call counts, response bytes, batches, retries, concurrency, and retention
    are bounded. Token/currency budgets are not claimed until providers expose trusted usage.
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
                    bounded triage
                           |
                           v
                    proposed HarnessGoal
                           |
                safe GoalPlanner call
                           |
                   review-ready revision
           workflow hash + effects + call budget
                           |
              operator approve / revise / reject
                           |
                           v
              versioned DynamicWorkflowSpec
                           |
                compile DAG into WorkItems
                           |
          +----------------+----------------+
          |                                 |
     lease + Attempt                   manual gate
          |
    prepared checkpoint
          |
    registered WorkHandler
          |
    committed checkpoint
          |
       self-test / shadow replay
          |
          v
    typed CapabilityArtifact candidate
          |
   existing human promotion + rollback
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
  ├── MaintenanceWorker schedule bounded self-test suites
  └── EvolutionWorker   existing propose/evaluate-only cycle
```

Configuration reload is cooperative: signal the old generation, stop new claims, await bounded
handler cancellation/lease relinquish, and only then start replacement workers. If shutdown times
out, leases expire naturally and the new generation still relies on fencing; it never assumes an
aborted task did not produce a side effect.

## Durable Domain Model

### Goal

`HarnessGoal` is the durable desired outcome, not an execution attempt.

```text
id / revision
title / objective / acceptance_criteria[]
target_capability
priority
source_signal_ids[]
created_by / approved_by?
status
created_at / updated_at
```

Goal state machine:

```text
proposed -> planning -> review_ready -> approved -> active -> verifying -> achieved
    |          |           |             |          |          |
 rejected    failed      revised       canceled   paused     active
                          rejected                 blocked    failed
                                                   failed

paused / blocked / failed --resume--> active
achieved / rejected / canceled are terminal
```

Acceptance criteria are typed: `output_assertion`, `self_test_suite`, `artifact_ready`, or
`manual`. `GoalVerifier` evaluates deterministic criteria and moves the goal through
`verifying`; a manual criterion requires an explicit verify endpoint. A passing verifier may
mark a goal `achieved`, while a failing verifier returns it to `active` or `failed` according to
the approved retry policy.

An operator `/goal <objective>` creates a `proposed` goal and starts only the pure planning step.
A goal derived from signals also starts as `proposed`. Approval occurs after the workflow,
effects, acceptance criteria, and budget are visible. Replanning increments the goal revision
and invalidates prior approval.

### WorkItem

`HarnessWorkItem` is one node in a persisted DAG.

```text
id / goal_id / workflow_revision
kind / payload_json / effect_class
depends_on[] / priority / available_at
status / max_attempts
lease_token? / lease_until? / fencing_version
created_at / updated_at
```

```text
queued -> leased -> running -> succeeded
   |        |          |        |
 canceled  expired     failed   skipped
                       |   |
                       |   +-> queued (retry policy)
                       +-----> waiting_confirmation
```

Dependencies must exist in the same goal and the graph must be acyclic. A work item becomes
claimable only when every dependency succeeded or was explicitly skipped by the approved
workflow policy. Dependency failure/cancellation propagates to downstream work as blocked or
canceled. Exhausted attempts fail the WorkItem and derive the Goal state. Pausing prevents new
claims and asks running handlers to cancel cooperatively; fencing still decides the authoritative
result.

### Attempt and Checkpoint

`HarnessAttempt` represents one worker claim. `HarnessCheckpoint` is immutable evidence about a
safe boundary inside that attempt.

```text
Attempt: id / work_item_id / number / lease_token / fencing_version
         status / started_at / finished_at / error_class?

Checkpoint: id / attempt_id / sequence / phase
            idempotency_key? / input_digest / output_digest?
            bounded_state_json / created_at
```

Checkpoint phases share an operation ID and persist a bounded `WorkOutcome`:

```text
prepared -> committed -> reconciled -> work succeeded
    |           |
    |           +-- crash before work success -> recover committed outcome, do not execute again
    |
process crash
    |
pure/read/idempotent write -> automatic retry
external write, outcome unknown -> waiting_confirmation -> confirm | retry | skip | fail
```

`/resume <goal-id>` performs a transaction that validates the goal state, expires stale leases,
requeues safe work, leaves uncertain external writes gated, advances the goal revision, and
records an event. A subsequent worker claim creates a new Attempt.

## Execution Contract

Every registered `WorkHandler` declares:

```text
kind
schema_version
effect: pure | read | local_write | external_write
validate(payload)
idempotency_key(payload, goal, work_item)?
execute(context, payload) -> WorkOutcome
```

Rules:

- `pure` and `read` handlers are automatically retryable.
- `local_write` handlers must use the provided idempotency key or commit in the same SQLite
  transaction as their durable result.
- `external_write` handlers must provide an idempotency key and optional status lookup. Without
  lookup support, a crash after `prepared` becomes `waiting_confirmation`.
- A stale fencing version cannot heartbeat, checkpoint, complete, or fail an attempt.
- Unknown handler kinds and unsupported schema versions are rejected before dispatch.
- A lease check immediately precedes handler execution and handlers receive a cancellation token,
  but fencing cannot revoke an external request already accepted by another service. Therefore
  Phase 2 registers no production `external_write` handler; the contract and crash behavior are
  tested with fakes until a connector supplies enforced idempotency/status lookup.

Initial production handlers are useful but non-effectful: `goal_planner`, `provider_analysis`,
`session_replay`, `self_test_suite`, `evolution_evaluate`, and `manual_gate`.

## Multi-source Evolution

### EvolutionSignal

All source adapters normalize into one append-only record:

```text
id
source_kind: trajectory | user_feedback | developer_feedback |
             community | self_test | session_replay | manual
external_id? / source_uri?
trust: internal | authenticated | external
title / bounded_content / metadata_json
content_sha256 / dedup_fingerprint
status: pending | triaged | accepted | rejected
observed_at / created_at
```

The unique dedup key is `(source_kind, dedup_fingerprint)`. Source adapters may store only
bounded content and allowlisted metadata. Secrets, authorization headers, and raw credentials
are rejected/redacted before persistence.

External content is always data, never a system instruction. Triage receives it inside a quoted
JSON envelope and produces strict, bounded JSON:

```json
{
  "title": "...",
  "objective": "...",
  "acceptance_criteria": ["..."],
  "target_capability": "dynamic_workflow",
  "priority": 50
}
```

Triage can create a `proposed` goal only. The signal-to-goal links remain queryable so a future
operator can trace every claim back to its source.

Existing negative trajectory feedback is adapted into `EvolutionSignal`; the existing feedback
endpoint remains compatible.

## Dynamic Workflow and Capability Artifacts

### Workflow

`GoalPlanner` converts a goal revision into strict `DynamicWorkflowSpec` JSON using only the
currently registered handler catalog. Invalid output fails planning without partial WorkItems.
The resulting workflow, effect manifest, acceptance criteria, and call-count/response-byte budget
are hashed together. Operator approval binds that hash; any change requires reapproval.

`DynamicWorkflowSpec` is a versioned, declarative DAG. Phase 2 validates, stores, and compiles
registered step kinds; it does not permit arbitrary shell or code payloads.

```text
version / name / description
steps[]:
  id / handler_kind / handler_schema_version
  payload / depends_on[] / retry_policy
```

Limits: 32 steps, 64 dependency edges, 32 KiB serialized manifest, acyclic graph, unique step
IDs, registered handlers only.

### Artifact registry

`CapabilityArtifact` separates a candidate's generic lifecycle from a driver-specific manifest:

```text
id / kind / schema_version / manifest_json / content_sha256
source_goal_id / status / created_at
```

`ArtifactDriver` declares `validate`, `self_test`, `activate`, and `rollback`. Registered kinds in
Phase 2:

- `prompt_policy`: reference-only adapter over the existing prompt candidate/deployment engine;
  it never duplicates prompt state, deployment history, or the active pointer.
- `dynamic_workflow`: validates and stores the workflow manifest; activation requires human
  approval.
- `session_replay_suite`: immutable replay suite manifest.
- `desktop_contract`: schema-only future integration contract; no GUI activation.

An unknown kind or version cannot be activated. Background workers may produce and test
artifacts but may not approve or activate them.

Generic artifacts use immutable approval records plus per-kind deployment history and active
pointers. The `prompt_policy` reference adapter delegates those operations to the existing
evolution tables instead of writing generic deployment state, avoiding two active pointers.

## Session Replay

Current trajectories gain bounded per-completion `TrajectoryFrame` records rather than a single
snapshot. One request can contain several provider/tool iterations, so every frame records:

```text
schema_version / sequence / stage / iteration
incoming message and bounded provider messages at that exact call
provider/model and completion option fingerprint
active policy/artifact IDs
tool catalog digest / selected skills
request digest / bounded response / response digest
recorded tool proposal/result references
replayability: exact | comparative | unavailable
```

Secrets and credentials are never included. Material redaction/truncation marks the frame
`comparative` or `unavailable`; it is never called an exact capture. Historical trajectories
without frames are reported as `unavailable`, not silently approximated. Exact replay caching
requires the full request, provider configuration, model revision/seed when available, completion
options, artifacts, and frame digest to match.

Replay modes:

1. `structural` — validate trace ordering, tool envelopes, exit reason, hashes, and assertions
   without a provider call.
2. `shadow` — call the provider for each captured frame, substitute recorded tool results, and
   evaluate assertions. Provider output is comparative/non-deterministic unless an exact seeded
   model revision is available. No live MCP/channel/memory writes.

Each replay run stores the snapshot digest, artifact IDs, provider/model, result, bounded output,
full output hash, and case-level issues. A replay failure emits a deduplicated `self_test` or
`session_replay` signal.

## Self-testing and Maintenance

`SelfTestRegistry` holds versioned built-in suites. Mutation and crash suites always receive a
fresh ephemeral SQLite database plus fake providers/handlers. Production maintenance registers
read-only suites only. Phase 2 suites:

- `loop_integrity`: invalid state transitions, orphan dependencies, expired leases, stale
  fencing, and checkpoint sequence consistency.
- `replay_integrity`: snapshot decoding, structural replay, shadow isolation, and assertion
  scoring.
- `evolution_integrity`: existing prompt baseline/candidate gates and active-policy consistency.

`SelfTestRunner` persists one immutable run and case results. It executes with:

- concurrency 1 by default;
- bounded cases and provider calls per run;
- a total runtime deadline;
- dedup key `(suite_version, subject_digest)`;
- no production activation permissions.

Failed cases emit one deduplicated signal. Passing the suite does not mutate a goal or artifact.
The maintenance timer is disabled by default and can also be triggered through the operator
control plane.

## Control Plane

Operator endpoints use the existing app API key. The caller-supplied actor header remains an audit
label, not identity proof; server-side actor identity is `operator:app-key`, while Telegram uses
the verified admin user ID. Signal ingestion uses a separate scoped `harness_ingest_api_key` that
cannot approve, dispatch, resume, verify, activate, or roll back anything.

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

Foreign keys preserve provenance. State mutations and their corresponding append-only event are
written in one transaction. Provider/tool calls happen after claim commit and before completion
commit.

Important indexes:

- signals: `(kind, created_at, id)` and unique source fingerprint/external ID;
- goals: `(status, updated_at, id)`;
- work: `(status, next_attempt_at, lease_until, created_at)`;
- attempts: `(work_item_id, attempt_number)` unique;
- checkpoints: `(attempt_id, sequence)` unique;
- events: `(goal_id, created_at, id)`;
- replay/self-test: subject/suite digest and creation time.

## Production Failure Modes

| Failure | Handling | Test | User-visible result |
|---|---|---|---|
| Worker dies before executing | Lease expires; safe work is claimable | Store integration | Goal remains resumable |
| Worker dies after external write | `prepared` remains without `committed` | Crash-window integration | `waiting_confirmation`, never silent replay |
| Worker dies after committed checkpoint | Reconcile stored outcome, never call handler again | Crash-window integration | Work finishes from durable result |
| Stale worker returns after lease takeover | Fencing CAS rejects checkpoint/completion | Concurrency integration | New attempt remains authoritative |
| Duplicate community webhook | Unique source fingerprint returns existing signal | Store integration | Idempotent response |
| Prompt injection in community text | Content stays JSON data; strict triage schema | Engine/eval | Proposed goal only |
| Invalid/cyclic workflow | Validation fails before work insertion | Domain/store unit | Actionable 422 response |
| Replay snapshot missing | Run marked `unavailable` | Replay unit/API | Explicit reason |
| Replay tries a live tool | Replay runtime has no live MCP capability | Integration | Test fails closed |
| Provider timeout in triage/replay | Attempt fails with retry policy and bounded error | Engine integration | Goal status and retry visible |
| Self-test repeatedly fails | Digest dedup emits one signal per subject revision | Maintenance integration | No signal storm |
| SQLite busy/locked | Short transactions, busy timeout, bounded retry | Store integration | Retryable failure, no partial state |
| Payload growth | Hard size limits plus payload retention/pruning | Boundary tests | 422 before persistence |
| Config reload overlaps worker generations | Cooperative stop/relinquish before replacement starts | Worker lifecycle integration | No double-claim window |

No listed failure is allowed to be both silent and untested.

## Performance and Retention

- Claim at most 8 work items per transaction; provider and handler calls are outside the
  transaction.
- Loop worker concurrency defaults to 2; maintenance/replay provider concurrency defaults to 1.
- Lease heartbeat is at most one third of lease duration.
- Signal content is capped at 16,384 characters, workflow/artifact manifests at 32 KiB,
  provider-frame requests at 128 KiB, responses at 256 KiB, and API list limits at 500.
- Replay and self-test results cache only by the complete immutable execution fingerprint.
- Provider budgets are enforced as persisted maximum call counts, deadlines, and response bytes.
  Token/currency accounting is informational only when usage metadata becomes available.
- Retention/pruning is deliberately not automated in this phase; immutable evidence remains until
  a future reference-aware retention job is reviewed and enabled.
- SQLite WAL and the existing five-second busy timeout remain. New write transactions must not
  span an await on a provider, MCP tool, channel, or filesystem operation.

## Test Coverage Plan

```text
CODE PATHS                                             USER FLOWS
[+] loop domain                                       [+] /goal create -> dispatch
  ├── [GAP][UNIT] every valid transition                ├── [GAP][E2E] authenticated direct goal
  ├── [GAP][UNIT] every invalid transition              ├── [GAP][E2E] external signal -> proposed
  ├── [GAP][UNIT] DAG validation/cycle                  └── [GAP][E2E] approve -> worker claim
  └── [GAP][UNIT] payload/size/version bounds

[+] SQLite store                                      [+] crash and recovery
  ├── [GAP][INTEGRATION] signal dedup/provenance         ├── [GAP][E2E] expired lease -> new attempt
  ├── [GAP][INTEGRATION] atomic state+event              ├── [GAP][E2E] stale worker rejected
  ├── [GAP][INTEGRATION] dependency-aware claim          └── [GAP][E2E] unknown external write gated
  ├── [GAP][INTEGRATION] lease heartbeat/fencing
  ├── [GAP][INTEGRATION] committed-outcome reconciliation
  └── [GAP][INTEGRATION] checkpoint sequence

[+] Signal triage                                     [+] operator recovery
  ├── [GAP][UNIT] validation/redaction/digest            ├── [GAP][E2E] pause -> resume
  ├── [GAP][INTEGRATION] strict JSON / provider error    ├── [GAP][E2E] confirm/skip external write
  └── [GAP][EVAL] injection content stays untrusted      └── [GAP][E2E] cancel terminal behavior

[+] Workflow/artifact registry                       [+] self-maintenance
  ├── [GAP][UNIT] unknown kind/version rejected          ├── [GAP][E2E] run suite -> persisted result
  ├── [GAP][UNIT] 32-step/64-edge/DAG limits             ├── [GAP][E2E] failure -> one signal
  └── [GAP][INTEGRATION] operator activation gate        └── [GAP][E2E] pass -> no mutation

[+] Session Replay                                   [+] HTTP/Telegram control
  ├── [GAP][UNIT] snapshot bounds/redaction              ├── [GAP][E2E] auth and actor gates
  ├── [GAP][UNIT] structural replay                      ├── [GAP][E2E] 404/409/422 mapping
  ├── [GAP][INTEGRATION] shadow provider                 ├── [GAP][E2E] duplicate submissions
  ├── [GAP][INTEGRATION] recorded tool substitution      └── [GAP][INTEGRATION] command parsing/access
  └── [GAP][E2E] live MCP cannot be reached

NEW-PATH COVERAGE BEFORE IMPLEMENTATION: 0 planned paths covered
TARGET: every enumerated state edge, crash window, concurrency race, and user recovery flow
```

Required test files:

- `tests/harness_loop_domain.rs`
- `tests/harness_loop_store.rs`
- `tests/harness_loop_engine.rs`
- `tests/harness_signal_triage.rs`
- `tests/harness_workflow.rs`
- `tests/harness_replay.rs`
- `tests/harness_self_test.rs`
- `tests/http_harness_control.rs`
- `tests/telegram_goal_commands.rs`

Prompt/LLM paths require deterministic fake-provider tests plus eval cases proving that external
Signal content cannot become system instructions and that replay never executes live tools.

## Implementation Sequence

1. Add schema migration boundaries, shared Harness IDs, typed errors, budgets, and cooperative
   worker shutdown semantics.
2. Add domain types, complete state transitions, workflow/effect validation, verifier criteria,
   and exhaustive unit tests.
3. Add dedicated LoopStore, SQLite schema, short transactions, indexes, lease/fencing,
   checkpoint reconciliation, and crash-window tests.
4. Add artifact/workflow lifecycle and reference-only prompt adapter before replay/self-test
   artifact types.
5. Add Signal ingestion/dedup, scoped credentials, strict triage, GoalPlanner, immutable approval,
   and tests.
6. Add LoopEngine claim/heartbeat/execute/checkpoint/resume behavior with fake handlers, then
   register the safe production handlers.
7. Add per-completion trajectory frames and structural/comparative shadow Replay.
8. Add isolated SelfTestRegistry/runner and failure-to-signal maintenance flow.
9. Add operator HTTP control plane, event cursor, Telegram `/goal`/`/resume`, and recovery gates.
10. Add bounded worker configuration, retention maintenance, metrics, docs, and migrations.
11. Run formatting, strict Clippy, all targets, release build, and crash-window smoke tests.

## Worktree Parallelization Strategy

The implementation has independent work after the core domain is stable:

| Step | Modules | Depends on |
|---|---|---|
| Core domain/store | `harness/loop_engine`, SQLite schema | — |
| Signal/triage | `harness/signals`, provider adapter | Core domain/store |
| Replay | trajectory snapshot, `harness/replay` | Core domain/store |
| Self-test | `harness/self_test` | Core domain/store, Replay |
| Artifact registry | `harness/artifacts`, evolution adapter | Core domain/store |
| Control plane | HTTP/Telegram | Engine, Signal, Replay, Self-test |

Potential lanes after the core lands: Signal + Replay + Artifact Registry. Self-test follows
Replay; Control Plane follows all engines. These lanes share `harness/mod.rs` and schema startup,
so merges need coordination. This task will be implemented sequentially in the current workspace.

## Implementation Tasks

- [x] **T1 (P1)** — Loop core — Build Goal, WorkItem, Attempt,
  Checkpoint, lease, fencing, events, and recovery.
- [x] **T2 (P1)** — Signals — Add append-only multi-source
  ingestion, provenance, trust, dedup, and proposed-goal triage.
- [x] **T3 (P1)** — Planner/verifier — Bind immutable workflow/effects/budgets to approval and
  close the Goal achievement loop.
- [x] **T4 (P1)** — Replay — Capture per-completion frames and provide structural replay without
  live tools. Comparative provider replay remains an explicit future mode, not an exactness claim.
- [x] **T5 (P1)** — Self-test — Persist bounded read-only suites/runs and emit
  deduplicated failure signals.
- [x] **T6 (P2)** — Artifact registry — Add typed, versioned drivers
  while preserving prompt activation safeguards.
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
- **VERDICT:** ENG CLEARED — ready to implement the scoped Phase 2 core.

NO UNRESOLVED DECISIONS
