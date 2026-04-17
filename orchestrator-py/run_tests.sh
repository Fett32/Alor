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

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$here"

python_bin="${PYTHON:-python3}"

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
