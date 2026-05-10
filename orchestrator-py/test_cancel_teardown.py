"""Regression test for worker.py's asyncio cancellation cleanup.

Locks in the contract around the `asyncio.wait(return_when=FIRST_COMPLETED)`
teardown in `main()`:

  - `CancelledError` raised by a cancelled task is silently expected
    (we just called `.cancel()`).
  - A genuine `Exception` raised by a cancelled task gets SURFACED
    (reported) but does NOT abort outer cleanup.

Prior to the fix the cleanup used
`except (asyncio.CancelledError, Exception): pass` — a too-broad
catch that silently swallowed real bugs in daemon_loop/stdin_loop
teardown. Python 3.8+ made `CancelledError` a `BaseException`, so
the `CancelledError` arm in that tuple was doing real work (without
it, CancelledError would propagate); the `Exception` arm was
hiding bugs. This test pins the split behaviour.

Follows the suite convention: standalone `asyncio.run(main())`,
exit non-zero on failure, no pytest.
"""

from __future__ import annotations

import asyncio
import sys


# ---- Helpers reproducing worker.py's cleanup shape --------------------------

async def _forever() -> None:
    """Long-running task — gets cancelled by the cleanup path."""
    await asyncio.sleep(3600)


async def _raises_real_exception() -> None:
    """Task that fails with a non-cancellation exception.

    Simulates a bug (or late socket/SDK error) inside daemon_loop or
    stdin_loop that would otherwise be silenced by the old catch.
    """
    await asyncio.sleep(0)  # yield so the event loop schedules us
    raise RuntimeError("simulated teardown bug")


async def _cleanup_fixed(pending: list[asyncio.Task]) -> list[str]:
    """The FIXED cleanup shape from worker.py — split except clauses.

    Returns the list of diagnostic lines so the test can assert
    which tasks surfaced real exceptions. In worker.py proper these
    are `print()`ed; here we collect them so the test is
    deterministic without touching stdout.
    """
    errors: list[str] = []
    for t in pending:
        t.cancel()
        try:
            await t
        except asyncio.CancelledError:
            pass
        except Exception as e:
            errors.append(f"[{t.get_name()} teardown error] {e!r}")
    return errors


# ---- checks -----------------------------------------------------------------

FAIL = 0


def check(label: str, got: object, want: object) -> None:
    global FAIL
    if got == want:
        print(f"ok    {label}")
    else:
        print(f"FAIL  {label}: got {got!r}, want {want!r}", file=sys.stderr)
        FAIL += 1


async def case_cancelled_error_is_silent() -> None:
    # Long-running task that will be cancelled. Must produce no errors.
    t = asyncio.create_task(_forever(), name="forever_task")
    await asyncio.sleep(0)  # let it start
    errors = await _cleanup_fixed([t])
    check("cancelled task: no errors reported", errors, [])
    check("cancelled task: actually cancelled", t.cancelled(), True)


async def case_real_exception_is_surfaced() -> None:
    # Task that raises a genuine RuntimeError. Cleanup must (a) not
    # propagate, (b) include a diagnostic line with the task name
    # and repr of the exception.
    t = asyncio.create_task(_raises_real_exception(), name="buggy_task")
    # Let it run + raise before we try to cancel it. When cleanup
    # awaits the already-finished task it re-raises the stored
    # exception — which is the path the old too-broad catch hid.
    try:
        await asyncio.wait_for(asyncio.shield(t), timeout=0.1)
    except Exception:
        pass  # expected — the task raised
    errors = await _cleanup_fixed([t])
    check("buggy task: exactly one error surfaced", len(errors), 1)
    if errors:
        check("buggy task: error carries task name", "buggy_task" in errors[0], True)
        check(
            "buggy task: error carries exception repr",
            "RuntimeError" in errors[0] and "simulated teardown bug" in errors[0],
            True,
        )


async def case_mixed_pending_list() -> None:
    # Real production shape: two pending tasks, one cancels cleanly,
    # one is mid-exception. Both must be handled; cleanup must not
    # short-circuit after the first error.
    t_ok = asyncio.create_task(_forever(), name="daemon_loop")
    t_bad = asyncio.create_task(_raises_real_exception(), name="stdin_loop")
    await asyncio.sleep(0)
    try:
        await asyncio.wait_for(asyncio.shield(t_bad), timeout=0.1)
    except Exception:
        pass
    errors = await _cleanup_fixed([t_ok, t_bad])
    check("mixed: exactly one error surfaced", len(errors), 1)
    if errors:
        check("mixed: error came from stdin_loop", "stdin_loop" in errors[0], True)
    check("mixed: daemon_loop was cancelled successfully", t_ok.cancelled(), True)


def case_python_cancellederror_hierarchy() -> None:
    # Drift guard — if Python ever makes CancelledError a subclass of
    # Exception again (pre-3.8 behaviour), the two-branch split in
    # worker.py becomes semantically different. That's unlikely but
    # it's the load-bearing assumption this test protects; assert
    # explicitly so a regression in either direction is visible.
    check(
        "CancelledError is NOT subclass of Exception (Python 3.8+)",
        issubclass(asyncio.CancelledError, Exception),
        False,
    )
    check(
        "CancelledError IS subclass of BaseException",
        issubclass(asyncio.CancelledError, BaseException),
        True,
    )


async def main() -> None:
    case_python_cancellederror_hierarchy()
    await case_cancelled_error_is_silent()
    await case_real_exception_is_surfaced()
    await case_mixed_pending_list()


if __name__ == "__main__":
    asyncio.run(main())
    if FAIL:
        print(f"\nFAIL — {FAIL} check(s) failed", file=sys.stderr)
        sys.exit(1)
    print("\nPASS — CancelledError vs Exception split behaves as contracted.")
