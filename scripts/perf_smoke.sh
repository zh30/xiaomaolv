#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

MODE="self-contained" # self-contained | running
TARGET_URL="http://127.0.0.1:8080"
APP_PORT=18081
MOCK_PORT=18082
SKIP_BUILD=0

HEALTH_N="${HEALTH_N:-5000}"
HEALTH_C="${HEALTH_C:-100}"
MSG_N="${MSG_N:-1000}"
MSG_C="${MSG_C:-16}"

MIN_HEALTH_RPS="${MIN_HEALTH_RPS:-1000}"
MIN_MSG_RPS="${MIN_MSG_RPS:-80}"

TMP_DIR="${TMPDIR:-/tmp}/xiaomaolv-perf-smoke.$$"
APP_PID=""
MOCK_PID=""

usage() {
  cat <<'EOF'
Usage:
  ./scripts/perf_smoke.sh [options]

Options:
  --self-contained            Run with local mock provider (default)
  --running URL               Benchmark an already running xiaomaolv service
  --app-port PORT             App port in self-contained mode (default: 18081)
  --mock-port PORT            Mock provider port (default: 18082)
  --skip-build                Skip `cargo build --release` in self-contained mode
  -h, --help                  Show help

Environment overrides:
  HEALTH_N, HEALTH_C          ab params for /health (default: 5000, 100)
  MSG_N, MSG_C                ab params for /v1/messages (default: 1000, 16)
  MIN_HEALTH_RPS              Pass threshold for /health rps (default: 1000)
  MIN_MSG_RPS                 Pass threshold for /v1/messages rps (default: 80)

Examples:
  ./scripts/perf_smoke.sh
  ./scripts/perf_smoke.sh --running http://127.0.0.1:8080
  MSG_C=32 MSG_N=2000 ./scripts/perf_smoke.sh
EOF
}

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "[perf] missing command: $1" >&2
    exit 2
  fi
}

cleanup() {
  if [[ -n "${APP_PID}" ]]; then
    kill "${APP_PID}" >/dev/null 2>&1 || true
    wait "${APP_PID}" >/dev/null 2>&1 || true
  fi
  if [[ -n "${MOCK_PID}" ]]; then
    kill "${MOCK_PID}" >/dev/null 2>&1 || true
    wait "${MOCK_PID}" >/dev/null 2>&1 || true
  fi
  rm -rf "${TMP_DIR}" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

while [[ $# -gt 0 ]]; do
  case "$1" in
    --self-contained)
      MODE="self-contained"
      shift
      ;;
    --running)
      MODE="running"
      TARGET_URL="${2:-}"
      if [[ -z "${TARGET_URL}" ]]; then
        echo "[perf] --running requires URL" >&2
        exit 2
      fi
      shift 2
      ;;
    --app-port)
      APP_PORT="${2:-}"
      shift 2
      ;;
    --mock-port)
      MOCK_PORT="${2:-}"
      shift 2
      ;;
    --skip-build)
      SKIP_BUILD=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "[perf] unknown argument: $1" >&2
      usage
      exit 2
      ;;
  esac
done

require_cmd curl
require_cmd ab

mkdir -p "${TMP_DIR}"

extract_metric_or_default() {
  local raw="$1"
  local pattern="$2"
  local field="$3"
  local default_value="$4"
  local value
  value="$(printf '%s\n' "${raw}" | awk -v p="${pattern}" -v f="${field}" '$0 ~ p {print $f; exit}')"
  if [[ -z "${value}" ]]; then
    printf '%s\n' "${default_value}"
  else
    printf '%s\n' "${value}"
  fi
}

wait_health() {
  local base_url="$1"
  local attempts=120
  local i
  for i in $(seq 1 "${attempts}"); do
    if curl -fsS "${base_url}/health" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

if [[ "${MODE}" == "self-contained" ]]; then
  require_cmd python3

  cat > "${TMP_DIR}/mock_openai.py" <<'PY'
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

class H(BaseHTTPRequestHandler):
    def do_POST(self):
        if self.path != "/v1/chat/completions":
            self.send_response(404)
            self.end_headers()
            return
        _ = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        payload = {"choices": [{"message": {"content": "ok from mock provider"}}]}
        body = json.dumps(payload).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format, *args):
        return

ThreadingHTTPServer(("127.0.0.1", int(__import__("os").environ["MOCK_PORT"])), H).serve_forever()
PY

  cat > "${TMP_DIR}/config.toml" <<TOML
[app]
bind = "127.0.0.1:${APP_PORT}"
default_provider = "mock"
max_history = 16
concurrency_limit = 128

[providers.mock]
kind = "openai-compatible"
base_url = "http://127.0.0.1:${MOCK_PORT}/v1"
api_key = "local"
model = "mock"
timeout_secs = 10
max_retries = 1

[channels.http]
enabled = true

[memory]
backend = "sqlite-only"
TOML

  cat > "${TMP_DIR}/body.json" <<'JSON'
{"session_id":"perf-smoke-1","user_id":"u1","text":"你好"}
JSON

  if [[ "${SKIP_BUILD}" -eq 0 ]]; then
    echo "[perf] building release binary..."
    (cd "${ROOT_DIR}" && cargo build --release >/dev/null)
  fi

  if [[ ! -x "${ROOT_DIR}/target/release/xiaomaolv" ]]; then
    echo "[perf] release binary missing: target/release/xiaomaolv" >&2
    exit 2
  fi

  echo "[perf] starting mock provider on :${MOCK_PORT}"
  MOCK_PORT="${MOCK_PORT}" python3 "${TMP_DIR}/mock_openai.py" >"${TMP_DIR}/mock.log" 2>&1 &
  MOCK_PID=$!

  TARGET_URL="http://127.0.0.1:${APP_PORT}"
  echo "[perf] starting xiaomaolv on ${TARGET_URL}"
  "${ROOT_DIR}/target/release/xiaomaolv" serve \
    --config "${TMP_DIR}/config.toml" \
    --database "sqlite://${TMP_DIR}/perf.db" >"${TMP_DIR}/app.log" 2>&1 &
  APP_PID=$!

  if ! wait_health "${TARGET_URL}"; then
    echo "[perf] app failed to become healthy" >&2
    tail -n 60 "${TMP_DIR}/app.log" >&2 || true
    exit 1
  fi
else
  if ! wait_health "${TARGET_URL%/}"; then
    echo "[perf] target is not healthy: ${TARGET_URL}/health" >&2
    exit 1
  fi

  cat > "${TMP_DIR}/body.json" <<'JSON'
{"session_id":"perf-smoke-running","user_id":"u1","text":"你好"}
JSON
fi

BASE_URL="${TARGET_URL%/}"
echo "[perf] target=${BASE_URL}"
echo "[perf] health test: n=${HEALTH_N}, c=${HEALTH_C}"
HEALTH_OUT="$(ab -n "${HEALTH_N}" -c "${HEALTH_C}" "${BASE_URL}/health" 2>&1)"

echo "[perf] messages test: n=${MSG_N}, c=${MSG_C}"
MSG_OUT="$(ab -l -n "${MSG_N}" -c "${MSG_C}" -p "${TMP_DIR}/body.json" -T application/json "${BASE_URL}/v1/messages" 2>&1)"

RSS_KB="N/A"
if [[ -n "${APP_PID}" ]]; then
  RSS_KB="$(ps -o rss= -p "${APP_PID}" | tr -d ' ' || true)"
fi

HEALTH_RPS="$(extract_metric_or_default "${HEALTH_OUT}" "Requests per second" 4 "0")"
HEALTH_FAILED="$(extract_metric_or_default "${HEALTH_OUT}" "Failed requests" 3 "0")"
HEALTH_P95="$(extract_metric_or_default "${HEALTH_OUT}" "^ *95%" 2 "N/A")"

MSG_RPS="$(extract_metric_or_default "${MSG_OUT}" "Requests per second" 4 "0")"
MSG_FAILED="$(extract_metric_or_default "${MSG_OUT}" "Failed requests" 3 "0")"
MSG_NON2XX="$(extract_metric_or_default "${MSG_OUT}" "Non-2xx responses" 3 "0")"
MSG_P95="$(extract_metric_or_default "${MSG_OUT}" "^ *95%" 2 "N/A")"
MSG_P99="$(extract_metric_or_default "${MSG_OUT}" "^ *99%" 2 "N/A")"

echo
echo "========== xiaomaolv perf smoke =========="
echo "mode:                ${MODE}"
echo "target:              ${BASE_URL}"
echo "rss_kb:              ${RSS_KB}"
echo
echo "[health]"
echo "  requests_per_sec:  ${HEALTH_RPS}"
echo "  failed_requests:   ${HEALTH_FAILED}"
echo "  p95_ms:            ${HEALTH_P95}"
echo
echo "[messages]"
echo "  requests_per_sec:  ${MSG_RPS}"
echo "  failed_requests:   ${MSG_FAILED}"
echo "  non_2xx:           ${MSG_NON2XX}"
echo "  p95_ms:            ${MSG_P95}"
echo "  p99_ms:            ${MSG_P99}"
echo "=========================================="
echo

HEALTH_RPS_OK="$(awk -v v="${HEALTH_RPS}" -v t="${MIN_HEALTH_RPS}" 'BEGIN{print (v+0 >= t+0) ? "1" : "0"}')"
MSG_RPS_OK="$(awk -v v="${MSG_RPS}" -v t="${MIN_MSG_RPS}" 'BEGIN{print (v+0 >= t+0) ? "1" : "0"}')"

if [[ "${HEALTH_FAILED}" != "0" || "${MSG_FAILED}" != "0" || "${MSG_NON2XX}" != "0" ]]; then
  echo "[perf] verdict: FAIL (request failures detected)" >&2
  exit 1
fi

if [[ "${HEALTH_RPS_OK}" != "1" || "${MSG_RPS_OK}" != "1" ]]; then
  echo "[perf] verdict: WARN (stable but below threshold)" >&2
  echo "[perf] hint: lower app.concurrency_limit to 16 on low-end machines, and keep memory.backend=sqlite-only" >&2
  exit 0
fi

TIER="$(awk -v r="${MSG_RPS}" 'BEGIN{
  if (r+0 >= 500) print "recommended: >= 1 vCPU + 1GB RAM";
  else if (r+0 >= 150) print "minimum: >= 1 vCPU + 512MB RAM";
  else print "conservative: >= 2 vCPU + 2GB RAM";
}')"

echo "[perf] verdict: PASS"
echo "[perf] ${TIER}"
