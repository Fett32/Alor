"""Reproduction + regression test for the BEGIN-without-END frame wedge.

Drives `worker.stdin_loop` with a synthetic stdin stream:

    BEGIN <uuid_a>
    body_a
    BEGIN <uuid_b>
    body_b
    END <uuid_b>

and asserts:

- Frame A is warned + dropped (no client.query, no orch_response for uuid_a).
- Frame B emits exactly one client.query with "body_b" as the body.
- Exactly one orch_response fires, carrying uuid_b and the captured reply.

Run standalone: `python test_frame_wedge.py` from orchestrator-py/. Exits 0 on
pass, non-zero with a diff on fail. No pytest dependency — the project has no
test harness yet and this is a point-fix verifier.
"""

from __future__ import annotations

import asyncio
import sys
from pathlib import Path

# Make sibling modules importable when run from repo root or orchestrator-py/.
sys.path.insert(0, str(Path(__file__).parent))

import common  # noqa: E402
import worker  # noqa: E402


class FakeClient:
    """Stand-in for ClaudeSDKClient. Records query bodies, no-op on the rest."""

    def __init__(self) -> None:
        self.queries: list[str] = []

    async def query(self, body: str) -> None:
        self.queries.append(body)


class FakeSock:
    """Stand-in for AgentClient. Records orch_response + user_input calls."""

    def __init__(self) -> None:
        self.orch_responses: list[dict] = []
        self.user_inputs: list[dict] = []

    async def send_worker_orch_response(
        self, *, correlation_id: str, text: str, during_task: bool, task_id: str | None
    ) -> None:
        self.orch_responses.append(
            {
                "correlation_id": correlation_id,
                "text": text,
                "during_task": during_task,
                "task_id": task_id,
            }
        )

    async def send_worker_user_input(
        self, *, text: str, during_task: bool, task_id: str | None
    ) -> None:
        self.user_inputs.append(
            {"text": text, "during_task": during_task, "task_id": task_id}
        )


class ScriptedStdin:
    """Replaces common.read_line with a deterministic line feeder.

    After the scripted lines are exhausted, returns None (EOF) so stdin_loop
    sets its stop event and exits cleanly.
    """

    def __init__(self, lines: list[str]) -> None:
        self._lines = list(lines)

    async def read_line(self, prompt: str = "") -> str | None:
        if not self._lines:
            return None
        return self._lines.pop(0)


async def noop_process_response(client, totals, cost, on_text=None) -> None:
    # Simulate a zero-text reply (keeps the test focused on framing, not SDK
    # response handling). on_text is still called so capture paths are
    # exercised.
    if on_text is not None:
        on_text("")


def patch_worker_deps(stdin: ScriptedStdin) -> None:
    """Monkeypatch worker.py's external deps for the duration of the test."""
    common.read_line = stdin.read_line  # type: ignore[assignment]
    worker.read_line = stdin.read_line  # type: ignore[assignment]
    common.process_response = noop_process_response  # type: ignore[assignment]
    worker.process_response = noop_process_response  # type: ignore[assignment]
    # print_footer reads real monotonic state; stub so we don't care.
    worker.print_footer = lambda *a, **kw: None  # type: ignore[assignment]


async def run_scenario() -> tuple[FakeClient, FakeSock, list[str]]:
    uuid_a = "aaaaaaaa-aaaa-4aaa-aaaa-aaaaaaaaaaaa"
    uuid_b = "bbbbbbbb-bbbb-4bbb-bbbb-bbbbbbbbbbbb"
    lines = [
        f"{worker.WORKER_ECHO_SENTINEL_BEGIN}{uuid_a}",
        "body_a_line_1",
        "body_a_line_2",
        f"{worker.WORKER_ECHO_SENTINEL_BEGIN}{uuid_b}",
        "body_b",
        f"{worker.WORKER_ECHO_SENTINEL_END}{uuid_b}",
    ]

    stdin = ScriptedStdin(lines)
    patch_worker_deps(stdin)

    client = FakeClient()
    sock = FakeSock()
    client_lock = asyncio.Lock()
    stop = asyncio.Event()
    current: dict[str, str | None] = {"task_id": None}

    # Capture stderr-ish warnings by redirecting print. The warn lines go to
    # stdout in worker.py; we hook via a shim to scan for the "nested BEGIN"
    # string.
    warnings: list[str] = []
    real_print = __builtins__.print if isinstance(__builtins__, dict) else print

    def capture_print(*args, **kwargs):
        msg = " ".join(str(a) for a in args)
        warnings.append(msg)
        # Still emit so a failing run is debuggable.
        real_print(*args, **kwargs)

    worker.print = capture_print  # type: ignore[attr-defined]

    try:
        # stdin_loop will hit ScriptedStdin EOF (None) after the scripted
        # lines, set stop, and return.
        await asyncio.wait_for(
            worker.stdin_loop(
                client,       # type: ignore[arg-type]
                client_lock,
                {},           # totals
                [0.0],        # cost
                0.0,          # session_start
                "test-agent",
                stop,
                sock,         # type: ignore[arg-type]
                current,
            ),
            timeout=5.0,
        )
    finally:
        # Restore print so assertion failures render normally.
        try:
            del worker.print
        except AttributeError:
            pass

    return client, sock, warnings


def assert_eq(label: str, got, want) -> None:
    if got != want:
        print(f"FAIL  {label}")
        print(f"  got : {got!r}")
        print(f"  want: {want!r}")
        raise SystemExit(1)
    print(f"ok    {label}")


async def main() -> int:
    client, sock, warnings = await run_scenario()

    # Frame B's body was dispatched exactly once.
    assert_eq("client.query called once", len(client.queries), 1)
    assert_eq("client.query body is body_b only", client.queries[0], "body_b")

    # Exactly one orch_response, tagged with uuid_b. Frame A's uuid must not
    # appear anywhere.
    assert_eq("orch_response count", len(sock.orch_responses), 1)
    uuid_b = "bbbbbbbb-bbbb-4bbb-bbbb-bbbbbbbbbbbb"
    uuid_a = "aaaaaaaa-aaaa-4aaa-aaaa-aaaaaaaaaaaa"
    assert_eq(
        "orch_response correlation_id is uuid_b",
        sock.orch_responses[0]["correlation_id"],
        uuid_b,
    )
    assert_eq(
        "no orch_response for uuid_a",
        any(r["correlation_id"] == uuid_a for r in sock.orch_responses),
        False,
    )

    # Fett-typed events must not have fired for any of the framed lines.
    assert_eq("no user_input events", len(sock.user_inputs), 0)

    # The nested-BEGIN warn line fired.
    saw_warn = any("nested BEGIN" in w for w in warnings)
    assert_eq("nested-BEGIN warning printed", saw_warn, True)

    print()
    print("PASS — BEGIN-without-END wedge recovery works as specified.")
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
