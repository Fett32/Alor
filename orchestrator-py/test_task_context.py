"""Regression tests for tools.set_current_task / current_task_id.

Workers publish the active task_id via a ContextVar so the
`agent_spawn` MCP tool can tag its cli.spawn payload with
`spawned_by_task`. The daemon uses that to warn at task-completion
time if worker-spawned instances weren't explicitly killed (see
src-tauri/src/daemon/state.rs::record_task_spawn +
transition_task warning path).

Covers:
  - Default: no task active → None.
  - set_current_task stores + reads back.
  - Clearing with None zeros it out.
  - ContextVar isolation: concurrent asyncio tasks in different
    contexts don't cross-contaminate. Workers process one task
    at a time, but the isolation is cheap insurance against a
    future change that runs tools in parallel contexts.

Run standalone: `python3 test_task_context.py`. Exits 0 on pass.
"""

from __future__ import annotations

import asyncio
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

import tools  # noqa: E402


FAILURES: list[str] = []


def check(label: str, got, want) -> None:
    if got != want:
        FAILURES.append(label)
        print(f"FAIL  {label}")
        print(f"  got : {got!r}")
        print(f"  want: {want!r}")
    else:
        print(f"ok    {label}")


def test_default_is_none() -> None:
    # Fresh ContextVar defaults to None. Call from a synchronous
    # context — no task has been set, should report no active task.
    tools.set_current_task(None)  # defensive reset
    check("default: current_task_id() is None", tools.current_task_id(), None)


def test_set_then_read_in_same_context() -> None:
    tools.set_current_task("abc-123")
    check(
        "set_current_task stores value",
        tools.current_task_id(),
        "abc-123",
    )
    tools.set_current_task(None)
    check(
        "cleared current_task_id is None",
        tools.current_task_id(),
        None,
    )


async def test_isolation_between_asyncio_contexts() -> None:
    """ContextVar should isolate per-coroutine context.

    We spawn two asyncio tasks that each set a different value and
    then yield control. If the contextvar weren't isolated, one
    would see the other's value after the yield.
    """
    results: dict[str, str | None] = {}

    async def set_and_readback(label: str, task_id: str) -> None:
        tools.set_current_task(task_id)
        await asyncio.sleep(0)   # give the other coroutine a chance
        await asyncio.sleep(0)
        results[label] = tools.current_task_id()

    await asyncio.gather(
        set_and_readback("a", "task-AAAA"),
        set_and_readback("b", "task-BBBB"),
    )

    check("context a sees its own task-AAAA", results["a"], "task-AAAA")
    check("context b sees its own task-BBBB", results["b"], "task-BBBB")


async def test_nested_await_preserves_current_task() -> None:
    """set_current_task value survives `await` within the same task."""
    tools.set_current_task("nested-1")

    async def inner() -> str | None:
        await asyncio.sleep(0)
        return tools.current_task_id()

    seen = await inner()
    check("nested await preserves task id", seen, "nested-1")
    tools.set_current_task(None)


async def main() -> int:
    test_default_is_none()
    test_set_then_read_in_same_context()
    await test_isolation_between_asyncio_contexts()
    await test_nested_await_preserves_current_task()

    print()
    if FAILURES:
        print(f"FAIL — {len(FAILURES)} check(s) failed:")
        for f in FAILURES:
            print(f"  - {f}")
        return 1
    print("PASS — task ContextVar publishes + isolates correctly.")
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
