"""Alor orchestrator — Claude Opus router, Max-subscription auth.

REPL that dispatches tasks to worker agents via the Alor daemon socket.
Uses claude-agent-sdk; auth piggybacks on the local `claude` CLI's
Pro/Max credentials.
"""

from __future__ import annotations

import asyncio
import os
import sys
import time
from pathlib import Path

from claude_agent_sdk import (
    AssistantMessage,
    ClaudeAgentOptions,
    ClaudeSDKClient,
    ResultMessage,
    SystemMessage,
    TextBlock,
    ThinkingBlock,
    ToolResultBlock,
    ToolUseBlock,
    UserMessage,
)

import daemon
import tools

DEFAULT_MODEL = os.environ.get("ALOR_ORCHESTRATOR_MODEL", "claude-opus-4-7")
PROMPT_PATH = Path(os.environ["HOME"]) / ".config" / "alor" / "orchestrator_prompt.md"

# ANSI colors
C_RESET = "\x1b[0m"
C_DIM = "\x1b[2m"
C_MAGENTA = "\x1b[1;35m"
C_CYAN = "\x1b[1;36m"
C_YELLOW = "\x1b[33m"
C_RED = "\x1b[31m"


def load_system_prompt() -> str:
    if not PROMPT_PATH.exists():
        sys.exit(f"system prompt not found at {PROMPT_PATH}")
    return PROMPT_PATH.read_text()


def banner(model: str) -> None:
    print(f"{C_MAGENTA}╭─ Alor Orchestrator ─────────────────────────────╮{C_RESET}")
    print(f"{C_MAGENTA}│{C_RESET} model: {model:<42}{C_MAGENTA}│{C_RESET}")
    print(f"{C_MAGENTA}│{C_RESET} commands: /reset  /usage  /quit               {C_MAGENTA}│{C_RESET}")
    print(f"{C_MAGENTA}╰─────────────────────────────────────────────────╯{C_RESET}")


def print_event(evt: daemon.Event) -> None:
    ts = evt.timestamp or ""
    print(f"\r{C_DIM}[event {ts} {evt.event}] {evt.data}{C_RESET}")
    print(f"{C_CYAN}orch>{C_RESET} ", end="", flush=True)


def print_footer(session_start: float, total_cost_usd: float, total_tokens: dict[str, int]) -> None:
    elapsed = int(time.monotonic() - session_start)
    h, m, s = elapsed // 3600, (elapsed % 3600) // 60, elapsed % 60
    t_in = total_tokens.get("input_tokens", 0)
    t_out = total_tokens.get("output_tokens", 0)
    cw = total_tokens.get("cache_creation_input_tokens", 0)
    cr = total_tokens.get("cache_read_input_tokens", 0)
    print(
        f"{C_DIM}  in {t_in} · out {t_out} · cache_w {cw} · cache_r {cr} · "
        f"${total_cost_usd:.4f} · {h:02d}:{m:02d}:{s:02d}{C_RESET}"
    )


async def event_watcher(stop: asyncio.Event) -> None:
    """Print daemon events until `stop` is set."""
    async def consume():
        async for evt in daemon.event_stream():
            if stop.is_set():
                break
            print_event(evt)

    task = asyncio.create_task(consume())
    await stop.wait()
    task.cancel()
    try:
        await task
    except asyncio.CancelledError:
        pass


async def read_line() -> str | None:
    """Read one line from stdin without blocking the event loop.

    Returns None on EOF.
    """
    loop = asyncio.get_running_loop()
    try:
        return await loop.run_in_executor(None, sys.stdin.readline)
    except (KeyboardInterrupt, EOFError):
        return None


async def process_response(
    client: ClaudeSDKClient,
    totals: dict[str, int],
    cost_accumulator: list[float],
) -> None:
    async for msg in client.receive_response():
        if isinstance(msg, AssistantMessage):
            for block in msg.content:
                if isinstance(block, TextBlock):
                    if block.text.strip():
                        print(block.text)
                elif isinstance(block, ToolUseBlock):
                    print(f"{C_DIM}  → {block.name}({block.input}){C_RESET}")
                elif isinstance(block, ThinkingBlock):
                    # Keep thinking collapsed by default; show a marker.
                    print(f"{C_DIM}  [thinking…]{C_RESET}")
        elif isinstance(msg, UserMessage):
            # Tool results — already visible via the tool_use line; don't reprint.
            for block in msg.content if hasattr(msg, "content") else []:
                if isinstance(block, ToolResultBlock) and block.is_error:
                    text = block.content if isinstance(block.content, str) else str(block.content)
                    print(f"{C_YELLOW}  [tool error] {text}{C_RESET}")
        elif isinstance(msg, SystemMessage):
            # System/init messages from the SDK — skip unless something's off.
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


async def main() -> int:
    system_prompt = load_system_prompt()
    model = DEFAULT_MODEL

    alor_server = tools.build_server()
    options = ClaudeAgentOptions(
        system_prompt=system_prompt,
        mcp_servers={"alor": alor_server},
        allowed_tools=tools.allowed_tool_names(),
        model=model,
        setting_sources=[],
        permission_mode="bypassPermissions",
        cwd=os.path.expanduser("~/Projects/Alor"),
    )

    banner(model)

    stop_events = asyncio.Event()
    event_task = asyncio.create_task(event_watcher(stop_events))

    session_start = time.monotonic()
    totals: dict[str, int] = {}
    cost_accumulator = [0.0]

    try:
        async with ClaudeSDKClient(options=options) as client:
            try:
                await client.query(
                    "Introduce yourself in one short line so Fett knows you're online and ready. "
                    "Do not list your tools."
                )
                await process_response(client, totals, cost_accumulator)
                print_footer(session_start, cost_accumulator[0], totals)
            except Exception as e:
                print(f"{C_RED}[greet error] {e}{C_RESET}")

            while True:
                print(f"{C_CYAN}orch>{C_RESET} ", end="", flush=True)
                line = await read_line()
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
                    # Reconnect client to drop context.
                    await client.disconnect()
                    await client.connect()
                    print("[conversation reset]")
                    continue

                try:
                    await client.query(text)
                    await process_response(client, totals, cost_accumulator)
                except Exception as e:
                    print(f"{C_RED}[error] {e}{C_RESET}")

                print_footer(session_start, cost_accumulator[0], totals)
    finally:
        stop_events.set()
        await event_task

    return 0


if __name__ == "__main__":
    try:
        sys.exit(asyncio.run(main()))
    except KeyboardInterrupt:
        print()
        sys.exit(130)
