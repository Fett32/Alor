"""
Vaelkor main window - task-first UI.
"""

import os
import pty
import subprocess

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
    QGroupBox,
    QDialog,
    QDialogButtonBox,
    QFormLayout,
    QTextEdit,
    QCheckBox,
)

from ..daemon.state import TaskState
from .controller import DaemonController


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


class NewTaskDialog(QDialog):
    """Dialog for creating a new task."""

    def __init__(self, agents: list[str], parent=None):
        super().__init__(parent)
        self.setWindowTitle("New Task")
        self.setMinimumWidth(400)
        self._setup_ui(agents)

    def _setup_ui(self, agents: list[str]):
        layout = QFormLayout(self)

        self.agent_combo = QComboBox()
        self.agent_combo.addItems(agents)
        layout.addRow("Agent:", self.agent_combo)

        self.summary_edit = QLineEdit()
        self.summary_edit.setPlaceholderText("Brief task summary")
        layout.addRow("Summary:", self.summary_edit)

        self.body_edit = QTextEdit()
        self.body_edit.setPlaceholderText("Detailed task description (optional)")
        self.body_edit.setMaximumHeight(100)
        layout.addRow("Details:", self.body_edit)

        self.scope_edit = QLineEdit()
        self.scope_edit.setPlaceholderText("src/*, tests/* (comma-separated)")
        layout.addRow("Scope:", self.scope_edit)

        self.review_only = QCheckBox("Review only (no edits)")
        layout.addRow("", self.review_only)

        buttons = QDialogButtonBox(
            QDialogButtonBox.StandardButton.Ok | QDialogButtonBox.StandardButton.Cancel
        )
        buttons.accepted.connect(self.accept)
        buttons.rejected.connect(self.reject)
        layout.addRow(buttons)

    def get_task_data(self) -> dict:
        constraints = []
        if self.review_only.isChecked():
            constraints = ["review_only", "no_file_edits"]

        scope = [s.strip() for s in self.scope_edit.text().split(",") if s.strip()]

        return {
            "to_agent": self.agent_combo.currentText(),
            "summary": self.summary_edit.text(),
            "body": self.body_edit.toPlainText() or None,
            "scope": scope or None,
            "constraints": constraints or None,
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

        self.new_task_btn = QPushButton("+ New Task")
        layout.addWidget(self.new_task_btn)

    def add_task(self, task_id: str, summary: str, agent: str, state: TaskState | str):
        if isinstance(state, str):
            state = TaskState(state)
        icon = "●" if state == TaskState.ACCEPTED else "○" if state == TaskState.ASSIGNED else "✓"
        text = f"{icon} {task_id} ({agent})\n   {summary}"
        item = QListWidgetItem(text)
        item.setData(Qt.ItemDataRole.UserRole, task_id)
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

    def __init__(self, on_input_callback=None):
        super().__init__()
        self.screen = pyte.Screen(120, 30)
        self.stream = pyte.Stream(self.screen)
        self.master_fd = None
        self.pid = None
        self.session_name = None
        self.on_input_callback = on_input_callback  # (agent, text, mode) -> None

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
        if not text:
            return

        mode = self.mode_combo.currentText()
        agent = self._get_current_agent()

        # If we have a callback and mode is override/chat, use daemon routing
        if self.on_input_callback and agent and mode in ("override", "chat"):
            self.on_input_callback(agent, text, mode)
            self.input_line.clear()
        elif self.master_fd:
            # Direct PTY write
            os.write(self.master_fd, (text + "\n").encode())
            self.input_line.clear()

    def _get_current_agent(self) -> str | None:
        """Extract agent name from session name (vaelkor-<agent>)."""
        if self.session_name and self.session_name.startswith("vaelkor-"):
            return self.session_name[8:]  # len("vaelkor-") = 8
        return None

    def closeEvent(self, event):
        self._detach()
        super().closeEvent(event)


class MainWindow(QMainWindow):
    """Vaelkor main application window."""

    def __init__(self, controller: DaemonController | None = None):
        super().__init__()
        self.controller = controller or DaemonController()
        self.setWindowTitle("Vaelkor")
        self.setGeometry(100, 100, 1200, 800)
        self._apply_style()
        self._setup_ui()
        self._connect_signals()

        # Start daemon
        self.controller.start()

    def _connect_signals(self):
        self.controller.task_added.connect(self._on_task_added)
        self.controller.agent_status_changed.connect(self._on_agent_status)
        self.controller.connected.connect(self._on_connected)

    def _on_task_added(self, task_id: str, summary: str, agent: str, state: str):
        self.task_list.add_task(task_id, summary, agent, state)

    def _on_agent_status(self, agent: str, status: str):
        self.agent_status.set_agent_status(agent, status)

    def _on_connected(self):
        self.statusBar().showMessage("Daemon connected", 3000)
        self.terminal.refresh_sessions()

    def closeEvent(self, event):
        self.controller.stop()
        self.terminal._detach()
        super().closeEvent(event)

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

        self.terminal = TerminalWidget(on_input_callback=self._on_terminal_input)
        splitter.addWidget(self.terminal)

        splitter.setSizes([350, 850])
        main_layout.addWidget(splitter)

        # Connect new task button
        self.task_list.new_task_btn.clicked.connect(self._show_new_task_dialog)

        # Refresh timer for terminal sessions
        self.refresh_timer = QTimer(self)
        self.refresh_timer.timeout.connect(self.terminal.refresh_sessions)
        self.refresh_timer.start(5000)

    def _show_new_task_dialog(self):
        agents = list(self.controller.config.agents.keys())
        dialog = NewTaskDialog(agents, self)
        if dialog.exec() == QDialog.DialogCode.Accepted:
            data = dialog.get_task_data()
            if data["summary"]:
                self.controller.assign_task(**data)

    def _on_terminal_input(self, agent: str, text: str, mode: str):
        """Handle terminal input via daemon."""
        if mode == "task":
            # Create a new task
            self.controller.assign_task(to_agent=agent, summary=text)
        else:
            # Send as direct input (override or chat)
            self.controller.send_input(agent, text)
