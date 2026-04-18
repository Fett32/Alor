"""Regression test for FrameWedgedError surfaced by agent_send_message_await.

Companion to test_frame_wedge.py (which proves the worker fires the
worker.frame_wedged event). This test drives the other side: given that
event arrives on the orch's `cli.event.stream` subscription carrying
`dropped_uuid == our correlation_id`, daemon.py must raise
`FrameWedgedError` instead of letting the await burn down to timeout.

We stand up a tiny asyncio unix-socket server that handles both
connections `agent_send_message_await` opens — the event-stream
subscription and the `cli.agent.send_message` RPC — and coordinates
between them via a shared state dict so the frame_wedged event it
pushes carries the same correlation_id the send handler just minted.

Run standalone: `python3 test_frame_wedged_event.py` from
orchestrator-py/. Exits 0 on pass. No pytest dependency.

Covers:
  - Matching wedge event -> FrameWedgedError with populated fields.
  - Wedge event for a DIFFERENT dropped_uuid is ignored (no false raise),
    and the caller still observes the legitimate orch_response that
    arrives afterwards.
"""

from __future__ import annotations

import asyncio
import json
import os
import sys
import tempfile
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

import daemon  # noqa: E402
from daemon import FrameWedgedError  # noqa: E402


# ---------------------------------------------------------------------------
# Canned-response unix server
# ---------------------------------------------------------------------------


class Coordinator:
    """Shared state across the two handler invocations.

    `agent_send_message_await` opens the event stream FIRST, then fires
    the send on a second connection. The send handler mints a corrid
    and stashes it here; the stream handler (which is parked on
    `send_ready.wait()`) then pushes the scripted event carrying that
    corrid as `dropped_uuid`.
    """

    def __init__(self, *, stream_events: list[dict]) -> None:
        self.send_ready = asyncio.Event()
        self.corrid: str | None = None
        # Events to write on the stream connection after the send lands.
        # Each entry may reference `{corrid}` in its `dropped_uuid` /
        # `correlation_id` fields (substituted with the actual corrid
        # post-send so the event appears to come from the daemon).
        self.stream_events = stream_events


async def _run_server(path: str, coord: Coordinator) -> asyncio.base_events.Server:
    async def handler(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        try:
            req_line = await reader.readline()
            if not req_line:
                return
            try:
                req = json.loads(req_line.decode().strip())
            except (UnicodeDecodeError, json.JSONDecodeError):
                return
            req_kind = req.get("type")
            req_corrid = req.get("correlation_id", "")

            if req_kind == "cli.event.stream":
                # Wait for the send to land and stash its corrid, then
                # push scripted events with {corrid} substituted.
                await coord.send_ready.wait()
                for evt in coord.stream_events:
                    resolved = json.loads(
                        json.dumps(evt).replace("{corrid}", coord.corrid or "")
                    )
                    env = {
                        "type": "event",
                        "correlation_id": str(uuid.uuid4()),
                        "payload": {
                            "event": resolved["event"],
                            "data": resolved["data"],
                        },
                    }
                    writer.write((json.dumps(env) + "\n").encode())
                    await writer.drain()
                # Keep the stream open — daemon.py closes it via the
                # outer try/finally once it decides what to do.
                # Block on read so we don't EOF the orch prematurely.
                try:
                    await reader.read(1)
                except Exception:
                    pass

            elif req_kind == "cli.agent.send_message":
                # Mint a corrid (as the real daemon would on suppress_echo)
                # and unblock the stream handler.
                coord.corrid = str(uuid.uuid4())
                resp = {
                    "type": "cli.response",
                    "correlation_id": req_corrid,
                    "payload": {
                        "sent": (req.get("payload") or {}).get("agent_id"),
                        "submit": (req.get("payload") or {}).get("submit", False),
                        "correlation_id": coord.corrid,
                    },
                }
                writer.write((json.dumps(resp) + "\n").encode())
                await writer.drain()
                coord.send_ready.set()

            else:
                # Unknown — just close.
                pass
        finally:
            writer.close()
            try:
                await writer.wait_closed()
            except Exception:
                pass

    return await asyncio.start_unix_server(handler, path=path)


# ---------------------------------------------------------------------------
# Test harness
# ---------------------------------------------------------------------------


def assert_eq(label: str, got, want) -> None:
    if got != want:
        print(f"FAIL  {label}")
        print(f"  got : {got!r}")
        print(f"  want: {want!r}")
        raise SystemExit(1)
    print(f"ok    {label}")


def assert_is(label: str, got, want_type) -> None:
    if not isinstance(got, want_type):
        print(f"FAIL  {label}")
        print(f"  got : {type(got).__name__}: {got!r}")
        print(f"  want: isinstance of {want_type.__name__}")
        raise SystemExit(1)
    print(f"ok    {label}")


async def run_case_matching_wedge(tmpdir: str) -> None:
    sock_path = os.path.join(tmpdir, "wedge-match.sock")
    daemon.DAEMON_SOCKET = sock_path

    new_uuid = "bbbbbbbb-bbbb-4bbb-bbbb-bbbbbbbbbbbb"
    coord = Coordinator(
        stream_events=[
            {
                "event": "worker.frame_wedged",
                "data": {
                    "agent_id": "alor-test",
                    "dropped_uuid": "{corrid}",  # substituted to caller's corrid
                    "new_uuid": new_uuid,
                    "bytes_dropped": 42,
                    "lines_dropped": 3,
                    "task_id": None,
                },
            }
        ]
    )
    server = await _run_server(sock_path, coord)
    try:
        raised: BaseException | None = None
        try:
            await daemon.agent_send_message_await(
                "alor-test", "hi from a doomed frame", submit=True, timeout=2.0
            )
        except BaseException as e:  # noqa: BLE001
            raised = e

        assert_is("matching wedge -> FrameWedgedError", raised, FrameWedgedError)
        # Fields lift through from the event payload.
        assert_eq("dropped_uuid == caller's corrid", raised.dropped_uuid, coord.corrid)
        assert_eq("new_uuid lifted from event", raised.new_uuid, new_uuid)
        assert_eq("bytes_dropped lifted", raised.bytes_dropped, 42)
        assert_eq("lines_dropped lifted", raised.lines_dropped, 3)
        assert_eq("agent_id lifted", raised.agent_id, "alor-test")
        assert_eq("task_id lifted (None)", raised.task_id, None)
        # Message mentions the agent and the dropped/new prefixes.
        msg = str(raised)
        assert_eq("message mentions agent", "alor-test" in msg, True)
        assert_eq("message mentions 'retry'", "retry" in msg.lower(), True)
    finally:
        server.close()
        await server.wait_closed()


async def run_case_unrelated_wedge_ignored(tmpdir: str) -> None:
    """A wedge event carrying some OTHER corrid must not raise. The
    caller should still receive its own orch_response normally."""
    sock_path = os.path.join(tmpdir, "wedge-unrelated.sock")
    daemon.DAEMON_SOCKET = sock_path

    coord = Coordinator(
        stream_events=[
            # Wedge event for a totally unrelated frame — must not raise.
            {
                "event": "worker.frame_wedged",
                "data": {
                    "agent_id": "alor-test",
                    "dropped_uuid": "deadbeef-dead-beef-dead-beefdeadbeef",
                    "new_uuid": "cafebabe-cafe-babe-cafe-babecafebabe",
                    "bytes_dropped": 7,
                    "lines_dropped": 1,
                    "task_id": None,
                },
            },
            # Then the legitimate orch_response for our send.
            {
                "event": "worker.orch_response",
                "data": {
                    "agent_id": "alor-test",
                    "correlation_id": "{corrid}",
                    "text": "ok",
                    "during_task": False,
                    "task_id": None,
                },
            },
        ]
    )
    server = await _run_server(sock_path, coord)
    try:
        result = await daemon.agent_send_message_await(
            "alor-test", "hi from a healthy frame", submit=True, timeout=2.0
        )
        assert_eq("unrelated wedge did not raise", True, True)
        assert_eq("reply timeout flag False", result["timeout"], False)
        assert_eq("reply text is 'ok'", result["text"], "ok")
        assert_eq("correlation_id matches minted", result["correlation_id"], coord.corrid)
    finally:
        server.close()
        await server.wait_closed()


async def main() -> int:
    original_socket = daemon.DAEMON_SOCKET
    try:
        with tempfile.TemporaryDirectory(prefix="alor-test-wedge-") as tmpdir:
            await run_case_matching_wedge(tmpdir)
            await run_case_unrelated_wedge_ignored(tmpdir)
    finally:
        daemon.DAEMON_SOCKET = original_socket

    print()
    print("PASS — FrameWedgedError surfaces on matching dropped_uuid; unrelated wedges ignored.")
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
