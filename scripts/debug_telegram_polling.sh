#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

ENV_FILE="${ROOT_DIR}/.env.realtest"
if [[ ! -f "${ENV_FILE}" ]]; then
  echo "[debug] missing ${ENV_FILE}; create it from .env.realtest.example first" >&2
  exit 1
fi

set -a
source "${ENV_FILE}"
set +a

if [[ -z "${TELEGRAM_BOT_TOKEN:-}" || "${TELEGRAM_BOT_TOKEN}" == replace_with_* ]]; then
  echo "[debug] TELEGRAM_BOT_TOKEN is missing in ${ENV_FILE}" >&2
  exit 1
fi

API_BASE="${TELEGRAM_API_BASE_URL:-https://api.telegram.org}"
API_BASE="${API_BASE%/}"
BOT_API="${API_BASE}/bot${TELEGRAM_BOT_TOKEN}"

echo "[debug] telegram api base: ${API_BASE}"
echo "[debug] step 1/4: reachability check"
if curl -sS --max-time 10 "${API_BASE}" >/dev/null; then
  echo "[ok] reachable: ${API_BASE}"
else
  echo "[fail] cannot reach ${API_BASE} (network/proxy/firewall issue likely)"
fi

echo
echo "[debug] step 2/4: getMe"
GETME_RAW="$(curl -sS --max-time 15 -X POST "${BOT_API}/getMe" || true)"
echo "${GETME_RAW}"
if [[ "${GETME_RAW}" == *"\"can_read_all_group_messages\":false"* ]]; then
  echo "[warn] privacy mode appears enabled (can_read_all_group_messages=false)."
  echo "[warn] for group @mention flow, run @BotFather -> /setprivacy -> select your bot -> Disable."
fi
echo

echo
echo "[debug] step 3/4: getWebhookInfo"
curl -sS --max-time 15 -X POST "${BOT_API}/getWebhookInfo" || true
echo

echo
echo "[debug] step 4/4: getUpdates (short polling)"
curl -sS --max-time 20 -X POST "${BOT_API}/getUpdates" \
  -H "content-type: application/json" \
  -d '{"timeout":1,"allowed_updates":["message"]}' || true
echo

echo
echo "[debug] if getMe/getUpdates fail with connection or timeout, check outbound network to api.telegram.org."
echo "[debug] if getUpdates returns ok=false with webhook conflict, clear webhook and keep polling mode."
