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
    ///
    /// This is the **terse** post-task summary — one paragraph, hard-
    /// capped server-side at `wrapper::protocol::TASK_SUMMARY_MAX_BYTES`
    /// (512 B) so the orchestrator's `task.completed` event injection
    /// stays cheap. The full report lives in `details` when the worker
    /// emitted one. See the `TaskComplete` docstring in
    /// `src-tauri/src/wrapper/protocol.rs` for the split rationale.
    #[serde(default)]
    pub summary: Option<String>,
    /// Full post-task report from the worker, when the terse `summary`
    /// wasn't enough to carry the whole thing. Capped at 1 MiB. Not
    /// injected into the orchestrator's context automatically — the
    /// orchestrator must call `task_get` to pull this (the
    /// `has_details` flag on the `task.completed` event tells it when
    /// there's something to fetch).
    #[serde(default)]
    pub details: Option<String>,
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

/// Single-task summary projection used by `cli.task.get` when the
/// caller passes `view = "summary"` (the default — see
/// `src-tauri/src/wrapper/protocol.rs::DEFAULT_TASK_GET_VIEW`).
///
/// Different shape from `TaskSummary` (which is for list scans and is
/// deliberately minimal). `TaskGetSummary` is a richer projection
/// sized for routine orchestrator queries — "has this completed? who
/// owns it? is there a proposal waiting? is there a details body I
/// should pull?" — without paying for the heavy text bodies.
///
/// The `has_*` flags are the key affordance: they tell the caller
/// which heavy fields are populated, so it can opt into a follow-up
/// `view = "full"` call only when there's actually something to
/// fetch. `has_summary` / `has_details` come directly from audit
/// 8b03cae6 fix #1's split: the orch's `task.completed` event tells
/// it `has_details` already, and `task_get(view="summary")` re-
/// confirms that flag (plus the others) for later follow-ups.
///
/// Serialize-only; no round-trip use case.
#[derive(Debug, Clone, Serialize)]
pub struct TaskGetSummary {
    pub id: Uuid,
    pub title: String,
    pub state: TaskState,
    pub assigned_to: Option<String>,
    pub project: Option<String>,
    pub parent_task_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub user_intervened: bool,
    /// True when `Task.description` is non-empty. `description` is
    /// set at task creation time (the brief) and is typically the
    /// bulkiest field on a fresh task — the flag lets the orch skip
    /// a follow-up `view="full"` when it already has the brief in
    /// its own context.
    pub has_description: bool,
    /// True when `Task.summary` is populated. Set by worker
    /// completion (terse, ≤512 B cap) or by retroactive close-out
    /// via `cli.task.complete`.
    pub has_summary: bool,
    /// True when `Task.details` is populated — the full post-task
    /// report when the worker split it out from the terse summary
    /// (audit 8b03cae6 fix #1). This is what the orch's event
    /// injection points at with "(Full report available via
    /// task_get)"; the flag re-confirms availability on follow-up
    /// get calls.
    pub has_details: bool,
    pub has_proposal_brief: bool,
    pub has_proposal_diff: bool,
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

    /// Project to the single-task summary-view shape used by
    /// `cli.task.get` (see `TaskGetSummary` for the rationale).
    ///
    /// The `has_*` flags inspect each heavy field:
    ///   - `description` is a `String`, never `None` — treat an empty
    ///     string as "not really populated" so the flag reflects
    ///     user-visible content rather than struct initialization.
    ///   - The `Option<String>` fields (summary / details / proposal_*)
    ///     use `Option::is_some` and ignore whether the inner string
    ///     is empty: an empty-string summary is genuinely rare and a
    ///     caller that wrote one probably wants to pull it.
    pub fn get_summary(&self) -> TaskGetSummary {
        TaskGetSummary {
            id: self.id,
            title: self.title.clone(),
            state: self.state.clone(),
            assigned_to: self.assigned_to.clone(),
            project: self.project.clone(),
            parent_task_id: self.parent_task_id,
            created_at: self.created_at,
            updated_at: self.updated_at,
            user_intervened: self.user_intervened,
            has_description: !self.description.is_empty(),
            has_summary: self.summary.is_some(),
            has_details: self.details.is_some(),
            has_proposal_brief: self.proposal_brief.is_some(),
            has_proposal_diff: self.proposal_diff.is_some(),
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
            details: None,
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

    /// Project to the summary-view shape used by `cli.status`'s
    /// default response. The caller supplies `current_tasks` —
    /// the non-terminal task UUIDs currently assigned to this
    /// agent — which replaces the `task_history` firehose for
    /// routing-decision use cases.
    ///
    /// The `tasks` field on the full `cli.status` response (which
    /// serializes every task in state.json inline) is the real
    /// bulk cost, not task_history itself. This projection drops
    /// task_history too for completeness since callers that want
    /// per-agent history can fetch individual tasks via `task_get`.
    pub fn summary(&self, current_tasks: Vec<Uuid>) -> AgentSummary {
        AgentSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            connected: self.connected,
            project: self.project.clone(),
            tier: self.tier.clone(),
            max_concurrent: self.max_concurrent,
            template: self.template.clone(),
            tmux_session: self.tmux_session.clone(),
            current_tasks,
        }
    }
}

/// Projection of `Agent` used by `cli.status` when the caller
/// requests `view = "summary"` (the default — see
/// `crate::wrapper::protocol::DEFAULT_STATUS_VIEW`).
///
/// Drops `task_history`, `socket_path`, `registered_at`,
/// `working_dir` from the serialized shape. Adds `current_tasks`
/// holding only the non-terminal task UUIDs assigned to this
/// agent at response-build time — enough information for
/// routing decisions (is this slot busy?) without dragging in
/// every past task's description.
///
/// Serialize-only; reconstructing a full `Agent` from a summary
/// on the wire is not needed. ~150-300 bytes per agent depending
/// on project / tmux_session string lengths; orders of magnitude
/// smaller than the full shape when state contains many tasks.
#[derive(Debug, Clone, Serialize)]
pub struct AgentSummary {
    pub id: String,
    pub name: String,
    pub connected: bool,
    pub project: Option<String>,
    pub tier: String,
    pub max_concurrent: u8,
    pub template: Option<String>,
    pub tmux_session: Option<String>,
    /// Non-terminal task UUIDs currently assigned to this agent.
    /// Empty Vec when the slot has no active work.
    pub current_tasks: Vec<Uuid>,
}

// ---------------------------------------------------------------------------
// AppState — Tauri managed state
// ---------------------------------------------------------------------------

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Cached full text of a `worker.orch_response` event, keyed by the
/// `correlation_id` the daemon embedded in the sentinel-prefixed
/// outbound `cli.agent.send_message` that triggered the reply.
///
/// The orchestrator's event formatter caps injected text at
/// `EVENT_TEXT_INJECT_MAX_BYTES` (2048) so a multi-KB reply doesn't
/// blow up the SDK prompt. When it truncates, it appends a pointer to
/// `worker_response_get(correlation_id)` — this record is what that
/// tool returns. Audit 8b03cae6 bloat fix #4.
///
/// Not persisted to state.json: escape-valve only, naturally
/// short-lived, no value in surviving a daemon restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerResponseRecord {
    pub correlation_id: Uuid,
    pub agent_id: String,
    pub text: String,
    pub task_id: Option<Uuid>,
    pub during_task: bool,
    pub timestamp: DateTime<Utc>,
}

/// Bounded LRU of recent `worker.orch_response` full texts. Capped at
/// `WORKER_RESPONSE_CACHE_CAP` entries — when full, the oldest entry
/// is evicted to make room. This is a context-window escape valve
/// for the orchestrator, not an audit log; a bigger cap just wastes
/// memory for replies the orch never comes back to fetch.
///
/// Hash lookup keyed by correlation_id (fast random access for the
/// `worker_response_get` RPC); `VecDeque` on the side tracks
/// insertion order so eviction is O(1). A per-entry write sees one
/// hashmap insert + one deque push_back; evictions (rare — only when
/// the cache is saturated) pop_front + remove.
#[derive(Default)]
struct WorkerResponseCache {
    map: HashMap<Uuid, WorkerResponseRecord>,
    order: VecDeque<Uuid>,
}

/// Maximum number of `worker.orch_response` full texts held in the
/// in-memory fetch-on-demand cache. 100 is plenty for the intended
/// use case (one or two oversized replies per session needing a
/// follow-up fetch); bigger just wastes memory.
pub const WORKER_RESPONSE_CACHE_CAP: usize = 100;

/// Default cap on terminal-state tasks retained in live `state.json`.
/// When the count exceeds this value, `prune_terminal_overflow` evicts
/// the oldest (by `updated_at`, UUID tiebreak) until the cap is met.
/// `0` means "no cap" — unbounded growth, pre-T14 behaviour.
///
/// 1000 is sized off operator expectation rather than a specific
/// benchmark: at ~2 KiB per serialized terminal task it's ~2 MiB of
/// state.json terminals, cheap to load on boot and cheap to serialize
/// on every save. Adjust via `DaemonConfig::max_terminal_tasks_retained`
/// in `~/.config/alor/daemon.yaml` without a recompile. Config knob,
/// not a build-time constant, so an operator can bump it mid-session
/// by restarting the daemon with a different value.
pub const DEFAULT_MAX_TERMINAL_RETAINED: usize = 1000;

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
    /// Fetch-on-demand cache for recent `worker.orch_response` full
    /// texts. See `WorkerResponseCache`. Bounded LRU keyed by
    /// correlation_id, populated in the MSG_WORKER_ORCH_RESPONSE
    /// handler before the event goes out, queried by
    /// `MSG_CLI_WORKER_RESPONSE_GET`.
    worker_responses: Arc<Mutex<WorkerResponseCache>>,
    /// T14: live-retention cap for terminal tasks. Every `save()`
    /// calls `prune_terminal_overflow(cap)` before serializing so the
    /// cap is always enforced at rest. `0` means "no cap" (unbounded).
    /// Populated at startup from `DaemonConfig::max_terminal_tasks_retained`
    /// via `set_max_terminal_retained`; defaults to
    /// `DEFAULT_MAX_TERMINAL_RETAINED` if config is absent.
    ///
    /// `AtomicUsize` because `save()` reads with a shared `&self` and
    /// the setter runs once at startup — atomic Relaxed loads are
    /// strictly cheaper than wrapping a scalar in a Mutex we'd hold
    /// only for the 8-byte read.
    max_terminal_retained: Arc<AtomicUsize>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(StateInner::default())),
            save_path: Arc::new(None),
            app_handle: Arc::new(Mutex::new(None)),
            task_spawns: Arc::new(Mutex::new(HashMap::new())),
            worker_responses: Arc::new(Mutex::new(WorkerResponseCache::default())),
            max_terminal_retained: Arc::new(AtomicUsize::new(DEFAULT_MAX_TERMINAL_RETAINED)),
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct StateInner {
    tasks: HashMap<Uuid, Task>,
    agents: HashMap<String, Agent>,
}

/// Legacy on-disk layout of `tasks-archive.json`. Written historically
/// by the pre-b4102e92 startup sweep; **no longer written** as of T14.
/// After b4102e92 terminal tasks stay live and queryable in
/// `state.json`, and T14 caps that growth via `prune_terminal_overflow`
/// instead of offloading. This struct exists only to deserialize any
/// archive file that survives on disk so `migrate_archive` can
/// rehydrate it back into live state and mark the file `.migrated`.
///
/// Schema matches the original hand-curated archive: a `tasks` map
/// keyed by UUID plus an RFC3339 `last_archive_run` timestamp.
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
            worker_responses: Arc::new(Mutex::new(WorkerResponseCache::default())),
            max_terminal_retained: Arc::new(AtomicUsize::new(DEFAULT_MAX_TERMINAL_RETAINED)),
        }
    }

    /// Set the terminal-retention cap. Called once at startup from
    /// `lib.rs` after loading `DaemonConfig`. `0` disables the cap.
    /// See `prune_terminal_overflow` for semantics.
    pub fn set_max_terminal_retained(&self, cap: usize) {
        self.max_terminal_retained.store(cap, Ordering::Relaxed);
    }

    /// Current terminal-retention cap. `0` = unbounded. Exposed for
    /// tests and introspection.
    pub fn max_terminal_retained(&self) -> usize {
        self.max_terminal_retained.load(Ordering::Relaxed)
    }

    /// Persist current state to disk. Called automatically after mutations.
    /// Clones data under the lock, then writes outside the lock using
    /// atomic rename to prevent corruption.
    ///
    /// T14: before serializing, prunes excess terminal tasks via
    /// `prune_terminal_overflow` so the cap is enforced at rest. The
    /// prune runs in its own lock cycle (acquire, mutate, release)
    /// before the snapshot-under-lock below. Two cycles rather than
    /// one keeps the mutation path and the read-only serialization
    /// path isolated — the serializer never sees a partial prune —
    /// and typical saves that don't evict fast-exit inside the prune
    /// on a `len()` check before any sorting.
    pub fn save(&self) {
        // Prune terminal overflow first. No-op when cap == 0 or the
        // terminal count is already at/below the cap.
        let cap = self.max_terminal_retained.load(Ordering::Relaxed);
        self.prune_terminal_overflow(cap);

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

    /// Evict oldest-first terminal tasks from live state when the
    /// terminal count exceeds `cap`. Called from `save()` on every
    /// persist so the cap is always enforced at rest.
    ///
    /// ## Semantics
    ///
    /// - `cap == 0` → no-op (unbounded). Matches the documented
    ///   `DaemonConfig::max_terminal_tasks_retained = 0` contract.
    /// - `terminal_count <= cap` → no-op. Fast path: one `len()`
    ///   filter count inside the lock, no sort.
    /// - Otherwise, sort terminal tasks by `(updated_at ASC, id ASC)`
    ///   and drop the first `terminal_count - cap` from the live
    ///   map. Non-terminal tasks are never touched.
    ///
    /// ## Eviction order rationale
    ///
    /// Oldest-first by `updated_at` picks the task that became
    /// terminal the *earliest* — the freshly-cancelled task at the
    /// cap boundary is more likely to still be relevant to an
    /// operator reviewing what just happened than a two-week-old
    /// completed task. `transition_task` bumps `updated_at` on every
    /// legal transition (state.rs:408), and terminal-bookkeeping
    /// re-labels (`Completed → Cancelled` etc., line 91–101) also
    /// bump it, so "terminal age" is correctly the age of the
    /// terminal-entry state.
    ///
    /// UUID tiebreak on timestamp collision makes eviction
    /// deterministic in tests — production collisions are rare (ns
    /// clock granularity + single-threaded transition path) but a
    /// mis-synced test wall-clock (CI containers with coarse time)
    /// can produce ties, and non-deterministic eviction would turn
    /// the test into a flake.
    ///
    /// ## Idempotence
    ///
    /// Second call on an already-capped state is a no-op (the
    /// `len() <= cap` early-exit). Means `save()` can invoke this
    /// every persist without per-call bookkeeping.
    ///
    /// ## Return
    ///
    /// Number of tasks evicted (0 on any no-op path).
    pub fn prune_terminal_overflow(&self, cap: usize) -> usize {
        if cap == 0 {
            return 0;
        }
        let mut s = self.inner.lock();

        // Fast path: count terminal-state tasks; skip the sort if
        // we're at or under the cap. Typical production saves never
        // cross the threshold, so this is the hot path.
        let terminal_count = s
            .tasks
            .values()
            .filter(|t| t.state.is_terminal())
            .count();
        if terminal_count <= cap {
            return 0;
        }

        // Collect (updated_at, id) pairs for terminal tasks. Clone-
        // free — we only need the fields we sort on.
        let mut terminal: Vec<(DateTime<Utc>, Uuid)> = s
            .tasks
            .iter()
            .filter_map(|(id, t)| {
                if t.state.is_terminal() {
                    Some((t.updated_at, *id))
                } else {
                    None
                }
            })
            .collect();

        // Oldest first, UUID tiebreak.
        terminal.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

        let to_evict = terminal_count - cap;
        let mut evicted = 0;
        for (_, id) in terminal.iter().take(to_evict) {
            // Re-check is_terminal under the same lock we're
            // mutating — paranoia against a concurrent transition we
            // don't expect here (all mutation goes through this
            // Mutex), but cheap.
            if s.tasks.get(id).map_or(false, |t| t.state.is_terminal()) {
                s.tasks.remove(id);
                evicted += 1;
            }
        }

        if evicted > 0 {
            tracing::info!(
                evicted,
                cap,
                terminal_count,
                "prune_terminal_overflow evicted oldest terminal tasks"
            );
        }
        evicted
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

    /// Stash a `worker.orch_response`'s full text in the fetch-on-
    /// demand cache so the orchestrator can retrieve it via
    /// `worker_response_get(correlation_id)` when its injected copy
    /// was truncated. See `WorkerResponseCache` for the LRU bounds;
    /// audit 8b03cae6 bloat fix #4 for the motivation.
    ///
    /// Idempotent on replay (same correlation_id overwrites the
    /// entry in-place without re-bumping the deque — an unlikely
    /// but benign outcome). Evicts the oldest entry when full.
    pub fn record_worker_response(&self, record: WorkerResponseRecord) {
        let mut cache = self.worker_responses.lock();
        let id = record.correlation_id;
        if cache.map.insert(id, record).is_none() {
            cache.order.push_back(id);
            // Enforce bound. Pop from the FRONT of the deque (oldest
            // inserted); remove from the map. Loop so a cap change
            // (shrink) drains extras in one call.
            while cache.order.len() > WORKER_RESPONSE_CACHE_CAP {
                if let Some(evict) = cache.order.pop_front() {
                    cache.map.remove(&evict);
                }
            }
        }
        // Else: correlation_id was already present — overwrite happened
        // in-place via HashMap::insert; don't duplicate in the deque.
    }

    /// Retrieve a cached `worker.orch_response` full text by its
    /// correlation_id. Returns `None` when the id was never seen or
    /// has been evicted from the LRU. The caller surfaces that to
    /// the orch as a typed error ("correlation_id not found or
    /// expired") rather than a silent empty string so the LLM
    /// doesn't retry forever.
    pub fn get_worker_response(
        &self,
        correlation_id: Uuid,
    ) -> Option<WorkerResponseRecord> {
        self.worker_responses
            .lock()
            .map
            .get(&correlation_id)
            .cloned()
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
    ///
    /// Note: The `MSG_TASK_COMPLETE` handler applies the protocol-level
    /// `TASK_SUMMARY_MAX_BYTES` (512 B) cap *before* calling this, so
    /// wrapper-driven summaries never hit the 1 MiB path in practice.
    /// The wider cap stays as a defense-in-depth safety net for
    /// retroactive summary writes (e.g. orch-driven
    /// `MSG_CLI_TASK_COMPLETE` on cancelled-task promotion).
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

    /// Record a worker-provided full-details report on the task. Mirrors
    /// `set_task_summary` but stores to the `details` field (the "full
    /// report, retrievable via task_get" counterpart to the terse
    /// `summary`). Capped at 1 MiB with UTF-8-boundary truncation.
    ///
    /// Separate setter (rather than reusing `set_task_summary`) so the
    /// two fields have independent lifetimes — e.g. a worker that only
    /// has a terse line skips `details` entirely; a retroactive
    /// close-out via `cli.task.complete` updates `summary` without
    /// overwriting a previously-stashed `details`.
    pub fn set_task_details(&self, id: Uuid, details: String) {
        const MAX_DETAILS_BYTES: usize = 1024 * 1024;
        let text = if details.len() > MAX_DETAILS_BYTES {
            tracing::warn!(task_id = %id, bytes = details.len(), "task details truncated");
            let mut end = MAX_DETAILS_BYTES;
            while end > 0 && !details.is_char_boundary(end) {
                end -= 1;
            }
            let mut d = details;
            d.truncate(end);
            d
        } else {
            details
        };
        let mut s = self.inner.lock();
        if let Some(task) = s.tasks.get_mut(&id) {
            task.details = Some(text);
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

    /// IDs of agents whose per-agent `connected: bool` is currently
    /// true. This is the authoritative "connected index" surfaced in
    /// the `cli.status` response's top-level `connected[]` list.
    ///
    /// Deriving the reported list from the per-agent flag — rather
    /// than from the transport-layer `writers` map on SocketServer —
    /// eliminates the split-brain that bit us when `mark_agent_zombie`
    /// / `mark_agent_killed` flip `connected → false` but deliberately
    /// leave the writers entry in place (so a lingering wrapper
    /// socket can still receive SHUTDOWN). The per-agent flag is the
    /// single source of truth for "does the UI / orchestrator
    /// consider this agent online?"; the writers map remains purely
    /// a transport concern (whose keys the daemon can still write to).
    ///
    /// Because every mutation of `Agent.connected` funnels through
    /// `set_agent_connected`, this derivation is automatically
    /// consistent: callers can't forget to update an auxiliary index.
    /// Order is not stable (HashMap iteration); callers that compare
    /// against a sorted list should sort before comparing.
    pub fn connected_agent_ids(&self) -> Vec<String> {
        self.inner
            .lock()
            .agents
            .values()
            .filter(|a| a.connected)
            .map(|a| a.id.clone())
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
    fn agent_summary_projects_expected_fields_and_current_tasks() {
        // Verify the AgentSummary shape: correct field selection,
        // task_history dropped, current_tasks populated from the
        // caller-supplied Vec. Mirrors the contract
        // `server.rs::MSG_CLI_STATUS` relies on.
        let mut a = Agent::new("cursor-alor", "@ cursor-alor");
        a.connected = true;
        a.project = Some("alor".to_string());
        a.tier = "light".to_string();
        a.max_concurrent = 1;
        a.template = Some("cursor".to_string());
        a.tmux_session = Some("alor-cursor-alor".to_string());
        a.working_dir = Some("~/Projects/Alor".to_string());
        // Populate task_history to confirm it DOESN'T flow into summary.
        for _ in 0..AGENT_TASK_HISTORY_MAX {
            a.push_task_history(Uuid::new_v4());
        }
        assert_eq!(a.task_history.len(), AGENT_TASK_HISTORY_MAX);

        let active = vec![Uuid::new_v4(), Uuid::new_v4()];
        let s = a.summary(active.clone());
        assert_eq!(s.id, "cursor-alor");
        assert_eq!(s.name, "@ cursor-alor");
        assert!(s.connected);
        assert_eq!(s.project.as_deref(), Some("alor"));
        assert_eq!(s.tier, "light");
        assert_eq!(s.template.as_deref(), Some("cursor"));
        assert_eq!(s.tmux_session.as_deref(), Some("alor-cursor-alor"));
        assert_eq!(s.current_tasks, active);

        // Serialize; confirm heavy/unneeded fields aren't on the wire.
        let json = serde_json::to_value(&s).expect("summary serializes");
        let obj = json.as_object().expect("is JSON object");
        let keys: std::collections::BTreeSet<&str> =
            obj.keys().map(String::as_str).collect();
        let expected: std::collections::BTreeSet<&str> = [
            "id",
            "name",
            "connected",
            "project",
            "tier",
            "max_concurrent",
            "template",
            "tmux_session",
            "current_tasks",
        ]
        .iter()
        .copied()
        .collect();
        assert_eq!(
            keys, expected,
            "AgentSummary must serialize exactly these 9 fields"
        );
        // Specifically, task_history / socket_path / registered_at /
        // working_dir must NOT be in the wire shape.
        for dropped in [
            "task_history",
            "socket_path",
            "registered_at",
            "working_dir",
        ] {
            assert!(
                !obj.contains_key(dropped),
                "summary wire shape must drop `{dropped}`"
            );
        }
    }

    #[test]
    fn status_summary_payload_is_at_least_10x_smaller_than_full() {
        // Simulates what the MSG_CLI_STATUS handler serializes for
        // each view, side-by-side, with realistic state: 1 agent +
        // 50 tasks each carrying a description of typical size.
        // The full response inlines every task; the summary drops
        // them entirely. The brief sets a 10x minimum size ratio
        // and expects the realistic case to clear it easily — this
        // test bakes that expectation in as a regression guard.
        use serde_json::json;

        let app_state = AppState::new();
        let mut agent = Agent::new("claude-alor", "* claude-alor");
        agent.connected = true;
        agent.project = Some("alor".to_string());
        agent.tmux_session = Some("alor-claude-alor".to_string());
        app_state.register_agent(agent);

        // 50 tasks with roughly production-sized descriptions — a
        // realistic session accumulates tasks with multi-KB briefs.
        // Using 2 KB each: 50 tasks × 2 KB ≈ 100 KB of task body
        // payload in the full shape.
        let bulky_desc: String = "abcdefghij".repeat(200); // ~2 KB
        let mut active_ids: Vec<Uuid> = Vec::new();
        for i in 0..50 {
            let mut t = Task::new(format!("task-{i}"), &bulky_desc);
            if i < 3 {
                t.state = TaskState::Accepted;
                t.assigned_to = Some("claude-alor".to_string());
                active_ids.push(t.id);
            } else {
                t.state = TaskState::Completed;
            }
            app_state.add_task(t);
        }

        // Full shape: what the server returns on view=full.
        let agents = app_state.all_agents();
        let tasks = app_state.all_tasks();
        let connected: Vec<String> = vec!["claude-alor".to_string()];
        let full_resp = json!({
            "view": "full",
            "agents": agents,
            "tasks": tasks,
            "connected": connected,
        });
        let full_bytes = serde_json::to_vec(&full_resp).expect("serialize full").len();

        // Summary shape: what the server returns on view=summary.
        // Rebuild the per-agent projection inline so the test
        // exercises the actual projection logic + size, not just
        // a hand-waved estimate.
        let summary_agents: Vec<_> = app_state
            .all_agents()
            .into_iter()
            .map(|a| {
                let current: Vec<Uuid> = app_state
                    .all_tasks()
                    .into_iter()
                    .filter(|t| {
                        !t.state.is_terminal()
                            && t.assigned_to.as_deref() == Some(&a.id)
                    })
                    .map(|t| t.id)
                    .collect();
                a.summary(current)
            })
            .collect();
        let summary_resp = json!({
            "view": "summary",
            "agents": summary_agents,
            "connected": connected,
        });
        let summary_bytes =
            serde_json::to_vec(&summary_resp).expect("serialize summary").len();

        // The brief asks for ≥10x reduction; realistic data should
        // blow past that. Asserting the exact ratio guards against
        // regressions where someone re-introduces a heavy field
        // into AgentSummary.
        let ratio = full_bytes as f64 / summary_bytes as f64;
        assert!(
            ratio >= 10.0,
            "summary payload must be at least 10x smaller than full; \
             got summary={} bytes, full={} bytes, ratio={:.1}x",
            summary_bytes,
            full_bytes,
            ratio,
        );

        // Also assert the summary actually carries the active task
        // IDs — a regression where current_tasks gets dropped would
        // defeat the whole point of the projection.
        let summary_obj = summary_resp
            .as_object()
            .unwrap();
        let agents_json = summary_obj.get("agents").unwrap().as_array().unwrap();
        assert_eq!(agents_json.len(), 1);
        let current = agents_json[0]
            .get("current_tasks")
            .and_then(|v| v.as_array())
            .expect("current_tasks present");
        assert_eq!(
            current.len(),
            3,
            "agent's 3 non-terminal tasks must be in current_tasks"
        );
    }

    #[test]
    fn task_get_summary_projects_expected_shape_and_has_flags() {
        // Audit 8b03cae6 fix #2 (task_get view modes). The summary
        // shape covers routine routing/state-check calls: scalar
        // fields the orch always wants + `has_*` booleans that tell
        // it which heavy fields exist without pulling them. This
        // test pins (a) the serialized key set and (b) the has_*
        // booleans for an unpopulated task.
        use std::collections::BTreeSet;
        let t = Task::new("title", "a description");
        let s = t.get_summary();

        assert_eq!(s.id, t.id);
        assert_eq!(s.title, "title");
        assert_eq!(s.state, TaskState::Pending);
        assert!(s.assigned_to.is_none());
        assert!(s.project.is_none());
        assert!(s.parent_task_id.is_none());
        assert_eq!(s.created_at, t.created_at);
        assert_eq!(s.updated_at, t.updated_at);
        assert!(!s.user_intervened);
        // Fresh-from-new task — description is non-empty, heavy
        // fields are all None.
        assert!(s.has_description);
        assert!(!s.has_summary);
        assert!(!s.has_details);
        assert!(!s.has_proposal_brief);
        assert!(!s.has_proposal_diff);

        // Serialize and confirm the exact wire shape. The `has_*`
        // flags are the key affordance — regressing any of them
        // (typo in the field name, accidental rename) would silently
        // break the orch's "is there something worth fetching?"
        // decision.
        let json = serde_json::to_value(&s).expect("serializes");
        let obj = json.as_object().expect("JSON object");
        let keys: BTreeSet<&str> = obj.keys().map(String::as_str).collect();
        let expected: BTreeSet<&str> = [
            "id",
            "title",
            "state",
            "assigned_to",
            "project",
            "parent_task_id",
            "created_at",
            "updated_at",
            "user_intervened",
            "has_description",
            "has_summary",
            "has_details",
            "has_proposal_brief",
            "has_proposal_diff",
        ]
        .iter()
        .copied()
        .collect();
        assert_eq!(
            keys, expected,
            "TaskGetSummary wire shape must carry exactly these 14 keys"
        );
        // Specifically, the heavy bodies must NOT be in the wire
        // shape — they're the whole reason this projection exists.
        for dropped in ["description", "summary", "details", "proposal_brief", "proposal_diff"] {
            assert!(
                !obj.contains_key(dropped),
                "TaskGetSummary must NOT serialize `{dropped}`"
            );
        }
    }

    #[test]
    fn task_get_summary_has_flags_flip_true_when_fields_populated() {
        // All heavy fields populated → every has_* flag true.
        // Pins the "is the field populated?" logic for each flag so
        // a future refactor (e.g. swapping Option for String with
        // sentinel-empty) doesn't silently break the affordance.
        let mut t = Task::new("title", "a real description");
        t.summary = Some("terse line".to_string());
        t.details = Some("full report body".to_string());
        t.proposal_brief = Some("what this will do".to_string());
        t.proposal_diff = Some("--- a/x.rs\n+++ b/x.rs".to_string());
        t.project = Some("alor".to_string());
        t.assigned_to = Some("claude-alor".to_string());
        t.user_intervened = true;

        let s = t.get_summary();
        assert!(s.has_description);
        assert!(s.has_summary);
        assert!(s.has_details);
        assert!(s.has_proposal_brief);
        assert!(s.has_proposal_diff);
        assert!(s.user_intervened);
        assert_eq!(s.project.as_deref(), Some("alor"));
        assert_eq!(s.assigned_to.as_deref(), Some("claude-alor"));

        // Edge: description is a `String`, never None, so an empty
        // string must read as "not populated" — otherwise every
        // freshly-created task without a brief would claim to have
        // one and the orch would waste a view=full call fetching
        // nothing.
        let empty = Task::new("title", "");
        assert!(
            !empty.get_summary().has_description,
            "empty description must not set has_description"
        );
    }

    #[test]
    fn task_get_summary_is_at_least_5x_smaller_than_full_for_populated_task() {
        // Acceptance criterion from the task spec: "full-view payload
        // is at least 5× larger than summary-view for a task with
        // populated description + summary + details + proposal_brief
        // + proposal_diff". Simulates what the MSG_CLI_TASK_GET
        // handler serializes for each view, side-by-side, with a
        // realistic populated task.
        use serde_json::json;

        let mut t = Task::new("title", "a".repeat(500)); // ~500 B description
        t.state = TaskState::Accepted;
        t.assigned_to = Some("claude-alor".to_string());
        t.project = Some("alor".to_string());
        t.summary = Some("b".repeat(400)); // near the 512 B cap
        t.details = Some("c".repeat(2_000)); // realistic worker report
        t.proposal_brief = Some("d".repeat(300));
        t.proposal_diff = Some("e".repeat(4_000)); // a modest diff

        // Full shape: what the handler returns on view=full.
        let full_resp = json!({"view": "full", "task": &t});
        let full_bytes = serde_json::to_vec(&full_resp).expect("full serializes").len();

        // Summary shape: what the handler returns on view=summary.
        let summary_resp = json!({"view": "summary", "task": t.get_summary()});
        let summary_bytes = serde_json::to_vec(&summary_resp)
            .expect("summary serializes")
            .len();

        let ratio = full_bytes as f64 / summary_bytes as f64;
        assert!(
            ratio >= 5.0,
            "summary payload must be at least 5x smaller than full; \
             got summary={} bytes, full={} bytes, ratio={:.1}x",
            summary_bytes,
            full_bytes,
            ratio,
        );

        // Additionally: the acceptance criterion in the task spec is
        // "Routine `task_get` call on an ACCEPTED task drops from
        // ~2–10 KB response to <400 B". Our projection under a
        // realistic populated task must clear that bar. 500 B of
        // leeway for timestamps + long strings.
        assert!(
            summary_bytes < 500,
            "summary response on a populated task must stay well under \
             the acceptance ceiling (<500 B); got {} bytes",
            summary_bytes,
        );
    }

    #[test]
    fn worker_response_cache_records_and_retrieves_by_correlation_id() {
        // Audit 8b03cae6 bloat fix #4: the orchestrator-side formatter
        // caps worker.orch_response text at 2 KiB and leaves a pointer
        // to worker_response_get. This is the storage the pointer
        // resolves against. Baseline round-trip: record one, fetch
        // by the same uuid, every field comes back.
        let state = AppState::new();
        let cid = Uuid::new_v4();
        let tid = Uuid::new_v4();
        state.record_worker_response(WorkerResponseRecord {
            correlation_id: cid,
            agent_id: "claude-alor".to_string(),
            text: "a".repeat(3_000),
            task_id: Some(tid),
            during_task: true,
            timestamp: Utc::now(),
        });

        let got = state.get_worker_response(cid).expect("present");
        assert_eq!(got.correlation_id, cid);
        assert_eq!(got.agent_id, "claude-alor");
        assert_eq!(got.text.len(), 3_000);
        assert_eq!(got.task_id, Some(tid));
        assert!(got.during_task);
    }

    #[test]
    fn worker_response_cache_returns_none_for_unknown_id() {
        // Unknown correlation_id must surface as `None` so the RPC
        // handler can return a typed cli_error rather than an
        // empty-string silent success.
        let state = AppState::new();
        let unknown = Uuid::new_v4();
        assert!(state.get_worker_response(unknown).is_none());
    }

    #[test]
    fn worker_response_cache_evicts_oldest_when_full() {
        // LRU bound: insert CAP+10 entries; the first 10 must be
        // evicted. Asserts the FIFO-by-insert order (we don't model
        // access recency; fetch-on-demand is write-heavy read-light
        // and a simple FIFO is cheaper + easier to reason about).
        let state = AppState::new();
        let n = WORKER_RESPONSE_CACHE_CAP + 10;
        let mut ids = Vec::with_capacity(n);
        for i in 0..n {
            let cid = Uuid::new_v4();
            ids.push(cid);
            state.record_worker_response(WorkerResponseRecord {
                correlation_id: cid,
                agent_id: "a".to_string(),
                text: format!("entry-{i}"),
                task_id: None,
                during_task: false,
                timestamp: Utc::now(),
            });
        }

        // The first 10 (oldest) must have been evicted.
        for (i, cid) in ids.iter().take(10).enumerate() {
            assert!(
                state.get_worker_response(*cid).is_none(),
                "entry {i} (oldest) must have been evicted at cap",
            );
        }
        // The most recent CAP entries must still be present.
        for (i, cid) in ids.iter().skip(10).enumerate() {
            assert!(
                state.get_worker_response(*cid).is_some(),
                "entry {} (within CAP window) must still be present",
                i + 10,
            );
        }
    }

    #[test]
    fn worker_response_cache_overwrites_same_correlation_id_without_growing() {
        // Edge: replay or same-uuid re-record. Must overwrite the
        // text in place without duplicating in the deque — otherwise
        // a malicious / buggy worker spamming the same correlation_id
        // would evict legitimate entries via fake "overwrite" pressure.
        let state = AppState::new();
        let cid = Uuid::new_v4();

        for i in 0..20 {
            state.record_worker_response(WorkerResponseRecord {
                correlation_id: cid,
                agent_id: "a".to_string(),
                text: format!("version-{i}"),
                task_id: None,
                during_task: false,
                timestamp: Utc::now(),
            });
        }

        // 20 overwrites of the same id should leave only one entry.
        // Fill to CAP with DIFFERENT ids and confirm our sentinel
        // entry (still at the front of insert order) is NOT evicted
        // until we actually overflow with unique entries.
        for _ in 0..(WORKER_RESPONSE_CACHE_CAP - 1) {
            state.record_worker_response(WorkerResponseRecord {
                correlation_id: Uuid::new_v4(),
                agent_id: "a".to_string(),
                text: "filler".to_string(),
                task_id: None,
                during_task: false,
                timestamp: Utc::now(),
            });
        }
        assert!(
            state.get_worker_response(cid).is_some(),
            "same-id overwrites must count as one entry in the deque"
        );
        // Latest value wins.
        assert_eq!(
            state.get_worker_response(cid).unwrap().text,
            "version-19",
            "latest overwrite wins in the in-place update path"
        );
    }

    #[test]
    fn set_task_details_round_trips_and_is_independent_of_summary() {
        // Task.details is the on-demand full-report counterpart to the
        // capped `summary`. The two fields must be independently
        // writable: setting details doesn't clobber summary, and vice
        // versa. Task_get returns both (the whole Task struct) so this
        // is the contract the orch relies on when it follows the
        // "full report via task_get" pointer.
        let state = AppState::new();
        let task = Task::new("title", "desc");
        let id = task.id;
        state.add_task(task);

        // Baseline: both fields start None.
        let t = state.get_task(id).unwrap();
        assert!(t.summary.is_none() && t.details.is_none());

        // Write summary only → details stays None.
        state.set_task_summary(id, "terse".to_string());
        let t = state.get_task(id).unwrap();
        assert_eq!(t.summary.as_deref(), Some("terse"));
        assert!(t.details.is_none());

        // Write details → summary is preserved.
        state.set_task_details(id, "full report body".to_string());
        let t = state.get_task(id).unwrap();
        assert_eq!(t.summary.as_deref(), Some("terse"));
        assert_eq!(t.details.as_deref(), Some("full report body"));

        // Overwrite summary → details preserved.
        state.set_task_summary(id, "updated terse".to_string());
        let t = state.get_task(id).unwrap();
        assert_eq!(t.summary.as_deref(), Some("updated terse"));
        assert_eq!(t.details.as_deref(), Some("full report body"));
    }

    #[test]
    fn set_task_details_truncates_at_1mib_on_utf8_boundary() {
        // The 1 MiB cap mirrors set_task_summary — defense-in-depth
        // against a worker shoveling a core dump into the field.
        // Protocol-level cap on summary is 512 B; this 1 MiB cap is
        // the storage-layer backstop.
        let state = AppState::new();
        let task = Task::new("t", "d");
        let id = task.id;
        state.add_task(task);

        // 1 MiB + 100 bytes of 2-byte UTF-8 so the cut can land mid-
        // codepoint if we're sloppy about boundaries.
        let over: String = "á".repeat((1024 * 1024 / 2) + 50);
        assert!(over.len() > 1024 * 1024);
        state.set_task_details(id, over);

        let stored = state.get_task(id).unwrap().details.unwrap();
        assert!(
            stored.len() <= 1024 * 1024,
            "details must be capped at 1 MiB; got {} bytes",
            stored.len()
        );
        // Valid UTF-8 — if the truncation landed mid-codepoint, stored
        // wouldn't be a valid String at all (impossible, it's typed).
        // Assert structurally that char iteration completes without
        // panic and the final byte is a char boundary.
        assert!(stored.is_char_boundary(stored.len()));
        let _ = stored.chars().count();
    }

    #[test]
    fn connected_agent_ids_tracks_set_agent_connected_flips() {
        // Regression: `agent_list` used to surface the writers-map keys
        // as its top-level `connected[]`, which drifted from the
        // per-agent `connected` flag under `mark_agent_zombie` /
        // `mark_agent_killed` (those deliberately leave writers
        // untouched so a lingering socket can still receive SHUTDOWN).
        // Live repro on cursor-alor: top-level `connected` included
        // the id while the agent row said `connected: false`.
        //
        // The fix pins the reported list to the per-agent flag via
        // `AppState::connected_agent_ids`. Because every mutation of
        // `Agent.connected` funnels through `set_agent_connected`,
        // flipping the flag automatically moves the id into or out
        // of the reported index — no auxiliary structure to forget.
        let state = AppState::new();

        // Baseline: empty index before any agents exist.
        assert!(state.connected_agent_ids().is_empty());

        // Auto-register via set_agent_connected(true) — the production
        // wrapper-register path. Agent must appear in the index.
        state
            .set_agent_connected("claude-alor", true)
            .expect("first connect");
        let ids = state.connected_agent_ids();
        assert_eq!(ids, vec!["claude-alor".to_string()]);

        // Second agent added; index grows.
        state
            .set_agent_connected("cursor-alor", true)
            .expect("second connect");
        let mut ids = state.connected_agent_ids();
        ids.sort();
        assert_eq!(
            ids,
            vec!["claude-alor".to_string(), "cursor-alor".to_string()]
        );

        // Flip cursor-alor → false via set_agent_connected. This is
        // the code path exercised by mark_agent_zombie /
        // mark_agent_killed / the read-loop cleanup. It must remove
        // the id from the index.
        state
            .set_agent_connected("cursor-alor", false)
            .expect("disconnect flip");
        let ids = state.connected_agent_ids();
        assert_eq!(
            ids,
            vec!["claude-alor".to_string()],
            "disconnected agent must be removed from the index"
        );

        // The per-agent row survives (we tombstone only via
        // remove_agent / delete_agent); its flag just reads false.
        let a = state
            .get_agent("cursor-alor")
            .expect("row preserved on disconnect");
        assert!(!a.connected);

        // Flip back to true: the id must re-appear.
        state
            .set_agent_connected("cursor-alor", true)
            .expect("reconnect flip");
        let mut ids = state.connected_agent_ids();
        ids.sort();
        assert_eq!(
            ids,
            vec!["claude-alor".to_string(), "cursor-alor".to_string()],
            "reconnected agent must be re-added to the index"
        );

        // Idempotent double-false: repeated disconnects must not
        // produce a negative / duplicate entry. (set_agent_connected
        // already tolerates this; belt-and-braces here because the
        // live repro originated from exactly this shape — repeated
        // disconnect signals arriving via different paths.)
        state
            .set_agent_connected("cursor-alor", false)
            .expect("first disconnect");
        state
            .set_agent_connected("cursor-alor", false)
            .expect("second disconnect is idempotent");
        let ids = state.connected_agent_ids();
        assert_eq!(ids, vec!["claude-alor".to_string()]);
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

    // ---- T14 prune_terminal_overflow ----

    /// Helper: build a terminal-state task with a synthetic `updated_at`
    /// so eviction-order tests don't rely on real-clock ordering of
    /// `transition_task` calls (which would be racy on a fast machine).
    fn terminal_task_at(state: TaskState, updated_at: DateTime<Utc>) -> Task {
        assert!(
            state.is_terminal(),
            "helper is only for terminal states, got {state:?}"
        );
        let mut t = Task::new(format!("{state:?}"), "test");
        t.state = state;
        t.updated_at = updated_at;
        t
    }

    /// Helper: non-terminal task so we can verify the prune leaves
    /// live work untouched even when its `updated_at` is older than
    /// the evicted terminals.
    fn pending_task_at(updated_at: DateTime<Utc>) -> Task {
        let mut t = Task::new("pending", "test");
        t.state = TaskState::Pending;
        t.updated_at = updated_at;
        t
    }

    #[test]
    fn prune_terminal_overflow_evicts_oldest_first_beyond_cap() {
        // 5 terminal tasks + 2 live, cap=3 → oldest 2 terminals
        // evicted, live untouched even though they're older still.
        let app = AppState::new();
        let base = Utc::now();
        // Seed in shuffled insertion order to prove we sort, not
        // rely on HashMap iteration order.
        let t_old_live = pending_task_at(base - chrono::Duration::hours(24));
        let t_oldest = terminal_task_at(TaskState::Completed, base - chrono::Duration::hours(10));
        let t_mid2 = terminal_task_at(TaskState::Cancelled, base - chrono::Duration::hours(4));
        let t_mid1 = terminal_task_at(TaskState::Stale, base - chrono::Duration::hours(8));
        let t_old2 = terminal_task_at(TaskState::TimedOut, base - chrono::Duration::hours(9));
        let t_live = pending_task_at(base - chrono::Duration::hours(12));
        let t_fresh = terminal_task_at(TaskState::Rejected, base - chrono::Duration::hours(1));

        let oldest_id = t_oldest.id;
        let old2_id = t_old2.id;
        let mid1_id = t_mid1.id;
        let mid2_id = t_mid2.id;
        let fresh_id = t_fresh.id;
        let live_id = t_live.id;
        let old_live_id = t_old_live.id;

        for task in [t_old_live, t_oldest, t_mid2, t_mid1, t_old2, t_live, t_fresh] {
            app.add_task(task);
        }

        let evicted = app.prune_terminal_overflow(3);
        assert_eq!(evicted, 2, "cap=3 over 5 terminals must evict 2");

        let live_ids: std::collections::HashSet<Uuid> =
            app.all_tasks().into_iter().map(|t| t.id).collect();
        // Evicted: the two OLDEST terminal tasks (10h, 9h).
        assert!(!live_ids.contains(&oldest_id), "oldest terminal evicted");
        assert!(!live_ids.contains(&old2_id), "second-oldest terminal evicted");
        // Kept: 3 terminals (cap) + 2 non-terminals (never touched).
        assert!(live_ids.contains(&mid1_id), "8h terminal kept");
        assert!(live_ids.contains(&mid2_id), "4h terminal kept");
        assert!(live_ids.contains(&fresh_id), "1h terminal kept");
        assert!(live_ids.contains(&live_id), "Pending task kept");
        assert!(
            live_ids.contains(&old_live_id),
            "24h-old Pending task kept — non-terminals never evicted even when older than terminals"
        );
        assert_eq!(live_ids.len(), 5);
    }

    #[test]
    fn prune_terminal_overflow_cap_zero_is_noop_unbounded() {
        // `cap == 0` is the documented "no cap" semantic. Must be a
        // true no-op even with hundreds of terminals — the config
        // knob's 0-means-unbounded contract is user-visible.
        let app = AppState::new();
        let base = Utc::now();
        for i in 0..20 {
            app.add_task(terminal_task_at(
                TaskState::Completed,
                base - chrono::Duration::minutes(i),
            ));
        }
        assert_eq!(app.all_tasks().len(), 20);
        let evicted = app.prune_terminal_overflow(0);
        assert_eq!(evicted, 0, "cap=0 must evict nothing");
        assert_eq!(app.all_tasks().len(), 20);
    }

    #[test]
    fn prune_terminal_overflow_idempotent_when_at_or_below_cap() {
        // At cap: no eviction, return 0. Second call must also
        // return 0 without mutating state (prune is called on every
        // save, so idempotence == zero-cost steady state).
        let app = AppState::new();
        let base = Utc::now();
        for i in 0..3 {
            app.add_task(terminal_task_at(
                TaskState::Completed,
                base - chrono::Duration::minutes(i),
            ));
        }
        assert_eq!(app.prune_terminal_overflow(5), 0, "below cap, no evict");
        assert_eq!(app.prune_terminal_overflow(3), 0, "at cap exactly, no evict");
        assert_eq!(app.all_tasks().len(), 3, "state unchanged");
        // A third call on the already-steady state stays a no-op.
        assert_eq!(app.prune_terminal_overflow(3), 0);
        assert_eq!(app.all_tasks().len(), 3);
    }

    #[test]
    fn prune_terminal_overflow_treats_every_terminal_state_identically() {
        // `is_terminal()` at HEAD covers Completed, Cancelled, Rejected,
        // TimedOut, Stale. Prune must not preference one state over
        // another — only `updated_at` age matters. Feed one of each,
        // spaced 1 minute apart; with cap=2, evict the 3 oldest and
        // keep the 2 newest.
        //
        // If a future branch adds a new terminal variant (e.g. the
        // in-flight `AcceptFailed` on the accept-watchdog work),
        // whoever lands that variant should grow this list too.
        // Intentionally NOT auto-discovered via a macro — keeping the
        // list explicit surfaces the grow-this-test step in review.
        let app = AppState::new();
        let base = Utc::now();
        let states = [
            TaskState::Completed,
            TaskState::Cancelled,
            TaskState::Rejected,
            TaskState::TimedOut,
            TaskState::Stale,
        ];
        let ids: Vec<Uuid> = states
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let t = terminal_task_at(
                    s.clone(),
                    base - chrono::Duration::minutes((states.len() - i) as i64),
                );
                let id = t.id;
                app.add_task(t);
                id
            })
            .collect();
        // states[0] is oldest (minus 5min), states[4] newest (minus 1min).

        let evicted = app.prune_terminal_overflow(2);
        assert_eq!(evicted, 3, "5 terminals, cap=2 → 3 evicted");

        let survivors: std::collections::HashSet<Uuid> =
            app.all_tasks().into_iter().map(|t| t.id).collect();
        // Oldest 3 evicted regardless of which terminal variant.
        for id in &ids[..3] {
            assert!(!survivors.contains(id), "oldest 3 variants evicted");
        }
        // Newest 2 kept regardless of variant.
        for id in &ids[3..] {
            assert!(survivors.contains(id), "newest 2 variants kept");
        }
    }

    #[test]
    fn prune_terminal_overflow_tiebreak_on_uuid_is_deterministic() {
        // Two terminal tasks with identical `updated_at` — test
        // clock collisions, CI coarse-time containers, etc. UUID
        // tiebreak gives deterministic eviction so this test can
        // assert exactly WHICH one survives when cap=1.
        let app = AppState::new();
        let ts = Utc::now() - chrono::Duration::hours(1);
        let mut a = terminal_task_at(TaskState::Completed, ts);
        let mut b = terminal_task_at(TaskState::Completed, ts);
        // Force known UUID ordering so the tiebreak is unambiguous.
        a.id = Uuid::from_u128(0x1000_0000_0000_0000_0000_0000_0000_0000);
        b.id = Uuid::from_u128(0x2000_0000_0000_0000_0000_0000_0000_0000);
        let a_id = a.id;
        let b_id = b.id;
        app.add_task(a);
        app.add_task(b);

        let evicted = app.prune_terminal_overflow(1);
        assert_eq!(evicted, 1);
        let survivors: std::collections::HashSet<Uuid> =
            app.all_tasks().into_iter().map(|t| t.id).collect();
        // Smaller UUID sorts first → evicted. Larger UUID survives.
        assert!(!survivors.contains(&a_id), "lower-UUID evicted on tiebreak");
        assert!(survivors.contains(&b_id), "higher-UUID retained on tiebreak");
    }

    #[test]
    fn prune_terminal_overflow_transition_bumps_updated_at_and_saves_from_eviction() {
        // Freshly-transitioned-to-terminal task must NOT be evicted
        // before older terminals. `transition_task` bumps
        // `updated_at` on the state change, so even if the task was
        // created earlier, its terminal-entry age is young and
        // the cap boundary should spare it.
        let app = AppState::new();
        let now = Utc::now();

        // Two old pre-existing terminals, 10h and 11h ago.
        let old_a = terminal_task_at(TaskState::Completed, now - chrono::Duration::hours(11));
        let old_b = terminal_task_at(TaskState::Completed, now - chrono::Duration::hours(10));
        let old_a_id = old_a.id;
        let old_b_id = old_b.id;
        app.add_task(old_a);
        app.add_task(old_b);

        // A task created MUCH earlier in Accepted state but
        // transitioned to Completed just now (bump). Its pre-
        // transition updated_at was the oldest of all three; post-
        // transition it's the youngest.
        let mut accepted = Task::new("long-running", "test");
        accepted.state = TaskState::Accepted;
        accepted.updated_at = now - chrono::Duration::hours(48);
        let accepted_id = accepted.id;
        app.add_task(accepted);
        app.transition_task(accepted_id, TaskState::Completed)
            .expect("Accepted → Completed");

        // Cap=2 over 3 terminals → evict the oldest ONE (old_a).
        // The fresh transition must keep the just-transitioned task
        // alive even though its creation is ancient.
        let evicted = app.prune_terminal_overflow(2);
        assert_eq!(evicted, 1);
        let survivors: std::collections::HashSet<Uuid> =
            app.all_tasks().into_iter().map(|t| t.id).collect();
        assert!(!survivors.contains(&old_a_id), "oldest terminal evicted");
        assert!(survivors.contains(&old_b_id), "second-oldest kept");
        assert!(
            survivors.contains(&accepted_id),
            "freshly-transitioned task survives — transition bumped updated_at"
        );
    }

    #[test]
    fn prune_terminal_overflow_leaves_non_terminal_tasks_alone_even_when_all_old() {
        // Edge case: cap=0 terminals, a bunch of ancient non-terminal
        // tasks. Must be a no-op — non-terminals participate in
        // neither the count nor the eviction. The cap is terminal-
        // specific by design.
        let app = AppState::new();
        let long_ago = Utc::now() - chrono::Duration::days(365);
        for _ in 0..10 {
            app.add_task(pending_task_at(long_ago));
        }
        let evicted = app.prune_terminal_overflow(1);
        assert_eq!(evicted, 0);
        assert_eq!(app.all_tasks().len(), 10);
    }

    #[test]
    fn save_invokes_prune_with_configured_cap() {
        // Integration: save() must call prune at its configured cap.
        // Setting cap=2 then save()-ing an app with 4 terminal tasks
        // should leave exactly 2 on disk (the two newest). This
        // locks the save()→prune wiring against a future refactor
        // that forgets to invoke the sweep.
        let nonce = Uuid::new_v4();
        let state_path = std::env::temp_dir().join(format!("alor-test-t14-save-{nonce}.json"));
        let _ = std::fs::remove_file(&state_path);

        let app = AppState::with_persistence(state_path.clone());
        app.set_max_terminal_retained(2);
        assert_eq!(app.max_terminal_retained(), 2);

        let base = Utc::now();
        for i in 0..4 {
            app.add_task(terminal_task_at(
                TaskState::Completed,
                base - chrono::Duration::hours(i),
            ));
        }
        // add_task already triggers save() internally in the real
        // daemon; call it explicitly here to match that path.
        app.save();

        // Two newest terminals survive in-memory AND on disk.
        assert_eq!(app.all_tasks().len(), 2);
        let on_disk = std::fs::read_to_string(&state_path).expect("read state");
        let parsed: StateInner = serde_json::from_str(&on_disk).expect("parse");
        assert_eq!(parsed.tasks.len(), 2, "on-disk tasks match in-memory post-prune");

        let _ = std::fs::remove_file(&state_path);
        let _ = std::fs::remove_file(state_path.with_extension("json.tmp"));
    }

    #[test]
    fn default_terminal_retention_cap_matches_documented_default() {
        // Lock the default against drive-by changes. Operators rely
        // on "1000 by default" being written in the daemon.yaml
        // docstring; if we change it, this test fires and forces
        // the docstring update in the same commit.
        assert_eq!(DEFAULT_MAX_TERMINAL_RETAINED, 1000);
        let app = AppState::new();
        assert_eq!(app.max_terminal_retained(), DEFAULT_MAX_TERMINAL_RETAINED);
    }

    #[test]
    fn set_max_terminal_retained_round_trips() {
        // Setter + getter pair is the public contract lib.rs uses at
        // startup. Lock that both 0 (disable) and a non-default
        // positive value (operator override) persist through
        // subsequent saves without clobber.
        let app = AppState::new();
        app.set_max_terminal_retained(42);
        assert_eq!(app.max_terminal_retained(), 42);
        app.set_max_terminal_retained(0);
        assert_eq!(app.max_terminal_retained(), 0);
        app.set_max_terminal_retained(1000);
        assert_eq!(app.max_terminal_retained(), 1000);
    }
}
