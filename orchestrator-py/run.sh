#!/usr/bin/env bash
# Launch the Alor orchestrator using its vendored venv.
# Runs from the script's directory regardless of cwd.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VENV_PY="$HERE/.venv/bin/python"

if [[ ! -x "$VENV_PY" ]]; then
  echo "[orchestrator] venv missing; creating..." >&2
  python3 -m venv "$HERE/.venv"
  "$HERE/.venv/bin/pip" install --upgrade pip >/dev/null
  "$HERE/.venv/bin/pip" install -r "$HERE/requirements.txt"
fi

exec "$VENV_PY" "$HERE/main.py" "$@"
