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
    msg = _DENIAL_MSG_ORCH if role == "orch" else _DENIAL_MSG_WORKER

    async def gate(
        tool_name: str,
        tool_input: dict[str, Any],
        _context: ToolPermissionContext,
    ):
        if tool_name in HOST_UI_TOOLS:
            return PermissionResultDeny(message=msg)
        return PermissionResultAllow()

    return gate
