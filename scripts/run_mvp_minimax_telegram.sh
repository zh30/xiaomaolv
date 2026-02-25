#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

MODE="sqlite-only"
HOT_RELOAD="0"

usage() {
  cat <<'USAGE'
Usage:
  ./scripts/run_mvp_minimax_telegram.sh [--hybrid-memory] [--hot-reload]

Options:
  --hybrid-memory   Enable hybrid memory backend (sqlite + zvec sidecar)
  --hot-reload      Watch source changes and auto-restart gateway (requires cargo-watch)
  -h, --help        Show this help message
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --hybrid-memory)
      MODE="hybrid-sqlite-zvec"
      ;;
    --hot-reload|--watch)
      HOT_RELOAD="1"
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown option: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
  shift
done

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
export MINIMAX_MODEL="${MINIMAX_MODEL:-MiniMax-M2.5-highspeed}"
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
echo "[mvp] minimax_model=${MINIMAX_MODEL}"
echo "[mvp] telegram mode check after boot: curl -sS http://127.0.0.1:8080/v1/channels/telegram/mode"

if lsof -nP -iTCP:8080 -sTCP:LISTEN >/dev/null 2>&1; then
  echo "[mvp] port 8080 is already in use. current listeners:" >&2
  lsof -nP -iTCP:8080 -sTCP:LISTEN >&2 || true
  echo "[mvp] stop the existing process (or change [app].bind) and retry." >&2
  exit 1
fi

if [[ "${HOT_RELOAD}" == "1" ]]; then
  if ! cargo watch --version >/dev/null 2>&1; then
    echo "[mvp] hot reload requires cargo-watch, but it is not installed." >&2
    echo "[mvp] install first: cargo install cargo-watch" >&2
    exit 1
  fi
  echo "[mvp] hot reload enabled (cargo watch)"
  cargo watch \
    --clear \
    -w src \
    -w config \
    -w Cargo.toml \
    -w Cargo.lock \
    -x "run -- serve --config ${RUNTIME_CONFIG} --database sqlite://xiaomaolv.db"
else
  cargo run -- serve \
    --config "${RUNTIME_CONFIG}" \
    --database sqlite://xiaomaolv.db
fi
