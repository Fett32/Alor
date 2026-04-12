"""
UI Controller - bridges daemon and Qt UI.

Runs daemon in a background thread, signals UI updates via Qt signals.
"""

import asyncio
import os
import subprocess
import sys
from pathlib import Path
from threading import Thread
from typing import Callable

from PySide6.QtCore import QObject, Signal

from ..config import load_config, SOCKET_DIR
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
        # Auto-resume last session if not specified
        if session_id is None:
            from ..config import DATA_DIR
            last_session_file = DATA_DIR / "last_session"
            if last_session_file.exists():
                session_id = last_session_file.read_text().strip()

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

        self.daemon = Daemon(
            heartbeat_interval=self.config.heartbeat_interval,
            task_assignment_timeout=self.config.task_assignment_timeout,
        )

        def on_task_complete(task_id: str, agent_name: str):
            # This runs in daemon thread, emit signal to UI thread
            self.task_updated.emit(task_id, "COMPLETED")
            self.agent_status_changed.emit(agent_name, "idle")

        async def run():
            await self.daemon.start(session_id)

            # Register task completion callback
            self.daemon._on_task_complete.append(on_task_complete)

            self.connected.emit()

            # Launch and connect to wrappers
            for name, agent_config in self.config.agents.items():
                socket_path = SOCKET_DIR / f"{name}.sock"
                pid_path = SOCKET_DIR / f"{name}.pid"

                # Launch wrapper if autolaunch
                if agent_config.autolaunch:
                    # Kill previous wrapper using PID file (not pattern-based)
                    if pid_path.exists():
                        try:
                            old_pid = int(pid_path.read_text().strip())
                            os.kill(old_pid, 15)  # SIGTERM
                            await asyncio.sleep(0.2)
                        except (ValueError, ProcessLookupError, PermissionError):
                            pass
                        pid_path.unlink(missing_ok=True)

                    if socket_path.exists():
                        socket_path.unlink()

                    # Launch fresh wrapper (will reattach to existing tmux session)
                    self._launch_wrapper(name)
                    # Wait for wrapper to start
                    for _ in range(20):  # 2 second timeout
                        await asyncio.sleep(0.1)
                        if socket_path.exists():
                            break

                # Connect if autoconnect
                if agent_config.autoconnect:
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

    def send_input(self, agent_name: str, text: str, mode: str = "override"):
        """Send input to an agent.

        Modes:
        - chat: Not logged, passes through
        - override: Logged, attached to current task
        """
        async def _send():
            await self.daemon.send_user_input(agent_name, text, mode)

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

    def _launch_wrapper(self, agent_name: str):
        """Launch a wrapper process for an agent (headless)."""
        log_path = SOCKET_DIR / f"{agent_name}.log"
        log_file = open(log_path, "w")
        env = dict(os.environ, TERM="xterm-256color")
        subprocess.Popen(
            [sys.executable, "-m", "vaelkor.wrapper.cli", agent_name],
            start_new_session=True,
            env=env,
            stdout=log_file,
            stderr=log_file,
        )
        # Don't close log_file - let wrapper write to it

    def launch_wrapper(self, agent_name: str) -> bool:
        """Launch wrapper and wait for it to be ready."""
        socket_path = SOCKET_DIR / f"{agent_name}.sock"
        if socket_path.exists():
            return True  # Already running

        self._launch_wrapper(agent_name)

        # Wait for socket (sync version for UI calls)
        import time
        for _ in range(20):
            time.sleep(0.1)
            if socket_path.exists():
                return True
        return False
