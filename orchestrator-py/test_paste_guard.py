"""Regression tests for the middle-click / clipboard paste guard.

Background:
    Middle-click on Linux X11 delivers PRIMARY selection bytes straight
    into the tty as if typed. Without bracketed paste, each embedded \\r
    fires accept-line and the first newline in a multi-line clipboard
    submits a partial message — observed repeatedly by Fett, canceled
    orch sessions.

    Fix lives in `common.py`: prompt_toolkit's Keys.BracketedPaste key
    event delivers the full paste as `event.data`, we sanitize it via
    `sanitize_paste(...)` and insert as one atomic buffer block.

This file unit-tests `sanitize_paste` — the pure function — across the
sanitation set the bug tracker asked for. The KeyBindings wiring itself
(that the handler overrides prompt_toolkit's default) is covered by the
merge-order analysis in common.py's docstring and can't easily be
automated without spinning up a full prompt_toolkit Application; manual
repro notes below.

Run standalone: `python3 test_paste_guard.py` from orchestrator-py/.
Exits 0 on pass. No pytest dependency.

Manual repro checklist (do in a real orch/worker TUI session after deploy):
    1. Copy a multi-line block (e.g. `echo -e "line1\\nline2\\nline3"
       | xclip -selection primary`). Middle-click into the TUI prompt.
       Expect: all three lines appear in the buffer as one insert; NO
       premature submit. Press Enter: submits the full three-line string.
    2. Copy a string containing ANSI color codes (e.g. from a `git log
       --color` output). Middle-click. Expect: plain text appears, no
       color codes or stray brackets.
    3. Copy a zero-width-char-laden string (common from browser pastes).
       Middle-click. Expect: clean text, no invisible bytes bloating the
       buffer.
    4. Type a normal line and hit Enter. Expect: submits as normal — the
       paste guard only runs on BracketedPaste events.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

from common import (  # noqa: E402
    ORCHESTRATOR_ASSUME_PASTE_TIME_MS,
    _build_paste_guard,
    reset_assume_paste_time,
    sanitize_paste,
)
from prompt_toolkit.keys import Keys  # noqa: E402


def assert_eq(label: str, got, want) -> None:
    if got != want:
        print(f"FAIL  {label}")
        print(f"  got : {got!r}")
        print(f"  want: {want!r}")
        raise SystemExit(1)
    print(f"ok    {label}")


# ---------------------------------------------------------------------------
# Line-ending normalization
# ---------------------------------------------------------------------------

def test_plain_text_passthrough() -> None:
    assert_eq("plain ascii untouched", sanitize_paste("hello world"), "hello world")
    assert_eq("empty -> empty", sanitize_paste(""), "")


def test_crlf_normalized_to_lf() -> None:
    assert_eq(
        "CRLF -> LF",
        sanitize_paste("line1\r\nline2\r\nline3"),
        "line1\nline2\nline3",
    )


def test_lone_cr_normalized_to_lf() -> None:
    # Old-Mac-style line endings, also common from carriage-return-only
    # clipboard paths.
    assert_eq(
        "lone CR -> LF",
        sanitize_paste("a\rb\rc"),
        "a\nb\nc",
    )


def test_mixed_line_endings() -> None:
    assert_eq(
        "mixed \\r \\r\\n \\n all become LF",
        sanitize_paste("a\r\nb\rc\nd"),
        "a\nb\nc\nd",
    )


def test_lf_preserved_in_output() -> None:
    # The whole point: after sanitization, LF stays in the buffer so the
    # user can submit a multi-line paste with an explicit Enter.
    out = sanitize_paste("line1\nline2")
    assert_eq("LF preserved", out, "line1\nline2")
    assert_eq("LF count correct", out.count("\n"), 1)


# ---------------------------------------------------------------------------
# Control-byte stripping
# ---------------------------------------------------------------------------

def test_nul_stripped() -> None:
    assert_eq(
        "NUL stripped from middle",
        sanitize_paste("hello\x00world"),
        "helloworld",
    )


def test_other_c0_controls_stripped() -> None:
    # Form feed, vertical tab, bell, shift-in/out, etc. — all the junk
    # that can hide in clipboard contents from mixed sources.
    raw = "A\x01B\x07C\x0bD\x0cE\x0eF\x1fG\x7fH"
    assert_eq("C0 controls stripped", sanitize_paste(raw), "ABCDEFGH")


def test_tab_preserved() -> None:
    assert_eq(
        "TAB preserved",
        sanitize_paste("col1\tcol2\tcol3"),
        "col1\tcol2\tcol3",
    )


# ---------------------------------------------------------------------------
# ANSI escape sequence stripping
# ---------------------------------------------------------------------------

def test_ansi_csi_color_stripped() -> None:
    # Plain SGR color codes.
    assert_eq(
        "CSI color codes stripped",
        sanitize_paste("\x1b[31mred\x1b[0m text \x1b[1;32mgreen\x1b[0m"),
        "red text green",
    )


def test_ansi_csi_cursor_movement_stripped() -> None:
    # Cursor hop sequences — dangerous if left in buffer since they'd
    # move the terminal cursor if re-emitted.
    assert_eq(
        "CSI cursor-hop stripped",
        sanitize_paste("abc\x1b[2Adef\x1b[5;10Hghi"),
        "abcdefghi",
    )


def test_ansi_osc_title_stripped() -> None:
    # OSC (Operating System Command), e.g. window title. Terminated by
    # BEL (\x07) or ST (\x1b\\).
    assert_eq(
        "OSC + BEL terminator stripped",
        sanitize_paste("pre\x1b]0;My Title\x07post"),
        "prepost",
    )
    assert_eq(
        "OSC + ST terminator stripped",
        sanitize_paste("pre\x1b]0;My Title\x1b\\post"),
        "prepost",
    )


def test_stray_esc_stripped() -> None:
    # Malformed escape: ESC followed by something that doesn't start a
    # valid sequence. Must not leave the ESC byte loose in the buffer —
    # it would terminal-poison a future render.
    out = sanitize_paste("before\x1bafter")
    assert_eq(
        "stray ESC byte stripped (one of ESC, 'a' must survive)",
        "\x1b" in out,
        False,
    )


# ---------------------------------------------------------------------------
# Zero-width / BOM stripping
# ---------------------------------------------------------------------------

def test_zero_width_stripped() -> None:
    # U+200B (ZWSP), U+200C (ZWNJ), U+200D (ZWJ), U+2060 (WJ), U+FEFF (BOM).
    assert_eq(
        "zero-width chars stripped",
        sanitize_paste("a\u200bb\u200cc\u200dd\u2060e\ufefff"),
        "abcdef",
    )


def test_bom_at_start() -> None:
    # BOM at start of paste is very common when copying from Windows-y
    # sources.
    assert_eq(
        "BOM at start stripped",
        sanitize_paste("\ufeffhello"),
        "hello",
    )


# ---------------------------------------------------------------------------
# Realistic mixed-junk paste
# ---------------------------------------------------------------------------

def test_realistic_browser_paste() -> None:
    # Simulates a common bad-paste: copied text from a browser with ANSI
    # codes (from terminal output), CRLF line endings, a BOM at the head,
    # zero-width chars, and a stray NUL.
    raw = (
        "\ufeff"                                     # BOM at start
        "\x1b[1mBUG\x1b[0m report\r\n"               # ANSI SGR + CRLF
        "\x00status: open\r\n"                       # NUL + CRLF
        "details:\n"                                 # plain LF
        "  line\u200bwith\u200czwsp\n"               # zero-width junk
        "\x1b[31mERROR:\x1b[0m something broke\r"    # more ANSI + lone CR
    )
    expected = (
        "BUG report\n"
        "status: open\n"
        "details:\n"
        "  linewithzwsp\n"
        "ERROR: something broke\n"
    )
    assert_eq("realistic browser paste", sanitize_paste(raw), expected)


def test_the_original_bug() -> None:
    """The exact failure mode from the bug report: multi-line clipboard
    containing embedded \\n that used to fire submit mid-paste."""
    multiline = "first line\nsecond line\nthird line"
    out = sanitize_paste(multiline)
    # Preserves the newlines — they'd land in the buffer, user submits
    # explicitly with Enter. No stripping, no joining with spaces.
    assert_eq("multi-line paste preserved", out, multiline)
    # Sanity: all three lines are present in the output.
    assert_eq("line count preserved", out.count("\n"), 2)


# ---------------------------------------------------------------------------
# KeyBindings wiring (no PromptSession — CI-tty-safe)
# ---------------------------------------------------------------------------

class _FakeBuffer:
    """Minimal stand-in for prompt_toolkit's Buffer.insert_text path."""

    def __init__(self) -> None:
        self.text = ""

    def insert_text(self, s: str) -> None:
        self.text += s


class _FakeEvent:
    """Minimal stand-in for prompt_toolkit's KeyPressEvent."""

    def __init__(self, data: str) -> None:
        self.data = data
        self.current_buffer = _FakeBuffer()


def test_paste_guard_registers_bracketed_paste_handler() -> None:
    kb = _build_paste_guard()
    handlers = [b for b in kb.bindings if Keys.BracketedPaste in b.keys]
    # Exactly one — our handler, no duplicates, nothing else hijacking
    # the BracketedPaste slot in our own KB.
    assert_eq("one BracketedPaste handler registered", len(handlers), 1)


def test_paste_guard_handler_sanitizes_event_data() -> None:
    """End-to-end on a synthetic event: the registered handler pulls
    event.data, sanitizes, and inserts into the buffer as one atomic
    call — no re-tokenization through the keystroke path."""
    kb = _build_paste_guard()
    handler = next(
        b.handler for b in kb.bindings if Keys.BracketedPaste in b.keys
    )
    evt = _FakeEvent("hi\r\nthere\x00\x1b[31mred\x1b[0m")
    handler(evt)
    # CRLF -> LF, NUL stripped, both ANSI SGR codes stripped. "there"
    # and "red" concatenate because the NUL between them is removed.
    assert_eq(
        "handler sanitizes paste + single insert_text call",
        evt.current_buffer.text,
        "hi\ntherered",
    )


# ---------------------------------------------------------------------------
# tmux assume-paste-time opt-out (orchestrator session)
# ---------------------------------------------------------------------------


def test_reset_assume_paste_time_default_is_non_zero() -> None:
    """Sanity: the default we reset to must be > 0, otherwise tmux's
    paste-timing heuristic stays off and the fix is a no-op. The exact
    value is a documented policy choice (500ms as of this commit)."""
    assert_eq("default paste-time > 0", ORCHESTRATOR_ASSUME_PASTE_TIME_MS > 0, True)
    assert_eq(
        "default paste-time below tmux's max-useful window",
        ORCHESTRATOR_ASSUME_PASTE_TIME_MS <= 2000,
        True,
    )


def test_reset_assume_paste_time_skips_outside_tmux(monkeypatched_env) -> None:
    """Calling outside tmux (no $TMUX) returns False without spawning
    anything. Guards against a bare `python main.py` dev invocation
    shelling out to tmux when there's no session to target."""
    monkeypatched_env.pop("TMUX", None)
    assert_eq(
        "no-op when not under tmux",
        reset_assume_paste_time("alor-orchestrator"),
        False,
    )


def test_reset_assume_paste_time_shells_out_when_under_tmux(
    monkeypatched_env, recorder
) -> None:
    """Under tmux: executes `tmux set-option -t <session>
    assume-paste-time <ms>`. We patch subprocess.run to record the
    args — no real tmux call. Confirms the exact command form the
    orchestrator emits at startup, which is the surface tmux will
    see when diagnosing.
    """
    import common
    monkeypatched_env["TMUX"] = "/tmp/tmux-1000/default,1234,5"
    with recorder.patch(common, "subprocess", run=lambda *a, **kw: recorder.RunOk()):
        ok = reset_assume_paste_time("alor-orchestrator", ms=500)
    assert_eq("tmux call succeeded (recorded)", ok, True)
    assert_eq("exactly one subprocess.run call", len(recorder.calls), 1)
    argv = recorder.calls[0][0][0]
    assert_eq(
        "tmux set-option command shape",
        argv,
        ["tmux", "set-option", "-t", "alor-orchestrator",
         "assume-paste-time", "500"],
    )


class _Recorder:
    """Tiny stand-in for pytest's monkeypatch + call recorder.

    Keeps this file's zero-runtime-deps promise: no pytest, no mock."""

    class RunOk:
        returncode = 0
        stdout = ""
        stderr = ""

    def __init__(self) -> None:
        self.calls: list[tuple[tuple, dict]] = []

    def patch(self, module, attr_name: str, **method_overrides):
        """Context manager that installs a proxy object in place of
        `module.<attr_name>` whose methods record calls + return the
        override result."""
        recorder = self

        class _Proxy:
            def __init__(self, overrides):
                self._overrides = overrides
                self.RunOk = recorder.RunOk
            def __getattr__(self, name):
                if name in self._overrides:
                    override = self._overrides[name]
                    def _wrapped(*args, **kwargs):
                        recorder.calls.append((args, kwargs))
                        return override(*args, **kwargs)
                    return _wrapped
                raise AttributeError(name)

        class _Ctx:
            def __enter__(self_):
                self_.original = getattr(module, attr_name)
                setattr(module, attr_name, _Proxy(method_overrides))
                return self_
            def __exit__(self_, *exc):
                setattr(module, attr_name, self_.original)

        return _Ctx()


# ---------------------------------------------------------------------------
# Runner
# ---------------------------------------------------------------------------


def main() -> int:
    test_plain_text_passthrough()
    test_crlf_normalized_to_lf()
    test_lone_cr_normalized_to_lf()
    test_mixed_line_endings()
    test_lf_preserved_in_output()

    test_nul_stripped()
    test_other_c0_controls_stripped()
    test_tab_preserved()

    test_ansi_csi_color_stripped()
    test_ansi_csi_cursor_movement_stripped()
    test_ansi_osc_title_stripped()
    test_stray_esc_stripped()

    test_zero_width_stripped()
    test_bom_at_start()

    test_realistic_browser_paste()
    test_the_original_bug()

    test_paste_guard_registers_bracketed_paste_handler()
    test_paste_guard_handler_sanitizes_event_data()

    # tmux paste-time opt-out suite.
    test_reset_assume_paste_time_default_is_non_zero()
    # Create a snapshot of os.environ we can mutate + restore per-test.
    env_snapshot = dict(os.environ)
    try:
        test_reset_assume_paste_time_skips_outside_tmux(os.environ)
        recorder = _Recorder()
        test_reset_assume_paste_time_shells_out_when_under_tmux(
            os.environ, recorder
        )
    finally:
        os.environ.clear()
        os.environ.update(env_snapshot)

    print()
    print("PASS — paste sanitization covers ANSI, controls, zero-width, line endings.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
