"""
Vaelkor control daemon.

Manages agent wrappers, routes messages, maintains task state.
"""

import asyncio
import json
import uuid
from datetime import datetime, UTC
from pathlib import Path

from ..protocol.messages import (
    AgentStatus,
    Message,
    MessageType,
    make_task_assign,
    make_wrapper_status,
)
from .state import (
    AgentState,
    MessageDeliveryState,
    MessageState,
    SessionState,
    Task,
    TaskState,
    can_transition,
)


class WrapperConnection:
    """Connection to an agent wrapper."""

    def __init__(self, agent_name: str, socket_path: Path):
        self.agent_name = agent_name
        self.socket_path = socket_path
        self.reader: asyncio.StreamReader | None = None
        self.writer: asyncio.StreamWriter | None = None
        self.connected = False

    async def connect(self) -> bool:
        try:
            self.reader, self.writer = await asyncio.open_unix_connection(
                str(self.socket_path)
            )
            self.connected = True
            return True
        except (ConnectionRefusedError, FileNotFoundError):
            self.connected = False
            return False

    async def disconnect(self):
        if self.writer:
            self.writer.close()
            await self.writer.wait_closed()
        self.connected = False

    async def send(self, msg: Message) -> Message | None:
        if not self.connected or not self.writer or not self.reader:
            return None

        try:
            self.writer.write((msg.to_json() + "\n").encode())
            await self.writer.drain()

            line = await asyncio.wait_for(self.reader.readline(), timeout=10.0)
            if line:
                return Message.from_json(line.decode().strip())
        except (asyncio.TimeoutError, ConnectionError):
            self.connected = False
        return None


class Daemon:
    """Core orchestration daemon."""

    def __init__(self, data_dir: Path | None = None):
        self.data_dir = data_dir or Path.home() / ".local/share/vaelkor"
        self.session_dir = self.data_dir / "sessions"
        self.socket_dir = Path("/tmp/vaelkor")

        self.state: SessionState | None = None
        self.wrappers: dict[str, WrapperConnection] = {}
        self.running = False

    async def start(self, session_id: str | None = None):
        """Start the daemon with a new or existing session."""
        self.socket_dir.mkdir(parents=True, exist_ok=True)

        if session_id:
            self.state = self._load_session(session_id)
            if self.state and not self.state.clean_shutdown:
                await self._recover_session()

        if not self.state:
            self.state = SessionState(
                session_id=session_id or f"{datetime.now().strftime('%Y%m%d-%H%M%S')}-{uuid.uuid4().hex[:6]}"
            )

        self._save_last_session()
        self.running = True

    async def stop(self):
        """Clean shutdown."""
        self.running = False

        for conn in self.wrappers.values():
            await conn.disconnect()

        if self.state:
            self.state.clean_shutdown = True
            self._save_state()

    def _load_session(self, session_id: str) -> SessionState | None:
        path = self.session_dir / session_id / "state.json"
        return SessionState.load(path)

    def _save_state(self):
        if self.state:
            path = self.session_dir / self.state.session_id / "state.json"
            self.state.save(path)

    def _save_last_session(self):
        if self.state:
            path = self.data_dir / "last_session"
            path.write_text(self.state.session_id)

    async def _recover_session(self):
        """Attempt to recover an unclean shutdown."""
        if not self.state:
            return

        for task_id, task in self.state.tasks.items():
            if task.state in (TaskState.ASSIGNED, TaskState.ACCEPTED, TaskState.BLOCKED):
                task.state = TaskState.RECOVERING

        for agent_name in self.state.agents:
            await self.connect_wrapper(agent_name)
            if agent_name in self.wrappers and self.wrappers[agent_name].connected:
                status_msg = Message(
                    type=MessageType.WRAPPER_GET_STATUS,
                    from_agent="daemon",
                    to_agent="wrapper",
                )
                response = await self.wrappers[agent_name].send(status_msg)
                if response and response.type == MessageType.WRAPPER_STATUS:
                    self.state.agents[agent_name].status = response.body.get("status", "unknown")
                    self.state.agents[agent_name].pid = response.body.get("pid")

    async def connect_wrapper(self, agent_name: str) -> bool:
        """Connect to an agent's wrapper."""
        socket_path = self.socket_dir / f"{agent_name}.sock"
        conn = WrapperConnection(agent_name, socket_path)

        if await conn.connect():
            self.wrappers[agent_name] = conn
            return True
        return False

    async def register_agent(self, agent_name: str, command: list[str]):
        """Register an agent in the session state."""
        if not self.state:
            return

        self.state.agents[agent_name] = AgentState(name=agent_name)
        self._save_state()

    async def assign_task(
        self,
        to_agent: str,
        summary: str,
        scope: list[str] | None = None,
        constraints: list[str] | None = None,
        context: str | None = None,
        body: str | None = None,
    ) -> Task | None:
        """Create and assign a task to an agent."""
        if not self.state:
            return None

        task_id = f"task-{len(self.state.tasks) + 1:03d}"
        task = Task(
            task_id=task_id,
            summary=summary,
            assigned_to=to_agent,
            scope=scope or ["*"],
            constraints=constraints or [],
            context=context,
            body=body,
            assigned_at=datetime.now(UTC).isoformat(),
        )

        self.state.tasks[task_id] = task

        msg = make_task_assign(
            from_agent="orchestrator",
            to_agent=to_agent,
            task_id=task_id,
            summary=summary,
            scope=scope,
            constraints=constraints,
            context=context,
            body_text=body,
        )

        msg_state = MessageState(
            msg_id=msg.id,
            sent_at=datetime.now(UTC).isoformat(),
        )
        self.state.messages[msg.id] = msg_state

        if to_agent in self.wrappers:
            wrapper_msg = Message(
                type=MessageType.WRAPPER_SEND_TASK,
                from_agent="daemon",
                to_agent="wrapper",
                task_id=task_id,
                summary=summary,
                body={
                    "scope": scope or ["*"],
                    "constraints": constraints or [],
                    "context": context,
                    "body": body,
                },
            )
            response = await self.wrappers[to_agent].send(wrapper_msg)

            if response and response.type == MessageType.WRAPPER_ACK:
                msg_state.state = MessageDeliveryState.DELIVERED
                msg_state.delivered_at = datetime.now(UTC).isoformat()

        self._save_state()
        return task

    async def update_task_state(self, task_id: str, new_state: TaskState) -> bool:
        """Update task state if transition is valid."""
        if not self.state or task_id not in self.state.tasks:
            return False

        task = self.state.tasks[task_id]
        if not can_transition(task.state, new_state):
            return False

        task.state = new_state
        if new_state == TaskState.COMPLETED:
            task.completed_at = datetime.now(UTC).isoformat()

        self._save_state()
        return True

    async def get_agent_transcript(self, agent_name: str, lines: int = 100) -> list[str]:
        """Get recent output from an agent."""
        if agent_name not in self.wrappers:
            return []

        msg = Message(
            type=MessageType.WRAPPER_TAIL_TRANSCRIPT,
            from_agent="daemon",
            to_agent="wrapper",
            body={"lines": lines},
        )
        response = await self.wrappers[agent_name].send(msg)

        if response and response.type == MessageType.WRAPPER_TRANSCRIPT:
            return response.body.get("lines", [])
        return []

    async def send_user_input(self, to_agent: str, text: str):
        """Send direct user input to an agent."""
        if to_agent not in self.wrappers:
            return

        msg = Message(
            type=MessageType.WRAPPER_APPEND_INPUT,
            from_agent="daemon",
            to_agent="wrapper",
            body={"text": text},
        )
        await self.wrappers[to_agent].send(msg)

    def get_active_tasks(self) -> list[Task]:
        """Get all non-terminal tasks."""
        if not self.state:
            return []

        terminal_states = {TaskState.COMPLETED, TaskState.CANCELLED, TaskState.REJECTED, TaskState.STALE}
        return [t for t in self.state.tasks.values() if t.state not in terminal_states]

    def get_task(self, task_id: str) -> Task | None:
        """Get a task by ID."""
        if not self.state:
            return None
        return self.state.tasks.get(task_id)
