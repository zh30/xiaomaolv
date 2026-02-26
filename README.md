# xiaomaolv

A high-performance Rust AI gateway. Configure your provider and channel, then run.

Chinese version: `README.zh.md`

## Table of Contents

- [Features](#features)
- [Quick Start (Recommended: MiniMax + Telegram)](#quick-start)
- [Documentation Index](#docs-index)
- [Runtime Modes](#run-modes)
- [Configuration Overview](#config-overview)
- [Optional: Hybrid Memory (SQLite + zvec sidecar)](#hybrid-memory)
- [HTTP API](#http-api)
- [MCP Integration](#mcp-integration)
- [Skills Integration](#skills-integration)
- [Plugin Extensions (Provider / Channel)](#plugin-system)
- [Local Development](#local-dev)
- [Engineering Quality Gates](#engineering-quality)
- [Performance Smoke Test](#perf-smoke)
- [Security Notes (Before Open Source)](#security)

<a id="features"></a>
## Features

- OpenAI-compatible provider abstraction (MiniMax/OpenAI/other compatible APIs)
- Message channels: HTTP + Telegram
- Telegram mode: `polling` only
- Telegram streaming replies (incremental updates via `editMessageText`)
- Telegram replies rendered in `MarkdownV2` (supports bold/italic/code/link/list/quote formatting)
- Telegram streaming prefers `sendMessageDraft` when supported, with automatic fallback
- Telegram typing heartbeat: immediate `sendChatAction("typing")`, then every 5 seconds until completion
- Telegram startup online status (via `setMyShortDescription`, configurable)
- Telegram slash commands: `/start`, `/help`, `/whoami`, `/mcp ...`, `/skills ...` (admin/private-chat control)
- Telegram group support:
  - `strict` mode: only replies on `@bot_username` or reply-to-bot
  - `smart` mode: contextual trigger with `Respond/ObserveOnly/Ignore`
- In groups, if a user replies to a bot message, bot replies in quoted-thread mode (`reply_to_message_id`)
- Group session routing uses `message_thread_id` (topics) or `reply_to_message_id`
- Telegram long replies are auto-split when text exceeds 4096 characters
- Telegram strips `<think>...</think>` and sends final body only
- Memory backends: `sqlite-only` (default) and `hybrid-sqlite-zvec` (optional)
- Plugin-style extension API for Provider/Channel/Memory
- Agent MCP auto tool loop (configurable max iterations/result size)
- Agent Skills runtime injection (configurable selection budget/match threshold)

<a id="quick-start"></a>
## Quick Start (Recommended: MiniMax + Telegram)

### 1) Requirements

- Rust (stable recommended)
- Telegram bot token (from `@BotFather`)
- MiniMax API key (OpenAI-compatible endpoint)

### 2) Prepare environment file

```bash
cp .env.realtest.example .env.realtest
```

Edit `.env.realtest` and fill at least:

- `MINIMAX_API_KEY`
- `TELEGRAM_BOT_TOKEN`

Optional model override:

- `MINIMAX_MODEL` (default: `MiniMax-M2.5-highspeed`)
- `TELEGRAM_BOT_USERNAME` (without `@`, recommended for group mention matching)
- `TELEGRAM_ADMIN_USER_IDS` (comma-separated Telegram user IDs for private chat access and `/mcp` + `/skills`, e.g. `123456789,987654321`)

You can also skip this step first and configure from the visual setup page after boot: `http://127.0.0.1:8080/setup`.

### 3) Start MVP in one command

```bash
./scripts/run_mvp_minimax_telegram.sh
```

Development hot reload mode (auto rebuild + restart on code change):

```bash
./scripts/run_mvp_minimax_telegram.sh --hot-reload
```

This script uses:

- config: `config/xiaomaolv.minimax-telegram.toml`
- database: `sqlite://xiaomaolv.db`
- Telegram mode: `polling` (no public URL required)

### 4) Verify service

```bash
curl -sS http://127.0.0.1:8080/health
curl -sS http://127.0.0.1:8080/v1/channels/telegram/mode
```

Then send a message to your Telegram bot.

<a id="docs-index"></a>
## Documentation Index

After MVP is running, use these docs in order:

- `docs/real-test-minimax-telegram.md`: real MiniMax + Telegram integration guide
- `docs/zvec-sidecar.md`: zvec sidecar protocol, startup, compatibility details
- `docs/mcp-integration.md`: MCP install model, CLI usage, HTTP tool-call API
- `docs/engineering-quality.md`: architecture constraints, quality gates, and performance baselines
- `config/xiaomaolv.minimax-telegram.toml`: recommended MVP config
- `config/xiaomaolv.example.toml`: generic template for custom provider/channel setups
- `scripts/perf_smoke.sh`: machine sizing smoke test script
- `scripts/debug_telegram_polling.sh`: one-command Telegram polling diagnostics (`getMe/getUpdates`)

<a id="run-modes"></a>
## Runtime Modes

### Telegram `polling`

- No public URL required
- Service pulls updates via Telegram `getUpdates`
- Startup calls `deleteWebhook` once to clear historical webhook settings and avoid polling conflicts

<a id="config-overview"></a>
## Configuration Overview

Default template files:

- `config/xiaomaolv.example.toml` (generic template)
- `config/xiaomaolv.minimax-telegram.toml` (MVP template)

Key settings:

- Provider config: `[providers.<name>]`
- Default provider: `[app].default_provider`
- System locale: `[app].locale = "${XIAOMAOLV_LOCALE:-en-US}"` (`en-US | zh-CN | hi-IN | es-ES | ar`)
- MiniMax model (MVP template): `model = "${MINIMAX_MODEL:-MiniMax-M2.5-highspeed}"`
- Telegram streaming:
  - `streaming_enabled = true`
  - `streaming_edit_interval_ms = 900`
  - `streaming_prefer_draft = true` (fallback to `sendMessage` + `editMessageText` on unsupported chats/bots)
  - `bot_username = "your_bot_username"` (used for mention matching and group decision context)
  - `group_trigger_mode = "${TELEGRAM_GROUP_TRIGGER_MODE:-smart}"` (`strict`: mention/reply only; `smart`: contextual auto-trigger; default `smart`)
  - `group_followup_window_secs = 180` (recent bot context window in smart mode)
  - `group_cooldown_secs = 20` (autonomous response cooldown in smart mode)
  - `group_rule_min_score = 70` (rule threshold for smart mode response)
  - `smart` mode learns summon aliases from chat context automatically (no manual alias config needed)
  - `group_llm_gate_enabled = false` (reserved switch for future gray-zone arbitration)
  - Scheduler:
    - `scheduler_enabled = true`
    - `scheduler_tick_secs = 2`
    - `scheduler_batch_size = 8`
    - `scheduler_lease_secs = 30`
    - `scheduler_default_timezone = "${TELEGRAM_SCHEDULER_DEFAULT_TIMEZONE:-Asia/Shanghai}"`
    - `scheduler_nl_enabled = true`
    - `scheduler_nl_min_confidence = 0.78`
    - `scheduler_require_confirm = true`
    - `scheduler_max_jobs_per_owner = 64`
    - admin commands in private chat: `/task list|add|every|pause|resume|del`
    - natural-language scheduling in private chat is enabled when `scheduler_nl_enabled=true`
    - with `scheduler_require_confirm=true`, bot sends a draft and waits for `确认` / `取消`
    - natural-language task control also supports pause/resume/delete with task id (for example: `暂停任务 task-...`)
  - `startup_online_enabled = true|false` (set Telegram bot startup status text)
  - `startup_online_text = "online"` (uses Telegram `setMyShortDescription`)
  - `commands_enabled = true|false` (enable slash command handling)
  - `commands_auto_register = true|false` (startup calls Telegram `setMyCommands`)
  - `commands_private_only = true|false` (`/mcp` + `/skills` only in private chat when true)
  - `admin_user_ids = "${TELEGRAM_ADMIN_USER_IDS:-}"` (private chat allowlist + `/mcp` + `/skills` allowlist; recommended in `.env.realtest`)
- Memory mode:
  - `backend = "sqlite-only"` (default)
  - `backend = "hybrid-sqlite-zvec"` (optional)
  - `context_window_tokens = 200000` and `context_reserved_tokens = 8192`
    - context budget guardrail: keeps useful context while reserving output room
  - `hybrid_keyword_enabled = true`
  - `hybrid_keyword_topk = 8`
  - `hybrid_keyword_candidate_limit = 256`
  - `hybrid_memory_snippet_max_chars = 420`
  - `hybrid_min_score = 0.18` (minimum relevance threshold for injected memory)
  - `context_memory_budget_ratio = 35` (max % of input budget for memory snippets)
  - `context_min_recent_messages = 8` (always keep recent turns for coherence)
- Agent MCP:
  - `mcp_enabled = true`
  - `mcp_max_iterations = 4`
  - `mcp_max_tool_result_chars = 4000`
- Agent Skills:
  - `skills_enabled = true`
  - `skills_max_selected = 3`
  - `skills_max_prompt_chars = 8000`
  - `skills_match_min_score = 0.45`
  - `skills_llm_rerank_enabled = false`
- Agent Code Mode (safe-by-default scaffold):
  - `code_mode.enabled = false`
  - `code_mode.shadow_mode = true`
  - `code_mode.max_calls = 6`
  - `code_mode.max_parallel = 2`
  - `code_mode.max_runtime_ms = 2500`
  - `code_mode.max_call_timeout_ms = 1200`
  - `code_mode.timeout_warn_ratio = 0.4`
  - `code_mode.timeout_auto_shadow_enabled = false`
  - `code_mode.timeout_auto_shadow_probe_every = 5`
  - `code_mode.timeout_auto_shadow_streak = 3`
  - `code_mode.max_result_chars = 12000`
  - `code_mode.execution_mode = "local"` (`local|subprocess`)
  - `code_mode.subprocess_timeout_secs = 8`
  - `code_mode.allow_network = false`
  - `code_mode.allow_filesystem = false`
  - `code_mode.allow_env = false`

<a id="hybrid-memory"></a>
## Optional: Hybrid Memory (SQLite + zvec sidecar)

### One-command mode

```bash
./scripts/run_mvp_minimax_telegram.sh --hybrid-memory
```

Hybrid memory + hot reload:

```bash
./scripts/run_mvp_minimax_telegram.sh --hybrid-memory --hot-reload
```

### Manual mode

```bash
./scripts/run_zvec_sidecar.sh
```

Details: `docs/zvec-sidecar.md`

<a id="http-api"></a>
## HTTP API

Core endpoints:

- `GET /health`
- `GET /setup` (visual setup page)
- `GET /v1/config/ui/state` (read setup page state)
- `POST /v1/config/ui/save` (save and hot-reload config)
- `POST /v1/messages`
- `GET /v1/code-mode/diag` (requires `Authorization: Bearer <channels.http.diag_bearer_token>`)
- `GET /v1/code-mode/metrics` (Prometheus format, same bearer token as diag)
- `GET /v1/channels/{channel}/mode`
- `GET /v1/channels/{channel}/diag`
- `POST /v1/channels/{channel}/inbound`
- `POST /v1/channels/{channel}/inbound/{secret}`

Example:

```bash
curl -X POST http://127.0.0.1:8080/v1/messages \
  -H 'content-type: application/json' \
  -d '{"session_id":"demo-1","user_id":"u1","text":"hello"}'

curl http://127.0.0.1:8080/v1/code-mode/diag \
  -H 'authorization: Bearer YOUR_HTTP_DIAG_BEARER_TOKEN'

curl http://127.0.0.1:8080/v1/code-mode/metrics \
  -H 'authorization: Bearer YOUR_HTTP_DIAG_BEARER_TOKEN'
```

`/v1/code-mode/diag` includes both current breaker state and cumulative counters
(`attempts_total`, `fallback_total`, `timed_out_calls_total`, `circuit_open_total`, etc.)
for alerting and dashboards.
Both endpoints are rate limited by `channels.http.diag_rate_limit_per_minute`.

Prometheus scraping + alerting examples: `docs/code-mode-observability.md`.

<a id="mcp-integration"></a>
## MCP Integration

xiaomaolv supports MCP server management with a Claude-style install habit and exposes MCP runtime APIs.

### CLI install/manage

Claude-style stdio install (recommended):

```bash
xiaomaolv mcp add tavily --scope user -- npx -y @tavily/mcp-server
```

Classic explicit flags (still supported):

```bash
xiaomaolv mcp add brave --scope user --command npx --arg -y --arg @brave/mcp-server
```

HTTP transport install:

```bash
xiaomaolv mcp add internal-http --transport http --url http://127.0.0.1:8787/mcp \
  --header "Authorization=Bearer YOUR_TOKEN"
```

Other operations:

```bash
xiaomaolv mcp ls --scope merged
xiaomaolv mcp test tavily
xiaomaolv mcp rm tavily --scope all
```

### Config locations

- User scope: `$XDG_CONFIG_HOME/xiaomaolv/mcp.toml` (macOS default: `~/Library/Application Support/xiaomaolv/mcp.toml`)
- Project scope: `./.xiaomaolv/mcp.toml`
- Override env:
  - `XIAOMAOLV_MCP_USER_CONFIG`
  - `XIAOMAOLV_MCP_PROJECT_CONFIG`

Merged priority: `project > user`.

### Runtime MCP HTTP endpoints

- `GET /v1/mcp/servers`
- `GET /v1/mcp/tools?server=<optional_server_name>`
- `POST /v1/mcp/tools/{server}/{tool}`

Example call:

```bash
curl -X POST http://127.0.0.1:8080/v1/mcp/tools/internal-http/search \
  -H 'content-type: application/json' \
  -d '{"query":"rust async mcp"}'
```

### Telegram `/mcp` commands

When Telegram command handling is enabled, bot supports:

- `/start`
- `/help`
- `/whoami`
- `/mcp ls [--scope merged|user|project]`
- `/mcp test <name>`
- `/mcp rm <name> [--scope all|user|project]`
- `/mcp add <name> ...` (same flags as CLI, including `-- <cmd> <args...>`)

Private chat is restricted to allowed admin users (`TELEGRAM_ADMIN_USER_IDS`).
`/mcp` also requires admin allowlist and private chat by default.
`/whoami` returns the current Telegram user ID and a ready-to-copy env config snippet.
After successful `/mcp add` or `/mcp rm`, MCP runtime is hot reloaded immediately.

## Skills Integration

xiaomaolv supports installing and managing local/catalog skills with user/project scopes.

### CLI skills commands

```bash
xiaomaolv skills ls --scope merged
xiaomaolv skills search "browser automation" --top 5 --index all
xiaomaolv skills install /path/to/my-skill --scope user --mode semantic
xiaomaolv skills install agent-browser --scope user --mode semantic --yes
xiaomaolv skills use agent-browser --scope all --mode always
xiaomaolv skills update agent-browser --scope all --latest
xiaomaolv skills rm agent-browser --scope all
```

Team catalog can be configured via `XIAOMAOLV_SKILLS_TEAM_INDEX_URL` (JSON index URL).

### Telegram `/skills` commands

- `/skills ls [--scope merged|user|project]`
- `/skills search <query> [--top 5] [--index official|team|all]`
- `/skills install <path|id|query> [--scope user|project] [--mode semantic|always|off] [--yes]`
- `/skills use <id> --mode semantic|always|off [--scope all|user|project]`
- `/skills update <id> [--scope all|user|project] [--latest|--to-version <v>]`
- `/skills rm <id> [--scope all|user|project]`

Private chat is restricted to allowed admin users (`TELEGRAM_ADMIN_USER_IDS`), same as `/mcp`.
After successful `/skills install|use|update|rm`, skills runtime is hot reloaded immediately.

### Agent auto tool loop

When `agent.mcp_enabled = true`, message handling will:

1. discover available MCP tools from configured servers  
2. ask model to emit strict JSON for tool calls  
3. execute MCP tools and feed results back to model  
4. stop at final answer or `mcp_max_iterations`

You can disable this globally:

```toml
[agent]
mcp_enabled = false
mcp_max_iterations = 4
mcp_max_tool_result_chars = 4000
skills_enabled = true
skills_max_selected = 3
skills_max_prompt_chars = 8000
skills_match_min_score = 0.45
skills_llm_rerank_enabled = false

[agent.code_mode]
enabled = false
shadow_mode = true
max_calls = 6
max_parallel = 2
max_runtime_ms = 2500
max_call_timeout_ms = 1200
timeout_warn_ratio = 0.4
timeout_auto_shadow_enabled = false
timeout_auto_shadow_probe_every = 5
timeout_auto_shadow_streak = 3
max_result_chars = 12000
execution_mode = "local" # local | subprocess
subprocess_timeout_secs = 8
allow_network = false
allow_filesystem = false
allow_env = false
```

<a id="plugin-system"></a>
## Plugin Extensions (Provider / Channel)

You can extend xiaomaolv like a classic plugin system:

- Provider: implement `ProviderFactory`, register in `ProviderRegistry`
- Channel: implement `ChannelFactory`, register in `ChannelRegistry`
- Custom router boot: `build_router_with_registries(...)`
- Runtime with background workers: `build_app_runtime_with_registries(...)`

Reference tests:

- `tests/provider_plugin_api.rs`
- `tests/channel_plugin_api.rs`
- `tests/telegram_channel_mode.rs`

<a id="local-dev"></a>
## Local Development

```bash
cargo fmt --all
cargo test -- --nocapture
```

<a id="engineering-quality"></a>
## Engineering Quality Gates

Before merging or releasing, run:

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Production build profile:

- `codegen-units = 1`
- `lto = "thin"`
- `opt-level = 3`
- `strip = "symbols"`

This is configured in `Cargo.toml` under `[profile.release]`.

<a id="perf-smoke"></a>
## Performance Smoke Test

Quickly estimate whether a machine can run xiaomaolv reliably:

```bash
./scripts/perf_smoke.sh
```

The script will:

- start a local mock provider (no real AI key needed)
- start `xiaomaolv` in release mode
- benchmark `/health` and `/v1/messages`
- print throughput, latency, failure rate, and machine sizing hints

Benchmark an already running service:

```bash
./scripts/perf_smoke.sh --running http://127.0.0.1:8080
```

Customize load:

```bash
MSG_C=32 MSG_N=2000 HEALTH_C=200 ./scripts/perf_smoke.sh
```

<a id="security"></a>
## Security Notes (Before Open Source)

- Never commit real secrets (for example `.env.realtest`)
- Keep `.env.realtest.example` as your safe template
- If any key appeared in local git history, rotate it before publishing
