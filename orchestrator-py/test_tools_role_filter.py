"""Regression tests for tools.build_server / allowed_tool_names role filter.

Workers get a restricted MCP surface (agent_spawn, agent_list,
agent_ensure_running, agent_send_message, agent_kill). The
orchestrator keeps the full 13-tool set. This file verifies the
split — specifically the tool-name membership that flows into
ClaudeAgentOptions.allowed_tools and create_sdk_mcp_server.

Run standalone: `python3 test_tools_role_filter.py` from
orchestrator-py/. Exits 0 on pass. No pytest dependency.
"""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

import tools  # noqa: E402


FAILURES: list[str] = []


def check(label: str, got, want) -> None:
    if got != want:
        FAILURES.append(label)
        print(f"FAIL  {label}")
        print(f"  got : {got!r}")
        print(f"  want: {want!r}")
    else:
        print(f"ok    {label}")


def check_true(label: str, cond: bool) -> None:
    check(label, bool(cond), True)


def bare_names(qualified: list[str]) -> set[str]:
    """Strip mcp__<server>__ prefix and return the bare tool names."""
    prefix = f"mcp__{tools.MCP_SERVER_NAME}__"
    return {n[len(prefix):] for n in qualified if n.startswith(prefix)}


def test_worker_accessible_tools_exact_set() -> None:
    # Locked set — drift should be deliberate and test-visible.
    expected = frozenset(
        {
            "agent_spawn",
            "agent_list",
            "agent_ensure_running",
            "agent_send_message",
            "agent_kill",
        }
    )
    check(
        "WORKER_ACCESSIBLE_TOOLS exact set", tools.WORKER_ACCESSIBLE_TOOLS, expected
    )


def test_allowed_tool_names_orch_has_all_13() -> None:
    names = bare_names(tools.allowed_tool_names("orch"))
    expected = {t.name for t in tools.ALL_TOOLS}
    check("orch allowed_tool_names == every ALL_TOOLS name", names, expected)
    check_true("orch has task_create", "task_create" in names)
    check_true("orch has memory_get", "memory_get" in names)


def test_allowed_tool_names_worker_is_the_five() -> None:
    names = bare_names(tools.allowed_tool_names("worker"))
    check(
        "worker allowed_tool_names == WORKER_ACCESSIBLE_TOOLS",
        names,
        set(tools.WORKER_ACCESSIBLE_TOOLS),
    )
    # Spot-check the exclusions — these must NOT leak into the
    # worker's allow-list.
    for excluded in (
        "task_create",
        "task_assign",
        "task_cancel",
        "task_get",
        "task_list",
        "task_intervention_clear",
        "project_get",
        "project_list",
        "memory_get",
    ):
        check(
            f"worker allowed_tool_names excludes {excluded}",
            excluded in names,
            False,
        )


def test_unknown_role_falls_back_to_worker() -> None:
    # Matches tool_gate.make_gate's "safer default" principle.
    unknown = bare_names(tools.allowed_tool_names("some-future-role"))
    worker = bare_names(tools.allowed_tool_names("worker"))
    check("unknown role falls back to worker surface", unknown, worker)


def test_build_server_tool_counts_match_allowed_names() -> None:
    """Sanity: the server's tool list and the allow-list stay in sync.

    A regression that updated one without the other would silently
    either expose unlisted tools or advertise tools that aren't
    actually served.
    """
    for role in ("orch", "worker"):
        server_tools = tools._tools_for_role(role)
        allow_names = bare_names(tools.allowed_tool_names(role))
        check(
            f"{role}: server tool count == allow_names count",
            len(server_tools),
            len(allow_names),
        )
        check(
            f"{role}: tool name sets match",
            {t.name for t in server_tools},
            allow_names,
        )


def main() -> int:
    test_worker_accessible_tools_exact_set()
    test_allowed_tool_names_orch_has_all_13()
    test_allowed_tool_names_worker_is_the_five()
    test_unknown_role_falls_back_to_worker()
    test_build_server_tool_counts_match_allowed_names()

    print()
    if FAILURES:
        print(f"FAIL — {len(FAILURES)} check(s) failed:")
        for f in FAILURES:
            print(f"  - {f}")
        return 1
    print("PASS — tools.build_server/allowed_tool_names respect the role split.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
