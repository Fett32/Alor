/// Wire protocol between the Alor orchestrator and agent wrappers.
///
/// All messages are newline-delimited JSON on a Unix socket at
/// /tmp/alor/<agent_id>.sock
///
/// Direction conventions:
///   O→W  orchestrator sends to wrapper
///   W→O  wrapper sends back to orchestrator

use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Message type constants (used as the `type` discriminant in JSON)
// ---------------------------------------------------------------------------

pub const MSG_TASK_ASSIGN: &str = "task.assign";
pub const MSG_TASK_ACCEPT: &str = "task.accept";
pub const MSG_TASK_PROPOSE: &str = "task.propose";
pub const MSG_TASK_BLOCKED: &str = "task.blocked";
pub const MSG_TASK_COMPLETE: &str = "task.complete";
pub const MSG_STATUS_RESPONSE: &str = "status.response";
pub const MSG_REGISTER: &str = "wrapper.register";
pub const MSG_ERROR: &str = "wrapper.error";
pub const MSG_USER_INTERVENTION: &str = "user.intervention";
pub const MSG_WORKER_USER_INPUT: &str = "worker.user_input";
pub const MSG_WORKER_ORCH_RESPONSE: &str = "worker.orch_response";
pub const MSG_WORKER_FRAME_WEDGED: &str = "worker.frame_wedged";
pub const MSG_SHUTDOWN: &str = "daemon.shutdown";

// Phase 9: CLI message types
pub const MSG_CLI_STATUS: &str = "cli.status";
pub const MSG_CLI_TASK_LIST: &str = "cli.task.list";
pub const MSG_CLI_TASK_GET: &str = "cli.task.get";
pub const MSG_CLI_TASK_CREATE: &str = "cli.task.create";
pub const MSG_CLI_TASK_CANCEL: &str = "cli.task.cancel";
pub const MSG_CLI_TASK_COMPLETE: &str = "cli.task.complete";
pub const MSG_CLI_TASK_INTERVENTION_CLEAR: &str = "cli.task.intervention.clear";
pub const MSG_CLI_ASSIGN: &str = "cli.assign";
pub const MSG_CLI_SPAWN: &str = "cli.spawn";
pub const MSG_CLI_KILL: &str = "cli.kill";
pub const MSG_CLI_DELETE: &str = "cli.delete";
pub const MSG_CLI_EVENT_STREAM: &str = "cli.event.stream";
pub const MSG_CLI_PROJECT_LIST: &str = "cli.project.list";
pub const MSG_CLI_PROJECT_GET: &str = "cli.project.get";
pub const MSG_CLI_PROJECT_SAVE: &str = "cli.project.save";
pub const MSG_CLI_INTEGRATIONS_GET: &str = "cli.integrations.get";
pub const MSG_CLI_AGENT_SEND_MESSAGE: &str = "cli.agent.send_message";
pub const MSG_CLI_AGENT_ENSURE_RUNNING: &str = "cli.agent.ensure_running";
pub const MSG_CLI_MEMORY_GET: &str = "cli.memory.get";
pub const MSG_CLI_RESPONSE: &str = "cli.response";
pub const MSG_CLI_ERROR: &str = "cli.error";
pub const MSG_EVENT: &str = "event";

// ---------------------------------------------------------------------------
// Structured error codes for `cli.error` payloads
// ---------------------------------------------------------------------------
//
// Every `cli.error` envelope carries `payload.error` (human-readable prose).
// Rejections a caller might plausibly want to handle specifically also carry
// `payload.code` — a stable identifier so orchestrator-py/daemon.py can map
// them onto typed Python exceptions instead of string-matching on the prose.
//
// Keep the code strings stable; Python side imports them as literals.

/// Caller set `suppress_echo=true` on `cli.agent.send_message` against an
/// agent whose runtime can't decode BEGIN/END framing (wrapper runtime, or
/// an unknown/unresolvable agent). Retry with `suppress_echo=false` or pick
/// a `claude-sdk`-backed agent.
pub const ERR_CODE_FRAMED_SEND_NOT_SUPPORTED: &str = "framed_send_not_supported";

// ---------------------------------------------------------------------------
// Echo-guard framing for orch → SDK-worker programmatic sends
// ---------------------------------------------------------------------------
//
// When `cli.agent.send_message` is called with `suppress_echo: true`, the
// daemon wraps the payload as:
//
//   {BEGIN}{uuid}\n
//   <multi-line body>\n
//   {END}{uuid}\n
//
// and feeds it to the worker's pane via `tmux send-keys -l`. Because tmux
// converts each literal `\n` into an Enter keystroke, the worker's
// prompt_toolkit `read_line` sees BEGIN, every body line, and END as
// separate lines — which is exactly what the worker's state machine needs
// to (a) suppress per-line `worker.user_input` emission across the whole
// frame, and (b) dispatch the accumulated body as a single SDK turn.
//
// The uuid is fresh per send and echoed back on `worker.orch_response` so
// orch can match the reply to its originating send. Including the uuid in
// the END marker means user-text collisions with the literal END prefix
// can't prematurely close the frame (would need to predict the uuid).
//
// Both markers are printable ASCII so they survive tmux → pty →
// prompt_toolkit intact (unbound control bytes get silently dropped).
//
// MUST stay in lockstep with `orchestrator-py/worker.py`'s
// `WORKER_ECHO_SENTINEL_BEGIN` / `WORKER_ECHO_SENTINEL_END`.
pub const WORKER_ECHO_SENTINEL_BEGIN: &str = "__ALOR_ORCH_ECHO_BEGIN__";
pub const WORKER_ECHO_SENTINEL_END: &str = "__ALOR_ORCH_ECHO_END__";

// ---------------------------------------------------------------------------
// Envelope — every message on the wire is wrapped in this
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    /// One of the MSG_* constants above.
    #[serde(rename = "type")]
    pub kind: String,
    /// Correlation ID so responses can be matched to requests.
    pub correlation_id: Uuid,
    /// The actual payload, type-erased as raw JSON.
    pub payload: serde_json::Value,
}

impl Envelope {
    pub fn new(kind: &str, payload: impl Serialize) -> anyhow::Result<Self> {
        Ok(Self {
            kind: kind.to_string(),
            correlation_id: Uuid::new_v4(),
            payload: serde_json::to_value(payload)?,
        })
    }

    pub fn decode_payload<T: for<'de> serde::Deserialize<'de>>(&self) -> anyhow::Result<T> {
        Ok(serde_json::from_value(self.payload.clone())?)
    }
}

// ---------------------------------------------------------------------------
// O→W  task.assign
// ---------------------------------------------------------------------------

/// Orchestrator assigns a task to an agent wrapper.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskAssign {
    pub task_id: Uuid,
    pub title: String,
    pub description: String,
    /// Optional timeout in seconds; wrapper should report TimedOut if exceeded.
    pub timeout_secs: Option<u64>,
}

// ---------------------------------------------------------------------------
// W→O  task.accept
// ---------------------------------------------------------------------------

/// Wrapper acknowledges it has received and will begin the task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskAccept {
    pub task_id: Uuid,
}

// ---------------------------------------------------------------------------
// W→O  task.propose
// ---------------------------------------------------------------------------

/// Wrapper proposes a plan or diff for human approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskPropose {
    pub task_id: Uuid,
    /// Logic brief explaining what will be done.
    pub brief: Option<String>,
    /// Optional diff showing exactly what will change.
    pub diff: Option<String>,
}

// ---------------------------------------------------------------------------
// W→O  task.blocked
// ---------------------------------------------------------------------------

/// Wrapper cannot proceed and is waiting for something external.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskBlocked {
    pub task_id: Uuid,
    pub reason: String,
    /// If the wrapper knows what it needs, it can suggest it here.
    pub waiting_for: Option<String>,
}

// ---------------------------------------------------------------------------
// W→O  task.complete
// ---------------------------------------------------------------------------

/// Wrapper reports successful completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskComplete {
    pub task_id: Uuid,
    /// Short human-readable summary of what was done.
    pub summary: Option<String>,
    /// Machine-readable output data (free-form JSON).
    pub output: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// W→O  wrapper.register — first message after connecting
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WrapperRegister {
    pub agent_id: String,
}

// ---------------------------------------------------------------------------
// W→O  wrapper.error — something went wrong
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WrapperError {
    pub agent_id: String,
    pub message: String,
}

// ---------------------------------------------------------------------------
// user.intervention — wrapper signals user attention needed
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserIntervention {
    pub agent_id: String,
}

// ---------------------------------------------------------------------------
// W→O  worker.user_input — SDK worker forwards a line Fett typed into its pane
// ---------------------------------------------------------------------------

/// SDK-runtime workers (Python worker.py) emit this every time Fett's stdin
/// feeds a non-slash line into the local SDK. Unlike `user.intervention` —
/// which is a noisy tmux-pane-diff signal from the Rust wrapper with no text
/// — this carries the exact input. Orch uses it to stay aware of follow-ups
/// that land on an agent after a task completes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerUserInput {
    pub agent_id: String,
    pub text: String,
    /// True if the worker was mid-task when the input was accepted. SDK
    /// workers serialize stdin behind `client_lock`, so mid-task input is
    /// queued until the current turn returns — the distinction matters for
    /// orch routing.
    #[serde(default)]
    pub during_task: bool,
    /// Optional task_id for mid-task inputs.
    #[serde(default)]
    pub task_id: Option<Uuid>,
}

// ---------------------------------------------------------------------------
// W→O  worker.orch_response — SDK worker reports back the reply to an
//      orch-origin `agent_send_message` (sentinel-prefixed) query
// ---------------------------------------------------------------------------

/// Mirror of `WorkerUserInput` for the other direction. When the orchestrator
/// injects a message into a worker's pane via `cli.agent.send_message` with
/// `suppress_echo: true`, the daemon stamps a correlation_id into the
/// sentinel prefix. The SDK worker extracts that id, runs the SDK turn to
/// completion (ResultMessage), and emits this event carrying the final
/// assistant TextBlock as `text`. Orch matches `correlation_id` back to its
/// originating send so `agent_send_message` becomes a real query/response
/// channel instead of fire-and-forget.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerOrchResponse {
    pub agent_id: String,
    /// The uuid the daemon embedded in the sentinel on the outbound send.
    pub correlation_id: Uuid,
    /// Final assistant text from the SDK turn. Same "latest TextBlock wins"
    /// semantics as `TaskComplete::summary`.
    pub text: String,
    /// True if the worker was mid-task when the orch send landed. SDK
    /// workers serialize behind `client_lock`, so mid-task sends are queued.
    #[serde(default)]
    pub during_task: bool,
    /// Optional task_id for mid-task sends.
    #[serde(default)]
    pub task_id: Option<Uuid>,
}

// ---------------------------------------------------------------------------
// W→O  worker.frame_wedged — SDK worker dropped a stale BEGIN frame on
//      nested-BEGIN recovery (see stdin_loop in orchestrator-py/worker.py)
// ---------------------------------------------------------------------------

/// Emitted when the worker's stdin state machine hits a BEGIN marker while
/// already inside an unclosed frame. The previous frame's body is
/// discarded (its orch caller will time out — same outcome as a lost END)
/// and processing restarts on the new uuid. This event gives the orch a
/// structured signal for the dropped frame so it can distinguish wedge
/// timeouts from any other END-loss timeout.
///
/// The stale caller's pending `agent_send_message_await` matches
/// `dropped_uuid` against its outstanding correlation_id to raise
/// `FrameWedgedError` (Python) instead of returning a bare timeout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerFrameWedged {
    pub agent_id: String,
    /// uuid of the stale BEGIN whose body was discarded.
    pub dropped_uuid: Uuid,
    /// uuid of the nested BEGIN that triggered recovery (the new frame).
    pub new_uuid: Uuid,
    /// Total body bytes discarded (joined with '\n' as the worker would
    /// have dispatched them). Useful for eyeballing wedge severity.
    pub bytes_dropped: u64,
    /// Number of buffered body lines discarded.
    pub lines_dropped: u32,
    /// task_id the stale frame was captured under, if the worker was mid-task.
    #[serde(default)]
    pub task_id: Option<Uuid>,
}

// ---------------------------------------------------------------------------
// daemon.shutdown — daemon tells wrapper to exit
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonShutdown {}

// ---------------------------------------------------------------------------
// Phase 9: CLI payload structs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliTaskCreate {
    pub title: String,
    pub description: String,
    /// Project name (profile) this task belongs to. Used to generate a
    /// TASK BRIEF prefix (key files, docs) when the task is dispatched.
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliTaskComplete {
    pub task_id: Uuid,
    /// Optional close-out summary recorded via `set_task_summary`. Primarily
    /// used to attach a retroactive note when promoting a Cancelled task to
    /// Completed.
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliTaskCancel {
    pub task_id: Uuid,
}

/// Payload for `cli.status`.
///
/// `view` controls the response shape:
///   - `None` or `"summary"` (default — see `DEFAULT_STATUS_VIEW`):
///     returns a lean agent roster only (no `tasks` array, no
///     per-agent `task_history`). Each agent is projected to the
///     `AgentSummary` shape plus a `current_tasks` list of
///     non-terminal task UUIDs currently assigned to the slot.
///     Orders of magnitude smaller than the full response when
///     many tasks exist; sized for orchestrator routing decisions
///     that call `agent_list` per turn.
///   - `"full"`: backwards-compatible firehose. `agents` (full
///     `Agent` structs including task_history), `tasks` (every
///     task in state.json inline), `connected` (list of live
///     socket agent_ids).
///   - Anything else: silently treated as `"summary"`. An LLM
///     caller with a typo shouldn't blow up the response.
///
/// The `view` field in the response echoes back which shape the
/// server actually applied.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CliStatus {
    #[serde(default)]
    pub view: Option<String>,
}

/// Default `view` for `cli.status`. "summary" keeps
/// routing-decision calls lean; consumers that need the task
/// firehose must opt in with `"full"` (alor-cli status does
/// this to preserve its display).
pub const DEFAULT_STATUS_VIEW: &str = "summary";

/// Payload for `cli.task.intervention.clear`. Orchestrator-callable
/// reset of a task's `user_intervened` flag. See
/// `Task.user_intervened` docstring for the flag's informational-only
/// semantics. Intended for the case where Fett accidentally typed
/// into an agent pane mid-task and the intervention is stale
/// (unsubmitted / irrelevant).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliTaskInterventionClear {
    pub task_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliTaskGet {
    pub task_id: Uuid,
}

/// Payload for `cli.task.list`.
///
/// `state_filter` narrows the response:
///   - `None` or `"default"` → non-terminal tasks only (hides Completed /
///     Cancelled / Rejected / TimedOut / Stale). This is what the
///     orchestrator's MCP `task_list` tool passes by default so terminal
///     records don't bloat its runtime context.
///   - `"all"` → everything, including terminal.
///   - A SCREAMING_SNAKE_CASE state name (`"COMPLETED"`, `"CANCELLED"`,
///     `"STALE"`, etc.) → only tasks in that exact state.
///
/// `limit` / `offset` paginate the filtered result:
///   - `limit = None` → server default `DEFAULT_TASK_LIST_LIMIT` (20).
///     Sized off a 79-task sample: avg task serializes to ~4 KB / ~1.1k
///     tokens, so 20 tasks ≈ 22k tokens — under the 25k-token Write-tool
///     rule-of-thumb ceiling Fett uses for lean tool output. p90 tasks
///     are ~2.3k tokens; the worst page is still bounded.
///   - `offset = None` → 0.
///   - Pass `limit = 0` for "no limit" (returns whole filtered set; only
///     use this when you've already counted and know it's safe).
///
/// `view` controls the per-task field projection:
///   - `None` or `"summary"` (default — see `DEFAULT_TASK_LIST_VIEW`):
///     return only {id, title, state, assigned_to, updated_at}. ~70
///     tokens per task, so e.g. a 200-task scan fits comfortably under
///     the 25k-token ceiling. Use for roster scans.
///   - `"full"`: return every `Task` field (including description,
///     proposal_brief, proposal_diff, summary, project). ~1.1k tokens
///     per task with heavy-tail tasks up to ~4k. Use when you need
///     detail on many tasks at once — but usually prefer `task_get`
///     for a single-task read.
///   - Anything else: silently treated as `"summary"`. An LLM caller
///     with a typo shouldn't blow up the list.
///
/// Response envelope includes `{tasks, total, returned, offset,
/// has_more, view}` so callers can paginate without a second RPC and
/// know which shape they got back.
///
/// The frontend's `get_tasks` Tauri command is a separate path and still
/// returns the full live set (filtering + chunking happen client-side in
/// TaskList.js for snappy dropdown toggles). Pagination caps + summary
/// view only apply to the CLI/MCP surface — the orchestrator-facing one
/// where context blowup is the real cost.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CliTaskList {
    #[serde(default)]
    pub state_filter: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub offset: Option<u32>,
    #[serde(default)]
    pub view: Option<String>,
}

/// Default `limit` for `cli.task.list` when the caller doesn't specify
/// one. See `CliTaskList` docstring for the sizing rationale.
pub const DEFAULT_TASK_LIST_LIMIT: u32 = 20;

/// Default `view` for `cli.task.list`. "summary" keeps scans lean;
/// callers that need detail must opt in with `"full"`.
pub const DEFAULT_TASK_LIST_VIEW: &str = "summary";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliAssign {
    pub task_id: Uuid,
    pub agent_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliSpawn {
    pub name: Option<String>,
    pub agent: String,
    pub role: Option<String>,
    /// Runtime project override (wins over the yaml config's project).
    /// Used when spawning an instance from a template.
    #[serde(default)]
    pub project: Option<String>,
    /// Runtime working-dir override (wins over the yaml config's working_dir).
    /// Used when spawning an instance from a template.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Task UUID that initiated this spawn. Set by workers that call
    /// `agent_spawn` mid-task so the daemon can warn at task-completion
    /// time if the instance wasn't explicitly killed. Orch-originated
    /// spawns leave this None (they're not tied to a single task).
    /// See AppState::record_task_spawn / transition_task warning path.
    #[serde(default)]
    pub spawned_by_task: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliKill {
    pub instance: String,
}

/// Permanent tombstone — removes the agent row from state.json.
/// Intended for template-spawned instances only; core yaml slots should
/// be killed (disconnect) rather than deleted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliDelete {
    pub instance: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliProjectGet {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliAgentSendMessage {
    pub agent_id: String,
    pub text: String,
    /// If true, append Enter after the text. Defaults to false so the
    /// caller can build multi-line inputs without submitting.
    #[serde(default)]
    pub submit: bool,
    /// If true, the daemon prepends the WORKER_ECHO_SENTINEL so the SDK
    /// worker recognizes this line as programmatic (orch/CLI) origin and
    /// skips re-emitting it as a `worker.user_input` event. Used by the
    /// orchestrator's own `agent_send_message` tool to avoid echo loops.
    #[serde(default)]
    pub suppress_echo: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliMemoryGet {
    pub project: String,
}

/// Idempotent spawn — if the agent is already running, this is a no-op.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliAgentEnsureRunning {
    pub agent_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliProjectSave {
    pub name: String,
    pub description: Option<String>,
    pub root_dir: Option<String>,
    pub stack: Option<Vec<String>>,
    pub key_files: Option<Vec<String>>,
    pub doc_paths: Option<Vec<String>>,
    /// Path to an agent's memory index file. When set, the daemon links
    /// it into the project's Memory Hub (move + symlink).
    #[serde(default)]
    pub memory_index: Option<String>,
    /// Which agent this memory_index belongs to. Defaults to "claude".
    #[serde(default)]
    pub memory_agent: Option<String>,
}

