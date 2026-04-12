"""
Vaelkor main window - task-first UI.
"""

import asyncio
import os
import pty
import subprocess
from pathlib import Path

import pyte
from PySide6.QtCore import Qt, QSocketNotifier, QTimer
from PySide6.QtGui import QFont, QTextCursor
from PySide6.QtWidgets import (
    QMainWindow,
    QWidget,
    QVBoxLayout,
    QHBoxLayout,
    QSplitter,
    QListWidget,
    QListWidgetItem,
    QPlainTextEdit,
    QLineEdit,
    QLabel,
    QPushButton,
    QComboBox,
    QFrame,
    QGroupBox,
)

from ..daemon.state import TaskState


# Beskar-inspired colors
COLORS = {
    "bg_dark": "#1a1a1a",
    "bg_mid": "#2a2a2a",
    "bg_light": "#3a3a3a",
    "text": "#f8f8f2",
    "text_dim": "#888888",
    "accent": "#4a9eff",
    "success": "#50fa7b",
    "warning": "#f1fa8c",
    "error": "#ff5555",
    "border": "#3a3a3a",
}


TASK_STATE_COLORS = {
    TaskState.ASSIGNED: COLORS["warning"],
    TaskState.ACCEPTED: COLORS["accent"],
    TaskState.COMPLETED: COLORS["success"],
    TaskState.BLOCKED: COLORS["error"],
    TaskState.CANCELLED: COLORS["text_dim"],
    TaskState.REJECTED: COLORS["text_dim"],
    TaskState.TIMED_OUT: COLORS["error"],
    TaskState.INTERRUPTED: COLORS["warning"],
    TaskState.RECOVERING: COLORS["warning"],
    TaskState.STALE: COLORS["text_dim"],
}


class TaskListWidget(QGroupBox):
    """Task list panel."""

    def __init__(self):
        super().__init__("Tasks")
        self._setup_ui()

    def _setup_ui(self):
        layout = QVBoxLayout(self)
        layout.setContentsMargins(8, 16, 8, 8)

        self.task_list = QListWidget()
        self.task_list.setAlternatingRowColors(True)
        layout.addWidget(self.task_list)

    def add_task(self, task_id: str, summary: str, agent: str, state: TaskState):
        icon = "●" if state == TaskState.ACCEPTED else "○" if state == TaskState.ASSIGNED else "✓"
        text = f"{icon} {task_id} ({agent})\n   {summary}"
        item = QListWidgetItem(text)
        item.setData(Qt.ItemDataRole.UserRole, task_id)
        color = TASK_STATE_COLORS.get(state, COLORS["text"])
        item.setForeground(Qt.GlobalColor.white)
        self.task_list.addItem(item)

    def clear_tasks(self):
        self.task_list.clear()


class AgentStatusWidget(QGroupBox):
    """Agent status panel."""

    def __init__(self):
        super().__init__("Agents")
        self._setup_ui()
        self.agent_labels: dict[str, QLabel] = {}

    def _setup_ui(self):
        self.layout = QVBoxLayout(self)
        self.layout.setContentsMargins(8, 16, 8, 8)

    def set_agent_status(self, name: str, status: str):
        if name not in self.agent_labels:
            label = QLabel(f"[{name}: {status}]")
            self.agent_labels[name] = label
            self.layout.addWidget(label)
        else:
            self.agent_labels[name].setText(f"[{name}: {status}]")


class TerminalWidget(QWidget):
    """Embedded terminal for agent output."""

    def __init__(self):
        super().__init__()
        self.screen = pyte.Screen(120, 30)
        self.stream = pyte.Stream(self.screen)
        self.master_fd = None
        self.pid = None
        self.session_name = None

        self._setup_ui()

    def _setup_ui(self):
        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(4)

        header = QHBoxLayout()
        header.addWidget(QLabel("Terminal"))

        self.session_combo = QComboBox()
        self.session_combo.setMinimumWidth(150)
        header.addWidget(self.session_combo)

        self.attach_btn = QPushButton("Attach")
        self.attach_btn.clicked.connect(self._attach_session)
        header.addWidget(self.attach_btn)

        self.mode_combo = QComboBox()
        self.mode_combo.addItems(["override", "chat", "task"])
        header.addWidget(self.mode_combo)

        header.addStretch()
        layout.addLayout(header)

        self.display = QPlainTextEdit()
        self.display.setReadOnly(True)
        self.display.setFont(QFont("JetBrains Mono", 10))
        self.display.setStyleSheet(f"""
            QPlainTextEdit {{
                background-color: {COLORS["bg_dark"]};
                color: {COLORS["text"]};
                border: 1px solid {COLORS["border"]};
            }}
        """)
        layout.addWidget(self.display, 1)

        self.input_line = QLineEdit()
        self.input_line.setFont(QFont("JetBrains Mono", 10))
        self.input_line.setPlaceholderText("Direct input...")
        self.input_line.returnPressed.connect(self._send_input)
        layout.addWidget(self.input_line)

    def refresh_sessions(self):
        self.session_combo.clear()
        try:
            result = subprocess.run(
                ["tmux", "list-sessions", "-F", "#{session_name}"],
                capture_output=True,
                text=True,
            )
            if result.returncode == 0:
                sessions = [s for s in result.stdout.strip().split("\n") if s.startswith("vaelkor-")]
                self.session_combo.addItems(sessions)
        except FileNotFoundError:
            pass

    def _attach_session(self):
        session = self.session_combo.currentText()
        if not session:
            return

        if self.master_fd:
            self._detach()

        self.pid, self.master_fd = pty.fork()
        if self.pid == 0:
            os.execvp("tmux", ["tmux", "attach-session", "-t", session])
        else:
            self.notifier = QSocketNotifier(
                self.master_fd, QSocketNotifier.Type.Read, self
            )
            self.notifier.activated.connect(self._read_output)
            self.session_name = session

    def _detach(self):
        if hasattr(self, "notifier"):
            self.notifier.setEnabled(False)
        if self.master_fd:
            os.close(self.master_fd)
            self.master_fd = None
        if self.pid:
            try:
                os.kill(self.pid, 9)
            except ProcessLookupError:
                pass
            self.pid = None

    def _read_output(self):
        try:
            data = os.read(self.master_fd, 4096)
            if data:
                self.stream.feed(data.decode("utf-8", errors="replace"))
                self._render()
        except OSError:
            self.notifier.setEnabled(False)

    def _render(self):
        lines = []
        for y in range(self.screen.lines):
            line = "".join(
                self.screen.buffer[y][x].data or " "
                for x in range(self.screen.columns)
            ).rstrip()
            lines.append(line)

        while lines and not lines[-1]:
            lines.pop()

        self.display.setPlainText("\n".join(lines))
        cursor = self.display.textCursor()
        cursor.movePosition(QTextCursor.MoveOperation.End)
        self.display.setTextCursor(cursor)

    def _send_input(self):
        text = self.input_line.text()
        if self.master_fd and text:
            os.write(self.master_fd, (text + "\n").encode())
            self.input_line.clear()

    def closeEvent(self, event):
        self._detach()
        super().closeEvent(event)


class MainWindow(QMainWindow):
    """Vaelkor main application window."""

    def __init__(self):
        super().__init__()
        self.setWindowTitle("Vaelkor")
        self.setGeometry(100, 100, 1200, 800)
        self._apply_style()
        self._setup_ui()

    def _apply_style(self):
        self.setStyleSheet(f"""
            QMainWindow, QWidget {{
                background-color: {COLORS["bg_mid"]};
                color: {COLORS["text"]};
            }}
            QGroupBox {{
                border: 1px solid {COLORS["border"]};
                border-radius: 4px;
                margin-top: 12px;
                padding-top: 8px;
            }}
            QGroupBox::title {{
                subcontrol-origin: margin;
                left: 8px;
                padding: 0 4px;
            }}
            QListWidget {{
                background-color: {COLORS["bg_dark"]};
                border: none;
                alternate-background-color: {COLORS["bg_light"]};
            }}
            QListWidget::item {{
                padding: 8px;
            }}
            QPushButton {{
                background-color: {COLORS["bg_light"]};
                border: 1px solid {COLORS["border"]};
                border-radius: 4px;
                padding: 6px 12px;
            }}
            QPushButton:hover {{
                background-color: {COLORS["accent"]};
            }}
            QComboBox {{
                background-color: {COLORS["bg_light"]};
                border: 1px solid {COLORS["border"]};
                border-radius: 4px;
                padding: 4px 8px;
            }}
            QLineEdit {{
                background-color: {COLORS["bg_dark"]};
                border: 1px solid {COLORS["border"]};
                border-radius: 4px;
                padding: 6px;
            }}
        """)

    def _setup_ui(self):
        central = QWidget()
        self.setCentralWidget(central)

        main_layout = QHBoxLayout(central)
        main_layout.setContentsMargins(8, 8, 8, 8)
        main_layout.setSpacing(8)

        splitter = QSplitter(Qt.Orientation.Horizontal)

        left_panel = QWidget()
        left_layout = QVBoxLayout(left_panel)
        left_layout.setContentsMargins(0, 0, 0, 0)

        self.task_list = TaskListWidget()
        left_layout.addWidget(self.task_list, 2)

        self.agent_status = AgentStatusWidget()
        left_layout.addWidget(self.agent_status, 1)

        splitter.addWidget(left_panel)

        self.terminal = TerminalWidget()
        splitter.addWidget(self.terminal)

        splitter.setSizes([350, 850])
        main_layout.addWidget(splitter)

        self._add_demo_data()

    def _add_demo_data(self):
        self.task_list.add_task("task-001", "Implement login flow", "claude", TaskState.COMPLETED)
        self.task_list.add_task("task-002", "Review auth for race conditions", "codex", TaskState.ACCEPTED)
        self.task_list.add_task("task-003", "Add rate limiting", "claude", TaskState.ASSIGNED)

        self.agent_status.set_agent_status("claude", "running")
        self.agent_status.set_agent_status("codex", "idle")

        self.terminal.refresh_sessions()
