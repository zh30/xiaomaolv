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
- `<think>...</think>` auto-rendered as collapsible Telegram spoiler content
- Memory backends: `sqlite-only` (default) and `hybrid-sqlite-zvec` (optional)
- Plugin-style extension API for Provider/Channel/Memory

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

### 3) Start MVP in one command

```bash
./scripts/run_mvp_minimax_telegram.sh
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
- `config/xiaomaolv.minimax-telegram.toml`: recommended MVP config
- `config/xiaomaolv.example.toml`: generic template for custom provider/channel setups
- `scripts/perf_smoke.sh`: machine sizing smoke test script

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
mode = "webhook"
webhook_secret = "${TELEGRAM_WEBHOOK_SECRET}"
streaming_enabled = true
streaming_edit_interval_ms = 900
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
- Telegram streaming:
  - `streaming_enabled = true`
  - `streaming_edit_interval_ms = 900`
- Memory mode:
  - `backend = "sqlite-only"` (default)
  - `backend = "hybrid-sqlite-zvec"` (optional)

<a id="hybrid-memory"></a>
## Optional: Hybrid Memory (SQLite + zvec sidecar)

### One-command mode

```bash
./scripts/run_mvp_minimax_telegram.sh --hybrid-memory
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
- `POST /v1/channels/{channel}/inbound`
- `POST /v1/channels/{channel}/inbound/{secret}`
- `POST /v1/telegram/webhook/{secret}`

Example:

```bash
curl -X POST http://127.0.0.1:8080/v1/messages \
  -H 'content-type: application/json' \
  -d '{"session_id":"demo-1","user_id":"u1","text":"hello"}'
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
