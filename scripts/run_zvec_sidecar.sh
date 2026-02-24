#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

python3 -m venv "${ROOT_DIR}/.venv-zvec-sidecar"
source "${ROOT_DIR}/.venv-zvec-sidecar/bin/activate"
pip install -r "${ROOT_DIR}/scripts/requirements-zvec-sidecar.txt"

export ZVEC_PATH="${ZVEC_PATH:-${ROOT_DIR}/data/zvec_memory}"
export ZVEC_COLLECTION="${ZVEC_COLLECTION:-agent_memory_v1}"
export ZVEC_EMBED_DIM="${ZVEC_EMBED_DIM:-256}"
export ZVEC_API_TOKEN="${ZVEC_API_TOKEN:-${ZVEC_SIDECAR_TOKEN:-}}"

exec uvicorn zvec_sidecar:app --app-dir "${ROOT_DIR}/scripts" --host 127.0.0.1 --port 3711
