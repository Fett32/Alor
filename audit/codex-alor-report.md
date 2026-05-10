# codex-alor report

Audit date: 2026-04-19

Scope:
- Rust/Tauri core in `src-tauri/`
- standalone Rust crates in `wrapper/` and `cli/`
- Python orchestrator/worker layer in `orchestrator-py/`
- frontend in `src/`
- config and agent yaml files in `~/.config/alor/`
- project profiles in `~/.local/share/alor/projects/`
- docs in `~/Projects/Obsidian Vault/Personal/Alor/`

Method:
- static review of the code and docs
- config/profile cross-checking
- `cargo test --workspace`
- attempted `python3 -m pytest -q orchestrator-py` from repo root

Verification notes:
- Rust tests mostly pass, but one integration test fails in this environment: `reconcile_panes_adds_missing_pane_for_connected_agent_with_live_session` in `src-tauri/tests/reconcile_integration.rs` because `tmux new-session` returned `Operation not permitted`.
- Python tests were not runnable here because `pytest` is not installed in the ambient interpreter.
- The worktree is dirty: `git status --short` shows untracked `orchestrator-py/alor_footer.py`. That matters because both `orchestrator-py/main.py` and `orchestrator-py/worker.py` import it.

## 1. Architecture overview

Alor is a local multi-process coordination system with four main layers:

1. Rust/Tauri daemon and UI shell.
   - `src-tauri/src/lib.rs` boots persistent state, loads yaml agent configs, starts the Unix socket server, starts tmux pane reconciliation, and exposes Tauri IPC commands to the frontend.
   - `src-tauri/src/daemon/state.rs` is the real kernel: task state machine, agent registry, persistence, task summaries/details, worker-response cache, and spawn-leak tracking.
   - `src-tauri/src/wrapper/server.rs` is the integration hub. It owns the daemon socket, processes wrapper/worker envelopes, broadcasts event-stream updates, and implements the CLI RPC surface used by the Python layer.

2. Wrapper / worker execution layer.
   - Wrapper-runtime agents go through `wrapper/src/main.rs`: tmux session management, trust-prompt acknowledgement, idle detection, completion inference, and user-intervention detection.
   - Claude SDK workers go through `orchestrator-py/worker.py` plus `orchestrator-py/agent_client.py`: persistent daemon connection, framed orch-to-worker messages, direct Claude SDK queries, summary/details generation, and reconnect/outbox logic.

3. Python orchestrator layer.
   - `orchestrator-py/main.py` is a router REPL on top of `claude-agent-sdk`.
   - `orchestrator-py/tools.py` exposes the daemon RPC surface as an MCP server.
   - `orchestrator-py/daemon.py` is the typed Unix-socket client and event-stream client.

4. Frontend.
   - Plain JS/Vite UI in `src/` with three modules: agent panel, task list, terminal.
   - The frontend is thin; almost all policy lives in Rust or Python.

Overall, the intended design is coherent:
- Rust owns durable state and transport.
- Python owns orchestration and Claude-SDK execution.
- tmux provides the human-observable shared workspace.
- the frontend is a war-room shell, not the source of truth.

## 2. Strengths

- The boundary between durable state and orchestration logic is good. `AppState` plus the socket server is the right place for persistence, indexing, and cross-process invariants.
- The `summary` / `details` split is a solid fix for prompt bloat. The same is true of summary views for `task_get`, `task_list`, and `agent_list`.
- The event-stream model is strong. `worker.user_input`, `worker.orch_response`, and `worker.frame_wedged` make the system materially more observable than a naive task queue.
- The Python worker outbox in `orchestrator-py/agent_client.py` is well thought through. Disk-backed replay is the right answer for daemon restarts.
- The template/instance model is conceptually clean and already reflected in state, yaml, and prompt docs.
- The UI is intentionally thin. That is the right trade here because most correctness lives in backend transitions, not in client-side state.
- The wrapper detector has matured meaningfully. Cursor/Gemini/Codex support is much better than the older docs suggest, and there are good focused tests in `wrapper/src/detector.rs`.
- There is substantial unit coverage around the Rust state machine and protocol shaping. The recent audit-driven fixes were not just hand-waved; many are pinned by tests.

## 3. Concerns

### Critical

1. Frontend task assignment bypasses the task state machine and can create impossible task states.
   - In `src-tauri/src/commands.rs:46-109`, `assign_task` creates a `Task`, sets `assigned_to`, persists it via `state.add_task`, and sends `task.assign` directly to the agent.
   - It never calls `AppState::assign_task_to_agent` in `src-tauri/src/daemon/state.rs:1242-1278`, so the task stays `PENDING` instead of transitioning to `ASSIGNED`.
   - That breaks the legal transition path: when the worker later sends `task.accept`, the daemon tries `PENDING -> ACCEPTED`, which is illegal per `TaskState::can_transition_to` in `src-tauri/src/daemon/state.rs:91-120`.
   - It also bypasses agent history updates and `max_concurrent` enforcement.
   - Net effect: the frontend "assign to agent" path is semantically different from the CLI/orchestrator path and is currently wrong.

2. The same frontend assignment path silently fails to mark tasks stale on dispatch failure.
   - `src-tauri/src/commands.rs:73-77` and `99-103` attempt `state.transition_task(task_id, TaskState::Stale)` after a failed send or disconnected agent.
   - But the task is still `PENDING`, and `PENDING -> STALE` is illegal in `src-tauri/src/daemon/state.rs:93-99`.
   - Errors are discarded with `let _ = ...`.
   - Result: a failed frontend assignment can leave a task looking queued/assigned while never having reached the worker.

3. Worker execution failures leave tasks stuck active forever.
   - In `orchestrator-py/worker.py:161-173`, if `client.query(...)` or `process_response(...)` throws during task execution, the worker sends `wrapper.error` and returns.
   - The daemon's `MSG_ERROR` handler in `src-tauri/src/wrapper/server.rs:631-642` only broadcasts/logs the error; it does not transition the task to `BLOCKED`, `INTERRUPTED`, `TIMED_OUT`, or `CANCELLED`.
   - Because the task was already accepted at `orchestrator-py/worker.py:132-137`, it remains `ACCEPTED` indefinitely and continues to count against capacity.
   - This is one of the highest-risk correctness issues in the current stack because it creates silent slot starvation after real worker failures.

### High

4. The Tauri `spawn_agent` command ignores runtime selection and always launches the wrapper binary.
   - `src-tauri/src/commands.rs:191-210` always calls `force_launch_wrapper`.
   - `force_launch_wrapper` in `src-tauri/src/daemon/config.rs:228-279` always launches `alor-wrapper`.
   - That is incompatible with yaml entries using `runtime: claude-sdk` such as `~/.config/alor/agents/claude.yaml:1-10`.
   - The daemon-side CLI/orchestrator spawn path handles runtimes correctly in `src-tauri/src/wrapper/server.rs:2215-2319`; the Tauri IPC path does not.
   - This creates a split-brain control surface where "spawn from UI" is not equivalent to "spawn from orchestrator".

5. `project_save` is implemented as destructive replace, not patch, and it does not persist `memory_hub`.
   - In `src-tauri/src/wrapper/server.rs:1409-1475`, `MSG_CLI_PROJECT_SAVE` creates a fresh `ProjectProfile::new(...)` and fills only fields present in the request.
   - Any omitted field from an existing profile is dropped on save, including `notes`, `memory_hub`, and any future additive fields.
   - It then calls `link_agent_memory(...)`, but the resulting hub path is returned in the response only; it is never written back into the saved profile.
   - That is inconsistent with the apparent patch semantics of the Python wrapper and with the persisted profiles under `~/.local/share/alor/projects/*.yaml`.

6. The repo’s Python runtime currently depends on an untracked file.
   - `orchestrator-py/main.py:26` and `orchestrator-py/worker.py:38` import `alor_footer`.
   - `git status --short` showed `?? orchestrator-py/alor_footer.py`.
   - If that file is not committed, clean checkout behavior will diverge from this workspace and both entrypoints can fail at import time.
   - At minimum, this is release hygiene debt. In the worst case it is a hidden local-only dependency.

7. The default model names look malformed.
   - `orchestrator-py/main.py:29` and `orchestrator-py/worker.py:42` default to `"claude-opus-4-7[1m]"`.
   - The bracket syntax is suspicious and is not set by `run.sh` or `run-worker.sh`; those scripts just exec Python.
   - If no `ALOR_*_MODEL` env var is set, startup may depend on SDK-side tolerance for an invalid model identifier.
   - The comments in `orchestrator-py/alor_footer.py:25-27` suggest this may be context-window notation leaking into what should be a real model id.

8. The release optimization block is in the wrong manifest.
   - `cargo test --workspace` warns that `[profile.release]` in `src-tauri/Cargo.toml:41-46` is ignored because this is a workspace member, not the workspace root.
   - So the intended `panic = "abort"`, `lto`, `strip`, and `opt-level = "s"` settings are not actually authoritative for workspace builds.
   - This is not a logic bug, but it is a packaging/build correctness issue.

### Medium

9. The UI cannot create project-scoped tasks, so it cannot benefit from project briefs.
   - `src/components/TaskList.js:206-221` sends only `title`, `description`, and `agentId`.
   - Backend support exists for `project` in `src-tauri/src/commands.rs:46-53` and for project brief injection in `80-88`.
   - Today the frontend path structurally cannot use that feature.

10. The UI offers disconnected and non-worker agents in the assignment dropdown.
   - `src/components/AgentPanel.js:302-318` adds every agent to `#modal-agent-select`.
   - Combined with the broken frontend assignment path, this increases the chance of producing a task that looks assigned but never legally entered `ASSIGNED`.
   - It is also a UX smell: users should mostly be choosing connected worker slots, not templates/offline entries/orchestrator rows.

11. `project_save`/profile docs and implementation are drifting.
   - Current docs and saved profiles treat `memory_hub` as a first-class property.
   - The save handler never persists it.
   - This is not just a documentation problem; it means round-tripping a profile through the API can erase meaningful metadata.

12. The frontend still lacks durable visibility into the fields the backend was recently improved to preserve.
   - `TaskList.js` renders only title, state, assigned_to, and timestamp.
   - It does not surface `summary`, `details`, `proposal_brief`, `proposal_diff`, `user_intervened`, or task-spawn leak warnings, even though the backend now persists them.
   - The system is generating more useful state than the main UI can show.

13. One Rust integration test is not isolated enough from tmux environment constraints.
   - `src-tauri/tests/reconcile_integration.rs:72-153` correctly checks for `tmux -V`, but not for the ability to create a disposable tmux session in the current environment.
   - In this workspace it failed with `error connecting to /tmp/tmux-1000/default (Operation not permitted)`.
   - The test suite therefore conflates "tmux exists" with "tmux is usable here".

14. YAML/schema drift is still present around undeclared fields.
   - The yaml files in `~/.config/alor/agents/*.yaml` include `autoconnect` and `constraints`.
   - `AgentConfig` in `src-tauri/src/daemon/config.rs:28-75` does not parse them.
   - That is not catastrophic because serde ignores unknown fields, but it means part of the visible configuration surface is inert.

### Low / observational

15. `alor_footer.py`'s context indicator is labeled like a session-fill meter but implemented as per-turn delta.
   - `orchestrator-py/alor_footer.py:74-87` calculates `last_ctx` as the delta since the previous footer call, then prints `ctx XX.X%`.
   - The docstring and comments read like this is "how full the session is", but the implementation is "how much context this turn loaded".
   - That may be intentional, but if it is, the naming is misleading.

16. CSS has a small variable bug.
   - `src/style.css:123-127` references `var(--text)`, but only `--text-primary`, `--text-secondary`, and `--text-dim` are defined.
   - Low impact, but it means focused header-input text color is relying on fallback/inheritance rather than an explicit token.

## 4. Docs vs implementation

The docs are useful and mostly high-signal, but they are not fully in sync with the shipped code.

Main drifts I noticed:

- Memory Hub path drift.
  - Docs say `~/.local/share/alor/memory/` in `Current State.md:15` and `File Map.md:91`.
  - Implementation and live profiles use `~/.local/share/alor/hubs/<project>/` via `src-tauri/src/daemon/project.rs:80-85` and the current `~/.local/share/alor/projects/alor.yaml`.

- Worker Pool design doc is now historical, not current.
  - `Worker Pool.md:43` explicitly says no runtime template instantiation in Phase 2.5.
  - Current implementation and `Current State.md:53-60` absolutely do support template/instance spawning.
  - This is acceptable if treated as historical design context, but not if read as current behavior.

- `Current State.md` says daemon-restart survival shipped, and on that point the code matches.
  - `Current State.md:82-84` lines up with the reconnect logic in `orchestrator-py/worker.py:252-320` and the boot recovery skip in `src-tauri/src/lib.rs:198-204` and `272-279`.

- The docs undersell non-Claude wrapper support relative to current code.
  - Old audit material describes missing Cursor/Codex handling.
  - Current `wrapper/src/detector.rs` does include explicit `Cursor`, `Codex`, and `Gemini` handling with tests.

My read: the doc set is valuable, but it now mixes live architecture notes with frozen design-history notes. That is fine for an Obsidian vault, but only if each document is clearly labeled as either current-state or historical plan.

## 5. Test coverage gaps

- No automated frontend tests. The most severe bug I found is in the Tauri/frontend assignment path, which is exactly the sort of issue unit-tested Rust state transitions will not catch.
- No end-to-end test for the Tauri `assign_task` IPC path with an assigned agent.
- No test proving parity between Tauri `spawn_agent` and daemon `cli.spawn` runtime behavior.
- No regression test for worker task-failure handling leaving a task stuck `ACCEPTED`.
- No test for `project_save` patch semantics or for preservation of existing profile fields.
- The Python suite exists, but this environment could not run it because `pytest` is missing. That is a tooling gap around reproducibility.
- The tmux integration suite needs an additional "usable tmux session" preflight, not just "tmux binary exists".

## 6. Top 10 concrete improvements ranked by impact

1. Fix `src-tauri/src/commands.rs::assign_task` to use the same backend path as CLI/orchestrator assignment.
   - Call `assign_task_to_agent`, enforce `max_concurrent`, and only then send `task.assign`.
   - This removes the most severe correctness bug in the UI path.

2. On worker task exceptions, transition the task out of `ACCEPTED`.
   - Best minimal fix: send `task.blocked` or a dedicated failure envelope from `worker.py` instead of only `wrapper.error`.
   - This closes the slot-starvation bug.

3. Make Tauri `spawn_agent` runtime-aware.
   - Reuse the same spawn code path as `cli.spawn` instead of hard-coding `force_launch_wrapper`.

4. Change `project_save` from replace semantics to merge semantics.
   - Load the existing profile first, patch only provided fields, then persist.
   - Also persist `memory_hub` after `link_agent_memory`.

5. Add tests for the frontend/Tauri IPC paths that currently bypass the Rust CLI surface.
   - Specifically: assigned-task creation, disconnected-agent assignment, and runtime-aware spawn.

6. Commit `orchestrator-py/alor_footer.py` or remove the import dependency.
   - Running production code against an untracked module is not acceptable release hygiene.

7. Fix or explicitly source the default model identifiers.
   - If the env vars are required, fail early with a clear error.
   - If defaults are intended, make them valid.

8. Move the workspace release profile to the workspace root `Cargo.toml`.
   - Make the actual release build match the intended optimization settings.

9. Add project selection to the frontend task-create flow and filter assignable agents.
   - That makes the UI participate in the same architecture as the orchestrator instead of being a degraded side path.

10. Split "current state" docs from "historical design" docs more explicitly.
   - The Memory Hub path drift and template-instantiation history are both signs that the vault needs clearer status labeling.

## 7. Anything else noticed

- The current codebase is materially stronger in the daemon/protocol/state layers than in the Tauri IPC layer. Most of the real bugs I found live in the alternate control surfaces, not in the core transport/state machinery.
- The system has a lot of good recent audit-driven hardening, and it shows. Most of the remaining risk is now inconsistency between entrypoints, not absence of architecture.
- The strongest pattern in the codebase is "CLI/orchestrator path is correct, Tauri convenience path drifts." That is the design smell I would watch going forward.

## 8. Bottom line

Alor is no longer a loose prototype. The core daemon, eventing model, state machine, and Python worker/orchestrator split are real system architecture now. The main risk is not conceptual fragility; it is behavioral divergence between surfaces.

If I had to summarize the audit in one sentence: the Rust kernel and Python orchestration layers are on a solid trajectory, but the Tauri/UI control paths need to be collapsed back onto the same authoritative backend APIs before they create hard-to-debug state divergence in daily use.
