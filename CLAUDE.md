# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**xiaomaolv** is a high-performance Rust AI gateway service with Telegram integration, MCP (Model Context Protocol) support, agent skills runtime, and hybrid memory (SQLite + optional zvec vector sidecar). It acts as an intelligent chatbot gateway that routes messages through AI providers (OpenAI-compatible API), manages conversation memory, executes scheduled tasks, and supports extensible plugin architectures for providers and channels.

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
| `provider.rs` | AI provider abstraction (`ChatProvider` trait + `ProviderRegistry`) |
| `channel.rs` | Telegram channel implementation with submodules for group/PM handling |
| `memory.rs` | Conversation memory with SQLite backend and hybrid zvec support |
| `scheduler.rs` | Scheduled task execution with cron-style scheduling |
| `mcp.rs` | MCP runtime for tool calling |
| `skills.rs` | Agent skills registry and runtime |
| `code_mode.rs` | Safe code execution sandbox |
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
- `[agent]` - MCP and skills settings, code mode sandbox settings

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

Safe-by-default sandbox for code execution. Disabled by default. Supports `local` and `subprocess` execution modes with configurable resource limits (max_calls, max_runtime_ms, allow_network/filesystem/env).

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
