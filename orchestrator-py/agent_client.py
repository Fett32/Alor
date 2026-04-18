"""Persistent daemon socket client for Alor workers.

Mirrors the role `alor-wrapper` plays: connect once, register, stay connected,
receive task.assign envelopes, send task.accept/complete/error back.

Unlike daemon.py (one-shot RPC), this keeps the connection open and streams
envelopes in both directions.
"""

from __future__ import annotations

import asyncio
import json
import sys
import uuid
from collections import deque
from dataclasses import dataclass
from typing import Any, AsyncIterator

DAEMON_SOCKET = "/tmp/alor/daemon.sock"

# Match daemon.py's 16 MiB read buffer so task.assign envelopes carrying
# long TASK BRIEF descriptions (or any envelope above 64 KiB) don't trip
# asyncio.StreamReader's default limit with "Separator is found, but
# chunk is longer than limit".
SOCKET_READ_LIMIT = 16 * 1024 * 1024

# In-memory outbox caps. A worker whose daemon dies mid-`task.complete`
# used to silently lose the envelope; now the send queues under one of
# these caps and flushes on reconnect. Kept in-memory because the worker
# ignores SIGHUP (see worker.py main) and survives daemon cycles intact.
OUTBOX_MAX_ENTRIES = 100
OUTBOX_MAX_BYTES = 1 * 1024 * 1024  # 1 MiB

# Wire-protocol constants — must match wrapper/src/protocol.rs
MSG_REGISTER = "wrapper.register"
MSG_TASK_ASSIGN = "task.assign"
MSG_TASK_ACCEPT = "task.accept"
MSG_TASK_COMPLETE = "task.complete"
MSG_WRAPPER_ERROR = "wrapper.error"
MSG_STATUS_REQUEST = "status.request"
MSG_STATUS_RESPONSE = "status.response"
MSG_SHUTDOWN = "daemon.shutdown"
MSG_USER_INTERVENTION = "user.intervention"
MSG_WORKER_USER_INPUT = "worker.user_input"
MSG_WORKER_ORCH_RESPONSE = "worker.orch_response"
MSG_WORKER_FRAME_WEDGED = "worker.frame_wedged"


class AgentClientError(RuntimeError):
    pass


@dataclass
class Envelope:
    kind: str
    correlation_id: str
    payload: dict[str, Any]

    def to_json(self) -> bytes:
        return (
            json.dumps(
                {
                    "type": self.kind,
                    "correlation_id": self.correlation_id,
                    "payload": self.payload,
                }
            )
            + "\n"
        ).encode()

    @classmethod
    def from_line(cls, line: str) -> "Envelope":
        obj = json.loads(line)
        return cls(
            kind=obj["type"],
            correlation_id=obj.get("correlation_id", ""),
            payload=obj.get("payload") or {},
        )

    @staticmethod
    def new(kind: str, payload: dict[str, Any] | None = None) -> "Envelope":
        return Envelope(kind=kind, correlation_id=str(uuid.uuid4()), payload=payload or {})


class AgentClient:
    """Persistent connection to the Alor daemon for a single agent slot."""

    def __init__(self, agent_id: str, socket_path: str = DAEMON_SOCKET) -> None:
        self.agent_id = agent_id
        self.socket_path = socket_path
        self._reader: asyncio.StreamReader | None = None
        self._writer: asyncio.StreamWriter | None = None
        # Outbox of pre-serialized frames whose send hit a dead/unwritable
        # socket. `send()` appends here instead of raising so the worker's
        # turn doesn't blow up; `daemon_loop` drains via `flush_outbox()`
        # after a successful reconnect. In-memory only — see module-level
        # cap constants and the comment there for why.
        self._outbox: deque[bytes] = deque()
        self._outbox_bytes: int = 0
        self._outbox_dropped: int = 0

    # --- outbox introspection (used by reconnect path + tests) ----------

    def pending_count(self) -> int:
        """Number of frames currently queued for retry."""
        return len(self._outbox)

    def pending_bytes(self) -> int:
        """Total bytes currently queued."""
        return self._outbox_bytes

    def dropped_count(self) -> int:
        """Cumulative frames evicted due to cap overflow."""
        return self._outbox_dropped

    def _enqueue(self, frame: bytes) -> None:
        """Append `frame` to the outbox, evicting oldest on cap breach.

        Two caps enforced: hard 100-entry ceiling and 1 MiB byte ceiling.
        Eviction is FIFO so the oldest (most likely stale) frame is
        dropped first. A single frame larger than the byte cap is still
        accepted when the outbox is empty — losing it outright is strictly
        worse than keeping one oversized envelope.
        """
        # Count cap.
        while len(self._outbox) >= OUTBOX_MAX_ENTRIES:
            dropped = self._outbox.popleft()
            self._outbox_bytes -= len(dropped)
            self._outbox_dropped += 1
            print(
                f"[agent_client] outbox full (entries); dropped oldest "
                f"({len(dropped)} bytes)",
                file=sys.stderr,
            )
        # Bytes cap. Stop evicting once the outbox is empty — the new
        # frame alone may exceed the cap, and keeping it beats losing it.
        while self._outbox and self._outbox_bytes + len(frame) > OUTBOX_MAX_BYTES:
            dropped = self._outbox.popleft()
            self._outbox_bytes -= len(dropped)
            self._outbox_dropped += 1
            print(
                f"[agent_client] outbox full (bytes); dropped oldest "
                f"({len(dropped)} bytes)",
                file=sys.stderr,
            )
        self._outbox.append(frame)
        self._outbox_bytes += len(frame)

    async def connect_and_register(self) -> None:
        """Open the socket and send the register envelope.

        The daemon's response (or lack thereof) is handled by the caller's
        recv loop — we don't block here, matching alor-wrapper's behavior.
        """
        self._reader, self._writer = await asyncio.open_unix_connection(
            self.socket_path, limit=SOCKET_READ_LIMIT
        )
        env = Envelope.new(MSG_REGISTER, {"agent_id": self.agent_id})
        self._writer.write(env.to_json())
        await self._writer.drain()

    async def send(self, kind: str, payload: dict[str, Any]) -> None:
        """Send an envelope, or queue it if the socket is dead.

        This is the choke point for all six `send_*` wrappers, so a daemon
        restart mid-turn no longer loses the envelope: on write/drain
        failure we append the serialized frame to the outbox and return
        normally. Callers don't raise, callers don't know. The reconnect
        path in `worker.daemon_loop` drains via `flush_outbox()`.
        """
        env = Envelope.new(kind, payload)
        frame = env.to_json()
        if self._writer is None:
            # Not connected — queue. Reconnect path will flush.
            self._enqueue(frame)
            return
        try:
            self._writer.write(frame)
            await self._writer.drain()
        except (BrokenPipeError, ConnectionResetError, ConnectionError) as e:
            # Socket died. Stash and let the reconnect path replay.
            print(
                f"[agent_client] send {kind} failed ({e!r}); "
                f"queued ({len(frame)} bytes, {self.pending_count() + 1} pending)",
                file=sys.stderr,
            )
            self._enqueue(frame)

    async def flush_outbox(self) -> int:
        """Replay queued frames in FIFO order. Returns number sent.

        Stops on the first write/drain failure and leaves the failing
        frame + remainder in the queue — caller (reconnect path) retries
        by reconnecting and calling again. If not currently connected,
        returns 0 without touching the queue.
        """
        if self._writer is None:
            return 0
        sent = 0
        while self._outbox:
            frame = self._outbox[0]
            try:
                self._writer.write(frame)
                await self._writer.drain()
            except (BrokenPipeError, ConnectionResetError, ConnectionError) as e:
                print(
                    f"[agent_client] flush stalled after {sent} frame(s) "
                    f"({e!r}); {self.pending_count()} still queued",
                    file=sys.stderr,
                )
                return sent
            # Only pop after a successful drain — on failure we retain
            # the frame so the next flush can retry from where we stopped.
            self._outbox.popleft()
            self._outbox_bytes -= len(frame)
            sent += 1
        return sent

    async def send_accept(self, task_id: str) -> None:
        await self.send(MSG_TASK_ACCEPT, {"task_id": task_id})

    async def send_complete(
        self, task_id: str, summary: str | None = None, output: Any = None
    ) -> None:
        payload: dict[str, Any] = {"task_id": task_id}
        if summary is not None:
            payload["summary"] = summary
        if output is not None:
            payload["output"] = output
        await self.send(MSG_TASK_COMPLETE, payload)

    async def send_error(self, message: str) -> None:
        await self.send(
            MSG_WRAPPER_ERROR,
            {"agent_id": self.agent_id, "message": message},
        )

    async def send_worker_user_input(
        self,
        text: str,
        during_task: bool,
        task_id: str | None,
    ) -> None:
        """Forward a line of Fett's stdin to the daemon as a broadcast event.

        The worker feeds the same line into its local SDK client too — this
        only makes the orch aware that something was said. Slash commands
        and sentinel-prefixed (orch-origin) lines are filtered by the
        caller.
        """
        payload: dict[str, Any] = {
            "agent_id": self.agent_id,
            "text": text,
            "during_task": during_task,
        }
        if task_id is not None:
            payload["task_id"] = task_id
        await self.send(MSG_WORKER_USER_INPUT, payload)

    async def send_worker_orch_response(
        self,
        correlation_id: str,
        text: str,
        during_task: bool,
        task_id: str | None,
    ) -> None:
        """Forward the SDK turn result for a sentinel-prefixed orch send
        back to the daemon as a broadcast event.

        Mirror of `send_worker_user_input` for the orch→worker→orch reply
        direction. `correlation_id` is the uuid the daemon embedded in the
        sentinel on the outbound send; orch uses it to match this reply
        against its originating `agent_send_message` call.
        """
        payload: dict[str, Any] = {
            "agent_id": self.agent_id,
            "correlation_id": correlation_id,
            "text": text,
            "during_task": during_task,
        }
        if task_id is not None:
            payload["task_id"] = task_id
        await self.send(MSG_WORKER_ORCH_RESPONSE, payload)

    async def send_worker_frame_wedged(
        self,
        dropped_uuid: str,
        new_uuid: str,
        bytes_dropped: int,
        lines_dropped: int,
        task_id: str | None,
    ) -> None:
        """Tell the daemon a stale BEGIN frame was discarded by the stdin
        state machine's nested-BEGIN recovery path.

        Routes through the outbox like every other send_*, so an outage
        during the wedge doesn't further compound the failure: the stale
        caller's orch still surfaces a timeout (not a wedge), but the
        event lands as soon as the daemon socket recovers.

        `dropped_uuid` is the correlation_id the orch stamped into the
        stale BEGIN; the orch matches on it to raise FrameWedgedError
        against the right pending send.
        """
        payload: dict[str, Any] = {
            "agent_id": self.agent_id,
            "dropped_uuid": dropped_uuid,
            "new_uuid": new_uuid,
            "bytes_dropped": bytes_dropped,
            "lines_dropped": lines_dropped,
        }
        if task_id is not None:
            payload["task_id"] = task_id
        await self.send(MSG_WORKER_FRAME_WEDGED, payload)

    async def send_status(
        self, task_id: str | None, alive: bool, details: str | None = None
    ) -> None:
        await self.send(
            MSG_STATUS_RESPONSE,
            {
                "agent_id": self.agent_id,
                "task_id": task_id,
                "alive": alive,
                "details": details,
            },
        )

    async def recv(self) -> Envelope | None:
        """Read one envelope. Returns None on EOF. Raises on malformed JSON."""
        if self._reader is None:
            raise AgentClientError("not connected")
        line = await self._reader.readline()
        if not line:
            return None
        return Envelope.from_line(line.decode().strip())

    async def recv_forever(self) -> AsyncIterator[Envelope]:
        """Iterator form — yields until EOF. Skips malformed envelopes instead
        of killing the whole worker loop."""
        while True:
            try:
                env = await self.recv()
            except (json.JSONDecodeError, KeyError, UnicodeDecodeError) as e:
                # One garbage line shouldn't take down the worker.
                import sys
                print(f"[agent_client] skipping malformed envelope: {e}", file=sys.stderr)
                continue
            if env is None:
                return
            yield env

    async def close(self) -> None:
        if self._writer is not None:
            self._writer.close()
            try:
                await self._writer.wait_closed()
            except Exception:
                pass
            self._writer = None
            self._reader = None
