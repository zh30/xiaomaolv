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

# Production build (thin LTO, opt-level 3, stripped)
cargo build --release
```

## Architecture

xiaomaolv is an AI gateway service that routes messages through AI providers, manages conversation memory, and supports extensible plugin architectures.

### Core Data Flow

```
Telegram/HTTP → Channel → Service → Memory → Provider (AI) → StreamSink → Channel → Response
                    ↓
              Scheduler (cron jobs)
                    ↓
              MCP Runtime / Skills / Code Mode
```

### Key Traits

- `ChatProvider::complete()` / `complete_stream()` - AI inference entry points
- `StreamSink::on_delta()` - streaming response handler (implemented by channel)
- `ChannelFactory::create_channel()` - channel instance creation
- `ProviderFactory` / `ChannelFactory` - plugin registration points

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

Safe-by-default sandbox with two execution modes:
- `local`: direct Rust evaluation (limited to math, string, format ops)
- `subprocess`: spawns subprocess with resource limits

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
