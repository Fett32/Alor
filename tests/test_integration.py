"""
Integration tests - wrapper and daemon communication.

Requires tmux to be installed: sudo apt install tmux
"""

import asyncio
import shutil
import tempfile
from pathlib import Path

import pytest

from vaelkor.wrapper.base import AgentWrapper, WrapperConfig
from vaelkor.daemon.core import Daemon
from vaelkor.protocol.messages import Message, MessageType


# Skip all tests if tmux not installed
pytestmark = pytest.mark.skipif(
    shutil.which("tmux") is None,
    reason="tmux not installed - run: sudo apt install tmux"
)


@pytest.fixture
def socket_dir():
    with tempfile.TemporaryDirectory() as tmpdir:
        yield Path(tmpdir)


@pytest.fixture
def data_dir():
    with tempfile.TemporaryDirectory() as tmpdir:
        yield Path(tmpdir)


@pytest.mark.asyncio
async def test_wrapper_starts_and_responds(socket_dir):
    """Wrapper starts tmux session and responds to status queries."""
    config = WrapperConfig(
        agent_name="test-agent",
        socket_path=socket_dir / "test.sock",
        command=["bash"],  # Simple shell, not real agent
        session_prefix="vaelkor-test",
    )

    wrapper = AgentWrapper(config)
    await wrapper.start()

    try:
        assert wrapper.status.value == "running"
        assert wrapper.session_name == "vaelkor-test-test-agent"

        # Connect to wrapper socket
        reader, writer = await asyncio.open_unix_connection(str(config.socket_path))

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
        assert response.body["status"] == "running"

        writer.close()
        await writer.wait_closed()

    finally:
        await wrapper.stop()


@pytest.mark.asyncio
async def test_wrapper_receives_task(socket_dir):
    """Wrapper receives task and injects prompt into agent."""
    config = WrapperConfig(
        agent_name="test-agent",
        socket_path=socket_dir / "test.sock",
        command=["bash"],
        session_prefix="vaelkor-test",
    )

    wrapper = AgentWrapper(config)
    await wrapper.start()

    try:
        reader, writer = await asyncio.open_unix_connection(str(config.socket_path))

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
async def test_daemon_connects_to_wrapper(socket_dir, data_dir):
    """Daemon can connect to a running wrapper."""
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
        daemon.socket_dir = socket_dir  # Use test socket dir
        await daemon.start()

        connected = await daemon.connect_wrapper("claude")
        assert connected
        assert "claude" in daemon.wrappers
        assert daemon.wrappers["claude"].connected

        await daemon.stop()

    finally:
        await wrapper.stop()


@pytest.mark.asyncio
async def test_full_task_flow(socket_dir, data_dir):
    """Full flow: daemon assigns task, wrapper receives it."""
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
