#!/usr/bin/env python3
"""
Terminal embedding spike using pyte + PTY.

Tests embedding a tmux session in a Qt widget using:
- pyte for terminal emulation (ANSI parsing)
- pty for pseudo-terminal
- QPlainTextEdit for display (monospace, simple)

Run: python spikes/terminal_pyte.py
"""

import os
import pty
import select
import subprocess
import sys

import pyte
from PySide6.QtCore import QSocketNotifier, Qt, QTimer
from PySide6.QtGui import QFont, QTextCharFormat, QColor, QTextCursor
from PySide6.QtWidgets import (
    QApplication,
    QMainWindow,
    QPlainTextEdit,
    QVBoxLayout,
    QWidget,
    QLineEdit,
)


ANSI_COLORS = {
    "black": "#1a1a1a",
    "red": "#ff5555",
    "green": "#50fa7b",
    "yellow": "#f1fa8c",
    "blue": "#6272a4",
    "magenta": "#ff79c6",
    "cyan": "#8be9fd",
    "white": "#f8f8f2",
    "default": "#f8f8f2",
}


class TerminalWidget(QWidget):
    def __init__(self, command=None):
        super().__init__()
        self.command = command or ["/bin/bash"]

        self.screen = pyte.Screen(120, 40)
        self.stream = pyte.Stream(self.screen)

        self.master_fd = None
        self.pid = None

        self._setup_ui()
        self._start_process()

    def _setup_ui(self):
        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(0)

        self.display = QPlainTextEdit()
        self.display.setReadOnly(True)
        self.display.setFont(QFont("JetBrains Mono", 11))
        self.display.setStyleSheet("""
            QPlainTextEdit {
                background-color: #1a1a1a;
                color: #f8f8f2;
                border: none;
            }
        """)

        self.input_line = QLineEdit()
        self.input_line.setFont(QFont("JetBrains Mono", 11))
        self.input_line.setStyleSheet("""
            QLineEdit {
                background-color: #2a2a2a;
                color: #f8f8f2;
                border: 1px solid #3a3a3a;
                padding: 4px;
            }
        """)
        self.input_line.returnPressed.connect(self._send_input)

        layout.addWidget(self.display, 1)
        layout.addWidget(self.input_line)

    def _start_process(self):
        self.pid, self.master_fd = pty.fork()

        if self.pid == 0:
            os.execvp(self.command[0], self.command)
        else:
            self.notifier = QSocketNotifier(
                self.master_fd, QSocketNotifier.Type.Read, self
            )
            self.notifier.activated.connect(self._read_output)

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

    def keyPressEvent(self, event):
        if self.master_fd:
            key = event.text()
            if key:
                os.write(self.master_fd, key.encode())
        super().keyPressEvent(event)

    def closeEvent(self, event):
        if self.master_fd:
            os.close(self.master_fd)
        if self.pid:
            try:
                os.kill(self.pid, 9)
            except ProcessLookupError:
                pass
        super().closeEvent(event)


class SpikeWindow(QMainWindow):
    def __init__(self):
        super().__init__()
        self.setWindowTitle("Terminal Spike - pyte")
        self.setGeometry(100, 100, 900, 600)

        self.terminal = TerminalWidget()
        self.setCentralWidget(self.terminal)


def main():
    app = QApplication(sys.argv)
    window = SpikeWindow()
    window.show()
    sys.exit(app.exec())


if __name__ == "__main__":
    main()
