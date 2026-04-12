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

    def __init__(
        self,
        data_dir: Path | None = None,
        heartbeat_interval: float = 5.0,
        task_assignment_timeout: float = 30.0,
    ):
        self.data_dir = data_dir or Path.home() / ".local/share/vaelkor"
        self.session_dir = self.data_dir / "sessions"
        self.socket_dir = Path("/tmp/vaelkor")
        self.heartbeat_interval = heartbeat_interval
        self.task_assignment_timeout = task_assignment_timeout

        self.state: SessionState | None = None
        self.wrappers: dict[str, WrapperConnection] = {}
        self.running = False
        self._heartbeat_task: asyncio.Task | None = None
        self._on_task_complete: list[callable] = []  # Callbacks for task completion

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

        # Start heartbeat polling
        self._heartbeat_task = asyncio.create_task(self._heartbeat_loop())

    async def stop(self):
        """Clean shutdown."""
        self.running = False

        # Stop heartbeat
        if self._heartbeat_task:
            self._heartbeat_task.cancel()
            try:
                await self._heartbeat_task
            except asyncio.CancelledError:
                pass

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

        # Mark in-flight tasks as RECOVERING
        recovering_tasks: dict[str, str] = {}  # task_id -> assigned_to
        for task_id, task in self.state.tasks.items():
            if task.state in (TaskState.ASSIGNED, TaskState.ACCEPTED, TaskState.BLOCKED):
                task.state = TaskState.RECOVERING
                recovering_tasks[task_id] = task.assigned_to

        # Reconnect to wrappers and resolve task states
        for agent_name in self.state.agents:
            connected = await self.connect_wrapper(agent_name)

            # Get tasks assigned to this agent
            agent_tasks = [tid for tid, agent in recovering_tasks.items() if agent == agent_name]

            if connected and agent_name in self.wrappers:
                status_msg = Message(
                    type=MessageType.WRAPPER_GET_STATUS,
                    from_agent="daemon",
                    to_agent="wrapper",
                )
                response = await self.wrappers[agent_name].send(status_msg)

                if response and response.type == MessageType.WRAPPER_STATUS:
                    self.state.agents[agent_name].status = response.body.get("status", "unknown")
                    self.state.agents[agent_name].pid = response.body.get("pid")
                    current_task = response.body.get("current_task")
                    completed = response.body.get("completed_tasks", [])

                    # Resolve recovering tasks for this agent
                    for task_id in agent_tasks:
                        task = self.state.tasks[task_id]
                        if task_id in completed:
                            # Task completed during crash
                            task.state = TaskState.COMPLETED
                            task.completed_at = datetime.now(UTC).isoformat()
                        elif task_id == current_task:
                            # Task still active
                            task.state = TaskState.ACCEPTED
                        else:
                            # Task state unknown - mark stale
                            task.state = TaskState.STALE
            else:
                # Wrapper not reachable - mark all its tasks stale
                for task_id in agent_tasks:
                    self.state.tasks[task_id].state = TaskState.STALE

        self._save_state()

    async def _heartbeat_loop(self):
        """Periodically poll wrappers for status and handle task completions."""
        while self.running:
            try:
                await asyncio.sleep(self.heartbeat_interval)

                for agent_name, conn in list(self.wrappers.items()):
                    if not conn.connected:
                        continue

                    status_msg = Message(
                        type=MessageType.WRAPPER_GET_STATUS,
                        from_agent="daemon",
                        to_agent="wrapper",
                    )
                    response = await conn.send(status_msg)

                    if response and response.type == MessageType.WRAPPER_STATUS:
                        # Update agent state
                        if self.state and agent_name in self.state.agents:
                            self.state.agents[agent_name].status = response.body.get("status", "unknown")
                            self.state.agents[agent_name].pid = response.body.get("pid")

                        # Handle completed tasks
                        completed = response.body.get("completed_tasks", [])
                        for task_id in completed:
                            await self._handle_task_completion(agent_name, task_id)

                self._save_state()

            except asyncio.CancelledError:
                break
            except Exception:
                # Don't crash heartbeat on errors
                pass

    async def _handle_task_completion(self, agent_name: str, task_id: str):
        """Handle a task completion from a wrapper."""
        if not self.state or task_id not in self.state.tasks:
            return

        task = self.state.tasks[task_id]
        if task.state in (TaskState.ASSIGNED, TaskState.ACCEPTED):
            task.state = TaskState.COMPLETED
            task.completed_at = datetime.now(UTC).isoformat()

            # Notify callbacks
            for callback in self._on_task_complete:
                try:
                    callback(task_id, agent_name)
                except Exception:
                    pass

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

        # Check if wrapper is connected
        if to_agent not in self.wrappers or not self.wrappers[to_agent].connected:
            # No wrapper connected - task stays ASSIGNED, will timeout
            # Schedule timeout check
            asyncio.create_task(self._check_task_timeout(task_id, self.task_assignment_timeout))
            self._save_state()
            return task

        # Send task to wrapper
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
            # Wrapper accepted - move to ACCEPTED state
            msg_state.state = MessageDeliveryState.DELIVERED
            msg_state.delivered_at = datetime.now(UTC).isoformat()
            task.state = TaskState.ACCEPTED

            # Update agent state
            if to_agent in self.state.agents:
                self.state.agents[to_agent].current_task_id = task_id
        else:
            # Wrapper didn't respond properly - schedule timeout
            asyncio.create_task(self._check_task_timeout(task_id, self.task_assignment_timeout))

        self._save_state()
        return task

    async def _check_task_timeout(self, task_id: str, timeout: float):
        """Check if task is still ASSIGNED after timeout and mark TIMED_OUT."""
        await asyncio.sleep(timeout)

        if not self.state or task_id not in self.state.tasks:
            return

        task = self.state.tasks[task_id]
        if task.state == TaskState.ASSIGNED:
            task.state = TaskState.TIMED_OUT
            self._save_state()

            # Notify callbacks
            for callback in self._on_task_complete:
                try:
                    callback(task_id, task.assigned_to)
                except Exception:
                    pass

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

    async def send_user_input(self, to_agent: str, text: str, mode: str = "override"):
        """Send direct user input to an agent.

        Modes:
        - chat: Not logged, passes through to agent
        - override: Logged, attached to current task context
        - task: Should use assign_task() instead
        """
        if to_agent not in self.wrappers:
            return

        msg = Message(
            type=MessageType.WRAPPER_APPEND_INPUT,
            from_agent="daemon",
            to_agent="wrapper",
            body={"text": text, "mode": mode},
        )
        await self.wrappers[to_agent].send(msg)

        # Log override inputs to session (chat mode is intentionally not logged)
        if mode == "override" and self.state:
            # Find current task for this agent
            current_task = None
            for task in self.state.tasks.values():
                if task.assigned_to == to_agent and task.state == TaskState.ACCEPTED:
                    current_task = task
                    break

            # Log the input
            log_entry = {
                "timestamp": datetime.now(UTC).isoformat(),
                "agent": to_agent,
                "mode": mode,
                "text": text,
                "task_id": current_task.task_id if current_task else None,
            }
            # Append to session log (simple approach - could be more sophisticated)
            log_path = self.session_dir / self.state.session_id / "user_inputs.log"
            log_path.parent.mkdir(parents=True, exist_ok=True)
            with open(log_path, "a") as f:
                f.write(json.dumps(log_entry) + "\n")

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
