"""Alor orchestrator — Claude Opus router, Max-subscription auth.

REPL that dispatches tasks to worker agents via the Alor daemon socket.
Uses claude-agent-sdk; auth piggybacks on the local `claude` CLI's
Pro/Max credentials.
"""

from __future__ import annotations

import asyncio
import os
import signal
import sys
import time
from pathlib import Path

from claude_agent_sdk import ClaudeAgentOptions, ClaudeSDKClient
from prompt_toolkit.patch_stdout import patch_stdout

import daemon
import tools
from common import (
    C_CYAN, C_DIM, C_RED, C_RESET,
    banner, print_footer, process_response, read_line,
)

DEFAULT_MODEL = os.environ.get("ALOR_ORCHESTRATOR_MODEL", "claude-opus-4-7")
PROMPT_PATH = Path(os.environ["HOME"]) / ".config" / "alor" / "orchestrator_prompt.md"


def load_system_prompt() -> str:
    if not PROMPT_PATH.exists():
        sys.exit(f"system prompt not found at {PROMPT_PATH}")
    return PROMPT_PATH.read_text()


def print_event(evt: daemon.Event) -> None:
    # With patch_stdout enabled around the REPL, prompt_toolkit re-renders
    # the input line after each print — so we no longer need the `\r` trick
    # or a follow-up prompt reprint.
    ts = evt.timestamp or ""
    print(f"{C_DIM}[event {ts} {evt.event}] {evt.data}{C_RESET}")


# Events that should wake the orch agent (push into SDK context) rather than
# only being printed to the terminal.
INJECTABLE_EVENTS = {
    "task.completed",
    "task.blocked",
    "user.intervention",
    "wrapper.error",
    "worker.user_input",
}


def format_event_for_agent(evt: daemon.Event) -> str | None:
    """Render an event as a user-message injection, or None to skip."""
    if evt.event not in INJECTABLE_EVENTS:
        return None
    d = evt.data or {}
    if evt.event == "task.completed":
        agent = d.get("agent_id", "?")
        task_id = str(d.get("task_id", ""))[:8] or "?"
        summary = d.get("summary")
        if summary:
            return (
                f"[Alor event] task {task_id} completed by {agent}.\n\n"
                f"Worker report:\n{summary}"
            )
        return f"[Alor event] task {task_id} completed by {agent} (no summary attached)."
    if evt.event == "task.blocked":
        agent = d.get("agent_id", "?")
        task_id = str(d.get("task_id", ""))[:8] or "?"
        reason = d.get("reason", "?")
        return f"[Alor event] task {task_id} blocked by {agent}: {reason}"
    if evt.event == "user.intervention":
        agent = d.get("agent_id", "?")
        task_ids = d.get("task_ids") or []
        if task_ids:
            short_ids = ", ".join(str(t)[:8] for t in task_ids)
            return (
                f"[Alor event] {agent} received user intervention "
                f"on task(s) {short_ids}. Likely Fett typed directly into "
                "the pane — task_get if you need the current state."
            )
        return (
            f"[Alor event] {agent} received user intervention "
            "(no active task flagged). Likely Fett typed a question or redirect "
            "into the pane while the agent was idle."
        )
    if evt.event == "wrapper.error":
        agent = d.get("agent_id", "?")
        msg = d.get("message", "?")
        return f"[Alor event] wrapper error from {agent}: {msg}"
    if evt.event == "worker.user_input":
        agent = d.get("agent_id", "?")
        text = d.get("text", "")
        during = bool(d.get("during_task"))
        task_id = str(d.get("task_id") or "")[:8]
        if during:
            context = (
                f"mid-task ({task_id}). SDK worker is serialized on its "
                "client_lock, so this line was queued until the current turn "
                "returned."
            )
        else:
            context = "post-task. The worker's local SDK will reply to Fett directly."
        return (
            f"[Alor event] Fett typed into {agent} — {context}\n\n"
            f"Verbatim input:\n{text}\n\n"
            "ACT ONLY IF Fett is clearly asking to re-route, reassign, kill, "
            "or create a new task — the worker is already answering him in "
            "its own pane. If this is just conversational follow-up to the "
            "worker, stay silent."
        )
    return None


async def event_watcher(
    stop: asyncio.Event,
    client: ClaudeSDKClient,
    client_lock: asyncio.Lock,
    totals: dict[str, int],
    cost: list[float],
    session_start: float,
) -> None:
    """Print daemon events and inject interesting ones into the orch SDK client."""
    async def consume():
        async for evt in daemon.event_stream():
            if stop.is_set():
                break
            print_event(evt)
            injected = format_event_for_agent(evt)
            if injected is None:
                continue
            try:
                async with client_lock:
                    await client.query(injected)
                    await process_response(client, totals, cost)
                print_footer(session_start, cost[0], totals)
            except Exception as e:
                print(f"{C_RED}[event inject error] {e}{C_RESET}")

    task = asyncio.create_task(consume())
    await stop.wait()
    task.cancel()
    try:
        await task
    except asyncio.CancelledError:
        pass


async def main() -> int:
    system_prompt = load_system_prompt()
    model = DEFAULT_MODEL

    alor_server = tools.build_server()
    options = ClaudeAgentOptions(
        system_prompt=system_prompt,
        mcp_servers={"alor": alor_server},
        allowed_tools=tools.allowed_tool_names(),
        disallowed_tools=["ToolSearch"],
        model=model,
        setting_sources=[],
        permission_mode="bypassPermissions",
        cwd=os.path.expanduser("~/Projects/Alor"),
    )

    banner(
        "Alor Orchestrator",
        [
            f"model: {model}",
            "commands: /reset  /usage  /quit",
        ],
    )

    stop_events = asyncio.Event()

    # Graceful shutdown on SIGTERM so Tauri's kill_all can close us cleanly
    # without leaving a dangling SDK session. SIGINT is left alone — it has
    # REPL semantics (Ctrl-C at the prompt).
    loop = asyncio.get_running_loop()
    try:
        loop.add_signal_handler(signal.SIGTERM, stop_events.set)
    except NotImplementedError:
        pass

    # Ignore SIGHUP so the orch survives the Alor daemon dying. Otherwise
    # a daemon restart sends SIGHUP via the process group / pty and the
    # default Python handler terminates us. The event_stream in daemon.py
    # already reconnects on socket EOF, so staying alive is enough.
    try:
        signal.signal(signal.SIGHUP, signal.SIG_IGN)
    except (ValueError, OSError):
        pass

    session_start = time.monotonic()
    totals: dict[str, int] = {}
    cost_accumulator = [0.0]
    client_lock = asyncio.Lock()
    event_task: asyncio.Task | None = None

    # patch_stdout makes every `print` go ABOVE the prompt_toolkit input
    # line, so streaming SDK output / event markers / footers never stomp
    # whatever Fett is currently typing.
    try:
        with patch_stdout(raw=True):
            async with ClaudeSDKClient(options=options) as client:
                event_task = asyncio.create_task(
                    event_watcher(
                        stop_events, client, client_lock, totals, cost_accumulator, session_start
                    )
                )
                try:
                    async with client_lock:
                        await client.query(
                            "Introduce yourself in one short line so Fett knows you're online and ready. "
                            "Do not list your tools."
                        )
                        await process_response(client, totals, cost_accumulator)
                    print_footer(session_start, cost_accumulator[0], totals)
                except Exception as e:
                    print(f"{C_RED}[greet error] {e}{C_RESET}")

                while True:
                    line = await read_line(f"{C_CYAN}orch>{C_RESET} ")
                    if line is None:
                        print()
                        break
                    text = line.strip()
                    if not text:
                        continue
                    if text in ("/quit", "/exit"):
                        break
                    if text == "/usage":
                        print_footer(session_start, cost_accumulator[0], totals)
                        continue
                    if text == "/reset":
                        async with client_lock:
                            await client.disconnect()
                            await client.connect()
                        print("[conversation reset]")
                        continue

                    try:
                        async with client_lock:
                            await client.query(text)
                            await process_response(client, totals, cost_accumulator)
                    except Exception as e:
                        print(f"{C_RED}[error] {e}{C_RESET}")

                    print_footer(session_start, cost_accumulator[0], totals)
    finally:
        stop_events.set()
        if event_task is not None:
            await event_task

    return 0


if __name__ == "__main__":
    try:
        sys.exit(asyncio.run(main()))
    except KeyboardInterrupt:
        print()
        sys.exit(130)
