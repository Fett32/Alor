#!/usr/bin/env python3
"""
Terminal spike: Attach to existing tmux session.

This tests the actual Vaelkor use case:
1. Create/find a tmux session
2. Attach to it from the Qt widget
3. Display output, send input

Run: python spikes/terminal_tmux_attach.py
"""

import os
import pty
import subprocess
import sys

import pyte
from PySide6.QtCore import QSocketNotifier, Qt
from PySide6.QtGui import QFont, QTextCursor
from PySide6.QtWidgets import (
    QApplication,
    QMainWindow,
    QPlainTextEdit,
    QVBoxLayout,
    QWidget,
    QLineEdit,
    QLabel,
    QHBoxLayout,
    QPushButton,
    QComboBox,
)


class TmuxTerminalWidget(QWidget):
    def __init__(self, session_name=None):
        super().__init__()
        self.session_name = session_name
        self.screen = pyte.Screen(120, 40)
        self.stream = pyte.Stream(self.screen)
        self.master_fd = None
        self.pid = None

        self._setup_ui()

    def _setup_ui(self):
        layout = QVBoxLayout(self)
        layout.setContentsMargins(4, 4, 4, 4)
        layout.setSpacing(4)

        controls = QHBoxLayout()
        controls.addWidget(QLabel("Session:"))

        self.session_combo = QComboBox()
        self.session_combo.setMinimumWidth(150)
        controls.addWidget(self.session_combo)

        self.refresh_btn = QPushButton("Refresh")
        self.refresh_btn.clicked.connect(self._refresh_sessions)
        controls.addWidget(self.refresh_btn)

        self.attach_btn = QPushButton("Attach")
        self.attach_btn.clicked.connect(self._attach_session)
        controls.addWidget(self.attach_btn)

        self.new_btn = QPushButton("New Session")
        self.new_btn.clicked.connect(self._create_session)
        controls.addWidget(self.new_btn)

        controls.addStretch()

        self.status_label = QLabel("Not attached")
        controls.addWidget(self.status_label)

        layout.addLayout(controls)

        self.display = QPlainTextEdit()
        self.display.setReadOnly(True)
        self.display.setFont(QFont("JetBrains Mono", 11))
        self.display.setStyleSheet("""
            QPlainTextEdit {
                background-color: #1a1a1a;
                color: #f8f8f2;
                border: 1px solid #3a3a3a;
            }
        """)

        self.input_line = QLineEdit()
        self.input_line.setFont(QFont("JetBrains Mono", 11))
        self.input_line.setPlaceholderText("Type command and press Enter...")
        self.input_line.setStyleSheet("""
            QLineEdit {
                background-color: #2a2a2a;
                color: #f8f8f2;
                border: 1px solid #3a3a3a;
                padding: 4px;
            }
        """)
        self.input_line.returnPressed.connect(self._send_input)
        self.input_line.setEnabled(False)

        layout.addWidget(self.display, 1)
        layout.addWidget(self.input_line)

        self._refresh_sessions()

    def _refresh_sessions(self):
        self.session_combo.clear()
        try:
            result = subprocess.run(
                ["tmux", "list-sessions", "-F", "#{session_name}"],
                capture_output=True,
                text=True,
            )
            if result.returncode == 0:
                sessions = result.stdout.strip().split("\n")
                self.session_combo.addItems([s for s in sessions if s])
        except FileNotFoundError:
            self.status_label.setText("tmux not found!")

    def _create_session(self):
        session_name = f"vaelkor-test-{os.getpid()}"
        subprocess.run(["tmux", "new-session", "-d", "-s", session_name])
        self._refresh_sessions()
        idx = self.session_combo.findText(session_name)
        if idx >= 0:
            self.session_combo.setCurrentIndex(idx)
        self._attach_session()

    def _attach_session(self):
        session = self.session_combo.currentText()
        if not session:
            self.status_label.setText("No session selected")
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
            self.input_line.setEnabled(True)
            self.status_label.setText(f"Attached: {session}")
            self.session_name = session

    def _detach(self):
        if self.notifier:
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
        self.input_line.setEnabled(False)
        self.status_label.setText("Detached")

    def _read_output(self):
        try:
            data = os.read(self.master_fd, 4096)
            if data:
                self.stream.feed(data.decode("utf-8", errors="replace"))
                self._render()
        except OSError:
            self.notifier.setEnabled(False)
            self.status_label.setText("Connection lost")

    def _render(self):
        lines = []
        for y in range(self.screen.lines):
            line = ""
            for x in range(self.screen.columns):
                char = self.screen.buffer[y][x]
                line += char.data if char.data else " "
            lines.append(line.rstrip())

        while lines and not lines[-1]:
            lines.pop()

        self.display.setPlainText("\n".join(lines))
        cursor = self.display.textCursor()
        cursor.movePosition(QTextCursor.End)
        self.display.setTextCursor(cursor)

    def _send_input(self):
        text = self.input_line.text()
        if self.master_fd and text:
            os.write(self.master_fd, (text + "\n").encode())
            self.input_line.clear()

    def send_keys(self, keys: str):
        """Send keys to tmux session (for programmatic control)."""
        if self.session_name:
            subprocess.run(["tmux", "send-keys", "-t", self.session_name, keys])

    def closeEvent(self, event):
        self._detach()
        super().closeEvent(event)


class SpikeWindow(QMainWindow):
    def __init__(self):
        super().__init__()
        self.setWindowTitle("Terminal Spike - tmux attach")
        self.setGeometry(100, 100, 900, 600)
        self.setStyleSheet("background-color: #2a2a2a; color: #f8f8f2;")

        self.terminal = TmuxTerminalWidget()
        self.setCentralWidget(self.terminal)


def main():
    app = QApplication(sys.argv)
    window = SpikeWindow()
    window.show()
    sys.exit(app.exec())


if __name__ == "__main__":
    main()
