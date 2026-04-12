"""
Agent wrapper - manages a CLI agent in a tmux session.

Each agent gets one wrapper process. The daemon talks to wrappers
over unix sockets using newline-delimited JSON.
"""

import asyncio
import json
import os
import re
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

from ..protocol.messages import (
    AgentStatus,
    Message,
    MessageType,
    make_wrapper_status,
)


CONSTRAINT_TEMPLATES = {
    "review_only": (
        "IMPORTANT: This is a review task. "
        "Do NOT modify any files. Only analyze and report findings."
    ),
    "no_file_edits": (
        "CONSTRAINT: You may not edit files directly. "
        "Output proposed changes as diffs or descriptions only."
    ),
    "scope_limited": (
        "SCOPE RESTRICTION: Only examine files within the specified scope. "
        "Do not access files outside this boundary."
    ),
}


# Patterns that indicate agent is idle/ready for input
# These are checked against the last few lines of output
IDLE_PATTERNS = {
    "claude": [
        r"^claude>",           # Claude CLI prompt
        r"^>\s*$",             # Generic prompt
    ],
    "codex": [
        r"^codex>",            # Codex prompt (if it has one)
        r"^>\s*$",
    ],
    "default": [
        r"^\$\s*$",            # Shell prompt
        r"^>\s*$",
    ],
}


@dataclass
class WrapperConfig:
    agent_name: str
    socket_path: Path
    command: list[str]
    session_prefix: str = "vaelkor"
    transcript_lines: int = 1000
    idle_check_interval: float = 2.0  # Seconds between idle checks
    idle_pattern_lines: int = 5       # Lines to check for idle pattern


class AgentWrapper:
    """Manages a single CLI agent in a tmux session."""

    def __init__(self, config: WrapperConfig):
        self.config = config
        self.session_name = f"{config.session_prefix}-{config.agent_name}"
        self.status = AgentStatus.STOPPED
        self.pid: int | None = None
        self.server: asyncio.Server | None = None
        self.transcript: list[str] = []
        self.current_task_id: str | None = None
        self._monitor_task: asyncio.Task | None = None
        self._last_output_hash: int = 0
        self._idle_since: float | None = None
        self._on_task_complete: Callable[[str], None] | None = None
        self._completed_tasks: list[str] = []  # Queue of completed task IDs

        # Compile idle patterns for this agent
        patterns = IDLE_PATTERNS.get(config.agent_name, IDLE_PATTERNS["default"])
        self._idle_patterns = [re.compile(p) for p in patterns]

    async def start(self):
        """Start the wrapper server and agent process."""
        self.config.socket_path.parent.mkdir(parents=True, exist_ok=True)

        if self.config.socket_path.exists():
            self.config.socket_path.unlink()

        self.server = await asyncio.start_unix_server(
            self._handle_connection,
            path=str(self.config.socket_path),
        )

        await self._start_agent()

        # Start output monitoring
        self._monitor_task = asyncio.create_task(self._monitor_output())

    async def stop(self):
        """Stop the agent and wrapper server."""
        # Stop monitoring
        if self._monitor_task:
            self._monitor_task.cancel()
            try:
                await self._monitor_task
            except asyncio.CancelledError:
                pass

        await self._stop_agent()
        if self.server:
            self.server.close()
            await self.server.wait_closed()
        if self.config.socket_path.exists():
            self.config.socket_path.unlink()

    async def _monitor_output(self):
        """Background task to monitor agent output for idle state."""
        import time

        while True:
            try:
                await asyncio.sleep(self.config.idle_check_interval)

                if not self.current_task_id:
                    # No active task, skip monitoring
                    self._idle_since = None
                    continue

                # Capture recent output
                lines = self._capture_pane(self.config.idle_pattern_lines)
                if not lines:
                    continue

                # Check if output changed
                output_hash = hash(tuple(lines))
                if output_hash == self._last_output_hash:
                    # Output unchanged, check if idle pattern matches
                    if self._check_idle_pattern(lines):
                        if self._idle_since is None:
                            self._idle_since = time.time()
                        elif time.time() - self._idle_since > 3.0:
                            # Idle for 3+ seconds with matching pattern = task complete
                            await self._signal_task_complete()
                else:
                    # Output changed, reset idle timer
                    self._last_output_hash = output_hash
                    self._idle_since = None

            except asyncio.CancelledError:
                break
            except Exception:
                # Don't crash monitor on errors
                pass

    def _check_idle_pattern(self, lines: list[str]) -> bool:
        """Check if recent output matches idle pattern."""
        for line in reversed(lines):
            line = line.strip()
            if not line:
                continue
            for pattern in self._idle_patterns:
                if pattern.match(line):
                    return True
            # First non-empty line doesn't match
            return False
        return False

    async def _signal_task_complete(self):
        """Signal that the current task appears complete."""
        if not self.current_task_id:
            return

        task_id = self.current_task_id
        self.current_task_id = None
        self._idle_since = None
        self.status = AgentStatus.IDLE

        # Queue completion for daemon to pick up
        self._completed_tasks.append(task_id)

        # Notify via callback if set
        if self._on_task_complete:
            self._on_task_complete(task_id)

    async def _start_agent(self):
        """Launch the agent CLI in a tmux session."""
        self.status = AgentStatus.STARTING

        if self._session_exists():
            subprocess.run(["tmux", "kill-session", "-t", self.session_name])

        cmd = ["tmux", "new-session", "-d", "-s", self.session_name]
        cmd.extend(self.config.command)

        result = subprocess.run(cmd, capture_output=True)
        if result.returncode == 0:
            self.status = AgentStatus.RUNNING
            self.pid = self._get_session_pid()
        else:
            self.status = AgentStatus.DEAD

    async def _stop_agent(self):
        """Kill the tmux session."""
        if self._session_exists():
            subprocess.run(["tmux", "kill-session", "-t", self.session_name])
        self.status = AgentStatus.STOPPED
        self.pid = None

    def _session_exists(self) -> bool:
        result = subprocess.run(
            ["tmux", "has-session", "-t", self.session_name],
            capture_output=True,
        )
        return result.returncode == 0

    def _get_session_pid(self) -> int | None:
        result = subprocess.run(
            ["tmux", "list-panes", "-t", self.session_name, "-F", "#{pane_pid}"],
            capture_output=True,
            text=True,
        )
        if result.returncode == 0 and result.stdout.strip():
            return int(result.stdout.strip().split("\n")[0])
        return None

    async def _handle_connection(
        self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ):
        """Handle incoming connection from daemon."""
        try:
            while True:
                line = await reader.readline()
                if not line:
                    break

                try:
                    msg = Message.from_json(line.decode().strip())
                    response = await self._handle_message(msg)
                    if response:
                        writer.write((response.to_json() + "\n").encode())
                        await writer.drain()
                except json.JSONDecodeError:
                    error_msg = Message(
                        type=MessageType.WRAPPER_ERROR,
                        from_agent="wrapper",
                        to_agent="daemon",
                        body={"error": "Invalid JSON"},
                    )
                    writer.write((error_msg.to_json() + "\n").encode())
                    await writer.drain()
        finally:
            writer.close()
            await writer.wait_closed()

    async def _handle_message(self, msg: Message) -> Message | None:
        """Process a message from the daemon."""
        match msg.type:
            case MessageType.WRAPPER_START:
                await self._start_agent()
                return make_wrapper_status(
                    self.config.agent_name, self.status, self.pid
                )

            case MessageType.WRAPPER_STOP:
                await self._stop_agent()
                return make_wrapper_status(
                    self.config.agent_name, self.status, self.pid
                )

            case MessageType.WRAPPER_SEND_TASK:
                prompt = self._build_task_prompt(msg)
                self._send_to_agent(prompt)
                self.current_task_id = msg.task_id
                return Message(
                    type=MessageType.WRAPPER_ACK,
                    from_agent="wrapper",
                    to_agent="daemon",
                    reply_to=msg.id,
                    body={"task_id": msg.task_id},
                )

            case MessageType.WRAPPER_INTERRUPT:
                self._send_keys("C-c")
                return make_wrapper_status(
                    self.config.agent_name, self.status, self.pid
                )

            case MessageType.WRAPPER_GET_STATUS:
                if self._session_exists():
                    if not self.current_task_id:
                        self.status = AgentStatus.IDLE
                    else:
                        self.status = AgentStatus.RUNNING
                    self.pid = self._get_session_pid()
                else:
                    self.status = AgentStatus.DEAD
                    self.pid = None

                # Drain completed tasks queue
                completed = self._completed_tasks.copy()
                self._completed_tasks.clear()

                return make_wrapper_status(
                    self.config.agent_name,
                    self.status,
                    self.pid,
                    completed_tasks=completed,
                    current_task=self.current_task_id,
                )

            case MessageType.WRAPPER_APPEND_INPUT:
                text = msg.body.get("text", "")
                self._send_to_agent(text)
                return Message(
                    type=MessageType.WRAPPER_ACK,
                    from_agent="wrapper",
                    to_agent="daemon",
                    reply_to=msg.id,
                )

            case MessageType.WRAPPER_TAIL_TRANSCRIPT:
                n = msg.body.get("lines", 100)
                transcript = self._capture_pane(n)
                return Message(
                    type=MessageType.WRAPPER_TRANSCRIPT,
                    from_agent="wrapper",
                    to_agent="daemon",
                    reply_to=msg.id,
                    body={"lines": transcript},
                )

            case _:
                return Message(
                    type=MessageType.WRAPPER_ERROR,
                    from_agent="wrapper",
                    to_agent="daemon",
                    reply_to=msg.id,
                    body={"error": f"Unknown message type: {msg.type}"},
                )

    def _build_task_prompt(self, msg: Message) -> str:
        """Build the prompt to inject into the agent."""
        body = msg.body or {}
        constraints = body.get("constraints", [])
        scope = body.get("scope", ["*"])
        context = body.get("context", "None provided")
        task_body = body.get("body", "")

        constraint_text = "\n".join(
            CONSTRAINT_TEMPLATES.get(c, f"CONSTRAINT: {c}")
            for c in constraints
        )

        return f"""
=== TASK FROM ORCHESTRATOR ===
{constraint_text}

TASK ID: {msg.task_id}
SUMMARY: {msg.summary}
SCOPE: {", ".join(scope)}
CONTEXT: {context}

{task_body}
=== END TASK ===
"""

    def _send_to_agent(self, text: str):
        """Send text to the tmux session."""
        subprocess.run(
            ["tmux", "send-keys", "-t", self.session_name, "-l", text],
        )
        subprocess.run(
            ["tmux", "send-keys", "-t", self.session_name, "Enter"],
        )

    def _send_keys(self, keys: str):
        """Send tmux key sequence."""
        subprocess.run(
            ["tmux", "send-keys", "-t", self.session_name, keys],
        )

    def _capture_pane(self, lines: int = 100) -> list[str]:
        """Capture recent output from the tmux pane."""
        result = subprocess.run(
            [
                "tmux", "capture-pane", "-t", self.session_name,
                "-p", "-S", f"-{lines}",
            ],
            capture_output=True,
            text=True,
        )
        if result.returncode == 0:
            return result.stdout.strip().split("\n")
        return []


async def run_wrapper(config: WrapperConfig):
    """Run a wrapper as a standalone process."""
    wrapper = AgentWrapper(config)
    await wrapper.start()

    try:
        while True:
            await asyncio.sleep(1)
    except asyncio.CancelledError:
        pass
    finally:
        await wrapper.stop()
