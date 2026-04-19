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
    banner, process_response, read_line,
)
from alor_footer import print_footer
from tool_gate import make_gate

DEFAULT_MODEL = os.environ.get("ALOR_ORCHESTRATOR_MODEL", "claude-opus-4-7[1m]")
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
    "worker.orch_response",
}

# Audit 8b03cae6 bloat fix #4. Hard cap on the raw `text` body of
# worker.user_input / worker.orch_response events when we inject them
# into the orch's SDK context. Sized to match the Rust daemon's
# `EVENT_TEXT_INJECT_MAX_BYTES` (src-tauri/src/wrapper/protocol.rs) —
# kept in sync by convention, not cross-language binding. 2 KiB is
# well above the task-complete summary cap (512 B) because these
# events legitimately carry more (console pastes, short agent
# replies, log snippets) while still bounding the pathological
# multi-KB paste. On orch_response the orchestrator can fetch the
# full body via `worker_response_get(correlation_id=X)` — on
# user_input there's no correlation_id, so truncation is terminal
# (the full text lives in the worker's own pane + SDK history).
EVENT_TEXT_INJECT_MAX_BYTES = 2048


def _truncate_text_for_inject(text: str, marker: str) -> str:
    """Cap `text` at EVENT_TEXT_INJECT_MAX_BYTES on a UTF-8 char
    boundary, appending `marker` when truncation happened.

    Returns `text` verbatim when already under the cap — no marker,
    no allocation surprises. Mirrors the Rust `truncate_summary`
    helper's contract (UTF-8-safe, marker fits inside cap or is
    omitted if it wouldn't fit).

    The 2 KiB cap leaves ~3 KiB of headroom under the 25 KiB
    rule-of-thumb ceiling Fett uses for lean tool output, even when
    a dozen of these events pile up in one orch turn.
    """
    encoded = text.encode("utf-8")
    if len(encoded) <= EVENT_TEXT_INJECT_MAX_BYTES:
        return text

    marker_bytes = marker.encode("utf-8")
    # Reserve space for the marker; if the cap is smaller than the
    # marker itself (degenerate — won't happen with the 2 KiB cap)
    # degrade to a bare hard-truncate.
    if len(marker_bytes) >= EVENT_TEXT_INJECT_MAX_BYTES:
        trimmed = encoded[:EVENT_TEXT_INJECT_MAX_BYTES]
        while trimmed and (trimmed[-1] & 0xC0) == 0x80:
            trimmed = trimmed[:-1]
        return trimmed.decode("utf-8", errors="ignore")

    target = EVENT_TEXT_INJECT_MAX_BYTES - len(marker_bytes)
    trimmed = encoded[:target]
    # Back up to a UTF-8 char boundary so we never emit an invalid
    # sequence — continuation bytes have 10xxxxxx in the top bits.
    while trimmed and (trimmed[-1] & 0xC0) == 0x80:
        trimmed = trimmed[:-1]
    return trimmed.decode("utf-8", errors="ignore") + marker


def format_event_for_agent(evt: daemon.Event) -> str | None:
    """Render an event as a user-message injection, or None to skip."""
    if evt.event not in INJECTABLE_EVENTS:
        return None
    d = evt.data or {}
    if evt.event == "task.completed":
        agent = d.get("agent_id", "?")
        task_id_full = str(d.get("task_id", ""))
        task_id = task_id_full[:8] or "?"
        title = d.get("title") or ""
        # `summary` is the terse, server-side-capped form
        # (TASK_SUMMARY_MAX_BYTES = 512 B). The full report — when the
        # worker supplied one — lives in the Task's `details` field,
        # retrievable via `task_get`. `has_details` flag on the event
        # tells us whether to hint at that.
        summary = d.get("summary")
        has_details = bool(d.get("has_details"))
        # Title may be empty if the task was pruned from state between
        # complete and broadcast; degrade gracefully rather than rendering
        # an empty quoted string.
        title_clause = f' "{title}"' if title else ""
        if summary:
            msg = (
                f"[Alor event] task {task_id}{title_clause} "
                f"completed by {agent}.\n\n"
                f"Worker report:\n{summary}"
            )
            if has_details and task_id_full:
                # Point the orch at `task_get` for the full report. Use
                # the full uuid so it can paste it straight into a tool
                # call without reconstructing from the 8-char preview.
                msg += (
                    f"\n\n(Full report available via "
                    f'task_get(task_id="{task_id_full}") — '
                    f"the Task's `details` field.)"
                )
            return msg
        return (
            f"[Alor event] task {task_id}{title_clause} "
            f"completed by {agent} (no summary attached)."
        )
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
        raw_text = d.get("text", "")
        during = bool(d.get("during_task"))
        task_id = str(d.get("task_id") or "")[:8]
        # Cap the verbatim paste at EVENT_TEXT_INJECT_MAX_BYTES (audit
        # 8b03cae6 fix #4). user_input has no correlation_id, so the
        # marker is plain — the full text lives in the worker's pane
        # and SDK history, not in daemon-side cache.
        text = _truncate_text_for_inject(raw_text, marker="… [truncated]")
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
    if evt.event == "worker.orch_response":
        # Direct reply to an orch-origin `agent_send_message` send. Orch
        # asked for this — no "stay silent" hedge. If the orch used the
        # awaiting variant (`await_response=True`), it already received
        # the text as a tool result; this event is redundant but harmless
        # in that path. In the fire-and-forget path it's the ONLY way the
        # orch sees the reply.
        agent = d.get("agent_id", "?")
        corrid_full = str(d.get("correlation_id") or "")
        corrid_short = corrid_full[:8] or "?"
        raw_text = d.get("text", "") or "(empty reply)"
        # Cap at EVENT_TEXT_INJECT_MAX_BYTES. orch_response carries a
        # correlation_id — when we truncate, point the orch at
        # `worker_response_get` so it can pull the full text on demand
        # (daemon-side LRU; bloat fix #4). The full uuid is used in the
        # marker so the LLM can paste it straight into a tool call.
        if corrid_full:
            fetch_marker = (
                f"… [truncated; fetch full text via "
                f'worker_response_get(correlation_id="{corrid_full}")]'
            )
        else:
            # Degenerate: no correlation_id on a worker.orch_response.
            # Shouldn't happen (the daemon always stamps one), but
            # degrade to the plain marker rather than emitting a broken
            # tool call hint.
            fetch_marker = "… [truncated]"
        text = _truncate_text_for_inject(raw_text, marker=fetch_marker)
        return (
            f"[Alor event] {agent} replied to your send {corrid_short}:\n\n"
            f"{text}"
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
    # Bare invocation -> fresh session. `--resume <session_id>` -> continue
    # from a prior SDK session (jsonl transcript under .claude/projects/...).
    # Glitches close the tmux pane but the transcript survives, so resume
    # gets the full message history back (tools, decisions, everything).
    resume_session: str | None = None
    argv = sys.argv[1:]
    if argv and argv[0] == "--resume":
        if len(argv) < 2:
            sys.exit("--resume requires a session id")
        resume_session = argv[1]

    system_prompt = load_system_prompt()
    model = DEFAULT_MODEL

    # Orchestrator sees the full Alor MCP surface (13 tools). Workers
    # get a restricted subset; see tools.WORKER_ACCESSIBLE_TOOLS and
    # worker.py. Role is passed explicitly even though "orch" is the
    # default so the split is visible at this call site.
    alor_server = tools.build_server("orch")
    options = ClaudeAgentOptions(
        system_prompt=system_prompt,
        mcp_servers={"alor": alor_server},
        allowed_tools=tools.allowed_tool_names("orch"),
        disallowed_tools=["ToolSearch"],
        model=model,
        setting_sources=[],
        permission_mode="bypassPermissions",
        cwd=os.path.expanduser("~/Projects/Alor"),
        # Deny host-UI-dependent tools (AskUserQuestion, EnterPlanMode,
        # ExitPlanMode) with a typed message pointing at assistant-text
        # prompting. See tool_gate.py — Fett reads the orch conversation
        # live, so the orch can just ask directly instead of invoking
        # a CC-TUI-only tool that silently drops.
        can_use_tool=make_gate("orch"),
        resume=resume_session,
    )

    banner_lines = [f"model: {model}"]
    if resume_session:
        banner_lines.append(f"resuming: {resume_session}")
    banner_lines.append("commands: /reset  /usage  /quit")
    banner(
        "Alor Orchestrator",
        banner_lines,
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
                        greet_prompt = (
                            "Resumed. Acknowledge in one short line that you're back online and "
                            "ready to continue from where the prior session left off. "
                            "Do not list your tools."
                            if resume_session
                            else "Introduce yourself in one short line so Fett knows you're online and ready. "
                            "Do not list your tools."
                        )
                        await client.query(greet_prompt)
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
