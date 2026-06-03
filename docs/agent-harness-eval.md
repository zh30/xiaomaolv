# Agent Harness Eval

This deterministic regression subset exercises agent harness behavior with fake providers and the built-in MCP time tool.

Run it with:

```bash
cargo test --test harness_eval -- --nocapture
```

Covered scenarios:

- MCP/tool loop: no tool needed, valid tool call, malformed tool JSON recovery, unavailable tool rejection, max iterations.
- Context compaction: no compaction, head-tail compaction, budget-based compaction.
- Verification: observe, retry once, block.

Each scenario asserts the final answer, trajectory exit reason, tool-call count, and visible verification/error markers where applicable.
