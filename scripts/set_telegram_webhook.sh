#!/usr/bin/env bash
set -euo pipefail

if [[ -z "${TELEGRAM_BOT_TOKEN:-}" || -z "${TELEGRAM_WEBHOOK_SECRET:-}" || -z "${PUBLIC_BASE_URL:-}" ]]; then
  echo "Missing env vars. Required: TELEGRAM_BOT_TOKEN, TELEGRAM_WEBHOOK_SECRET, PUBLIC_BASE_URL" >&2
  exit 1
fi

WEBHOOK_URL="${PUBLIC_BASE_URL%/}/v1/telegram/webhook/${TELEGRAM_WEBHOOK_SECRET}"

curl -sS -X POST "https://api.telegram.org/bot${TELEGRAM_BOT_TOKEN}/setWebhook" \
  -H 'content-type: application/json' \
  -d "{\"url\":\"${WEBHOOK_URL}\"}" | tee /tmp/telegram_set_webhook_result.json

echo "\nWebhook target: ${WEBHOOK_URL}"
echo "Saved response: /tmp/telegram_set_webhook_result.json"
