"""
Vaelkor protocol message definitions.

All IPC uses newline-delimited JSON over unix sockets.
"""

from dataclasses import dataclass, field, asdict
from datetime import datetime, UTC
from enum import Enum
from typing import Any
import json
import uuid


class MessageType(Enum):
    # Task lifecycle
    TASK_ASSIGN = "task.assign"
    TASK_ACCEPT = "task.accept"
    TASK_REJECT = "task.reject"
    TASK_BLOCKED = "task.blocked"
    TASK_RESULT = "task.result"
    TASK_CANCEL = "task.cancel"

    # Agent lifecycle
    AGENT_STATUS = "agent.status"
    AGENT_HEARTBEAT = "agent.heartbeat"

    # User interaction
    USER_INPUT = "user.input"
    USER_ESCALATION = "user.escalation"

    # Wrapper control (daemon -> wrapper)
    WRAPPER_START = "wrapper.start"
    WRAPPER_STOP = "wrapper.stop"
    WRAPPER_SEND_TASK = "wrapper.send_task"
    WRAPPER_INTERRUPT = "wrapper.interrupt"
    WRAPPER_GET_STATUS = "wrapper.get_status"
    WRAPPER_APPEND_INPUT = "wrapper.append_input"
    WRAPPER_TAIL_TRANSCRIPT = "wrapper.tail_transcript"

    # Wrapper responses (wrapper -> daemon)
    WRAPPER_ACK = "wrapper.ack"
    WRAPPER_STATUS = "wrapper.status"
    WRAPPER_TRANSCRIPT = "wrapper.transcript"
    WRAPPER_ERROR = "wrapper.error"


class TaskState(Enum):
    ASSIGNED = "ASSIGNED"
    ACCEPTED = "ACCEPTED"
    COMPLETED = "COMPLETED"
    BLOCKED = "BLOCKED"
    CANCELLED = "CANCELLED"
    REJECTED = "REJECTED"
    TIMED_OUT = "TIMED_OUT"
    INTERRUPTED = "INTERRUPTED"
    RECOVERING = "RECOVERING"
    STALE = "STALE"


class AgentStatus(Enum):
    STOPPED = "stopped"
    STARTING = "starting"
    RUNNING = "running"
    IDLE = "idle"
    DEAD = "dead"


class InputMode(Enum):
    CHAT = "chat"
    TASK = "task"
    OVERRIDE = "override"


@dataclass
class Message:
    type: MessageType
    from_agent: str
    to_agent: str
    id: str = field(default_factory=lambda: f"msg-{uuid.uuid4().hex[:8]}")
    task_id: str | None = None
    reply_to: str | None = None
    timestamp: str = field(default_factory=lambda: datetime.now(UTC).isoformat())
    summary: str | None = None
    body: dict[str, Any] = field(default_factory=dict)

    def to_json(self) -> str:
        d = asdict(self)
        d["type"] = self.type.value
        return json.dumps(d)

    @classmethod
    def from_json(cls, data: str) -> "Message":
        d = json.loads(data)
        d["type"] = MessageType(d["type"])
        return cls(**d)


def make_task_assign(
    from_agent: str,
    to_agent: str,
    task_id: str,
    summary: str,
    scope: list[str] | None = None,
    constraints: list[str] | None = None,
    context: str | None = None,
    body_text: str | None = None,
) -> Message:
    return Message(
        type=MessageType.TASK_ASSIGN,
        from_agent=from_agent,
        to_agent=to_agent,
        task_id=task_id,
        summary=summary,
        body={
            "scope": scope or ["*"],
            "constraints": constraints or [],
            "context": context,
            "body": body_text,
        },
    )


def make_task_result(
    from_agent: str,
    to_agent: str,
    task_id: str,
    summary: str,
    result: dict[str, Any],
) -> Message:
    return Message(
        type=MessageType.TASK_RESULT,
        from_agent=from_agent,
        to_agent=to_agent,
        task_id=task_id,
        summary=summary,
        body={"result": result},
    )


def make_wrapper_start(session_name: str, command: list[str]) -> Message:
    return Message(
        type=MessageType.WRAPPER_START,
        from_agent="daemon",
        to_agent="wrapper",
        body={"session_name": session_name, "command": command},
    )


def make_wrapper_status(
    agent_name: str, status: AgentStatus, pid: int | None = None
) -> Message:
    return Message(
        type=MessageType.WRAPPER_STATUS,
        from_agent="wrapper",
        to_agent="daemon",
        body={"agent": agent_name, "status": status.value, "pid": pid},
    )
