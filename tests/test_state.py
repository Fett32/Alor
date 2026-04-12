"""Tests for state management."""

import tempfile
from pathlib import Path

from vaelkor.daemon.state import (
    Task,
    TaskState,
    SessionState,
    can_transition,
    TASK_TRANSITIONS,
)


def test_task_state_transitions():
    # Valid transitions
    assert can_transition(TaskState.ASSIGNED, TaskState.ACCEPTED)
    assert can_transition(TaskState.ASSIGNED, TaskState.REJECTED)
    assert can_transition(TaskState.ACCEPTED, TaskState.COMPLETED)
    assert can_transition(TaskState.ACCEPTED, TaskState.BLOCKED)
    assert can_transition(TaskState.INTERRUPTED, TaskState.ACCEPTED)
    assert can_transition(TaskState.INTERRUPTED, TaskState.STALE)

    # Invalid transitions
    assert not can_transition(TaskState.COMPLETED, TaskState.ACCEPTED)
    assert not can_transition(TaskState.REJECTED, TaskState.ACCEPTED)
    assert not can_transition(TaskState.ASSIGNED, TaskState.COMPLETED)


def test_task_serialization():
    task = Task(
        task_id="task-001",
        summary="Test task",
        assigned_to="codex",
        scope=["src/*"],
        constraints=["review_only"],
    )

    d = task.to_dict()
    restored = Task.from_dict(d)

    assert restored.task_id == "task-001"
    assert restored.summary == "Test task"
    assert restored.state == TaskState.ASSIGNED
    assert restored.scope == ["src/*"]


def test_session_persistence():
    with tempfile.TemporaryDirectory() as tmpdir:
        path = Path(tmpdir) / "state.json"

        session = SessionState(session_id="test-session")
        session.tasks["task-001"] = Task(
            task_id="task-001",
            summary="Test",
            assigned_to="claude",
        )

        session.save(path)

        loaded = SessionState.load(path)
        assert loaded.session_id == "test-session"
        assert "task-001" in loaded.tasks
        assert loaded.tasks["task-001"].summary == "Test"
