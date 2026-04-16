"""Shared utilities for Alor's Python agents (orchestrator + workers).

Kept intentionally small — anything specific to a role lives in its own module.
"""

from __future__ import annotations

import asyncio
import sys
import time
from typing import Awaitable, Callable

from claude_agent_sdk import (
    AssistantMessage,
    ClaudeSDKClient,
    ResultMessage,
    SystemMessage,
    TextBlock,
    ThinkingBlock,
    ToolResultBlock,
    ToolUseBlock,
    UserMessage,
)

# ---- ANSI colors ------------------------------------------------------------

C_RESET = "\x1b[0m"
C_DIM = "\x1b[2m"
C_BOLD = "\x1b[1m"
C_MAGENTA = "\x1b[1;35m"
C_CYAN = "\x1b[1;36m"
C_GREEN = "\x1b[1;32m"
C_YELLOW = "\x1b[33m"
C_RED = "\x1b[31m"
C_BLUE = "\x1b[1;34m"


# ---- Banners ---------------------------------------------------------------

def banner(title: str, lines: list[str], color: str = C_MAGENTA) -> None:
    """Print a boxed banner.  Lines are inner content, not padded to width.

    Width is 49 interior chars to match the existing orchestrator banner.
    """
    bar_top = f"{color}╭─ {title} {'─' * max(0, 46 - len(title))}╮{C_RESET}"
    bar_bot = f"{color}╰{'─' * 49}╯{C_RESET}"
    print(bar_top)
    for ln in lines:
        padded = f"{ln:<46}"
        print(f"{color}│{C_RESET} {padded}{color}│{C_RESET}")
    print(bar_bot)


def print_footer(session_start: float, total_cost_usd: float, totals: dict[str, int]) -> None:
    """Compact single-line status after each turn."""
    elapsed = int(time.monotonic() - session_start)
    h, m, s = elapsed // 3600, (elapsed % 3600) // 60, elapsed % 60
    t_in = totals.get("input_tokens", 0)
    t_out = totals.get("output_tokens", 0)
    cw = totals.get("cache_creation_input_tokens", 0)
    cr = totals.get("cache_read_input_tokens", 0)
    print(
        f"{C_DIM}  in {t_in} · out {t_out} · cache_w {cw} · cache_r {cr} · "
        f"${total_cost_usd:.4f} · {h:02d}:{m:02d}:{s:02d}{C_RESET}"
    )


# ---- stdin ------------------------------------------------------------------

async def read_line() -> str | None:
    """Read one line from stdin without blocking the event loop.  None on EOF."""
    loop = asyncio.get_running_loop()
    try:
        return await loop.run_in_executor(None, sys.stdin.readline)
    except (KeyboardInterrupt, EOFError):
        return None


# ---- SDK response pump ------------------------------------------------------

async def process_response(
    client: ClaudeSDKClient,
    totals: dict[str, int],
    cost_accumulator: list[float],
    on_text: Callable[[str], None] | None = None,
) -> None:
    """Consume one response stream from the SDK client.

    Prints text, tool calls, thinking markers, and tool errors.  Accumulates
    cost + token usage.  Returns when the response's ResultMessage arrives.
    """
    async for msg in client.receive_response():
        if isinstance(msg, AssistantMessage):
            for block in msg.content:
                if isinstance(block, TextBlock):
                    if block.text.strip():
                        if on_text:
                            on_text(block.text)
                        else:
                            print(block.text)
                elif isinstance(block, ToolUseBlock):
                    print(f"{C_DIM}  → {block.name}({block.input}){C_RESET}")
                elif isinstance(block, ThinkingBlock):
                    print(f"{C_DIM}  [thinking…]{C_RESET}")
        elif isinstance(msg, UserMessage):
            for block in (msg.content if hasattr(msg, "content") else []):
                if isinstance(block, ToolResultBlock) and block.is_error:
                    text = block.content if isinstance(block.content, str) else str(block.content)
                    print(f"{C_YELLOW}  [tool error] {text}{C_RESET}")
        elif isinstance(msg, SystemMessage):
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
