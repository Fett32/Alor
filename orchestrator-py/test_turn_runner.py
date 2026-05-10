"""Regression tests for main.TurnRunner.

Locks in the contract from the "[T3] Reduce client_lock scope" task:

  - `submit(text)` enqueues without blocking the caller on the SDK
    client. Returns a future that resolves when THAT turn completes.
  - FIFO ordering: turns execute in submission order.
  - Fire-and-forget safety: awaiting the submit-future is optional
    (event injection pattern doesn't block on it).
  - Exception isolation: if one turn raises, subsequent turns still
    run; the failing future carries the exception for its caller.
  - `reset_client()` is mutually exclusive with turn execution.
  - `stop()` drains cleanly via the sentinel.

Pre-fix, every `client.query + process_response` pair ran under a
shared `asyncio.Lock`. While the orch's turn was streaming a
response, any event injection contended on the same lock — the
event would wait for the current turn to finish AND re-acquire. On
a long Fett turn that meant worker events sat un-injected for
minutes.

Follows the suite convention: standalone `asyncio.run(main())`,
non-zero exit on failure, no pytest.
"""

from __future__ import annotations

import asyncio
import sys
from typing import Any
from unittest.mock import patch


# ---- Fake SDK client --------------------------------------------------------


class FakeResponseMessage:
    """Non-ResultMessage placeholder for the iterator. process_response
    ignores unknown types, so this just drives the async for loop
    through one iteration before the ResultMessage closes the turn.
    """
    pass


class FakeClient:
    """Minimal ClaudeSDKClient stand-in. Each test configures:
      * `query_calls`: accumulates the texts passed to query().
      * `turn_finish_events`: per-turn asyncio.Event that the test
        sets to unblock that turn's receive_response generator.
      * `turn_exception`: if non-None for a turn index, the turn's
        receive_response raises that exception instead of completing.
    """

    def __init__(self) -> None:
        self.query_calls: list[str] = []
        self.turn_finish_events: list[asyncio.Event] = []
        self.turn_exceptions: list[BaseException | None] = []
        self.disconnects = 0
        self.connects = 0

    def queue_turn(self, exc: BaseException | None = None) -> asyncio.Event:
        """Pre-register a turn: returns an Event the test sets to
        release that turn's receive_response. `exc` if non-None
        raises instead of returning cleanly.
        """
        evt = asyncio.Event()
        self.turn_finish_events.append(evt)
        self.turn_exceptions.append(exc)
        return evt

    async def query(self, text: str) -> None:
        self.query_calls.append(text)

    async def receive_response(self):
        # Pop the next prepared turn. Tests should have queued enough
        # turns before the runner processes them; if not, we
        # deliberately raise so the failure is visible (beats
        # hanging).
        if not self.turn_finish_events:
            raise RuntimeError(
                "FakeClient.receive_response called with no queued turns"
            )
        evt = self.turn_finish_events.pop(0)
        exc = self.turn_exceptions.pop(0)
        await evt.wait()
        if exc is not None:
            raise exc
        # Yield nothing — process_response's stub (below) just
        # iterates and returns when the iterator ends.
        if False:
            yield None

    async def disconnect(self) -> None:
        self.disconnects += 1

    async def connect(self) -> None:
        self.connects += 1


# Stub process_response so tests don't need real SDK types. Just
# iterates the client's receive_response generator.
async def _fake_process_response(client, totals, cost, on_text=None):
    async for _ in client.receive_response():
        pass


# ---- checks -----------------------------------------------------------------


FAIL = 0


def check(label: str, got: Any, want: Any) -> None:
    global FAIL
    if got == want:
        print(f"ok    {label}")
    else:
        print(f"FAIL  {label}: got {got!r}, want {want!r}", file=sys.stderr)
        FAIL += 1


def check_true(label: str, cond: bool) -> None:
    check(label, bool(cond), True)


# ---- cases ------------------------------------------------------------------


async def case_submit_and_await_happy_path() -> None:
    """Baseline: submit a turn, await the future, future resolves
    after the turn completes.
    """
    import main as orch_main

    client = FakeClient()
    evt = client.queue_turn()
    with patch.object(orch_main, "process_response", _fake_process_response):
        runner = orch_main.TurnRunner(client, totals={}, cost=[0.0])
        runner.start()

        fut = await runner.submit("hello", label="test")
        check_true("happy: future is not done immediately", not fut.done())
        # Let the runner grab it and start processing.
        await asyncio.sleep(0)
        check("happy: query was called", client.query_calls, ["hello"])
        # Unblock the turn.
        evt.set()
        await fut
        check_true("happy: future resolves after turn completes", fut.done())
        check_true("happy: no exception on future", fut.exception() is None)

        await runner.stop()


async def case_submit_is_non_blocking_during_in_flight_turn() -> None:
    """The key property the refactor guarantees: submitting a SECOND
    turn while the FIRST is mid-receive_response does NOT block the
    submitter. Pre-fix the caller contended on `client_lock` and
    waited for the entire in-flight turn.
    """
    import main as orch_main

    client = FakeClient()
    evt_a = client.queue_turn()
    evt_b = client.queue_turn()
    with patch.object(orch_main, "process_response", _fake_process_response):
        runner = orch_main.TurnRunner(client, totals={}, cost=[0.0])
        runner.start()

        fut_a = await runner.submit("A", label="fett")
        # Yield once so runner grabs A.
        await asyncio.sleep(0)
        # At this point receive_response is awaiting evt_a. Submit B.
        fut_b = await runner.submit("B", label="event")
        # fut_b must NOT block waiting for A — submit returned. Both
        # futures are pending; queue.put was the only await.
        check_true("non-blocking: fut_a pending", not fut_a.done())
        check_true("non-blocking: fut_b pending", not fut_b.done())
        # Only A's query has fired (B is in the queue, not yet in
        # receive_response).
        check("non-blocking: only A queried so far", client.query_calls, ["A"])

        # Release A. Runner finishes A, then pulls B.
        evt_a.set()
        await fut_a
        check_true("non-blocking: A resolved", fut_a.done())
        # Runner should pick up B next.
        await asyncio.sleep(0)
        check("non-blocking: B queried after A", client.query_calls, ["A", "B"])
        # Release B.
        evt_b.set()
        await fut_b
        check_true("non-blocking: B resolved", fut_b.done())

        await runner.stop()


async def case_fifo_ordering() -> None:
    """Turns execute in submission order, regardless of arrival
    cadence. Important for Fett + event interleaving: a flurry of
    events during Fett's turn all queue BEHIND Fett, not ahead.
    """
    import main as orch_main

    client = FakeClient()
    for _ in range(3):
        client.queue_turn()
    with patch.object(orch_main, "process_response", _fake_process_response):
        runner = orch_main.TurnRunner(client, totals={}, cost=[0.0])
        runner.start()

        fut_a = await runner.submit("first", label="fett")
        fut_b = await runner.submit("second", label="event-1")
        fut_c = await runner.submit("third", label="event-2")

        # Release all three in reverse order — shouldn't matter,
        # queries go out in submission order.
        for evt in list(client.turn_finish_events):
            evt.set()

        await asyncio.wait_for(asyncio.gather(fut_a, fut_b, fut_c), timeout=2.0)
        check(
            "fifo: queries fired in submission order",
            client.query_calls,
            ["first", "second", "third"],
        )

        await runner.stop()


async def case_fire_and_forget_still_executes() -> None:
    """Event injection pattern: submit and move on without awaiting
    the future. The turn must still execute.
    """
    import main as orch_main

    client = FakeClient()
    evt = client.queue_turn()
    with patch.object(orch_main, "process_response", _fake_process_response):
        runner = orch_main.TurnRunner(client, totals={}, cost=[0.0])
        runner.start()

        # Fire and forget: don't bind fut.
        _ = await runner.submit("[event] task.completed", label="event")

        await asyncio.sleep(0)
        check_true(
            "fire-and-forget: query fired even without awaiting future",
            client.query_calls == ["[event] task.completed"],
        )

        # Release so shutdown is clean.
        evt.set()
        await asyncio.sleep(0)

        await runner.stop()


async def case_exception_isolates_one_turn() -> None:
    """A turn that raises should:
      - set the exception on its future (caller sees it).
      - NOT crash the runner — subsequent turns continue to run.
    """
    import main as orch_main

    client = FakeClient()
    evt_a = client.queue_turn(exc=RuntimeError("synthetic SDK failure"))
    evt_b = client.queue_turn()
    with patch.object(orch_main, "process_response", _fake_process_response):
        runner = orch_main.TurnRunner(client, totals={}, cost=[0.0])
        runner.start()

        fut_a = await runner.submit("boom", label="fett")
        fut_b = await runner.submit("after-boom", label="event")
        evt_a.set()
        evt_b.set()

        # fut_a should carry the RuntimeError.
        caught: BaseException | None = None
        try:
            await asyncio.wait_for(fut_a, timeout=2.0)
        except RuntimeError as e:
            caught = e
        check_true(
            "isolation: failing turn's future raises RuntimeError",
            isinstance(caught, RuntimeError) and "synthetic SDK failure" in str(caught),
        )

        # fut_b must still succeed — the runner didn't crash.
        await asyncio.wait_for(fut_b, timeout=2.0)
        check_true("isolation: subsequent turn still succeeds", fut_b.done() and fut_b.exception() is None)
        check(
            "isolation: both queries fired",
            client.query_calls,
            ["boom", "after-boom"],
        )

        await runner.stop()


async def case_reset_client_is_mutually_exclusive_with_turns() -> None:
    """`reset_client` must wait for any in-flight turn to finish
    before disconnect/connect. A turn submitted AFTER the reset is
    called queues; it runs after reconnect completes.
    """
    import main as orch_main

    client = FakeClient()
    evt_a = client.queue_turn()
    evt_b = client.queue_turn()
    with patch.object(orch_main, "process_response", _fake_process_response):
        runner = orch_main.TurnRunner(client, totals={}, cost=[0.0])
        runner.start()

        fut_a = await runner.submit("before-reset", label="fett")
        await asyncio.sleep(0)  # let runner grab A
        check("reset: A is in-flight (query fired)", client.query_calls, ["before-reset"])
        check("reset: no disconnect yet", client.disconnects, 0)

        # Kick off a reset concurrently with A still in flight. The
        # reset should WAIT for A's turn lock to release.
        reset_task = asyncio.create_task(runner.reset_client())
        # Yield a few times — disconnect should not fire yet.
        for _ in range(5):
            await asyncio.sleep(0)
        check("reset: disconnect still blocked", client.disconnects, 0)

        # Release A. Reset should complete now.
        evt_a.set()
        await fut_a
        await asyncio.wait_for(reset_task, timeout=2.0)
        check("reset: disconnect fired after A finished", client.disconnects, 1)
        check("reset: connect fired", client.connects, 1)

        # Turn B submitted before reset should NOT have run yet —
        # we didn't submit it. Now submit a post-reset turn.
        fut_b = await runner.submit("after-reset", label="event")
        evt_b.set()
        await fut_b
        check(
            "reset: post-reset turn runs after reconnect",
            client.query_calls,
            ["before-reset", "after-reset"],
        )

        await runner.stop()


async def case_stop_drains_cleanly_via_sentinel() -> None:
    """stop() should unblock a worker parked on queue.get() via the
    sentinel; the worker task should complete without errors.
    """
    import main as orch_main

    client = FakeClient()
    with patch.object(orch_main, "process_response", _fake_process_response):
        runner = orch_main.TurnRunner(client, totals={}, cost=[0.0])
        runner.start()
        # No turns submitted — worker is parked on queue.get().
        # stop() puts the sentinel and awaits the worker.
        await asyncio.wait_for(runner.stop(), timeout=2.0)
        check_true("stop: worker task cleaned up", runner._worker_task is None)


# ---- driver ----------------------------------------------------------------


async def main() -> None:
    await case_submit_and_await_happy_path()
    await case_submit_is_non_blocking_during_in_flight_turn()
    await case_fifo_ordering()
    await case_fire_and_forget_still_executes()
    await case_exception_isolates_one_turn()
    await case_reset_client_is_mutually_exclusive_with_turns()
    await case_stop_drains_cleanly_via_sentinel()


if __name__ == "__main__":
    asyncio.run(main())
    if FAIL:
        print(f"\nFAIL — {FAIL} check(s) failed", file=sys.stderr)
        sys.exit(1)
    print("\nPASS — TurnRunner serializes turns without a shared client lock.")
