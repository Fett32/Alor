"""Orchestrator tools: maps Claude tool calls to daemon socket calls.

Exposes ~10 orchestration primitives as an in-process MCP server.
No Read/Edit/Bash/Grep — the router role is enforced by tool absence.
"""

from __future__ import annotations

import json
from typing import Any

from claude_agent_sdk import create_sdk_mcp_server, tool

import daemon


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
    "Get full state of a task: state, assigned_to, proposal_brief, "
    "proposal_diff, user_intervened flag.",
    {"task_id": str},
)
async def task_get(args: dict[str, Any]) -> dict[str, Any]:
    try:
        return _ok(await daemon.task_get(args["task_id"]))
    except Exception as e:
        return _err(str(e))


@tool(
    "task_list",
    "List all tasks. Prefer task_get by id when you already have one.",
    {},
)
async def task_list(args: dict[str, Any]) -> dict[str, Any]:
    try:
        return _ok(await daemon.task_list())
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
    "agent_list",
    "List worker agents with full state (id, name, connected, project, tier, "
    "max_concurrent, tmux_session, task_history) plus all tasks. Call before "
    "task_assign or agent_ensure_running so you know who's online and what "
    "each slot is scoped to.",
    {},
)
async def agent_list(args: dict[str, Any]) -> dict[str, Any]:
    try:
        return _ok(await daemon.status())
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
    "id is already registered and running.",
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
        return _ok(
            await daemon.agent_spawn(
                agent=args["agent"],
                name=args.get("name") or None,
                role=args.get("role") or None,
                project=args.get("project") or None,
                working_dir=args.get("working_dir") or None,
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
    "`worker.orch_response` event injection).",
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
    "Read Memory Hub for a project — cross-agent shared notes from prior "
    "sessions. Check when a task might have relevant prior context.",
    {"project": str},
)
async def memory_get(args: dict[str, Any]) -> dict[str, Any]:
    try:
        return _ok(await daemon.memory_get(args["project"]))
    except Exception as e:
        return _err(str(e))


ALL_TOOLS = [
    task_create,
    task_assign,
    task_get,
    task_list,
    task_cancel,
    agent_list,
    agent_ensure_running,
    agent_spawn,
    agent_kill,
    agent_send_message,
    project_get,
    project_list,
    memory_get,
]

MCP_SERVER_NAME = "alor"


def build_server():
    return create_sdk_mcp_server(name=MCP_SERVER_NAME, tools=ALL_TOOLS)


def allowed_tool_names() -> list[str]:
    """List of MCP-qualified tool names for ClaudeAgentOptions.allowed_tools."""
    return [f"mcp__{MCP_SERVER_NAME}__{t.name}" for t in ALL_TOOLS]
