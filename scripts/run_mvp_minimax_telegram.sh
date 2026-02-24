#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

MODE="sqlite-only"
if [[ "${1:-}" == "--hybrid-memory" ]]; then
  MODE="hybrid-sqlite-zvec"
fi

ENV_FILE="${ROOT_DIR}/.env.realtest"
if [[ ! -f "${ENV_FILE}" ]]; then
  cp "${ROOT_DIR}/.env.realtest.example" "${ENV_FILE}"
  echo "created ${ENV_FILE}; fill MINIMAX_API_KEY and TELEGRAM_BOT_TOKEN first" >&2
  exit 1
fi

set -a
source "${ENV_FILE}"
set +a

if [[ -z "${MINIMAX_API_KEY:-}" || "${MINIMAX_API_KEY}" == replace_with_* ]]; then
  echo "MINIMAX_API_KEY is missing in ${ENV_FILE}" >&2
  exit 1
fi
if [[ -z "${TELEGRAM_BOT_TOKEN:-}" || "${TELEGRAM_BOT_TOKEN}" == replace_with_* ]]; then
  echo "TELEGRAM_BOT_TOKEN is missing in ${ENV_FILE}" >&2
  exit 1
fi

BASE_CONFIG="${ROOT_DIR}/config/xiaomaolv.minimax-telegram.toml"
RUNTIME_CONFIG="${ROOT_DIR}/config/.xiaomaolv.minimax-telegram.runtime.toml"

cp "${BASE_CONFIG}" "${RUNTIME_CONFIG}"
if [[ "${MODE}" == "hybrid-sqlite-zvec" ]]; then
  sed -i '' 's/backend = "sqlite-only"/backend = "hybrid-sqlite-zvec"/' "${RUNTIME_CONFIG}"
fi

SIDECAR_PID=""
cleanup() {
  if [[ -n "${SIDECAR_PID}" ]]; then
    kill "${SIDECAR_PID}" >/dev/null 2>&1 || true
    wait "${SIDECAR_PID}" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT INT TERM

if [[ "${MODE}" == "hybrid-sqlite-zvec" ]]; then
  export ZVEC_SIDECAR_ENDPOINT="${ZVEC_SIDECAR_ENDPOINT:-http://127.0.0.1:3711}"
  echo "[mvp] starting zvec sidecar at ${ZVEC_SIDECAR_ENDPOINT}"
  "${ROOT_DIR}/scripts/run_zvec_sidecar.sh" >/tmp/xiaomaolv_zvec_sidecar.log 2>&1 &
  SIDECAR_PID="$!"
  sleep 2
fi

echo "[mvp] mode=${MODE}"
echo "[mvp] config=${RUNTIME_CONFIG}"
echo "[mvp] database=sqlite://xiaomaolv.db"
echo "[mvp] telegram mode check after boot: curl -sS http://127.0.0.1:8080/v1/channels/telegram/mode"

exec cargo run -- serve \
  --config "${RUNTIME_CONFIG}" \
  --database sqlite://xiaomaolv.db
