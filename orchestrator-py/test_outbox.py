"""Regression tests for AgentClient's in-memory outbox.

Covers the daemon-restart-mid-send failure mode: workers used to raise
(or silently lose) envelopes like `task.complete` when the daemon socket
died on drain. AgentClient now queues those frames and the reconnect
path in worker.daemon_loop flushes them on the way back up.

Run standalone: `python3 test_outbox.py` from orchestrator-py/. Exits 0
on pass. No pytest dependency to match test_frame_wedge.py.

Covers:
  - Happy-path send leaves the outbox empty.
  - Dead-writer sends enqueue and return (do not raise).
  - Send before connect enqueues.
  - All three dead-socket exception classes are caught.
  - Count cap evicts oldest (FIFO) with warn.
  - Byte cap evicts oldest (FIFO) with warn.
  - Single oversized frame is still kept when outbox is empty.
  - Flush replays in FIFO order.
  - Partial flush leaves the failing frame + remainder in the queue.
  - All six send_* wrappers route through the outbox on a dead writer.
"""

from __future__ import annotations

import asyncio
import json
import sys
import tempfile
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).parent))

import agent_client  # noqa: E402
from agent_client import AgentClient, OUTBOX_MAX_ENTRIES, OUTBOX_MAX_BYTES  # noqa: E402


# ---------------------------------------------------------------------------
# Stub writers
# ---------------------------------------------------------------------------


class CapturingWriter:
    """Stand-in for asyncio.StreamWriter. Records every write."""

    def __init__(self) -> None:
        self.writes: list[bytes] = []

    def write(self, frame: bytes) -> None:
        self.writes.append(frame)

    async def drain(self) -> None:
        return None

    def close(self) -> None:
        pass

    async def wait_closed(self) -> None:
        pass


class DeadWriterOnDrain:
    """StreamWriter that accepts writes but fails on drain.

    Mimics the real failure shape: asyncio buffers the write and surfaces
    the dead socket on the subsequent drain(). Either pass `exc_cls` for
    a simple no-arg exception, or `exc_factory` for a parameterized one
    (e.g. `lambda: OSError(errno.EPIPE, "broken pipe")`).
    """

    def __init__(
        self,
        exc_cls: type[BaseException] = BrokenPipeError,
        exc_factory: Any = None,
    ) -> None:
        self.writes: list[bytes] = []
        self._exc_cls = exc_cls
        self._exc_factory = exc_factory

    def write(self, frame: bytes) -> None:
        self.writes.append(frame)

    async def drain(self) -> None:
        if self._exc_factory is not None:
            raise self._exc_factory()
        raise self._exc_cls("simulated dead socket")

    def close(self) -> None:
        pass

    async def wait_closed(self) -> None:
        pass


class IntermittentWriter:
    """Captures writes; drains succeed until `fail_on_index`.

    When the Nth drain() is called (0-indexed), raise BrokenPipeError.
    After that point, subsequent drain() calls also fail — matching the
    real semantics of a dead socket staying dead.
    """

    def __init__(self, fail_on_index: int) -> None:
        self.writes: list[bytes] = []
        self.drain_calls: int = 0
        self._fail_on = fail_on_index

    def write(self, frame: bytes) -> None:
        self.writes.append(frame)

    async def drain(self) -> None:
        idx = self.drain_calls
        self.drain_calls += 1
        if idx >= self._fail_on:
            raise BrokenPipeError("simulated dead socket after success")

    def close(self) -> None:
        pass

    async def wait_closed(self) -> None:
        pass


# ---------------------------------------------------------------------------
# Assertion helper
# ---------------------------------------------------------------------------


FAILURES: list[str] = []


def check(label: str, got: Any, want: Any) -> None:
    if got != want:
        FAILURES.append(label)
        print(f"FAIL  {label}")
        print(f"  got : {got!r}")
        print(f"  want: {want!r}")
    else:
        print(f"ok    {label}")


def check_true(label: str, cond: bool) -> None:
    check(label, bool(cond), True)


def decode(frame: bytes) -> dict[str, Any]:
    return json.loads(frame.decode().rstrip("\n"))


def make_client(writer: Any) -> AgentClient:
    sock = AgentClient("test-agent")
    sock._writer = writer  # type: ignore[assignment]
    return sock


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


async def test_happy_path() -> None:
    w = CapturingWriter()
    sock = make_client(w)
    await sock.send_accept("task-1")
    check("happy: one frame written", len(w.writes), 1)
    check("happy: outbox empty", sock.pending_count(), 0)
    check("happy: no drops", sock.dropped_count(), 0)
    body = decode(w.writes[0])
    check("happy: frame kind", body["type"], "task.accept")


async def test_dead_writer_enqueues() -> None:
    w = DeadWriterOnDrain(BrokenPipeError)
    sock = make_client(w)
    # Must not raise.
    await sock.send_complete("task-1", summary="done")
    check("dead-writer: pending_count == 1", sock.pending_count(), 1)
    check("dead-writer: bytes > 0", sock.pending_bytes() > 0, True)
    check("dead-writer: no drops", sock.dropped_count(), 0)
    frame = sock._outbox[0]
    body = decode(frame)
    check("dead-writer: frame kind", body["type"], "task.complete")
    check("dead-writer: frame task_id", body["payload"]["task_id"], "task-1")


async def test_send_before_connect_enqueues() -> None:
    sock = AgentClient("test-agent")  # no _writer set
    await sock.send_error("boom")
    check("pre-connect: pending_count", sock.pending_count(), 1)
    body = decode(sock._outbox[0])
    check("pre-connect: frame kind", body["type"], "wrapper.error")


async def test_all_exception_types_caught() -> None:
    # ConnectionError family — the original narrow catch.
    for exc_cls in (BrokenPipeError, ConnectionResetError, ConnectionError):
        w = DeadWriterOnDrain(exc_cls)
        sock = make_client(w)
        await sock.send_status(task_id="t", alive=True, details="x")
        check(
            f"exc {exc_cls.__name__}: pending_count",
            sock.pending_count(),
            1,
        )

    # Plain OSError variants. Regression test for the dropped-completion
    # bug: during a daemon reboot, drain() can raise a bare OSError
    # (not a ConnectionError subclass) with any of these errnos, and
    # the old narrow catch let the frame escape unqueued.
    import errno

    for eno in (errno.EPIPE, errno.EBADF, errno.ENOTCONN, errno.EIO):
        w = DeadWriterOnDrain(
            exc_factory=lambda e=eno: OSError(e, f"errno {e}"),
        )
        sock = make_client(w)
        await sock.send_complete("task-reboot", summary="done")
        check(
            f"exc OSError(errno={eno}): pending_count",
            sock.pending_count(),
            1,
        )
        check(
            f"exc OSError(errno={eno}): frame is task.complete",
            decode(sock._outbox[0])["type"],
            "task.complete",
        )


async def test_count_cap_evicts_oldest() -> None:
    # Uses send_complete rather than send_accept as the outbox-push
    # vehicle — send_accept was promoted to strict-send in the
    # 73378a0d follow-up fix, so it no longer queues. send_complete
    # still routes through the queueing `send()` path and keeps the
    # same shape (task_id in payload) so the FIFO assertions below
    # still read naturally.
    w = DeadWriterOnDrain()
    sock = make_client(w)
    # Send OUTBOX_MAX_ENTRIES + 5 frames with distinguishable payloads.
    for i in range(OUTBOX_MAX_ENTRIES + 5):
        await sock.send_complete(f"task-{i}", summary="ok")
    check("count-cap: length capped", sock.pending_count(), OUTBOX_MAX_ENTRIES)
    check("count-cap: 5 dropped", sock.dropped_count(), 5)
    # FIFO: the first 5 frames (task-0…task-4) were evicted; the head
    # should now be task-5.
    head = decode(sock._outbox[0])
    tail = decode(sock._outbox[-1])
    check("count-cap: head is task-5", head["payload"]["task_id"], "task-5")
    check(
        "count-cap: tail is newest",
        tail["payload"]["task_id"],
        f"task-{OUTBOX_MAX_ENTRIES + 4}",
    )


async def test_byte_cap_evicts_oldest() -> None:
    w = DeadWriterOnDrain()
    sock = make_client(w)
    # Each frame ~128 KiB of payload → 8 frames to clear 1 MiB cap, 10
    # total sends force at least 2 evictions by bytes.
    big = "x" * (128 * 1024)
    for i in range(10):
        await sock.send_complete(f"task-{i}", summary=big)
    check_true(
        "byte-cap: total bytes within cap",
        sock.pending_bytes() <= OUTBOX_MAX_BYTES,
    )
    check_true("byte-cap: at least one drop", sock.dropped_count() >= 1)
    # Oldest surviving frame must be after the drops: task-N where N > 0.
    head = decode(sock._outbox[0])
    head_n = int(head["payload"]["task_id"].split("-")[1])
    check_true("byte-cap: head is not task-0", head_n > 0)


async def test_oversized_frame_kept_when_empty() -> None:
    w = DeadWriterOnDrain()
    sock = make_client(w)
    # Single frame exceeding OUTBOX_MAX_BYTES. With an empty outbox the
    # byte cap does not evict (nothing to evict); the frame is retained.
    huge = "x" * (OUTBOX_MAX_BYTES + 10_000)
    await sock.send_complete("task-big", summary=huge)
    check("oversized: single frame kept", sock.pending_count(), 1)
    check("oversized: no drops", sock.dropped_count(), 0)
    check_true(
        "oversized: bytes above cap",
        sock.pending_bytes() > OUTBOX_MAX_BYTES,
    )


async def test_flush_fifo_order() -> None:
    # Queue on a dead writer, then swap to a capturing writer and flush.
    # send_accept was swapped out (now strict — doesn't queue); using
    # send_error as the first queueing kind instead so this test still
    # exercises heterogeneous-kind FIFO ordering through the outbox.
    dead = DeadWriterOnDrain()
    sock = make_client(dead)
    await sock.send_error("boom")
    await sock.send_complete("task-1", summary="ok")
    await sock.send_status(task_id="task-1", alive=True)
    check("flush-fifo: 3 pending", sock.pending_count(), 3)

    live = CapturingWriter()
    sock._writer = live  # type: ignore[assignment]
    flushed = await sock.flush_outbox()
    check("flush-fifo: flushed 3", flushed, 3)
    check("flush-fifo: outbox empty after", sock.pending_count(), 0)
    check("flush-fifo: 3 writes on live", len(live.writes), 3)
    kinds = [decode(f)["type"] for f in live.writes]
    check(
        "flush-fifo: order preserved",
        kinds,
        ["wrapper.error", "task.complete", "status.response"],
    )


async def test_partial_flush_keeps_remainder() -> None:
    # Queue 3 frames on a dead writer. Swapped from send_accept to
    # send_complete after send_accept became strict — same FIFO
    # invariant under test, different envelope kind.
    dead = DeadWriterOnDrain()
    sock = make_client(dead)
    await sock.send_complete("task-1", summary="a")
    await sock.send_complete("task-2", summary="b")
    await sock.send_complete("task-3", summary="c")
    check("partial: 3 pending before flush", sock.pending_count(), 3)

    # Writer that succeeds once then fails.
    flaky = IntermittentWriter(fail_on_index=1)
    sock._writer = flaky  # type: ignore[assignment]
    flushed = await sock.flush_outbox()
    check("partial: flushed 1", flushed, 1)
    check("partial: 2 still pending", sock.pending_count(), 2)
    # The failing frame must still be at head — not dropped.
    head = decode(sock._outbox[0])
    check("partial: head is task-2", head["payload"]["task_id"], "task-2")
    tail = decode(sock._outbox[-1])
    check("partial: tail is task-3", tail["payload"]["task_id"], "task-3")


async def test_queueing_send_paths_route_through_outbox() -> None:
    """Every public send_* wrapper that uses the QUEUEING send path
    must end up in the outbox when the writer is dead — that's the
    whole point of routing through `send()`.

    Renamed from `test_all_six_*` when `send_accept` was promoted to
    the strict-send path (2026-04-20, task 73378a0d follow-up).
    Silent queue on a dead socket was actively dangerous for
    task.accept — the daemon would never see the accept, the task
    would sit in ASSIGNED while the worker burned tokens. Strict
    semantics there surface the failure to `run_task`, which now
    refuses to enter the SDK phase. `test_send_accept_is_strict`
    locks in the new raise behavior as a sibling.
    """
    paths: list[tuple[str, Any, str]] = [
        (
            "send_complete",
            lambda s: s.send_complete("task-1", summary="ok"),
            "task.complete",
        ),
        (
            "send_error",
            lambda s: s.send_error("boom"),
            "wrapper.error",
        ),
        (
            "send_status",
            lambda s: s.send_status(task_id="task-1", alive=True),
            "status.response",
        ),
        (
            "send_worker_user_input",
            lambda s: s.send_worker_user_input(
                text="hi", during_task=False, task_id=None
            ),
            "worker.user_input",
        ),
        (
            "send_worker_orch_response",
            lambda s: s.send_worker_orch_response(
                correlation_id="corr-1",
                text="reply",
                during_task=False,
                task_id=None,
            ),
            "worker.orch_response",
        ),
    ]
    for name, call, expected_kind in paths:
        w = DeadWriterOnDrain()
        sock = make_client(w)
        await call(sock)
        check(f"{name}: 1 queued", sock.pending_count(), 1)
        body = decode(sock._outbox[0])
        check(f"{name}: kind {expected_kind}", body["type"], expected_kind)


# ---------------------------------------------------------------------------
# Driver
# ---------------------------------------------------------------------------


async def test_send_accept_is_strict() -> None:
    """send_accept MUST NOT silently queue on a dead writer.

    The complementary test to `test_queueing_send_paths_route_through_outbox`
    — where that test asserts five send_* paths DO queue, this one
    asserts send_accept specifically does NOT. Silent queue on
    task.accept was an observed live bug (2026-04-20, task
    73378a0d): daemon never saw the accept, task sat in ASSIGNED
    while the worker streamed an SDK response into the void.

    Expected behavior now: raise `AgentClientError` so `run_task`
    can refuse to enter the SDK phase. Outbox must remain empty —
    a strict-send failure must NOT leak frames into the queue that
    might flush later under a stale task id.
    """
    from agent_client import AgentClientError

    # Dead writer — drain() raises BrokenPipeError.
    w = DeadWriterOnDrain(BrokenPipeError)
    sock = make_client(w)

    raised = False
    try:
        await sock.send_accept("task-strict-1")
    except AgentClientError:
        raised = True
    except BaseException as e:
        check(
            f"strict-accept: wrong exception type escaped ({type(e).__name__})",
            False,
            True,
        )
        return

    check_true("strict-accept: raises on dead writer", raised)
    check("strict-accept: outbox stays empty (no silent queue)", sock.pending_count(), 0)
    check("strict-accept: zero drops", sock.dropped_count(), 0)

    # Pre-connect variant: writer is None (never connected). Strict
    # send must ALSO raise here — pre-fix the `if self._writer is
    # None: self._enqueue(frame); return` branch of send() would
    # silently queue task.accept even before the socket existed.
    sock2 = AgentClient("never-connected")  # no _writer set
    raised2 = False
    try:
        await sock2.send_accept("task-strict-2")
    except AgentClientError:
        raised2 = True
    except BaseException as e:
        check(
            f"strict-accept pre-connect: wrong exception type ({type(e).__name__})",
            False,
            True,
        )
        return
    check_true("strict-accept pre-connect: raises when no writer", raised2)
    check(
        "strict-accept pre-connect: outbox empty",
        sock2.pending_count(),
        0,
    )


async def test_disk_persist_round_trip() -> None:
    """Queued frames survive process restart via on-disk JSONL mirror.

    Regression test for the dogfood-reboot bug: pkill -9 of the Python
    worker tore down the in-memory outbox and queued completions were
    lost. With disk persistence, a fresh AgentClient pointed at the
    same path rehydrates the deque.
    """
    with tempfile.TemporaryDirectory() as td:
        outbox_path = Path(td) / "claude-alor.jsonl"

        # First life: queue two frames on a dead writer, file exists.
        sock_a = AgentClient("claude-alor", outbox_path=outbox_path)
        sock_a._writer = DeadWriterOnDrain()  # type: ignore[assignment]
        await sock_a.send_complete("task-A", summary="first")
        await sock_a.send_complete("task-B", summary="second")
        check("disk: file exists after enqueue", outbox_path.exists(), True)
        check("disk: 2 queued in memory", sock_a.pending_count(), 2)

        # Second life: fresh AgentClient reads the same path.
        # The old sock_a is gone (simulating pkill -9 + respawn).
        sock_b = AgentClient("claude-alor", outbox_path=outbox_path)
        check("disk: rehydrated 2 frames", sock_b.pending_count(), 2)
        head = decode(sock_b._outbox[0])
        tail = decode(sock_b._outbox[1])
        check("disk: FIFO preserved (head task-A)", head["payload"]["task_id"], "task-A")
        check("disk: FIFO preserved (tail task-B)", tail["payload"]["task_id"], "task-B")
        check("disk: rehydrated kind is task.complete", head["type"], "task.complete")


async def test_disk_persist_cleared_on_empty() -> None:
    """Successful flush empties the file (file removed)."""
    with tempfile.TemporaryDirectory() as td:
        outbox_path = Path(td) / "clean.jsonl"

        sock = AgentClient("clean", outbox_path=outbox_path)
        sock._writer = DeadWriterOnDrain()  # type: ignore[assignment]
        # send_accept is now strict (raises on dead writer); use
        # send_complete here since this test only cares about the
        # disk-persist-then-clear behavior, not the specific kind.
        await sock.send_complete("task-1", summary="ok")
        check("empty-cleanup: file written", outbox_path.exists(), True)

        # Swap to a live writer and drain.
        sock._writer = CapturingWriter()  # type: ignore[assignment]
        flushed = await sock.flush_outbox()
        check("empty-cleanup: flushed 1", flushed, 1)
        check("empty-cleanup: outbox empty", sock.pending_count(), 0)
        check(
            "empty-cleanup: file removed once empty",
            outbox_path.exists(),
            False,
        )


async def test_disk_persist_skips_corrupt_lines() -> None:
    """A tampered outbox with garbage lines still boots cleanly."""
    with tempfile.TemporaryDirectory() as td:
        outbox_path = Path(td) / "dirty.jsonl"
        outbox_path.parent.mkdir(parents=True, exist_ok=True)
        # Two valid frames bracketing one garbage line.
        valid_1 = (
            json.dumps(
                {"type": "task.accept", "correlation_id": "c1", "payload": {"task_id": "ok-1"}}
            )
            + "\n"
        ).encode()
        valid_2 = (
            json.dumps(
                {"type": "task.accept", "correlation_id": "c2", "payload": {"task_id": "ok-2"}}
            )
            + "\n"
        ).encode()
        garbage = b"{this is not json\n"
        outbox_path.write_bytes(valid_1 + garbage + valid_2)

        sock = AgentClient("dirty", outbox_path=outbox_path)
        check("corrupt: 2 valid frames loaded", sock.pending_count(), 2)
        ids = [decode(f)["payload"]["task_id"] for f in sock._outbox]
        check("corrupt: valid payloads preserved", ids, ["ok-1", "ok-2"])


async def test_disk_persist_missing_file_is_empty_start() -> None:
    """A nonexistent outbox path → fresh empty outbox, no error."""
    with tempfile.TemporaryDirectory() as td:
        outbox_path = Path(td) / "never-written.jsonl"
        sock = AgentClient("fresh", outbox_path=outbox_path)
        check("missing: empty start", sock.pending_count(), 0)
        check("missing: no file created yet", outbox_path.exists(), False)


async def test_disk_persist_in_memory_mode_writes_nothing() -> None:
    """outbox_path=None → disk is never touched."""
    with tempfile.TemporaryDirectory() as td:
        sock = AgentClient("mem-only", outbox_path=None)
        sock._writer = DeadWriterOnDrain()  # type: ignore[assignment]
        # send_accept is strict now; send_complete still queues.
        await sock.send_complete("t1", summary="ok")
        check("in-mem: 1 queued", sock.pending_count(), 1)
        # No file anywhere under the tmpdir.
        leftovers = list(Path(td).rglob("*"))
        check("in-mem: no file written", leftovers, [])


async def main() -> int:
    await test_happy_path()
    await test_dead_writer_enqueues()
    await test_send_before_connect_enqueues()
    await test_all_exception_types_caught()
    await test_count_cap_evicts_oldest()
    await test_byte_cap_evicts_oldest()
    await test_oversized_frame_kept_when_empty()
    await test_flush_fifo_order()
    await test_partial_flush_keeps_remainder()
    await test_queueing_send_paths_route_through_outbox()
    await test_send_accept_is_strict()
    await test_disk_persist_round_trip()
    await test_disk_persist_cleared_on_empty()
    await test_disk_persist_skips_corrupt_lines()
    await test_disk_persist_missing_file_is_empty_start()
    await test_disk_persist_in_memory_mode_writes_nothing()

    print()
    if FAILURES:
        print(f"FAIL — {len(FAILURES)} check(s) failed:")
        for f in FAILURES:
            print(f"  - {f}")
        return 1
    print("PASS — outbox survives dead sockets, caps, and partial flush.")
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
