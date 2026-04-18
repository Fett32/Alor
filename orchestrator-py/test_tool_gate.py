"""Regression tests for the host-UI tool gate.

Background:
    Claude Code's TUI provides AskUserQuestion / EnterPlanMode /
    ExitPlanMode via an interactive permission component. In an
    SDK-hosted worker or orchestrator runtime that component doesn't
    exist — a live probe confirms AskUserQuestion returns an opaque
    `"Answer questions?"` tool_result error that the agent can't
    programmatically distinguish from any other failure. The gate
    (tool_gate.py) intercepts these calls at the `can_use_tool` hook
    and returns a typed PermissionResultDeny with a role-specific
    message pointing at the working escape valve.

This file unit-tests:
    - HOST_UI_TOOLS lists exactly the three confirmed host-UI-
      dependent tools (guard against accidental additions/removals).
    - Deny path: each HOST_UI_TOOLS member returns a PermissionResultDeny
      with the role-specific message text.
    - Allow path: common tools (Bash, Read, Edit, Write, Grep, Glob,
      WebFetch, TodoWrite, and untested "random" names) return
      PermissionResultAllow.
    - Role routing: "worker" vs "orch" vs unknown role all resolve to
      the correct message; unknown roles fall back to worker.
    - make_gate returns an independently-callable coroutine factory
      (each call builds a fresh gate with its own captured message).

Run standalone: `python3 test_tool_gate.py` from orchestrator-py/.
Exits 0 on pass. No pytest dependency.
"""

from __future__ import annotations

import asyncio
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

from claude_agent_sdk import (  # noqa: E402
    PermissionResultAllow,
    PermissionResultDeny,
)

from tool_gate import (  # noqa: E402
    HOST_UI_TOOLS,
    ORCHESTRATOR_AGENT_IDS,
    make_gate,
)


def assert_eq(label: str, got, want) -> None:
    if got != want:
        print(f"FAIL  {label}")
        print(f"  got : {got!r}")
        print(f"  want: {want!r}")
        raise SystemExit(1)
    print(f"ok    {label}")


def assert_is(label: str, got, want_type) -> None:
    if not isinstance(got, want_type):
        print(f"FAIL  {label}")
        print(f"  got : {type(got).__name__}: {got!r}")
        print(f"  want: isinstance of {want_type.__name__}")
        raise SystemExit(1)
    print(f"ok    {label}")


async def call_gate(gate, tool_name: str, tool_input=None):
    """Dispatch the gate on (tool_name, input, context=None).

    The gate signature takes a ToolPermissionContext; `None` is fine
    for tests because our gate doesn't read it.
    """
    return await gate(tool_name, tool_input or {}, None)


# ---------------------------------------------------------------------------
# HOST_UI_TOOLS membership
# ---------------------------------------------------------------------------


def test_host_ui_tools_locked_set() -> None:
    # Guard against accidental additions/removals — the set matters for
    # scope and should change deliberately, not drift.
    expected = frozenset({"AskUserQuestion", "EnterPlanMode", "ExitPlanMode"})
    assert_eq("HOST_UI_TOOLS is exactly the locked set", HOST_UI_TOOLS, expected)


# ---------------------------------------------------------------------------
# Deny path — worker role
# ---------------------------------------------------------------------------


async def test_worker_denies_ask_user_question() -> None:
    gate = make_gate("worker")
    result = await call_gate(gate, "AskUserQuestion", {"questions": []})
    assert_is("worker AskUserQuestion -> PermissionResultDeny", result, PermissionResultDeny)
    assert_eq("worker deny behavior is 'deny'", result.behavior, "deny")
    # Worker message should point at task.blocked + final summary.
    assert_eq(
        "worker msg names task.blocked",
        "task.blocked" in result.message,
        True,
    )
    assert_eq(
        "worker msg names final summary",
        "final" in result.message.lower() and "summary" in result.message.lower(),
        True,
    )


async def test_worker_denies_enter_plan_mode() -> None:
    gate = make_gate("worker")
    result = await call_gate(gate, "EnterPlanMode", {})
    assert_is("worker EnterPlanMode -> Deny", result, PermissionResultDeny)


async def test_worker_denies_exit_plan_mode() -> None:
    gate = make_gate("worker")
    result = await call_gate(gate, "ExitPlanMode", {})
    assert_is("worker ExitPlanMode -> Deny", result, PermissionResultDeny)


# ---------------------------------------------------------------------------
# Deny path — orch role
# ---------------------------------------------------------------------------


async def test_orch_denies_ask_user_question() -> None:
    gate = make_gate("orch")
    result = await call_gate(gate, "AskUserQuestion", {"questions": []})
    assert_is("orch AskUserQuestion -> Deny", result, PermissionResultDeny)
    # Orch message should point at assistant text / live conversation.
    assert_eq(
        "orch msg names assistant text",
        "assistant text" in result.message,
        True,
    )
    assert_eq(
        "orch msg mentions Fett reads live",
        "Fett" in result.message and "live" in result.message,
        True,
    )


async def test_orch_denies_all_three_host_ui_tools() -> None:
    gate = make_gate("orch")
    for name in ("AskUserQuestion", "EnterPlanMode", "ExitPlanMode"):
        result = await call_gate(gate, name, {})
        assert_is(f"orch {name} -> Deny", result, PermissionResultDeny)


# ---------------------------------------------------------------------------
# Role messages are distinct
# ---------------------------------------------------------------------------


async def test_worker_and_orch_messages_differ() -> None:
    worker_gate = make_gate("worker")
    orch_gate = make_gate("orch")
    w = await call_gate(worker_gate, "AskUserQuestion", {})
    o = await call_gate(orch_gate, "AskUserQuestion", {})
    # Different text by design — worker points at structured escape
    # valves; orch points at direct assistant-text replies.
    assert_eq("worker and orch deny messages differ", w.message == o.message, False)


async def test_unknown_role_falls_back_to_worker_message() -> None:
    # make_gate uses "worker" as the safer default — its message points
    # at structured escape valves that any role has access to, while
    # the orch message assumes Fett is live-reading the conversation
    # (not true for a hypothetical third role).
    worker_gate = make_gate("worker")
    mystery_gate = make_gate("some-role-we-did-not-plan-for")
    w = await call_gate(worker_gate, "AskUserQuestion", {})
    m = await call_gate(mystery_gate, "AskUserQuestion", {})
    assert_eq(
        "unknown role message == worker message",
        m.message,
        w.message,
    )


# ---------------------------------------------------------------------------
# Allow path
# ---------------------------------------------------------------------------


async def test_worker_allows_common_tools() -> None:
    gate = make_gate("worker")
    for name in (
        "Bash",
        "Read",
        "Edit",
        "Write",
        "Grep",
        "Glob",
        "WebFetch",
        "TodoWrite",
        "NotebookEdit",
        # Worker-accessible Alor MCP tools — should never be gated.
        "mcp__alor__agent_spawn",
        "mcp__alor__agent_list",
        "mcp__alor__agent_ensure_running",
        "mcp__alor__agent_kill",
        # Made-up name — anything unknown is allow by default.
        "some_future_tool_nobody_has_heard_of_yet",
    ):
        result = await call_gate(gate, name, {"x": 1})
        assert_is(f"worker {name} -> Allow", result, PermissionResultAllow)
        assert_eq(f"worker {name} behavior 'allow'", result.behavior, "allow")


async def test_orch_allows_common_tools() -> None:
    gate = make_gate("orch")
    for name in (
        "Bash",
        "Read",
        "Edit",
        "Write",
        "Grep",
        "Glob",
        # Orch sees every Alor MCP tool.
        "mcp__alor__task_create",
        "mcp__alor__task_assign",
        "mcp__alor__task_cancel",
        "mcp__alor__project_get",
        "mcp__alor__memory_get",
        "mcp__alor__agent_send_message",
    ):
        result = await call_gate(gate, name, {})
        assert_is(f"orch {name} -> Allow", result, PermissionResultAllow)


# ---------------------------------------------------------------------------
# PushNotification / EnterWorktree / ExitWorktree are NOT gated
# ---------------------------------------------------------------------------


async def test_push_notification_not_gated() -> None:
    # Degrades cleanly headlessly — not a silent-drop case, intentionally
    # left allow.
    gate = make_gate("worker")
    result = await call_gate(gate, "PushNotification", {"message": "x", "status": "proactive"})
    assert_is("PushNotification -> Allow (not gated)", result, PermissionResultAllow)


# ---------------------------------------------------------------------------
# Worker-role Alor MCP restrictions
# ---------------------------------------------------------------------------


async def test_worker_denies_orch_only_alor_tools() -> None:
    """Worker gate must deny task_*/project_*/memory_* by name.

    These are blocked structurally by tools.build_server("worker")
    filtering them out, but the gate is belt-and-braces — a copy/paste
    regression that loaded the full server for a worker shouldn't
    actually let the tool execute.
    """
    gate = make_gate("worker")
    for bare in (
        "task_create",
        "task_assign",
        "task_cancel",
        "task_get",
        "task_list",
        "project_get",
        "project_list",
        "memory_get",
    ):
        # Both the qualified MCP name and the bare name should deny —
        # the gate strips the prefix before looking up.
        for name in (f"mcp__alor__{bare}", bare):
            result = await call_gate(gate, name, {})
            assert_is(
                f"worker {name} -> Deny (orch-only)", result, PermissionResultDeny
            )
            assert_eq(
                f"worker {name} deny mentions '{bare}'",
                bare in result.message,
                True,
            )
            assert_eq(
                f"worker {name} deny mentions orch-only nature",
                "orchestrator-only" in result.message,
                True,
            )


async def test_worker_allows_the_five_worker_accessible_tools() -> None:
    gate = make_gate("worker")
    for bare in (
        "agent_spawn",
        "agent_list",
        "agent_ensure_running",
        "agent_kill",
    ):
        name = f"mcp__alor__{bare}"
        result = await call_gate(gate, name, {})
        assert_is(f"worker {name} -> Allow", result, PermissionResultAllow)
    # agent_send_message with a non-orchestrator target allows:
    result = await call_gate(
        gate,
        "mcp__alor__agent_send_message",
        {"agent_id": "codex-debug-test", "text": "hi"},
    )
    assert_is(
        "worker agent_send_message to non-orch target -> Allow",
        result,
        PermissionResultAllow,
    )


async def test_worker_cannot_send_to_orchestrator() -> None:
    """Worker-origin agent_send_message to any orchestrator agent_id
    is denied so workers can't inject prompts into the orch's SDK."""
    gate = make_gate("worker")
    for orch_id in ORCHESTRATOR_AGENT_IDS:
        result = await call_gate(
            gate,
            "mcp__alor__agent_send_message",
            {"agent_id": orch_id, "text": "poisoned prompt"},
        )
        assert_is(
            f"worker send_message -> {orch_id}: Deny",
            result,
            PermissionResultDeny,
        )
        assert_eq(
            f"worker send_message deny names target '{orch_id}'",
            orch_id in result.message,
            True,
        )
        assert_eq(
            f"worker send_message deny mentions injection risk",
            "injection" in result.message.lower(),
            True,
        )


async def test_orch_unrestricted_on_alor_tools() -> None:
    """Orch role keeps full Alor MCP access — no regression."""
    gate = make_gate("orch")
    for bare in (
        "task_create",
        "task_assign",
        "task_cancel",
        "agent_spawn",
        "agent_send_message",
        "project_get",
        "memory_get",
    ):
        name = f"mcp__alor__{bare}"
        # Orch role: agent_send_message to orch-self is allowed
        # (orch can address its own slot; nothing to prevent).
        result = await call_gate(
            gate, name, {"agent_id": "claude-alor", "text": "x"}
        )
        assert_is(f"orch {name} -> Allow", result, PermissionResultAllow)


# ---------------------------------------------------------------------------
# PushNotification / Worktree tools (unchanged behavior)
# ---------------------------------------------------------------------------


async def test_worktree_tools_not_gated() -> None:
    # Different failure class (session-state mutation, not UI). Pending
    # separate investigation — current gate deliberately doesn't touch
    # them, confirm that stays true so a drift in HOST_UI_TOOLS here
    # surfaces as a test failure.
    gate = make_gate("worker")
    for name in ("EnterWorktree", "ExitWorktree"):
        result = await call_gate(gate, name, {})
        assert_is(f"{name} -> Allow (not gated)", result, PermissionResultAllow)


# ---------------------------------------------------------------------------
# Runner
# ---------------------------------------------------------------------------


async def main() -> int:
    test_host_ui_tools_locked_set()

    await test_worker_denies_ask_user_question()
    await test_worker_denies_enter_plan_mode()
    await test_worker_denies_exit_plan_mode()

    await test_orch_denies_ask_user_question()
    await test_orch_denies_all_three_host_ui_tools()

    await test_worker_and_orch_messages_differ()
    await test_unknown_role_falls_back_to_worker_message()

    await test_worker_allows_common_tools()
    await test_orch_allows_common_tools()

    await test_worker_denies_orch_only_alor_tools()
    await test_worker_allows_the_five_worker_accessible_tools()
    await test_worker_cannot_send_to_orchestrator()
    await test_orch_unrestricted_on_alor_tools()

    await test_push_notification_not_gated()
    await test_worktree_tools_not_gated()

    print()
    print("PASS — host-UI tool gate denies confirmed-broken tools, allows everything else.")
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
