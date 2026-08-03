# Agent Harness Eval

This guide maps the deterministic regression suites for both the message-execution harness and
the durable Loop Engineering layer. Tests use fake providers, local SQLite, and the built-in MCP
time tool; they do not require production credentials.

## Message harness subset

Run it with:

```bash
cargo test --test harness_eval -- --nocapture
```

Covered scenarios:

- AgentRun lifecycle: final answer, tool error, max iterations, internal error.
- ToolProtocol: valid tool call, malformed JSON recovery, unknown tool rejection, schema-invalid arguments.
- Context compaction: no compaction, head-tail compaction, budget-based compaction, persisted summary reuse.
- OutputExit: observe, revise once, block hidden tool errors, block unresolved tool-call JSON.
- Skills runtime: selected skill IDs are observable before prompt rendering.
- Self-evolution: prompt validation and scoring gates, SQLite state and audit persistence,
  feedback-driven proposal, concurrent evidence deduplication, human promotion, runtime
  injection, automatic-cycle safety, and rollback.

Each scenario asserts the final answer, trajectory exit reason, tool-call count, and visible verification/error markers where applicable.

## Prompt Evolution subset

Run the self-evolution subset with:

```bash
cargo test --test harness_evolution \
  --test harness_evolution_store \
  --test harness_evolution_engine \
  --test service_harness_evolution \
  --test http_evolution_api
```

This covers bounded prompt candidates, immutable scorecards, evidence deduplication, human
promotion, runtime injection, automatic-cycle safety, and rollback. Prompt activation is never a
Loop Worker side effect.

## Loop Engineering subset

```bash
cargo test --test harness_loop_engine -- --nocapture
cargo test --test http_loop_engine_api -- --nocapture
cargo test --test service_harness_trajectory -- --nocapture
cargo test --test harness_evolution_engine -- --nocapture
```

Coverage includes:

- Goal/Workflow/WorkItem/Attempt/Checkpoint transitions, approval hash binding, DAG validation,
  lease expiry, fencing, retry, and committed-outcome reconciliation.
- Multi-source Signal provenance/deduplication, scoped ingestion, and proposed-Goal-only flow.
- Read-only `core` Self-test persistence and repeated-failure Signal deduplication.
- Immutable Artifact validation, including reference-only `prompt_policy_ref`.
- Provider frame capture for plain, Code Mode, and MCP non-stream/stream completions.
- Structural Session Replay integrity with zero live tools.
- Operator versus ingest authentication, Goal lifecycle, collections, and event cursor behavior.
- Bounded `evolution_evaluate` integration without approval or activation.

For a release-level check, run the repository gates:

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --release
```

The SSE route and disabled-mode guard exist in production code but do not yet have dedicated
integration assertions; add those before changing either contract.
