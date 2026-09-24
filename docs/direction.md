# Architecture Direction — Durable, Gated Execution

**Status:** Active  
**Date:** 2026-09-24  
**Audience:** operators and agentic workers deciding what to build next

This document records the architectural north star for xiaomaolv and the reasoning behind it.
Task-level sequencing lives in `docs/plans/2026-09-24-execution-unification-plan.md`.

## Where we are

xiaomaolv is a single-binary, single-operator AI gateway plus a recoverable agent harness. The
differentiated bet has already been made: durable `Goal -> Workflow -> WorkItem -> Attempt ->
Checkpoint` state, at-least-once claims behind leases and fencing tokens, approval-bound
dispatch, evidence deduplication, and human promotion gates. Most agent frameworks skip
durability entirely; this codebase did not. That is the moat.

Three honest observations about the current state:

1. **Two parallel execution worlds exist.** Chat messages flow through
   `src/service.rs` — a ~5,600-line module that contains the swarm dispatcher, MCP tool loop,
   Code Mode path, skills injection, compaction, verification, trajectory hooks, and ~30
   pass-through storage proxies. Goals flow through `src/harness/loop_engine/` — a clean,
   durable, recoverable world. The two worlds do not share execution semantics: a swarm run
   cannot resume after a crash, and a message turn produces no checkpoint.

2. **The harness is a read-only analyst, not an actor.** `external_write` handlers are rejected
   at registration and at plan validation. Self-tests are read-only, replay never touches live
   tools, and evolution can only propose a bounded prompt patch. The system proves safety by
   refusing effects. That was the right foundation, but a harness that cannot act is an audit
   log, not a runtime.

3. **The gates are the product.** Approval-bound plan hashes, effect manifests, deduplicated
   multi-source signals, shadow evaluation with immutable scorecards — this is what separates
   the project from LangChain-style "give the model tools and hope" frameworks. Depth here
   compounds; breadth elsewhere does not.

## North star

**One durable, gated execution fabric for all work the service performs.**

Every unit of work — a chat turn, a swarm node, a workflow step, an evolution evaluation — is an
auditable attempt behind a lease, producing checkpoints that survive process death, with effects
gated by an explicit class system. `service.rs` shrinks to orchestration over named stages; the
loop engine becomes the single execution substrate.

## Direction (phased)

### Phase A — Decompose the message pipeline

Split `src/service.rs` into a module tree (`src/service/`) along the seams that already exist:
turn preparation, swarm execution, completion paths, storage delegates, streaming adapters.
Zero behavior change; each step is mechanical and covered by the existing suite. This is a
prerequisite — every later phase touches this file, and 5,600 lines is where refactors go to
die.

### Phase B — Converge the execution worlds

Record swarm runs in loop-engine terms: a run projects to a Goal, nodes project to WorkItems,
and dynamic decomposition maps to new workflow revisions via the existing `Replan` status.
Because swarm activation happens inside a chat turn (no human in the loop per node), this
requires a bounded **internal auto-approval policy**: an `internal:auto` actor, effect classes
capped at `read`, and hard budget limits. The swarm keeps its in-process executor initially;
durability of record comes first, durability of execution second.

### Phase C — Open the effect surface, carefully

Enable `external_write` behind a config flag, a per-handler allowlist, and the discipline the
checkpoint model already encodes: `prepared` records write intent plus an idempotency key,
`committed` records the outcome, `reconciled` confirms post-crash that committed effects are not
replayed. The first external-write handler should be one we fully control — sending a message
through a registered channel — so the semantics are honest (at-least-once, approval + evidence
as mitigation) rather than aspirational (exactly-once claims we cannot keep).

### Phase D — Deepen the gates

Expand the deterministic eval suite into a quality benchmark whose scorecards feed the
evolution shadow-evaluation gate. Polish the signal-to-goal pipeline (dedup quality, operator
review UX). The ceiling of the evolution engine is set by eval quality, not by proposal
machinery.

### Phase E — Desktop control plane (deferred)

The HTTP/SSE contract is already stable (collections, per-goal event cursors). A UI over a
read-only engine is a dashboard; a UI over an acting engine is mission control. Build it after
Phase C makes the engine worth watching.

## Explicit non-goals

- **More channels or providers.** Discord/Slack/Anthropic-native adapters are commodity surface
  area. The OpenAI-compatible provider + Telegram/HTTP channels cover the single-operator
  product.
- **Multi-tenant or team operation.** SQLite, single binary, and the lease model all assume one
  operator. Going multi-tenant is a different product (Postgres, authn/z, worker pools) and is
  rejected for now — it would dilute the moat before the moat is deep.
- **More memory backends.** `sqlite-only` and `hybrid-sqlite-zvec` are sufficient; semantic
  memory depth is a feature-level concern, not an architectural one.

## The decision already embedded in this direction

If the harness never gains controlled write capability, it remains a well-instrumented observer
and the durability work is ornamental. The strategy therefore commits to Phase C — but only
through the gate model (approval, effect manifests, idempotency, reconciliation), never around
it. Any future proposal to "just let the model call tools in workflows" without those gates
contradicts the north star and should be rejected.

## Decisions

### 2026-09-24 — Ordinary chat turns stay outside the Goal/Attempt model

**Decision:** `handle`/`handle_stream` do not emit a loop-engine Attempt per turn. Swarm runs
project into loop-engine state (Phase B); ordinary turns do not.

**Rationale:**

- The durable record already exists. A turn persists the inbound message, one trajectory frame
  per provider call (when `enable_trajectory` is on), and the assistant reply. A mid-turn crash
  leaves a diagnosable state — stored question, partial frames, no reply — not a mystery.
- Recovery is not the same as replayability. Resuming a chat turn means re-running the provider
  call *and* re-delivering to the channel, and channel delivery is exactly the `external_write`
  Phase C has not opened yet. Even then the Telegram reply context and stream sink are not
  durable, so a resumed turn could only be a degraded re-send. The honest primitive is a
  delivery gate (T9 `channel_send`), not a turn-level DAG.
- The cost is asymmetric. Goal + plan + approval + claim + attempt + checkpoint + verify is a
  heavy write amplification for a high-frequency, mostly read-only path; the Goal model exists
  for multi-step durable work, not per-message bookkeeping.
- Turns that do perform durable work already project through their own paths (swarm → Goal,
  scheduler → jobs table). What remains uncovered is only "reply not delivered" — a channel
  concern, not an execution-model gap.

**Revisit when:** Phase C lands `channel_send`; if post-crash silence proves annoying, the seam
is a small pending-reply reconciliation in `prepare_turn`/`persist_assistant_reply`, not a full
turn workflow.
