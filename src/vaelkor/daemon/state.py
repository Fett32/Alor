"""
Task and session state management.

The daemon owns all task state. Wrappers only report process state.
"""

from dataclasses import dataclass, field
from datetime import datetime
from enum import Enum
from pathlib import Path
import json


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


class MessageDeliveryState(Enum):
    SENT = "sent"
    DELIVERED = "delivered"
    ACKED = "acked"
    PERSISTED = "persisted"


@dataclass
class Task:
    task_id: str
    summary: str
    assigned_to: str
    state: TaskState = TaskState.ASSIGNED
    created_at: str = field(default_factory=lambda: datetime.utcnow().isoformat() + "Z")
    assigned_at: str | None = None
    completed_at: str | None = None
    scope: list[str] = field(default_factory=lambda: ["*"])
    constraints: list[str] = field(default_factory=list)
    context: str | None = None
    body: str | None = None
    result: dict | None = None

    def to_dict(self) -> dict:
        return {
            "task_id": self.task_id,
            "summary": self.summary,
            "assigned_to": self.assigned_to,
            "state": self.state.value,
            "created_at": self.created_at,
            "assigned_at": self.assigned_at,
            "completed_at": self.completed_at,
            "scope": self.scope,
            "constraints": self.constraints,
            "context": self.context,
            "body": self.body,
            "result": self.result,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "Task":
        d = d.copy()
        d["state"] = TaskState(d["state"])
        return cls(**d)


@dataclass
class AgentState:
    name: str
    status: str = "stopped"
    pid: int | None = None
    wrapper_pid: int | None = None
    last_msg_id: str | None = None
    current_task_id: str | None = None

    def to_dict(self) -> dict:
        return {
            "name": self.name,
            "status": self.status,
            "pid": self.pid,
            "wrapper_pid": self.wrapper_pid,
            "last_msg_id": self.last_msg_id,
            "current_task_id": self.current_task_id,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "AgentState":
        return cls(**d)


@dataclass
class MessageState:
    msg_id: str
    state: MessageDeliveryState = MessageDeliveryState.SENT
    sent_at: str | None = None
    delivered_at: str | None = None
    acked_at: str | None = None
    persisted_at: str | None = None

    def to_dict(self) -> dict:
        return {
            "msg_id": self.msg_id,
            "state": self.state.value,
            "sent_at": self.sent_at,
            "delivered_at": self.delivered_at,
            "acked_at": self.acked_at,
            "persisted_at": self.persisted_at,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "MessageState":
        d = d.copy()
        d["state"] = MessageDeliveryState(d["state"])
        return cls(**d)


@dataclass
class SessionState:
    session_id: str
    started_at: str = field(default_factory=lambda: datetime.utcnow().isoformat() + "Z")
    clean_shutdown: bool = False
    agents: dict[str, AgentState] = field(default_factory=dict)
    tasks: dict[str, Task] = field(default_factory=dict)
    messages: dict[str, MessageState] = field(default_factory=dict)

    def to_dict(self) -> dict:
        return {
            "session_id": self.session_id,
            "started_at": self.started_at,
            "clean_shutdown": self.clean_shutdown,
            "agents": {k: v.to_dict() for k, v in self.agents.items()},
            "tasks": {k: v.to_dict() for k, v in self.tasks.items()},
            "messages": {k: v.to_dict() for k, v in self.messages.items()},
        }

    @classmethod
    def from_dict(cls, d: dict) -> "SessionState":
        state = cls(
            session_id=d["session_id"],
            started_at=d["started_at"],
            clean_shutdown=d["clean_shutdown"],
        )
        state.agents = {k: AgentState.from_dict(v) for k, v in d.get("agents", {}).items()}
        state.tasks = {k: Task.from_dict(v) for k, v in d.get("tasks", {}).items()}
        state.messages = {k: MessageState.from_dict(v) for k, v in d.get("messages", {}).items()}
        return state

    def save(self, path: Path):
        path.parent.mkdir(parents=True, exist_ok=True)
        with open(path, "w") as f:
            json.dump(self.to_dict(), f, indent=2)

    @classmethod
    def load(cls, path: Path) -> "SessionState | None":
        if not path.exists():
            return None
        with open(path) as f:
            return cls.from_dict(json.load(f))


# Valid state transitions
TASK_TRANSITIONS = {
    TaskState.ASSIGNED: {TaskState.ACCEPTED, TaskState.REJECTED, TaskState.TIMED_OUT, TaskState.RECOVERING},
    TaskState.ACCEPTED: {TaskState.COMPLETED, TaskState.BLOCKED, TaskState.CANCELLED, TaskState.INTERRUPTED, TaskState.RECOVERING},
    TaskState.BLOCKED: {TaskState.ACCEPTED, TaskState.CANCELLED, TaskState.RECOVERING},
    TaskState.INTERRUPTED: {TaskState.ACCEPTED, TaskState.STALE},
    TaskState.RECOVERING: {TaskState.ACCEPTED, TaskState.STALE},
}


def can_transition(from_state: TaskState, to_state: TaskState) -> bool:
    return to_state in TASK_TRANSITIONS.get(from_state, set())
