"""Unix socket client for the Alor daemon.

Wire protocol: newline-delimited JSON envelopes.
Envelope: {"type": "<kind>", "correlation_id": "<uuid>", "payload": {...}}
Socket: /tmp/alor/daemon.sock
"""

from __future__ import annotations

import asyncio
import json
import uuid
from dataclasses import dataclass
from typing import Any, AsyncIterator

DAEMON_SOCKET = "/tmp/alor/daemon.sock"
REQUEST_TIMEOUT_S = 30.0


class DaemonError(RuntimeError):
    pass


def _envelope(kind: str, payload: dict[str, Any] | None = None) -> dict[str, Any]:
    return {
        "type": kind,
        "correlation_id": str(uuid.uuid4()),
        "payload": payload or {},
    }


async def _one_shot(kind: str, payload: dict[str, Any] | None = None) -> dict[str, Any]:
    """Send one request, read one response, close the connection."""
    try:
        reader, writer = await asyncio.open_unix_connection(DAEMON_SOCKET)
    except (FileNotFoundError, ConnectionRefusedError) as e:
        raise DaemonError(f"daemon not reachable at {DAEMON_SOCKET}: {e}") from e

    try:
        line = (json.dumps(_envelope(kind, payload)) + "\n").encode()
        writer.write(line)
        await writer.drain()
        resp_line = await asyncio.wait_for(
            reader.readline(), timeout=REQUEST_TIMEOUT_S
        )
        if not resp_line:
            raise DaemonError("daemon closed connection without response")
        resp = json.loads(resp_line.decode().strip())
        if resp.get("type") in ("error", "cli.error"):
            err = (resp.get("payload") or {}).get("error", "unknown daemon error")
            raise DaemonError(f"daemon error: {err}")
        return resp.get("payload") or {}
    finally:
        writer.close()
        try:
            await writer.wait_closed()
        except Exception:
            pass


# ---- Typed wrappers ----

async def task_create(title: str, description: str, project: str | None = None) -> str:
    payload = {"title": title, "description": description}
    if project:
        payload["project"] = project
    resp = await _one_shot("cli.task.create", payload)
    task_id = resp.get("task_id")
    if not task_id:
        raise DaemonError("task.create: missing task_id in response")
    return task_id


async def task_assign(task_id: str, agent_id: str) -> dict[str, Any]:
    return await _one_shot("cli.assign", {"task_id": task_id, "agent_id": agent_id})


async def task_get(task_id: str) -> dict[str, Any]:
    return await _one_shot("cli.task.get", {"task_id": task_id})


async def task_list() -> dict[str, Any]:
    return await _one_shot("cli.task.list")


async def task_cancel(task_id: str) -> dict[str, Any]:
    return await _one_shot("cli.task.cancel", {"task_id": task_id})


async def status() -> dict[str, Any]:
    return await _one_shot("cli.status")


async def project_get(name: str) -> dict[str, Any]:
    return await _one_shot("cli.project.get", {"name": name})


async def project_list() -> dict[str, Any]:
    return await _one_shot("cli.project.list")


async def agent_send_message(
    agent_id: str, text: str, submit: bool = False
) -> dict[str, Any]:
    return await _one_shot(
        "cli.agent.send_message",
        {"agent_id": agent_id, "text": text, "submit": submit},
    )


async def memory_get(project: str) -> dict[str, Any]:
    return await _one_shot("cli.memory.get", {"project": project})


async def agent_spawn(
    agent: str, name: str | None = None, role: str | None = None
) -> dict[str, Any]:
    payload: dict[str, Any] = {"agent": agent}
    if name:
        payload["name"] = name
    if role:
        payload["role"] = role
    return await _one_shot("cli.spawn", payload)


async def agent_kill(instance: str) -> dict[str, Any]:
    return await _one_shot("cli.kill", {"instance": instance})


async def agent_ensure_running(agent_id: str) -> dict[str, Any]:
    return await _one_shot("cli.agent.ensure_running", {"agent_id": agent_id})


# ---- Event stream ----

@dataclass
class Event:
    event: str
    data: dict[str, Any]
    timestamp: str | None = None


async def event_stream() -> AsyncIterator[Event]:
    """Yield events as they arrive. Auto-reconnect on drop.

    Usage:
        async for evt in event_stream():
            ...
    """
    while True:
        try:
            reader, writer = await asyncio.open_unix_connection(DAEMON_SOCKET)
        except (FileNotFoundError, ConnectionRefusedError):
            await asyncio.sleep(2.0)
            continue

        try:
            writer.write((json.dumps(_envelope("cli.event.stream")) + "\n").encode())
            await writer.drain()
            while True:
                line = await reader.readline()
                if not line:
                    break
                try:
                    env = json.loads(line.decode().strip())
                    payload = env.get("payload") or {}
                    yield Event(
                        event=payload.get("event", "?"),
                        data=payload.get("data") or {},
                        timestamp=payload.get("timestamp"),
                    )
                except (json.JSONDecodeError, KeyError):
                    continue
        finally:
            writer.close()
            try:
                await writer.wait_closed()
            except Exception:
                pass
        await asyncio.sleep(2.0)
