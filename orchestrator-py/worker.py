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
from tool_gate import make_gate

DEFAULT_MODEL = os.environ.get("ALOR_WORKER_MODEL", "claude-opus-4-7[1m]")
PROMPT_TEMPLATE_PATH = Path(__file__).parent / "worker_prompt.md"

# Echo-guard framing. Kept in lockstep with the Rust daemon's
# `WORKER_ECHO_SENTINEL_BEGIN` / `_END` (src-tauri/src/wrapper/protocol.rs).
#
# When the orchestrator calls `cli.agent.send_message` with `suppress_echo:
# true`, the daemon wraps the payload on-wire as:
#
#     {BEGIN}{correlation_id}\n
#     <multi-line body>\n
#     {END}{correlation_id}\n
#
# and feeds it to the worker pane via `tmux send-keys -l`, which converts
# each literal `\n` into a real Enter keystroke. So the worker's
# `read_line` sees BEGIN, every body line, and END as separate lines —
# exactly what the state machine in `stdin_loop` needs to (a) suppress
# per-line `worker.user_input` emission across the whole frame, and
# (b) dispatch the accumulated body as a single SDK turn. The uuid is
# echoed back in the subsequent `worker.orch_response` event so the orch
# can match the reply to its originating send.
#
# The uuid in the END marker is what keeps body text containing a
# stray `{END}…` line from prematurely closing the frame — the uuid is
# fresh per send, so a real collision would require predicting it.
#
# Printable ASCII so tmux → pty → prompt_toolkit passes the bytes
# through intact; unbound control chars (RS/US) get silently dropped.
WORKER_ECHO_SENTINEL_BEGIN = "__ALOR_ORCH_ECHO_BEGIN__"
WORKER_ECHO_SENTINEL_END = "__ALOR_ORCH_ECHO_END__"


def parse_frame_marker(raw_line: str) -> tuple[str | None, str | None]:
    """Classify a raw stdin line as a programmatic frame marker.

    Returns:
        ("begin", uuid) — line is a BEGIN marker with `uuid` trailing.
        ("end",   uuid) — line is an END marker with `uuid` trailing.
        (None, None)    — line is not a frame marker.

    Match is anchored at the start and the whole remainder (after trimming
    trailing whitespace) is treated as the uuid. No validation of uuid
    shape is done here — the caller enforces an exact-match check against
    the stashed BEGIN uuid when closing a frame.
    """
    for kind, prefix in (
        ("begin", WORKER_ECHO_SENTINEL_BEGIN),
        ("end", WORKER_ECHO_SENTINEL_END),
    ):
        if raw_line.startswith(prefix):
            return (kind, raw_line[len(prefix):].rstrip())
    return (None, None)


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
    """Receive envelopes from the daemon, reconnecting on EOF.

    Without the outer reconnect loop, a daemon restart would EOF the socket,
    return from recv_forever, exit this coroutine, and take the worker with
    it — which is why workers used to die every time Alor restarted.
    """
    while not stop.is_set():
        try:
            async for env in sock.recv_forever():
                if stop.is_set():
                    return
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
                    return
                else:
                    # Ignore unknown envelopes — may be cli.* responses or
                    # events that the daemon mistakenly echoed.
                    pass
        except Exception as e:
            print(f"{C_RED}[daemon_loop error] {e}{C_RESET}")

        if stop.is_set():
            return

        # recv_forever returned because the socket hit EOF (daemon restart,
        # crash, SIGTERM to the daemon, etc). Reconnect and re-register with
        # backoff so the worker survives the daemon cycling.
        print(f"{C_YELLOW}[daemon connection lost, reconnecting in 2s…]{C_RESET}")
        await sock.close()
        await asyncio.sleep(2.0)
        try:
            await sock.connect_and_register()
            print(f"{C_GREEN}[reconnected to daemon]{C_RESET}")
        except Exception as e:
            print(f"{C_RED}[reconnect failed] {e}; retrying in 3s{C_RESET}")
            await asyncio.sleep(3.0)
            # Skip the flush attempt this iteration — not connected. The
            # outer `while not stop.is_set()` loop takes us back through
            # the reconnect dance on the next pass.
            continue

        # Drain any envelopes that were stashed while the socket was down
        # (e.g. a `task.complete` that caught BrokenPipeError mid-flight
        # while the daemon was restarting). FIFO, strict: if the replay
        # stalls again, loop back through the reconnect path instead of
        # entering `recv_forever` with unflushed frames — we don't want
        # fresh sends interleaved ahead of a pending completion.
        pending = sock.pending_count()
        if pending:
            try:
                flushed = await sock.flush_outbox()
            except Exception as e:
                print(f"{C_RED}[flush error] {e}{C_RESET}")
                flushed = 0
            remaining = sock.pending_count()
            if remaining:
                print(
                    f"{C_YELLOW}[flushed {flushed}/{pending} queued; "
                    f"{remaining} still pending, retrying reconnect]{C_RESET}"
                )
                await sock.close()
                continue
            print(f"{C_GREEN}[flushed {flushed} queued envelope(s)]{C_RESET}")


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

    Two classes of input arrive on stdin:

    1. Fett-typed lines (unframed). Each line is forwarded to the daemon
       as a `worker.user_input` event so the orch stays aware of
       follow-ups, then dispatched to the SDK as its own turn.

    2. Orch-origin programmatic sends, wrapped in BEGIN/END framing
       (see `WORKER_ECHO_SENTINEL_BEGIN/_END`). The state machine below
       treats everything between a matching BEGIN/END pair as one atomic
       send: body lines are accumulated verbatim without per-line
       forwarding or per-line SDK dispatch, then flushed as a single
       `client.query` when END arrives. After the SDK turn completes,
       a `worker.orch_response` event carries the final assistant
       TextBlock plus the correlation_id from BEGIN, so the orch can
       match the reply to its originating `agent_send_message` send.

    The framing is what makes multi-line programmatic sends safe —
    without it, tmux's `send-keys -l` would split the payload across
    multiple `read_line` calls, only the first line would be
    sentinel-guarded, and the rest would bounce back as bogus
    "Fett typed" events.
    """
    # Framing state. When `in_frame` is True, we are between a BEGIN and
    # its matching END: every incoming line goes into `frame_buffer`
    # verbatim (including blanks, leading whitespace, and would-be slash
    # commands — those are just body text inside a programmatic send).
    # No SDK dispatch, no event forwarding until the frame closes.
    in_frame = False
    frame_uuid: str | None = None
    frame_buffer: list[str] = []
    # Captured at BEGIN time so `during_task` reflects when the orch
    # sent the message, not when we happened to get around to dispatching
    # it (could differ if a task was mid-flight on `client_lock`).
    frame_task_id: str | None = None

    while not stop.is_set():
        # prompt_toolkit owns the prompt line; patch_stdout keeps streaming
        # task output above it without clobbering the input buffer.
        raw_line = await read_line(f"{C_CYAN}{agent_id}>{C_RESET} ")
        if raw_line is None:
            stop.set()
            return

        # ---- Inside a programmatic frame: only the matching END closes. ----
        if in_frame:
            # Exact END match (prefix + stashed uuid). Deliberately strict on
            # END: we do NOT match END markers with a different uuid, so the
            # body can contain arbitrary text (including our own END marker
            # strings for other uuids) without tripping false boundaries.
            # Only the uuid we issued on BEGIN can close this frame.
            expected_end = f"{WORKER_ECHO_SENTINEL_END}{frame_uuid}"
            if raw_line.rstrip() == expected_end:
                body = "\n".join(frame_buffer)
                closed_uuid = frame_uuid
                closed_task_id = frame_task_id
                in_frame = False
                frame_uuid = None
                frame_buffer = []
                frame_task_id = None

                # Dispatch the accumulated body as ONE SDK turn, capture the
                # final assistant TextBlock for the reply.
                latest: dict[str, str] = {"text": ""}

                def capture_orch_reply(t: str) -> None:
                    latest["text"] = t
                    print(t)

                dispatched = False
                if body.strip():
                    try:
                        async with client_lock:
                            await client.query(body)
                            await process_response(
                                client, totals, cost, on_text=capture_orch_reply
                            )
                        dispatched = True
                    except Exception as e:
                        print(f"{C_RED}[error] {e}{C_RESET}")
                        # Fall through — still emit orch_response (possibly
                        # empty) so the orch's await doesn't hang forever.
                # else: empty-body frame. Still report back so the awaiting
                # caller gets a prompt timeout-or-empty answer instead of
                # waiting 60s.

                reply = latest["text"].strip()
                encoded = reply.encode("utf-8")
                MAX = 64 * 1024
                if len(encoded) > MAX:
                    trimmed = encoded[:MAX]
                    while trimmed and (trimmed[-1] & 0xC0) == 0x80:
                        trimmed = trimmed[:-1]
                    reply = trimmed.decode("utf-8", errors="ignore") + "\n…[truncated]"

                try:
                    await sock.send_worker_orch_response(
                        correlation_id=closed_uuid or "",
                        text=reply,
                        during_task=closed_task_id is not None,
                        task_id=closed_task_id,
                    )
                except Exception as e:
                    # Non-fatal — orch will just time out waiting. Worth a
                    # visible warning though, since this means a tool call
                    # on the orch side will return a timeout marker.
                    print(f"{C_RED}[orch_response send failed] {e}{C_RESET}")

                if dispatched:
                    print_footer(session_start, cost[0], totals)
                continue

            # Nested BEGIN recovery. If a BEGIN marker arrives while we're
            # already inside a frame, the previous BEGIN never got its END —
            # concurrent `agent_send_message` calls interleaving at the
            # tmux-server level (tmux doesn't guarantee send-keys atomicity
            # across calls to the same pane), pane detach/reattach losing
            # keystrokes mid-frame, or a daemon crash between the BEGIN and
            # END enqueue. Without this recovery the stale buffer would
            # accumulate forever and only flush on worker EOF.
            #
            # Bounded damage: discard the stale frame (its orch caller will
            # time out — same outcome as if the END had been lost) and start
            # fresh on the new uuid. The new frame completes normally.
            #
            # Collision risk: a body line that happens to exactly match
            # `BEGIN_PREFIX + <uuid-shaped-string>` would trigger a false
            # reset. That's the same shape of risk we already accept for
            # END collisions, and the prefix is a distinctive 24-byte
            # literal — vanishingly unlikely in free-form prose.
            kind, marker_uuid = parse_frame_marker(raw_line)
            if kind == "begin" and marker_uuid:
                stale_uuid = frame_uuid or "?"
                lines_dropped = len(frame_buffer)
                # Match the byte count the worker would have dispatched had
                # the frame closed normally (body = "\n".join(buffer)).
                bytes_dropped = len(
                    "\n".join(frame_buffer).encode("utf-8")
                )
                print(
                    f"{C_YELLOW}[warn] nested BEGIN {marker_uuid[:8]} "
                    f"inside unclosed frame {stale_uuid[:8]} — discarding "
                    f"{lines_dropped} buffered line(s), restarting on "
                    f"new uuid{C_RESET}"
                )
                # Structured telemetry alongside the warn line. Lets the
                # stale caller's orch distinguish a wedge-induced timeout
                # from an ordinary END-loss timeout and attribute the drop.
                # Fire-and-forget via the outbox — if the daemon is down
                # we just queue, same as every other worker→orch send.
                if stale_uuid != "?":
                    try:
                        await sock.send_worker_frame_wedged(
                            dropped_uuid=stale_uuid,
                            new_uuid=marker_uuid,
                            bytes_dropped=bytes_dropped,
                            lines_dropped=lines_dropped,
                            task_id=frame_task_id,
                        )
                    except Exception as e:
                        # Recovery must continue even if the send blows up.
                        print(
                            f"{C_RED}[frame_wedged send failed] {e}{C_RESET}"
                        )
                # Stay in_frame; swap uuid + buffer + task_id for the fresh
                # frame. The orch that issued the stale BEGIN will time out
                # on its own await — we can't resurrect that send.
                frame_uuid = marker_uuid
                frame_buffer = []
                frame_task_id = current.get("task_id")
                continue

            # Not the matching END, not a nested BEGIN — body line. Append
            # verbatim (preserve blanks / leading whitespace / would-be
            # slash commands / stray END markers for other uuids).
            frame_buffer.append(raw_line)
            continue

        # ---- Not in a frame: classify the line. ----
        kind, marker_uuid = parse_frame_marker(raw_line)
        if kind == "begin":
            if not marker_uuid:
                # Malformed BEGIN (no uuid). Ignore — don't enter frame
                # state, because without a uuid we can't match END.
                print(
                    f"{C_YELLOW}[warn] BEGIN marker without uuid, ignoring{C_RESET}"
                )
                continue
            in_frame = True
            frame_uuid = marker_uuid
            frame_buffer = []
            frame_task_id = current.get("task_id")
            continue
        if kind == "end":
            # Stray END outside a frame — daemon drift, orphaned marker, or
            # Fett typing the literal string. Either way, ignore rather than
            # treat as Fett input (forwarding a random END to the SDK would
            # just confuse it).
            print(
                f"{C_YELLOW}[warn] END marker outside any frame, ignoring: "
                f"{raw_line[:80]}{C_RESET}"
            )
            continue

        # Normal Fett-typed line.
        text = raw_line.strip()
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

        # Forward Fett-typed lines to the orch as an event so it stays
        # aware of post-task follow-ups. Programmatic sends are handled
        # in the framed path above and never reach here.
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
                await process_response(client, totals, cost, on_text=None)
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
        # Deny host-UI-dependent tools (AskUserQuestion, EnterPlanMode,
        # ExitPlanMode) with a typed message pointing at task.blocked /
        # final summary. See tool_gate.py — without this, the worker
        # would hit an opaque `"Answer questions?"` tool_result error
        # and plow past the ask-user moment blind.
        can_use_tool=make_gate("worker"),
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

    # Ignore SIGHUP so the worker survives the Alor daemon dying. The old
    # daemon's SIGHUP cascade (inherited process group, or tmux pane PTY
    # hiccup during restart) would otherwise take Python's default handler
    # and terminate us. The daemon_loop's reconnect path handles the rest.
    try:
        signal.signal(signal.SIGHUP, signal.SIG_IGN)
    except (ValueError, OSError):
        pass  # Not in main thread or not supported — not fatal.

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
