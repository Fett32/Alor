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
import traceback
from pathlib import Path

from claude_agent_sdk import ClaudeAgentOptions, ClaudeSDKClient
from prompt_toolkit.patch_stdout import patch_stdout

import agent_client
from agent_client import (
    AgentClient,
    Envelope,
    MSG_TASK_ASSIGN,
    MSG_STATUS_REQUEST,
    MSG_SHUTDOWN,
    default_outbox_path,
)
import common
from common import (
    C_BLUE, C_CYAN, C_DIM, C_GREEN, C_RED, C_RESET, C_YELLOW,
    banner, process_response, read_line,
)
from alor_footer import print_footer
import tools
from tool_gate import make_gate

# `[1m]` suffix → see main.py's matching constant for the full note.
# Short version: valid Claude Code CLI variant-ID suffix for the 1M-
# context route, NOT a leaked context-window label. Verified live.
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
    p.add_argument(
        "--workdir",
        default=None,
        help=(
            "cwd for the SDK client. If omitted, falls back to a per-agent scratch "
            "dir under ~/.local/share/alor/workers/<agent_id>/cwd so Claude Code's "
            "cwd-keyed auto-memory does not write into the user's primary "
            "~/.claude/projects/-home-fett/memory/ tree."
        ),
    )
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
    #
    # `sock.send_accept` routes through the strict-send path, NOT the
    # queueing one: silent enqueue of task.accept was a real bug
    # (2026-04-20, task 73378a0d). If send_accept raises, the daemon
    # does NOT know we own this task — proceeding into the SDK turn
    # would stream a response that could never be credited. Refuse
    # loudly; let the daemon-side timeout / reassign path handle it.
    try:
        await sock.send_accept(task_id)
    except asyncio.CancelledError:
        # Shutting down before we even started the task. Propagate.
        raise
    except Exception as e:
        # send_accept raised (either AgentClientError from the strict
        # path, or an unexpected programming bug). Either way: DO NOT
        # enter the SDK phase. The task stays in ASSIGNED on the
        # daemon side; operator/timeout reassigns.
        print(
            f"{C_RED}[accept not delivered — refusing to proceed; "
            f"task stays ASSIGNED until daemon-side reassignment] "
            f"{e!r}{C_RESET}\n{C_DIM}{traceback.format_exc()}{C_RESET}"
        )
        # Best-effort error broadcast so the orch's event stream sees
        # the worker-side refusal. This uses the queueing `send()`
        # path (not strict), so if the socket is still down this just
        # queues for reconnect — acceptable for telemetry.
        try:
            await sock.send_error(
                f"send_accept failed for task {task_id}: {e!r}"
            )
        except asyncio.CancelledError:
            raise
        except Exception:
            pass
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
    # Also publish to the tools ContextVar so the agent_spawn MCP tool
    # can tag cli.spawn payloads with `spawned_by_task`. Daemon uses
    # that to warn at task-completion time if worker-spawned instances
    # weren't explicitly killed. Cleared in `finally` so post-task
    # orchestrator spawns (if any) stay untagged.
    tools.set_current_task(task_id or None)
    try:
        try:
            async with client_lock:
                await client.query(prompt)
                await process_response(client, totals, cost, on_text=capture)
        except asyncio.CancelledError:
            # Cancellation is a control-flow signal, not a bug. The
            # outer cleanup (`finally` below) still clears the
            # per-turn state before the cancellation propagates.
            # Previously swallowed by `except Exception` (pre-3.8
            # habit where CancelledError was a subclass), which
            # made /quit-driven shutdown look like a "worker
            # exception during task" in logs + fired a stray
            # wrapper.error at the daemon.
            raise
        except Exception as e:
            # Real bug — not cancellation, not a transport failure
            # (SDK client errors land here). Log the full traceback
            # so bugs become diagnosable rather than showing up as
            # a one-line "worker exception during task: X" with no
            # stack.
            tb = traceback.format_exc()
            err = f"worker exception during task: {e}"
            print(f"{C_RED}[task error] {err}{C_RESET}\n{C_DIM}{tb}{C_RESET}")

            # Two envelopes to the daemon, in order:
            #
            # 1. `wrapper.error` — telemetry broadcast. Surfaces the
            #    error + traceback in the orch's event stream +
            #    daemon logs. Same as pre-fix behavior.
            #
            # 2. `task.blocked` — STATE TRANSITION. Without this,
            #    the daemon's view stays at ACCEPTED forever and
            #    the agent slot is stuck at capacity (especially
            #    bad for max_concurrent=1 agents). The daemon's
            #    MSG_TASK_BLOCKED handler transitions
            #    Accepted → Blocked, which clears the slot for
            #    reassignment. This is the codex-alor audit fix
            #    (2026-04-20).
            #
            # Both use the queueing `send()` path — if one fails,
            # the other still attempts. send_error is strictly
            # informational; send_blocked is the one that matters
            # for slot availability.
            try:
                await sock.send_error(f"{err}\n{tb}")
            except asyncio.CancelledError:
                raise
            except Exception:
                # Nested send failure — best-effort. Primary error
                # already printed; orch may just miss this specific
                # error broadcast.
                pass

            try:
                # Terse reason for the daemon event. The full
                # traceback already went to the orch via
                # wrapper.error above; keeping this short keeps the
                # task.blocked event compact for the event stream.
                await sock.send_blocked(
                    task_id=task_id,
                    reason=f"worker exception: {e!r}",
                    waiting_for=None,  # unclear what would unblock
                )
            except asyncio.CancelledError:
                raise
            except Exception as nested:
                # If the blocked-transition send fails too, log
                # loudly — this is the state-transition path, and
                # a miss means the task stays ACCEPTED (the exact
                # bug we're fixing). The outbox will replay on
                # reconnect, but if the failure is non-transport
                # the slot stays stuck.
                print(
                    f"{C_RED}[task.blocked send failed after worker exception] "
                    f"{nested!r}{C_RESET}"
                )
            return

        print_footer(session_start, cost[0], totals)
    finally:
        current["task_id"] = None
        tools.set_current_task(None)

    full_report = latest["text"].strip() or None

    # Split into terse summary + optional full-report details. The
    # orchestrator's `task.completed` event only carries `summary`,
    # which the daemon hard-caps at 512 B. Anything longer gets
    # stashed in `details` and is reachable from the orch via
    # `task_get` (hinted at by the event formatter when
    # `has_details: true` lands in the broadcast).
    #
    # Policy (audit 8b03cae6 fix #1):
    #   - Report fits in ~400 B → send as `summary` only. No details,
    #     no truncation marker. Back-compat with single-string callers.
    #   - Report > 400 B → emit the first paragraph (or first 400 B if
    #     there's no paragraph break within that window) as the terse
    #     `summary`, and send the *full* report as `details`.
    #
    # The 400 B worker-side target leaves headroom under the 512 B
    # protocol cap so the daemon's safety-net truncation rarely fires;
    # if it does (e.g. a worker with no paragraph break in 400 B), the
    # daemon's UTF-8-safe `truncate_summary` still yields a well-
    # formed string. 1 MiB server-side cap on `details` is enforced
    # by `AppState::set_task_details`; we clamp client-side at 64 KiB
    # to match the prior behavior and keep outbox frames bounded.
    TERSE_TARGET = 400  # bytes, leaves ~112 B headroom under server cap
    DETAILS_MAX = 64 * 1024  # bytes

    summary: str | None = None
    details: str | None = None

    if full_report is not None:
        encoded_full = full_report.encode("utf-8")
        if len(encoded_full) <= TERSE_TARGET:
            # Short report — summary-only. Matches the pre-split shape
            # so the orch sees exactly what it used to for small reports.
            summary = full_report
        else:
            # Long report — split. Prefer a paragraph boundary inside
            # the first TERSE_TARGET bytes; fall back to a UTF-8-safe
            # hard cut if the worker's output has no blank-line break
            # in that window.
            head_bytes = encoded_full[:TERSE_TARGET]
            # Back up to a UTF-8 boundary first so slicing a paragraph
            # break doesn't split a multi-byte char.
            while head_bytes and (head_bytes[-1] & 0xC0) == 0x80:
                head_bytes = head_bytes[:-1]
            head = head_bytes.decode("utf-8", errors="ignore")
            para_end = head.find("\n\n")
            if para_end > 0:
                summary = head[:para_end].rstrip()
            else:
                # No paragraph break — hard-cut with a terse marker.
                # Daemon's TASK_SUMMARY_TRUNCATION_MARKER would land
                # here too; we synthesize our own so the orch event
                # carries the pointer to `task_get` regardless.
                summary = head.rstrip() + "…"

            # Details = the whole report, capped client-side. Daemon
            # will re-cap at 1 MiB as a safety net.
            if len(encoded_full) > DETAILS_MAX:
                trimmed = encoded_full[:DETAILS_MAX]
                while trimmed and (trimmed[-1] & 0xC0) == 0x80:
                    trimmed = trimmed[:-1]
                details = trimmed.decode("utf-8", errors="ignore") + "\n…[truncated]"
            else:
                details = full_report

    try:
        await sock.send_complete(task_id, summary=summary, details=details)
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

    Task-assign handling is DETACHED: each `task.assign` spawns a
    background `run_task` via `asyncio.create_task` so the
    `async for env in sock.recv_forever()` loop stays drainable for
    other envelopes while the SDK turn streams. Actual SDK-turn
    serialization is still enforced by `client_lock` inside
    `run_task` itself — detachment only lifts concurrency at the
    envelope-processing layer.

    Pre-fix the loop awaited `run_task` inline, which held the loop
    blocked for the entire SDK stream. That caused the 2026-04-20
    head-of-line stall: a second `task.assign` arriving while the
    first task was mid-stream sat in ASSIGNED state until the first
    task finished. `status.request` / `shutdown` were starved the
    same way.
    """
    # Background run_task set. add_done_callback(_on_run_task_done)
    # removes entries on completion; the `finally` block below drains
    # what's left on shutdown.
    active_run_tasks: set[asyncio.Task[None]] = set()

    def _on_run_task_done(t: asyncio.Task[None]) -> None:
        active_run_tasks.discard(t)
        if t.cancelled():
            return
        exc = t.exception()
        if exc is None or isinstance(exc, asyncio.CancelledError):
            return
        # run_task has its own except-Exception arm that logs task-
        # internal failures and returns; reaching this callback means
        # an exception escaped that arm (intentional re-raise, or a
        # bug outside the inner try). Surface it — otherwise detached
        # tasks silently drop their exceptions, which was one of the
        # classic pitfalls of asyncio.create_task without a done-
        # callback.
        tb = "".join(
            traceback.format_exception(type(exc), exc, exc.__traceback__)
        )
        print(
            f"{C_RED}[detached {t.get_name()} raised] {exc!r}{C_RESET}\n"
            f"{C_DIM}{tb}{C_RESET}"
        )

    try:
      while not stop.is_set():
        try:
            async for env in sock.recv_forever():
                if stop.is_set():
                    return
                if env.kind == MSG_TASK_ASSIGN:
                    # Detach run_task so daemon_loop stays drainable.
                    # See module docstring on the HOL-block fix.
                    task_id_for_name = str(
                        (env.payload or {}).get("task_id", "?")
                    )[:8]
                    bg = asyncio.create_task(
                        run_task(
                            client,
                            client_lock,
                            sock,
                            env,
                            totals,
                            cost,
                            session_start,
                            current,
                        ),
                        name=f"run_task:{task_id_for_name}",
                    )
                    active_run_tasks.add(bg)
                    bg.add_done_callback(_on_run_task_done)
                elif env.kind == MSG_STATUS_REQUEST:
                    task_id = (env.payload or {}).get("task_id")
                    try:
                        await sock.send_status(task_id=task_id, alive=True, details="worker online")
                    except asyncio.CancelledError:
                        raise
                    except Exception as e:
                        # Best-effort: the status.request isn't
                        # critical — daemon will infer liveness from
                        # other signals. But log unexpected errors
                        # with traceback instead of a bare `pass`
                        # that hides bugs in the status-build path.
                        print(
                            f"{C_YELLOW}[status send failed] {e!r}{C_RESET}\n"
                            f"{C_DIM}{traceback.format_exc()}{C_RESET}"
                        )
                elif env.kind == MSG_SHUTDOWN:
                    print(f"{C_YELLOW}[daemon shutdown received]{C_RESET}")
                    stop.set()
                    return
                else:
                    # Ignore unknown envelopes — may be cli.* responses or
                    # events that the daemon mistakenly echoed.
                    pass
        except asyncio.CancelledError:
            # Cooperative shutdown (worker /quit, SIGTERM, outer
            # cleanup after stdin_loop exited first). Propagate so
            # the task actually cancels — DO NOT treat as a
            # "daemon connection lost" event and enter the
            # reconnect loop. That's exactly the bug the old
            # `except Exception` caused: CancelledError is NOT an
            # Exception subclass in Python 3.8+, so this arm only
            # exists to make intent explicit — but we want to be
            # loud about the split since the tuple `except
            # (CancelledError, Exception)` pre-fix pattern was
            # written by someone who thought they caught the same
            # thing.
            raise
        except OSError as e:
            # Real transport-layer error: broken socket, connection
            # reset, EPIPE mid-read, daemon restart closed our end.
            # Expected during daemon cycling; the reconnect block
            # below handles recovery. Log the errno shape so
            # post-mortem can attribute to a specific failure mode.
            print(f"{C_YELLOW}[daemon_loop transport error] {e!r}{C_RESET}")
        except Exception as e:
            # Real bug — KeyError in envelope decode, TypeError in
            # a handler, JSONDecodeError re-raised past the
            # `recv_forever` skip path, a programming mistake in
            # one of the MSG_* branches, etc. Pre-fix, these
            # landed in `[daemon_loop error]` with no traceback
            # and triggered a reconnect — disguising the bug as a
            # "network error" + silently continuing.
            #
            # Fix: dump the full traceback so the bug is
            # diagnosable, flag explicitly as a bug (not a
            # transport error), and still continue into the
            # recovery path — we don't want a single rogue
            # envelope to permanently kill the worker, but the
            # log evidence now makes drift visible.
            print(
                f"{C_RED}[daemon_loop BUG — not a transport error] "
                f"{e!r}{C_RESET}\n{C_DIM}{traceback.format_exc()}{C_RESET}"
            )

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
        except asyncio.CancelledError:
            raise
        except OSError as e:
            # Expected shape during daemon-down windows.
            print(f"{C_RED}[reconnect failed] {e!r}; retrying in 3s{C_RESET}")
            await asyncio.sleep(3.0)
            continue
        except Exception as e:
            # Non-transport failure during reconnect (e.g. auth /
            # registration logic bug). Don't treat as a "retry in
            # 3s" — surface the traceback so the bug is visible,
            # but still continue the reconnect loop so the worker
            # can recover if the bug turns out to be environmental.
            print(
                f"{C_RED}[reconnect BUG — not a transport error] "
                f"{e!r}; retrying in 3s{C_RESET}\n"
                f"{C_DIM}{traceback.format_exc()}{C_RESET}"
            )
            await asyncio.sleep(3.0)
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
            except asyncio.CancelledError:
                raise
            except OSError as e:
                print(f"{C_YELLOW}[flush transport error] {e!r}{C_RESET}")
                flushed = 0
            except Exception as e:
                print(
                    f"{C_RED}[flush BUG — not a transport error] "
                    f"{e!r}{C_RESET}\n"
                    f"{C_DIM}{traceback.format_exc()}{C_RESET}"
                )
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
    finally:
        # Drain outstanding detached run_task(s) on shutdown. On
        # normal /quit or SIGTERM the worker should finish cleanly
        # rather than leaving orphan background tasks — which would
        # otherwise print "Task was destroyed but it is pending!"
        # warnings and potentially lose a partial task.complete mid-
        # send. Bounded wait so a wedged SDK turn can't block
        # shutdown indefinitely.
        if active_run_tasks:
            pending_tasks = [t for t in active_run_tasks if not t.done()]
            if pending_tasks:
                print(
                    f"{C_YELLOW}[daemon_loop shutdown: cancelling "
                    f"{len(pending_tasks)} active run_task(s)]{C_RESET}"
                )
                for t in pending_tasks:
                    t.cancel()
                # Shield from CancelledError propagating into us
                # before we've finished draining — we want each
                # run_task's own finally (which clears per-turn
                # state) to get a fair chance to run.
                try:
                    await asyncio.wait(pending_tasks, timeout=2.0)
                except asyncio.CancelledError:
                    # We're being cancelled on top of already
                    # cancelling children. Let it propagate after
                    # one more best-effort nudge.
                    for t in pending_tasks:
                        if not t.done():
                            t.cancel()
                    raise


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
                    except asyncio.CancelledError:
                        # Cooperative cancel (worker shutting down
                        # or outer task cancelled). Propagate — the
                        # orch_response emit path below is skipped,
                        # but the awaiting orch will time out
                        # naturally. Getting stuck with an
                        # un-propagated CancelledError would leak
                        # the stdin_loop task instead.
                        raise
                    except Exception as e:
                        # Real bug path. Dump traceback so we can
                        # actually see what went wrong — the old
                        # bare `[error] <e>` hid KeyError /
                        # TypeError / SDK-side bugs in a single-
                        # line log. Still fall through to emit an
                        # orch_response (possibly empty) so the
                        # orch's await doesn't hang forever.
                        print(
                            f"{C_RED}[frame dispatch error] {e!r}{C_RESET}\n"
                            f"{C_DIM}{traceback.format_exc()}{C_RESET}"
                        )
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
                except asyncio.CancelledError:
                    raise
                except Exception as e:
                    # Non-fatal — orch will just time out waiting. Worth a
                    # visible warning though, since this means a tool call
                    # on the orch side will return a timeout marker.
                    # Traceback for non-OSError shapes so a send_*
                    # bug isn't masked as "orch will retry".
                    print(
                        f"{C_RED}[orch_response send failed] {e!r}{C_RESET}\n"
                        f"{C_DIM}{traceback.format_exc()}{C_RESET}"
                    )

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
                    except asyncio.CancelledError:
                        raise
                    except Exception as e:
                        # Recovery must continue even if the send blows up.
                        # Traceback so wedged-telemetry-path bugs are
                        # visible rather than one-line warnings.
                        print(
                            f"{C_RED}[frame_wedged send failed] {e!r}{C_RESET}\n"
                            f"{C_DIM}{traceback.format_exc()}{C_RESET}"
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
                except asyncio.CancelledError:
                    raise
                except Exception as e:
                    # Disconnect / reconnect failing is a real
                    # problem — either the SDK shim is wedged or a
                    # credential refresh failed. Give enough
                    # context to diagnose instead of a one-liner.
                    print(
                        f"{C_RED}[reset failed] {e!r}{C_RESET}\n"
                        f"{C_DIM}{traceback.format_exc()}{C_RESET}"
                    )
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
        except asyncio.CancelledError:
            raise
        except Exception as e:
            # Non-fatal: keep the local conversation going even if the
            # daemon is unreachable. The orch just won't see this line.
            # Traceback keeps a silent bug in user_input encoding from
            # turning into "orch missed a message" with no clue why.
            print(
                f"{C_RED}[forward to daemon failed] {e!r}{C_RESET}\n"
                f"{C_DIM}{traceback.format_exc()}{C_RESET}"
            )

        try:
            async with client_lock:
                await client.query(text)
                await process_response(client, totals, cost, on_text=None)
        except asyncio.CancelledError:
            # Worker is shutting down mid-turn. Propagate.
            raise
        except Exception as e:
            # Real bug — SDK internal, tool-dispatch failure, etc.
            # Traceback so we can actually debug the interactive
            # path's rare failures.
            print(
                f"{C_RED}[interactive turn error] {e!r}{C_RESET}\n"
                f"{C_DIM}{traceback.format_exc()}{C_RESET}"
            )

        print_footer(session_start, cost[0], totals)


async def main() -> int:
    args = parse_args()
    if args.workdir is None:
        # No explicit --workdir: route to per-agent scratch. Claude Code's
        # auto-memory dir is keyed off cwd, so a default of ~/ would put
        # the worker's MEMORY.md writes into ~/.claude/projects/-home-fett/
        # memory/ — the user's primary curated brief. Scratch keeps it
        # isolated. Direct `python worker.py <id>` invocations hit this
        # path; daemon-spawned workers get an explicit --workdir from
        # agent_lifecycle.rs (which has its own scratch fallback).
        workdir = os.path.expanduser(
            f"~/.local/share/alor/workers/{args.agent_id}/cwd"
        )
        os.makedirs(workdir, exist_ok=True)
        print(
            f"[worker] no --workdir provided; using per-agent scratch: {workdir}",
            file=sys.stderr,
        )
    else:
        workdir = os.path.expanduser(args.workdir)

    system_prompt = render_prompt(args.agent_id, args.project, workdir, args.model)

    # Worker-restricted Alor MCP surface: agent_spawn, agent_list,
    # agent_ensure_running, agent_send_message, agent_kill. Lets a
    # worker do end-to-end live verification (spawn test instance →
    # poll state → message it → clean up) without routing through
    # the orchestrator. Task lifecycle, project profiles, and Memory
    # Hub remain orch-only — filtered out by tools.build_server("worker")
    # and double-denied by make_gate("worker") for defense-in-depth.
    alor_server = tools.build_server("worker")
    options = ClaudeAgentOptions(
        system_prompt=system_prompt,
        model=args.model,
        setting_sources=["project"],   # picks up project-level CLAUDE.md
        permission_mode="bypassPermissions",
        cwd=workdir,
        mcp_servers={tools.MCP_SERVER_NAME: alor_server},
        allowed_tools=tools.allowed_tool_names("worker"),
        # Deny host-UI-dependent tools (AskUserQuestion, EnterPlanMode,
        # ExitPlanMode) with a typed message pointing at task.blocked /
        # final summary. Also denies worker-role attempts at orch-only
        # Alor tools + worker→orch agent_send_message. See tool_gate.py.
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
    # Pass an outbox path so queued frames survive a full worker-
    # process restart (dogfood reboots SIGKILL workers via pkill -f
    # — see src-tauri/src/commands.rs::kill_all_agents). Without
    # on-disk persistence a task.complete queued between Alor going
    # down and coming back up would be lost when the worker dies.
    sock = AgentClient(
        args.agent_id,
        outbox_path=default_outbox_path(args.agent_id),
    )
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
                ),
                name="daemon_loop",
            )
            stdin_task = asyncio.create_task(
                stdin_loop(
                    client, client_lock, totals, cost, session_start,
                    args.agent_id, stop, sock, current,
                ),
                name="stdin_loop",
            )

            done, pending = await asyncio.wait(
                [daemon_task, stdin_task], return_when=asyncio.FIRST_COMPLETED
            )
            stop.set()
            for t in pending:
                t.cancel()
                try:
                    await t
                except asyncio.CancelledError:
                    # Expected: we just called `t.cancel()` above.
                    # CancelledError is a BaseException (not Exception)
                    # since Python 3.8, so it WON'T be caught by the
                    # `except Exception` branch below — the two are
                    # split deliberately. See main.py's matching
                    # cleanup block for the same pattern on the orch
                    # side.
                    pass
                except Exception as e:
                    # The loser task raised a real exception during
                    # teardown (bug in daemon_loop/stdin_loop, socket
                    # error while winding down, SDK oddity, etc.).
                    # Don't re-raise — the winning task already
                    # produced the exit result, and the outer cleanup
                    # (sock.close) still needs to run. But surface it
                    # so bugs aren't swallowed by a too-broad except.
                    print(
                        f"{C_RED}[{t.get_name()} teardown error] "
                        f"{e!r}{C_RESET}"
                    )

    await sock.close()
    return 0


if __name__ == "__main__":
    try:
        sys.exit(asyncio.run(main()))
    except KeyboardInterrupt:
        print()
        sys.exit(130)
