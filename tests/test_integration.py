"""
Integration tests - wrapper and daemon communication.

Requires tmux to be installed: sudo apt install tmux
"""

import asyncio
import json
import shutil
import tempfile
from pathlib import Path

import pytest

from vaelkor.wrapper.base import AgentWrapper, WrapperConfig
from vaelkor.daemon.core import Daemon
from vaelkor.daemon.state import AgentState, SessionState, Task, TaskState
from vaelkor.protocol.messages import Message, MessageType


# Skip all tests if tmux not installed
pytestmark = pytest.mark.skipif(
    shutil.which("tmux") is None,
    reason="tmux not installed - run: sudo apt install tmux"
)


@pytest.mark.asyncio
async def test_wrapper_starts_and_responds():
    """Wrapper starts tmux session and responds to status queries."""
    with tempfile.TemporaryDirectory() as tmpdir:
        config = WrapperConfig(
            agent_name="test-agent",
            socket_path=Path(tmpdir) / "test.sock",
            command=["bash"],
            session_prefix="vaelkor-test",
        )

        wrapper = AgentWrapper(config)
        await wrapper.start()

        try:
            assert wrapper.status.value in ("running", "idle")
            assert wrapper.session_name == "vaelkor-test-test-agent"

            # Connect to wrapper socket
            reader, writer = await asyncio.open_unix_connection(
                str(config.socket_path)
            )

            # Send status query
            msg = Message(
                type=MessageType.WRAPPER_GET_STATUS,
                from_agent="daemon",
                to_agent="wrapper",
            )
            writer.write((msg.to_json() + "\n").encode())
            await writer.drain()

            # Read response
            line = await asyncio.wait_for(reader.readline(), timeout=5.0)
            response = Message.from_json(line.decode().strip())

            assert response.type == MessageType.WRAPPER_STATUS
            assert response.body["status"] in ("running", "idle")

            writer.close()
            await writer.wait_closed()
        finally:
            await wrapper.stop()


@pytest.mark.asyncio
async def test_wrapper_receives_task():
    """Wrapper receives task and injects prompt into agent."""
    with tempfile.TemporaryDirectory() as tmpdir:
        config = WrapperConfig(
            agent_name="test-agent",
            socket_path=Path(tmpdir) / "test.sock",
            command=["bash"],
            session_prefix="vaelkor-test",
        )

        wrapper = AgentWrapper(config)
        await wrapper.start()

        try:
            reader, writer = await asyncio.open_unix_connection(
                str(config.socket_path)
            )

            # Send task
            msg = Message(
                type=MessageType.WRAPPER_SEND_TASK,
                from_agent="daemon",
                to_agent="wrapper",
                task_id="task-001",
                summary="Test task",
                body={
                    "scope": ["src/*"],
                    "constraints": ["review_only"],
                    "context": "Testing",
                    "body": "Please review the code.",
                },
            )
            writer.write((msg.to_json() + "\n").encode())
            await writer.drain()

            # Read ack
            line = await asyncio.wait_for(reader.readline(), timeout=5.0)
            response = Message.from_json(line.decode().strip())

            assert response.type == MessageType.WRAPPER_ACK
            assert response.body["task_id"] == "task-001"

            # Give tmux a moment to receive the input
            await asyncio.sleep(0.5)

            # Get transcript to verify prompt was injected
            msg = Message(
                type=MessageType.WRAPPER_TAIL_TRANSCRIPT,
                from_agent="daemon",
                to_agent="wrapper",
                body={"lines": 50},
            )
            writer.write((msg.to_json() + "\n").encode())
            await writer.drain()

            line = await asyncio.wait_for(reader.readline(), timeout=5.0)
            response = Message.from_json(line.decode().strip())

            assert response.type == MessageType.WRAPPER_TRANSCRIPT
            transcript = "\n".join(response.body["lines"])
            assert "TASK FROM ORCHESTRATOR" in transcript
            assert "task-001" in transcript

            writer.close()
            await writer.wait_closed()
        finally:
            await wrapper.stop()


@pytest.mark.asyncio
async def test_daemon_connects_to_wrapper():
    """Daemon can connect to a running wrapper."""
    with tempfile.TemporaryDirectory() as socket_tmpdir, \
         tempfile.TemporaryDirectory() as data_tmpdir:

        socket_dir = Path(socket_tmpdir)
        data_dir = Path(data_tmpdir)

        # Start wrapper first
        config = WrapperConfig(
            agent_name="claude",
            socket_path=socket_dir / "claude.sock",
            command=["bash"],
            session_prefix="vaelkor-test",
        )
        wrapper = AgentWrapper(config)
        await wrapper.start()

        try:
            # Start daemon and connect
            daemon = Daemon(data_dir=data_dir)
            daemon.socket_dir = socket_dir
            await daemon.start()

            connected = await daemon.connect_wrapper("claude")
            assert connected
            assert "claude" in daemon.wrappers
            assert daemon.wrappers["claude"].connected

            await daemon.stop()
        finally:
            await wrapper.stop()


@pytest.mark.asyncio
async def test_full_task_flow():
    """Full flow: daemon assigns task, wrapper receives it."""
    with tempfile.TemporaryDirectory() as socket_tmpdir, \
         tempfile.TemporaryDirectory() as data_tmpdir:

        socket_dir = Path(socket_tmpdir)
        data_dir = Path(data_tmpdir)

        # Start wrapper
        config = WrapperConfig(
            agent_name="claude",
            socket_path=socket_dir / "claude.sock",
            command=["bash"],
            session_prefix="vaelkor-test",
        )
        wrapper = AgentWrapper(config)
        await wrapper.start()

        try:
            # Start daemon
            daemon = Daemon(data_dir=data_dir)
            daemon.socket_dir = socket_dir
            await daemon.start()
            await daemon.connect_wrapper("claude")

            # Assign task
            task = await daemon.assign_task(
                to_agent="claude",
                summary="Review authentication code",
                scope=["src/auth/*"],
                constraints=["review_only", "no_file_edits"],
                context="Focus on race conditions",
            )

            assert task is not None
            assert task.task_id == "task-001"
            assert task.assigned_to == "claude"
            # Task should be ACCEPTED since wrapper ACK'd
            assert task.state.value == "ACCEPTED"

            # Verify task is in daemon state
            assert "task-001" in daemon.state.tasks

            # Get transcript to verify it reached the wrapper
            await asyncio.sleep(0.5)
            transcript = await daemon.get_agent_transcript("claude", 50)
            transcript_text = "\n".join(transcript)

            assert "TASK FROM ORCHESTRATOR" in transcript_text
            assert "Review authentication code" in transcript_text

            await daemon.stop()
        finally:
            await wrapper.stop()


@pytest.mark.asyncio
async def test_task_timeout():
    """Task times out when no wrapper is connected."""
    with tempfile.TemporaryDirectory() as data_tmpdir:
        data_dir = Path(data_tmpdir)

        # Start daemon WITHOUT wrapper
        daemon = Daemon(data_dir=data_dir)
        await daemon.start()

        try:
            # Assign task to non-existent wrapper (should stay ASSIGNED)
            task = await daemon.assign_task(
                to_agent="nonexistent",
                summary="This will timeout",
            )

            assert task is not None
            assert task.state.value == "ASSIGNED"  # Not ACCEPTED since no wrapper

            # Don't wait for full 30s timeout in test
            # Just verify the task was created in ASSIGNED state

            await daemon.stop()
        finally:
            pass


@pytest.mark.asyncio
async def test_task_accepted_after_ack():
    """Task transitions to ACCEPTED state after wrapper ACKs."""
    with tempfile.TemporaryDirectory() as socket_tmpdir, \
         tempfile.TemporaryDirectory() as data_tmpdir:

        socket_dir = Path(socket_tmpdir)
        data_dir = Path(data_tmpdir)

        # Start wrapper
        config = WrapperConfig(
            agent_name="claude",
            socket_path=socket_dir / "claude.sock",
            command=["bash"],
            session_prefix="vaelkor-test",
        )
        wrapper = AgentWrapper(config)
        await wrapper.start()

        try:
            # Start daemon and connect
            daemon = Daemon(data_dir=data_dir)
            daemon.socket_dir = socket_dir
            await daemon.start()
            await daemon.connect_wrapper("claude")

            # Task should be ASSIGNED initially, then ACCEPTED after wrapper ACKs
            task = await daemon.assign_task(
                to_agent="claude",
                summary="Test task for acceptance",
            )

            assert task is not None
            # After ACK, task should be ACCEPTED
            assert task.state.value == "ACCEPTED"

            await daemon.stop()
        finally:
            await wrapper.stop()


@pytest.mark.asyncio
async def test_task_timeout_state_transition():
    """Task transitions to TIMED_OUT after timeout when unacknowledged."""
    with tempfile.TemporaryDirectory() as data_tmpdir:
        data_dir = Path(data_tmpdir)

        # Start daemon with very short timeout
        daemon = Daemon(data_dir=data_dir, task_assignment_timeout=0.5)
        await daemon.start()

        try:
            task = await daemon.assign_task(
                to_agent="nonexistent",
                summary="This will timeout quickly",
            )

            assert task is not None
            assert task.state.value == "ASSIGNED"

            # Wait for timeout
            await asyncio.sleep(0.7)

            # Task should now be TIMED_OUT
            updated_task = daemon.get_task(task.task_id)
            assert updated_task.state.value == "TIMED_OUT"

            await daemon.stop()
        finally:
            pass


@pytest.mark.asyncio
async def test_input_mode_override():
    """Override mode wraps input with task context."""
    with tempfile.TemporaryDirectory() as socket_tmpdir, \
         tempfile.TemporaryDirectory() as data_tmpdir:

        socket_dir = Path(socket_tmpdir)
        data_dir = Path(data_tmpdir)

        config = WrapperConfig(
            agent_name="claude",
            socket_path=socket_dir / "claude.sock",
            command=["bash"],
            session_prefix="vaelkor-test",
        )
        wrapper = AgentWrapper(config)
        await wrapper.start()

        try:
            daemon = Daemon(data_dir=data_dir)
            daemon.socket_dir = socket_dir
            await daemon.start()
            await daemon.connect_wrapper("claude")

            # Assign task first so override has context
            await daemon.assign_task(
                to_agent="claude",
                summary="Test task",
            )

            # Send override input
            await daemon.send_user_input("claude", "focus on auth", "override")

            # Check transcript for OVERRIDE prefix
            await asyncio.sleep(0.3)
            transcript = await daemon.get_agent_transcript("claude", 50)
            transcript_text = "\n".join(transcript)

            assert "[OVERRIDE for task-001]" in transcript_text
            assert "focus on auth" in transcript_text

            await daemon.stop()
        finally:
            await wrapper.stop()


@pytest.mark.asyncio
async def test_input_mode_chat():
    """Chat mode passes input without wrapping."""
    with tempfile.TemporaryDirectory() as socket_tmpdir, \
         tempfile.TemporaryDirectory() as data_tmpdir:

        socket_dir = Path(socket_tmpdir)
        data_dir = Path(data_tmpdir)

        config = WrapperConfig(
            agent_name="claude",
            socket_path=socket_dir / "claude.sock",
            command=["bash"],
            session_prefix="vaelkor-test",
        )
        wrapper = AgentWrapper(config)
        await wrapper.start()

        try:
            daemon = Daemon(data_dir=data_dir)
            daemon.socket_dir = socket_dir
            await daemon.start()
            await daemon.connect_wrapper("claude")

            # Assign task first
            await daemon.assign_task(
                to_agent="claude",
                summary="Test task",
            )

            # Send chat input
            await daemon.send_user_input("claude", "hello there", "chat")

            # Check transcript - should NOT have OVERRIDE prefix
            await asyncio.sleep(0.3)
            transcript = await daemon.get_agent_transcript("claude", 50)
            transcript_text = "\n".join(transcript)

            assert "hello there" in transcript_text
            assert "[OVERRIDE" not in transcript_text

            await daemon.stop()
        finally:
            await wrapper.stop()


@pytest.mark.asyncio
async def test_recovery_tasks_become_stale_without_wrapper():
    """Tasks in flight during crash become STALE if wrapper unreachable."""
    with tempfile.TemporaryDirectory() as data_tmpdir:
        data_dir = Path(data_tmpdir)
        session_id = "test-recovery-session"

        # Create a crashed session state with in-flight tasks
        session_dir = data_dir / "sessions" / session_id
        session_dir.mkdir(parents=True)

        state = SessionState(session_id=session_id, clean_shutdown=False)
        state.agents["claude"] = AgentState(name="claude", status="running")
        state.tasks["task-001"] = Task(
            task_id="task-001",
            summary="Task that was in flight",
            assigned_to="claude",
            state=TaskState.ACCEPTED,
        )
        state.tasks["task-002"] = Task(
            task_id="task-002",
            summary="Another in-flight task",
            assigned_to="claude",
            state=TaskState.ASSIGNED,
        )
        state.save(session_dir / "state.json")

        # Start daemon with crashed session - no wrapper available
        daemon = Daemon(data_dir=data_dir)
        await daemon.start(session_id)

        try:
            # Both tasks should be STALE since wrapper is unreachable
            assert daemon.state.tasks["task-001"].state == TaskState.STALE
            assert daemon.state.tasks["task-002"].state == TaskState.STALE

            await daemon.stop()
        finally:
            pass


@pytest.mark.asyncio
async def test_recovery_resolves_to_accepted_if_current():
    """Task resolves to ACCEPTED if wrapper reports it as current."""
    with tempfile.TemporaryDirectory() as socket_tmpdir, \
         tempfile.TemporaryDirectory() as data_tmpdir:

        socket_dir = Path(socket_tmpdir)
        data_dir = Path(data_tmpdir)
        session_id = "test-recovery-current"

        # Start wrapper first and give it a current task
        config = WrapperConfig(
            agent_name="claude",
            socket_path=socket_dir / "claude.sock",
            command=["bash"],
            session_prefix="vaelkor-test",
        )
        wrapper = AgentWrapper(config)
        await wrapper.start()
        wrapper.current_task_id = "task-001"  # Simulate task in progress

        try:
            # Create crashed session state
            session_dir = data_dir / "sessions" / session_id
            session_dir.mkdir(parents=True)

            state = SessionState(session_id=session_id, clean_shutdown=False)
            state.agents["claude"] = AgentState(name="claude", status="running")
            state.tasks["task-001"] = Task(
                task_id="task-001",
                summary="Task still running in wrapper",
                assigned_to="claude",
                state=TaskState.ACCEPTED,
            )
            state.save(session_dir / "state.json")

            # Start daemon - should recover and find task still active
            daemon = Daemon(data_dir=data_dir)
            daemon.socket_dir = socket_dir
            await daemon.start(session_id)

            # Task should resolve back to ACCEPTED
            assert daemon.state.tasks["task-001"].state == TaskState.ACCEPTED

            await daemon.stop()
        finally:
            await wrapper.stop()


@pytest.mark.asyncio
async def test_recovery_resolves_to_completed_if_done():
    """Task resolves to COMPLETED if wrapper reports it in completed_tasks."""
    with tempfile.TemporaryDirectory() as socket_tmpdir, \
         tempfile.TemporaryDirectory() as data_tmpdir:

        socket_dir = Path(socket_tmpdir)
        data_dir = Path(data_tmpdir)
        session_id = "test-recovery-completed"

        # Start wrapper and mark task as completed
        config = WrapperConfig(
            agent_name="claude",
            socket_path=socket_dir / "claude.sock",
            command=["bash"],
            session_prefix="vaelkor-test",
        )
        wrapper = AgentWrapper(config)
        await wrapper.start()
        wrapper._completed_tasks.append("task-001")  # Task finished during crash

        try:
            # Create crashed session state
            session_dir = data_dir / "sessions" / session_id
            session_dir.mkdir(parents=True)

            state = SessionState(session_id=session_id, clean_shutdown=False)
            state.agents["claude"] = AgentState(name="claude", status="running")
            state.tasks["task-001"] = Task(
                task_id="task-001",
                summary="Task that completed during crash",
                assigned_to="claude",
                state=TaskState.ACCEPTED,
            )
            state.save(session_dir / "state.json")

            # Start daemon - should recover and find task completed
            daemon = Daemon(data_dir=data_dir)
            daemon.socket_dir = socket_dir
            await daemon.start(session_id)

            # Task should resolve to COMPLETED
            assert daemon.state.tasks["task-001"].state == TaskState.COMPLETED
            assert daemon.state.tasks["task-001"].completed_at is not None

            await daemon.stop()
        finally:
            await wrapper.stop()
