#!/usr/bin/env bash
# Run every test_*.py under orchestrator-py/ as a standalone script.
#
# The Python suite here intentionally avoids pytest — tests are hand-rolled
# `async def main()` scripts that exit non-zero on failure (see the module
# docstrings in test_frame_wedge.py / test_outbox.py). This runner just
# iterates them, so CI fails if any script fails.
#
# Run from anywhere: the runner cd's into its own directory before execing
# tests so relative imports (sys.path.insert at module scope) resolve.
#
# By default, reuses the vendored .venv (same one run.sh / run-worker.sh
# provision). This keeps `./run_tests.sh` self-contained: no ambient
# pip-install needed. Override with PYTHON=/path/to/python to run against a
# different interpreter (CI sets PYTHON to the workflow-managed interpreter).

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$here"

# Pick the interpreter:
#   - PYTHON env wins (CI, ad-hoc overrides)
#   - else the vendored .venv, auto-provisioned if absent
#   - else fall back to python3 (user is on their own for deps)
if [[ -n "${PYTHON:-}" ]]; then
    python_bin="$PYTHON"
else
    venv_py="$here/.venv/bin/python"
    if [[ ! -x "$venv_py" ]]; then
        echo "[run_tests] venv missing; creating $here/.venv ..." >&2
        python3 -m venv "$here/.venv"
        "$here/.venv/bin/pip" install --upgrade pip >/dev/null
        "$here/.venv/bin/pip" install -r "$here/requirements.txt" >/dev/null
    fi
    python_bin="$venv_py"
fi

# Sanity check: imports needed by the tests must resolve. If not, bail with a
# clear message instead of letting every test file die with ModuleNotFoundError.
if ! "$python_bin" -c "import claude_agent_sdk" >/dev/null 2>&1; then
    echo "[run_tests] $python_bin cannot import claude_agent_sdk." >&2
    echo "[run_tests] install with: pip install -r $here/requirements.txt" >&2
    exit 2
fi

shopt -s nullglob
tests=(test_*.py)
if [[ ${#tests[@]} -eq 0 ]]; then
    echo "no test_*.py files found in $here" >&2
    exit 1
fi

failed=()
for t in "${tests[@]}"; do
    echo "=== $t ==="
    if "$python_bin" "$t"; then
        echo "--- $t: PASS"
    else
        echo "--- $t: FAIL" >&2
        failed+=("$t")
    fi
done

if [[ ${#failed[@]} -gt 0 ]]; then
    echo >&2
    echo "FAILED: ${failed[*]}" >&2
    exit 1
fi

echo
echo "all ${#tests[@]} test script(s) passed"
