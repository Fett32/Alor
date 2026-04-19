"""Orchestrator tools: maps Claude tool calls to daemon socket calls.

Exposes ~10 orchestration primitives as an in-process MCP server.
No Read/Edit/Bash/Grep — the router role is enforced by tool absence.
"""

from __future__ import annotations

import json
from contextvars import ContextVar
from typing import Any

from claude_agent_sdk import create_sdk_mcp_server, tool

import daemon

# Worker-role task context. Set by the worker's run_task loop before
# SDK tool invocations and cleared on task exit. `agent_spawn` reads
# this and passes it through as `spawned_by_task` on the underlying
# cli.spawn RPC so the daemon can warn at task-completion time if the
# worker forgot to agent_kill a sibling instance it spawned.
#
# Using a ContextVar (not a module-level mutable) is the safe choice
# even though workers process one task at a time — contextvars
# survive `asyncio.create_task` correctly and don't leak across
# nested awaits the way a bare module global can.
_current_task_id: ContextVar[str | None] = ContextVar(
    "alor_current_task_id", default=None
)


def set_current_task(task_id: str | None) -> None:
    """Set the current task_id (for workers entering run_task).

    Pass None on task exit to clear. Safe to call from anywhere; the
    value is threaded through the ContextVar so nested async work
    sees the same task id.
    """
    _current_task_id.set(task_id)


def current_task_id() -> str | None:
    """Return the current task_id, or None if no task is active.

    Orchestrator never sets this so orch-initiated agent_spawn calls
    get `spawned_by_task=None` — correct, since they aren't scoped
    to a single task.
    """
    return _current_task_id.get()


def _ok(value: Any) -> dict[str, Any]:
    if isinstance(value, str):
        text = value
    else:
        text = json.dumps(value, ensure_ascii=False, indent=2)
    return {"content": [{"type": "text", "text": text}]}


def _err(msg: str) -> dict[str, Any]:
    return {"content": [{"type": "text", "text": f"error: {msg}"}], "is_error": True}


@tool(
    "task_create",
    "Create a new task. Returns a task_id. Always pass `project` when the "
    "task is scoped to a known project — the daemon injects a TASK BRIEF "
    "(key files, docs, notes) into the worker's assignment.",
    {"title": str, "description": str, "project": str},
)
async def task_create(args: dict[str, Any]) -> dict[str, Any]:
    try:
        task_id = await daemon.task_create(
            title=args["title"],
            description=args["description"],
            project=args.get("project") or None,
        )
        return _ok({"task_id": task_id})
    except Exception as e:
        return _err(str(e))


@tool(
    "task_assign",
    "Assign an existing task to a worker agent. Agent must be connected — "
    "call agent_list first if unsure.",
    {"task_id": str, "agent_id": str},
)
async def task_assign(args: dict[str, Any]) -> dict[str, Any]:
    try:
        return _ok(await daemon.task_assign(args["task_id"], args["agent_id"]))
    except Exception as e:
        return _err(str(e))


@tool(
    "task_get",
    "Get one task's state. Defaults to `view='summary'` — routing-"
    "useful scalars (id, title, state, assigned_to, project, "
    "parent_task_id, created_at, updated_at, user_intervened) plus "
    "`has_description` / `has_summary` / `has_details` / "
    "`has_proposal_brief` / `has_proposal_diff` booleans that tell "
    "you which heavy fields are populated. ~300–500 B per call — use "
    "this for routine state checks (has it completed? who owns it? "
    "is there a proposal waiting?). "
    "Pass `view='full'` ONLY when you need the heavy bodies — "
    "description, summary, details, proposal_brief, proposal_diff — "
    "typically after a `has_*` flag on a summary call told you "
    "there's something to fetch. A full call on a task with a "
    "populated proposal_diff can run into multi-KB territory.",
    {"task_id": str, "view": str},
)
async def task_get(args: dict[str, Any]) -> dict[str, Any]:
    try:
        # Missing/blank view → server default ("summary"). Lowercase
        # for forgiveness on LLM typos like "Summary" or "FULL"; the
        # server treats unknown values as summary anyway.
        raw_view = (args.get("view") or "").strip().lower()
        view = raw_view or None
        return _ok(await daemon.task_get(args["task_id"], view=view))
    except Exception as e:
        return _err(str(e))


@tool(
    "task_list",
    "List tasks, paginated. Defaults to non-terminal "
    "(Pending/Assigned/Accepted/Blocked/Proposed/Staged/Interrupted/"
    "Recovering) so the response stays lean. Pass state='completed' / "
    "'cancelled' / 'stale' / 'all' / or a specific SCREAMING_SNAKE_CASE "
    "state name to query terminal tasks. Responses are capped "
    "server-side (default 20 tasks/page) to keep orch context under "
    "budget; response includes total/returned/offset/has_more — "
    "paginate by re-calling with offset += returned while has_more is "
    "true. "
    "View defaults to 'summary' "
    "(id/title/state/assigned_to/updated_at only, ~70 tokens/task) — "
    "use it for scans and raise limit freely (e.g. 200) or pass "
    "limit=0 for unlimited (summary is cheap enough at any scale). "
    "view='full' is for a specific handful of tasks you need "
    "descriptions/proposals for — pair it with a small limit (≤20) "
    "or rely on the server's auto-clamp at 50 (limit=0 or limit>50 "
    "on full view is clamped to 50 and the response carries "
    "`limit_clamped_from` + `limit_applied` fields). For bulk scans, "
    "ALWAYS use summary — never full. For a single task's detail, "
    "prefer task_get over task_list(view='full', limit=1).",
    {"state": str, "limit": int, "offset": int, "view": str},
)
async def task_list(args: dict[str, Any]) -> dict[str, Any]:
    try:
        # Accept either "completed" or "COMPLETED" from the LLM — normalize
        # non-sentinel values to uppercase so they match the server-side
        # SCREAMING_SNAKE_CASE state names. Sentinels ("default" / "all")
        # stay lowercase for the server's match arms.
        raw = (args.get("state") or "").strip() or "default"
        lower = raw.lower()
        state = lower if lower in ("default", "all") else raw.upper()

        # `limit`/`offset` are optional. Missing/non-numeric → let the
        # server default kick in (limit=20). Explicit `limit=0` means
        # "no cap" and flows through untouched.
        def _as_int(v: Any) -> int | None:
            if v is None or v == "":
                return None
            try:
                return int(v)
            except (TypeError, ValueError):
                return None

        limit = _as_int(args.get("limit"))
        offset = _as_int(args.get("offset")) or 0

        # `view` is optional. Missing/blank → server default ("summary").
        # Lowercase so "Summary" / "FULL" / "summary" all work; the
        # server treats unknown values as "summary" anyway, so this is
        # just cosmetic normalization.
        raw_view = (args.get("view") or "").strip().lower()
        view = raw_view or None

        return _ok(
            await daemon.task_list(state=state, limit=limit, offset=offset, view=view)
        )
    except Exception as e:
        return _err(str(e))


@tool(
    "task_cancel",
    "Cancel a task. Only when user explicitly asks or task is obsolete.",
    {"task_id": str},
)
async def task_cancel(args: dict[str, Any]) -> dict[str, Any]:
    try:
        return _ok(await daemon.task_cancel(args["task_id"]))
    except Exception as e:
        return _err(str(e))


@tool(
    "task_intervention_clear",
    "Reset the `user_intervened` flag on a task. The flag is "
    "informational-only (nothing in the daemon blocks completion on "
    "it) but latches true when the wrapper detects Fett typing into "
    "an agent pane mid-task. Call this when the intervention was "
    "unsubmitted / unintentional and the flag is misleading — e.g. "
    "worker has since finished, stray keystrokes didn't actually "
    "steer anything. Does NOT unstick a task that's actually stuck "
    "in ACCEPTED; for that, use `task_complete` explicitly.",
    {"task_id": str},
)
async def task_intervention_clear(args: dict[str, Any]) -> dict[str, Any]:
    try:
        return _ok(await daemon.task_intervention_clear(args["task_id"]))
    except Exception as e:
        return _err(str(e))


@tool(
    "agent_list",
    "List worker agents. Defaults to `view='summary'` — per-agent "
    "{id, name, connected, project, tier, max_concurrent, template, "
    "tmux_session, current_tasks} with current_tasks holding "
    "non-terminal task UUIDs only (no embedded task data). Use this "
    "for routing decisions (task_assign / agent_ensure_running): "
    "it's the right shape for \"who's online, is the slot busy?\" "
    "and avoids the ~85k-token-per-call bloat that `view='full'` "
    "incurs when state.json has many tasks. "
    "Pass `view='full'` only when you genuinely need per-agent "
    "task_history or the full cross-agent task firehose — for "
    "plain task-roster queries, prefer task_list(view=summary) "
    "instead.",
    {"view": str},
)
async def agent_list(args: dict[str, Any]) -> dict[str, Any]:
    try:
        # Missing/blank view → server default ("summary"). Lowercase
        # for forgiveness on LLM typos like "Summary" or "FULL"; the
        # server treats unknown values as summary anyway.
        raw_view = (args.get("view") or "").strip().lower()
        view = raw_view or None
        return _ok(await daemon.status(view=view))
    except Exception as e:
        return _err(str(e))


@tool(
    "agent_ensure_running",
    "Idempotently bring a yaml-declared agent slot online. If already "
    "connected, no-op. Prefer this over agent_spawn when you just need a "
    "known slot (e.g. 'claude-alor') available.",
    {"agent_id": str},
)
async def agent_ensure_running(args: dict[str, Any]) -> dict[str, Any]:
    try:
        return _ok(await daemon.agent_ensure_running(args["agent_id"]))
    except Exception as e:
        return _err(str(e))


@tool(
    "agent_spawn",
    "Spawn a new agent instance from a base config or template. "
    "`agent` = base config name (e.g. 'claude-alor' for a fixed slot, or "
    "'claude' for the generic template). `name` = instance id; omit when "
    "spawning from a template and you also pass `project` — the daemon "
    "will auto-derive '{agent}-{project}' (e.g. 'claude-mandaspace'). "
    "Pass `project` + `working_dir` to parameterize a template for a "
    "specific project (working_dir may include ~). Fails if the instance "
    "id is already registered and running. "
    "Worker role is restricted: `agent` must be in "
    "{codex, gemini, cursor, claude} (generic templates only — no fixed "
    "slots), `name` is required (no auto-derive), and `name` must start "
    "with 'debug-' or 'test-' so verification instances self-identify as "
    "throwaway. Orchestrator role is unrestricted.",
    {
        "agent": str,
        "name": str,
        "role": str,
        "project": str,
        "working_dir": str,
    },
)
async def agent_spawn(args: dict[str, Any]) -> dict[str, Any]:
    try:
        # Thread the current worker task id (set by worker.run_task via
        # set_current_task) down to the daemon so it can track which
        # task spawned which instance. Orchestrator calls have no task
        # context and pass None → daemon treats the spawn as untracked.
        return _ok(
            await daemon.agent_spawn(
                agent=args["agent"],
                name=args.get("name") or None,
                role=args.get("role") or None,
                project=args.get("project") or None,
                working_dir=args.get("working_dir") or None,
                spawned_by_task=current_task_id(),
            )
        )
    except Exception as e:
        return _err(str(e))


@tool(
    "agent_kill",
    "Stop a running agent instance by its id. Use when a slot has stale "
    "context you'd rather not reuse and you've confirmed with Fett that "
    "killing it is fine, OR when a slot is definitely finished.",
    {"instance": str},
)
async def agent_kill(args: dict[str, Any]) -> dict[str, Any]:
    try:
        return _ok(await daemon.agent_kill(args["instance"]))
    except Exception as e:
        return _err(str(e))


@tool(
    "agent_send_message",
    "Inject text into a connected agent's tmux pane. Use for answering "
    "agent questions or relaying follow-up instructions without creating a "
    "new task. Set submit=true to append Enter. Set await_response=true to "
    "block until the agent's SDK turn finishes and the reply comes back — "
    "returns the reply text inline so you can act on the answer in the "
    "same turn. Default is fire-and-forget (reply arrives later as a "
    "`worker.orch_response` event injection). "
    "Only works against `claude-sdk` runtime workers — wrapper-runtime "
    "agents (codex, gemini, etc.) reject framed sends; use a new task for "
    "those instead.",
    {
        "agent_id": str,
        "text": str,
        "submit": bool,
        "await_response": bool,
    },
)
async def agent_send_message(args: dict[str, Any]) -> dict[str, Any]:
    try:
        if bool(args.get("await_response", False)):
            # Awaiting variant: submit defaults to True because you
            # almost always want Enter appended when you're blocking
            # on the reply — otherwise the SDK never runs.
            submit = bool(args.get("submit", True))
            result = await daemon.agent_send_message_await(
                args["agent_id"], args["text"], submit=submit
            )
            return _ok(result)
        return _ok(
            await daemon.agent_send_message(
                args["agent_id"], args["text"], bool(args.get("submit", False))
            )
        )
    except Exception as e:
        return _err(str(e))


@tool(
    "project_get",
    "Read a project's profile: description, stack, root_dir, key_files, "
    "doc_paths, memory_hub, notes. Use before creating a task to pick the "
    "right worker and brief.",
    {"name": str},
)
async def project_get(args: dict[str, Any]) -> dict[str, Any]:
    try:
        return _ok(await daemon.project_get(args["name"]))
    except Exception as e:
        return _err(str(e))


@tool(
    "project_list",
    "List all project names the daemon knows about.",
    {},
)
async def project_list(args: dict[str, Any]) -> dict[str, Any]:
    try:
        return _ok(await daemon.project_list())
    except Exception as e:
        return _err(str(e))


@tool(
    "memory_get",
    "Read specific Memory Hub files for a project — pass "
    "`file_names=[...]` to pull only what you need (basenames; names "
    "with `..` or `/` are silently rejected). The Memory Hub is "
    "cross-agent shared notes from prior sessions; checking is useful "
    "when a task might have relevant prior context. "
    "Discover file names via project_get (`memory_index` field) first "
    "when possible — that's the canonical per-project index and it's "
    "cheap. Omit `file_names` only when you genuinely don't know "
    "what's there yet (e.g. first touch of a project); the response "
    "is uncapped and can run 10 KB+ on hubs with accumulated notes. "
    "Response shape: {project, files: {name → content}, missing: "
    "[names]} — `missing` only present when `file_names` was passed.",
    {"project": str, "file_names": list},
)
async def memory_get(args: dict[str, Any]) -> dict[str, Any]:
    try:
        # Normalize file_names: accept missing, None, empty list, or
        # a populated list. An empty list deliberately falls through
        # to "no filter" (same as None) — the server treats them
        # equivalently, and forcing the LLM to distinguish None vs
        # [] would be friction for no gain.
        raw_names = args.get("file_names")
        file_names: list[str] | None
        if raw_names is None:
            file_names = None
        elif isinstance(raw_names, list):
            # Keep only string entries; silently drop non-strings
            # (LLM edge cases like passing a single string or a
            # nested dict). Empty or non-string → None.
            cleaned = [n for n in raw_names if isinstance(n, str) and n]
            file_names = cleaned if cleaned else None
        else:
            file_names = None
        return _ok(await daemon.memory_get(args["project"], file_names=file_names))
    except Exception as e:
        return _err(str(e))


@tool(
    "worker_response_get",
    "Fetch the full untruncated text of a prior worker.orch_response. "
    "Use ONLY when you just saw a truncated `[Alor event]` injection "
    "whose marker said `fetch via worker_response_get(correlation_id=X)` "
    "and you actually need the rest of the body to reason about the "
    "reply. Do NOT speculatively fetch — most orch_response injections "
    "aren't truncated, and those that are usually carry enough in the "
    "first 2 KB to route on. The daemon keeps the 100 most recent "
    "responses in an in-memory LRU; older entries are evicted and "
    "return a 'not found' error — treat that as 'the truncated copy is "
    "all I'll ever see'. Returns "
    "{correlation_id, agent_id, text, task_id, during_task, timestamp}.",
    {"correlation_id": str},
)
async def worker_response_get(args: dict[str, Any]) -> dict[str, Any]:
    try:
        return _ok(await daemon.worker_response_get(args["correlation_id"]))
    except Exception as e:
        return _err(str(e))


ALL_TOOLS = [
    task_create,
    task_assign,
    task_get,
    task_list,
    task_cancel,
    task_intervention_clear,
    agent_list,
    agent_ensure_running,
    agent_spawn,
    agent_kill,
    agent_send_message,
    project_get,
    project_list,
    memory_get,
    worker_response_get,
]

MCP_SERVER_NAME = "alor"

# Tool names workers are allowed to invoke. Everything else in
# ALL_TOOLS is orchestrator-only and gets filtered out by
# build_server(role="worker"). The intent is end-to-end live
# verification: a worker should be able to spawn a throwaway
# instance, poll its state, message it, and clean up — all without
# orchestrator involvement. Task lifecycle (task_*), project
# profiles (project_*), and memory (memory_*) remain orch-only
# because those are curation / routing / assignment decisions that
# belong to the orchestrator role by design.
#
# Defense-in-depth: tool_gate.make_gate("worker") ALSO denies the
# orch-only tools at call-time, so even if a worker somehow loaded
# the full server (copy-paste, future refactor) the gate surfaces
# a typed error rather than letting the call through.
WORKER_ACCESSIBLE_TOOLS: frozenset[str] = frozenset(
    {
        "agent_spawn",
        "agent_list",
        "agent_ensure_running",
        "agent_send_message",
        "agent_kill",
    }
)


def _tools_for_role(role: str):
    """Filter ALL_TOOLS by caller role.

    role:
      - "orch"   → every tool (current orchestrator behaviour).
      - "worker" → WORKER_ACCESSIBLE_TOOLS subset only.
      - anything else → worker (safer default; matches
        tool_gate.make_gate's fallback).
    """
    if role == "orch":
        return list(ALL_TOOLS)
    return [t for t in ALL_TOOLS if t.name in WORKER_ACCESSIBLE_TOOLS]


def build_server(role: str = "orch"):
    """Build the Alor MCP server for the given role.

    Default is "orch" for backwards compatibility with callers that
    predate the worker-side surface.
    """
    return create_sdk_mcp_server(
        name=MCP_SERVER_NAME, tools=_tools_for_role(role)
    )


def allowed_tool_names(role: str = "orch") -> list[str]:
    """MCP-qualified tool names for ClaudeAgentOptions.allowed_tools.

    Matches the tool set returned by `build_server(role)` so the
    allow-list and the server surface stay in sync.
    """
    return [
        f"mcp__{MCP_SERVER_NAME}__{t.name}" for t in _tools_for_role(role)
    ]
