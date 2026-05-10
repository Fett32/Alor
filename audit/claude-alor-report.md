# Alor Full Audit — claude-alor

**Date:** 2026-04-19
**Auditor:** claude-alor (Claude Opus 4.7, 1M context)
**Scope:** Rust/Tauri core, Python orchestrator, frontend, configs, docs.
**Method:** Static code review + doc cross-reference. 4 parallel Explore agents + direct spot-checks.
**Recon only — no code was modified.**

Repo size: ~19 KLOC across Rust (9.3k), Python (4.8k incl. tests), JS (~1k), plus docs.

---

## TL;DR

Alor is surprisingly mature for a solo project in this phase. Architecture is coherent (Rust daemon = authoritative state + IPC, Python = SDK-bearing workers, Tauri WebView = operator UI), the protocol defenses against orchestrator context bloat are sophisticated (summary/details split, echo-guard sentinels, worker_response LRU), and test coverage on the *hard* pieces (state machine, outbox persistence, frame wedge, tool gate) is excellent. The weakest strata are (1) the 3,700-line `wrapper/server.rs` god-module, (2) **unbounded `state.json` growth** because `archive_terminal_tasks` is wired in tests but never called at runtime, (3) doc drift — several Obsidian claims (Memory Hub unwired, STALE non-terminal) are already fixed in code but the docs don't reflect it, and (4) a handful of small but real concurrency gaps in the Python orchestrator layer. There are no panics, no obvious deadlocks, and no critical security holes for a local-only deployment.

---

## 1. Architecture Overview

### Process graph

```
 ┌─────────────────────────┐        unix socket        ┌──────────────────────────┐
 │ Tauri WebView (JS UI)   │◄──invoke/emit──►│  Rust daemon (src-tauri/)           │
 │ src/main.js + xterm     │                  │  ├─ AppState (tasks, agents)        │
 └─────────────────────────┘                  │  ├─ wrapper::server (IPC broker)    │
        ▲                                     │  ├─ terminal::pane_manager (tmux)   │
        │ PTY stream                          │  ├─ commands (Tauri surface)        │
        │                                     │  └─ persists state.json             │
        │                                     └──┬─────────────────────────────────┘
        │                                        │         /tmp/alor/daemon.sock
        │                                        │
 ┌──────┴──────┐  tmux attach       ┌─────────────┴────────────┐   ┌─────────────┐
 │ alor-main   │◄─── PTY ───────────│ wrapper binary (Rust)    │   │ worker.py   │
 │ tmux session│                    │ non-SDK agents (claude/  │   │ SDK agents  │
 │ (tiled)     │                    │  codex/gemini/cursor TUI)│   │ (Claude SDK)│
 └─────────────┘                    │ + idle detector          │   └──────┬──────┘
                                    └──────────────────────────┘          │
                                                                          │
                                                          main.py (Orchestrator, Claude SDK)
```

**Runtime modes:**
- **`claude-sdk` runtime** → Python `worker.py` subscribes via persistent socket, runs prompt via `claude-agent-sdk`, emits `MSG_TASK_COMPLETE` with summary/details.
- **`wrapper` runtime** → Rust `alor-wrapper` spawns a TUI agent in tmux, polls pane output with a per-runtime regex detector, emits completion on idle stability.

**Authoritative state** lives in `AppState` (Rust). Everything else is a view, a broker, or a compute-only worker.

**Key design moves that pay off:**
- Task/Agent *Summary views* project away heavy fields before wire/disk serialization — measured 10× payload reduction in tests.
- Atomic write-rename with fsync on every state mutation (multiple files use this pattern).
- `WorkerResponseCache` LRU (cap 100) + 2 KiB inject cap lets the orchestrator see terse summaries and pull full text on demand via `worker_response_get`. This is exactly the right architecture for long-running orchestrator context budgets.
- Dual-layer tool gating: MCP server-side role filter (`_tools_for_role`) AND call-time `can_use_tool` hook. Defense-in-depth.

---

## 2. Strengths

1. **Protocol rigor (wrapper/protocol.rs).** 28+ typed message constants, explicit context-bloat caps (`TASK_SUMMARY_MAX_BYTES=512`, `EVENT_TEXT_INJECT_MAX_BYTES=2048`, `TASK_LIST_FULL_MAX_LIMIT=50`), `#[serde(default)]` on new fields for forward-compat, UUID-seeded echo-guard sentinels.
2. **State machine rigor (state.rs).** 13 explicit `TaskState` variants, `can_transition_to` gate on every mutation, 30+ tests enforcing terminal bookkeeping.
3. **Persistence discipline.** `save()` writes temp → `sync_all` → atomic rename in every module (state, project, config, session). No torn writes on power loss.
4. **Terminal subsystem.** Real PTY via `portable-pty` (no capture-pane polling), incremental streaming, 5 s write timeout to prevent Tauri runtime stalls, X11+Wayland PRIMARY mirroring via tmux copy-pipe with propagated DISPLAY/WAYLAND_DISPLAY env.
5. **Outbox persistence (agent_client.py).** Workers persist failed sends to JSONL, replay FIFO on reconnect, caps enforced (100 entries / 1 MiB). Task completions survive daemon restarts.
6. **Frame-wedge recovery (worker.py + test_frame_wedge.py).** Nested `BEGIN` on orch→worker stdin framing is detected, the stale frame is discarded, `worker.frame_wedged` event fired, awaiting caller gets `FrameWedgedError`. Well-tested.
7. **Tool gate rigor (tool_gate.py).** Blocks worker→orch `agent_send_message` (prompt-injection vector), restricts worker `agent_spawn` to generic templates with `debug-`/`test-` name prefixes, enforces explicit `name` argument. 612 LOC of tests.
8. **Reconcile loop (pane_manager.rs + reconcile_integration.rs).** Periodic safety net for "agent says connected but tmux session is gone" (zombie) — actual integration test that spins up tmux.
9. **Signal handling discipline.** SIGTERM → graceful shutdown, SIGHUP → ignored (survives daemon restarts) in orch and worker alike.
10. **Summary/details split (worker.py).** 400 B terse paragraph → orchestrator context, rest → server-side `details` field fetched via `worker_response_get`. Directly addresses the cheapest-scaling-axis of orchestrated agent systems.

---

## 3. Concerns

### 3.1 High — architectural / operational

**C1. `state.json` is unbounded.** `archive_terminal_tasks` exists, is correct, is tested (state.rs:2526), and is marked `#[allow(dead_code)]` at state.rs:723. `lib.rs:63` has a comment explicitly noting "Previously this site ran `archive_terminal_tasks` on every boot" — it has been **disabled**, not replaced. On multi-week sessions the HashMap grows without bound; every mutation serializes the whole thing. Likely noticeable at a few thousand terminal tasks. **Fix: re-enable on boot and/or wire to a periodic interval.**

**C2. `wrapper/server.rs` is 3,708 LOC.** Handles socket accept, registration handshake, message dispatch, CLI command dispatch, spawn/kill lifecycle, and event broadcast in one file. No obvious bug from the size alone, but it's the module most resistant to change and the thing that will break first under feature pressure. Recommendation: extract `handle_cli_message` (~1,200 LOC) and `handle_spawn` (~300 LOC) into sibling modules.

**C3. No protocol version field in `Envelope`.** Forward-compat is handled correctly via `#[serde(default)]` on additive fields, but a breaking shape change will silently corrupt older wrappers. Low risk today (single deployment) but the design principle is worth adding before external users.

### 3.2 Medium — bugs / race conditions / leaks

**C4. `worker.py:702–711` cancellation swallows real errors.** The cleanup block `except (asyncio.CancelledError, Exception)` catches bugs in `daemon_loop`/`stdin_loop` along with cancellation. Programming errors become silent reconnect loops. **Fix: split into `except asyncio.CancelledError` and explicitly re-raise or log other Exception.**

**C5. `worker.py:291` daemon_loop catches bare `Exception`** then loops back to reconnect. Same pathology — AttributeError/TypeError are treated as transient network errors. A broken build will just reconnect forever.

**C6. `agent_client.py._enqueue` is not lock-protected.** `send()` (async) and `_load_outbox_from_disk()` (sync-in-ctor) both mutate the deque; cap enforcement is TOCTOU-vulnerable. In practice workers are single-task-at-a-time so this hasn't bitten, but under any multi-task future it will.

**C7. `recv_forever()` / `event_stream()` silently skip malformed envelopes.** stderr log only. A corrupt outbox or a daemon bug emitting garbage will hang a worker in an invisible loop.

**C8. Tauri `kill_all_agents` uses `pkill -f alor-wrapper` (commands.rs:331).** SDK workers aren't `alor-wrapper`, they're `python worker.py`. The kill-all misses them. Per-agent tmux kill (which the same function also does) is the actual effective path, so `pkill` is belt-and-suspenders at best and misleading at worst.

**C9. Subscriber broadcast holds per-client writer locks during `write`+`flush` (server.rs:2385).** Outer snapshot-and-drop pattern is correct; this is per-client. One slow Unix-socket client will delay others by its drain time. Typically trivial; becomes real if any subscriber is a pipe over a slow link.

**C10. `send_to` in server.rs:1155 has a lookup-then-send TOCTOU** (`writers.contains_key` → `writers.get_mut`). In practice safe because disconnects only occur in `handle_connection`'s drop path, but the pattern is a footgun for future edits — make the lookup return the writer under one lock acquire.

### 3.3 Medium — security / coupling

**C11. Agent-kind inference from name substring (wrapper/src/main.rs:282).** If a yaml slot named "alor-main-worker-claude" wraps a Codex binary, Claude idle regex runs → completion never fires. `--command` flag overrides, but no assert that agent name ↔ runtime are consistent.

**C12. Trust-prompt auto-ack in wrapper runs at startup only (main.rs:325).** Safer than on-every-poll. Still: if a user runs `grep "Do you trust"` in the pane within the startup window, wrapper would send `1`. Tighter: match against the exact framed prompt box, not the bare substring.

**C13. `assign_task` silently falls back to raw description on profile load failure (commands.rs:80–88).** Warn log only; operator doesn't see degraded context on the UI. User thinks the task has the full brief; it doesn't.

**C14. Event-type coupling (main.py).** `INJECTABLE_EVENTS = {...}` is a hardcoded allowlist in the orchestrator. Add a new event type in Rust → orch ignores it until the Python set is updated. Good: fails closed. Bad: no schema-level contract between emitter and consumer.

**C15. Summary sizing constants duplicated.** `worker_prompt.md` says "≤ ~400 bytes", `worker.py` has `TERSE_TARGET = 400`. These will diverge. Single-source-of-truth in `protocol.rs` or a shared constants module.

**C16. CSP is permissive (`unsafe-inline`).** Fine now (all dynamic content goes through `textContent`), but before the Staged Approval panel starts rendering diff content, tighten to nonce-based inline styles only.

**C17. `MSG_USER_INTERVENTION` agent_id comes from the bound connection** (good per File Map) — but document this explicitly in `protocol.rs` because a reader of the wire format will assume the `agent_id` field is the source of truth.

### 3.4 Low — nits / polish

- `config.rs:20` `dirs_home` falls back to `/tmp` if HOME is unset. Reasonable but can mask misconfiguration.
- `codex.yaml` and `codex-reviewer.yaml` are byte-identical (per docs) — `use_for` at least should differ.
- `autoconnect` / `constraints` yaml fields are silently ignored (not in `AgentConfig`).
- `agent_client.close()` swallows close-time errors without logging (agent_client.py:571).
- Task `summary` field is persisted and broadcast, but the TaskList UI never renders it.
- `user_intervened` flag is informational only — doesn't gate completion. By design, worth documenting in orchestrator-facing docs.
- Agent ID on register has no length bound — 10 KiB names would bloat state.json (not exploitable, just untidy).

---

## 4. Doc Drift (vs current code)

I directly spot-checked the most consequential drift claims from the Obsidian `Audit 2026-04-16.md` and `Review Resume 2026-04-16.md`:

| Claim in docs | Status in current code | Evidence |
|---|---|---|
| "STALE is non-terminal → max_concurrent deadlock" | **FIXED.** `TaskState::Stale` is in `is_terminal()` | state.rs:55 |
| "`link_agent_memory` has zero callers" | **FIXED.** Wired in server.rs | server.rs:1442 |
| "`wrapper.error` never broadcast" | Fixed per audit sub-agent. | server.rs MSG_ERROR arm |
| "Subscriber lock held across write" | Fixed (snapshot-drop pattern) | server.rs:2364 |
| "Summary truncation uses `chars()` not bytes" | Fixed | worker.py TERSE_TARGET logic |
| "`record_user_intervention` only matches Accepted" | Fixed (covers Proposed/Staged/Blocked/Recovering) | state.rs:1336 |
| "Atomic write has no fsync" | Fixed (write_all + sync_all + rename) | state.rs:678 |
| "Template collision guard missing" | Fixed | server.rs `handle_spawn` |
| "Delete action for template instances" | **NOT DONE.** `state.remove_agent()` doesn't exist. | — |
| "Task-time TASK BRIEF templating (key_files → startup)" | **NOT DONE.** Wrapper only reads static `startup_file`; no assign-time templating. Commands.rs builds a prefix via `build_task_brief()` for the description only. | project.rs, commands.rs:80 |
| "Memory Hub integration live" | Partial — `link_agent_memory` is wired on registration, but cross-agent mount/symlink story isn't covered end-to-end in tests. | memory.rs has no filesystem tests |
| "`MSG_CLI_ASSIGN` broadcasts before persist" | Did not verify in this pass — worth re-checking | — |
| "Path traversal in project_name" | project.rs validates names against a regex; worth a second read for CLI-originating paths. | project.rs:90 |

**Docs that need touching up:**
- `Design Doc.md` still reads "Vaelkor" in places and references PySide6 UI (replaced by Tauri). Architecturally still correct; naming is stale.
- `Current State.md` (2026-04-17) is the most current but overclaims Memory Hub completeness and TASK BRIEF automation.
- `Audit 2026-04-16.md` findings list is partially obsolete — many items are fixed. Worth annotating in place with ✅/❌ so it becomes useful history rather than a stale TODO.
- `File Map.md` is accurate and the tmux-target grammar table is a real asset. Keep it.

---

## 5. Test Coverage Gaps

**Well covered:**
- Task state machine (1,500+ LOC of state.rs tests).
- Summary projection & size ratios.
- Outbox persistence / caps / FIFO eviction.
- Tool gate (612 LOC — probably the best-tested single file).
- Frame-wedge recovery (2 focused test files).
- Paste-guard sanitization (ANSI, C0, zero-width).
- Reconcile zombie path (real tmux integration).

**Gaps:**
1. **No end-to-end test of `task.completed` → event inject → `worker_response_get`** round-trip. This is the hottest orchestrator path.
2. **No async test of `daemon_loop` reconnect + outbox flush.** Outbox unit tests are solid; the loop that actually uses it isn't exercised.
3. **No test of `event_watcher` cancellation / exception propagation** (main.py).
4. **No test of SDK `client_lock` contention** (concurrent `query()` + event inject).
5. **No server-side integration tests of `handle_message` / `handle_cli_message`.** The 3.7 KLOC file has inline unit tests (2612–2999) but no over-the-wire e2e.
6. **No CLI e2e.** `alor task create` → daemon → state.json → response is manual-tested only.
7. **No protocol fuzz or round-trip of envelope + type matrix.** Individual payloads are round-tripped (TaskComplete with/without details), but not the full envelope × message-type cartesian.
8. **No filesystem-level test of `link_agent_memory`.** Logic-only tests; real symlink collisions / `.alor_backup` handling is not exercised.
9. **No persistence round-trip test under concurrent mutations.** `AppState` tests are single-threaded.
10. **No signal-handler tests** (SIGTERM/SIGHUP) in worker/orch.

---

## 6. Top 10 Concrete Improvements (ranked by impact)

| # | Change | File(s) | Why |
|---|---|---|---|
| 1 | **Re-enable `archive_terminal_tasks` on daemon boot** (and optionally a 12 h interval) | `lib.rs:63`, `state.rs:724` | Unbounded `state.json` growth is the clearest operational footgun. ~20 LOC fix, tests already exist. |
| 2 | **Split `worker.py` cancellation handling** — `except CancelledError` vs `except Exception` with log+raise | `worker.py:702–711`, `:291` | Currently hides real bugs as "network errors". High debug-time ROI. |
| 3 | **Build task-time TASK BRIEF templating** from `key_files` + `doc_paths` at assign time | `commands.rs:80`, `project.rs`, wrapper startup path | Design promised it, Current State claims it, wrapper only reads static `startup_file`. Close the loop or amend the doc. |
| 4 | **Factor `wrapper/server.rs`**: extract `handle_cli_message` + `handle_spawn` to sibling modules | `wrapper/server.rs` | 3.7 KLOC god-module is the biggest future-drag in the repo. Mechanical refactor, no behavior change. |
| 5 | **Add protocol version field to `Envelope`** (default `1`) and verify on register | `wrapper/protocol.rs` | Cheap insurance against silent breakage when the wire format changes. |
| 6 | **Surface degraded-brief fallback to the operator** when `build_task_brief` errors | `commands.rs:80–88` | Today it's a warn log; operator thinks task has full context but doesn't. |
| 7 | **Implement `state.remove_agent` + delete-button wiring for template instances** | `state.rs`, `commands.rs`, `AgentPanel.js` | Outstanding from 2026-04-16 pre-boot list; UI has stubs. |
| 8 | **Lock `agent_client._enqueue`** (asyncio.Lock around cap/push/persist) | `agent_client.py:156` | Small fix now; painful race if workers ever go multi-task. |
| 9 | **Render `task.summary` in TaskList.js** (details-on-expand; fetch via `task_get`) | `src/components/TaskList.js` | Data is already on the wire; UI ignores it. Also needed for Staged Approval panel. |
| 10 | **Single-source summary sizing constants** — pull `TERSE_TARGET`/`DETAILS_MAX`/`INJECT_MAX` into `protocol.rs` and re-export | `wrapper/protocol.rs`, `worker.py`, `worker_prompt.md` | Three places today; will diverge. |

---

## 7. Other Notes

- **`archive_terminal_tasks` tests exist** (state.rs:2526+, sweeps only terminal states, idempotent). The fact that the function is well-tested but unwired is itself a signal — someone wrote it, validated it, then rolled back the call site at `lib.rs:63` and left the comment. Worth asking *why* before re-enabling; there may be a reason (archive path collision? migration concerns?) that should be captured as a runtime config rather than hard-disabling.
- **Claude-SDK `client_lock` lifecycle** — `main.py` documents that the SDK client isn't thread-safe and holds a single `asyncio.Lock` across every query and event inject. This means a slow response blocks event injection. If that ever becomes a UX problem, the answer is probably to move injection to an outbox-style queue rather than synchronous.
- **`codex.yaml` ≡ `codex-reviewer.yaml`** (byte identical) — smells like they're supposed to diverge on `use_for`, `model`, or `system_prompt`. Trivial to fix but worth deciding what "reviewer" means semantically.
- **`link_agent_memory` has no integration test.** Unit tests the logic; does not actually create symlinks on a tempdir. First real filesystem collision (`.alor_backup` path) will be a manual debugging session.
- **Wrapper binary auto-ack logic** is clever but fragile. If Anthropic, OpenAI, or Google change their first-run prompts, the trust-ack flow silently breaks. Consider a one-time manual-ack mode gated by a config flag rather than always-on scanning.
- **The Python test suite is genuinely good** — 2.8 KLOC across 9 files. Focused on the hard stuff (outbox, frame-wedge, tool gate, paste guard). This is above the bar for a solo project at this phase.
- **No CI configuration** was examined in this pass; `.github/workflows` should be checked to confirm the test suites are actually run on every change.
- **`Cargo.toml` release profile** does LTO + strip + abort-on-panic. Good for binary size, but abort-on-panic means any reachable panic path becomes a daemon kill. State.rs production paths are panic-free; worth a focused audit of wrapper/server.rs message handlers for any panicking `unwrap`s before the next cut of the release binary.

---

## 8. Final Verdict

**Ship-quality for the current operator-of-one use case.** Nothing in this audit would stop Fett from dogfooding Alor tomorrow. The architecture is well-chosen, the hot paths (state machine, persistence, framing, outbox) are hardened, and the test suite covers the parts that are actually hard to get right.

The three improvements that would move the needle most are (1) re-enable `archive_terminal_tasks` before `state.json` grows teeth, (2) fix the two `except Exception` swallowers in `worker.py`, and (3) refactor `wrapper/server.rs` before the next feature round makes it worse.

The doc layer lags the code — several "open" items in the April 16 audit are already shipped. Before the next planning session, sweeping the Obsidian docs with ✅/❌ annotations would save orchestrator-dispatched agents from chasing fixed bugs.
