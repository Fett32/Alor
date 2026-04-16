"""Persistent daemon socket client for Alor workers.

Mirrors the role `alor-wrapper` plays: connect once, register, stay connected,
receive task.assign envelopes, send task.accept/complete/error back.

Unlike daemon.py (one-shot RPC), this keeps the connection open and streams
envelopes in both directions.
"""

from __future__ import annotations

import asyncio
import json
import uuid
from dataclasses import dataclass
from typing import Any, AsyncIterator

DAEMON_SOCKET = "/tmp/alor/daemon.sock"

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

    async def connect_and_register(self) -> None:
        """Open the socket and send the register envelope.

        The daemon's response (or lack thereof) is handled by the caller's
        recv loop — we don't block here, matching alor-wrapper's behavior.
        """
        self._reader, self._writer = await asyncio.open_unix_connection(self.socket_path)
        env = Envelope.new(MSG_REGISTER, {"agent_id": self.agent_id})
        self._writer.write(env.to_json())
        await self._writer.drain()

    async def send(self, kind: str, payload: dict[str, Any]) -> None:
        if self._writer is None:
            raise AgentClientError("not connected")
        env = Envelope.new(kind, payload)
        self._writer.write(env.to_json())
        await self._writer.drain()

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
