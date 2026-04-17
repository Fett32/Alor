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
pub const MSG_SHUTDOWN: &str = "daemon.shutdown";

// Phase 9: CLI message types
pub const MSG_CLI_STATUS: &str = "cli.status";
pub const MSG_CLI_TASK_LIST: &str = "cli.task.list";
pub const MSG_CLI_TASK_GET: &str = "cli.task.get";
pub const MSG_CLI_TASK_CREATE: &str = "cli.task.create";
pub const MSG_CLI_TASK_CANCEL: &str = "cli.task.cancel";
pub const MSG_CLI_TASK_COMPLETE: &str = "cli.task.complete";
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliTaskCancel {
    pub task_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliTaskGet {
    pub task_id: Uuid,
}

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

