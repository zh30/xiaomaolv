# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**xiaomaolv** is a high-performance Rust AI gateway service with Telegram integration, MCP (Model Context Protocol) support, agent skills runtime, hybrid memory (SQLite + optional zvec vector sidecar), and a durable Loop Engineering harness. It routes messages through OpenAI-compatible providers, manages conversation memory and scheduled tasks, and can turn operator goals or provenance-preserving signals into approved, resumable workflow DAGs without granting arbitrary production mutation rights.

## Tech Stack

- **Language**: Rust (edition 2024)
- **Web Framework**: Axum 0.8
- **Database**: SQLite via sqlx
- **Async Runtime**: Tokio
- **Serialization**: TOML (config), JSON (API)
- **HTTP Client**: reqwest with rustls

## Build Commands

```bash
# Format
cargo fmt --all

# Lint
cargo clippy --all-targets -- -D warnings

# Test
cargo test --all-targets

# Run (uses config/xiaomaolv.toml by default)
cargo run -- serve

# Production build (thin LTO, opt-level 3, stripped)
cargo build --release

# Run integration test
cargo test --test <test_name> -- --nocapture
```

## Architecture

### Core Modules (src/)

| Module | Purpose |
|--------|---------|
| `service.rs` | Central message processing pipeline, orchestrates all subsystems |
| `http.rs` | HTTP API layer (Axum router, endpoints) |
| `http/harness_control.rs` | Authenticated Loop Engineering HTTP/SSE control plane |
| `provider.rs` | AI provider abstraction (`ChatProvider` trait + `ProviderRegistry`) |
| `channel.rs` | Telegram channel implementation with submodules for group/PM handling |
| `memory.rs` | Conversation memory with SQLite backend and hybrid zvec support |
| `scheduler.rs` | Scheduled task execution with cron-style scheduling |
| `mcp.rs` | MCP runtime for tool calling |
| `skills.rs` | Agent skills registry and runtime |
| `code_mode.rs` | Bounded local/subprocess execution layer and capability policy |
| `harness/` | Run lifecycle, trajectories, compaction, verification, output/tool protocol, and evolution |
| `harness/loop_engine/` | Durable goals, workflow DAGs, leases/checkpoints, signals, self-tests, replay, artifacts, and workers |
| `config.rs` | TOML config parsing with env placeholder resolution |
| `skills_commands.rs` | CLI skill management commands |
| `mcp_commands.rs` | CLI MCP management commands |

### Plugin Architecture

**Provider Plugin API** (`provider_plugin_api.rs`):
- Implement `ProviderFactory` trait to add new AI providers
- Register factories in `ProviderRegistry`
- Built-in: `OpenAiCompatibleProviderFactory` for OpenAI-compatible APIs

**Channel Plugin API** (`channel_plugin_api.rs`):
- Implement `ChannelFactory` trait to add new message channels
- Register in `ChannelRegistry`
- Built-in: Telegram channel (with submodules: group_pipeline, update_pipeline, workers)

### Configuration

Configuration uses TOML with environment variable placeholders (`${VAR:-default}`). See `config/xiaomaolv.example.toml` for the full schema.

Key config sections:
- `[app]` - bind address, default provider, locale, concurrency limits
- `[providers.<name>]` - AI provider configs (kind, base_url, api_key, model)
- `[channels.telegram]` - Telegram bot settings, streaming, scheduler, group behavior
- `[channels.http]` - HTTP channel for programmatic messaging
- `[memory]` - Memory backend (sqlite-only or hybrid-sqlite-zvec)
- `[agent]` - MCP/Skills settings and Code Mode execution policy
- `[agent.harness]` - trajectory, compaction, verification, prompt evolution, and Loop Engineering settings
- `[agent.harness.evolution]` - bounded prompt candidates, shadow evaluation, and human promotion gates
- `[agent.harness.loop_engine]` - durable goals, scoped signal ingestion, worker polling/leases/concurrency, and periodic self-tests

### Memory Backend

Two modes:
1. **sqlite-only** (default) - stores messages in SQLite
2. **hybrid-sqlite-zvec** - combines SQLite with optional zvec vector sidecar for semantic search

### MCP Integration

MCP servers can be installed via CLI or Telegram `/mcp` commands:
```bash
xiaomaolv mcp add tavily --scope user -- npx -y @tavily/mcp-server
```

Runtime exposes HTTP endpoints for tool calling at `/v1/mcp/tools/{server}/{tool}`.

### Skills Runtime

Skills are local scripts loaded dynamically. Install via CLI or Telegram `/skills`:
```bash
xiaomaolv skills install agent-browser --scope user --mode semantic
```

### Code Mode

Safe-by-default execution layer, disabled by default. It supports `local` and `subprocess` modes
with configurable limits (`max_calls`, `max_runtime_ms`, network/filesystem/env capability
filters). Capability metadata controls MCP access, but `subprocess` is not an OS-level sandbox.

### Loop Engineering Harness

The persisted execution hierarchy is `Goal -> immutable Workflow revision -> WorkItem DAG -> Attempt -> Checkpoint`. Planning is non-executing; dispatch requires approval of the exact goal revision and plan hash. Claims are at-least-once and protected by leases and fencing tokens. `/resume` reconciles committed checkpoints without replaying effects.

Only registered safe handlers can execute. `external_write`, autonomous code/deploy/credential changes, and automatic prompt activation are not available. Only an operator route can convert a multi-source Signal into a proposed Goal; the production `core` self-test suite is read-only, and structural Session Replay calls no live tools. The HTTP/SSE API is Desktop-ready, but no Desktop GUI is present. See `docs/loop-engineering-harness.md` for the runtime contract.

### Telegram Group Behavior

- **smart mode** (default): contextual auto-trigger with recent bot context window
- **strict mode**: requires explicit @mention or reply
- Natural language scheduler parsing with confirmation workflow
- Group alias learning (no manual config needed for summon aliases)

### Test Organization (tests/)

Integration tests mirror the src structure:
- `tests/service_pipeline.rs` - message processing pipeline
- `tests/telegram_channel_mode.rs` - Telegram channel modes
- `tests/provider_plugin_api.rs` - provider extension API
- `tests/channel_plugin_api.rs` - channel extension API
- `tests/mcp_commands.rs` - MCP CLI commands
- `tests/skills_registry.rs`, `tests/skills_commands.rs` - skills subsystem
- `tests/memory_store.rs`, `tests/hybrid_memory_sidecar.rs` - memory subsystem
- `tests/scheduler_store.rs`, `tests/scheduler_domain.rs` - scheduler subsystem
- `tests/http_api.rs` - HTTP API endpoints
- `tests/harness_loop_engine.rs` - durable state, leases/fencing, recovery, signals, self-tests, replay, artifacts, and safe handlers
- `tests/http_loop_engine_api.rs` - Loop Engineering auth, Signal scope, collections, Goal lifecycle, and event cursor
- `tests/service_harness_trajectory.rs` - provider-frame capture for plain, Code Mode, and MCP paths
- `tests/harness_evolution_engine.rs` - bounded Loop Engine adapter into prompt evolution
- `tests/config_bootstrap.rs`, `tests/config_ui.rs` - config handling

## Development Patterns

### Adding a New Provider
1. Implement `ChatProvider` trait in `src/provider.rs`
2. Create a `ProviderFactory` implementation
3. Register in `ProviderRegistry::with_defaults()`

### Adding a New Channel
1. Implement channel types in `src/channel/`
2. Create a `ChannelFactory` implementation
3. Register in the channel registry during app build

### Key Trait Boundaries
- `ChatProvider::complete()` / `complete_stream()` - AI inference
- `StreamSink::on_delta()` - streaming response handler
- `ChannelFactory::create_channel()` - channel instance creation
- `LoopStore` - durable Loop Engineering persistence
- `WorkHandler` - registered workflow effect boundary; external writes are rejected in this release
