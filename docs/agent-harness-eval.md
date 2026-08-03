# Agent Harness Eval

This deterministic regression subset exercises agent harness behavior with fake providers and the built-in MCP time tool.

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

Run the self-evolution subset with:

```bash
cargo test --test harness_evolution \
  --test harness_evolution_store \
  --test harness_evolution_engine \
  --test service_harness_evolution \
  --test http_evolution_api
```
