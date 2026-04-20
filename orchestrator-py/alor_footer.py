"""Alor-specific REPL footer — separate from common.print_footer.

The common.py footer shows raw token counts + theoretical API cost. For
Max subscribers the dollar figure is a counterfactual that confused more
than informed (SDK reported $1586 for an 18h session that actually cost
$0 beyond the monthly sub).

This module is a drop-in replacement just for Alor REPLs (orch + workers)
that swaps the dollar for a context-fill indicator — the real signal for
when to restart a session — with color-coded urgency.

Drop-in: `from alor_footer import print_footer`. Signature matches
common.print_footer so callers need no further changes. Module-level
state snapshots the cumulative SDK totals so each call can derive
this turn's context size (see the `print_footer` body for why that
equals the current session context fill in Claude chat mode).
"""

from __future__ import annotations

import time

from common import C_DIM, C_RED, C_RESET, C_YELLOW


# Alor runs [1m]-context models everywhere. Kept as a single constant
# rather than threading the model string through every call site.
CTX_BUDGET = 1_000_000

# Cumulative SDK totals snapshot from the previous footer call. The
# SDK reports `input_tokens` / `cache_read_input_tokens` per turn and
# common.process_response accumulates them into running sums — it
# doesn't expose a per-turn field directly. Diffing the current
# cumulative total against this snapshot recovers THIS TURN'S
# (input + cache_read) count, which in Claude chat mode equals the
# size of the context sent to the model on this turn (and therefore
# the current session context fill — Claude is stateless, so every
# turn's input IS the full running conversation + cached prefix).
_last_snapshot: dict[str, int] = {}


def _human_tokens(n: int) -> str:
    if n >= 1_000_000:
        return f"{n / 1_000_000:.1f}M"
    if n >= 1_000:
        return f"{n / 1_000:.1f}k"
    return str(n)


def _ctx_color(pct: float) -> str:
    """Color the context-fill number by urgency — the restart signal.

    <50%  dim    — plenty of room
    50-80 plain  — fine
    80-95 yellow — think about restarting soon
    95+   red    — restart now
    """
    if pct >= 95:
        return C_RED
    if pct >= 80:
        return C_YELLOW
    if pct >= 50:
        return C_RESET
    return C_DIM


def print_footer(
    session_start: float,
    total_cost_usd: float,
    totals: dict[str, int],
) -> None:
    """One-line status after each SDK turn.

    Signature matches common.print_footer for drop-in replacement.
    `total_cost_usd` is accepted but not displayed.

    Layout:  ctx XX.X% · turn N · in Xk · out Xk · HH:MM:SS
    """
    global _last_snapshot

    # Derive this turn's context size by diffing cumulative totals
    # against the previous snapshot. In Claude chat mode the SDK
    # sends the full running conversation as input on every turn
    # (the API itself is stateless), so per-turn (input + cache_read)
    # = context bytes the model processed this turn ≈ current
    # session context fill. That's why `ctx_pct` below is the
    # restart-signal mentioned in the module docstring — not a
    # per-turn "how much did we add" figure.
    cur_in = totals.get("input_tokens", 0)
    cur_cr = totals.get("cache_read_input_tokens", 0)
    prev_in = _last_snapshot.get("input_tokens", 0)
    prev_cr = _last_snapshot.get("cache_read_input_tokens", 0)
    turn_ctx_size = (cur_in - prev_in) + (cur_cr - prev_cr)
    turns = _last_snapshot.get("_turns", 0) + 1

    _last_snapshot["input_tokens"] = cur_in
    _last_snapshot["cache_read_input_tokens"] = cur_cr
    _last_snapshot["_turns"] = turns

    pct = 100.0 * turn_ctx_size / CTX_BUDGET if CTX_BUDGET else 0.0
    color = _ctx_color(pct)

    elapsed = int(time.monotonic() - session_start)
    h, m, s = elapsed // 3600, (elapsed % 3600) // 60, elapsed % 60

    t_in = cur_in
    t_out = totals.get("output_tokens", 0)

    # Silence unused-arg warnings; kept in signature for compatibility.
    _ = total_cost_usd

    print(
        f"{C_DIM}  {C_RESET}{color}ctx {pct:4.1f}%{C_RESET}"
        f"{C_DIM} · turn {turns} · "
        f"in {_human_tokens(t_in)} · out {_human_tokens(t_out)} · "
        f"{h:02d}:{m:02d}:{s:02d}{C_RESET}"
    )
