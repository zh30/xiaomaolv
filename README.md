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
- [Plugin Extensions (Provider / Channel)](#plugin-system)
- [Local Development](#local-dev)
- [Performance Smoke Test](#perf-smoke)
- [Security Notes (Before Open Source)](#security)

<a id="features"></a>
## Features

- OpenAI-compatible provider abstraction (MiniMax/OpenAI/other compatible APIs)
- Message channels: HTTP + Telegram
- Telegram dual mode: `polling` (default) and `webhook` (optional)
- Telegram streaming replies (incremental updates via `editMessageText`)
- Telegram streaming prefers `sendMessageDraft` when supported, with automatic fallback
- Telegram typing heartbeat: immediate `sendChatAction("typing")`, then every 5 seconds until completion
- Telegram startup online status (via `setMyShortDescription`, configurable)
- Telegram slash commands: `/start`, `/help`, `/whoami`, `/mcp ...` (admin/private-chat control)
- Telegram group support: only replies when the bot is mentioned (`@bot_username`)
- In groups, if a user replies to a bot message, bot replies in quoted-thread mode (`reply_to_message_id`)
- Group session routing uses `message_thread_id` (topics) or `reply_to_message_id`
- Telegram long replies are auto-split when text exceeds 4096 characters
- `<think>...</think>` auto-rendered as collapsible Telegram spoiler content
- Memory backends: `sqlite-only` (default) and `hybrid-sqlite-zvec` (optional)
- Plugin-style extension API for Provider/Channel/Memory
- Agent MCP auto tool loop (configurable max iterations/result size)

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
- `TELEGRAM_ADMIN_USER_IDS` (comma-separated Telegram user IDs for `/mcp`, e.g. `123456789,987654321`)

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

- `docs/real-test-minimax-telegram.md`: real MiniMax + Telegram integration (webhook setup/verification included)
- `docs/zvec-sidecar.md`: zvec sidecar protocol, startup, compatibility details
- `docs/mcp-integration.md`: MCP install model, CLI usage, HTTP tool-call API
- `config/xiaomaolv.minimax-telegram.toml`: recommended MVP config
- `config/xiaomaolv.example.toml`: generic template for custom provider/channel setups
- `scripts/perf_smoke.sh`: machine sizing smoke test script
- `scripts/debug_telegram_polling.sh`: one-command Telegram polling diagnostics (`getMe/getWebhookInfo/getUpdates`)

<a id="run-modes"></a>
## Runtime Modes

### Telegram `polling` (default)

- No public URL required
- Service pulls updates via Telegram `getUpdates`
- Startup calls `deleteWebhook` to prevent webhook/polling conflicts

### Telegram `webhook` (optional)

Recommended for public production deployments.

1. Enable webhook in config:

```toml
[channels.telegram]
enabled = true
bot_token = "${TELEGRAM_BOT_TOKEN}"
bot_username = "${TELEGRAM_BOT_USERNAME:-}"
mode = "webhook"
webhook_secret = "${TELEGRAM_WEBHOOK_SECRET}"
streaming_enabled = true
streaming_edit_interval_ms = 900
streaming_prefer_draft = true
startup_online_enabled = true
startup_online_text = "${TELEGRAM_STARTUP_ONLINE_TEXT:-online}"
commands_enabled = true
commands_auto_register = true
commands_private_only = true
admin_user_ids = "${TELEGRAM_ADMIN_USER_IDS:-}"
```

2. Fill in `.env.realtest`:

- `TELEGRAM_WEBHOOK_SECRET`
- `PUBLIC_BASE_URL` (public HTTPS URL)

3. Register webhook:

```bash
set -a
source .env.realtest
set +a
./scripts/set_telegram_webhook.sh
```

Webhook endpoint:

`POST /v1/telegram/webhook/{webhook_secret}`

<a id="config-overview"></a>
## Configuration Overview

Default template files:

- `config/xiaomaolv.example.toml` (generic template)
- `config/xiaomaolv.minimax-telegram.toml` (MVP template)

Key settings:

- Provider config: `[providers.<name>]`
- Default provider: `[app].default_provider`
- MiniMax model (MVP template): `model = "${MINIMAX_MODEL:-MiniMax-M2.5-highspeed}"`
- Telegram streaming:
  - `streaming_enabled = true`
  - `streaming_edit_interval_ms = 900`
  - `streaming_prefer_draft = true` (fallback to `sendMessage` + `editMessageText` on unsupported chats/bots)
  - `bot_username = "your_bot_username"` (group mode: only reply when mentioned)
  - `startup_online_enabled = true|false` (set Telegram bot startup status text)
  - `startup_online_text = "online"` (uses Telegram `setMyShortDescription`)
  - `commands_enabled = true|false` (enable slash command handling)
  - `commands_auto_register = true|false` (startup calls Telegram `setMyCommands`)
  - `commands_private_only = true|false` (`/mcp` only in private chat when true)
  - `admin_user_ids = "${TELEGRAM_ADMIN_USER_IDS:-}"` (recommended: configure in `.env.realtest`)
- Memory mode:
  - `backend = "sqlite-only"` (default)
  - `backend = "hybrid-sqlite-zvec"` (optional)
- Agent MCP:
  - `mcp_enabled = true`
  - `mcp_max_iterations = 4`
  - `mcp_max_tool_result_chars = 4000`

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
- `POST /v1/messages`
- `GET /v1/channels/{channel}/mode`
- `GET /v1/channels/{channel}/diag`
- `POST /v1/channels/{channel}/inbound`
- `POST /v1/channels/{channel}/inbound/{secret}`
- `POST /v1/telegram/webhook/{secret}`

Example:

```bash
curl -X POST http://127.0.0.1:8080/v1/messages \
  -H 'content-type: application/json' \
  -d '{"session_id":"demo-1","user_id":"u1","text":"hello"}'
```

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

`/mcp` runs only for allowed admin users (`TELEGRAM_ADMIN_USER_IDS`) and in private chat by default.
`/whoami` returns the current Telegram user ID and a ready-to-copy env config snippet.
After successful `/mcp add` or `/mcp rm`, MCP runtime is hot reloaded immediately.

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
