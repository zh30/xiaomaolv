# Loop Engineering Harness

xiaomaolv includes a durable, safety-gated engineering loop alongside the existing message and
prompt-evolution runtimes. It turns operator goals and multi-source feedback into reviewable
workflows, persists every attempt/checkpoint, can recover after process restarts, runs read-only
self-maintenance, and exposes a stable HTTP/SSE model for a future Desktop client.

## Enable it

The control plane is disabled by default. Configure a non-empty `app.api_key` for operator
routes and a separate ingestion key for untrusted feedback sources:

```toml
[app]
api_key = "${XIAOMAOLV_APP_API_KEY}"

[agent.harness]
enable_trajectory = true

[agent.harness.loop_engine]
enabled = true
ingest_api_key = "${XIAOMAOLV_HARNESS_INGEST_API_KEY}"
worker_enabled = true
worker_poll_interval_secs = 2
worker_lease_secs = 30
worker_max_parallel = 2
self_test_interval_secs = 3600
```

`worker_enabled = false` leaves planning, approval, recovery, self-test, replay, and all read APIs
available while preventing background claims. `self_test_interval_secs = 0` disables periodic
maintenance; self-tests remain manually callable.

The built-in worker accepts only registered `pure`, `read`, and `local_write` handlers. Arbitrary
code changes, deployments, credential changes, and unknown external writes are not enabled.
When `[agent.harness.evolution].enabled = true`, the `evolution_evaluate` handler adapts the
existing prompt-candidate engine: it reserves exactly two provider calls per enabled eval case,
enforces the Goal deadline and cumulative response-byte budget, and publishes only a compact
evaluation reference. Human approval/activation remains in the existing evolution control plane.

## Durable loop

The persisted hierarchy is:

```text
Goal -> immutable Workflow revision -> WorkItem DAG -> Attempt -> Checkpoint
```

Creating a goal and planning it does not execute work. The returned `plan_hash` binds the exact
workflow, effect manifest, acceptance criteria, and provider/response/deadline budget. An operator
must approve that same goal revision and hash before it becomes dispatchable.

Execution is at-least-once. Each claim has a lease token and monotonically increasing fencing
token. A handler writes a `prepared` checkpoint before work and a `committed` outcome afterward.
`/resume` reconciles committed outcomes without replaying the effect, expires stale leases, and
unlocks dependency-ready work. A stale worker cannot heartbeat, checkpoint, finish, or fail an
attempt after another claim has taken ownership.

Provider calls occur outside SQLite transactions. Approved workflows persist maximum provider
calls, a wall-clock deadline, and maximum response bytes. The worker reserves these budgets before
calling a provider.

## Telegram operations

The commands are restricted to configured Telegram admins in private chat:

```text
/goal <objective>
/goal approve <goal-id> <revision> <plan-hash>
/resume <goal-id>
```

`/goal` creates a proposed goal and produces the safe recommended plan. The approval command must
repeat the reviewed revision and hash. `/resume` reports durable goal/work/attempt/checkpoint state
and performs safe reconciliation.

## HTTP and Desktop boundary

Operator routes use `Authorization: Bearer <app.api_key>`. `X-Harness-Actor` is a bounded audit
label, not an authentication mechanism.

| Method | Route | Purpose |
|---|---|---|
| `GET/POST` | `/v1/harness/goals` | List goals or create/auto-plan one |
| `GET` | `/v1/harness/goals/{id}` | Read durable goal state |
| `POST` | `/v1/harness/goals/{id}/plan` | Store a validated Dynamic Workflow revision |
| `POST` | `/v1/harness/goals/{id}/approve` | Approve the immutable revision/hash |
| `POST` | `/v1/harness/goals/{id}/resume` | Recover and return work/attempt/checkpoint state |
| `GET` | `/v1/harness/goals/{id}/events` | Cursor-based durable event snapshot |
| `GET` | `/v1/harness/goals/{id}/events/stream` | SSE event stream for Desktop |
| `POST` | `/v1/harness/goals/{id}/verify/manual` | Record a named manual criterion and re-verify |
| `GET` | `/v1/harness/signals` | List provenance-preserving evolution signals |
| `POST` | `/v1/harness/signals/{id}/propose-goal` | Convert a signal into a proposed goal |
| `POST` | `/v1/harness/self-tests/{suite}` | Run a bounded read-only suite (`core`) |
| `GET` | `/v1/harness/self-test-runs/{id}` | Read persisted maintenance evidence |
| `GET` | `/v1/harness/trajectories/{id}/frames` | Read per-provider-call replay frames |
| `POST` | `/v1/harness/trajectories/{id}/replay/structural` | Replay structure without live tools |
| `GET/POST` | `/v1/harness/artifacts` | List or publish immutable typed artifacts |
| `GET` | `/v1/harness/artifacts/{id}` | Read a typed artifact |

Signal ingestion is the only route family that uses the scoped ingestion key:

```bash
curl -sS -X POST "$XIAOMAOLV_URL/v1/harness/signals" \
  -H "Authorization: Bearer $XIAOMAOLV_HARNESS_INGEST_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "kind":"community",
    "trust":"external",
    "source":"github:discussion",
    "external_id":"discussion-42",
    "content":"Make recovery state easier to inspect",
    "metadata":{"repository":"xiaomaolv"}
  }'
```

Scoped clients cannot assert `internal` trust or create, approve, dispatch, resume, verify, or
activate anything. Source/external-ID/content fingerprints make ingestion idempotent. Community,
user, developer, trajectory, replay, manual, and self-test signals all use the same immutable
model; they can only become proposed goals until an operator reviews the resulting plan.

The HTTP resources plus monotonic goal-event cursor are the supported Desktop contract. Desktop
business logic does not live in the server core.

## Self-test and Session Replay

The production `core` suite is read-only. It checks schema version/tables, work references,
checkpoint outcomes, signal provenance, and approval-to-plan bindings. Runs and individual cases
are immutable. Repeated identical failure sets emit one deduplicated internal EvolutionSignal;
they never approve work or change an active prompt policy.

When trajectory logging is enabled, every main plain, Code Mode, and MCP provider completion is
stored as a bounded frame containing the exact request messages at that call, JSON mode flag,
model/config fingerprint, response, and hashes. Structural replay validates frame continuity and
integrity using recorded data and executes zero live MCP tools. A frame is exact-replayable only
when it is a full capture and the provider fingerprint can be reproduced; normal runtime captures
are conservatively marked truncated.

## Artifact and prompt safety

Artifacts are immutable and typed (`dynamic_workflow`, `analysis_report`, `self_test_report`,
`replay_corpus`, `desktop_view`, and related kinds). A `prompt_policy_ref` may store only existing
evolution candidate/deployment IDs; it cannot duplicate prompt text. The existing prompt
candidate approval, activation, stale-baseline check, and rollback implementation remains the sole
source of truth for active prompt policy.

## Verify locally

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --release
```

Focused harness coverage:

```bash
cargo test --test harness_loop_engine -- --nocapture
cargo test --test http_loop_engine_api -- --nocapture
cargo test --test service_harness_trajectory -- --nocapture
```
