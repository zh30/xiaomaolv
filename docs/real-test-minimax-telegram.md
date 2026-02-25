# Real Test Guide: MiniMax + Telegram

This project now follows xiaomaolv-style Telegram behavior:

- Default mode is `polling`.
- In polling mode, you do **not** need a public base URL.
- Webhook mode is optional and requires public HTTPS + `webhook_secret`.
- Telegram reply supports streaming by default (`streaming_enabled = true`).
- When provider output contains `<think>...</think>`, Telegram will show it as click-to-expand spoiler content.
- Startup can set bot profile short description to `online` (configurable).
- Group chats are supported: bot replies only when explicitly `@mentioned`.
- In groups, replying to a bot message also triggers a reply; bot response will quote the user reply message.
- In group chats, session routing uses `message_thread_id` (topic) or `reply_to_message_id`.

## 1. Prepare env file

```bash
cd /Users/henry/code/xiaomaolv
cp .env.realtest.example .env.realtest
```

Fill these required values for polling mode:

- `MINIMAX_API_KEY`
- `TELEGRAM_BOT_TOKEN`

Optional model override:

- `MINIMAX_MODEL` (default: `MiniMax-M2.5-highspeed`)
- `TELEGRAM_BOT_USERNAME` (without `@`, recommended for group mention matching)
- `TELEGRAM_STARTUP_ONLINE_TEXT` (optional, default: `online`)

## 1.5 Fastest path (recommended)

SQLite memory MVP:

```bash
./scripts/run_mvp_minimax_telegram.sh
```

Hybrid memory MVP (starts sidecar automatically):

```bash
./scripts/run_mvp_minimax_telegram.sh --hybrid-memory
```

## 2. Export env vars

```bash
set -a
source .env.realtest
set +a
```

## 3. Start gateway (polling mode)

`config/xiaomaolv.minimax-telegram.toml` is already set to polling mode.
It is also set to Telegram streaming mode by default.

```bash
cargo run -- serve \
  --config config/xiaomaolv.minimax-telegram.toml \
  --database sqlite://xiaomaolv.db
```

## Optional: Enable hybrid zvec memory (sidecar)

1. Start sidecar in another terminal:

```bash
cd /Users/henry/code/xiaomaolv
./scripts/run_zvec_sidecar.sh
```

2. In `config/xiaomaolv.minimax-telegram.toml`, set:

```toml
[memory]
backend = "hybrid-sqlite-zvec"
max_recent_turns = 0
max_semantic_memories = 8
semantic_lookback_days = 90

[memory.zvec]
endpoint = "${ZVEC_SIDECAR_ENDPOINT}"
collection = "agent_memory_v1"
query_topk = 20
request_timeout_secs = 3
upsert_path = "/v1/memory/upsert"
query_path = "/v1/memory/query"
# auth_bearer_token = "${ZVEC_SIDECAR_TOKEN}"
```

3. Ensure `.env.realtest` has:

- `ZVEC_SIDECAR_ENDPOINT=http://127.0.0.1:3711`
- Optional: `ZVEC_SIDECAR_TOKEN=...`

## 4. Send real Telegram messages

First verify current Telegram ingress mode:

```bash
curl -sS http://127.0.0.1:8080/v1/channels/telegram/mode
```

Expected output in default setup:

```json
{"channel":"telegram","mode":"polling"}
```

Directly send a message to your bot in Telegram.

The service will pull updates using `getUpdates` and reply automatically.
It also sends Telegram typing status immediately and refreshes it every 5 seconds until the reply is finished.
Startup online status uses Telegram `setMyShortDescription` and does not change typing behavior during message generation.

## 5. Optional local HTTP channel test

```bash
curl -X POST http://127.0.0.1:8080/v1/messages \
  -H 'content-type: application/json' \
  -d '{"session_id":"realtest-1","user_id":"u1","text":"你好，做个自我介绍"}'
```

## 6. Debug polling connectivity quickly

If console shows `failed to call telegram getUpdates`, run:

```bash
./scripts/debug_telegram_polling.sh
```

This checks:

- API reachability
- `getMe`
- `getWebhookInfo`
- `getUpdates`

If `getMe` shows `"can_read_all_group_messages": false`, group messages may not be delivered.
For group @mention flow, disable privacy in `@BotFather`:

1. `/setprivacy`
2. Select your bot
3. Choose `Disable`

For group mention diagnostics, start with debug log level:

```bash
RUST_LOG=info,xiaomaolv::channel=debug ./scripts/run_mvp_minimax_telegram.sh
```

Then watch for:

- `telegram group mention check` (`mentioned=true` means it should process reply path)

You can also query built-in channel diagnostics:

```bash
curl -sS http://127.0.0.1:8080/v1/channels/telegram/diag
```

It includes:

- cached bot username used for mention matching
- polling worker health (`last_poll_ok_at_unix`, `last_poll_error`, counters)
- current `getMe` result (including `can_read_all_group_messages`)
- current `getWebhookInfo` result

---

## Optional: Webhook mode (requires public URL)

If you want webhook mode:

1. In `config/xiaomaolv.minimax-telegram.toml`, set:

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
```

2. Fill in `.env.realtest`:

- `TELEGRAM_WEBHOOK_SECRET`
- `PUBLIC_BASE_URL`

3. Set Telegram webhook:

```bash
./scripts/set_telegram_webhook.sh
```

4. Verify:

```bash
curl -sS "https://api.telegram.org/bot${TELEGRAM_BOT_TOKEN}/getWebhookInfo"
curl -sS http://127.0.0.1:8080/v1/channels/telegram/mode
```

## Notes

- If your machine is in China mainland, use `https://api.minimaxi.com/v1`.
- If you later test outside mainland, MiniMax docs show international endpoint `https://api.minimax.io/v1`.
- Default model in MVP config is `MiniMax-M2.5-highspeed`.
- You can switch model without touching TOML by setting `MINIMAX_MODEL` in `.env.realtest`.
