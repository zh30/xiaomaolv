# Loop Engineering Harness

**Status:** implemented and verified (schema version 1)

**Runtime contract:** this document and the code/tests it links; `docs/plans/` is historical context

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

Startup validation enforces:

| Setting | Valid values |
|---|---|
| `worker_poll_interval_secs` | `1..=60` |
| `worker_lease_secs` | `1..=3600` |
| `worker_max_parallel` | `1..=16` |
| `self_test_interval_secs` | `0`, or `10..=2592000` |
| `external_write_enabled` | `false` default; enables allowlisted `external_write` steps |
| `external_write_handlers` | list of handler names; currently only `channel_send` exists |

`worker_enabled = true` requires `enabled = true`. Periodic maintenance is independent of work
claiming: when the Loop Engine is enabled, a positive self-test interval runs even if the worker
is disabled. Set `enable_trajectory = true` to capture provider frames from normal message paths.

The built-in worker accepts only registered `pure`, `read`, and `local_write` handlers by
default. Arbitrary code changes, deployments, credential changes, and unknown external writes
are not enabled. `external_write_enabled = true` opens a narrow gate: only handlers named in
`external_write_handlers` may plan, approve, register, or dispatch `external_write` steps —
checked again at approve and dispatch time so disabling the flag between plan and run fails
closed. Delivery is at-least-once: a crash between send and commit parks the work item in
`waiting_confirmation` for operator review instead of resending. The operator then resolves the
park through `POST .../resolve-confirmation` with a required audit reason: `confirmed` attests
the external effect happened and commits the prepared checkpoint with an operator-attested
outcome (item `succeeded`); `retry` attests it did not, voids the checkpoint, and re-queues the
item (or fails it when the attempt budget is exhausted); `abandoned` voids the checkpoint and
fails the item. Resolutions are single-use and goal-scoped.
When `[agent.harness.evolution].enabled = true`, the `evolution_evaluate` handler adapts the
existing prompt-candidate engine: it reserves exactly two provider calls per enabled eval case,
enforces the Goal deadline and cumulative response-byte budget, and publishes only a compact
evaluation reference. Human approval/activation remains in the existing evolution control plane.

Shadow evaluation always scores two populations together: operator-managed eval cases and the
versioned benchmark suite (`src/harness/benchmark.rs`, labeled `id@version`, currently
`core@2026-09-24.1`). Benchmark cases carry the same assertion vocabulary — required/forbidden
substrings, JSON validity, and `max_output_chars` — and are marked `benchmark` in the scorecard
with the suite label recorded alongside. A benchmark regression (baseline pass -> candidate
fail on a benchmark case) is always fatal to promotion: `benchmark_regressions > 0` rejects the
candidate regardless of `max_regressions`, which bounds only operator-case regressions. An
operator eval case whose id collides with a benchmark case fails the evaluation closed.

Every finished trajectory also produces a persisted, deterministic `RunScorecard`
(`mcp_trajectory_scorecards`): a composite `0..=100` fitness score built from exit-reason
completion weight, answer presence, tool health, wall-clock band, failed-tool penalties, and
verification-failure penalties — each component stored alongside for audit. Scorecards are the
fitness function of the self-improvement loop: `propose_from_trajectories` attaches each
failure's score to the proposal evidence, and each cycle runs a **deployment outcome review**
before proposing — it compares the mean run score of the 32 runs before the active
deployment's `activated_at` against the first 8..64 runs after it. A drop of 12+ points writes
a `deployment_regressed` audit event and emits one deduplicated `deployment_outcome` signal
into the operator triage pipeline (`evolution:deployment-review`, external id
`deployment:{id}:regression`). The rollback decision itself stays human-gated. Scorecards are
readable via `GET /v1/harness/scorecards` (filters: channel/model/hours/min/max score) and
`GET /v1/harness/scorecards/summary` (hour-windowed bands and failure totals); each trajectory
detail response embeds its scorecard. Runs that predate the table are backfilled lazily on
read and in bounded batches inside summaries. A run scoring below 40 also self-reports at
finish time as a deduplicated `trajectory` signal (`trajectory:scorecard`), so chat-quality
failures enter the same operator triage pipeline as self-test regressions — identical failure
shapes collapse into one signal via near-duplicate dedup. Every proposal additionally carries
the loop's failed-intervention memory (`prior_interventions`: rejected/failed candidates and
rolled-back deployments), so evidence fingerprints change when an intervention is buried and
the model is instructed not to resurface it.

Real users feed the same loop without operator ceremony: when a Telegram message is a reply
to a bot answer and its entire body is an unambiguous marker (`👍`/`❤`/`+1`/`好` for +0.5,
`👎`/`-1`/`差` for -0.5), the channel records it as trajectory feedback bound to the most
recent finished run under that chat's session prefix (`tg:{chat_id}%`, max 2h old) instead of
invoking the model. Free-form text, multi-emoji bodies, replies to non-bot messages, and
chats with no recent run all fall through to normal processing. The feature activates only
when evolution is enabled and trajectory logging records runs. Feedback aggregates too:
three or more distinct runs flagged negative inside six hours emits one deduplicated
hourly-bucket `user_feedback` signal (`evolution:feedback-spike`), so sustained user
displeasure surfaces as triage work while a lone complaint stays evidence-only.

The eval suite itself grows from production: `POST /v1/harness/evolution/eval-cases/
from-trajectory` turns a recorded run into a **disabled** eval case — the operator supplies
a name and at least one assertion; the input defaults to the run's first user message
extracted from its earliest captured provider request frame (or an explicit override when
no frame was captured). Seeding is human-actor-gated and audited (`eval_case_seeded`); the
case only enters shadow evaluation after a second deliberate enable via the eval-cases
endpoint.

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

Each handler declares crash semantics with `WorkHandler::replay_safe()`. When a stale lease
expires with a `prepared`-but-uncommitted checkpoint, a `replay_safe` handler's work item simply
re-queues for a fresh attempt (and the superseded prepared checkpoint is voided); a handler that
declares `replay_safe = false` parks in `waiting_confirmation` for the same operator verdict
path as `external_write` ambiguity, because re-running might double-apply an effect that may
already have landed. `channel_send` declares `replay_safe = false`; built-in idempotent
handlers declare `true`.

A work item whose handler is not registered in the claiming process becomes `blocked` with a
missing-handler reason instead of failing permanently — rolling deploys and late registration
no longer kill in-flight goals. On each poll the worker re-queues such items whose handler now
exists in its registry. Dependency-failure cascades also use `blocked` but carry a different
reason and are never auto-requeued.

A handler may also return a `sleep_until` outcome: the attempt defers without committing an
outcome and the work item becomes claimable again at the stored unix timestamp. The deferral
persists in the database, holds no lease, and survives process restarts — this is how the `wait`
handler implements a durable timer.

Work items can also span **multiple committed phases**. A handler returns
`Outcome.continuation = {phase, state, delay_secs}` instead of finishing: the worker commits
the boundary to `harness_work_items.progress_json` *before* the next phase runs, then invokes
the handler again inside the same attempt (same lease, same deadline budget). A crash between
boundaries abandons the attempt — the next claim injects the persisted progress via
`WorkHandlerContext.resume` and the handler continues from the last committed phase rather
than restarting. With `delay_secs` set, the attempt yields instead: the item becomes
claimable again after the delay with progress carried forward (durable sleep + progress).
Committed boundaries per item are capped at `MAX_PHASE_CONTINUES` (64) so a handler that
never produces a terminal outcome is bounded, and a successful finish clears the progress.

The `evolution_evaluate` handler is the reference consumer: each phase advances a durable
`evolution_eval_runs` cursor by exactly one eval case (baseline + candidate pair). The run
row freezes the suite snapshot and baseline policy at first contact, commits outputs per
case, and is cleared when the last case finalizes the evaluation. A crashed attempt — or a
fresh worker process — resumes at `case_index` without redoing provider calls, and each
phase reserves only the two calls it actually spends. The continuation state carries only
the cursor metadata; the outputs themselves live in the store, not the token budget.

Goal admission is idempotent on an optional `request_id` (1..=160 bytes, unique per
`harness_goals`): retrying a create with the same key returns the already-admitted goal without
writing a second `goal.created` event or re-planning. The signal-propose path uses
`signal:{signal_id}` and swarm projection uses `swarm:{run_id}` so crash/retry windows cannot
duplicate goals.

Provider calls occur outside SQLite transactions. Approved workflows persist maximum provider
calls, a wall-clock deadline, and maximum response bytes. The worker reserves these budgets before
calling a provider.

Goal states are `proposed`, `planning`, `review_ready`, `approved`, `active`, `verifying`,
`achieved`, `rejected`, `canceled`, `paused`, `blocked`, `failed`, and `replan`. Work and attempt
states are stored separately so an interrupted attempt never erases the durable Goal state.

Dynamic Workflow validation is intentionally small and predictable:

- `1..=32` steps, at most `64` edges, and at most `32768` serialized bytes.
- The graph must be acyclic; step IDs and edges are unique and all references must resolve.
- Each step input is at most `8192` bytes, has `1..=10` attempts, and at most `3600` seconds of
  retry backoff.
- A plan has `1..=16` acceptance criteria.
- Provider-call budget is at most `64`; deadline is `1..=86400` seconds; response budget is
  `1..=10485760` bytes.

Registered handlers:

| Handler | Current behavior |
|---|---|
| `goal_planner` | Publishes the reviewed Dynamic Workflow artifact |
| `provider_analysis` | Runs one bounded provider analysis and publishes `analysis_report` |
| `self_test_suite` | Runs the read-only `core` suite and publishes `self_test_report` |
| `session_replay` | Runs structural replay and publishes `replay_corpus` |
| `manual_gate` | Produces evidence that a manual gate was reached |
| `evolution_evaluate` | Bounded adapter to an existing prompt candidate evaluation |
| `channel_send` | Sends `{channel, session_id, text}` through the configured outbound channel; `external_write`, registered only when the feature flag is on and the handler is allowlisted |
| `wait` | Durable timer step; defers until `input.until_unix` (absolute) or `input.delay_secs` after item creation, then completes on the next claim |

`evolution_evaluate` is registered as unavailable unless
`[agent.harness.evolution].enabled = true`. When enabled, it reserves exactly two provider calls
per enabled eval case, enforces the
Goal deadline and cumulative response-byte budget, and publishes `evolution_evaluation`; it does
not approve or activate the candidate.

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

The pre-existing `GET /v1/harness/trajectories` and
`GET /v1/harness/trajectories/{id}` endpoints expose trajectory list/detail records and follow the
normal optional `app.api_key` behavior. The frame/replay endpoints below are Loop Engine routes:
they require a configured operator key and `loop_engine.enabled = true`.

| Method | Route | Purpose |
|---|---|---|
| `GET/POST` | `/v1/harness/goals` | List goals or create/auto-plan one; optional `request_id` makes admission idempotent |
| `GET` | `/v1/harness/goals/{id}` | Read durable goal state |
| `POST` | `/v1/harness/goals/{id}/plan` | Store a validated Dynamic Workflow revision |
| `POST` | `/v1/harness/goals/{id}/approve` | Approve the immutable revision/hash |
| `POST` | `/v1/harness/goals/{id}/resume` | Recover and return work/attempt/checkpoint state |
| `GET` | `/v1/harness/goals/{id}/work-items` | List durable work items for a goal |
| `POST` | `/v1/harness/goals/{goal_id}/work-items/{work_item_id}/resolve-confirmation` | Operator verdict for a `waiting_confirmation` item |
| `GET` | `/v1/harness/goals/{id}/events` | Cursor-based durable event snapshot |
| `GET` | `/v1/harness/goals/{id}/events/stream` | SSE event stream for Desktop; commit-driven push with monotonic `sequence` cursor (5 s fallback poll) |
| `POST` | `/v1/harness/goals/{id}/verify/manual` | Record a named manual criterion and re-verify |
| `GET` | `/v1/harness/signals` | List signals; `?status=observed|triaged|proposed|ignored` filters by triage state |
| `POST` | `/v1/harness/signals` | Ingest one external/authenticated signal with the scoped key |
| `GET` | `/v1/harness/signals/{id}` | Read one signal and its current triage status |
| `POST` | `/v1/harness/signals/{id}/propose-goal` | Convert a pending signal into a proposed goal |
| `POST` | `/v1/harness/signals/{id}/ignore` | Mark a pending signal ignored with an operator reason |
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
activate anything. Ingestion is idempotent across three dedup layers, all scoped to the signal
source: exact `(source, external_id)` or `(source, fingerprint)` match, a normalized-content
hash that collapses case/whitespace/punctuation variants per kind, and token-set near-duplicate
detection (Jaccard >= 0.9 against the 128 most recent same-source/kind signals). Community,
user, developer, trajectory, replay, manual, and self-test signals all use the same immutable
model.

Signal triage is a small lifecycle: `observed` (or `triaged`) -> `proposed` via the
propose-goal route, or -> `ignored` with a required operator reason. `proposed` and `ignored`
are terminal for both transitions — an ignored signal cannot be proposed, and a proposed
signal cannot be proposed again or ignored. Over Telegram, `/signals` lists pending signals and
`/signal <id> [ignore <reason> | goal <objective>]` reviews, rejects, or converts one.

Supported signal kinds are `trajectory`, `user_feedback`, `developer_feedback`, `community`,
`self_test`, `session_replay`, and `manual`; trust levels are `internal`, `authenticated`, and
`external`. The scoped endpoint rejects `internal`. Source is limited to 160 bytes, external ID to
200 bytes, content to 16384 characters, and metadata to 64 entries/8192 serialized bytes.

The HTTP resources plus monotonic goal-event cursor are the supported Desktop contract. Desktop
business logic does not live in the server core. `GET /console` serves an embedded single-file
operator dashboard over this contract: it authenticates with the same `app.api_key` bearer plus
`x-harness-actor` header, renders goals/signals/artifacts/trajectories/self-tests/evolution,
drives the existing plan/approve/resume/verify/propose/ignore/evolution POST routes, and reads
the goal SSE stream via a fetch reader (native `EventSource` cannot send the Authorization
header). The page adds no backend semantics; a native Desktop shell can consume the same
contract later.

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

Each provider frame contains at most 256 request messages/131072 serialized request bytes and
262144 response bytes. `capture` is one of `full`, `redacted`, or `truncated`; main runtime and
Loop Worker provider calls currently use conservative `truncated` capture because seed/provider
configuration cannot yet be reproduced exactly. The `shadow_comparative` replay type is reserved
for a future live-provider mode and has no control-plane endpoint in this release.

## Artifact and prompt safety

Artifacts are immutable and typed: `goal_template`, `dynamic_workflow`, `eval_suite`,
`skill_manifest`, `replay_corpus`, `desktop_view`, `analysis_report`, `self_test_report`,
`evolution_evaluation`, and `prompt_policy_ref`. Content is limited to 32768 bytes. A
`prompt_policy_ref` accepts only `evolution_candidate_id` and optional `deployment_id` reference
fields; it cannot duplicate prompt text. Those IDs are opaque references, not a second active-policy
pointer. The existing Prompt Candidate approval, activation, stale-baseline check, and rollback
implementation remains the sole source of truth for active prompt policy.

## Persistence and current boundary

SQLite schema version 1 stores the current Goal/work state plus append-only events, attempts,
checkpoint phases, signals, Self-test cases/runs, provider frames/replay runs, and Artifact events.
The current release intentionally does not provide:

- a native Desktop GUI shell (the browser dashboard at `/console` is the reference client; the
  HTTP collection/detail resources and per-Goal SSE cursor are the contract),
- arbitrary code edits, commits, deployments, credentials/permission changes, or
  `external_write` handlers beyond the configured allowlist,
- comparative live-provider replay or live MCP calls during structural replay,
- automatic retention/pruning, or vendor-specific community connectors,
- automatic approval or activation of Prompt Evolution candidates.

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
cargo test --test harness_evolution_engine -- --nocapture
```
