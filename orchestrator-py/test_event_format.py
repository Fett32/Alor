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


def test_task_completed_with_has_details_appends_task_get_pointer() -> None:
    """Audit 8b03cae6 fix #1: the worker splits its report into a terse
    `summary` (injected verbatim) + an optional `details` (stashed on
    the Task). When `has_details: true` rides on the event, the
    formatter must append a `task_get(task_id=…)` pointer so the orch
    knows where the full report lives. The full task_id (not the
    8-char preview) has to land in the pointer so the orch can paste
    it straight into a tool call.
    """
    full_id = "abcdef0123456789abcdef0123456789"
    msg = orch_main.format_event_for_agent(
        evt(
            "task.completed",
            {
                "task_id": full_id,
                "agent_id": "claude-alor",
                "title": "Run the thing",
                "summary": "Done; see details.",
                "has_details": True,
            },
        )
    )
    assert msg is not None
    assert_contains("has_details: still renders terse summary", msg, "Done; see details.")
    assert_contains("has_details: appends task_get pointer", msg, "task_get(")
    assert_contains("has_details: pointer carries full task_id", msg, full_id)
    assert_contains(
        "has_details: pointer mentions details field",
        msg,
        "`details`",
    )


def test_task_completed_without_has_details_omits_pointer() -> None:
    """Back-compat path: `has_details` absent (old daemon) or False (new
    daemon, worker's report fit in the terse budget) → no task_get
    pointer. Short reports must still render as they did pre-fix.
    """
    msg = orch_main.format_event_for_agent(
        evt(
            "task.completed",
            {
                "task_id": "abcdef0123456789",
                "agent_id": "claude-alor",
                "title": "Tiny task",
                "summary": "Done.",
                # has_details deliberately omitted
            },
        )
    )
    assert msg is not None
    assert_contains("no has_details: terse summary still rendered", msg, "Done.")
    assert_eq(
        "no has_details: no task_get pointer appended",
        "task_get(" in msg,
        False,
    )

    # Explicit False is equivalent to absent.
    msg = orch_main.format_event_for_agent(
        evt(
            "task.completed",
            {
                "task_id": "abcdef0123456789",
                "agent_id": "claude-alor",
                "title": "Tiny task",
                "summary": "Done.",
                "has_details": False,
            },
        )
    )
    assert msg is not None
    assert_eq(
        "has_details=False: no task_get pointer appended",
        "task_get(" in msg,
        False,
    )


def test_worker_orch_response_under_cap_is_not_truncated() -> None:
    """Audit 8b03cae6 fix #4: replies under EVENT_TEXT_INJECT_MAX_BYTES
    (2 KiB) must round-trip verbatim — no marker, no allocation
    surprises, no behavior change for routine short replies. This is
    the back-compat contract: the cap is a ceiling, not a rewrite."""
    body = "Short reply, just a line or two about what I found."
    corrid = "ccccccccdddddddd11112222aaaabbbb"
    msg = orch_main.format_event_for_agent(
        evt(
            "worker.orch_response",
            {
                "agent_id": "claude-alor",
                "correlation_id": corrid,
                "text": body,
            },
        )
    )
    assert msg is not None
    assert_contains("short reply: body round-trips verbatim", msg, body)
    assert_eq(
        "short reply: no truncation marker",
        "[truncated" in msg,
        False,
    )
    assert_eq(
        "short reply: no fetch-on-demand pointer",
        "worker_response_get(" in msg,
        False,
    )


def test_worker_orch_response_over_cap_truncates_with_fetch_hint() -> None:
    """The main fix: a 5 KB reply must get capped at
    EVENT_TEXT_INJECT_MAX_BYTES (2 KiB) with a marker that points
    the orch at `worker_response_get(correlation_id=<full_uuid>)`
    for the rest of the body. The correlation_id in the marker is
    the FULL uuid so the LLM can paste it straight into a tool
    call without reconstructing from a prefix."""
    big = "X" * 5_000
    corrid = "fefefefe0000111122223333aabbccdd"
    msg = orch_main.format_event_for_agent(
        evt(
            "worker.orch_response",
            {
                "agent_id": "claude-mandaforge",
                "correlation_id": corrid,
                "text": big,
            },
        )
    )
    assert msg is not None
    # The injected text must have been capped somewhere under the
    # 5 KB original. Acceptance criterion: ≤ ~2150 B (cap + marker +
    # envelope preamble). Be generous — envelope preamble is a few
    # hundred bytes. Tight upper bound: 2.5 KB total.
    msg_bytes = len(msg.encode("utf-8"))
    assert_true_label = "truncated injection stays under ~2.5 KB"
    if msg_bytes >= 2500:
        print(f"FAIL  {assert_true_label}: got {msg_bytes} bytes")
        raise SystemExit(1)
    print(f"ok    {assert_true_label} (got {msg_bytes} B)")

    # The marker must point at worker_response_get with the FULL uuid.
    assert_contains(
        "truncation marker names worker_response_get",
        msg,
        "worker_response_get(",
    )
    assert_contains(
        "truncation marker carries full correlation_id (not just the 8-char prefix)",
        msg,
        corrid,
    )
    assert_contains("truncation marker uses `[truncated` keyword", msg, "[truncated")


def test_worker_user_input_over_cap_truncates_without_fetch_hint() -> None:
    """user_input has no correlation_id (Fett just typed into the
    pane), so truncation is terminal — no daemon-side cache to
    fall back on. The formatter must still cap the text but emit
    only the plain marker, not a broken tool-call hint."""
    big = "Y" * 5_000
    msg = orch_main.format_event_for_agent(
        evt(
            "worker.user_input",
            {
                "agent_id": "claude-alor",
                "text": big,
                "during_task": False,
            },
        )
    )
    assert msg is not None
    msg_bytes = len(msg.encode("utf-8"))
    # Same ~2.5 KB ceiling (envelope preamble for this event is a
    # bit larger — more static prose — but still well under).
    label = "user_input truncated injection stays under ~3 KB"
    if msg_bytes >= 3000:
        print(f"FAIL  {label}: got {msg_bytes} bytes")
        raise SystemExit(1)
    print(f"ok    {label} (got {msg_bytes} B)")

    # Plain marker — no fetch pointer, since the daemon never cached
    # a user_input (no correlation_id to key by).
    assert_contains(
        "user_input truncation marker is plain `[truncated]`",
        msg,
        "[truncated]",
    )
    assert_eq(
        "user_input truncation does NOT suggest worker_response_get",
        "worker_response_get(" in msg,
        False,
    )


def test_truncate_helper_is_utf8_boundary_safe() -> None:
    """Direct unit test of the helper — the formatter tests above
    exercise it end-to-end with ASCII; pin the UTF-8 boundary
    property explicitly here so a future rewrite can't silently
    break multi-byte handling."""
    # 2-byte chars: 1500 × 2 = 3000 bytes, well over the 2 KiB cap.
    # If the cut lands mid-codepoint we'd emit invalid UTF-8 which
    # json/Python would mangle on encode.
    s = "á" * 1500  # 3000 bytes of valid UTF-8
    assert len(s.encode("utf-8")) > orch_main.EVENT_TEXT_INJECT_MAX_BYTES
    out = orch_main._truncate_text_for_inject(s, marker="…x")
    out_bytes = out.encode("utf-8")
    assert len(out_bytes) <= orch_main.EVENT_TEXT_INJECT_MAX_BYTES, (
        f"truncated output must respect cap; got {len(out_bytes)} bytes"
    )
    # Round-trip through bytes with strict UTF-8 — the output must be
    # valid UTF-8, not a mid-codepoint prefix.
    out_bytes.decode("utf-8")  # raises UnicodeDecodeError if broken
    assert_contains("helper appends the marker", out, "…x")


def main() -> int:
    test_task_completed_full_payload()
    test_task_completed_no_summary()
    test_task_completed_missing_title_fallback()
    test_task_blocked_and_other_paths_unchanged()
    test_task_completed_with_has_details_appends_task_get_pointer()
    test_task_completed_without_has_details_omits_pointer()
    test_worker_orch_response_under_cap_is_not_truncated()
    test_worker_orch_response_over_cap_truncates_with_fetch_hint()
    test_worker_user_input_over_cap_truncates_without_fetch_hint()
    test_truncate_helper_is_utf8_boundary_safe()

    print()
    print("PASS — task.completed renders a single rich injection with title + report.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
