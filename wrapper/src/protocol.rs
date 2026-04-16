/// Wire protocol types for the wrapper binary.
///
/// Mirrors the Envelope + payload model defined in src-tauri/src/wrapper/protocol.rs
/// so the daemon (Tauri side) and this wrapper speak the same JSON schema.
///
/// All messages are newline-delimited JSON on Unix sockets:
///   Daemon → Wrapper: /tmp/alor/daemon.sock   (wrapper connects to this)
///   Wrapper → Daemon: same connection, other direction
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---- message type constants -------------------------------------------------
// Must match the constants in src-tauri/src/wrapper/protocol.rs exactly.

pub const MSG_TASK_ASSIGN: &str = "task.assign";
pub const MSG_TASK_ACCEPT: &str = "task.accept";
pub const MSG_TASK_COMPLETE: &str = "task.complete";
pub const MSG_STATUS_REQUEST: &str = "status.request";
pub const MSG_STATUS_RESPONSE: &str = "status.response";
pub const MSG_REGISTER: &str = "wrapper.register";
pub const MSG_ERROR: &str = "wrapper.error";
pub const MSG_SHUTDOWN: &str = "daemon.shutdown";
pub const MSG_USER_INTERVENTION: &str = "user.intervention";

// ---- envelope ---------------------------------------------------------------

/// Every message on the wire is wrapped in this struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    #[serde(rename = "type")]
    pub kind: String,
    pub correlation_id: Uuid,
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

    /// Unwrap and deserialize the payload into `T`.
    pub fn decode_payload<T: for<'de> Deserialize<'de>>(&self) -> anyhow::Result<T> {
        Ok(serde_json::from_value(self.payload.clone())?)
    }
}

// ---- daemon → wrapper payloads ----------------------------------------------

/// task.assign — daemon tells wrapper to run a prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskAssign {
    pub task_id: Uuid,
    pub title: String,
    pub description: String,
    pub timeout_secs: Option<u64>,
}

/// status.request — daemon asks for the wrapper's state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusRequest {
    pub task_id: Option<Uuid>,
}

/// daemon.shutdown — daemon is going away.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonShutdown {}

// ---- wrapper → daemon payloads ----------------------------------------------

/// wrapper.register — first message sent after connecting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WrapperRegister {
    pub agent_id: String,
}

/// task.accept — wrapper acknowledges it received the task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskAccept {
    pub task_id: Uuid,
}

/// task.complete — idle pattern detected after a task.assign.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskComplete {
    pub task_id: Uuid,
    pub summary: Option<String>,
    pub output: Option<serde_json::Value>,
}

/// status.response — reply to StatusRequest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub agent_id: String,
    pub task_id: Option<Uuid>,
    pub alive: bool,
    pub details: Option<String>,
}

/// wrapper.error — something went wrong.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WrapperError {
    pub agent_id: String,
    pub message: String,
}

/// user.intervention — the user typed something directly into the tmux pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserIntervention {
    pub agent_id: String,
}

// ---- runtime state (not on the wire) ----------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Running { task_id: Uuid },
}
