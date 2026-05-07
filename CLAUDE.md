# Alor — Claude Code worker brief

**Note for the dispatched-worker case:** if you're reading this because Alor dispatched you a TASK BRIEF, also read `AGENTS.md` at the repo root. It has the worker dispatch contract (verdict-paragraph reporting, user-rule arbitration, no walkthroughs). This file is for direct work *on* Alor itself.

## What Alor is
Multi-agent orchestration platform. Tauri/Rust app with a Python (claude-agent-sdk) orchestrator and worker layer, talking over a Unix socket and tmux. Renamed from Vaelkor 2026-04-14. Repo branches: `dev` (working — default target), `main` (published).

## Workspace layout
- `src-tauri/` → binary `alor`, lib `alor_lib` (Tauri app + daemon).
- `wrapper/` → `alor-wrapper`, the per-agent tmux bridge for non-SDK agents (gemini, cursor, codex).
- `cli/` → `alor-cli`, every command talks to the daemon socket.
- `orchestrator-py/` → orchestrator REPL (`main.py`) and Claude SDK worker (`worker.py`).
- `src/` → Tauri frontend (vanilla JS: `TaskList.js`, `AgentPanel.js`, `Terminal.js`, `toast.js`).
- `audit/` → snapshot audit reports per agent.
- `proto/alor_protocol.yaml` → wire protocol reference.

## Core contracts

**Component roles.** Daemon = Cognitive Kernel. UI = War Room. Wrapper = agent-side bridge (idle + intervention detection). Orchestrator = router (no Read/Edit/Bash/Grep — judgment by tool absence).

**Runtime split.** Workers run `runtime: claude-sdk` (Python SDK, structural readiness, no scrape) or `runtime: wrapper` (tmux scrape with idle regex). One yaml field flips it.

**Templates vs slots.** A yaml with `template: true` is a recipe — not a slot. `agent_spawn(agent="claude", project=..., working_dir=...)` parameterizes into a runtime instance whose template back-pointer is persisted in `state.json`. Core yaml slots are tombstoned via Kill (disconnect, keep row); template instances support both Kill and Delete.

**tmux target grammar.** `=` sigil forces exact session match — but only on session-target commands (`has-session`, `kill-session`, `attach-session`). Pane-target commands (`send-keys`, `capture-pane`) need `=name:`. `set-option` rejects `=` entirely; relies on `handle_spawn`'s collision guard. See Obsidian `File Map.md` for the cheatsheet — getting this wrong silently no-ops or hits the wrong session.

**Echo guard.** `agent_send_message` from orch → claude-sdk worker prepends `WORKER_ECHO_SENTINEL`; worker's stdin_loop strips it and suppresses the `worker.user_input` event for that line. Constant kept in lockstep between `src-tauri/src/wrapper/server.rs` and `orchestrator-py/worker.py` — change one, change the other.

**Staged approval.** No silent writes. Code changes go `PROPOSED` → `STAGED` (Logic Brief + Diff) → user approves via Tauri panel or REPL → applied. Only the orchestrator writes project profiles.

## Build / run
- `cargo tauri dev` — full app (UI + daemon + builds all crates).
- `cargo build -p alor-cli` / `-p alor-wrapper` — rebuild a single crate.
- `cargo test --workspace` — Rust tests (~236 markers).
- `orchestrator-py/run_tests.sh` — Python tests (13 files).
- `orchestrator-py/run.sh` — orchestrator REPL by hand.

Release profile (LTO, codegen-units=1, opt-level="s", panic=abort, strip) is at the workspace root — `[profile.*]` in member crates is silently ignored by cargo, don't move it.

## State
- `~/.config/alor/` — agents yaml, rules.md, orchestrator_prompt.md, integrations.yaml.
- `~/.local/share/alor/` — `state.json` (tasks + agents w/ template back-pointers; atomic write_all+sync_all+rename), `tasks-archive.json`, `wrapper_pids.json`, `hubs/<project>/`.
- `/tmp/alor/daemon.sock` — daemon socket.

## Known traps
- `[profile.release]` only honored at workspace root.
- `claude-alor` is a fixed slot, not a template (despite the naming pattern).
- No agent `autolaunch` on boot — Fett "Start Brain" launches the orch, orch brings workers up on demand via `agent_ensure_running`.
- `STALE` is terminal; non-terminal `STALE` causes `max_concurrent` deadlocks.
- Worker `client_lock` held across the whole task by default — stdin follow-ups and `/reset` block. There is a known fix (asyncio.Queue) but check current state before re-fixing.
- Anthropic doesn't support custom agents on Pro/Max via Rust or raw HTTP — Python SDK is the only path. Don't try to port the orch off claude-agent-sdk.

## Documentation pointers
- Obsidian `Personal/Alor/File Map.md` — module/file map, tmux grammar cheatsheet, CLI/Tauri/MCP command reference. Most accurate single doc.
- Obsidian `Personal/Alor/Current State.md` — last detailed phase report (slightly behind HEAD; use git log for precise current state).
- Obsidian `Design Doc.md` — Vaelkor-era. Do not cite.
- `audit/` — snapshot audits per agent.
- `AGENTS.md` — worker dispatch rule (only relevant when running as a dispatched worker).
- `~/.claude/projects/-home-fett/memory/project_alor.md` — Fett's curated brief.

## Reporting style
This repo's owner (Fett) prefers forward-directive density over progress narration. Don't write phase changelogs into curated docs (those go in Obsidian Current State or git log). When closing a task, lead with the verdict.
