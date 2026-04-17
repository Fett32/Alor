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
    agent_id: str,
    text: str,
    submit: bool = False,
    suppress_echo: bool = True,
) -> dict[str, Any]:
    """Inject text into an agent's tmux pane.

    `suppress_echo` defaults to True because the only current caller is the
    orchestrator's own `agent_send_message` tool — orch→worker messages
    must not round-trip back as `worker.user_input` events, which would
    echo-loop into the orch's SDK context. Set to False only if you
    specifically want the send to register as a user-origin event
    (currently no legitimate case; the flag exists so wrapper-runtime
    agents stay unaffected if future callers opt out).

    When `suppress_echo` is True the daemon stamps a uuid into the
    sentinel prefix and returns it as `correlation_id` in the response;
    the SDK worker echoes the same id back in the subsequent
    `worker.orch_response` event so callers can match the reply.
    """
    return await _one_shot(
        "cli.agent.send_message",
        {
            "agent_id": agent_id,
            "text": text,
            "submit": submit,
            "suppress_echo": suppress_echo,
        },
    )


async def agent_send_message_await(
    agent_id: str,
    text: str,
    submit: bool = True,
    timeout: float = 60.0,
) -> dict[str, Any]:
    """Fire `agent_send_message` and await the matching `worker.orch_response`.

    Turns the fire-and-forget inject into a real query/response primitive.
    `submit` defaults to True here (different from `agent_send_message`) —
    you almost always want Enter appended so the worker's stdin loop
    actually processes the line.

    Return shape:
        {
            "correlation_id": <uuid str>,
            "timeout": <bool>,
            "text": <reply text or None>,
            "agent_id": <str or None>,
            "during_task": <bool or None>,
            "task_id": <str or None>,
        }

    On timeout, `text` is None and `timeout` is True — the caller decides
    whether to retry, surface the timeout to Fett, etc. Does not raise on
    timeout (it's a valid outcome: worker busy / SDK stalled / reply got
    dropped).

    Implementation: opens its own `cli.event.stream` subscription BEFORE
    firing the send to close the race window where the reply fires before
    we subscribe. The orch's primary event_watcher is unaffected — the
    daemon broadcasts to all subscribers, so both see the event.
    """
    try:
        reader, writer = await asyncio.open_unix_connection(DAEMON_SOCKET)
    except (FileNotFoundError, ConnectionRefusedError) as e:
        raise DaemonError(f"daemon not reachable at {DAEMON_SOCKET}: {e}") from e

    try:
        writer.write((json.dumps(_envelope("cli.event.stream")) + "\n").encode())
        await writer.drain()

        # Fire the send AFTER subscribing. Response carries the
        # correlation_id the daemon stamped into the sentinel.
        send_resp = await agent_send_message(
            agent_id, text, submit=submit, suppress_echo=True
        )
        corrid = send_resp.get("correlation_id")
        if not corrid:
            raise DaemonError(
                "agent_send_message did not return correlation_id "
                "(is suppress_echo disabled or daemon too old?)"
            )

        loop = asyncio.get_event_loop()
        deadline = loop.time() + timeout
        while True:
            remaining = deadline - loop.time()
            if remaining <= 0:
                return {
                    "correlation_id": corrid,
                    "timeout": True,
                    "text": None,
                    "agent_id": None,
                    "during_task": None,
                    "task_id": None,
                }
            try:
                line = await asyncio.wait_for(reader.readline(), timeout=remaining)
            except asyncio.TimeoutError:
                return {
                    "correlation_id": corrid,
                    "timeout": True,
                    "text": None,
                    "agent_id": None,
                    "during_task": None,
                    "task_id": None,
                }
            if not line:
                return {
                    "correlation_id": corrid,
                    "timeout": True,
                    "text": None,
                    "agent_id": None,
                    "during_task": None,
                    "task_id": None,
                }
            try:
                env = json.loads(line.decode().strip())
            except (json.JSONDecodeError, UnicodeDecodeError):
                continue
            payload = env.get("payload") or {}
            if payload.get("event") != "worker.orch_response":
                continue
            data = payload.get("data") or {}
            if data.get("correlation_id") != corrid:
                continue
            return {
                "correlation_id": corrid,
                "timeout": False,
                "text": data.get("text"),
                "agent_id": data.get("agent_id"),
                "during_task": data.get("during_task"),
                "task_id": data.get("task_id"),
            }
    finally:
        writer.close()
        try:
            await writer.wait_closed()
        except Exception:
            pass


async def memory_get(project: str) -> dict[str, Any]:
    return await _one_shot("cli.memory.get", {"project": project})


async def agent_spawn(
    agent: str,
    name: str | None = None,
    role: str | None = None,
    project: str | None = None,
    working_dir: str | None = None,
) -> dict[str, Any]:
    payload: dict[str, Any] = {"agent": agent}
    if name:
        payload["name"] = name
    if role:
        payload["role"] = role
    if project:
        payload["project"] = project
    if working_dir:
        payload["working_dir"] = working_dir
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
                except (json.JSONDecodeError, KeyError, UnicodeDecodeError) as e:
                    # Don't silently swallow parse errors — we need to see
                    # them when "why did my event never fire" is the question.
                    import sys
                    print(
                        f"[event_stream] malformed envelope, skipping: {e}",
                        file=sys.stderr,
                    )
                    continue
        finally:
            writer.close()
            try:
                await writer.wait_closed()
            except Exception:
                pass
        await asyncio.sleep(2.0)
