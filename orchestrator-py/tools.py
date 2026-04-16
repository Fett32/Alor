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
    "List worker agents (id, name, connected, tmux_session) and current "
    "tasks. Call before task_assign if unsure who's online.",
    {},
)
async def agent_list(args: dict[str, Any]) -> dict[str, Any]:
    try:
        return _ok(await daemon.status())
    except Exception as e:
        return _err(str(e))


@tool(
    "agent_send_message",
    "Inject text into a connected agent's tmux pane. Use for answering "
    "agent questions or relaying follow-up instructions without creating a "
    "new task. Set submit=true to append Enter.",
    {"agent_id": str, "text": str, "submit": bool},
)
async def agent_send_message(args: dict[str, Any]) -> dict[str, Any]:
    try:
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
