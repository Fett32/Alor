# Alor codebase audit — cursor-alor report

**Auditor:** Cursor agent (single pass, no other reviewers assumed).  
**Date:** 2026-04-19  
**Repo root:** `/home/fett/Projects/Alor`  
**Scope:** Rust/Tauri (`src-tauri/`, `wrapper/`, `cli/`), Python (`orchestrator-py/`), frontend (`src/`), configs, CI, and cross-check against `~/Projects/Obsidian Vault/Personal/Alor/`.  
**Method:** Static review (read source, grep tests/workflow, compare docs vs code). No runtime dogfood or benchmarks.

---

## 1. Architecture overview

Alor is a **local-first cognitive kernel**: a Tauri desktop shell hosts the “daemon” (task registry, agents, Unix socket API, tmux/pane orchestration) while separate processes implement **agent runtimes**.

**Layers (top to bottom):**

1. **Frontend (`src/`)** — Vanilla ES modules + Vite. Talks to Rust via Tauri `invoke()` and listens for push events (`tasks-changed`, `agents-changed`, `terminal-output`, etc.). Renders task list, agent roster, and an xterm.js view into `alor-main`.
2. **Tauri app + daemon (`src-tauri/`)** — `lib.rs` boots tracing, loads `~/.config/alor` agent YAML, hydrates `AppState` from `state.json`, runs optional archive migration, spawns the **Unix socket server** (`SocketServer` on `/tmp/alor/daemon.sock`), pane manager, PTY bridge, tray, and several asynchronous recovery loops (wrapper reclaim, pane reconciliation, PTY relay).
3. **Wire protocol** — Newline-delimited JSON **envelopes** (`type`, `correlation_id`, `payload`). Two families: persistent **wrapper/worker** sessions (register → stream) and one-shot **`cli.*`** RPCs. Rich **event broadcast** fan-out for orchestrator subscribers (`cli.event.stream`).
4. **Agent runtimes**
   - **`runtime: wrapper`** — `alor-wrapper` (`wrapper/`) drives a per-agent tmux session, tails output, idle-detects completion, reconnects to the daemon, and reports task lifecycle messages.
   - **`runtime: claude-sdk`** — `orchestrator-py/worker.py` + `agent_client.py` speak the same registration/task protocol from Python (Claude Agent SDK inside tmux for visibility).
5. **Orchestrator (`orchestrator-py/main.py`)** — Claude-powered router with a **constrained MCP tool surface** (`tools.py` → `daemon.py`). Subscribes to daemon events and **injects** selected events into the SDK context under a shared lock (deadlock/latency tradeoff by design).
6. **CLI (`cli/`)** — Thin one-shot Unix socket client for humans/scripts; duplicates envelope structs locally.

**Data paths:** Config under `~/.config/alor/`, durable state under `~/.local/share/alor/` (per `session.rs` conventions), socket under `/tmp/alor/`. Project profiles and memory hub live beside state (see `File Map` notes in Obsidian).

**Mental model that fits the code:** tmux is **infrastructure for human visibility and some runtimes**, not the authority for task truth. The daemon’s `AppState` + socket routing are authoritative.

---

## 2. Strengths

- **Clear separation of concerns** between UI, daemon routing, wrapper scraping, and SDK workers — the `runtime: wrapper | claude-sdk` split is a pragmatic dual stack.
- **Operational rigor in hot paths:** summary/details split for completions, task list/get **views** to bound token bloat, UTF-8-safe truncation, structured `cli.error` codes (e.g. framed send unsupported), inbox/outbox patterns for worker reconnects, and explicit comments where race windows were considered (e.g. pane reconcile timing).
- **Strong inline documentation** in Rust and Python — many files read like engineering notes, not bare code. That lowers onboarding cost and explains *why* oddities exist (tmux `=` prefix rules, Sway placement, etc.).
- **Automated tests exist where it hurts most:**
  - Large `#[cfg(test)]` surface in `wrapper/server.rs` and `daemon/state.rs`.
  - `src-tauri/tests/reconcile_integration.rs` exercises pane reconciliation against real tmux when available.
  - `wrapper/src/detector.rs` has substantive regex/window tests (including Cursor-specific fixtures).
  - Python uses self-contained `test_*.py` scripts + `run_tests.sh` wired in GitHub Actions.
- **UX depth for a developer tool:** terminal bridge work (PTY, primary selection bridging X11 + Wayland, mouse forwarding) shows rare polish for a Tauri+tmux stack.
- **Security basics for local IPC:** daemon socket permissions set to `0600` after bind.
- **Obsidian “File Map” and “Current State”** are unusually accurate engineering maps compared to typical README drift; they describe the multi-crate layout and template/instance model well.

---

## 3. Concerns

### 3.1 Complexity and maintainability

- **`wrapper/server.rs` is very large** (thousands of lines). It concentrates CLI routing, spawn orchestration, event fan-out, task transitions, and edge-case policy. This is a **single point of cognitive load** — refactors are high-risk without broader integration coverage.
- **Protocol triplication:** message constants and shapes live in `src-tauri/.../protocol.rs`, `wrapper/src/protocol.rs`, and again in Python (`daemon.py`, `agent_client.py`). Comments enforce “lockstep,” but **humans drift** — a schema or codegen would reduce class-of-bugs.

### 3.2 Concurrency, races, and consistency

- **`assign_task` (Tauri)** checks `is_connected` then sends; a disconnect in between can still yield **Stale** tasks — acceptable if treated as normal, but UI/users should understand it as “best-effort dispatch.”
- **Orchestrator + worker** share `asyncio.Lock` around SDK turns while stdin/event injection competes — documented tradeoff; long-running `client.query` still **blocks** inject path latency.
- **Wrapper runtime** completion remains ** Heuristic** (idle tail). SDK path removed polling for Claude, but wrapper agents still embody benign race stories (already heavily discussed in historical audit notes).

### 3.3 Safety and operational sharp edges

- **`kill_all_agents`** is intentionally nuclear (`pkill -9` patterns, tmux session kills). Mis-click or script bug is painful; worth treating as a “dangerous operation” with stronger UX affordances (typed confirm, recap list).
- **Local trust model:** Unix socket is user-private (`0600`), which is fine for a single-user workstation narrative. **Any process as the same UID** can own the orchestrator — document that this is not a multi-tenant security boundary.

### 3.4 Frontend / UX friction

- **Vanilla JS at ~500+ LOC per panel** (`TaskList.js`) is manageable today but will **sprawl** as features accumulate; no component framework, limited type-checking (JSDoc hints only).
- **Error handling** is often `console.*` + `alert()` — fine for internal tooling, but noisy for routine failures (daemon down, permission errors).
- **Settings** (`spawn_workspace`) persist immediately but **apply on next launch** — correct per code comments, easy to confuse users without inline help text.
- **Agent register form** overloads “role” into the `spawn_agent` path (`AgentPanel.js`) — works but reads slightly **semantically tangled** for newcomers (name vs role vs kind).

### 3.5 Documentation drift (Obsidian vs repo)

- **`Design Doc.md` still titles “Vaelkor”** and describes a **PySide6** UI — pre-architecture pivot. Treat as historical; **do not** treat as current implementation truth.
- **`Current State.md` (2026-04-16)** claims a **lean `state.json`** with tasks moved to `tasks-archive.json` — the current `lib.rs` narrative describes **rehydrating** the legacy archive into live state and **stopping boot-time pruning** so completed tasks stay queryable. The **direction of travel changed**; “archive as cold storage” vs “everything in live state” should be reconciled in docs.
- **`File Map.md`** remains the best high-level map; minor details (exact task state diagrams, idle tail sizes) may lag — prefer reading enums and `AgentKind` in code.

### 3.6 Repo hygiene

- **`orchestrator-py/alor_footer.py`** is imported by `main.py` and `worker.py` but appeared **untracked** in git status at audit time — risk of **fresh clone breakage** if omitted.

---

## 4. Test coverage gaps

| Area | Observation |
|------|-------------|
| **Rust CI** | Only **`.github/workflows/python-tests.yml`** present. No automated `cargo test` / `clippy` / `fmt` gate on push — regressions in daemon can slip in silently. |
| **`alor-cli`** | No unit tests found; CLI struct drift (e.g. template spawn flags) is easy. |
| **End-to-end daemon** | Integration tests cover **pane reconcile** paths well; **full socket conversation** tests are mostly embedded in `server.rs` unit tests — valuable, but a **black-box** harness (spawn server binary + scripted client) would catch wiring regressions across crates. |
| **Frontend** | No automated UI/component tests; manual verification only. |
| **Python** | Good targeted tests (framing, gates, outbox, events); **not** a full orchestration E2E against a running Rust binary in CI (understandable CI complexity). |

---

## 5. Top 10 concrete improvements (ranked by impact)

1. **Add Rust CI** — `cargo test -p alor_lib`, `cargo test -p alor-wrapper` (or workspace), optionally `clippy`. Highest ROI for preventing daemon regressions.
2. **Modularize `server.rs`** — split by concern (`cli/tasks.rs`, `cli/agents.rs`, `events.rs`, `spawn.rs`) behind thin `SocketServer` facade; keep behavior identical, reduce review burden.
3. **Protocol contract artifact** — single JSON Schema or proto for envelope kinds + payloads; generate Rust/Python constants or add a `cargo xtask` consistency check in CI.
4. **`alor-cli` parity for template spawns** — add `--project` / `--working-dir` to match daemon capabilities documented in Obsidian; reduces “only Python can spawn instances” friction.
5. **Task volume strategy** — `TaskList.js` already paginates DOM; **`state.json` growth** still affects parse/save cost. Implement archival policy or periodic compaction consistent with product goals (Obsidian hints at this tension vs “full history in live state”).
6. **Python SDK watchdogs** — bounded-time waits around `process_response` / tool loops where a stuck SDK call **blocks event injection** indefinitely.
7. **Harden `kill_all_agents` UX** — second-step confirmation, list affected sessions, or “copy command for shell” pattern; align with how destructive it is in code.
8. **Frontend ergonomics** — lightweight state layer or TypeScript + shared types from Rust (`ts-rs` or openapi from CLI) to reduce stringly-typed IPC mistakes.
9. **Documentation reconciliation** — rename or stamp **Vaelkor** design doc as archive; add a one-page **“Alor current architecture”** that matches `lib.rs` + `File Map` (including archive migration story).
10. **Observability** — structured metrics or a debug **`alor doctor`** command (socket reachable, tmux present, configs parse, disk paths writable) to cut support time for “why won’t it connect?”

---

## 6. Misc observations (outside the buckets)

- The project has **already absorbed at least one major audit cycle** (Obsidian `Audit 2026-04-16.md`); several “CRITICAL” items called out there (e.g. `wrapper.error` broadcast, `task.completed` transition gating) **appear addressed** in the current `server.rs` scan — good evidence of **fast corrective velocity**.
- **Wrapper `detector.rs`** quality is notably high for a notoriously flaky domain (TUI idle detection); continued investment here beats adding heuristics ad hoc in `main.rs`.
- **`KILL_ALL_AGENTS` + `pkill`** patterns make CI and sandboxed environments fragile — acceptable for personal tooling, worth calling out in contributor docs.
- **Tauri CSP** is typical for local app (`tauri.conf.json`); frontend connects to `ipc:` and dev `ws://localhost:*` — standard for Tauri 2 dev; ensure production bundle expectations stay aligned.

---

## References (key paths)

- Core boot: `src-tauri/src/lib.rs`
- State + persistence: `src-tauri/src/daemon/state.rs`
- Agent YAML: `src-tauri/src/daemon/config.rs`
- Socket server: `src-tauri/src/wrapper/server.rs`
- Wire types: `src-tauri/src/wrapper/protocol.rs`
- Tauri IPC: `src-tauri/src/commands.rs`
- Wrapper: `wrapper/src/main.rs`, `wrapper/src/detector.rs`
- CLI: `cli/src/main.rs`
- Python: `orchestrator-py/main.py`, `worker.py`, `daemon.py`, `agent_client.py`, `tools.py`, `common.py`
- Frontend: `src/main.js`, `src/components/*.js`
- Docs: `~/Projects/Obsidian Vault/Personal/Alor/` (especially `File Map.md`, `Current State.md`)

---

*End of report.*
