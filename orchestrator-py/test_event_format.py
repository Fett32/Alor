"""Regression test for orchestrator event → SDK-injection formatting.

Locks the shape of `format_event_for_agent` — specifically for
`task.completed`, which was reworked to carry `title` alongside
`task_id` / `agent_id` / `summary` after the tmux-poke dedup (one rich
injection replaces the prior two: rich SDK inject + terse tmux-poke).

Acceptance criteria from the dedup task:
  - Exactly ONE injection per completion (tested by construction here —
    the tmux-poke on the Rust side is gone, not emulated).
  - Surviving injection includes: task id, agent, title, worker report.
  - Tasks missing a title (edge: state race between complete + broadcast)
    still render a coherent message.

Run standalone: `python3 test_event_format.py` from orchestrator-py/.
Exits 0 on pass. No pytest dependency.
"""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

import daemon  # noqa: E402
import main as orch_main  # noqa: E402


def assert_eq(label: str, got, want) -> None:
    if got != want:
        print(f"FAIL  {label}")
        print(f"  got : {got!r}")
        print(f"  want: {want!r}")
        raise SystemExit(1)
    print(f"ok    {label}")


def assert_contains(label: str, haystack: str, needle: str) -> None:
    if needle not in haystack:
        print(f"FAIL  {label}")
        print(f"  haystack: {haystack!r}")
        print(f"  needle  : {needle!r}")
        raise SystemExit(1)
    print(f"ok    {label}")


def evt(event: str, data: dict) -> daemon.Event:
    return daemon.Event(event=event, data=data, timestamp=None)


def test_task_completed_full_payload() -> None:
    # All four required fields land in the rendered injection.
    msg = orch_main.format_event_for_agent(
        evt(
            "task.completed",
            {
                "task_id": "abcdef0123456789",
                "agent_id": "claude-alor",
                "title": "Dedup completion notifications",
                "summary": "Killed the tmux-poke, kept the event path.",
            },
        )
    )
    assert msg is not None, "format returned None for task.completed"
    # Short-form task id (first 8 chars).
    assert_contains("contains 8-char task_id", msg, "abcdef01")
    assert_contains("contains agent_id", msg, "claude-alor")
    assert_contains("contains quoted title", msg, '"Dedup completion notifications"')
    assert_contains("contains Worker report header", msg, "Worker report:")
    assert_contains("contains summary body", msg, "Killed the tmux-poke, kept the event path.")
    assert_contains("keeps [Alor event] prefix", msg, "[Alor event]")


def test_task_completed_no_summary() -> None:
    msg = orch_main.format_event_for_agent(
        evt(
            "task.completed",
            {
                "task_id": "deadbeef00000000",
                "agent_id": "claude-mandaforge",
                "title": "Investigate flaky test",
                # summary deliberately absent
            },
        )
    )
    assert msg is not None
    assert_contains("no-summary: title still present", msg, '"Investigate flaky test"')
    assert_contains("no-summary: agent still present", msg, "claude-mandaforge")
    assert_contains("no-summary: falls back to 'no summary attached'", msg, "no summary attached")
    # No Worker report header on this branch.
    assert_eq(
        "no-summary: no Worker report header",
        "Worker report:" in msg,
        False,
    )


def test_task_completed_missing_title_fallback() -> None:
    """State-race: task pruned between complete and broadcast → no title.
    Formatter must still render a coherent message without the empty
    quoted string ''.
    """
    msg = orch_main.format_event_for_agent(
        evt(
            "task.completed",
            {
                "task_id": "00000000cafe0000",
                "agent_id": "claude-alor",
                # title omitted / empty
                "summary": "ok",
            },
        )
    )
    assert msg is not None
    assert_eq(
        "missing title: no empty quoted string",
        '""' in msg,
        False,
    )
    assert_contains("missing title: agent + id still present", msg, "00000000")
    assert_contains("missing title: summary still rendered", msg, "ok")


def test_task_blocked_and_other_paths_unchanged() -> None:
    """Guard against regressions in other injectable events — we only
    touched task.completed; everything else must keep rendering as
    before."""
    blocked = orch_main.format_event_for_agent(
        evt(
            "task.blocked",
            {
                "task_id": "feedbeef11112222",
                "agent_id": "claude-alor",
                "reason": "scope-check gate fired",
            },
        )
    )
    assert blocked is not None
    assert_contains("task.blocked: still has agent_id", blocked, "claude-alor")
    assert_contains("task.blocked: still has reason", blocked, "scope-check gate fired")

    # Non-injectable event returns None.
    skipped = orch_main.format_event_for_agent(
        evt("something.else", {"agent_id": "x"})
    )
    assert_eq("non-injectable event -> None", skipped, None)


def main() -> int:
    test_task_completed_full_payload()
    test_task_completed_no_summary()
    test_task_completed_missing_title_fallback()
    test_task_blocked_and_other_paths_unchanged()

    print()
    print("PASS — task.completed renders a single rich injection with title + report.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
