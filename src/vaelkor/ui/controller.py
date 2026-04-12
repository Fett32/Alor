"""
UI Controller - bridges daemon and Qt UI.

Runs daemon in a background thread, signals UI updates via Qt signals.
"""

import asyncio
from threading import Thread
from typing import Callable

from PySide6.QtCore import QObject, Signal

from ..config import load_config
from ..daemon.core import Daemon
from ..daemon.state import Task, TaskState


class DaemonController(QObject):
    """Controls daemon from Qt UI thread."""

    # Signals for UI updates
    task_added = Signal(str, str, str, str)  # task_id, summary, agent, state
    task_updated = Signal(str, str)  # task_id, new_state
    agent_status_changed = Signal(str, str)  # agent_name, status
    transcript_received = Signal(str, list)  # agent_name, lines
    connected = Signal()
    disconnected = Signal()

    def __init__(self):
        super().__init__()
        self.daemon: Daemon | None = None
        self.config = load_config()
        self._loop: asyncio.AbstractEventLoop | None = None
        self._thread: Thread | None = None
        self._running = False

    def start(self, session_id: str | None = None):
        """Start daemon in background thread."""
        self._running = True
        self._thread = Thread(target=self._run_daemon_thread, args=(session_id,), daemon=True)
        self._thread.start()

    def stop(self):
        """Stop daemon."""
        self._running = False
        if self._loop:
            self._loop.call_soon_threadsafe(self._loop.stop)

    def _run_daemon_thread(self, session_id: str | None):
        """Run daemon event loop in thread."""
        self._loop = asyncio.new_event_loop()
        asyncio.set_event_loop(self._loop)

        self.daemon = Daemon()

        async def run():
            await self.daemon.start(session_id)
            self.connected.emit()

            # Connect to autostart wrappers
            for name, agent_config in self.config.agents.items():
                if agent_config.autostart:
                    success = await self.daemon.connect_wrapper(name)
                    status = "running" if success else "disconnected"
                    self.agent_status_changed.emit(name, status)

            # Keep running until stopped
            while self._running:
                await asyncio.sleep(0.1)

            await self.daemon.stop()
            self.disconnected.emit()

        try:
            self._loop.run_until_complete(run())
        finally:
            self._loop.close()

    def _run_async(self, coro):
        """Run coroutine on daemon's event loop."""
        if self._loop and self._running:
            future = asyncio.run_coroutine_threadsafe(coro, self._loop)
            return future.result(timeout=10)
        return None

    def assign_task(
        self,
        to_agent: str,
        summary: str,
        scope: list[str] | None = None,
        constraints: list[str] | None = None,
        context: str | None = None,
        body: str | None = None,
    ) -> Task | None:
        """Assign a task to an agent."""
        async def _assign():
            task = await self.daemon.assign_task(
                to_agent, summary, scope, constraints, context, body
            )
            if task:
                self.task_added.emit(
                    task.task_id, task.summary, task.assigned_to, task.state.value
                )
            return task

        return self._run_async(_assign())

    def get_transcript(self, agent_name: str, lines: int = 100):
        """Get agent transcript."""
        async def _get():
            transcript = await self.daemon.get_agent_transcript(agent_name, lines)
            self.transcript_received.emit(agent_name, transcript)
            return transcript

        return self._run_async(_get())

    def send_input(self, agent_name: str, text: str):
        """Send input to an agent."""
        async def _send():
            await self.daemon.send_user_input(agent_name, text)

        return self._run_async(_send())

    def get_active_tasks(self) -> list[Task]:
        """Get active tasks."""
        if self.daemon:
            return self.daemon.get_active_tasks()
        return []

    def connect_wrapper(self, agent_name: str) -> bool:
        """Connect to an agent wrapper."""
        async def _connect():
            success = await self.daemon.connect_wrapper(agent_name)
            status = "running" if success else "disconnected"
            self.agent_status_changed.emit(agent_name, status)
            return success

        return self._run_async(_connect())
