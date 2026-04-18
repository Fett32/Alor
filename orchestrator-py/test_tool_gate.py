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
from typing import Any

sys.path.insert(0, str(Path(__file__).parent))

from claude_agent_sdk import (  # noqa: E402
    PermissionResultAllow,
    PermissionResultDeny,
)

from tool_gate import (  # noqa: E402
    HOST_UI_TOOLS,
    ORCHESTRATOR_AGENT_IDS,
    WORKER_SPAWN_NAME_PREFIXES,
    WORKER_SPAWNABLE_TEMPLATES,
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
    # Tools whose allow is payload-independent — no per-arg gate
    # checks. agent_spawn / agent_send_message are NOT in this list
    # because they enforce payload-specific rules; their allow-path
    # coverage lives in test_worker_allows_the_five_worker_accessible_tools.
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
        # Worker-accessible Alor MCP tools with no payload check.
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
    # Each tool gets a minimal valid payload. Some (agent_spawn,
    # agent_send_message) have additional payload checks in the gate
    # — those are covered by their own dedicated test functions below.
    gate = make_gate("worker")
    valid_calls: list[tuple[str, dict[str, Any]]] = [
        ("mcp__alor__agent_list", {}),
        ("mcp__alor__agent_ensure_running", {"agent_id": "codex"}),
        ("mcp__alor__agent_kill", {"instance": "codex-debug-test"}),
        (
            "mcp__alor__agent_spawn",
            {"agent": "claude", "name": "debug-smoke"},
        ),
        (
            "mcp__alor__agent_send_message",
            {"agent_id": "codex-debug-test", "text": "hi"},
        ),
    ]
    for name, payload in valid_calls:
        result = await call_gate(gate, name, payload)
        assert_is(f"worker {name} -> Allow", result, PermissionResultAllow)


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


async def test_worker_spawnable_templates_and_prefixes_locked() -> None:
    # Guard against accidental additions/removals — both sets are
    # security-relevant; drift should be deliberate and test-visible.
    assert_eq(
        "WORKER_SPAWNABLE_TEMPLATES is exactly the locked set",
        WORKER_SPAWNABLE_TEMPLATES,
        frozenset({"codex", "gemini", "cursor", "claude"}),
    )
    assert_eq(
        "WORKER_SPAWN_NAME_PREFIXES is exactly (debug-, test-)",
        WORKER_SPAWN_NAME_PREFIXES,
        ("debug-", "test-"),
    )


async def test_worker_agent_spawn_allows_generic_templates_with_prefix() -> None:
    gate = make_gate("worker")
    happy_paths = [
        ("codex", "debug-foo"),
        ("gemini", "test-reconnect"),
        ("cursor", "debug-"),            # empty suffix still prefix-valid
        ("claude", "test-deep-nested-1"),
        ("claude", "debug-outbox-repro"),
    ]
    for agent, name in happy_paths:
        result = await call_gate(
            gate,
            "mcp__alor__agent_spawn",
            {
                "agent": agent,
                "name": name,
                "project": "debug-scratch",
                "working_dir": "/tmp/debug",
            },
        )
        assert_is(
            f"worker agent_spawn(agent={agent!r}, name={name!r}) -> Allow",
            result,
            PermissionResultAllow,
        )


async def test_worker_agent_spawn_denies_non_template_agents() -> None:
    gate = make_gate("worker")
    blocked_agents = [
        "claude-alor",         # fixed orchestrator slot
        "cursor-alor",         # fixed slot
        "claude-mandaspace",   # derived instance id, not a template
        "",                    # missing
        "CLAUDE",              # case-sensitive check — uppercase disallowed
        "nonexistent-name",
    ]
    for agent in blocked_agents:
        result = await call_gate(
            gate,
            "mcp__alor__agent_spawn",
            {"agent": agent, "name": "debug-ok"},
        )
        assert_is(
            f"worker agent_spawn(agent={agent!r}) -> Deny",
            result,
            PermissionResultDeny,
        )
        # Error should name the rule + list allowed templates so the
        # LLM can self-correct.
        assert_eq(
            f"worker agent_spawn(agent={agent!r}) deny mentions 'generic templates'",
            "generic templates" in result.message,
            True,
        )
        assert_eq(
            f"worker agent_spawn(agent={agent!r}) deny lists allowed set",
            "codex" in result.message
            and "gemini" in result.message
            and "cursor" in result.message
            and "claude" in result.message,
            True,
        )


async def test_worker_agent_spawn_denies_bad_name_prefix() -> None:
    gate = make_gate("worker")
    bad_names = [
        "foo",
        "deploy-prod",
        "alor-test",
        "debug_foo",        # underscore, not dash — strict prefix
        "debugfoo",         # no separator
        "Debug-foo",        # case-sensitive check
        "TEST-foo",
        " debug-foo",       # leading whitespace breaks prefix
        "production",
    ]
    for name in bad_names:
        result = await call_gate(
            gate,
            "mcp__alor__agent_spawn",
            {"agent": "claude", "name": name},
        )
        assert_is(
            f"worker agent_spawn(name={name!r}) -> Deny",
            result,
            PermissionResultDeny,
        )
        assert_eq(
            f"worker agent_spawn(name={name!r}) deny names '{name}'",
            name in result.message,
            True,
        )
        assert_eq(
            f"worker agent_spawn(name={name!r}) deny mentions debug-/test- prefix",
            "debug-" in result.message and "test-" in result.message,
            True,
        )


async def test_worker_agent_spawn_denies_missing_name() -> None:
    gate = make_gate("worker")
    missing_name_payloads = [
        {"agent": "claude"},                            # no name key
        {"agent": "claude", "name": ""},               # empty string
        {"agent": "claude", "name": None},             # None
        {"agent": "claude", "project": "debug-foo"},   # relies on daemon auto-derive
        {"agent": "claude", "project": "debug-foo", "working_dir": "/tmp/x"},
    ]
    for payload in missing_name_payloads:
        result = await call_gate(gate, "mcp__alor__agent_spawn", payload)
        assert_is(
            f"worker agent_spawn({payload}) -> Deny (missing name)",
            result,
            PermissionResultDeny,
        )
        assert_eq(
            f"worker agent_spawn({payload}) deny mentions 'explicit name'",
            "explicit `name`" in result.message or "explicit `name" in result.message,
            True,
        )
        assert_eq(
            f"worker agent_spawn({payload}) deny says auto-derive disabled",
            "auto-derivation is disabled" in result.message,
            True,
        )


async def test_worker_agent_spawn_denial_order_templates_first() -> None:
    """When both template + name prefix are wrong, the template deny
    wins — it's the most likely root cause (the LLM picked the wrong
    slot), so the error should point there first."""
    gate = make_gate("worker")
    result = await call_gate(
        gate,
        "mcp__alor__agent_spawn",
        {"agent": "claude-alor", "name": "production"},
    )
    assert_is("both-wrong spawn -> Deny", result, PermissionResultDeny)
    # Template message mentions "generic templates"; name-prefix
    # message doesn't — so presence of the former confirms order.
    assert_eq(
        "both-wrong: template error fires before name-prefix error",
        "generic templates" in result.message,
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


async def test_orch_agent_spawn_ignores_worker_restrictions() -> None:
    """Orch can spawn anything — the template allowlist, explicit-name
    requirement, and prefix check only apply to the worker role."""
    gate = make_gate("orch")
    orch_spawns = [
        {"agent": "claude-alor"},                                       # fixed slot
        {"agent": "cursor-alor", "name": "production-worker"},          # no prefix
        {"agent": "claude", "project": "mandaspace"},                   # auto-derive
        {"agent": "claude", "name": "prod-deploy"},                     # non-debug prefix
        {},                                                             # orch passes raw — daemon validates
    ]
    for payload in orch_spawns:
        result = await call_gate(gate, "mcp__alor__agent_spawn", payload)
        assert_is(
            f"orch agent_spawn({payload}) -> Allow (worker rules don't apply)",
            result,
            PermissionResultAllow,
        )


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
    await test_worker_spawnable_templates_and_prefixes_locked()
    await test_worker_agent_spawn_allows_generic_templates_with_prefix()
    await test_worker_agent_spawn_denies_non_template_agents()
    await test_worker_agent_spawn_denies_bad_name_prefix()
    await test_worker_agent_spawn_denies_missing_name()
    await test_worker_agent_spawn_denial_order_templates_first()
    await test_orch_unrestricted_on_alor_tools()
    await test_orch_agent_spawn_ignores_worker_restrictions()

    await test_push_notification_not_gated()
    await test_worktree_tools_not_gated()

    print()
    print("PASS — host-UI tool gate denies confirmed-broken tools, allows everything else.")
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
