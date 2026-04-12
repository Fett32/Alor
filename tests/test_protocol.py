"""Tests for protocol messages."""

import json
from vaelkor.protocol.messages import (
    Message,
    MessageType,
    make_task_assign,
    make_task_result,
    make_wrapper_status,
    AgentStatus,
)


def test_message_roundtrip():
    msg = Message(
        type=MessageType.TASK_ASSIGN,
        from_agent="claude",
        to_agent="codex",
        task_id="task-001",
        summary="Review the auth code",
    )
    json_str = msg.to_json()
    parsed = Message.from_json(json_str)

    assert parsed.type == MessageType.TASK_ASSIGN
    assert parsed.from_agent == "claude"
    assert parsed.to_agent == "codex"
    assert parsed.task_id == "task-001"
    assert parsed.summary == "Review the auth code"


def test_make_task_assign():
    msg = make_task_assign(
        from_agent="orchestrator",
        to_agent="codex",
        task_id="task-002",
        summary="Check for race conditions",
        scope=["src/auth/*"],
        constraints=["review_only", "no_file_edits"],
        context="Focus on token refresh",
    )

    assert msg.type == MessageType.TASK_ASSIGN
    assert msg.body["scope"] == ["src/auth/*"]
    assert "review_only" in msg.body["constraints"]


def test_make_wrapper_status():
    msg = make_wrapper_status("claude", AgentStatus.RUNNING, pid=1234)

    assert msg.type == MessageType.WRAPPER_STATUS
    assert msg.body["agent"] == "claude"
    assert msg.body["status"] == "running"
    assert msg.body["pid"] == 1234
