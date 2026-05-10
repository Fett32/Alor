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

/// Schema version — must match `src-tauri/src/wrapper/protocol.rs`
/// and `orchestrator-py/agent_client.py`. Pinned to
/// `proto/alor_protocol.yaml`'s `protocol_version`. The
/// drift-detector test enforces all three stay in lockstep.
pub const PROTOCOL_VERSION: u32 = 1;

fn default_protocol_version() -> u32 {
    PROTOCOL_VERSION
}

/// Every message on the wire is wrapped in this struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    #[serde(rename = "type")]
    pub kind: String,
    pub correlation_id: Uuid,
    /// Protocol schema version. Back-compat via serde default —
    /// pre-gate envelopes parse as the current version and pass.
    /// Receivers call `expect_current_version` to fail loud on
    /// mismatch. See tauri-side docs for the full contract.
    #[serde(default = "default_protocol_version")]
    pub protocol_version: u32,
    pub payload: serde_json::Value,
}

/// Version-mismatch error. Same shape as the tauri crate's
/// `ProtocolVersionMismatch` (can't cross-crate-share since each
/// crate defines its own Envelope); the drift-detector test
/// asserts the values stay synced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolVersionMismatch {
    pub wire: u32,
    pub local: u32,
}

impl std::fmt::Display for ProtocolVersionMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "protocol version mismatch: wire={} local={}",
            self.wire, self.local
        )
    }
}

impl std::error::Error for ProtocolVersionMismatch {}

impl Envelope {
    pub fn new(kind: &str, payload: impl Serialize) -> anyhow::Result<Self> {
        Ok(Self {
            kind: kind.to_string(),
            correlation_id: Uuid::new_v4(),
            protocol_version: PROTOCOL_VERSION,
            payload: serde_json::to_value(payload)?,
        })
    }

    /// Verify version match. Callers should run this after
    /// decoding each inbound envelope.
    pub fn expect_current_version(&self) -> Result<(), ProtocolVersionMismatch> {
        if self.protocol_version == PROTOCOL_VERSION {
            Ok(())
        } else {
            Err(ProtocolVersionMismatch {
                wire: self.protocol_version,
                local: PROTOCOL_VERSION,
            })
        }
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
///
/// Mirrors the daemon-side `TaskComplete` in
/// `src-tauri/src/wrapper/protocol.rs`. See that file for the
/// summary / details split rationale. The Rust wrapper emits only
/// `summary = None` today (idle-poll has no text to report), but
/// the `details` field is carried so the Python SDK worker's richer
/// completion payloads round-trip through the same struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskComplete {
    pub task_id: Uuid,
    pub summary: Option<String>,
    #[serde(default)]
    pub details: Option<String>,
    #[serde(default)]
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

// ---------------------------------------------------------------------------
// Protocol-version gate tests. Mirror
// src-tauri/src/wrapper/protocol.rs — identical contract, own
// implementation (can't share across crates). Drift-detector
// (orchestrator-py/test_protocol_drift.py) asserts the
// PROTOCOL_VERSION constants match across all three files.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_new_stamps_current_protocol_version() {
        let env = Envelope::new(MSG_TASK_ACCEPT, TaskAccept { task_id: Uuid::nil() })
            .expect("build");
        assert_eq!(env.protocol_version, PROTOCOL_VERSION);
    }

    #[test]
    fn envelope_expect_current_version_ok_on_match() {
        let env = Envelope::new(MSG_TASK_ACCEPT, TaskAccept { task_id: Uuid::nil() })
            .expect("build");
        assert!(env.expect_current_version().is_ok());
    }

    #[test]
    fn envelope_expect_current_version_errors_on_mismatch() {
        let mut env = Envelope::new(MSG_TASK_ACCEPT, TaskAccept { task_id: Uuid::nil() })
            .expect("build");
        env.protocol_version = PROTOCOL_VERSION + 1;
        let err = env.expect_current_version().expect_err("mismatch");
        assert_eq!(err.wire, PROTOCOL_VERSION + 1);
        assert_eq!(err.local, PROTOCOL_VERSION);
        let s = format!("{err}");
        assert!(s.contains("wire="), "{s}");
        assert!(s.contains("local="), "{s}");
    }

    #[test]
    fn envelope_deserialize_pre_gate_defaults_to_current_version() {
        // Back-compat: an envelope written before the gate existed
        // parses and defaults to the current version. The wrapper
        // binary's `client.rs` may receive outbox frames queued
        // before this field was added; the serde default keeps
        // replay working.
        let wire = serde_json::json!({
            "type": MSG_TASK_ACCEPT,
            "correlation_id": "00000000-0000-0000-0000-000000000001",
            "payload": {"task_id": "00000000-0000-0000-0000-000000000002"}
        });
        let env: Envelope = serde_json::from_value(wire).expect("parse");
        assert_eq!(env.protocol_version, PROTOCOL_VERSION);
    }

    #[test]
    fn envelope_serialize_includes_protocol_version() {
        let env = Envelope::new(MSG_TASK_ACCEPT, TaskAccept { task_id: Uuid::nil() })
            .expect("build");
        let s = serde_json::to_string(&env).expect("serialize");
        assert!(
            s.contains("\"protocol_version\":"),
            "wire must include protocol_version: {s}"
        );
    }
}
