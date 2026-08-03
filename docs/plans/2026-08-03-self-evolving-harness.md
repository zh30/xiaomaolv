# Self-Evolving Harness Design

**Status:** Complete  
**Date:** 2026-08-03

> This is the completed Phase 1 design record. Loop Engineering extends its evidence and execution
> scope; the Prompt Policy promotion safety invariants here remain active. Current operator docs:
> `docs/self-evolving-harness.md` and `docs/loop-engineering-harness.md`.

## Goal

Turn the existing execution harness into a closed, production-controlled improvement loop:

```text
trajectories + explicit eval cases
              |
              v
       improvement candidate
              |
              v
   baseline/candidate shadow eval
       |                  |
   rejected             ready
                          |
                   human approval
                          |
                       activate
                          |
                 observe or rollback
```

The harness may discover and test improvements by itself. It must not silently change
production behavior. Promotion remains an explicit, authenticated operation and every
state change is durable and auditable.

## First Evolvable Surface

The first supported candidate type is a bounded system-prompt patch. The active patch is
appended as a distinct system message before normal inference. This surface is useful,
hot-swappable, and reversible without granting the model filesystem, deployment, tool
permission, or arbitrary configuration mutation access.

Code changes, MCP capability changes, secrets, provider credentials, and channel settings
are intentionally not evolvable in this phase.

## Safety Invariants

1. Evolution is disabled by default.
2. Candidate prompt patches have a configured character limit and cannot contain the
   internal tool-result control markers used by the runtime.
3. Evaluation is shadow-only: it calls the provider directly and cannot execute MCP tools,
   write conversation memory, or send channel messages.
4. Candidate and active-policy outputs are evaluated against the same enabled eval cases.
5. A candidate becomes `ready` only when it clears the minimum score, minimum improvement,
   regression, and minimum-case gates.
6. Only a `ready` candidate can be approved; only an approved candidate can be activated.
7. Activation requires an authenticated human actor by default. No background task can
   bypass this gate.
8. Activation and rollback update one durable active-policy pointer transactionally.
9. Every proposal attempt, evaluation, approval, rejection, activation, and rollback produces an
   immutable audit event.
10. Runtime startup reloads the active policy from SQLite. Corrupt active policy state fails
    startup rather than silently changing behavior.
11. The bounded evidence snapshot SHA-256 is globally unique in SQLite, so concurrent runtime
    instances cannot persist duplicates or reuse stale evidence after policy activation.

## Domain Model

```text
EvolutionCandidate
  id
  parent_candidate_id?
  evidence_fingerprint?
  prompt_patch
  rationale
  source_trajectory_ids[]
  status: draft | evaluating | ready | rejected | approved | failed
  created_at / updated_at

EvolutionEvalCase
  id / name / input
  required_substrings[]
  forbidden_substrings[]
  require_json
  weight / enabled

EvolutionEvaluation
  id / candidate_id
  baseline_candidate_id?
  baseline_score / candidate_score / score_delta
  regressions / passed_cases / total_cases
  decision / case_results[]

EvolutionDeployment
  id / candidate_id
  previous_deployment_id?
  activated_by / reason / activated_at
  rolled_back_at? / rolled_back_by? / rollback_reason?

EvolutionAuditEvent
  id / candidate_id? / deployment_id?
  event_type / actor / details / created_at

EvolutionFeedback
  id / trajectory_id / score
  tags[] / comment? / actor / created_at
```

Candidate lifecycle:

```text
draft -> evaluating -> ready -> approved
   |          |          |
   |          +-------> failed
   +------------------> rejected

failed/rejected -> evaluating (explicit retry)

Activation is represented by deployments rather than an `active` candidate status. This
preserves activation history and allows a previously approved candidate to be reactivated.
```

## Evaluation Contract

Each enabled case runs twice through the same provider in an isolated, no-tool shadow prompt:

- Baseline: user case plus the currently active patch, if any.
- Candidate: the same user case plus the candidate replacement patch.

A case passes when all required substrings are present, no forbidden substring is present,
and JSON parsing succeeds when requested. Weighted score is `passed_weight / total_weight`.
A regression is a case passed by the baseline and failed by the candidate. The immutable
scorecard includes each case definition, bounded output excerpts, and full-output SHA-256
hashes so the decision can be reviewed after the live suite changes.

Promotion gates are configured under `[agent.harness.evolution]`:

- `min_eval_cases`
- `min_candidate_score`
- `min_score_delta`
- `max_regressions`
- `max_prompt_patch_chars`

## Runtime and Control Plane

`EvolutionEngine` is the public command seam. It owns state transitions, calls the
provider for proposal/evaluation, persists through `EvolutionStore`, and updates a shared
`EvolutionPolicyRuntime` cache after activation or rollback.

Authenticated HTTP endpoints expose eval-case management, proposal, evaluation, approval,
activation, rollback, status, and audit history. The existing app API key protects all
evolution endpoints. Mutating endpoints also use the existing API rate limiter.

An optional cycle endpoint and timer perform discovery, proposal, and evaluation, but stop
at `ready`; human approval and activation remain separate operations. Negative explicit
feedback can promote otherwise successful trajectories into proposal evidence. Cycle status
tracks the last start, finish, candidate, outcome, and bounded error.

## Completion Evidence

- Domain/state transition tests reject invalid transitions.
- SQLite tests prove persistence, single active pointer, restart recovery, audit immutability,
  and rollback.
- Engine tests prove baseline/candidate comparison and every promotion gate.
- Service tests prove the active patch reaches provider-backed normal and streaming inference.
- HTTP tests prove auth, validation, lifecycle, activation, and rollback.
- Config bootstrap tests prove safe defaults and bounds.
- The deterministic harness eval suite covers an end-to-end evolution cycle.
- `cargo fmt --all`, strict Clippy, all targets, and release build pass.

## Not In This Phase

- Autonomous code editing or deployment.
- Automatic production activation.
- Tool calls during shadow evaluation.
- Evolution of credentials, network/filesystem permissions, or provider/channel settings.

These exclusions are safety boundaries, not substitutes for the requested closed loop. The
project becomes self-evolving through a real discover/propose/evaluate/promote/rollback
policy loop while keeping irreversible surfaces outside model control.
