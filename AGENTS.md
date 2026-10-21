# AGENTS.md

## Build & Test

```bash
# Format
cargo fmt --all

# Lint
cargo clippy --all-targets -- -D warnings

# Test
cargo test --all-targets

# Run integration test
cargo test --test <test_name> -- --nocapture

# Loop Engineering focused regression tests
cargo test --test harness_loop_engine -- --nocapture
cargo test --test http_loop_engine_api -- --nocapture
cargo test --test service_harness_trajectory -- --nocapture
cargo test --test harness_evolution_engine -- --nocapture

# Production build (thin LTO, opt-level 3, stripped)
cargo build --release
```

## Workflow

- After completing each feature block, commit it and open a pull request for
  review — do not accumulate multiple features in one unreviewed branch.

## Architecture

xiaomaolv is an AI gateway service that routes messages through AI providers, manages conversation memory, and supports extensible plugin architectures.

### Core Data Flow

```
Telegram/HTTP → Channel → Service → Memory → Provider (AI) → StreamSink → Channel → Response
                    ↓
              Scheduler (cron jobs)
                    ↓
              MCP Runtime / Skills / Code Mode
                    ↓
     Trajectories/Feedback → Evolution Engine → Shadow Eval → Human Promotion

HTTP/Telegram/Signals → LoopEngine → Workflow DAG → Leased Worker → Checkpoints/Artifacts
                              ↓                         ↓
                       Goal events/SSE           Self-test / Replay
```

### Key Traits

- `ChatProvider::complete()` / `complete_stream()` - AI inference entry points
- `StreamSink::on_delta()` - streaming response handler (implemented by channel)
- `ChannelFactory::create_channel()` - channel instance creation
- `ProviderFactory` / `ChannelFactory` - plugin registration points
- `LoopStore` - durable Goal/Workflow/Attempt/Checkpoint, Signal, Replay, Self-test, and Artifact persistence seam
- `WorkHandler` - registered Dynamic Workflow execution boundary with an explicit effect class

### Message Processing Pipeline (service.rs)

The `Service` struct orchestrates the entire pipeline:
1. Receives inbound message from channel
2. Queries memory for relevant context
3. Optionally runs MCP auto tool loop
4. Optionally runs Skills matching
5. Sends to AI provider
6. Streams response back through channel's StreamSink

### Memory Backend

- **sqlite-only** (default): `memory.rs` handles all storage
- **hybrid-sqlite-zvec**: Layered search combining SQLite + vector sidecar for semantic search
  - Keyword fallback when vector search returns nothing
  - `hybrid_min_score` threshold gates relevance injection
  - `context_memory_budget_ratio` limits memory as % of input budget

### MCP Auto Tool Loop

When `agent.mcp_enabled = true`:
1. Discover tools from configured MCP servers
2. Ask model to emit JSON tool calls
3. Execute tools, feed results back to model
4. Stop at `mcp_max_iterations` or final answer

### Skills Runtime

- Skills are local scripts loaded dynamically with semantic matching
- `skills_match_min_score` gates selection
- `skills_max_selected` limits how many skills are injected into prompt
- MCP tools and Skills operate at different levels: MCP extends AI capabilities, Skills extends agent behavior

### Telegram Group Behavior

- **smart mode** (default): contextual auto-trigger with `group_followup_window_secs` recent bot context window; learns summon aliases automatically
- **strict mode**: requires explicit @mention or reply
- Scheduler commands: natural language parsing with confirmation workflow

### Code Mode

Safe-by-default execution layer with two modes:
- `local`: direct Rust evaluation (limited to math, string, format ops)
- `subprocess`: spawns subprocess with resource limits

Capability metadata filters MCP access before execution. `subprocess` is not an OS-level sandbox.

### Self-Evolving Harness

- `EvolutionEngine` owns the discover/propose/evaluate/approve/activate/rollback state machine
- Evolution is disabled by default and only evolves a bounded replacement system-prompt patch
- Automatic cycles consume failed trajectories or negative feedback and stop at `ready`
- Finished trajectories persist a deterministic `RunScorecard` (composite 0-100 fitness); each cycle runs a deployment outcome review comparing pre/post-activation mean scores — a 12+ point regression emits an audit event plus one deduplicated `deployment_outcome` signal, with rollback staying human-gated; runs scoring <40 self-report as deduplicated `trajectory:scorecard` signals, and proposals carry `prior_interventions` (rejected/failed candidates + rolled-back deployments) so buried ideas are not resurfaced; replying to a bot answer with a bare 👍/👎/±1/好/差 marker records ±0.5 trajectory feedback bound to the chat's most recent finished run (2h window); 3+ flagged runs in 6h emit one hourly-deduped `user_feedback` spike signal; `POST .../eval-cases/from-trajectory` seeds a disabled eval case from a real run (frame-derived input, human-gated, `eval_case_seeded` audit)
- Shadow evaluation calls the provider directly and cannot execute tools, write memory, or send messages
- Shadow evaluation scores operator eval cases plus the versioned benchmark suite (`harness/benchmark.rs`, `id@version`); benchmark regressions are always fatal regardless of `max_regressions`
- Human approval and activation are separate authenticated operations; rollback restores the prior deployment
- SQLite stores candidates, eval snapshots, feedback, deployments, the active pointer, and immutable audit events
- Full bounded evidence SHA-256 is globally unique to prevent stale or concurrent duplicate proposals

### Loop Engineering Harness

- `LoopEngine` owns the durable `Goal -> Workflow revision -> WorkItem DAG -> Attempt -> Checkpoint` lifecycle
- Planning never dispatches work; approval binds the exact goal revision, plan hash, effect manifest, acceptance criteria, and execution budget
- `LoopWorker` uses at-least-once claims, expiring leases, monotonically increasing fencing tokens, and prepared/committed/reconciled checkpoints
- `/resume` expires stale leases and reconciles committed outcomes without replaying their effects
- Registered handlers are `goal_planner`, `provider_analysis`, `self_test_suite`, `session_replay`, `manual_gate`, `evolution_evaluate`, `wait` (durable sleep via `sleep_until` deferral), and — only behind `external_write_enabled` plus a handler allowlist — `channel_send`; unlisted `external_write` handlers are rejected at plan/approve/register/dispatch; `waiting_confirmation` parks are resolved via the goal-scoped `resolve-confirmation` route (`confirmed`/`retry`/`abandoned` + audit reason); handlers declare `replay_safe` crash semantics — unsafe prepared checkpoints park for operator verdict instead of blind re-runs, and a missing handler blocks the work item until a registry holding it polls again; `WorkOutcome.continuation` commits a durable phase boundary (`progress_json` on the work item) before the next phase runs in the same attempt, so a crash resumes mid-work via `ctx.resume` (cap: `MAX_PHASE_CONTINUES`); dependents read declared predecessors' committed outcomes via `ctx.upstream` (step_id → outcome, claim-time snapshot); `evolution_evaluate` runs one eval case per phase against a durable `evolution_eval_runs` cursor (frozen suite + baseline, per-case commit, cleared on finalize)
- Multi-source `EvolutionSignal` records preserve source/trust/deduplication metadata; ingestion dedups on exact keys, normalized content, and token-set near-duplicates; only an operator route can convert `observed`/`triaged` signals into `proposed` goals or mark them `ignored` (HTTP + Telegram `/signals`)
- Telegram updates pass through a durable admission ledger (`telegram_admissions`): `update_id` dedup at the poll boundary, a processing lease that lets a re-delivery reclaim a crashed attempt (bounded attempts), and a sweep that turns stale in-flight rows into failed records plus a deduplicated triage signal — no silent message loss; sent bot messages are recorded in `telegram_outbound_messages` (chat_id+message_id → session+sent_at) so a reply-marker binds the exact producing trajectory via `latest_finished_trajectory_before`, with the recency heuristic as fallback
- Production `core` self-tests are read-only; repeated identical failure sets produce one deduplicated internal signal
- Provider frames are recorded for main plain, Code Mode, and MCP completion paths when trajectory logging is enabled; structural replay executes zero live tools
- HTTP collection/detail resources plus per-goal monotonic SSE are the Desktop boundary; `GET /console` serves an embedded single-file operator dashboard over that contract (same bearer auth, no new backend semantics)
- The evolution adapter may evaluate an existing prompt candidate, but approval, activation, and rollback remain exclusively in `EvolutionEngine`

### Plugin Architecture

**Provider** (`provider.rs` + `provider_plugin_api.rs`):
- Implement `ChatProvider` trait
- Register `ProviderFactory` in `ProviderRegistry`
- Built-in: `OpenAiCompatibleProviderFactory`

**Channel** (`channel.rs` + `channel_plugin_api.rs`):
- Implement channel types in `channel/` subdirectory (group_pipeline, update_pipeline, workers)
- Register `ChannelFactory` in `ChannelRegistry`

### Configuration

TOML with env placeholders (`${VAR:-default}`). Key sections:
- `[app]` - bind, default provider, locale, concurrency limits
- `[providers.<name>]` - AI provider configs
- `[channels.telegram]` - streaming, scheduler, group behavior
- `[channels.http]` - HTTP channel for programmatic messaging
- `[memory]` - backend selection and hybrid settings
- `[agent]` - MCP, skills, code mode settings
- `[agent.harness]` - trajectory, compaction, verification, evolution, and loop-engine settings
- `[agent.harness.evolution]` - self-evolution cycle, evidence, eval gates, and human promotion policy
- `[agent.harness.loop_engine]` - durable control plane, scoped signal ingestion, worker leases/concurrency, and maintenance interval

Current operator documentation: `docs/loop-engineering-harness.md`. Historical documents under
`docs/plans/` describe decisions at the time they were written and are not the runtime contract.
