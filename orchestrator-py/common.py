"""Shared utilities for Alor's Python agents (orchestrator + workers).

Kept intentionally small — anything specific to a role lives in its own module.
"""

from __future__ import annotations

import asyncio
import os
import re
import subprocess
import sys
import time
from typing import Awaitable, Callable

from claude_agent_sdk import (
    AssistantMessage,
    ClaudeSDKClient,
    ResultMessage,
    SystemMessage,
    TextBlock,
    ThinkingBlock,
    ToolResultBlock,
    ToolUseBlock,
    UserMessage,
)
from prompt_toolkit import PromptSession
from prompt_toolkit.formatted_text import ANSI
from prompt_toolkit.key_binding import KeyBindings
from prompt_toolkit.keys import Keys

# ---- ANSI colors ------------------------------------------------------------

C_RESET = "\x1b[0m"
C_DIM = "\x1b[2m"
C_BOLD = "\x1b[1m"
C_MAGENTA = "\x1b[1;35m"
C_CYAN = "\x1b[1;36m"
C_GREEN = "\x1b[1;32m"
C_YELLOW = "\x1b[33m"
C_RED = "\x1b[31m"
C_BLUE = "\x1b[1;34m"


# ---- Banners ---------------------------------------------------------------

def banner(title: str, lines: list[str], color: str = C_MAGENTA) -> None:
    """Print a boxed banner.  Lines are inner content, not padded to width.

    Width is 49 interior chars to match the existing orchestrator banner.
    """
    bar_top = f"{color}╭─ {title} {'─' * max(0, 46 - len(title))}╮{C_RESET}"
    bar_bot = f"{color}╰{'─' * 49}╯{C_RESET}"
    print(bar_top)
    for ln in lines:
        padded = f"{ln:<46}"
        print(f"{color}│{C_RESET} {padded}{color}│{C_RESET}")
    print(bar_bot)


def print_footer(session_start: float, total_cost_usd: float, totals: dict[str, int]) -> None:
    """Compact single-line status after each turn."""
    elapsed = int(time.monotonic() - session_start)
    h, m, s = elapsed // 3600, (elapsed % 3600) // 60, elapsed % 60
    t_in = totals.get("input_tokens", 0)
    t_out = totals.get("output_tokens", 0)
    cw = totals.get("cache_creation_input_tokens", 0)
    cr = totals.get("cache_read_input_tokens", 0)
    print(
        f"{C_DIM}  in {t_in} · out {t_out} · cache_w {cw} · cache_r {cr} · "
        f"${total_cost_usd:.4f} · {h:02d}:{m:02d}:{s:02d}{C_RESET}"
    )


# ---- tmux option hygiene ----------------------------------------------------

# Default paste-time for roles that don't need the "tmux treats every LF as a
# submit" behavior the wrapper path relies on.
#
# Background: `alor-wrapper` sets `assume-paste-time 0` on every session it
# manages (wrapper/src/tmux.rs::ensure_session_defaults). That setting
# disables tmux's timing heuristic for paste detection, which is load-
# bearing for the framed-send path — `tmux send-keys -l <BEGIN\nbody\nEND>`
# must land at the receiving worker.py as separate `read_line` calls per
# line, or `stdin_loop`'s BEGIN/END state machine never observes the frame
# boundary (it'd receive the whole thing as one bracketed-paste block).
#
# The side effect: interactive Fett pastes into ANY wrapper-managed pane
# lose bracketed-paste behavior too — middle-click / Ctrl-Shift-V of a
# multi-line clipboard lands as a stream of per-line submits, one Enter
# per embedded LF. For the orchestrator pane this was a real papercut
# (per the b2f03b69 verification incident: pasting a worker's multi-line
# verdict reply into the orch turned a single message into a 6-turn
# back-and-forth).
#
# Fix: roles that DON'T receive framed tmux sends (currently: orchestrator
# only) reset `assume-paste-time` on their own session at startup. Workers
# stay on the wrapper default because their framing requires it.
#
# 500ms is generous: anything typed slower than 2 key/sec stays keystroke-
# style (prefix key processing, key bindings all work). Anything faster
# than that is clearly a paste — tmux's heuristic kicks in and prompt_
# toolkit's BracketedPaste handler runs with the full clipboard as
# event.data. Bracketed-paste via the outer terminal's markers (xterm,
# Alacritty, VTE, kitty) continues to work independently.
ORCHESTRATOR_ASSUME_PASTE_TIME_MS = 500


def reset_assume_paste_time(session: str, ms: int = ORCHESTRATOR_ASSUME_PASTE_TIME_MS) -> bool:
    """Restore tmux paste-time heuristic on the given session.

    Best-effort: silently skips when not running under tmux or when
    `tmux` isn't on PATH. Returns True on successful set-option, False
    otherwise — callers generally don't care, but tests use the return
    value to gate "did we actually touch anything" assertions.

    Callers: orchestrator's `main.py` on startup. See the module-level
    `ORCHESTRATOR_ASSUME_PASTE_TIME_MS` comment for the full rationale.
    """
    if not os.environ.get("TMUX"):
        return False
    try:
        result = subprocess.run(
            ["tmux", "set-option", "-t", session, "assume-paste-time", str(ms)],
            capture_output=True,
            text=True,
            timeout=2.0,
        )
        return result.returncode == 0
    except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
        return False


# ---- Paste sanitization ----------------------------------------------------

# Paste guard for middle-click / clipboard paste into the TUI.
#
# Middle-click on Linux X11 delivers PRIMARY selection bytes straight into
# the tty as if typed, so without bracketed paste mode each embedded \r in
# a multi-line clipboard fires accept-line and submits a partial message
# mid-paste — observed repeatedly by Fett, broke orch sessions.
#
# prompt_toolkit's vt100 input enables bracketed paste by default
# (\e[?2004h) and exposes a Keys.BracketedPaste event carrying the whole
# paste as event.data. The default handler in
# prompt_toolkit.key_binding.bindings.basic only normalizes CRLF → LF; we
# replace it with a fuller sanitize that also strips ANSI, NUL, other C0
# controls, BOM, and zero-width chars. User bindings are merged LAST in
# PromptSession's key-binding stack (see shortcuts/prompt.py) and
# key_processor picks matches[-1], so our handler wins over the default.
#
# Bytes that survive sanitization are inserted as a single atomic block
# via current_buffer.insert_text — never re-tokenized through the keystroke
# path, which is exactly how the newline-as-submit bug used to happen.

# ANSI escape sequences. Must run before stripping stray \x1b so the full
# sequence (CSI / OSC / DCS / SOS / PM / APC / single-char) is consumed
# atomically, not left dangling. The OSC branch accepts either BEL (\x07)
# or ST (\x1b\\) as the terminator.
_ANSI_ESC = re.compile(
    r"""
    \x1b
    (?:
        \[ [0-?]* [ -/]* [@-~]              # CSI: \e[ params inters final
      | \] [^\x07\x1b]* (?: \x07 | \x1b\\ ) # OSC: \e] ... BEL | ST
      | [PX^_] [^\x1b]* \x1b\\              # DCS/SOS/PM/APC: ... ST
      | [@-_]                               # \e followed by single byte
    )
    """,
    re.VERBOSE,
)

# C0 control bytes to strip. Preserves tab (\x09) and LF (\x0a); CR
# (\x0d) is normalized to LF upstream of this regex so never reaches it.
_CTRL_BYTES = re.compile(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]")

# Zero-width chars + BOM. Common junk from browser / word-processor
# clipboards — invisible on paste but bloats the buffer and confuses
# downstream text handling.
_ZERO_WIDTH = re.compile(r"[\u200B\u200C\u200D\u2060\uFEFF]")


def sanitize_paste(text: str) -> str:
    """Scrub clipboard text before it enters the input buffer.

    Order matters: strip ANSI sequences first so stray \\x1b bytes from
    malformed sequences are handled by the follow-up replace.

    - ANSI CSI / OSC / DCS / etc. sequences: stripped.
    - Stray \\x1b bytes (bytes that didn't form a valid escape): stripped.
    - CRLF / lone CR: normalized to LF.
    - NUL + other C0 controls (except tab + LF): stripped.
    - Zero-width chars + BOM: stripped.
    - LF preserved — lands as a literal newline in the buffer but does
      NOT trigger submit (accept-line only fires on an actual Enter
      keypress outside the paste event), so multi-line pastes stay atomic
      and the user decides when to submit.
    """
    text = _ANSI_ESC.sub("", text)
    text = text.replace("\x1b", "")
    text = text.replace("\r\n", "\n").replace("\r", "\n")
    text = _CTRL_BYTES.sub("", text)
    text = _ZERO_WIDTH.sub("", text)
    return text


def _build_paste_guard() -> KeyBindings:
    """KeyBindings registering a paste-guard for Keys.BracketedPaste.

    Overrides prompt_toolkit's default BracketedPaste handler. Sanitizes
    the full paste atomically and inserts as one buffer block.
    """
    kb = KeyBindings()

    @kb.add(Keys.BracketedPaste)
    def _(event):
        # event.data is the full paste content between \e[200~ / \e[201~.
        event.current_buffer.insert_text(sanitize_paste(event.data))

    return kb


# ---- stdin ------------------------------------------------------------------

# One PromptSession per process.  Holds history, key bindings, rendering
# state; reusing it across calls is what gives Up/Down arrow history.
_session: PromptSession | None = None


def _get_session() -> PromptSession:
    global _session
    if _session is None:
        # Paste guard is installed as user key_bindings so it overrides the
        # default BracketedPaste handler (which only normalizes CRLF).
        _session = PromptSession(key_bindings=_build_paste_guard())
    return _session


async def read_line(prompt: str = "") -> str | None:
    """Read one line of input with full line-editing.

    Backed by prompt_toolkit, so arrow keys, word-jump (Alt+B/F or
    Ctrl+Left/Right), Home/End, Ctrl+W/U/K, and Up/Down history all work.
    Returns None on EOF or Ctrl-C at an empty prompt.

    `prompt` may contain ANSI escape sequences — they're wrapped in the
    prompt_toolkit `ANSI` formatted-text so colours render correctly
    instead of being typed as literal characters.
    """
    sess = _get_session()
    try:
        return await sess.prompt_async(ANSI(prompt))
    except (KeyboardInterrupt, EOFError):
        return None


# ---- SDK response pump ------------------------------------------------------

async def process_response(
    client: ClaudeSDKClient,
    totals: dict[str, int],
    cost_accumulator: list[float],
    on_text: Callable[[str], None] | None = None,
) -> None:
    """Consume one response stream from the SDK client.

    Prints text, tool calls, thinking markers, and tool errors.  Accumulates
    cost + token usage.  Returns when the response's ResultMessage arrives.
    """
    async for msg in client.receive_response():
        if isinstance(msg, AssistantMessage):
            for block in msg.content:
                if isinstance(block, TextBlock):
                    if block.text.strip():
                        if on_text:
                            on_text(block.text)
                        else:
                            print(block.text)
                elif isinstance(block, ToolUseBlock):
                    print(f"{C_DIM}  → {block.name}({block.input}){C_RESET}")
                elif isinstance(block, ThinkingBlock):
                    print(f"{C_DIM}  [thinking…]{C_RESET}")
        elif isinstance(msg, UserMessage):
            for block in (msg.content if hasattr(msg, "content") else []):
                if isinstance(block, ToolResultBlock) and block.is_error:
                    text = block.content if isinstance(block.content, str) else str(block.content)
                    print(f"{C_YELLOW}  [tool error] {text}{C_RESET}")
        elif isinstance(msg, SystemMessage):
            pass
        elif isinstance(msg, ResultMessage):
            if msg.total_cost_usd:
                cost_accumulator[0] += msg.total_cost_usd
            u = msg.usage or {}
            for k in (
                "input_tokens",
                "output_tokens",
                "cache_creation_input_tokens",
                "cache_read_input_tokens",
            ):
                totals[k] = totals.get(k, 0) + int(u.get(k, 0) or 0)
            if msg.is_error:
                err = msg.result or "<no detail>"
                print(f"{C_RED}[result error] {err}{C_RESET}")
            return
