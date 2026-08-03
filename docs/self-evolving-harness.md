# Self-Evolving Harness

xiaomaolv can turn production trajectories and explicit human feedback into bounded
system-prompt candidates, compare each candidate with the active policy in an isolated
shadow suite, and then let an authenticated operator approve, activate, or roll it back.

The loop is deliberately split at the production boundary:

```text
trajectory failure or negative feedback
                |
                v
        bounded prompt proposal
                |
                v
      baseline vs candidate eval
          |              |
       rejected        ready
                          |
                 human approve + activate
                          |
                    observe / rollback
```

Automatic work stops at `ready`. The model cannot approve a candidate, activate it, edit
code, change tool permissions, change credentials, or deploy the service.

## Safety Model

- Disabled by default.
- The only evolvable surface is a replacement system-prompt patch.
- Prompt patches are length bounded and cannot contain internal MCP/code-mode result markers.
- Shadow evaluation calls the provider directly: no MCP calls, memory writes, channel sends,
  or code execution.
- Promotion gates require a minimum suite size, candidate score and score improvement, and
  cap regressions.
- Approval and activation reject scorecards whose baseline no longer matches the active policy;
  stale candidates must be re-evaluated and re-approved.
- Human approval is required by default. HTTP promotion also requires the app API key and an
  `x-evolution-actor` audit label.
- Activation and rollback atomically update one SQLite active-deployment pointer.
- The runtime reloads the active deployment after restart.
- The full bounded evidence snapshot has a globally unique SHA-256, preventing stale evidence
  reuse and duplicate candidates across concurrent process instances. New feedback changes the
  snapshot and can legitimately produce a new candidate.
- Proposal attempts, state transitions, evaluations, feedback, activations, and rollbacks are
  recorded in an append-only audit table.

## Configuration

Set a non-empty `[app].api_key`; the evolution control plane refuses access without one.

```toml
[agent.harness]
# Required for evidence discovery and automatic cycles.
enable_trajectory = true

[agent.harness.evolution]
enabled = true

# Optional background discovery/evaluation. It never activates a candidate.
auto_cycle_enabled = false
cycle_interval_secs = 3600       # minimum 60 when automatic cycles are enabled
cycle_initial_delay_secs = 60

max_source_trajectories = 20
max_evidence_chars = 8000

min_eval_cases = 3                # enabled suite is capped at 50 cases
min_candidate_score = 0.80
min_score_delta = 0.05
max_regressions = 0
max_prompt_patch_chars = 4000
require_human_approval = true
```

`auto_cycle_enabled = true` requires all of the following at startup:

- `agent.harness.evolution.enabled = true`
- `agent.harness.enable_trajectory = true`
- `cycle_interval_secs >= 60`
- a non-empty `app.api_key`

## Authentication

Every endpoint below requires:

```text
Authorization: Bearer <app.api_key>
```

Human mutations also require:

```text
x-evolution-actor: <operator-id>
```

The actor may contain ASCII letters, digits, `-`, `_`, `.`, and `@`, up to 128 bytes. It is
recorded for attribution after the shared app API key authenticates the request; it is not a
separate identity proof.

The examples use:

```bash
export XIAOMAOLV_URL=http://127.0.0.1:8080
export XIAOMAOLV_KEY=replace-me
export XIAOMAOLV_ACTOR=henry
```

## 1. Create the Shadow Eval Suite

At least `min_eval_cases` enabled cases are required. Assertions are deterministic:
required substrings, forbidden substrings, and optional JSON validity.

```bash
curl -sS -X POST "$XIAOMAOLV_URL/v1/harness/evolution/eval-cases" \
  -H "authorization: Bearer $XIAOMAOLV_KEY" \
  -H "x-evolution-actor: $XIAOMAOLV_ACTOR" \
  -H 'content-type: application/json' \
  -d '{
    "id":"answer-format",
    "name":"Answer follows the supported format",
    "input":"Return a compact answer containing SAFE_OK.",
    "assertions":{
      "required_substrings":["SAFE_OK"],
      "forbidden_substrings":["UNSAFE"],
      "require_json":false
    },
    "weight":1.0,
    "enabled":true
  }'
```

List cases:

```bash
curl -sS "$XIAOMAOLV_URL/v1/harness/evolution/eval-cases?enabled_only=true" \
  -H "authorization: Bearer $XIAOMAOLV_KEY"
```

## 2. Attach Explicit Feedback

Trajectory failures are discovered automatically. A negative score also makes a normal
`final_answer` trajectory eligible evidence.

```bash
curl -sS -X POST "$XIAOMAOLV_URL/v1/harness/evolution/feedback" \
  -H "authorization: Bearer $XIAOMAOLV_KEY" \
  -H "x-evolution-actor: $XIAOMAOLV_ACTOR" \
  -H 'content-type: application/json' \
  -d '{
    "trajectory_id":"trajectory-id-from-harness",
    "score":-1.0,
    "tags":["incorrect","format"],
    "comment":"The answer ignored the requested output contract."
  }'
```

Scores must be in `[-1, 1]`. Feedback must reference an existing trajectory.

## 3. Propose and Evaluate

Run one evidence-driven cycle:

```bash
curl -sS -X POST "$XIAOMAOLV_URL/v1/harness/evolution/cycle" \
  -H "authorization: Bearer $XIAOMAOLV_KEY"
```

The cycle gathers bounded failure metadata (including failed tool arguments/results, final
answer excerpts, and explicit feedback), asks the configured provider for strict proposal
JSON, persists one candidate, evaluates baseline and candidate outputs, and ends in `ready`,
`rejected`, or `failed`. Re-running against unchanged evidence does not create a duplicate.
At least `min_eval_cases` enabled cases must exist before a cycle will consume evidence. If a
process stops after persisting a `draft`, the next cycle resumes that candidate instead of
creating another one.

For a manually authored candidate:

```bash
curl -sS -X POST "$XIAOMAOLV_URL/v1/harness/evolution/candidates" \
  -H "authorization: Bearer $XIAOMAOLV_KEY" \
  -H "x-evolution-actor: $XIAOMAOLV_ACTOR" \
  -H 'content-type: application/json' \
  -d '{
    "prompt_patch":"Always honor the requested output contract and state uncertainty.",
    "rationale":"Address repeated format and unsupported-certainty failures.",
    "source_trajectory_ids":[]
  }'
```

Then evaluate it:

```bash
curl -sS -X POST \
  "$XIAOMAOLV_URL/v1/harness/evolution/candidates/<candidate-id>/evaluate" \
  -H "authorization: Bearer $XIAOMAOLV_KEY"
```

## 4. Review, Approve, and Activate

Inspect the candidate and its latest immutable scorecard:

```bash
curl -sS "$XIAOMAOLV_URL/v1/harness/evolution/candidates/<candidate-id>" \
  -H "authorization: Bearer $XIAOMAOLV_KEY"
```

Only a `ready` candidate whose latest decision passed can be approved:

```bash
curl -sS -X POST \
  "$XIAOMAOLV_URL/v1/harness/evolution/candidates/<candidate-id>/approve" \
  -H "authorization: Bearer $XIAOMAOLV_KEY" \
  -H "x-evolution-actor: $XIAOMAOLV_ACTOR" \
  -H 'content-type: application/json' \
  -d '{"reason":"Reviewed the patch and all case-level results."}'
```

Activation is a separate decision and takes effect for new provider-backed normal and
streaming requests without restarting the process. Built-in fast paths that do not call the
provider do not consume the patch:

```bash
curl -sS -X POST \
  "$XIAOMAOLV_URL/v1/harness/evolution/candidates/<candidate-id>/activate" \
  -H "authorization: Bearer $XIAOMAOLV_KEY" \
  -H "x-evolution-actor: $XIAOMAOLV_ACTOR" \
  -H 'content-type: application/json' \
  -d '{"reason":"Controlled production rollout after operator review."}'
```

## 5. Observe and Roll Back

Status includes the active policy, gate configuration, and the last automatic/manual cycle.
Expected idle runs are reported as `skipped` with `last_skip_reason`, not as failures:

```bash
curl -sS "$XIAOMAOLV_URL/v1/harness/evolution/status" \
  -H "authorization: Bearer $XIAOMAOLV_KEY"
```

Audit history:

```bash
curl -sS "$XIAOMAOLV_URL/v1/harness/evolution/audit?limit=100" \
  -H "authorization: Bearer $XIAOMAOLV_KEY"
```

Rollback restores the prior deployment, or the built-in behavior when there was no prior
deployment:

```bash
curl -sS -X POST "$XIAOMAOLV_URL/v1/harness/evolution/rollback" \
  -H "authorization: Bearer $XIAOMAOLV_KEY" \
  -H "x-evolution-actor: $XIAOMAOLV_ACTOR" \
  -H 'content-type: application/json' \
  -d '{"reason":"Observed a production regression."}'
```

## Endpoint Reference

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/v1/harness/evolution/status` | Active policy, gates, cycle status |
| `GET/POST` | `/v1/harness/evolution/candidates` | List or manually create candidates |
| `GET` | `/v1/harness/evolution/candidates/{id}` | Candidate and latest evaluation |
| `POST` | `/v1/harness/evolution/candidates/{id}/evaluate` | Run isolated shadow eval |
| `POST` | `/v1/harness/evolution/candidates/{id}/abandon-evaluation` | Recover an interrupted evaluation |
| `POST` | `/v1/harness/evolution/candidates/{id}/approve` | Record human approval |
| `POST` | `/v1/harness/evolution/candidates/{id}/activate` | Atomically activate |
| `GET/POST` | `/v1/harness/evolution/eval-cases` | List or upsert eval cases |
| `GET/POST` | `/v1/harness/evolution/feedback` | List or record feedback |
| `POST` | `/v1/harness/evolution/cycle` | Discover, propose, and evaluate once |
| `GET` | `/v1/harness/evolution/audit` | Immutable audit history |
| `POST` | `/v1/harness/evolution/rollback` | Restore prior active deployment |

## Persistence and Failure Behavior

SQLite stores candidates, eval cases, evaluations, feedback, deployments, the singleton
active pointer, and audit events. Candidate transitions use compare-and-set semantics;
activation and rollback are transactions. Evaluation provider failures move an in-progress
candidate to `failed`. Each scorecard keeps the evaluated case definition and input, bounded
baseline/candidate output excerpts, and full-output SHA-256 hashes so approval remains
reviewable after the live eval suite changes. Proposal failures record only a bounded stage
classification in audit, not the raw model output.

If a process exits while a candidate is `evaluating`, an operator can post a reason to
`/candidates/{id}/abandon-evaluation`. This moves it to `failed`; `failed` and `rejected`
candidates can be evaluated again after the underlying issue or eval suite changes.

The background worker treats “no new failure evidence” as a skipped cycle and keeps running.
It is intentionally not a deployment worker.

Setting `enabled = false` is a runtime kill switch: the stored active deployment is retained,
but its prompt is not injected until evolution is enabled again. Use the rollback endpoint when
you want to clear or restore the durable deployment pointer instead.

## Current Boundary

Phase one evaluates the replacement prompt against explicit deterministic cases using the
same provider. It does not replay MCP tools, channel delivery, or conversation memory during
shadow evaluation. Use eval cases that represent the behavior you are willing to gate, inspect
case-level results before approval, and keep rollback available during rollout.
