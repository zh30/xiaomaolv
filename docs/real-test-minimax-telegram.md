# Real Test Guide: MiniMax + Telegram

This project now follows xiaomaolv-style Telegram behavior:

- Default mode is `polling`.
- In polling mode, you do **not** need a public base URL.
- Webhook mode is optional and requires public HTTPS + `webhook_secret`.
- Telegram reply supports streaming by default (`streaming_enabled = true`).
- When provider output contains `<think>...</think>`, Telegram will show it as click-to-expand spoiler content.

## 1. Prepare env file

```bash
cd /Users/henry/code/xiaomaolv
cp .env.realtest.example .env.realtest
```

Fill these required values for polling mode:

- `MINIMAX_API_KEY`
- `TELEGRAM_BOT_TOKEN`

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

## 5. Optional local HTTP channel test

```bash
curl -X POST http://127.0.0.1:8080/v1/messages \
  -H 'content-type: application/json' \
  -d '{"session_id":"realtest-1","user_id":"u1","text":"你好，做个自我介绍"}'
```

---

## Optional: Webhook mode (requires public URL)

If you want webhook mode:

1. In `config/xiaomaolv.minimax-telegram.toml`, set:

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
- MiniMax OpenAI compatibility supports models like `MiniMax-M2.5`, `MiniMax-M2.1`, `MiniMax-M2`.
