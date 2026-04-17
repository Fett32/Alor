use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
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
}
