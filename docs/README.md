# Documentation Map

This index separates current runtime contracts from historical design records. When a plan and a
runbook differ, the current code, tests, configuration templates, and runbook take precedence.

## Start here

- `../README.md` / `../README.zh.md` — installation, configuration overview, and public API entry.
- `development.md` — contributor workflow, module boundaries, focused tests, and PR checklist.
- `engineering-quality.md` — repository-wide quality and architecture invariants.

## Current subsystem runbooks

- `loop-engineering-harness.md` — Durable Goal/Workflow execution, approval, recovery,
  multi-source Signals, Self-test, Session Replay, Artifacts, and Desktop-ready HTTP/SSE.
- `self-evolving-harness.md` — Prompt candidate discovery/evaluation, human approval, activation,
  audit, and rollback. This remains the only active Prompt Policy authority.
- `agent-harness-eval.md` — deterministic regression suites for the message harness, Prompt
  Evolution, and Loop Engineering.
- `mcp-integration.md` — MCP install/runtime contract and its trajectory/replay boundary.
- `code-mode-observability.md` — Code Mode metrics, alerts, and provider-frame data handling.
- `real-test-minimax-telegram.md` — real MiniMax/Telegram setup and optional `/goal` smoke test.
- `zvec-sidecar.md` — hybrid SQLite/zvec memory contract.

## Historical records

Files under `plans/` preserve the decisions, gaps, and task breakdown known at their stated date.
They are not a backlog and may describe a “current state” that has since changed. In particular:

- `plans/2026-08-03-self-evolving-harness.md` records the completed Prompt Evolution phase.
- `plans/2026-08-03-loop-engineering-harness.md` records the implemented Phase 2 design and its
  intentionally deferred boundaries.
- Earlier Harness roadmaps are retained to explain how the current architecture evolved.

Current intentionally deferred Loop Engineering scope is explicit in
`loop-engineering-harness.md`: no Desktop GUI, arbitrary external-write/code/deploy/credential
mutation, comparative live-provider replay, automated retention/pruning, or automatic Prompt
activation.
