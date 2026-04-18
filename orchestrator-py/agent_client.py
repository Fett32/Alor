"""Persistent daemon socket client for Alor workers.

Mirrors the role `alor-wrapper` plays: connect once, register, stay connected,
receive task.assign envelopes, send task.accept/complete/error back.

Unlike daemon.py (one-shot RPC), this keeps the connection open and streams
envelopes in both directions.
"""

from __future__ import annotations

import asyncio
import json
import os
import sys
import tempfile
import uuid
from collections import deque
from dataclasses import dataclass
from pathlib import Path
from typing import Any, AsyncIterator

DAEMON_SOCKET = "/tmp/alor/daemon.sock"


def default_outbox_path(agent_id: str) -> Path:
    """Canonical on-disk outbox location for an agent.

    `$XDG_DATA_HOME/alor/outbox/<agent_id>.jsonl`, defaulting to
    `~/.local/share/alor/outbox/<agent_id>.jsonl`. Matches the Rust
    daemon's `data_dir()` convention (src-tauri/src/daemon/session.rs)
    so both halves of Alor read/write under the same root.

    Callers are free to pass an explicit path instead — tests do.
    """
    base = os.environ.get("XDG_DATA_HOME")
    root = Path(base) if base else Path.home() / ".local" / "share"
    return root / "alor" / "outbox" / f"{agent_id}.jsonl"

# Match daemon.py's 16 MiB read buffer so task.assign envelopes carrying
# long TASK BRIEF descriptions (or any envelope above 64 KiB) don't trip
# asyncio.StreamReader's default limit with "Separator is found, but
# chunk is longer than limit".
SOCKET_READ_LIMIT = 16 * 1024 * 1024

# Outbox caps. A worker whose daemon dies mid-`task.complete` used to
# silently lose the envelope; now the send queues under one of these
# caps and flushes on reconnect.
#
# Persistence: the outbox is mirrored to disk at
# `default_outbox_path(agent_id)` (JSONL, one frame per line). On
# AgentClient construction the file is read back so a full process
# restart — which happens during dogfood reboots where kill_all_agents
# SIGKILLs the Python worker — doesn't lose queued completions. In-
# memory operation is still supported by passing `outbox_path=None`;
# the tests rely on that path for isolation.
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

    def __init__(
        self,
        agent_id: str,
        socket_path: str = DAEMON_SOCKET,
        outbox_path: Path | None = None,
    ) -> None:
        self.agent_id = agent_id
        self.socket_path = socket_path
        self._reader: asyncio.StreamReader | None = None
        self._writer: asyncio.StreamWriter | None = None
        # Outbox of pre-serialized frames whose send hit a dead/unwritable
        # socket. `send()` appends here instead of raising so the worker's
        # turn doesn't blow up; `daemon_loop` drains via `flush_outbox()`
        # after a successful reconnect.
        #
        # When `outbox_path` is set, the deque is mirrored to an on-disk
        # JSONL file after every mutation and rehydrated from it on
        # construction. This is what lets a dogfood reboot (pkill -9 of
        # the worker) replay a queued task.complete once the new worker
        # process comes up. Pass `outbox_path=None` for purely in-memory
        # operation (tests).
        self._outbox: deque[bytes] = deque()
        self._outbox_bytes: int = 0
        self._outbox_dropped: int = 0
        self._outbox_path: Path | None = outbox_path
        if self._outbox_path is not None:
            self._load_outbox_from_disk()

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

        On every successful mutation the deque is flushed to disk (when
        an outbox path was configured) so a worker reboot doesn't lose
        queued frames.
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
        self._persist_outbox()

    # --- on-disk persistence --------------------------------------------
    #
    # Format: the file is a concatenation of the exact frames as they go
    # over the wire — each frame is already newline-terminated JSON, so
    # the file reads as JSONL for free. No header, no metadata: if the
    # file exists it's a valid outbox; if it's empty or missing, the
    # outbox is empty.

    def _load_outbox_from_disk(self) -> None:
        """Rehydrate the deque from disk on construction.

        Silently skips a missing file (the common cold-start case).
        Logs and ignores a corrupted file — a worker that can't parse
        its stale outbox should still come up clean rather than crash.
        Subject to the same entry/byte caps as in-memory enqueue, so a
        tampered file can't force the process over its limits.
        """
        path = self._outbox_path
        if path is None or not path.exists():
            return
        try:
            raw = path.read_bytes()
        except OSError as e:
            print(
                f"[agent_client] failed to read outbox {path}: {e!r}; "
                f"continuing with empty outbox",
                file=sys.stderr,
            )
            return
        if not raw:
            return
        # Split on newlines; each frame already ends in one so the
        # trailing split yields an empty string we drop.
        loaded = 0
        for line in raw.split(b"\n"):
            if not line:
                continue
            # Validate by parsing — a frame that can't parse as JSON
            # can't be a valid envelope; skip it rather than queue a
            # poison pill the daemon will choke on.
            try:
                json.loads(line)
            except (ValueError, UnicodeDecodeError) as e:
                print(
                    f"[agent_client] skipping corrupt outbox line "
                    f"({len(line)} bytes, {e!r})",
                    file=sys.stderr,
                )
                continue
            # Re-terminate with newline since we split it off.
            frame = line + b"\n"
            # Cap enforcement is done via _enqueue's direct appends —
            # but we want to avoid re-triggering _persist_outbox() for
            # each reloaded frame (N^2 I/O). Append manually under cap.
            if (
                len(self._outbox) >= OUTBOX_MAX_ENTRIES
                or (
                    self._outbox
                    and self._outbox_bytes + len(frame) > OUTBOX_MAX_BYTES
                )
            ):
                # File exceeded caps — drop oldest to stay under. Rare;
                # would require someone to have written a too-big file
                # while the worker was down.
                dropped = self._outbox.popleft()
                self._outbox_bytes -= len(dropped)
                self._outbox_dropped += 1
            self._outbox.append(frame)
            self._outbox_bytes += len(frame)
            loaded += 1
        if loaded > 0:
            print(
                f"[agent_client] rehydrated {loaded} frame(s) "
                f"({self._outbox_bytes} bytes) from {path}",
                file=sys.stderr,
            )

    def _persist_outbox(self) -> None:
        """Atomically write the current deque to disk.

        Temp-file + rename so a crash mid-write can't leave a half-
        truncated outbox. When the deque is empty the file is removed
        (clean shutdown tidies up after itself). Errors are logged
        but non-fatal: losing persistence is worse than crashing the
        worker.
        """
        path = self._outbox_path
        if path is None:
            return
        try:
            path.parent.mkdir(parents=True, exist_ok=True)
            if not self._outbox:
                # Empty outbox — remove the file entirely.
                try:
                    path.unlink()
                except FileNotFoundError:
                    pass
                return
            # Build the full payload in memory (bounded by OUTBOX_MAX_BYTES
                # + per-frame newlines already included).
            payload = b"".join(self._outbox)
            # Atomic write: temp file in the same directory → rename.
            # delete=False so we can rename; we clean up on the error
            # path ourselves.
            tmp_fd, tmp_name = tempfile.mkstemp(
                prefix=".outbox.",
                suffix=".tmp",
                dir=str(path.parent),
            )
            try:
                with os.fdopen(tmp_fd, "wb") as f:
                    f.write(payload)
                    f.flush()
                    os.fsync(f.fileno())
                os.replace(tmp_name, path)
            except Exception:
                # Best-effort cleanup of the temp file if rename failed.
                try:
                    os.unlink(tmp_name)
                except FileNotFoundError:
                    pass
                raise
        except OSError as e:
            print(
                f"[agent_client] failed to persist outbox to {path}: "
                f"{e!r}",
                file=sys.stderr,
            )

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
        except OSError as e:
            # Socket died. Stash and let the reconnect path replay.
            #
            # OSError is the common parent of every transport failure
            # Python raises on a dead Unix socket:
            #   - BrokenPipeError / ConnectionResetError / ConnectionError
            #     (previous narrow tuple — all subclasses of OSError).
            #   - Plain `OSError(errno=EPIPE/EBADF/ENOTCONN/EIO/...)` —
            #     the variants that slipped through the narrow tuple
            #     during a daemon reboot and dropped the frame on the
            #     floor without queuing.
            # Not broadening to `Exception`: real programming bugs
            # (KeyError, TypeError, etc.) should still surface.
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
            except OSError as e:
                # Same rationale as `send` — catch the OSError family
                # (covers ConnectionError subclasses and plain errno
                # variants), leave real programming bugs to propagate.
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
            # Persist after every successful pop so a crash mid-flush
            # doesn't cause the drained frame to replay next boot (the
            # daemon is idempotent on replays anyway, but tidier this
            # way). When the outbox empties this will delete the file.
            self._persist_outbox()
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
