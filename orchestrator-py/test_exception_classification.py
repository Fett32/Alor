"""Regression tests for worker.py's exception-classification fix.

Locks in the contract from the "[T3] Differentiate CancelledError vs
Exception" task:

  - `CancelledError` raised from inside `run_task`'s SDK call
    PROPAGATES (control-flow signal, not a bug).
  - `Exception` (e.g. RuntimeError) raised from inside `run_task`'s
    SDK call is CAUGHT, logged with traceback, reported via
    `sock.send_error(...)`, and run_task returns normally.
  - The logged output includes "traceback" markers (not just a
    one-line `[task error] X`).
  - `daemon_loop`'s except-arm classification:
      * `CancelledError` → propagates.
      * `OSError` → `[daemon_loop transport error]` log.
      * other `Exception` → `[daemon_loop BUG` log with traceback.

Pre-fix, a bare `except Exception` in both places hid real bugs as
either "worker exception during task: X" (one line, no stack) or
"[daemon_loop error] X" that looked like a network problem and
silently triggered reconnect. CancelledError was swallowed by the
`Exception` arm (pre-3.8 habit where it was still an `Exception`
subclass) on top of that.

Follows the suite convention: standalone `asyncio.run(main())`,
non-zero exit on failure, no pytest.
"""

from __future__ import annotations

import asyncio
import io
import sys
from contextlib import redirect_stdout, redirect_stderr
from typing import Any
from unittest.mock import AsyncMock


# ---- failure injection helpers ---------------------------------------------


class FakeSock:
    """Minimal AgentClient stand-in for run_task.

    Records the calls run_task makes so tests can assert shape
    without standing up a real Unix-socket connection.
    """

    def __init__(self) -> None:
        self.accepts: list[str] = []
        self.errors: list[str] = []
        self.completes: list[dict] = []
        # Blocked sends arrive here — (task_id, reason, waiting_for).
        # Tests assert on this to confirm the Accepted → Blocked
        # state transition path ran from the worker side.
        self.blocked: list[tuple[str, str, str | None]] = []

    async def send_accept(self, task_id: str) -> None:
        self.accepts.append(task_id)

    async def send_error(self, msg: str) -> None:
        self.errors.append(msg)

    async def send_complete(self, **kwargs: Any) -> None:
        self.completes.append(kwargs)

    async def send_blocked(
        self,
        task_id: str,
        reason: str,
        waiting_for: str | None = None,
    ) -> None:
        self.blocked.append((task_id, reason, waiting_for))


class FakeClient:
    """Minimal ClaudeSDKClient stand-in. `query` raises whatever
    exception the test sets; `receive_response` yields nothing so
    `process_response` returns immediately if we get that far.
    """

    def __init__(self, raise_on_query: BaseException | None = None) -> None:
        self._raise_on_query = raise_on_query

    async def query(self, text: str) -> None:
        if self._raise_on_query is not None:
            raise self._raise_on_query

    async def receive_response(self):  # pragma: no cover — used when not raising
        if False:
            yield None
        return


def make_env(task_id: str = "t-abc", title: str = "T", description: str = "D"):
    class _Env:
        payload = {
            "task_id": task_id,
            "title": title,
            "description": description,
        }

    return _Env()


# ---- checks ----------------------------------------------------------------


FAIL = 0


def check(label: str, got: object, want: object) -> None:
    global FAIL
    if got == want:
        print(f"ok    {label}")
    else:
        print(f"FAIL  {label}: got {got!r}, want {want!r}", file=sys.stderr)
        FAIL += 1


def check_true(label: str, cond: bool) -> None:
    check(label, bool(cond), True)


# ---- run_task error classification -----------------------------------------


async def case_run_task_propagates_cancelled_error() -> None:
    """CancelledError raised during `client.query` must propagate —
    pre-fix the `except Exception` arm swallowed it (pre-3.8
    subclass assumption) and fired a stray `wrapper.error`.
    """
    import worker

    sock = FakeSock()
    client = FakeClient(raise_on_query=asyncio.CancelledError())
    lock = asyncio.Lock()
    totals: dict[str, int] = {}
    cost: list[float] = [0.0]
    current: dict[str, str | None] = {}
    env = make_env()

    propagated = False
    try:
        await worker.run_task(
            client,
            lock,
            sock,
            env,
            totals,
            cost,
            session_start=0.0,
            current=current,
        )
    except asyncio.CancelledError:
        propagated = True
    except BaseException as e:
        check_true(f"run_task cancel: wrong exception type escaped ({type(e).__name__})", False)
        return

    check_true("run_task cancel: CancelledError propagated past except-Exception", propagated)
    # send_accept ran (pre-query). send_error must NOT have fired —
    # cancellation is not an error the orch should see.
    check("run_task cancel: send_accept was called", sock.accepts, ["t-abc"])
    check("run_task cancel: send_error NOT called (not a bug)", sock.errors, [])
    # current["task_id"] must have been cleared by the finally block.
    check("run_task cancel: current task_id cleared by finally", current.get("task_id"), None)


async def case_run_task_logs_traceback_on_exception() -> None:
    """A genuine RuntimeError during the SDK call must:
      - be caught (run_task returns, doesn't propagate),
      - get a traceback printed (not just the one-line `[task error] X`),
      - produce a `send_error` call carrying the error text.
    """
    import worker

    sock = FakeSock()
    client = FakeClient(raise_on_query=RuntimeError("synthetic SDK failure XYZ"))
    lock = asyncio.Lock()
    env = make_env(task_id="t-xyz")

    buf = io.StringIO()
    with redirect_stdout(buf):
        # run_task must NOT raise for a regular Exception.
        try:
            await worker.run_task(
                client,
                lock,
                sock,
                env,
                totals={},
                cost=[0.0],
                session_start=0.0,
                current={},
            )
            returned_normally = True
        except BaseException as e:
            returned_normally = False
            check_true(
                f"run_task exception: wrapped as unexpected {type(e).__name__}",
                False,
            )

    check_true("run_task exception: returned normally (not raised)", returned_normally)
    out = buf.getvalue()
    check_true(
        "run_task exception: log mentions the exception",
        "synthetic SDK failure XYZ" in out,
    )
    check_true(
        "run_task exception: log includes traceback (Traceback keyword)",
        "Traceback" in out,
    )
    check_true(
        "run_task exception: log includes worker frame (`worker.py`)",
        "worker.py" in out,
    )

    check_true(
        "run_task exception: send_error was called",
        len(sock.errors) == 1,
    )
    if sock.errors:
        # send_error body should carry the traceback too (so orch-side
        # logs see the stack, not just the worker stderr).
        check_true(
            "run_task exception: send_error payload includes traceback",
            "Traceback" in sock.errors[0] and "synthetic SDK failure XYZ" in sock.errors[0],
        )

    # NEW (2026-04-20 codex-alor audit fix): the worker MUST also
    # send task.blocked so the daemon transitions Accepted → Blocked
    # and frees the agent slot. Pre-fix only `send_error` (telemetry
    # broadcast) fired, and the task stayed ACCEPTED forever on
    # max_concurrent=1 agents.
    check_true(
        "run_task exception: task.blocked was sent (state transition)",
        len(sock.blocked) == 1,
    )
    if sock.blocked:
        (blocked_task_id, blocked_reason, blocked_waiting_for) = sock.blocked[0]
        check(
            "run_task exception: task.blocked carries correct task_id",
            blocked_task_id,
            "t-xyz",
        )
        check_true(
            "run_task exception: task.blocked reason mentions the exception",
            "worker exception" in blocked_reason
            and "synthetic SDK failure XYZ" in blocked_reason,
        )
        check(
            "run_task exception: task.blocked waiting_for is None (unknown)",
            blocked_waiting_for,
            None,
        )


async def case_run_task_cancel_does_not_send_blocked() -> None:
    """CancelledError is control flow, not a bug. It must NOT fire
    task.blocked — the state machine should NOT transition the
    task to Blocked on a cooperative shutdown. (If the worker is
    shutting down, the daemon's disconnect handler will reconcile
    state; the worker shouldn't pretend the task failed.)
    """
    import worker

    sock = FakeSock()
    client = FakeClient(raise_on_query=asyncio.CancelledError())
    lock = asyncio.Lock()
    env = make_env(task_id="t-cancel")

    try:
        await worker.run_task(
            client, lock, sock, env,
            totals={}, cost=[0.0], session_start=0.0, current={},
        )
    except asyncio.CancelledError:
        pass

    check(
        "run_task cancel: task.blocked NOT sent (cancel is not failure)",
        sock.blocked,
        [],
    )


# ---- daemon_loop error classification --------------------------------------


class RaisingRecvForever:
    """Async iterable stand-in for AgentClient.recv_forever.

    Raises the configured exception on first `__anext__`. Used to
    drive daemon_loop through each except-arm.
    """

    def __init__(self, raise_with: BaseException) -> None:
        self._raise_with = raise_with
        self._raised = False

    def __aiter__(self):
        return self

    async def __anext__(self):
        if not self._raised:
            self._raised = True
            raise self._raise_with
        raise StopAsyncIteration


class FakeDaemonSock:
    def __init__(self, raise_with: BaseException) -> None:
        self._raise_with = raise_with
        self.close_calls = 0
        self.connect_calls = 0

    def recv_forever(self):
        return RaisingRecvForever(self._raise_with)

    async def close(self) -> None:
        self.close_calls += 1

    async def connect_and_register(self) -> None:
        self.connect_calls += 1
        # Simulate persistent failure so daemon_loop stays in the
        # reconnect-failed arm — keeps the test bounded without
        # having to fake a full reconnect cycle.
        raise OSError("simulated reconnect refusal")

    def pending_count(self) -> int:
        return 0

    async def flush_outbox(self) -> int:
        return 0


async def _run_daemon_loop_briefly(sock, client, stop) -> str:
    """Run daemon_loop long enough to hit the first except-arm, then
    cancel it. Returns captured stdout+stderr so tests can assert on
    log shape.
    """
    import worker

    buf = io.StringIO()

    async def _run() -> None:
        lock = asyncio.Lock()
        await worker.daemon_loop(
            sock,
            client,
            lock,
            totals={},
            cost=[0.0],
            session_start=0.0,
            stop=stop,
            current={},
        )

    task = asyncio.create_task(_run(), name="daemon_loop_test")
    # Let daemon_loop reach the except arm + first reconnect attempt.
    with redirect_stdout(buf), redirect_stderr(buf):
        await asyncio.sleep(0)
        await asyncio.sleep(0)
        # daemon_loop hits except, sleeps 2s, calls connect_and_register
        # (which raises OSError), sleeps 3s. We don't want to actually
        # wait — signal stop and cancel.
        stop.set()
        task.cancel()
        try:
            await task
        except asyncio.CancelledError:
            pass
        except BaseException:
            pass
    return buf.getvalue()


async def case_daemon_loop_os_error_logged_as_transport() -> None:
    """OSError from recv_forever → '[daemon_loop transport error]' log,
    NO '[daemon_loop BUG]' log. This is the normal daemon-restart path.
    """
    import worker

    sock = FakeDaemonSock(raise_with=OSError("broken pipe, daemon restart"))
    client = object()  # unused in this path
    stop = asyncio.Event()
    out = await _run_daemon_loop_briefly(sock, client, stop)

    check_true(
        "daemon_loop OSError: 'transport error' label appears",
        "transport error" in out,
    )
    check_true(
        "daemon_loop OSError: 'BUG' label does NOT appear",
        "BUG" not in out,
    )


async def case_daemon_loop_generic_exception_flagged_as_bug() -> None:
    """Non-OSError exception (e.g. KeyError from a logic bug in an
    envelope handler) → '[daemon_loop BUG' log with traceback.
    Pre-fix this landed as '[daemon_loop error]' and was
    indistinguishable from a network error.
    """
    import worker

    sock = FakeDaemonSock(raise_with=KeyError("synthetic logic bug"))
    client = object()
    stop = asyncio.Event()
    out = await _run_daemon_loop_briefly(sock, client, stop)

    check_true(
        "daemon_loop bug: 'BUG' label appears",
        "BUG" in out and "not a transport error" in out,
    )
    check_true(
        "daemon_loop bug: traceback in log",
        "Traceback" in out,
    )
    check_true(
        "daemon_loop bug: 'transport error' label does NOT appear for this case",
        "daemon_loop transport error" not in out,
    )


async def case_daemon_loop_cancelled_propagates() -> None:
    """CancelledError from recv_forever must propagate — pre-fix the
    `except Exception` arm caught it (CancelledError was once a
    subclass) and triggered a reconnect cycle instead of letting
    the task cancel cleanly.
    """
    import worker

    sock = FakeDaemonSock(raise_with=asyncio.CancelledError())
    client = object()
    stop = asyncio.Event()

    lock = asyncio.Lock()
    task = asyncio.create_task(
        worker.daemon_loop(
            sock,
            client,
            lock,
            totals={},
            cost=[0.0],
            session_start=0.0,
            stop=stop,
            current={},
        ),
        name="daemon_loop_cancel",
    )
    try:
        await asyncio.wait_for(task, timeout=2.0)
    except asyncio.CancelledError:
        # Propagated — expected.
        pass
    except asyncio.TimeoutError:
        task.cancel()
        check_true(
            "daemon_loop cancel: CancelledError propagated (not swallowed + reconnected)",
            False,
        )
        return
    except BaseException as e:
        check_true(
            f"daemon_loop cancel: unexpected {type(e).__name__} escaped",
            False,
        )
        return

    # Close out the task cleanly if still pending.
    if not task.done():
        task.cancel()
        try:
            await task
        except BaseException:
            pass

    check_true("daemon_loop cancel: task ended promptly", task.done())


# ---- driver -----------------------------------------------------------------


async def case_run_task_refuses_sdk_phase_when_send_accept_raises() -> None:
    """send_accept strict mode (from the 73378a0d follow-up): if
    `sock.send_accept` raises, `run_task` must NOT proceed to the
    SDK turn. Pre-fix, send_accept silently queued on a dead
    socket; the worker would stream a response that the daemon
    couldn't credit (task stayed in ASSIGNED while tokens burned).
    """
    import worker
    from agent_client import AgentClientError

    class RefusingSock(FakeSock):
        async def send_accept(self, task_id: str) -> None:
            raise AgentClientError(
                f"send {task_id} failed: simulated dead socket"
            )

    sock = RefusingSock()
    query_calls: list[str] = []

    class QueryProbeClient(FakeClient):
        async def query(self, text: str) -> None:
            # run_task must NOT reach here when send_accept raised.
            query_calls.append(text)

    client = QueryProbeClient()
    lock = asyncio.Lock()
    env = make_env(task_id="t-refused")

    buf = io.StringIO()
    with redirect_stdout(buf):
        await worker.run_task(
            client,
            lock,
            sock,
            env,
            totals={},
            cost=[0.0],
            session_start=0.0,
            current={},
        )

    check("strict-accept: SDK query NOT called", query_calls, [])
    check_true(
        "strict-accept: log flags refusal explicitly",
        "refusing to proceed" in buf.getvalue(),
    )
    check_true(
        "strict-accept: log mentions task stays ASSIGNED",
        "ASSIGNED" in buf.getvalue(),
    )
    # Best-effort wrapper.error should have fired via the queueing
    # send() path (even if queued on a dead socket — acceptable).
    check_true(
        "strict-accept: send_error invoked for orch telemetry",
        len(sock.errors) == 1,
    )


async def case_daemon_loop_no_head_of_line_block() -> None:
    """Two back-to-back task.assign envelopes must BOTH progress
    through send_accept within a scheduler tick, even while the
    first task's client.query is still blocked on client_lock.

    Pre-fix the `await run_task(...)` inline in daemon_loop's
    async-for held the loop blocked for the entire SDK stream; a
    second task.assign sat in the socket buffer until task #1
    finished. That's the 2026-04-20 73378a0d incident (stuck
    ASSIGNED after 0ba2bec6 completed).
    """
    import worker

    class TaskAssignEnvelope:
        def __init__(self, task_id: str):
            self.kind = "task.assign"
            self.payload = {
                "task_id": task_id,
                "title": f"T {task_id}",
                "description": "D",
            }

    class TwoAssignsThenBlock:
        """recv_forever that yields two task.assigns then blocks
        forever (simulating the daemon socket staying open with no
        new traffic). daemon_loop should keep draining envelopes
        without either assign blocking the other.
        """

        def __init__(self) -> None:
            self._q: list[object] = [
                TaskAssignEnvelope("t-first"),
                TaskAssignEnvelope("t-second"),
            ]
            self._done = asyncio.Event()

        def __aiter__(self):
            return self

        async def __anext__(self):
            if self._q:
                return self._q.pop(0)
            # Park here until cancelled.
            await self._done.wait()
            raise StopAsyncIteration

    class FakeConcurrentSock:
        def __init__(self) -> None:
            self.accepts: list[str] = []
            self.errors: list[str] = []
            self._iter = TwoAssignsThenBlock()

        def recv_forever(self):
            return self._iter

        async def send_accept(self, task_id: str) -> None:
            self.accepts.append(task_id)

        async def send_error(self, msg: str) -> None:
            self.errors.append(msg)

        async def send_complete(self, **kwargs) -> None:
            pass

        async def send_status(self, **kwargs) -> None:
            pass

        async def close(self) -> None:
            pass

        async def connect_and_register(self) -> None:
            pass

        def pending_count(self) -> int:
            return 0

    # A client whose `query` blocks on an Event — simulates a long-
    # running SDK turn. client_lock is held for the duration, so a
    # serialized design would starve the second task.
    block = asyncio.Event()

    class BlockingQueryClient:
        async def query(self, text: str) -> None:
            await block.wait()

        async def receive_response(self):
            if False:
                yield None

    sock = FakeConcurrentSock()
    client = BlockingQueryClient()
    lock = asyncio.Lock()
    stop = asyncio.Event()

    daemon_task = asyncio.create_task(
        worker.daemon_loop(
            sock,
            client,
            lock,
            totals={},
            cost=[0.0],
            session_start=0.0,
            stop=stop,
            current={},
        ),
        name="daemon_loop_hol_test",
    )

    # Give daemon_loop multiple ticks to pull both envelopes and
    # spawn their detached run_tasks. Each run_task calls
    # send_accept BEFORE acquiring client_lock, so both accepts
    # should land even while run_task #1 is stuck on client.query.
    for _ in range(30):
        await asyncio.sleep(0)

    # Primary assertion: both accepts fired.
    check_true(
        "no-HOL: first accept fired",
        "t-first" in sock.accepts,
    )
    check_true(
        "no-HOL: second accept fired while first task is "
        "blocked mid-query",
        "t-second" in sock.accepts,
    )
    check_true(
        "no-HOL: exactly 2 accepts (no duplicates)",
        len(sock.accepts) == 2,
    )

    # Cleanup — unblock query, signal stop, cancel daemon_loop.
    block.set()
    stop.set()
    daemon_task.cancel()
    try:
        await asyncio.wait_for(daemon_task, timeout=2.0)
    except (asyncio.CancelledError, asyncio.TimeoutError):
        pass


async def main() -> None:
    await case_run_task_propagates_cancelled_error()
    await case_run_task_logs_traceback_on_exception()
    await case_run_task_cancel_does_not_send_blocked()
    await case_run_task_refuses_sdk_phase_when_send_accept_raises()
    await case_daemon_loop_os_error_logged_as_transport()
    await case_daemon_loop_generic_exception_flagged_as_bug()
    await case_daemon_loop_cancelled_propagates()
    await case_daemon_loop_no_head_of_line_block()


if __name__ == "__main__":
    asyncio.run(main())
    if FAIL:
        print(f"\nFAIL — {FAIL} check(s) failed", file=sys.stderr)
        sys.exit(1)
    print("\nPASS — CancelledError vs Exception classification holds.")
