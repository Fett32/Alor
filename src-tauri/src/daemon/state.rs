use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use parking_lot::Mutex;
use tauri::AppHandle;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Task state machine
// ---------------------------------------------------------------------------

/// All valid states a task can be in.
/// Transitions are enforced by `TaskEntry::transition`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskState {
    /// Task created but not yet assigned to any agent.
    Pending,
    /// Orchestrator has created the task and nominated an agent.
    Assigned,
    /// Agent has acknowledged and begun work.
    Accepted,
    /// Agent finished successfully.
    Completed,
    /// Agent is waiting on a dependency or external event.
    Blocked,
    /// Orchestrator or user explicitly cancelled the task.
    Cancelled,
    /// Agent refused to take the task.
    Rejected,
    /// Watchdog deadline passed without a status update.
    TimedOut,
    /// Agent was interrupted mid-run (e.g. tmux pane died).
    Interrupted,
    /// Agent is attempting to resume after an interruption.
    Recovering,
    /// Task was assigned but no acknowledgement received within the grace window.
    Stale,
    /// Agent has proposed a plan or diff and is waiting for human approval.
    Proposed,
    /// Human has approved the proposal; agent is now applying the changes.
    Staged,
}

impl TaskState {
    /// Returns `true` if the state is terminal (no further transitions allowed).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskState::Completed
                | TaskState::Cancelled
                | TaskState::Rejected
                | TaskState::TimedOut
                | TaskState::Stale
        )
    }

    /// Validate whether a transition from `self` to `next` is legal.
    pub fn can_transition_to(&self, next: &TaskState) -> bool {
        use TaskState::*;
        // Narrow terminal-bookkeeping edges. None of these resume work;
        // they just re-label the final state so the user / orchestrator
        // can tidy up a completed run without fighting the state
        // machine.
        //   - Cancelled -> Completed: "I cancelled this, but it was
        //     actually done" (retroactive close-out; pre-existing).
        //   - {Stale, Completed, Rejected, TimedOut} -> Cancelled:
        //     user-initiated finalize from the Tauri UI. STALE is the
        //     motivating case (stale tasks don't resume, user wants
        //     them off the board); the other three are symmetric and
        //     harmless — cancelling a completed/rejected/timed-out
        //     task just collapses its label to "cancelled" for
        //     filtering purposes.
        //     Cancelled -> Cancelled is handled as a no-op in
        //     `transition_task` (not here) so double-clicks don't
        //     surface as errors.
        // Terminal classification (is_terminal / assigned_to clear /
        // agent_active_task_count) is unchanged by any of these edges.
        match (self, next) {
            (Cancelled, Completed) => return true,
            (Stale, Cancelled)
            | (Completed, Cancelled)
            | (Rejected, Cancelled)
            | (TimedOut, Cancelled) => return true,
            _ => {}
        }
        if self.is_terminal() {
            return false;
        }
        matches!(
            (self, next),
            (Pending, Assigned)
                | (Pending, Cancelled)
                | (Assigned, Accepted)
                | (Assigned, Rejected)
                | (Assigned, Cancelled)
                | (Assigned, Stale)
                | (Accepted, Proposed)
                | (Accepted, Completed)
                | (Accepted, Blocked)
                | (Accepted, Cancelled)
                | (Accepted, Interrupted)
                | (Accepted, TimedOut)
                | (Proposed, Staged)
                | (Proposed, Accepted)
                | (Proposed, Cancelled)
                | (Staged, Completed)
                | (Staged, Blocked)
                | (Staged, Cancelled)
                | (Staged, Interrupted)
                | (Blocked, Accepted)
                | (Blocked, Cancelled)
                | (Blocked, TimedOut)
                | (Interrupted, Recovering)
                | (Interrupted, Cancelled)
                | (Recovering, Accepted)
                | (Recovering, Cancelled)
                | (Recovering, Interrupted)
        )
    }
}

// ---------------------------------------------------------------------------
// Task
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: Uuid,
    pub title: String,
    pub description: String,
    pub state: TaskState,
    /// Agent ID this task is assigned to (may be empty if unassigned).
    pub assigned_to: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub parent_task_id: Option<Uuid>,
    #[serde(default)]
    pub subtask_order: u32,
    /// Set to true by `record_user_intervention` when the wrapper's
    /// tmux poll detects that pane content changed while the agent
    /// wasn't idle — typically the user typing (or pasting) into the
    /// agent's pane mid-task. The `user_intervened_at` companion
    /// holds the timestamp of the most recent intervention.
    ///
    /// Informational-only: NOTHING in the daemon reads this flag as a
    /// gate or guard. The task-completion paths (wrapper-driven
    /// `MSG_TASK_COMPLETE`, orch-driven `MSG_CLI_TASK_COMPLETE`, state
    /// machine `transition_task`) do not check it. It exists so the
    /// orchestrator can surface "hey, Fett touched this task's pane"
    /// in its own reasoning / summaries. If you see a task that
    /// appears stuck with `user_intervened: true`, the flag is a
    /// symptom, not the cause — look elsewhere (idle-detector
    /// stability, wrapper reconnect cycles, etc).
    ///
    /// Cleared by `AppState::clear_user_intervention` or by setting
    /// it back manually; see `cli.task.intervention.clear` RPC for
    /// the orchestrator-facing path.
    #[serde(default)]
    pub user_intervened: bool,
    #[serde(default)]
    pub user_intervened_at: Option<DateTime<Utc>>,
    /// Short logic brief for the proposed change (for HITL approval).
    #[serde(default)]
    pub proposal_brief: Option<String>,
    /// Unified diff or JSON representation of proposed changes.
    #[serde(default)]
    pub proposal_diff: Option<String>,
    /// Final free-form answer/report from the worker (set on completion).
    #[serde(default)]
    pub summary: Option<String>,
    /// Name of the project profile this task relates to; used to build a
    /// TASK BRIEF (key files, docs) that is prepended when dispatching.
    #[serde(default)]
    pub project: Option<String>,
}

/// Projection of `Task` used by `cli.task.list` when the caller passes
/// `view = "summary"` (the default — see
/// `src-tauri/src/wrapper/protocol.rs::DEFAULT_TASK_LIST_VIEW`).
///
/// Five fields: id, title, state, assigned_to, updated_at. ~0.2k tokens
/// per task vs. ~1.1k tokens for the full Task — lets a scan pull
/// hundreds of entries safely under the orch-context ceiling. Detail
/// (description / proposal_brief / proposal_diff / summary / project)
/// stays reachable via `task_get`.
///
/// Serialize-only; there's no reason to reconstruct a Task from the
/// summary on the wire.
#[derive(Debug, Clone, Serialize)]
pub struct TaskSummary {
    pub id: Uuid,
    pub title: String,
    pub state: TaskState,
    pub assigned_to: Option<String>,
    pub updated_at: DateTime<Utc>,
}

impl Task {
    /// Project to the summary-view shape. Called by the server after
    /// filtering + pagination when `view = "summary"`.
    pub fn summary(&self) -> TaskSummary {
        TaskSummary {
            id: self.id,
            title: self.title.clone(),
            state: self.state.clone(),
            assigned_to: self.assigned_to.clone(),
            updated_at: self.updated_at,
        }
    }

    pub fn new(title: impl Into<String>, description: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            title: title.into(),
            description: description.into(),
            state: TaskState::Pending,
            assigned_to: None,
            created_at: now,
            updated_at: now,
            parent_task_id: None,
            subtask_order: 0,
            user_intervened: false,
            user_intervened_at: None,
            proposal_brief: None,
            proposal_diff: None,
            summary: None,
            project: None,
        }
    }

    /// Attempt a state transition.  Returns `Err` if the transition is illegal.
    pub fn transition(&mut self, next: TaskState) -> anyhow::Result<()> {
        if self.state.can_transition_to(&next) {
            tracing::info!(
                task_id = %self.id,
                from = ?self.state,
                to = ?next,
                "task state transition"
            );
            self.state = next;
            self.updated_at = Utc::now();
            Ok(())
        } else {
            anyhow::bail!(
                "illegal transition {:?} -> {:?} for task {}",
                self.state,
                next,
                self.id
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub name: String,
    /// tmux session name this agent lives in.
    pub tmux_session: Option<String>,
    /// Path to the Unix socket the wrapper exposes (if connected).
    pub socket_path: Option<String>,
    pub connected: bool,
    pub registered_at: DateTime<Utc>,
    /// Project this slot is bound to (null for generic/unscoped).
    #[serde(default)]
    pub project: Option<String>,
    /// Tier classification: heavy | mid | light.
    #[serde(default = "default_tier")]
    pub tier: String,
    /// Max simultaneous non-terminal tasks before the daemon rejects new assignments.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: u8,
    /// Bounded deque of recent task ids this agent handled (most recent first).
    #[serde(default)]
    pub task_history: VecDeque<Uuid>,
    /// Working directory for this instance.  Set at spawn time from either
    /// the yaml config or a runtime override; persisted so the daemon can
    /// respawn template-based instances after restart.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// For instances spawned from a template: the template's yaml id
    /// (e.g. "claude" for a claude-mandaspace instance).  None for fixed
    /// yaml-declared slots.
    #[serde(default)]
    pub template: Option<String>,
}

fn default_tier() -> String {
    "mid".to_string()
}

fn default_max_concurrent() -> u8 {
    1
}

const AGENT_TASK_HISTORY_MAX: usize = 20;

impl Agent {
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            tmux_session: None,
            socket_path: None,
            connected: false,
            registered_at: Utc::now(),
            project: None,
            tier: default_tier(),
            max_concurrent: default_max_concurrent(),
            task_history: VecDeque::new(),
            working_dir: None,
            template: None,
        }
    }

    /// Push a task_id onto the front of the history deque and trim to the
    /// bounded maximum.
    pub fn push_task_history(&mut self, task_id: Uuid) {
        self.task_history.retain(|&existing| existing != task_id);
        self.task_history.push_front(task_id);
        while self.task_history.len() > AGENT_TASK_HISTORY_MAX {
            self.task_history.pop_back();
        }
    }
}

// ---------------------------------------------------------------------------
// AppState — Tauri managed state
// ---------------------------------------------------------------------------

use std::collections::HashSet;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Mutex<StateInner>>,
    save_path: Arc<Option<PathBuf>>,
    /// App handle for emitting push events to the frontend.
    app_handle: Arc<Mutex<Option<AppHandle>>>,
    /// Tracks agent instances spawned by a specific task so we can
    /// warn on terminal transition if any weren't cleaned up.
    ///
    /// Lifecycle:
    ///   - `cli.spawn` with `spawned_by_task = Some(tid)` inserts the
    ///     new instance_id into `task_spawns[tid]`.
    ///   - `cli.delete` removes the instance_id from every entry —
    ///     an explicit kill clears the tracking.
    ///   - `transition_task` to a terminal state drains the entry
    ///     for that task_id; any survivors become the warning
    ///     appended to the task's `summary`.
    ///
    /// Deliberately NOT persisted to state.json: it's debug
    /// scaffolding that only makes sense for in-flight tasks. A
    /// daemon reboot drops the tracking along with the task context
    /// that gives it meaning.
    task_spawns: Arc<Mutex<HashMap<Uuid, HashSet<String>>>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(StateInner::default())),
            save_path: Arc::new(None),
            app_handle: Arc::new(Mutex::new(None)),
            task_spawns: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct StateInner {
    tasks: HashMap<Uuid, Task>,
    agents: HashMap<String, Agent>,
}

/// On-disk layout of `tasks-archive.json`.  Terminal tasks are swept out of
/// live state into this file on every daemon startup so the running state
/// file stays lean.  Schema intentionally matches the format of the existing
/// hand-curated archive: a `tasks` map keyed by UUID plus an RFC3339
/// `last_archive_run` timestamp.
#[derive(Default, Serialize, Deserialize)]
struct TasksArchive {
    #[serde(default)]
    tasks: HashMap<Uuid, Task>,
    #[serde(default)]
    last_archive_run: Option<DateTime<Utc>>,
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the app handle for push events. Call once during Tauri setup.
    pub fn set_app_handle(&self, handle: AppHandle) {
        *self.app_handle.lock() = Some(handle);
    }

    /// Emit an event to the frontend (non-blocking, best-effort).
    pub fn emit_event(&self, event: &str) {
        let guard = self.app_handle.lock();
        if let Some(ref handle) = *guard {
            use tauri::Emitter;
            if let Err(e) = handle.emit(event, ()) {
                tracing::warn!("failed to emit {event}: {e}");
            }
        }
    }

    /// Emit an event with a serializable payload.
    pub fn emit_event_with<S: serde::Serialize + Clone>(&self, event: &str, payload: S) {
        let guard = self.app_handle.lock();
        if let Some(ref handle) = *guard {
            use tauri::Emitter;
            if let Err(e) = handle.emit(event, payload) {
                tracing::warn!("failed to emit {event}: {e}");
            }
        }
    }

    /// Create a new AppState that auto-saves to the given path.
    /// If the file exists, state is restored from it (agents marked disconnected).
    pub fn with_persistence(path: PathBuf) -> Self {
        let inner = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(json) => match serde_json::from_str::<StateInner>(&json) {
                    Ok(mut restored) => {
                        // Mark all agents disconnected — connections are transient.
                        for agent in restored.agents.values_mut() {
                            agent.connected = false;
                        }
                        tracing::info!(
                            tasks = restored.tasks.len(),
                            agents = restored.agents.len(),
                            "session restored from {}",
                            path.display()
                        );
                        restored
                    }
                    Err(e) => {
                        tracing::warn!("failed to parse session file: {e}, starting fresh");
                        StateInner::default()
                    }
                },
                Err(e) => {
                    tracing::warn!("failed to read session file: {e}, starting fresh");
                    StateInner::default()
                }
            }
        } else {
            StateInner::default()
        };

        Self {
            inner: Arc::new(Mutex::new(inner)),
            save_path: Arc::new(Some(path)),
            app_handle: Arc::new(Mutex::new(None)),
            task_spawns: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Persist current state to disk. Called automatically after mutations.
    /// Clones data under the lock, then writes outside the lock using
    /// atomic rename to prevent corruption.
    pub fn save(&self) {
        if let Some(path) = self.save_path.as_ref() {
            // Clone data under lock, then release immediately.
            let json = {
                let s = self.inner.lock();
                match serde_json::to_string_pretty(&*s) {
                    Ok(j) => j,
                    Err(e) => {
                        tracing::warn!("failed to serialize session: {e}");
                        return;
                    }
                }
            };
            // Write to temp file, fsync, then atomic rename. Without the
            // fsync the rename can beat dirty pagecache to disk and leave
            // a truncated file after a crash, which is exactly what
            // "atomic rename" was supposed to prevent.
            let tmp_path = path.with_extension("json.tmp");
            match std::fs::File::create(&tmp_path) {
                Ok(mut f) => {
                    use std::io::Write;
                    if let Err(e) = f.write_all(json.as_bytes()) {
                        tracing::warn!("failed to write temp session file: {e}");
                        return;
                    }
                    if let Err(e) = f.sync_all() {
                        tracing::warn!("failed to fsync session file: {e}");
                        return;
                    }
                }
                Err(e) => {
                    tracing::warn!("failed to open temp session file: {e}");
                    return;
                }
            }
            if let Err(e) = std::fs::rename(&tmp_path, path) {
                tracing::warn!("failed to rename session file: {e}");
            }
        }
    }

    /// Sweep terminal tasks out of live state into `archive_path`. Kept
    /// here for future reuse by a capped auto-archive pass (not wired up
    /// yet — b4102e92 accepts unbounded state.json growth for now).
    /// Marked `#[allow(dead_code)]` so `cargo check` stays clean.
    #[allow(dead_code)]
    pub fn archive_terminal_tasks(&self, archive_path: &Path) -> usize {
        // Snapshot terminal tasks under lock without removing yet.
        let terminal: Vec<(Uuid, Task)> = {
            let s = self.inner.lock();
            s.tasks
                .iter()
                .filter(|(_, t)| t.state.is_terminal())
                .map(|(id, t)| (*id, t.clone()))
                .collect()
        };

        if terminal.is_empty() {
            return 0;
        }

        // Load existing archive (if any).
        let mut archive: TasksArchive = if archive_path.exists() {
            match std::fs::read_to_string(archive_path) {
                Ok(json) => match serde_json::from_str(&json) {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::warn!(
                            "failed to parse {}: {e}; starting a fresh archive",
                            archive_path.display()
                        );
                        TasksArchive::default()
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        "failed to read {}: {e}; starting a fresh archive",
                        archive_path.display()
                    );
                    TasksArchive::default()
                }
            }
        } else {
            TasksArchive::default()
        };

        let terminal_ids: Vec<Uuid> = terminal.iter().map(|(id, _)| *id).collect();
        for (id, task) in terminal {
            archive.tasks.insert(id, task);
        }
        archive.last_archive_run = Some(Utc::now());

        // Serialize + atomic write (tmp + fsync + rename), same pattern as save().
        let json = match serde_json::to_string_pretty(&archive) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!("failed to serialize archive: {e}");
                return 0;
            }
        };
        let tmp_path = archive_path.with_extension("json.tmp");
        match std::fs::File::create(&tmp_path) {
            Ok(mut f) => {
                use std::io::Write;
                if let Err(e) = f.write_all(json.as_bytes()) {
                    tracing::warn!("failed to write temp archive file: {e}");
                    return 0;
                }
                if let Err(e) = f.sync_all() {
                    tracing::warn!("failed to fsync archive file: {e}");
                    return 0;
                }
            }
            Err(e) => {
                tracing::warn!(
                    "failed to open temp archive file {}: {e}",
                    tmp_path.display()
                );
                return 0;
            }
        }
        if let Err(e) = std::fs::rename(&tmp_path, archive_path) {
            tracing::warn!("failed to rename archive file: {e}");
            return 0;
        }

        // Archive durable — now drop these from live state.  Re-check is_terminal()
        // per id in case something mutated between snapshot and now (shouldn't
        // happen during startup, but the check is cheap).
        let removed = {
            let mut s = self.inner.lock();
            let mut n = 0;
            for id in &terminal_ids {
                if let Some(task) = s.tasks.get(id) {
                    if task.state.is_terminal() {
                        s.tasks.remove(id);
                        n += 1;
                    }
                }
            }
            n
        };

        // Persist the shrunken live state.  No event emit here: this runs at
        // startup before the app handle is wired up, and nothing's listening.
        self.save();

        removed
    }

    /// One-shot migration: absorb the legacy `tasks-archive.json` into
    /// live state. After b4102e92 we stopped pruning terminal tasks on
    /// boot, so the archive file's accumulated records need to come back
    /// into `state.json` to be queryable via `task_get` /
    /// `task_list(state=...)`.
    ///
    /// Runs at startup in `lib.rs` before the Tauri builder. Idempotent:
    /// on first run renames the archive to `<path>.migrated` so
    /// subsequent boots skip. If the `.migrated` file exists OR the
    /// archive file doesn't exist, this is a no-op.
    ///
    /// Merge policy: `or_insert` — if an archive task's id already
    /// exists in live state, live state wins. Guards against the
    /// pathological case where a task was archived then re-created
    /// with the same id (shouldn't happen; UUIDs).
    ///
    /// Returns the number of tasks rehydrated. Errors during load or
    /// rename are logged, NOT fatal — startup must not block on
    /// migration glitches.
    pub fn migrate_archive(&self, archive_path: &Path) -> usize {
        if !archive_path.exists() {
            return 0;
        }
        let migrated_marker = archive_path.with_extension("json.migrated");
        if migrated_marker.exists() {
            tracing::debug!(
                path = %migrated_marker.display(),
                "archive already migrated; skipping"
            );
            return 0;
        }

        let archive: TasksArchive = match std::fs::read_to_string(archive_path) {
            Ok(json) => match serde_json::from_str(&json) {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!(
                        "failed to parse {}: {e}; skipping archive migration",
                        archive_path.display()
                    );
                    return 0;
                }
            },
            Err(e) => {
                tracing::warn!(
                    "failed to read {}: {e}; skipping archive migration",
                    archive_path.display()
                );
                return 0;
            }
        };

        if archive.tasks.is_empty() {
            // Empty archive: still rename so we don't re-check on every boot.
            if let Err(e) = std::fs::rename(archive_path, &migrated_marker) {
                tracing::warn!("failed to rename empty archive file: {e}");
            }
            return 0;
        }

        let merged_count = {
            let mut s = self.inner.lock();
            let mut n = 0;
            for (id, task) in archive.tasks {
                // or_insert: live state wins on any collision. Safer than
                // `insert` which would overwrite a live active-task record
                // with a stale archived copy.
                if !s.tasks.contains_key(&id) {
                    s.tasks.insert(id, task);
                    n += 1;
                }
            }
            n
        };

        // Persist the merged live state before renaming the archive — if
        // the rename fails after save, next boot re-merges idempotently
        // (contains_key skips duplicates). If save fails, next boot
        // retries the whole migration.
        self.save();

        if let Err(e) = std::fs::rename(archive_path, &migrated_marker) {
            tracing::warn!(
                "merged archive into live state but failed to rename {}: {e}",
                archive_path.display()
            );
        } else {
            tracing::info!(
                migrated = merged_count,
                marker = %migrated_marker.display(),
                "migrated archive into live state"
            );
        }

        merged_count
    }

    // --- tasks ---------------------------------------------------------------

    pub fn add_task(&self, task: Task) {
        let mut s = self.inner.lock();
        s.tasks.insert(task.id, task);
        drop(s);
        self.save();
        self.emit_event("tasks-changed");
    }

    pub fn get_task(&self, id: Uuid) -> Option<Task> {
        self.inner.lock().tasks.get(&id).cloned()
    }

    pub fn all_tasks(&self) -> Vec<Task> {
        self.inner.lock().tasks.values().cloned().collect()
    }

    // --- task-spawn tracking -------------------------------------------------
    //
    // Workers can `agent_spawn` sibling instances during a task (for
    // live verification, debug reproducibility, etc.). To avoid
    // resource leaks where a worker forgets to `agent_kill` before
    // completing, we track (task_id, instance_id) here and emit a
    // warning on terminal transition. See `transition_task` for the
    // emit path.

    /// Record that `instance_id` was spawned on behalf of `task_id`.
    /// Idempotent — re-recording the same pair is a no-op (HashSet).
    pub fn record_task_spawn(&self, task_id: Uuid, instance_id: impl Into<String>) {
        self.task_spawns
            .lock()
            .entry(task_id)
            .or_default()
            .insert(instance_id.into());
    }

    /// Remove `instance_id` from every task's spawn set. Called when
    /// `cli.delete` (explicit kill) runs — marks the instance as
    /// cleanly released so the completion-time warning stays silent.
    /// Does NOT fire on disconnect: a dead pane isn't the same as an
    /// intentional kill.
    pub fn clear_task_spawn(&self, instance_id: &str) {
        let mut guard = self.task_spawns.lock();
        for set in guard.values_mut() {
            set.remove(instance_id);
        }
        // Drop empty entries so `pending_spawns_for_task` stays sparse.
        guard.retain(|_, set| !set.is_empty());
    }

    /// Inspect (without draining) the spawn set for `task_id`.
    /// Primarily for tests and diagnostics.
    pub fn pending_spawns_for_task(&self, task_id: Uuid) -> Vec<String> {
        self.task_spawns
            .lock()
            .get(&task_id)
            .map(|s| {
                let mut v: Vec<String> = s.iter().cloned().collect();
                v.sort(); // stable order for tests / warning message
                v
            })
            .unwrap_or_default()
    }

    /// Drain and return the spawn set for `task_id`. Used by
    /// `transition_task` on terminal transitions — the tracking is
    /// scoped to the task's in-flight lifetime.
    pub fn take_pending_spawns_for_task(&self, task_id: Uuid) -> Vec<String> {
        self.task_spawns
            .lock()
            .remove(&task_id)
            .map(|s| {
                let mut v: Vec<String> = s.into_iter().collect();
                v.sort();
                v
            })
            .unwrap_or_default()
    }

    /// Transition a task to a new state.  Returns the updated task on success.
    pub fn transition_task(&self, id: Uuid, next: TaskState) -> anyhow::Result<Task> {
        let is_completing = next == TaskState::Completed;
        // Cancel-on-Cancelled is a no-op, not an error. Lets the UI /
        // CLI hit Cancel on an already-cancelled task (double click,
        // race with a background sweep, etc.) without surfacing a
        // scary "illegal transition" error. Consistent with the
        // rest of the user-initiated finalize-from-terminal semantics
        // in `TaskState::can_transition_to`.
        {
            let s = self.inner.lock();
            if let Some(task) = s.tasks.get(&id) {
                if task.state == TaskState::Cancelled && next == TaskState::Cancelled {
                    return Ok(task.clone());
                }
            }
        }
        let mut s = self.inner.lock();
        let task = s
            .tasks
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("task {} not found", id))?;
        task.transition(next)?;
        // Clear assigned_to when terminal so the agent's active-task count
        // drops and max_concurrent slots free up.
        if task.state.is_terminal() {
            task.assigned_to = None;
        }

        // Terminal transition: surface any agent instances the worker
        // spawned but didn't explicitly kill. Appends to `summary` so
        // the warning is visible via task_get and in the task-list UI.
        // Don't auto-kill — the worker may have intentionally left
        // instances alive (e.g. for follow-up debugging).
        let terminal_leak: Option<Vec<String>> = if task.state.is_terminal() {
            // Drain inline under the same lock-free window (we already
            // dropped `s` below for save/event emission). We own
            // task_spawns separately so taking it here is safe.
            let leaked = self.take_pending_spawns_for_task(id);
            if leaked.is_empty() {
                None
            } else {
                let warning = format!(
                    "[ALERT] {} agent instance(s) spawned by this task \
                     were not explicitly killed: {}. Call agent_kill on \
                     each before completing — or confirm they're meant \
                     to outlive the task.",
                    leaked.len(),
                    leaked.join(", "),
                );
                task.summary = Some(match task.summary.take() {
                    Some(existing) if !existing.is_empty() => {
                        format!("{existing}\n\n{warning}")
                    }
                    _ => warning,
                });
                tracing::warn!(
                    task_id = %id,
                    leaked = ?leaked,
                    "task completed with unreleased worker spawns"
                );
                Some(leaked)
            }
        } else {
            None
        };

        let result = task.clone();
        drop(s);
        self.save();
        self.emit_event("tasks-changed");
        if is_completing {
            self.emit_event_with("task-completed", result.title.clone());
        }
        // Emit a dedicated event if we appended a leak warning so the
        // UI / orch can surface it distinctly if it wants to.
        if let Some(leaked) = terminal_leak {
            self.emit_event_with(
                "task-spawn-leak",
                serde_json::json!({
                    "task_id": id.to_string(),
                    "instances": leaked,
                }),
            );
        }
        Ok(result)
    }

    /// Transition a task to PROPOSED state with a brief and/or diff.
    /// Brief and diff are each capped at 10 MiB to prevent a malicious or
    /// runaway agent from ballooning the session state file.
    pub fn propose_task(
        &self,
        id: Uuid,
        brief: Option<String>,
        diff: Option<String>,
    ) -> anyhow::Result<Task> {
        const MAX_PROPOSAL_BYTES: usize = 10 * 1024 * 1024;
        if let Some(ref b) = brief {
            if b.len() > MAX_PROPOSAL_BYTES {
                anyhow::bail!("proposal brief exceeds 10 MiB ({} bytes)", b.len());
            }
        }
        if let Some(ref d) = diff {
            if d.len() > MAX_PROPOSAL_BYTES {
                anyhow::bail!("proposal diff exceeds 10 MiB ({} bytes)", d.len());
            }
        }

        let mut s = self.inner.lock();
        let task = s
            .tasks
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("task {} not found", id))?;

        task.transition(TaskState::Proposed)?;
        task.proposal_brief = brief;
        task.proposal_diff = diff;

        let result = task.clone();
        drop(s);
        self.save();
        self.emit_event("tasks-changed");
        Ok(result)
    }

    /// Record a worker-provided summary on the task.  Capped at 1 MiB to
    /// prevent a runaway worker from ballooning the session state file.
    pub fn set_task_summary(&self, id: Uuid, summary: String) {
        const MAX_SUMMARY_BYTES: usize = 1024 * 1024;
        let text = if summary.len() > MAX_SUMMARY_BYTES {
            tracing::warn!(task_id = %id, bytes = summary.len(), "task summary truncated");
            // Truncate on a UTF-8 char boundary at or before MAX_SUMMARY_BYTES
            // so we never emit an invalid sequence.
            let mut end = MAX_SUMMARY_BYTES;
            while end > 0 && !summary.is_char_boundary(end) {
                end -= 1;
            }
            let mut s = summary;
            s.truncate(end);
            s
        } else {
            summary
        };
        let mut s = self.inner.lock();
        if let Some(task) = s.tasks.get_mut(&id) {
            task.summary = Some(text);
            task.updated_at = Utc::now();
        }
        drop(s);
        self.save();
    }

    /// Assign a task to an agent (sets `assigned_to` and transitions to Assigned).
    /// Also records the task in the agent's task_history.
    pub fn assign_task_to_agent(
        &self,
        task_id: Uuid,
        agent_id: &str,
    ) -> anyhow::Result<Task> {
        let mut s = self.inner.lock();
        let task = s
            .tasks
            .get_mut(&task_id)
            .ok_or_else(|| anyhow::anyhow!("task {} not found", task_id))?;

        // Transition Pending → Assigned if needed (audit #8: verify state).
        if task.state == TaskState::Pending {
            task.transition(TaskState::Assigned)?;
        } else if task.state != TaskState::Assigned {
            anyhow::bail!(
                "cannot assign task {} in state {:?}",
                task_id, task.state
            );
        }

        task.assigned_to = Some(agent_id.to_string());
        task.updated_at = Utc::now();
        let result = task.clone();

        // Record on the agent's history (same lock for atomicity).
        if let Some(agent) = s.agents.get_mut(agent_id) {
            agent.push_task_history(task_id);
        }

        drop(s);
        self.save();
        self.emit_event("tasks-changed");
        self.emit_event("agents-changed");
        Ok(result)
    }

    /// Count non-terminal, non-pending tasks assigned to an agent.
    /// Used to enforce `max_concurrent` before dispatching.
    pub fn agent_active_task_count(&self, agent_id: &str) -> usize {
        self.inner
            .lock()
            .tasks
            .values()
            .filter(|t| {
                t.assigned_to.as_deref() == Some(agent_id)
                    && !t.state.is_terminal()
                    && t.state != TaskState::Pending
            })
            .count()
    }

    /// Update metadata fields on an existing agent (project, tier, max_concurrent,
    /// working_dir, template).  Used to propagate AgentConfig values and any
    /// runtime overrides to spawned instances.  `None` leaves a field unchanged;
    /// `Some(None)` is not expressible here — call set_agent_metadata_clear_* if
    /// you need to wipe a field.
    pub fn set_agent_metadata(
        &self,
        agent_id: &str,
        project: Option<String>,
        tier: Option<String>,
        max_concurrent: Option<u8>,
        working_dir: Option<String>,
        template: Option<String>,
    ) {
        let mut s = self.inner.lock();
        if let Some(agent) = s.agents.get_mut(agent_id) {
            if let Some(p) = project {
                agent.project = Some(p);
            }
            if let Some(t) = tier {
                agent.tier = t;
            }
            if let Some(m) = max_concurrent {
                agent.max_concurrent = m;
            }
            if let Some(wd) = working_dir {
                agent.working_dir = Some(wd);
            }
            if let Some(tmpl) = template {
                agent.template = Some(tmpl);
            }
        }
        drop(s);
        self.save();
        self.emit_event("agents-changed");
    }

    /// Mark any Running/Accepted task assigned to `agent_id` as user-intervened.
    /// Returns the task_ids that were flagged so callers can include them in
    /// event payloads.
    pub fn record_user_intervention(&self, agent_id: &str) -> Vec<Uuid> {
        let mut s = self.inner.lock();
        let now = Utc::now();
        let mut affected = Vec::new();
        for task in s.tasks.values_mut() {
            // Flag any in-flight task on this agent. Previously only Accepted
            // matched, which missed Assigned (pre-ack), Proposed (awaiting
            // approval), and Staged (post-approval mid-apply).
            if task.assigned_to.as_deref() == Some(agent_id)
                && matches!(
                    task.state,
                    TaskState::Assigned
                        | TaskState::Accepted
                        | TaskState::Proposed
                        | TaskState::Staged
                        | TaskState::Blocked
                        | TaskState::Recovering
                )
            {
                task.user_intervened = true;
                task.user_intervened_at = Some(now);
                task.updated_at = now;
                affected.push(task.id);
                tracing::info!(
                    task_id = %task.id,
                    agent_id,
                    "recorded user intervention on task"
                );
            }
        }
        drop(s);
        if !affected.is_empty() {
            self.save();
            self.emit_event("tasks-changed");
        }
        affected
    }

    /// Reset `user_intervened` + `user_intervened_at` on the given task.
    /// Orchestrator-facing counterpart to `record_user_intervention`.
    ///
    /// Use case: Fett typed something into a worker's pane (which
    /// latched `user_intervened: true`) but the input was
    /// unsubmitted or unintentional. The flag is informational —
    /// nothing in the daemon blocks completion on it — but leaving
    /// it set is misleading when the task is still in-flight and
    /// the intervention isn't relevant anymore. Exposed via
    /// `cli.task.intervention.clear`.
    ///
    /// Idempotent: clearing an already-clear flag is a no-op.
    /// Persists the state change + emits `tasks-changed` so the UI
    /// refreshes. Returns the updated Task on success, NotFound on
    /// unknown task id.
    pub fn clear_user_intervention(&self, id: Uuid) -> anyhow::Result<Task> {
        let mut s = self.inner.lock();
        let task = s
            .tasks
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("task {} not found", id))?;
        let was_set = task.user_intervened;
        task.user_intervened = false;
        task.user_intervened_at = None;
        if was_set {
            task.updated_at = Utc::now();
        }
        let result = task.clone();
        drop(s);
        if was_set {
            tracing::info!(
                task_id = %id,
                "cleared user_intervened flag"
            );
            self.save();
            self.emit_event("tasks-changed");
        }
        Ok(result)
    }

    // --- agents --------------------------------------------------------------

    pub fn register_agent(&self, agent: Agent) {
        let mut s = self.inner.lock();
        s.agents.insert(agent.id.clone(), agent);
        drop(s);
        self.save();
        self.emit_event("agents-changed");
    }

    pub fn all_agents(&self) -> Vec<Agent> {
        self.inner
            .lock()
            .agents
            .values()
            .cloned()
            .collect()
    }

    pub fn get_agent(&self, id: &str) -> Option<Agent> {
        self.inner.lock().agents.get(id).cloned()
    }

    /// Permanently remove an agent row from state. Used for tombstoning
    /// template-spawned instances (claude-mandaspace, etc.) — core yaml
    /// slots should be killed (disconnect) rather than deleted.
    /// Returns true if the agent existed and was removed.
    pub fn remove_agent(&self, id: &str) -> bool {
        let removed = {
            let mut s = self.inner.lock();
            s.agents.remove(id).is_some()
        };
        if removed {
            self.save();
            self.emit_event("agents-changed");
        }
        removed
    }

    pub fn clear_all_agents(&self) {
        let mut s = self.inner.lock();
        for agent in s.agents.values_mut() {
            agent.connected = false;
        }
        drop(s);
        self.save();
        self.emit_event("agents-changed");
    }

    /// Update the connected status of an agent.
    /// Returns Err if the agent is already connected (prevents collisions).
    pub fn set_agent_connected(&self, id: &str, connected: bool) -> anyhow::Result<()> {
        let mut s = self.inner.lock();
        if let Some(agent) = s.agents.get_mut(id) {
            if connected && agent.connected {
                anyhow::bail!("agent {} already connected", id);
            }
            agent.connected = connected;
            tracing::info!(agent_id = id, connected, "agent connection status updated");
        } else {
            // Auto-register agent on first connection
            let mut agent = Agent::new(id, id);
            agent.connected = connected;
            agent.tmux_session = Some(format!("alor-{}", id));
            tracing::info!(agent_id = id, "agent auto-registered on connection");
            s.agents.insert(id.to_string(), agent);
        }
        drop(s);
        self.save();
        self.emit_event("agents-changed");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_bookkeeping_edges_are_legal() {
        // Cancelled is still terminal for counting/assigned_to semantics …
        assert!(TaskState::Cancelled.is_terminal());
        // … but the narrow retroactive-closeout edge is allowed.
        assert!(TaskState::Cancelled.can_transition_to(&TaskState::Completed));

        // User-initiated finalize-from-terminal: the UI lets the user
        // cancel any terminal card (with a confirm prompt); the state
        // machine must accept those transitions.
        assert!(TaskState::Stale.can_transition_to(&TaskState::Cancelled));
        assert!(TaskState::Completed.can_transition_to(&TaskState::Cancelled));
        assert!(TaskState::Rejected.can_transition_to(&TaskState::Cancelled));
        assert!(TaskState::TimedOut.can_transition_to(&TaskState::Cancelled));

        // Cancelled -> Cancelled is NOT a legal state-machine edge
        // (can_transition_to stays false); transition_task short-
        // circuits it as a no-op before ever calling the state
        // machine. See `cancel_on_already_cancelled_is_noop`.
        assert!(!TaskState::Cancelled.can_transition_to(&TaskState::Cancelled));

        // Resume-style transitions out of terminal states remain
        // forbidden — the only escape hatches are the two bookkeeping
        // edges above.
        assert!(!TaskState::Cancelled.can_transition_to(&TaskState::Pending));
        assert!(!TaskState::Cancelled.can_transition_to(&TaskState::Assigned));
        assert!(!TaskState::Completed.can_transition_to(&TaskState::Accepted));
        assert!(!TaskState::Rejected.can_transition_to(&TaskState::Completed));
        assert!(!TaskState::Stale.can_transition_to(&TaskState::Accepted));
    }

    #[test]
    fn task_summary_projects_only_the_five_lean_fields() {
        // Build a Task with every field populated so we can confirm the
        // projection genuinely drops the heavy ones (description,
        // proposal_brief, proposal_diff, summary, project,
        // created_at, parent_task_id, subtask_order, user_intervened[_at]).
        let mut t = Task::new("title", "a long description");
        t.state = TaskState::Accepted;
        t.assigned_to = Some("claude-alor".to_string());
        t.proposal_brief = Some("brief".to_string());
        t.proposal_diff = Some("diff".to_string());
        t.summary = Some("summary".to_string());
        t.project = Some("alor".to_string());
        t.parent_task_id = Some(Uuid::new_v4());
        t.subtask_order = 3;
        t.user_intervened = true;
        t.user_intervened_at = Some(Utc::now());

        let s = t.summary();
        assert_eq!(s.id, t.id);
        assert_eq!(s.title, "title");
        assert_eq!(s.state, TaskState::Accepted);
        assert_eq!(s.assigned_to.as_deref(), Some("claude-alor"));
        assert_eq!(s.updated_at, t.updated_at);

        // Serialize the summary and confirm heavy fields aren't in the
        // wire JSON. This is the real contract: server sends these,
        // callers don't see description etc.
        let json = serde_json::to_value(&s).expect("summary serializes");
        let obj = json.as_object().expect("summary is a JSON object");
        let keys: std::collections::BTreeSet<&str> =
            obj.keys().map(String::as_str).collect();
        let expected: std::collections::BTreeSet<&str> = [
            "id", "title", "state", "assigned_to", "updated_at",
        ]
        .iter()
        .copied()
        .collect();
        assert_eq!(keys, expected, "summary must serialize exactly 5 fields");
    }

    #[test]
    fn task_transition_cancelled_to_completed_actually_applies() {
        let mut t = Task::new("retro", "close out a cancelled task");
        t.state = TaskState::Cancelled;
        t.transition(TaskState::Completed).expect("should succeed");
        assert_eq!(t.state, TaskState::Completed);
    }

    #[test]
    fn task_transition_stale_to_cancelled_actually_applies() {
        let mut t = Task::new("stale", "clean up a stale task");
        t.state = TaskState::Stale;
        t.transition(TaskState::Cancelled).expect("should succeed");
        assert_eq!(t.state, TaskState::Cancelled);
    }

    // --- task-spawn warning tests -----------------------------------------

    /// Helper: fresh AppState with a state-persistence path under tempdir.
    /// Per-test nonce avoids parallel-run collisions.
    fn fresh_state() -> (AppState, PathBuf) {
        let nonce = Uuid::new_v4();
        let state_path = std::env::temp_dir().join(format!("alor-test-spawn-{nonce}.json"));
        let _ = std::fs::remove_file(&state_path);
        (AppState::with_persistence(state_path.clone()), state_path)
    }

    /// Helper: put an Accepted task into state so transition_task
    /// can move it to Completed/Cancelled.
    fn seed_accepted_task(app: &AppState) -> Uuid {
        let mut t = Task::new("spawn-leak test", "desc");
        t.state = TaskState::Accepted;
        let id = t.id;
        app.add_task(t);
        id
    }

    #[test]
    fn task_spawn_tracking_round_trips() {
        let (app, state_path) = fresh_state();
        let tid = Uuid::new_v4();

        assert!(app.pending_spawns_for_task(tid).is_empty());

        app.record_task_spawn(tid, "codex-debug-a");
        app.record_task_spawn(tid, "claude-debug-b");
        // Idempotent re-record is a no-op.
        app.record_task_spawn(tid, "codex-debug-a");
        assert_eq!(
            app.pending_spawns_for_task(tid),
            vec!["claude-debug-b".to_string(), "codex-debug-a".to_string()],
        );

        // clear_task_spawn by instance removes it from every task set.
        app.clear_task_spawn("codex-debug-a");
        assert_eq!(
            app.pending_spawns_for_task(tid),
            vec!["claude-debug-b".to_string()],
        );

        // Draining removes the entry entirely.
        let drained = app.take_pending_spawns_for_task(tid);
        assert_eq!(drained, vec!["claude-debug-b".to_string()]);
        assert!(app.pending_spawns_for_task(tid).is_empty());

        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn transition_to_completed_with_no_spawns_does_not_warn() {
        let (app, state_path) = fresh_state();
        let id = seed_accepted_task(&app);

        let out = app
            .transition_task(id, TaskState::Completed)
            .expect("transition");
        assert_eq!(out.state, TaskState::Completed);
        assert!(
            out.summary.is_none() || !out.summary.as_deref().unwrap().contains("[ALERT]"),
            "summary should be untouched when there are no tracked spawns; got {:?}",
            out.summary
        );

        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn transition_to_completed_warns_on_leaked_spawn() {
        // "spawn-only" case: worker spawned but never killed.
        let (app, state_path) = fresh_state();
        let id = seed_accepted_task(&app);

        app.record_task_spawn(id, "codex-debug-leaked");

        let out = app
            .transition_task(id, TaskState::Completed)
            .expect("transition");
        let summary = out.summary.expect("warning should populate summary");
        assert!(summary.contains("[ALERT]"), "summary missing alert marker: {summary}");
        assert!(
            summary.contains("codex-debug-leaked"),
            "summary should name the leaked instance: {summary}"
        );
        assert!(
            summary.contains("agent_kill"),
            "summary should point at the cleanup tool: {summary}"
        );
        // Tracking was drained.
        assert!(app.pending_spawns_for_task(id).is_empty());

        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn transition_to_completed_with_cleanly_killed_spawns_does_not_warn() {
        // "clean spawn+kill" case: worker spawned and explicitly killed.
        let (app, state_path) = fresh_state();
        let id = seed_accepted_task(&app);

        app.record_task_spawn(id, "codex-debug-cleaned");
        // Simulate the cli.delete/cli.kill path clearing the tracking.
        app.clear_task_spawn("codex-debug-cleaned");

        let out = app
            .transition_task(id, TaskState::Completed)
            .expect("transition");
        assert!(
            out.summary.is_none(),
            "no warning expected; got summary: {:?}",
            out.summary
        );

        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn transition_to_completed_multi_spawn_partial_kill_warns_on_remainder() {
        // "multi-spawn partial kill" case: three spawns, one killed,
        // warning should list the two survivors in stable order.
        let (app, state_path) = fresh_state();
        let id = seed_accepted_task(&app);

        app.record_task_spawn(id, "codex-debug-a");
        app.record_task_spawn(id, "claude-debug-b");
        app.record_task_spawn(id, "gemini-debug-c");
        // Kill one.
        app.clear_task_spawn("claude-debug-b");

        let out = app
            .transition_task(id, TaskState::Completed)
            .expect("transition");
        let summary = out.summary.expect("warning should populate summary");
        // Survivors present.
        assert!(summary.contains("codex-debug-a"), "missing 'a': {summary}");
        assert!(summary.contains("gemini-debug-c"), "missing 'c': {summary}");
        // Killed one absent.
        assert!(
            !summary.contains("claude-debug-b"),
            "killed instance should not appear: {summary}"
        );
        // Count is the survivor count.
        assert!(
            summary.contains("2 agent instance"),
            "count should be 2: {summary}"
        );

        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn transition_to_cancelled_also_triggers_leak_warning() {
        // Terminal != just Completed — a cancelled task with leaked
        // spawns should also surface the warning.
        let (app, state_path) = fresh_state();
        let id = seed_accepted_task(&app);

        app.record_task_spawn(id, "codex-debug-x");

        let out = app
            .transition_task(id, TaskState::Cancelled)
            .expect("transition");
        let summary = out.summary.expect("warning should populate summary");
        assert!(summary.contains("codex-debug-x"));
        assert!(summary.contains("[ALERT]"));

        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn leak_warning_appends_to_existing_summary() {
        // If the worker sent a real summary and there's ALSO a leak,
        // the warning should append rather than replace.
        let (app, state_path) = fresh_state();

        let mut t = Task::new("worker summary preserved", "desc");
        t.state = TaskState::Accepted;
        t.summary = Some("Real worker summary text goes here.".to_string());
        let id = t.id;
        app.add_task(t);

        app.record_task_spawn(id, "codex-debug-leak");
        let out = app
            .transition_task(id, TaskState::Completed)
            .expect("transition");
        let summary = out.summary.expect("summary");
        assert!(
            summary.starts_with("Real worker summary text goes here."),
            "worker summary must survive: {summary}"
        );
        assert!(summary.contains("[ALERT]"), "warning must append: {summary}");
        // Two sections separated by blank line.
        assert!(summary.contains("\n\n[ALERT]"));

        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn clear_user_intervention_resets_flag_and_timestamp() {
        // record_user_intervention latches the flag; clear_user_intervention
        // is the orch-facing reset. Used when Fett types into a worker's
        // pane but the intervention is stale (unsubmitted keystrokes,
        // typing mistake, etc.) and shouldn't keep the flag set.
        let nonce = Uuid::new_v4();
        let state_path = std::env::temp_dir().join(format!("alor-test-int-clear-{nonce}.json"));
        let _ = std::fs::remove_file(&state_path);

        let app_state = AppState::with_persistence(state_path.clone());
        let mut t = Task::new("intervention clear", "test");
        t.state = TaskState::Accepted;
        t.assigned_to = Some("cursor-test".to_string());
        let id = t.id;
        app_state.add_task(t);

        // Latch the flag via the normal intervention path.
        let flagged = app_state.record_user_intervention("cursor-test");
        assert_eq!(flagged, vec![id], "our task should be in the flagged set");
        let after_set = app_state.get_task(id).expect("still present");
        assert!(after_set.user_intervened);
        assert!(after_set.user_intervened_at.is_some());
        let set_updated_at = after_set.updated_at;

        // Clear. Should reset both fields + bump updated_at.
        let cleared = app_state
            .clear_user_intervention(id)
            .expect("clear succeeds");
        assert!(!cleared.user_intervened);
        assert_eq!(cleared.user_intervened_at, None);
        assert_ne!(
            cleared.updated_at, set_updated_at,
            "clear must bump updated_at when the flag was actually set"
        );
        // Task state itself is unchanged.
        assert_eq!(cleared.state, TaskState::Accepted);
        assert_eq!(cleared.assigned_to.as_deref(), Some("cursor-test"));

        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn clear_user_intervention_on_unset_flag_is_noop_no_updated_at_bump() {
        // Clearing when the flag was never set must not modify the
        // task (including updated_at). Matters because the orch may
        // defensively call clear any time it re-processes a task.
        let nonce = Uuid::new_v4();
        let state_path = std::env::temp_dir().join(format!("alor-test-int-noop-{nonce}.json"));
        let _ = std::fs::remove_file(&state_path);

        let app_state = AppState::with_persistence(state_path.clone());
        let t = Task::new("never flagged", "test");
        let id = t.id;
        let original_updated_at = t.updated_at;
        app_state.add_task(t);

        let cleared = app_state
            .clear_user_intervention(id)
            .expect("clear succeeds on unset flag");
        assert!(!cleared.user_intervened);
        assert_eq!(
            cleared.updated_at, original_updated_at,
            "no-op clear must not bump updated_at"
        );

        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn clear_user_intervention_unknown_task_errors() {
        let app_state = AppState::new();
        let err = app_state
            .clear_user_intervention(Uuid::new_v4())
            .expect_err("clearing an unknown task must error");
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn cancel_on_already_cancelled_is_noop() {
        // Unique per-test tmp path so parallel runs don't collide.
        let nonce = Uuid::new_v4();
        let state_path = std::env::temp_dir().join(format!("alor-test-cxl-{nonce}.json"));
        let _ = std::fs::remove_file(&state_path);

        let app_state = AppState::with_persistence(state_path.clone());
        let mut t = Task::new("already cancelled", "idempotent-cancel test");
        t.state = TaskState::Cancelled;
        let id = t.id;
        let original_updated_at = t.updated_at;
        app_state.add_task(t);

        // Re-cancelling should succeed (no error) and not mutate the
        // task — specifically, updated_at must not advance.
        let out = app_state
            .transition_task(id, TaskState::Cancelled)
            .expect("cancel-on-cancelled should be a no-op, not an error");
        assert_eq!(out.state, TaskState::Cancelled);
        assert_eq!(out.updated_at, original_updated_at, "no-op must not bump updated_at");

        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn archive_terminal_tasks_sweeps_only_terminal_states_and_is_idempotent() {
        use std::collections::HashSet;

        // Unique per-test tmp paths so parallel runs don't collide.
        let nonce = Uuid::new_v4();
        let tmp_root = std::env::temp_dir();
        let archive_path = tmp_root.join(format!("alor-test-archive-{nonce}.json"));
        let state_path = tmp_root.join(format!("alor-test-state-{nonce}.json"));
        // Belt-and-braces — make sure nothing left over from a prior run.
        let _ = std::fs::remove_file(&archive_path);
        let _ = std::fs::remove_file(&state_path);

        let app_state = AppState::with_persistence(state_path.clone());

        // Mix of states: 2 terminal (Completed, Cancelled), 2 non-terminal
        // (Pending, Accepted). Only the first two should archive.
        let mk = |state: TaskState| {
            let mut t = Task::new(format!("{state:?}"), "test");
            t.state = state;
            t
        };
        let completed = mk(TaskState::Completed);
        let cancelled = mk(TaskState::Cancelled);
        let pending = mk(TaskState::Pending);
        let accepted = mk(TaskState::Accepted);
        let completed_id = completed.id;
        let cancelled_id = cancelled.id;
        let pending_id = pending.id;
        let accepted_id = accepted.id;

        app_state.add_task(completed);
        app_state.add_task(cancelled);
        app_state.add_task(pending);
        app_state.add_task(accepted);
        assert_eq!(app_state.all_tasks().len(), 4);

        // First archive sweep: should move the 2 terminal tasks.
        let moved = app_state.archive_terminal_tasks(&archive_path);
        assert_eq!(moved, 2, "expected exactly 2 terminal tasks archived");

        let live_ids: HashSet<Uuid> =
            app_state.all_tasks().into_iter().map(|t| t.id).collect();
        assert_eq!(live_ids.len(), 2);
        assert!(live_ids.contains(&pending_id));
        assert!(live_ids.contains(&accepted_id));
        assert!(!live_ids.contains(&completed_id));
        assert!(!live_ids.contains(&cancelled_id));

        // Archive file written with both moved tasks.
        assert!(archive_path.exists(), "archive file should exist after sweep");
        let archive_json =
            std::fs::read_to_string(&archive_path).expect("read archive");
        let archive: TasksArchive =
            serde_json::from_str(&archive_json).expect("parse archive");
        assert_eq!(archive.tasks.len(), 2);
        assert!(archive.tasks.contains_key(&completed_id));
        assert!(archive.tasks.contains_key(&cancelled_id));
        assert!(archive.last_archive_run.is_some());
        let first_run_ts = archive.last_archive_run;

        // Second sweep: idempotent — nothing left terminal, nothing moves,
        // archive file untouched (no spurious timestamp bump).
        let moved_again = app_state.archive_terminal_tasks(&archive_path);
        assert_eq!(moved_again, 0, "second call must be a no-op");
        assert_eq!(app_state.all_tasks().len(), 2, "live state unchanged");
        let archive_json2 =
            std::fs::read_to_string(&archive_path).expect("read archive");
        let archive2: TasksArchive =
            serde_json::from_str(&archive_json2).expect("parse archive");
        assert_eq!(archive2.tasks.len(), 2);
        assert_eq!(
            archive2.last_archive_run, first_run_ts,
            "timestamp must not bump on a zero-work sweep"
        );

        // Now flip one of the non-terminal tasks to terminal and sweep again.
        app_state
            .transition_task(accepted_id, TaskState::Completed)
            .expect("Accepted → Completed is legal");
        let moved_third = app_state.archive_terminal_tasks(&archive_path);
        assert_eq!(moved_third, 1);
        let archive3: TasksArchive = serde_json::from_str(
            &std::fs::read_to_string(&archive_path).expect("read archive"),
        )
        .expect("parse archive");
        assert_eq!(archive3.tasks.len(), 3, "archive grew by one");
        assert!(archive3.tasks.contains_key(&accepted_id));
        assert_eq!(app_state.all_tasks().len(), 1);
        assert_eq!(app_state.all_tasks()[0].id, pending_id);

        // Cleanup.
        let _ = std::fs::remove_file(&archive_path);
        let _ = std::fs::remove_file(&state_path);
        let _ = std::fs::remove_file(state_path.with_extension("json.tmp"));
        let _ = std::fs::remove_file(archive_path.with_extension("json.tmp"));
    }
}
