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

# asyncio.StreamReader defaults to a 64 KiB buffer per readline, which is
# the same order as our per-task summary cap — so a task_list or agent_list
# response with even a few summaries blows past the limit and raises
# "Separator is found, but chunk is longer than limit". Bump to 16 MiB to
# match the daemon's server-side cap ceiling and give headroom.
SOCKET_READ_LIMIT = 16 * 1024 * 1024


class DaemonError(RuntimeError):
    pass


# Structured `cli.error` codes the daemon stamps into `payload.code`. Keep
# in lockstep with `src-tauri/src/wrapper/protocol.rs` (ERR_CODE_*). Codes
# named here get mapped to typed exceptions below; unrecognized codes fall
# through to a generic DaemonError with the server's prose.
ERR_CODE_FRAMED_SEND_NOT_SUPPORTED = "framed_send_not_supported"


class FrameWedgedError(DaemonError):
    """Raised when the worker dropped the stale BEGIN frame carrying this
    `agent_send_message_await` caller's correlation_id before the matching
    END arrived.

    Triggered by the worker.py stdin state machine's nested-BEGIN recovery
    path: a second `agent_send_message` landed against the same worker
    while this caller's frame was still open, so the worker discarded this
    body and swapped to the new uuid. The SDK turn this caller was waiting
    on never ran — no retry-safe state was mutated on the worker side.

    Attributes:
        dropped_uuid: correlation_id of the dropped frame (== this caller's).
        new_uuid: correlation_id of the frame that interrupted us.
        bytes_dropped / lines_dropped: body size stats, mostly for logging.
        agent_id / task_id: as-reported by the worker event.

    Retry is usually safe and usually the right call, but scheduling is up
    to the caller — re-running immediately just risks wedging again if the
    same competing caller is still active.
    """

    def __init__(
        self,
        message: str,
        *,
        dropped_uuid: str,
        new_uuid: str,
        bytes_dropped: int,
        lines_dropped: int,
        agent_id: str | None,
        task_id: str | None,
    ) -> None:
        super().__init__(message)
        self.dropped_uuid = dropped_uuid
        self.new_uuid = new_uuid
        self.bytes_dropped = bytes_dropped
        self.lines_dropped = lines_dropped
        self.agent_id = agent_id
        self.task_id = task_id


class FramedSendNotSupportedError(DaemonError):
    """Raised when `cli.agent.send_message` with `suppress_echo=true`
    targets an agent whose runtime can't decode BEGIN/END framing.

    Only `claude-sdk` runtime workers implement the sentinel state machine;
    wrapper-runtime agents (codex, gemini, any unknown/unresolvable agent)
    would see literal `__ALOR_ORCH_ECHO_BEGIN__<uuid>` bytes land in their
    pty, so the daemon fail-closes with this typed error.

    Workarounds the caller can choose from:
      - retry against a claude-sdk-backed agent, or
      - call with `suppress_echo=False` to inject raw text (loses the
        orch_response correlation channel — the send becomes
        fire-and-forget).

    Tied to `ERR_CODE_FRAMED_SEND_NOT_SUPPORTED` on the Rust side.
    """


# Map server-side code strings to the exception class to raise. Extend here
# rather than bolting conditionals onto `_one_shot`.
_CLI_ERROR_CODE_TO_EXC: dict[str, type[DaemonError]] = {
    ERR_CODE_FRAMED_SEND_NOT_SUPPORTED: FramedSendNotSupportedError,
}


def _envelope(kind: str, payload: dict[str, Any] | None = None) -> dict[str, Any]:
    return {
        "type": kind,
        "correlation_id": str(uuid.uuid4()),
        "payload": payload or {},
    }


async def _one_shot(kind: str, payload: dict[str, Any] | None = None) -> dict[str, Any]:
    """Send one request, read one response, close the connection."""
    try:
        reader, writer = await asyncio.open_unix_connection(DAEMON_SOCKET, limit=SOCKET_READ_LIMIT)
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
            payload = resp.get("payload") or {}
            err = payload.get("error", "unknown daemon error")
            # `code` is present on rejections the daemon promises to keep
            # stable (see ERR_CODE_* in protocol.rs). Route those to typed
            # exceptions; fall through to a generic DaemonError for
            # uncoded errors so callers can still branch on isinstance.
            code = payload.get("code")
            if code and code in _CLI_ERROR_CODE_TO_EXC:
                raise _CLI_ERROR_CODE_TO_EXC[code](err)
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


async def task_get(task_id: str, view: str | None = None) -> dict[str, Any]:
    """Fetch one task's state, projected per `view`.

    `view` selects the response shape:
      - None (falls through to server default, "summary") / "summary"
        → routing-useful scalars (id, title, state, assigned_to,
        project, parent_task_id, created_at, updated_at,
        user_intervened) plus `has_*` booleans for the heavy fields
        (description, summary, details, proposal_brief,
        proposal_diff). ~300–500 B. Use for routine state checks.
      - "full" → every Task field including the heavy text bodies.
        Use when a `has_*` flag on a prior summary tells you
        something's worth pulling, or when you need the brief /
        proposal / full post-task report.
      - Any other string: the server silently treats it as "summary".

    Response shape: `{task: ..., view: str}`. `view` echoes back
    which shape the server actually applied.

    Server-side projection via `view` on `cli.task.get`. See
    `src-tauri/src/wrapper/protocol.rs::CliTaskGet`. Sized off audit
    8b03cae6 fix #2 — `task_get` was a P0 bloat source before the
    split.
    """
    payload: dict[str, Any] = {"task_id": task_id}
    if view is not None:
        payload["view"] = view
    return await _one_shot("cli.task.get", payload)


async def task_list(
    state: str = "default",
    limit: int | None = None,
    offset: int = 0,
    view: str | None = None,
) -> dict[str, Any]:
    """List tasks, filtered by state, paginated, projected by view.

    `state` values:
      - "default" (default) → non-terminal tasks only (hides Completed /
        Cancelled / Rejected / TimedOut / Stale). Keeps the orch's MCP
        task_list output lean.
      - "all" → every task in state.json.
      - Any SCREAMING_SNAKE_CASE state name ("COMPLETED", "CANCELLED",
        "STALE", etc.) → only tasks in that exact state.

    `limit` / `offset` paginate the filtered result. `limit=None` falls
    through to the server default (20 — sized to keep responses under
    the 25k-token orch-context ceiling at typical task sizes; see
    src-tauri/src/wrapper/protocol.rs::DEFAULT_TASK_LIST_LIMIT). Pass
    `limit=0` to disable capping (use sparingly).

    `view` selects the per-task field projection:
      - None (falls through to server default, "summary") / "summary"
        → {id, title, state, assigned_to, updated_at} only. ~70
        tokens/task — safe to pull hundreds per page for scans.
      - "full" → every Task field (description, proposal_brief,
        proposal_diff, summary, project, etc.). ~1.1k tokens/task
        average. Prefer `task_get` for single-task detail reads.
      - Any other string: the server silently treats it as "summary".

    Response shape: `{tasks: [...], total: N, returned: M, offset: O,
    has_more: bool, view: str}`. Callers paginate by re-calling with
    `offset += returned` while `has_more` is true. `view` echoes back
    which shape the server actually applied.

    Server-side filter / pagination / projection via the `state_filter`
    / `limit` / `offset` / `view` payload fields on `cli.task.list`
    (see src-tauri/src/wrapper/protocol.rs::CliTaskList).
    """
    payload: dict[str, Any] = {"state_filter": state, "offset": offset}
    if limit is not None:
        payload["limit"] = limit
    if view is not None:
        payload["view"] = view
    return await _one_shot("cli.task.list", payload)


async def task_cancel(task_id: str) -> dict[str, Any]:
    return await _one_shot("cli.task.cancel", {"task_id": task_id})


async def task_intervention_clear(task_id: str) -> dict[str, Any]:
    """Reset the `user_intervened` flag on a task.

    See src-tauri/src/daemon/state.rs::Task docstring: the flag is
    informational-only (nothing in the daemon blocks completion on
    it). Call this to clear a stale intervention — e.g. when Fett
    typed into a worker's pane but nothing was actually submitted
    and the intervention isn't semantically relevant anymore.

    Returns {task_id, user_intervened} from the daemon response.
    """
    return await _one_shot(
        "cli.task.intervention.clear", {"task_id": task_id}
    )


async def status(view: str | None = None) -> dict[str, Any]:
    """Fetch agent roster (+ optionally tasks).

    `view`:
      - None or "summary" (server default) → lean response:
        `{view, agents: [AgentSummary], connected}`. Each agent
        has `id / name / connected / project / tier /
        max_concurrent / template / tmux_session / current_tasks`
        (non-terminal task UUIDs only). No `tasks` firehose.
        Use for routing decisions where only "who's online, is
        the slot busy?" matters.
      - "full" → backwards-compatible firehose:
        `{view, agents, tasks, connected}` with full Agent structs
        (including task_history) and every task in state.json
        inline. Use when you genuinely need per-agent history or
        cross-agent task scans; prefer `task_list(view=summary)`
        for plain task-roster queries.

    Server-side branching via `view` on `cli.status` (see
    src-tauri/src/wrapper/protocol.rs::CliStatus).
    """
    payload: dict[str, Any] = {}
    if view is not None:
        payload["view"] = view
    return await _one_shot("cli.status", payload if payload else None)


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

    Raises `FramedSendNotSupportedError` if `suppress_echo=True` and the
    target agent's runtime can't decode BEGIN/END framing (wrapper
    workers, unresolvable agent ids). Callers that catch this can retry
    with `suppress_echo=False` or route the message through a new task.
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

    Raises `FrameWedgedError` (a DaemonError subclass) if the worker emits
    a `worker.frame_wedged` event carrying this call's correlation_id as
    its `dropped_uuid` — i.e. a nested BEGIN arrived and the worker threw
    away our body without running the SDK turn. That's a distinct outcome
    from a vanilla timeout and retry is generally safe; see the exception
    class docstring. Also raises `FramedSendNotSupportedError` if the
    underlying send hits the runtime gate.

    Implementation: opens its own `cli.event.stream` subscription BEFORE
    firing the send to close the race window where the reply fires before
    we subscribe. The orch's primary event_watcher is unaffected — the
    daemon broadcasts to all subscribers, so both see the event.
    """
    try:
        reader, writer = await asyncio.open_unix_connection(DAEMON_SOCKET, limit=SOCKET_READ_LIMIT)
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
            event = payload.get("event")
            data = payload.get("data") or {}

            # Wedge signal: the worker discarded our frame on a
            # nested-BEGIN recovery. Surface a typed error instead of
            # letting the await burn down to timeout and pretending we
            # don't know what happened.
            if event == "worker.frame_wedged" and data.get("dropped_uuid") == corrid:
                raise FrameWedgedError(
                    f"frame wedged on agent {data.get('agent_id') or '?'}: "
                    f"our BEGIN ({corrid[:8]}) was discarded mid-send by a "
                    f"nested BEGIN ({(data.get('new_uuid') or '?')[:8]}); "
                    f"{data.get('bytes_dropped', 0)} body bytes dropped, "
                    "SDK turn never ran — retry is safe",
                    dropped_uuid=corrid,
                    new_uuid=data.get("new_uuid") or "",
                    bytes_dropped=int(data.get("bytes_dropped") or 0),
                    lines_dropped=int(data.get("lines_dropped") or 0),
                    agent_id=data.get("agent_id"),
                    task_id=data.get("task_id"),
                )

            if event != "worker.orch_response":
                continue
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


async def worker_response_get(correlation_id: str) -> dict[str, Any]:
    """Retrieve the full text of a prior `worker.orch_response` from
    the daemon's bounded in-memory cache.

    Audit 8b03cae6 bloat fix #4. The orchestrator's event formatter
    caps injected text at EVENT_TEXT_INJECT_MAX_BYTES (2 KiB) and,
    when it truncates, appends a
    `worker_response_get(correlation_id=X)` pointer. This is the RPC
    that follows that pointer.

    Response shape:
      {correlation_id, agent_id, text, task_id, during_task,
       timestamp}

    Daemon returns a `cli.error` (surfaces in Python as DaemonError)
    when the correlation_id was never seen or has been evicted from
    the 100-entry LRU. Callers should treat "not found" as
    "truncated copy is all we'll ever see" — there's no retry that
    recovers an evicted entry.
    """
    return await _one_shot(
        "cli.worker.response.get",
        {"correlation_id": correlation_id},
    )


async def agent_spawn(
    agent: str,
    name: str | None = None,
    role: str | None = None,
    project: str | None = None,
    working_dir: str | None = None,
    spawned_by_task: str | None = None,
) -> dict[str, Any]:
    """Spawn a new agent instance.

    `spawned_by_task`: optional task UUID. Workers that spawn
    sibling instances mid-task should pass their current task_id so
    the daemon can warn at task-completion time if the instance
    wasn't explicitly killed (see
    src-tauri/src/daemon/state.rs::record_task_spawn +
    transition_task warning path). Orchestrator calls leave this
    unset — orch-initiated spawns aren't scoped to a single task.
    """
    payload: dict[str, Any] = {"agent": agent}
    if name:
        payload["name"] = name
    if role:
        payload["role"] = role
    if project:
        payload["project"] = project
    if working_dir:
        payload["working_dir"] = working_dir
    if spawned_by_task:
        payload["spawned_by_task"] = spawned_by_task
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
            reader, writer = await asyncio.open_unix_connection(DAEMON_SOCKET, limit=SOCKET_READ_LIMIT)
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
