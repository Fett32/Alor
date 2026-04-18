"""can_use_tool gate that denies SDK-incompatible host-UI tools.

Claude Code's TUI implements AskUserQuestion / EnterPlanMode /
ExitPlanMode via an interactive permission component ("the
permission component", per the SDK tool schemas). In an SDK-hosted
worker or orchestrator runtime that component doesn't exist — a
live repro confirms AskUserQuestion returns an opaque
`"Answer questions?"` tool_result error, indistinguishable from any
other failure. The agent sees an error but has nothing typed to
branch on, so it plows past the "need user input" moment without
ever surfacing the question to Fett.

Gate them at the `can_use_tool` hook so the agent gets a typed,
explanatory denial instead. The denial message points the agent at
the working fallback primitive for its role (workers use
`task.blocked`; the orch speaks directly to Fett in its assistant
text).

Scope of the gate:
- AskUserQuestion / EnterPlanMode / ExitPlanMode: UI-dependent,
  silent-drop in SDK workers. Gated.
- PushNotification: has a graceful headless fallback ("push not
  sent" is documented as expected). NOT gated.
- EnterWorktree / ExitWorktree: not UI-dependent but mutate the
  Claude Code CLI's session state (cwd + git worktree); failure
  class is different, pending separate investigation. NOT gated.

Role-based Alor-MCP gating (worker only):
- Worker builds its MCP server from tools.WORKER_ACCESSIBLE_TOOLS,
  so the task_* / project_* / memory_get tools are structurally
  absent. This gate is a belt-and-braces second line: it denies
  those by name if a worker-role call ever reaches the hook,
  catching copy/paste or future-refactor regressions where someone
  builds the full server for a worker by mistake.
- agent_send_message has an additional payload check: workers
  must not target an orchestrator agent_id (prevents worker →
  orchestrator prompt injection loops).
"""

from __future__ import annotations

from typing import Any

from claude_agent_sdk import (
    PermissionResultAllow,
    PermissionResultDeny,
    ToolPermissionContext,
)

# Deny-list keyed by the SDK-internal tool name. Kept small on
# purpose — only tools whose failure mode is genuinely opaque in
# the SDK runtime. Adding tools here that DO have a working SDK
# code path would just deny them unnecessarily.
HOST_UI_TOOLS: frozenset[str] = frozenset(
    {
        "AskUserQuestion",
        "EnterPlanMode",
        "ExitPlanMode",
    }
)


# MCP prefix for Alor's own tool names. Keep in sync with
# tools.MCP_SERVER_NAME — not imported to avoid a circular import
# (tools.py doesn't need tool_gate, and tool_gate is called from
# worker.py and main.py both of which import tools too).
_ALOR_MCP_PREFIX = "mcp__alor__"

# Alor MCP tool names workers must NOT invoke. Filtering at the
# MCP-server level (tools._tools_for_role) is the primary defense;
# this set is the call-time second line in case the full server
# gets loaded for a worker by mistake. Keep in sync with the
# inverse of tools.WORKER_ACCESSIBLE_TOOLS.
_WORKER_DENIED_ALOR_TOOLS: frozenset[str] = frozenset(
    {
        "task_create",
        "task_assign",
        "task_cancel",
        "task_get",
        "task_list",
        "project_get",
        "project_list",
        "memory_get",
    }
)

# Agent IDs treated as orchestrator-authority. A worker-origin
# agent_send_message to any of these is denied — prevents a worker
# from injecting prompts into the orch's live SDK session.
# `claude-alor` is the fixed orchestrator-role slot per the Alor
# memory-hub docs; the other two are defensive aliases so a future
# rename doesn't silently open the hole.
ORCHESTRATOR_AGENT_IDS: frozenset[str] = frozenset(
    {
        "orch",
        "orchestrator",
        "claude-alor",
    }
)

# Base-template names workers may pass as `agent` to agent_spawn.
# Restricted to the generic templates (no fixed slots like
# `claude-alor` / `cursor-alor`) so workers can't accidentally
# clobber production slot ids. Orch role is unrestricted.
WORKER_SPAWNABLE_TEMPLATES: frozenset[str] = frozenset(
    {
        "codex",
        "gemini",
        "cursor",
        "claude",
    }
)

# Required prefixes for the `name` arg on worker-origin agent_spawn.
# Every worker-spawned instance must self-identify as throwaway so
# it's obvious at `agent_list` / `state.json` which entries came
# from debug/verification flows. Case-sensitive — keeps the check
# predictable and aligns with the lowercase agent-id convention.
WORKER_SPAWN_NAME_PREFIXES: tuple[str, ...] = ("debug-", "test-")


def _strip_mcp_prefix(tool_name: str) -> str:
    """Return the bare Alor tool name if `tool_name` is one of ours,
    else return the input unchanged."""
    if tool_name.startswith(_ALOR_MCP_PREFIX):
        return tool_name[len(_ALOR_MCP_PREFIX):]
    return tool_name


def _deny_worker_orch_only(tool_name: str) -> str:
    return (
        f"Tool '{tool_name}' is orchestrator-only. Workers have a "
        f"restricted Alor-MCP surface (agent_spawn, agent_list, "
        f"agent_ensure_running, agent_send_message, agent_kill) "
        f"for end-to-end live verification. Task lifecycle, project "
        f"profiles, and Memory Hub are curated by the orchestrator; "
        f"if you need one of those, report via task.blocked or your "
        f"final summary so the orch can act on it."
    )


def _deny_worker_sends_to_orch(target: str) -> str:
    return (
        f"Workers may not agent_send_message to orchestrator "
        f"agent_id '{target}'. This prevents worker → orchestrator "
        f"prompt-injection loops. Target a non-orchestrator worker "
        f"instead (use agent_spawn to create a throwaway test "
        f"instance if you don't have one to address)."
    )


def _deny_worker_spawn_template(agent: str) -> str:
    allowed = ", ".join(sorted(WORKER_SPAWNABLE_TEMPLATES))
    return (
        f"Workers may only agent_spawn generic templates. "
        f"`agent='{agent}'` is not allowed — it's either a fixed "
        f"production slot (claude-alor / cursor-alor / etc.) or an "
        f"unknown name. Allowed templates: {{{allowed}}}."
    )


def _deny_worker_spawn_missing_name() -> str:
    prefixes = " / ".join(WORKER_SPAWN_NAME_PREFIXES)
    return (
        f"Workers must pass an explicit `name` to agent_spawn — "
        f"daemon auto-derivation is disabled for the worker role. "
        f"Name must start with {prefixes} (e.g. 'debug-foo', "
        f"'test-reconnect-repro') so debug instances are obvious "
        f"in agent_list and state.json."
    )


def _deny_worker_spawn_name_prefix(name: str) -> str:
    prefixes = " / ".join(WORKER_SPAWN_NAME_PREFIXES)
    return (
        f"Worker agent_spawn `name='{name}'` is rejected: names must "
        f"start with {prefixes} so debug/verification instances "
        f"self-identify as throwaway. Pick something like "
        f"'debug-{name}' or 'test-{name}'."
    )


# Worker context: the SDK worker is running inside a tmux pane that
# Fett doesn't normally read during task execution. The orch
# relays structured events (task.blocked, task.complete summaries)
# back to Fett via its own SDK loop. So a worker that wants to ask
# Fett something should either block the task on that question (if
# it's blocking) or surface it in the final summary (if it's
# post-hoc clarification).
_DENIAL_MSG_WORKER = (
    "This tool requires Claude Code's interactive TUI and is not "
    "available in the Alor SDK worker runtime. If this question is "
    "blocking the task, stop and report via `task.blocked` (your "
    "AgentClient exposes `send_*` helpers) with the question as "
    "the reason — the orchestrator surfaces task.blocked events to "
    "Fett. If the question is non-blocking clarification, include "
    "it in your final task-complete summary; Fett reads the "
    "summary and can follow up."
)


# Orch context: Fett is reading the orch's assistant text live in
# the orch tmux pane (claude-sdk-backed orchestrator-py/main.py).
# Assistant text IS the UI — the orch can just ask directly.
_DENIAL_MSG_ORCH = (
    "This tool requires Claude Code's interactive TUI and is not "
    "available here. Ask the user directly in your assistant text "
    "— Fett is reading this conversation live in the orchestrator "
    "pane."
)


def make_gate(role: str):
    """Build a `can_use_tool` callback bound to a role.

    role: "worker" or "orch" — selects the denial message text.

    Any other value is treated as worker (safer default: points at
    structured escape valves rather than "just ask directly",
    which only works for the orch).
    """
    host_ui_msg = _DENIAL_MSG_ORCH if role == "orch" else _DENIAL_MSG_WORKER
    # `is_worker` also catches unknown roles (matches _DENIAL_MSG_WORKER
    # fallback above — same "safer default" principle).
    is_worker = role != "orch"

    async def gate(
        tool_name: str,
        tool_input: dict[str, Any],
        _context: ToolPermissionContext,
    ):
        # Host-UI tools (AskUserQuestion / EnterPlanMode /
        # ExitPlanMode): always denied in the SDK runtime regardless
        # of role — the permission component they need doesn't exist.
        if tool_name in HOST_UI_TOOLS:
            return PermissionResultDeny(message=host_ui_msg)

        # Worker-role extra restrictions on the Alor MCP surface.
        # Orch role is unchanged from before.
        if is_worker:
            bare = _strip_mcp_prefix(tool_name)

            # Orch-only Alor tools — deny at call-time as a second
            # line alongside tools._tools_for_role filtering.
            if bare in _WORKER_DENIED_ALOR_TOOLS:
                return PermissionResultDeny(
                    message=_deny_worker_orch_only(bare)
                )

            # agent_send_message → orchestrator is a worker →
            # orchestrator prompt injection vector. Deny based on
            # the payload's agent_id.
            if bare == "agent_send_message":
                target = (tool_input or {}).get("agent_id", "")
                if target in ORCHESTRATOR_AGENT_IDS:
                    return PermissionResultDeny(
                        message=_deny_worker_sends_to_orch(target)
                    )

            # agent_spawn guardrails: workers can only spawn generic
            # templates (no fixed slots), must pass an explicit
            # `name`, and that name must be debug-/test- prefixed.
            # Order matters: invalid agent first (most likely root
            # cause), then missing name, then bad prefix — keeps the
            # error message precise about which rule tripped.
            if bare == "agent_spawn":
                payload = tool_input or {}
                agent = payload.get("agent", "")
                name = payload.get("name") or ""
                if agent not in WORKER_SPAWNABLE_TEMPLATES:
                    return PermissionResultDeny(
                        message=_deny_worker_spawn_template(agent)
                    )
                if not name:
                    return PermissionResultDeny(
                        message=_deny_worker_spawn_missing_name()
                    )
                if not name.startswith(WORKER_SPAWN_NAME_PREFIXES):
                    return PermissionResultDeny(
                        message=_deny_worker_spawn_name_prefix(name)
                    )

        return PermissionResultAllow()

    return gate
