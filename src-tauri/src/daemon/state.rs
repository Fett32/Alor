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
        // Narrow edge: retroactively close out a Cancelled task as Completed
        // (bookkeeping — "I cancelled this, but it was actually done").
        // Cancelled remains terminal for `is_terminal()` purposes so
        // `agent_active_task_count` and `transition_task`'s assigned_to-clear
        // semantics are unchanged.
        if matches!((self, next), (Cancelled, Completed)) {
            return true;
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

impl Task {
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

use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Mutex<StateInner>>,
    save_path: Arc<Option<PathBuf>>,
    /// App handle for emitting push events to the frontend.
    app_handle: Arc<Mutex<Option<AppHandle>>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(StateInner::default())),
            save_path: Arc::new(None),
            app_handle: Arc::new(Mutex::new(None)),
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

    /// Transition a task to a new state.  Returns the updated task on success.
    pub fn transition_task(&self, id: Uuid, next: TaskState) -> anyhow::Result<Task> {
        let is_completing = next == TaskState::Completed;
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
        let result = task.clone();
        drop(s);
        self.save();
        self.emit_event("tasks-changed");
        if is_completing {
            self.emit_event_with("task-completed", result.title.clone());
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
    fn cancelled_to_completed_is_legal_retroactive_closeout() {
        // Cancelled is still terminal for counting/assigned_to semantics …
        assert!(TaskState::Cancelled.is_terminal());
        // … but the narrow retroactive-closeout edge is allowed.
        assert!(TaskState::Cancelled.can_transition_to(&TaskState::Completed));
        // Other terminal states remain fully terminal.
        assert!(!TaskState::Cancelled.can_transition_to(&TaskState::Pending));
        assert!(!TaskState::Cancelled.can_transition_to(&TaskState::Assigned));
        assert!(!TaskState::Completed.can_transition_to(&TaskState::Cancelled));
        assert!(!TaskState::Rejected.can_transition_to(&TaskState::Completed));
    }

    #[test]
    fn task_transition_cancelled_to_completed_actually_applies() {
        let mut t = Task::new("retro", "close out a cancelled task");
        t.state = TaskState::Cancelled;
        t.transition(TaskState::Completed).expect("should succeed");
        assert_eq!(t.state, TaskState::Completed);
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
