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
import traceback
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

from claude_agent_sdk import ClaudeAgentOptions, ClaudeSDKClient
from prompt_toolkit.patch_stdout import patch_stdout

import daemon
import tools
from common import (
    C_CYAN, C_DIM, C_RED, C_RESET,
    banner, process_response, read_line, reset_assume_paste_time,
)
from alor_footer import print_footer
from tool_gate import make_gate

# `[1m]` is a Claude Code CLI variant-ID suffix (documented in the
# bundled CLI alongside `-fast`, `-1024k`, `-200k`, dated snapshots).
# It selects the 1M-context routing of the public model — NOT a stray
# context-window notation that leaked into the id. The Agent SDK's
# bundled CLI parses the suffix and forwards the correct beta headers;
# passed through raw to the Anthropic REST API it would 404, but we
# never do — every call in this codebase goes through the CLI bundle.
# Verified live: `claude --model "claude-opus-4-7[1m]" --print "…"`
# round-trips a response. See also `alor_footer.py` module docstring
# ("Alor runs [1m]-context models everywhere"). Codex-alor audit #7
# flagged this as malformed; it is not.
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


# ---- TurnRunner ------------------------------------------------------------
#
# Owns the SDK interaction. Serializes query + process_response turns
# internally so callers never hold an external `client_lock` around the
# SDK client — they `await runner.submit(text)` and get back a future
# that resolves when THEIR turn completes.
#
# Pre-fix, the orch held a shared `asyncio.Lock` around every
# `client.query(...) + process_response(...)` pair: Fett's interactive
# turn, the greet-turn at startup, /reset, and event injection all
# contended on the same lock. Mid-turn worker events (`task.completed`,
# `worker.orch_response`, etc.) couldn't be queued into the SDK until
# the current turn released the lock. For a long Fett turn that meant
# the SDK didn't see the event in its next turn boundary — observable
# as "orch doesn't know the worker finished until Fett types something
# else."
#
# After fix, event_watcher just submits each injection to the runner
# queue and moves on (doesn't even need to await — fire-and-forget is
# fine for telemetry). Queue is FIFO; Fett's interactive turns and
# event injections interleave based on arrival order. `/reset` still
# needs mutual exclusion with turn execution, so the runner exposes
# `reset_client()` which acquires the turn-executing lock internally.

@dataclass
class _Turn:
    """One queued SDK turn awaiting the runner."""
    text: str
    # Optional callback invoked with each assistant TextBlock; None
    # means process_response's default print path. Used by the
    # capture-last-assistant pattern if callers need it later.
    on_text: Callable[[str], None] | None
    # Short label for the footer / diagnostic log.
    label: str
    # Resolved by the runner when the turn completes (set_result on
    # success, set_exception on any raise).
    future: asyncio.Future


class TurnRunner:
    """Serialize SDK query + process_response turns behind an internal
    queue. Callers submit without contending; events and Fett input
    both flow through the same FIFO.

    Contract:
      * `submit(text)` returns a future that resolves when the turn's
        ResultMessage arrives (or with an exception if the turn
        raised). Callers who need to block until completion do
        `await (await runner.submit(text))`. Callers who don't care
        (event injection) can just `await runner.submit(text)` and
        drop the future on the floor.
      * `reset_client()` waits for the current turn to finish, then
        runs `disconnect/connect` with no turns in flight. Queued
        turns resume after reset completes.
      * `stop()` drains the queue by injecting a sentinel, waits for
        the worker task to exit. Turns queued after stop() is called
        will hang on their future — only call on shutdown.
    """

    def __init__(
        self,
        client: ClaudeSDKClient,
        totals: dict[str, int],
        cost: list[float],
    ) -> None:
        self._client = client
        self._totals = totals
        self._cost = cost
        self._queue: asyncio.Queue[_Turn | None] = asyncio.Queue()
        # Held by the runner worker only WHILE a turn is executing.
        # reset_client acquires this externally to pause the worker
        # between turns — guarantees the SDK client isn't mid-turn
        # when we disconnect/connect it.
        self._turn_executing = asyncio.Lock()
        self._stopped = False
        self._worker_task: asyncio.Task | None = None

    def start(self) -> None:
        """Launch the background worker. Idempotent within one
        lifecycle — calling twice is a bug but won't double-spawn
        since we guard on the existing task."""
        if self._worker_task is None:
            self._worker_task = asyncio.create_task(
                self._run(), name="turn_runner"
            )

    async def submit(
        self,
        text: str,
        *,
        on_text: Callable[[str], None] | None = None,
        label: str = "turn",
    ) -> asyncio.Future:
        """Enqueue a turn. Returns a future that resolves when the
        turn completes. Never blocks on the SDK — only on the queue
        put, which is bounded only by memory.
        """
        loop = asyncio.get_running_loop()
        future: asyncio.Future = loop.create_future()
        await self._queue.put(
            _Turn(text=text, on_text=on_text, label=label, future=future)
        )
        return future

    async def reset_client(self) -> None:
        """Disconnect + reconnect the SDK client. Acquires the turn-
        executing lock so it waits for any in-flight turn to finish
        before tearing down; queued turns stay queued and resume
        after reconnect.
        """
        async with self._turn_executing:
            await self._client.disconnect()
            await self._client.connect()

    async def stop(self) -> None:
        """Signal the worker to exit and wait for it."""
        self._stopped = True
        # Sentinel so a worker parked on queue.get() wakes up.
        await self._queue.put(None)
        if self._worker_task is not None:
            try:
                await self._worker_task
            except asyncio.CancelledError:
                pass
            self._worker_task = None

    async def _run(self) -> None:
        while not self._stopped:
            t = await self._queue.get()
            if t is None:  # sentinel
                return
            async with self._turn_executing:
                try:
                    await self._client.query(t.text)
                    await process_response(
                        self._client,
                        self._totals,
                        self._cost,
                        on_text=t.on_text,
                    )
                    if not t.future.done():
                        t.future.set_result(None)
                except asyncio.CancelledError:
                    if not t.future.done():
                        t.future.cancel()
                    raise
                except Exception as e:
                    # Log with traceback so SDK-side bugs don't hide
                    # behind the one-line caller log. Caller's
                    # `await future` re-raises.
                    print(
                        f"{C_RED}[turn_runner {t.label} error] "
                        f"{e!r}{C_RESET}\n"
                        f"{C_DIM}{traceback.format_exc()}{C_RESET}"
                    )
                    if not t.future.done():
                        t.future.set_exception(e)


async def event_watcher(
    stop: asyncio.Event,
    runner: TurnRunner,
    session_start: float,
    totals: dict[str, int],
    cost: list[float],
) -> None:
    """Print daemon events; submit injectable ones to the turn runner.

    Runs the footer after each injected turn completes — kept on the
    event-watcher side (not the runner's) so footer output stays
    co-located with the event it followed.
    """
    async def consume() -> None:
        async for evt in daemon.event_stream():
            if stop.is_set():
                break
            print_event(evt)
            injected = format_event_for_agent(evt)
            if injected is None:
                continue
            # Fire into the runner queue. Awaiting submit() is just
            # the queue-put; this returns essentially instantly even
            # if the runner is mid-turn, unblocking event_watcher to
            # pull the next event. The turn runner processes the
            # injection when it's that turn's slot in the FIFO.
            try:
                fut = await runner.submit(injected, label="event")
            except asyncio.CancelledError:
                raise
            except Exception as e:
                print(f"{C_RED}[event submit failed] {e!r}{C_RESET}")
                continue
            # Separately await the turn-complete signal so we can
            # print the footer in the right order. If the turn
            # raises, the runner already logged; we just skip the
            # footer.
            try:
                await fut
            except asyncio.CancelledError:
                raise
            except Exception:
                continue
            print_footer(session_start, cost[0], totals)

    task = asyncio.create_task(consume(), name="event_watcher_consume")
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

    # Restore tmux paste-time heuristic on our own session. alor-wrapper
    # sets `assume-paste-time 0` on every managed session (needed for
    # worker-side BEGIN/END framing to see line-by-line reads), but that
    # breaks Fett's interactive multi-line pastes into the orch pane —
    # each LF submits separately instead of landing as one message. The
    # orchestrator never receives framed sends over tmux (events arrive
    # via the daemon socket, not the pane), so it can opt out of the
    # wrapper default. See common.py::reset_assume_paste_time for the
    # full rationale. Best-effort: silently skipped when not running
    # inside tmux (e.g. a dev running `python main.py` bare).
    reset_assume_paste_time("alor-orchestrator")

    system_prompt = load_system_prompt()
    model = DEFAULT_MODEL

    # Orchestrator sees the full Alor MCP surface (13 tools). Workers
    # get a restricted subset; see tools.WORKER_ACCESSIBLE_TOOLS and
    # worker.py. Role is passed explicitly even though "orch" is the
    # default so the split is visible at this call site.
    alor_server = tools.build_server("orch")
    # Scratch cwd so Claude Code's auto-memory (keyed by cwd) writes its
    # own MEMORY.md into an isolated dir instead of clobbering Fett's
    # ~/.claude/projects/-home-fett-Projects-Alor/memory/ tree. The orch
    # has no Read/Edit/Bash/Grep and `setting_sources=[]`, so cwd is only
    # an auto-memory key — moving it has no functional impact on routing.
    orch_cwd = os.path.expanduser("~/.local/share/alor/orchestrator/cwd")
    os.makedirs(orch_cwd, exist_ok=True)
    options = ClaudeAgentOptions(
        system_prompt=system_prompt,
        mcp_servers={"alor": alor_server},
        allowed_tools=tools.allowed_tool_names("orch"),
        disallowed_tools=["ToolSearch"],
        model=model,
        setting_sources=[],
        permission_mode="bypassPermissions",
        cwd=orch_cwd,
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
    event_task: asyncio.Task | None = None

    # patch_stdout makes every `print` go ABOVE the prompt_toolkit input
    # line, so streaming SDK output / event markers / footers never stomp
    # whatever Fett is currently typing.
    try:
        with patch_stdout(raw=True):
            async with ClaudeSDKClient(options=options) as client:
                # All SDK turns (greet, Fett interactive input, event
                # injection, /reset) go through one serializing runner.
                # Replaces the prior shared `client_lock` — see the
                # TurnRunner docstring for the motivation.
                runner = TurnRunner(client, totals, cost_accumulator)
                runner.start()

                event_task = asyncio.create_task(
                    event_watcher(
                        stop_events, runner, session_start, totals, cost_accumulator,
                    ),
                    name="event_watcher",
                )
                try:
                    greet_prompt = (
                        "Resumed. Acknowledge in one short line that you're back online and "
                        "ready to continue from where the prior session left off. "
                        "Do not list your tools."
                        if resume_session
                        else "Introduce yourself in one short line so Fett knows you're online and ready. "
                        "Do not list your tools."
                    )
                    greet_fut = await runner.submit(greet_prompt, label="greet")
                    await greet_fut
                    print_footer(session_start, cost_accumulator[0], totals)
                except asyncio.CancelledError:
                    raise
                except Exception as e:
                    # Runner already logged with traceback; this is the
                    # caller-side summary.
                    print(f"{C_RED}[greet error] {e!r}{C_RESET}")

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
                        # Goes through the runner so disconnect /
                        # connect is mutually exclusive with any in-
                        # flight turn (event injection mid-reset
                        # would blow up the SDK client).
                        try:
                            await runner.reset_client()
                            print("[conversation reset]")
                        except asyncio.CancelledError:
                            raise
                        except Exception as e:
                            print(
                                f"{C_RED}[reset failed] {e!r}{C_RESET}\n"
                                f"{C_DIM}{traceback.format_exc()}{C_RESET}"
                            )
                        continue

                    # Interactive turn — submit + await the future.
                    # submit() just enqueues; the await is what blocks
                    # until the ResultMessage arrives. During the
                    # await, event injections can interleave in the
                    # FIFO, but they run in isolation from each other
                    # and from Fett's turn — no mid-turn races on
                    # totals/cost or the receive_response iterator.
                    try:
                        turn_fut = await runner.submit(text, label="fett")
                        await turn_fut
                    except asyncio.CancelledError:
                        raise
                    except Exception as e:
                        print(f"{C_RED}[error] {e!r}{C_RESET}")

                    print_footer(session_start, cost_accumulator[0], totals)
    finally:
        stop_events.set()
        if event_task is not None:
            await event_task
        # runner is only defined if the inner `async with` entered — in
        # the happy path it's always defined. On a setup failure before
        # the `with`, the NameError would bubble but we're already in
        # an error state. Guard anyway.
        try:
            await runner.stop()  # type: ignore[possibly-undefined]
        except (NameError, AttributeError):
            pass

    return 0


if __name__ == "__main__":
    try:
        sys.exit(asyncio.run(main()))
    except KeyboardInterrupt:
        print()
        sys.exit(130)
