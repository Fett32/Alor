#!/usr/bin/env bash
# Launch an Alor Claude worker using the vendored venv.
# Usage: run-worker.sh <agent_id> [--workdir PATH] [--project NAME]
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VENV_PY="$HERE/.venv/bin/python"

if [[ ! -x "$VENV_PY" ]]; then
  echo "[worker] venv missing; creating..." >&2
  python3 -m venv "$HERE/.venv"
  "$HERE/.venv/bin/pip" install --upgrade pip >/dev/null
  "$HERE/.venv/bin/pip" install -r "$HERE/requirements.txt"
fi

exec "$VENV_PY" "$HERE/worker.py" "$@"
