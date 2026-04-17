"""Alor Claude worker — receives task.assign envelopes from the daemon,
dispatches them to a ClaudeSDKClient, reports back on completion.

CLI: worker.py <agent_id> [--workdir PATH] [--project NAME] [--model NAME]

Runs inside its own tmux session (spawned by the daemon) so Fett can watch
and optionally type follow-ups. stdin typed lines are forwarded as
additional queries to the current SDK client.
"""

from __future__ import annotations

import argparse
import asyncio
import os
import signal
import sys
import time
from pathlib import Path

from claude_agent_sdk import ClaudeAgentOptions, ClaudeSDKClient
from prompt_toolkit.patch_stdout import patch_stdout

import agent_client
from agent_client import AgentClient, Envelope, MSG_TASK_ASSIGN, MSG_STATUS_REQUEST, MSG_SHUTDOWN
import common
from common import (
    C_BLUE, C_CYAN, C_DIM, C_GREEN, C_RED, C_RESET, C_YELLOW,
    banner, print_footer, process_response, read_line,
)

DEFAULT_MODEL = os.environ.get("ALOR_WORKER_MODEL", "claude-opus-4-7")
PROMPT_TEMPLATE_PATH = Path(__file__).parent / "worker_prompt.md"

# Echo-guard sentinel. Kept in lockstep with the Rust daemon's
# `WORKER_ECHO_SENTINEL` (src-tauri/src/wrapper/server.rs). Any stdin line
# that starts with this prefix originated from a programmatic
# `cli.agent.send_message` with `suppress_echo: true` — typically the
# orchestrator answering the worker through the tmux path. The worker
# strips the prefix before feeding text to the SDK, and skips emitting a
# `worker.user_input` event so orch→worker messages don't bounce back
# and re-enter the orch as if Fett had typed them.
#
# Kept to printable ASCII so it survives tmux→pty→prompt_toolkit intact;
# unbound control chars (RS/US) get silently dropped by prompt_toolkit
# and would break the guard.
WORKER_ECHO_SENTINEL = "__ALOR_ORCH_ECHO__ "


def render_prompt(agent_id: str, project: str | None, workdir: str, model: str) -> str:
    template = PROMPT_TEMPLATE_PATH.read_text()
    return template.format(
        agent_id=agent_id,
        project=project or "generic / unscoped",
        workdir=workdir,
        model=model,
    )


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Alor Claude worker")
    p.add_argument("agent_id", help="e.g. claude-alor, claude-mandaforge")
    p.add_argument("--workdir", default=os.path.expanduser("~"), help="cwd for the SDK client")
    p.add_argument("--project", default=None, help="Project slug (for prompt + CLAUDE.md hints)")
    p.add_argument("--model", default=DEFAULT_MODEL)
    return p.parse_args()


async def run_task(
    client: ClaudeSDKClient,
    client_lock: asyncio.Lock,
    sock: AgentClient,
    env: Envelope,
    totals: dict[str, int],
    cost: list[float],
    session_start: float,
    current: dict[str, str | None],
) -> None:
    payload = env.payload
    task_id = payload.get("task_id", "")
    title = payload.get("title", "")
    description = payload.get("description", "")

    print(f"\n{C_BLUE}━━ TASK {task_id[:8]} ━━{C_RESET}  {C_DIM}{title}{C_RESET}")

    # Accept immediately — SDK readiness is structural, no injection race.
    try:
        await sock.send_accept(task_id)
    except Exception as e:
        print(f"{C_RED}[accept send failed] {e}{C_RESET}")
        return

    prompt = f"{title}\n\n{description}" if title else description

    # Keep only the last text block from the last assistant message as the
    # post-task summary.  Intermediate thinking-out-loud text (e.g. "let me
    # check X…", "reading Y…") is still printed to the pane, but what the
    # orch sees is just the final wrap-up.
    latest: dict[str, str] = {"text": ""}

    def capture(text: str) -> None:
        latest["text"] = text
        print(text)

    # Publish the active task_id so stdin_loop can tag forwarded events with
    # `during_task: true` and the task id. Cleared in `finally` so post-task
    # stdin fires with `during_task: false`.
    current["task_id"] = task_id or None
    try:
        try:
            async with client_lock:
                await client.query(prompt)
                await process_response(client, totals, cost, on_text=capture)
        except Exception as e:
            err = f"worker exception during task: {e}"
            print(f"{C_RED}[task error] {err}{C_RESET}")
            try:
                await sock.send_error(err)
            except Exception:
                pass
            return

        print_footer(session_start, cost[0], totals)
    finally:
        current["task_id"] = None

    summary = latest["text"].strip() or None
    # Cap summary client-side too so we don't blow through the daemon's
    # 1 MiB hard cap and get silently truncated; 64 KiB is plenty for a
    # post-task report.
    if summary is not None:
        encoded = summary.encode("utf-8")
        MAX = 64 * 1024
        if len(encoded) > MAX:
            trimmed = encoded[:MAX]
            # Back up to a valid UTF-8 boundary.
            while trimmed and (trimmed[-1] & 0xC0) == 0x80:
                trimmed = trimmed[:-1]
            summary = trimmed.decode("utf-8", errors="ignore") + "\n…[truncated]"

    try:
        await sock.send_complete(task_id, summary=summary)
    except Exception as e:
        print(f"{C_RED}[complete send failed] {e}{C_RESET}")


async def daemon_loop(
    sock: AgentClient,
    client: ClaudeSDKClient,
    client_lock: asyncio.Lock,
    totals: dict[str, int],
    cost: list[float],
    session_start: float,
    stop: asyncio.Event,
    current: dict[str, str | None],
) -> None:
    """Receive envelopes from the daemon forever."""
    async for env in sock.recv_forever():
        if stop.is_set():
            break
        if env.kind == MSG_TASK_ASSIGN:
            await run_task(
                client, client_lock, sock, env, totals, cost, session_start, current
            )
        elif env.kind == MSG_STATUS_REQUEST:
            task_id = (env.payload or {}).get("task_id")
            try:
                await sock.send_status(task_id=task_id, alive=True, details="worker online")
            except Exception:
                pass
        elif env.kind == MSG_SHUTDOWN:
            print(f"{C_YELLOW}[daemon shutdown received]{C_RESET}")
            stop.set()
            break
        else:
            # Ignore unknown envelopes — may be cli.* responses or events
            # that the daemon mistakenly echoed.
            pass


async def stdin_loop(
    client: ClaudeSDKClient,
    client_lock: asyncio.Lock,
    totals: dict[str, int],
    cost: list[float],
    session_start: float,
    agent_id: str,
    stop: asyncio.Event,
    sock: AgentClient,
    current: dict[str, str | None],
) -> None:
    """Let Fett type follow-ups into the worker's tmux pane.

    Non-slash lines are forwarded to the daemon as `worker.user_input`
    events so the orchestrator stays aware of follow-ups. Lines prefixed
    with `WORKER_ECHO_SENTINEL` originated from a programmatic send
    (orch → tmux send-keys with `suppress_echo: true`); the sentinel is
    stripped and the line is NOT forwarded — avoids an echo loop back into
    the orch's SDK context.
    """
    while not stop.is_set():
        # prompt_toolkit owns the prompt line; patch_stdout keeps streaming
        # task output above it without clobbering the input buffer.
        line = await read_line(f"{C_CYAN}{agent_id}>{C_RESET} ")
        if line is None:
            stop.set()
            return
        # Echo-guard: identify (and strip) programmatic injections BEFORE
        # .strip() — the sentinel must be anchored at the start of the raw
        # line so leading whitespace in Fett's typed text can't spoof it.
        is_programmatic = line.startswith(WORKER_ECHO_SENTINEL)
        if is_programmatic:
            line = line[len(WORKER_ECHO_SENTINEL):]
        text = line.strip()
        if not text:
            continue
        if text in ("/quit", "/exit"):
            stop.set()
            return
        if text == "/usage":
            print_footer(session_start, cost[0], totals)
            continue
        if text == "/reset":
            async with client_lock:
                try:
                    await client.disconnect()
                    await client.connect()
                    print("[conversation reset]")
                except Exception as e:
                    print(f"{C_RED}[reset failed] {e}{C_RESET}")
            continue

        # Forward to the orch as an event so it stays aware of post-task
        # follow-ups. Skip when the line came from a programmatic sender
        # (orch itself) — otherwise orch→worker→orch loops on every reply.
        if not is_programmatic:
            task_id = current.get("task_id")
            try:
                await sock.send_worker_user_input(
                    text=text,
                    during_task=task_id is not None,
                    task_id=task_id,
                )
            except Exception as e:
                # Non-fatal: keep the local conversation going even if the
                # daemon is unreachable. The orch just won't see this line.
                print(f"{C_RED}[forward to daemon failed] {e}{C_RESET}")

        try:
            async with client_lock:
                await client.query(text)
                await process_response(client, totals, cost)
        except Exception as e:
            print(f"{C_RED}[error] {e}{C_RESET}")
        print_footer(session_start, cost[0], totals)


async def main() -> int:
    args = parse_args()
    workdir = os.path.expanduser(args.workdir)

    system_prompt = render_prompt(args.agent_id, args.project, workdir, args.model)

    options = ClaudeAgentOptions(
        system_prompt=system_prompt,
        model=args.model,
        setting_sources=["project"],   # picks up project-level CLAUDE.md
        permission_mode="bypassPermissions",
        cwd=workdir,
    )

    banner(
        f"Alor Worker — {args.agent_id}",
        [
            f"model: {args.model}",
            f"project: {args.project or '(none)'}",
            f"workdir: {workdir}",
            "commands: /reset  /usage  /quit",
        ],
        color=C_BLUE,
    )

    session_start = time.monotonic()
    totals: dict[str, int] = {}
    cost: list[float] = [0.0]
    stop = asyncio.Event()
    # Shared pointer to the currently-running task_id (or None when idle).
    # Updated by run_task, read by stdin_loop to tag `worker.user_input`
    # events with the right `during_task` flag.
    current: dict[str, str | None] = {"task_id": None}

    # Graceful shutdown on SIGTERM/SIGINT — sets the stop event so both
    # daemon_loop and stdin_loop exit cleanly instead of leaving orphan tmux
    # sessions + half-closed sockets.
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        try:
            loop.add_signal_handler(sig, stop.set)
        except NotImplementedError:
            pass  # Non-Unix; the KeyboardInterrupt path still catches Ctrl-C.

    # Connect to daemon first — fail fast if it's not reachable.
    sock = AgentClient(args.agent_id)
    try:
        await sock.connect_and_register()
    except Exception as e:
        print(f"{C_RED}[daemon register failed] {e}{C_RESET}")
        return 1

    print(f"{C_GREEN}{args.agent_id} online — waiting for tasks.{C_RESET}")

    # patch_stdout keeps streaming task output above the live prompt line
    # so Fett can type follow-ups without them getting overwritten.
    with patch_stdout(raw=True):
        async with ClaudeSDKClient(options=options) as client:
            client_lock = asyncio.Lock()
            daemon_task = asyncio.create_task(
                daemon_loop(
                    sock, client, client_lock, totals, cost, session_start, stop, current
                )
            )
            stdin_task = asyncio.create_task(
                stdin_loop(
                    client, client_lock, totals, cost, session_start,
                    args.agent_id, stop, sock, current,
                )
            )

            done, pending = await asyncio.wait(
                [daemon_task, stdin_task], return_when=asyncio.FIRST_COMPLETED
            )
            stop.set()
            for t in pending:
                t.cancel()
                try:
                    await t
                except (asyncio.CancelledError, Exception):
                    pass

    await sock.close()
    return 0


if __name__ == "__main__":
    try:
        sys.exit(asyncio.run(main()))
    except KeyboardInterrupt:
        print()
        sys.exit(130)
