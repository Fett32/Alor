"""Regression test for the framed-send runtime gate's Python surface.

The Rust-side daemon (src-tauri/src/wrapper/server.rs) rejects
`cli.agent.send_message` with `suppress_echo=true` against a
non-claude-sdk agent by returning a `cli.error` envelope whose payload
carries a stable `code` field (ERR_CODE_FRAMED_SEND_NOT_SUPPORTED).
This test verifies orchestrator-py/daemon.py maps that code to
`FramedSendNotSupportedError` instead of a generic DaemonError.

We don't run the real daemon — we stand up a tiny asyncio unix-socket
server that replays canned responses, point `daemon.DAEMON_SOCKET` at
its path, and exercise the Python client.

Run standalone: `python3 test_framed_send_gate.py` from orchestrator-py/.
Exits 0 on pass. No pytest dependency to match test_frame_wedge.py /
test_outbox.py.

Covers:
  - Coded `cli.error` with `framed_send_not_supported` raises the typed
    FramedSendNotSupportedError subclass, exception message is the
    server's prose (no `daemon error:` prefix pollution).
  - FramedSendNotSupportedError is still a DaemonError
    (isinstance compatibility for existing except-clauses).
  - Coded `cli.error` with an *unknown* code falls through to plain
    DaemonError — we don't want rogue codes to leak out uncaught.
  - Uncoded `cli.error` (legacy shape) still raises plain DaemonError
    with the `daemon error: <prose>` prefix.
"""

from __future__ import annotations

import asyncio
import json
import os
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

import daemon  # noqa: E402
from daemon import (  # noqa: E402
    DaemonError,
    FramedSendNotSupportedError,
)


# ---------------------------------------------------------------------------
# Canned-response unix server
# ---------------------------------------------------------------------------


async def _serve_once(path: str, response_payload: dict, response_type: str = "cli.error"):
    """Accept one connection, read one line, reply with a canned envelope, close.

    Returns the server object; caller must `await server.wait_closed()` after
    cancelling or letting it hit the accept cap.
    """

    async def handler(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        try:
            req_line = await reader.readline()
            if not req_line:
                return
            try:
                req = json.loads(req_line.decode().strip())
            except (UnicodeDecodeError, json.JSONDecodeError):
                req = {}
            corrid = req.get("correlation_id", "00000000-0000-0000-0000-000000000000")
            resp = {
                "type": response_type,
                "correlation_id": corrid,
                "payload": response_payload,
            }
            writer.write((json.dumps(resp) + "\n").encode())
            await writer.drain()
        finally:
            writer.close()
            try:
                await writer.wait_closed()
            except Exception:
                pass

    return await asyncio.start_unix_server(handler, path=path)


# ---------------------------------------------------------------------------
# Test harness
# ---------------------------------------------------------------------------


def assert_eq(label: str, got, want) -> None:
    if got != want:
        print(f"FAIL  {label}")
        print(f"  got : {got!r}")
        print(f"  want: {want!r}")
        raise SystemExit(1)
    print(f"ok    {label}")


def assert_is(label: str, got, want_type) -> None:
    if not isinstance(got, want_type):
        print(f"FAIL  {label}")
        print(f"  got : {type(got).__name__}: {got!r}")
        print(f"  want: isinstance of {want_type.__name__}")
        raise SystemExit(1)
    print(f"ok    {label}")


async def _with_canned_response(sock_path: str, response_payload: dict, response_type: str = "cli.error"):
    """Context helper: spin up the server, yield, tear down.

    Returns the server so the caller can close it. Uses a fresh socket
    path per call so concurrent tests don't collide.
    """
    server = await _serve_once(sock_path, response_payload, response_type=response_type)
    return server


async def run_case_framed_send_not_supported(tmpdir: str) -> None:
    sock_path = os.path.join(tmpdir, "fsns.sock")
    daemon.DAEMON_SOCKET = sock_path

    prose = (
        "framed send (suppress_echo=true) not supported for agent 'codex-x': "
        "only claude-sdk runtime workers implement BEGIN/END framing. "
        "Retry with suppress_echo=false to inject raw text."
    )
    server = await _with_canned_response(
        sock_path,
        {"code": "framed_send_not_supported", "error": prose},
    )
    try:
        raised: BaseException | None = None
        try:
            await daemon.agent_send_message("codex-x", "hi", submit=False, suppress_echo=True)
        except BaseException as e:  # noqa: BLE001 — test wants to see everything
            raised = e

        assert_is("coded cli.error -> FramedSendNotSupportedError", raised, FramedSendNotSupportedError)
        assert_is("FramedSendNotSupportedError is a DaemonError", raised, DaemonError)
        # Exception message should be the server prose verbatim — NOT
        # prefixed with "daemon error:" (that prefix is reserved for the
        # generic fall-through path).
        assert_eq(
            "exception message is server prose, no prefix",
            str(raised),
            prose,
        )
    finally:
        server.close()
        await server.wait_closed()


async def run_case_unknown_code_falls_through(tmpdir: str) -> None:
    sock_path = os.path.join(tmpdir, "unknown.sock")
    daemon.DAEMON_SOCKET = sock_path

    server = await _with_canned_response(
        sock_path,
        {"code": "some_future_code_we_dont_know_yet", "error": "nope"},
    )
    try:
        raised: BaseException | None = None
        try:
            await daemon.agent_send_message("whatever", "hi")
        except BaseException as e:  # noqa: BLE001
            raised = e

        assert_is("unknown code -> generic DaemonError", raised, DaemonError)
        # Must NOT be the typed subclass — an unknown code should stay
        # generic so it can't silently get swallowed by a narrow except.
        got_is_framed = isinstance(raised, FramedSendNotSupportedError)
        assert_eq("unknown code is not FramedSendNotSupportedError", got_is_framed, False)
        assert_eq(
            "unknown code message has 'daemon error:' prefix",
            str(raised),
            "daemon error: nope",
        )
    finally:
        server.close()
        await server.wait_closed()


async def run_case_legacy_uncoded_error(tmpdir: str) -> None:
    sock_path = os.path.join(tmpdir, "legacy.sock")
    daemon.DAEMON_SOCKET = sock_path

    # No `code` field — this is the legacy shape still emitted by the 60+
    # other cli_error callsites on the Rust side.
    server = await _with_canned_response(
        sock_path,
        {"error": "task abc123 not found"},
    )
    try:
        raised: BaseException | None = None
        try:
            await daemon.task_get("abc123")
        except BaseException as e:  # noqa: BLE001
            raised = e

        assert_is("uncoded cli.error -> generic DaemonError", raised, DaemonError)
        got_is_framed = isinstance(raised, FramedSendNotSupportedError)
        assert_eq("uncoded error is not a typed subclass", got_is_framed, False)
        assert_eq(
            "uncoded error message has 'daemon error:' prefix",
            str(raised),
            "daemon error: task abc123 not found",
        )
    finally:
        server.close()
        await server.wait_closed()


async def main() -> int:
    original_socket = daemon.DAEMON_SOCKET
    try:
        with tempfile.TemporaryDirectory(prefix="alor-test-fsgate-") as tmpdir:
            await run_case_framed_send_not_supported(tmpdir)
            await run_case_unknown_code_falls_through(tmpdir)
            await run_case_legacy_uncoded_error(tmpdir)
    finally:
        daemon.DAEMON_SOCKET = original_socket

    print()
    print("PASS — framed-send gate Python error surface is correctly typed.")
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
