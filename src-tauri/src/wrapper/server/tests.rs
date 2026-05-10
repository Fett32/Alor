//! Tests for the `server` module family — moved out of `mod.rs`
//! during the god-module decomposition so the wiring file stays
//! small. Contents unchanged from the prior in-file
//! `#[cfg(test)] mod tests` block; test paths adjusted where
//! symbols moved to submodules (e.g. `super::routing::is_safe_hub_basename`,
//! `super::agent_lifecycle::{resolve_agent_runtime, ...}`).
//!
//! Declared from `mod.rs` as `#[cfg(test)] mod tests;` so the
//! `#[cfg(test)]` gate stays on the declaration and this file
//! doesn't need the attribute at its top.

use super::*;
// Re-import symbols the tests used to see via `super::*` back when
// mod.rs imported the full wrapper protocol surface. After the god-
// module split mod.rs only imports what it needs itself, so tests
// reach for the real sources here.
use super::agent_lifecycle::{
    is_framed_send_allowed, resolve_agent_runtime, SDK_FRAMED_RUNTIME,
};
use crate::daemon::state::{Agent, Task, TaskState, WorkerResponseRecord};
use crate::wrapper::protocol::{
    Envelope, TaskAccept, TaskComplete, WorkerOrchResponse,
    MSG_CLI_MEMORY_GET, MSG_CLI_RESPONSE, MSG_CLI_TASK_GET, MSG_CLI_TASK_LIST,
    MSG_CLI_WORKER_RESPONSE_GET, MSG_TASK_ACCEPT, MSG_TASK_COMPLETE,
    MSG_WORKER_ORCH_RESPONSE,
    MEMORY_GET_WARN_BYTES, TASK_LIST_FULL_MAX_LIMIT, TASK_SUMMARY_MAX_BYTES,
    TASK_SUMMARY_TRUNCATION_MARKER,
};


/// Build a minimal AgentConfig with just `runtime` set; every other
/// field defaults. Kept here rather than in config.rs because these
/// tests are the only thing that need a programmatically-built
/// config (production code deserializes from yaml).
fn cfg(runtime: &str) -> AgentConfig {
    // Round-trip through yaml — simpler than manually filling every
    // field, and exercises the same defaults the loader uses in
    // production.
    let yaml = format!("runtime: {runtime}\n");
    serde_yaml::from_str::<AgentConfig>(&yaml).expect("valid test yaml")
}

#[test]
fn framed_send_allowed_for_claude_sdk_slot() {
    let configs = vec![("alor".to_string(), cfg("claude-sdk"))];
    let state = AppState::new();
    assert!(is_framed_send_allowed(&configs, &state, "alor"));
}

#[test]
fn framed_send_rejected_for_wrapper_slot() {
    // The latent hazard: default runtime is "wrapper", and a
    // framed send to one would splat literal BEGIN/END markers
    // into the CLI's pty.
    let configs = vec![("codex".to_string(), cfg("wrapper"))];
    let state = AppState::new();
    assert!(!is_framed_send_allowed(&configs, &state, "codex"));
}

#[test]
fn framed_send_rejected_for_unknown_agent() {
    // Fail-closed: no config + no state entry means we can't prove
    // it's SDK-framed-safe, so reject.
    let configs = vec![("alor".to_string(), cfg("claude-sdk"))];
    let state = AppState::new();
    assert!(!is_framed_send_allowed(&configs, &state, "ghost"));
}

#[test]
fn framed_send_allowed_for_claude_sdk_template_instance() {
    // Template-spawned instance: no direct config entry, but its
    // `template` field points at a yaml slot whose runtime is
    // claude-sdk. Must resolve transitively.
    let configs = vec![("claude".to_string(), cfg("claude-sdk"))];
    let state = AppState::new();
    let mut instance = Agent::new("claude-mandaspace", "claude-mandaspace");
    instance.template = Some("claude".to_string());
    state.register_agent(instance);
    assert!(is_framed_send_allowed(
        &configs,
        &state,
        "claude-mandaspace"
    ));
}

#[test]
fn framed_send_rejected_for_wrapper_template_instance() {
    // Same transitive lookup, but the template is a wrapper
    // runtime — the gate must still refuse.
    let configs = vec![("codex".to_string(), cfg("wrapper"))];
    let state = AppState::new();
    let mut instance = Agent::new("codex-scratch", "codex-scratch");
    instance.template = Some("codex".to_string());
    state.register_agent(instance);
    assert!(!is_framed_send_allowed(&configs, &state, "codex-scratch"));
}

#[test]
fn framed_send_rejected_when_template_points_at_missing_config() {
    // Defensive: if an instance's template id no longer resolves
    // (e.g. yaml was deleted between spawn and now), we can't
    // prove runtime, so fail-closed.
    let configs: Vec<(String, AgentConfig)> = vec![];
    let state = AppState::new();
    let mut instance = Agent::new("orphan", "orphan");
    instance.template = Some("gone".to_string());
    state.register_agent(instance);
    assert!(!is_framed_send_allowed(&configs, &state, "orphan"));
}

#[test]
fn resolve_runtime_direct_slot_wins_over_template() {
    // If an agent_id exists as both a direct yaml slot AND a
    // state-persisted instance (shouldn't happen in practice, but
    // belts-and-suspenders), the direct config is authoritative.
    let configs = vec![("alor".to_string(), cfg("claude-sdk"))];
    let state = AppState::new();
    let mut instance = Agent::new("alor", "alor");
    instance.template = Some("some-wrapper-template".to_string());
    state.register_agent(instance);
    assert_eq!(
        resolve_agent_runtime(&configs, &state, "alor").as_deref(),
        Some("claude-sdk")
    );
}

/// Build a SocketServer wired to real AppState + PaneManager but with
/// no connected wrappers and no event subscribers — good enough to
/// exercise `handle_message` without a live socket.
fn server_for_test() -> SocketServer {
    SocketServer::with_configs(
        AppState::new(),
        crate::terminal::pane_manager::PaneManager::new(),
        vec![],
    )
}

/// Drive a task all the way to Completed with a baseline summary so
/// the idempotency tests have something to protect.
fn completed_task_with_summary(state: &AppState, summary: &str) -> Uuid {
    let task = Task::new("test-title", "test-description");
    let id = task.id;
    state.add_task(task);
    state
        .transition_task(id, TaskState::Assigned)
        .expect("Pending → Assigned");
    state
        .transition_task(id, TaskState::Accepted)
        .expect("Assigned → Accepted");
    state.set_task_summary(id, summary.to_string());
    state
        .transition_task(id, TaskState::Completed)
        .expect("Accepted → Completed");
    id
}

#[tokio::test]
async fn task_complete_is_idempotent_on_replay() {
    // Outbox replays can re-deliver a MSG_TASK_COMPLETE the daemon
    // already processed before its previous crash. The guard must
    // treat it as a no-op: summary untouched, state untouched, no
    // second tmux orchestrator ping.
    let server = server_for_test();
    let task_id = completed_task_with_summary(&server.app_state, "first-summary");

    let replay = Envelope::new(
        MSG_TASK_COMPLETE,
        TaskComplete {
            task_id,
            summary: Some("second-summary".to_string()),
            details: None,
            output: None,
        },
    )
    .expect("build envelope");

    // Must not panic, must not re-transition, must not overwrite.
    server.handle_message("test-agent", replay).await;

    let task = server
        .app_state
        .get_task(task_id)
        .expect("task still present");
    assert_eq!(task.state, TaskState::Completed);
    assert_eq!(
        task.summary.as_deref(),
        Some("first-summary"),
        "replayed completion must not overwrite existing summary"
    );
}

#[tokio::test]
async fn task_complete_splits_summary_and_details_on_the_wire() {
    // Hot-path bloat fix #1 (audit 8b03cae6). When a worker sends
    // `task.complete` with both `summary` and `details`, the daemon
    // must:
    //   (a) store the summary verbatim (we're under the 512 B cap
    //       here — no truncation marker).
    //   (b) store the details on the Task for `task_get` to return.
    //   (c) transition the task to Completed.
    //
    // The associated broadcast shape (has_details flag, summary
    // injected, details body NOT injected) is exercised separately
    // via format_event_for_agent regression tests on the Python
    // side; here we pin the state mutations.
    let server = server_for_test();
    let task = Task::new("t", "d");
    let id = task.id;
    server.app_state.add_task(task);
    server
        .app_state
        .transition_task(id, TaskState::Assigned)
        .expect("assign");
    server
        .app_state
        .transition_task(id, TaskState::Accepted)
        .expect("accept");

    let env = Envelope::new(
        MSG_TASK_COMPLETE,
        TaskComplete {
            task_id: id,
            summary: Some("done; scope-check passed".to_string()),
            details: Some(
                "Full report:\n- 3 tests added\n- 1 refactor\n- diff attached".to_string(),
            ),
            output: None,
        },
    )
    .expect("build envelope");
    server.handle_message("test-agent", env).await;

    let t = server.app_state.get_task(id).expect("task present");
    assert_eq!(t.state, TaskState::Completed);
    assert_eq!(t.summary.as_deref(), Some("done; scope-check passed"));
    assert_eq!(
        t.details.as_deref(),
        Some("Full report:\n- 3 tests added\n- 1 refactor\n- diff attached")
    );
    // Summary under cap must NOT carry the truncation marker.
    assert!(
        !t.summary
            .as_deref()
            .unwrap()
            .ends_with(TASK_SUMMARY_TRUNCATION_MARKER),
        "summary under cap must not be marked truncated"
    );
}

#[tokio::test]
async fn task_complete_truncates_oversize_summary_with_marker() {
    // Back-compat path: a worker (old or misbehaving) sends a multi-
    // KB summary with no `details` split. The daemon must truncate
    // at the protocol cap and stash the clipped form — it may NOT
    // let a 10 KB summary reach the orch-event broadcast verbatim
    // (that's the whole point of the fix). No details field gets
    // synthesized from the oversize summary; the daemon is a
    // dumb cap, not a splitter. Workers are responsible for the
    // summary/details split on their side.
    let server = server_for_test();
    let task = Task::new("t", "d");
    let id = task.id;
    server.app_state.add_task(task);
    server
        .app_state
        .transition_task(id, TaskState::Assigned)
        .expect("assign");
    server
        .app_state
        .transition_task(id, TaskState::Accepted)
        .expect("accept");

    // 5 KB of ASCII — comfortably over the 512 B cap.
    let big = "a".repeat(5_000);
    let env = Envelope::new(
        MSG_TASK_COMPLETE,
        TaskComplete {
            task_id: id,
            summary: Some(big.clone()),
            details: None,
            output: None,
        },
    )
    .expect("build envelope");
    server.handle_message("test-agent", env).await;

    let t = server.app_state.get_task(id).expect("task present");
    assert_eq!(t.state, TaskState::Completed);
    let stored = t.summary.as_deref().expect("summary stored");
    assert!(
        stored.len() <= TASK_SUMMARY_MAX_BYTES,
        "oversize summary must be truncated to cap; got {} bytes",
        stored.len()
    );
    assert!(
        stored.ends_with(TASK_SUMMARY_TRUNCATION_MARKER),
        "truncated summary must carry the marker; got tail {:?}",
        &stored[stored.len().saturating_sub(32)..]
    );
    // And the handler does NOT invent a details field from the clip.
    // Details synthesis is the worker's job; daemon is cap-only.
    assert!(
        t.details.is_none(),
        "server must not synthesize details from a truncated summary"
    );
}

#[tokio::test]
async fn task_get_default_view_returns_summary_shape_with_has_flags() {
    // Default (no `view` field on the payload) must resolve to
    // the "summary" projection: lean scalars + has_* booleans,
    // heavy bodies dropped. This is the fix's default contract.
    let server = server_for_test();
    let mut t = Task::new("title", "non-empty description");
    t.summary = Some("ok".to_string());
    t.details = Some("long report".to_string());
    let id = t.id;
    server.app_state.add_task(t);

    let env = Envelope::new(
        MSG_CLI_TASK_GET,
        serde_json::json!({"task_id": id}),
    )
    .expect("build envelope");
    let resp = server.handle_cli_message(env).await;

    let payload = resp.payload;
    assert_eq!(payload.get("view").and_then(|v| v.as_str()), Some("summary"));
    let task = payload.get("task").expect("task present").as_object().expect("object");
    // Summary-shape affordances.
    assert_eq!(task.get("has_description"), Some(&serde_json::Value::Bool(true)));
    assert_eq!(task.get("has_summary"), Some(&serde_json::Value::Bool(true)));
    assert_eq!(task.get("has_details"), Some(&serde_json::Value::Bool(true)));
    assert_eq!(task.get("has_proposal_brief"), Some(&serde_json::Value::Bool(false)));
    assert_eq!(task.get("has_proposal_diff"), Some(&serde_json::Value::Bool(false)));
    // Heavy bodies must not be present.
    assert!(task.get("description").is_none(), "summary must not carry description");
    assert!(task.get("summary").is_none(), "summary view must not carry the summary body");
    assert!(task.get("details").is_none(), "summary view must not carry details");
    assert!(task.get("proposal_diff").is_none());
}

#[tokio::test]
async fn task_get_full_view_returns_entire_task() {
    // Explicit opt-in to `view="full"` must return the full Task
    // including heavy bodies. Back-compat for alor-cli (which
    // now passes view="full" explicitly) + the orch's follow-up
    // fetch path after a has_* flag told it there's something
    // to pull.
    let server = server_for_test();
    let mut t = Task::new("title", "the description body");
    t.summary = Some("ok".to_string());
    t.details = Some("the long report".to_string());
    t.proposal_diff = Some("--- a/x\n+++ b/x".to_string());
    let id = t.id;
    server.app_state.add_task(t);

    let env = Envelope::new(
        MSG_CLI_TASK_GET,
        serde_json::json!({"task_id": id, "view": "full"}),
    )
    .expect("build envelope");
    let resp = server.handle_cli_message(env).await;

    let payload = resp.payload;
    assert_eq!(payload.get("view").and_then(|v| v.as_str()), Some("full"));
    let task = payload.get("task").expect("task present").as_object().expect("object");
    // Heavy bodies round-trip.
    assert_eq!(
        task.get("description").and_then(|v| v.as_str()),
        Some("the description body")
    );
    assert_eq!(task.get("summary").and_then(|v| v.as_str()), Some("ok"));
    assert_eq!(
        task.get("details").and_then(|v| v.as_str()),
        Some("the long report")
    );
    assert_eq!(
        task.get("proposal_diff").and_then(|v| v.as_str()),
        Some("--- a/x\n+++ b/x")
    );
    // has_* flags are NOT on the full view — they're a summary-
    // shape affordance, redundant once the bodies are inline.
    assert!(
        task.get("has_description").is_none(),
        "full view must not carry the has_* affordance flags"
    );
}

#[tokio::test]
async fn task_get_unknown_view_falls_through_to_summary() {
    // LLM typo forgiveness: `view="Summary"`, `view="FULL"`,
    // `view="whatever"` all resolve to the safe default
    // (summary) rather than error. Mirrors the agent_list /
    // task_list silent fallback.
    let server = server_for_test();
    let t = Task::new("title", "desc");
    let id = t.id;
    server.app_state.add_task(t);

    let env = Envelope::new(
        MSG_CLI_TASK_GET,
        serde_json::json!({"task_id": id, "view": "whatever-typo"}),
    )
    .expect("build envelope");
    let resp = server.handle_cli_message(env).await;

    let payload = resp.payload;
    assert_eq!(
        payload.get("view").and_then(|v| v.as_str()),
        Some("summary"),
        "unknown view string must fall through to summary"
    );
}

#[tokio::test]
async fn task_get_missing_task_returns_cli_error() {
    // Regression guard: not-found path must still return a
    // cli.error (not a summary of a default Task). Shape this
    // test to the error envelope kind rather than content so a
    // future error-prose change doesn't flap.
    let server = server_for_test();
    let missing = Uuid::new_v4();

    let env = Envelope::new(
        MSG_CLI_TASK_GET,
        serde_json::json!({"task_id": missing}),
    )
    .expect("build envelope");
    let resp = server.handle_cli_message(env).await;

    // cli_error produces an MSG_CLI_ERROR envelope with the
    // prose in payload.error. Confirm shape.
    assert_eq!(resp.kind, MSG_CLI_ERROR);
    assert!(
        resp.payload
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .contains("not found"),
        "missing-task error payload must say 'not found'; got {:?}",
        resp.payload
    );
}

// ---------------------------------------------------------------------
// task_list full-view limit clamp (audit 8b03cae6 fix #3)
// ---------------------------------------------------------------------
//
// The list view=full escape hatch used to let a caller land 100+
// full-serialized Task objects (1.1 KB each → 110 KB+) in a single
// orchestrator tool response by passing `limit=0` or a large limit.
// These tests pin the server-side clamp at TASK_LIST_FULL_MAX_LIMIT
// (50), the limit_clamped_from / limit_applied response fields, and
// the summary-view leave-alone.

/// Helper: seed `n` tasks into state. Returns nothing — state is
/// consulted via the MSG_CLI_TASK_LIST handler directly.
fn seed_tasks(state: &AppState, n: usize) {
    for i in 0..n {
        state.add_task(Task::new(format!("task-{i}"), "d"));
    }
}

/// Helper: fire a MSG_CLI_TASK_LIST with the given view/limit and
/// return the response payload for inspection.
async fn task_list_payload(
    server: &SocketServer,
    view: Option<&str>,
    limit: Option<u32>,
) -> serde_json::Value {
    let mut body = serde_json::json!({});
    if let Some(v) = view {
        body["view"] = serde_json::json!(v);
    }
    if let Some(l) = limit {
        body["limit"] = serde_json::json!(l);
    }
    // Force state_filter="all" so non-terminal filter doesn't
    // silently drop our fixture tasks (new Task defaults to
    // Pending, which IS non-terminal, but be explicit).
    body["state_filter"] = serde_json::json!("all");
    let env = Envelope::new(MSG_CLI_TASK_LIST, body).expect("build envelope");
    let resp = server.handle_cli_message(env).await;
    resp.payload
}

#[tokio::test]
async fn task_list_full_view_limit_zero_is_clamped_to_max() {
    // limit=0 historically meant "no limit". On view=full that
    // was the canonical escape hatch. Must now clamp to
    // TASK_LIST_FULL_MAX_LIMIT and surface the clamp metadata.
    let server = server_for_test();
    seed_tasks(&server.app_state, 100);

    let payload = task_list_payload(&server, Some("full"), Some(0)).await;

    assert_eq!(payload.get("view").and_then(|v| v.as_str()), Some("full"));
    assert_eq!(
        payload
            .get("returned")
            .and_then(|v| v.as_u64())
            .expect("returned"),
        TASK_LIST_FULL_MAX_LIMIT as u64,
        "limit=0 on view=full must clamp to TASK_LIST_FULL_MAX_LIMIT"
    );
    assert_eq!(
        payload
            .get("limit_clamped_from")
            .and_then(|v| v.as_u64())
            .expect("limit_clamped_from present"),
        0,
    );
    assert_eq!(
        payload
            .get("limit_applied")
            .and_then(|v| v.as_u64())
            .expect("limit_applied present"),
        TASK_LIST_FULL_MAX_LIMIT as u64,
    );
    // has_more correctly reflects the clamp — 100 tasks, 50
    // returned, more remain.
    assert_eq!(
        payload.get("has_more").and_then(|v| v.as_bool()),
        Some(true),
        "clamp must not zero out has_more"
    );
}

#[tokio::test]
async fn task_list_full_view_large_limit_is_clamped_to_max() {
    // limit=100 (or any limit > MAX) must clamp, with the clamp
    // metadata reflecting the original value.
    let server = server_for_test();
    seed_tasks(&server.app_state, 100);

    let payload = task_list_payload(&server, Some("full"), Some(100)).await;

    assert_eq!(
        payload
            .get("returned")
            .and_then(|v| v.as_u64())
            .expect("returned"),
        TASK_LIST_FULL_MAX_LIMIT as u64,
    );
    assert_eq!(
        payload
            .get("limit_clamped_from")
            .and_then(|v| v.as_u64())
            .expect("limit_clamped_from present"),
        100,
    );
    assert_eq!(
        payload
            .get("limit_applied")
            .and_then(|v| v.as_u64())
            .expect("limit_applied present"),
        TASK_LIST_FULL_MAX_LIMIT as u64,
    );
}

#[tokio::test]
async fn task_list_full_view_small_limit_is_not_clamped() {
    // A limit inside the cap must pass through untouched — no
    // clamp metadata in the response so the normal-path shape
    // is unchanged.
    let server = server_for_test();
    seed_tasks(&server.app_state, 100);

    let payload = task_list_payload(&server, Some("full"), Some(10)).await;

    assert_eq!(
        payload
            .get("returned")
            .and_then(|v| v.as_u64())
            .expect("returned"),
        10,
    );
    assert!(
        payload.get("limit_clamped_from").is_none(),
        "unclamped response must NOT carry limit_clamped_from; \
         got payload = {}",
        payload
    );
    assert!(
        payload.get("limit_applied").is_none(),
        "unclamped response must NOT carry limit_applied"
    );
}

#[tokio::test]
async fn task_list_full_view_at_exact_cap_is_not_clamped() {
    // Boundary: limit == MAX must NOT trigger the clamp (it's
    // the exact allowed value). Guards against off-by-one in
    // the `would_exceed` predicate.
    let server = server_for_test();
    seed_tasks(&server.app_state, 100);

    let payload = task_list_payload(
        &server,
        Some("full"),
        Some(TASK_LIST_FULL_MAX_LIMIT),
    )
    .await;

    assert_eq!(
        payload
            .get("returned")
            .and_then(|v| v.as_u64())
            .expect("returned"),
        TASK_LIST_FULL_MAX_LIMIT as u64,
    );
    assert!(
        payload.get("limit_clamped_from").is_none(),
        "limit == cap must not trigger clamp metadata"
    );
}

#[tokio::test]
async fn task_list_summary_view_limit_zero_is_not_clamped() {
    // Summary view remains unclamped — the per-task projection
    // is lean (~70 tokens) and unlimited scans are context-safe
    // at realistic task counts. This is the behavior callers
    // doing a "give me everything for the dashboard" scan rely
    // on; changing it here would be a regression for every
    // summary consumer.
    let server = server_for_test();
    seed_tasks(&server.app_state, 100);

    let payload = task_list_payload(&server, Some("summary"), Some(0)).await;

    assert_eq!(payload.get("view").and_then(|v| v.as_str()), Some("summary"));
    assert_eq!(
        payload
            .get("returned")
            .and_then(|v| v.as_u64())
            .expect("returned"),
        100,
        "summary view with limit=0 must return the whole filtered set"
    );
    assert!(
        payload.get("limit_clamped_from").is_none(),
        "summary-view response must not carry clamp metadata"
    );
}

#[tokio::test]
async fn task_list_default_view_with_limit_zero_is_not_clamped() {
    // Default view (no `view` field on the payload) resolves to
    // summary per DEFAULT_TASK_LIST_VIEW. Same leave-alone
    // contract as explicit summary.
    let server = server_for_test();
    seed_tasks(&server.app_state, 100);

    let payload = task_list_payload(&server, None, Some(0)).await;

    assert_eq!(payload.get("view").and_then(|v| v.as_str()), Some("summary"));
    assert_eq!(
        payload
            .get("returned")
            .and_then(|v| v.as_u64())
            .expect("returned"),
        100,
    );
    assert!(payload.get("limit_clamped_from").is_none());
}

// ---------------------------------------------------------------------
// worker_response_get fetch-on-demand (audit 8b03cae6 fix #4)
// ---------------------------------------------------------------------

#[tokio::test]
async fn worker_response_get_returns_cached_text_by_correlation_id() {
    // End-to-end: seed a response in the cache, fire
    // MSG_CLI_WORKER_RESPONSE_GET, confirm the response envelope
    // carries the full text + metadata. This is the path the
    // orchestrator hits after seeing a truncated worker.orch_response
    // event carrying a worker_response_get(correlation_id=X) pointer.
    let server = server_for_test();
    let cid = Uuid::new_v4();
    let tid = Uuid::new_v4();
    let full_text = "x".repeat(5_000);
    server.app_state.record_worker_response(WorkerResponseRecord {
        correlation_id: cid,
        agent_id: "claude-mandaforge".to_string(),
        text: full_text.clone(),
        task_id: Some(tid),
        during_task: true,
        timestamp: chrono::Utc::now(),
    });

    let env = Envelope::new(
        MSG_CLI_WORKER_RESPONSE_GET,
        serde_json::json!({"correlation_id": cid}),
    )
    .expect("build envelope");
    let resp = server.handle_cli_message(env).await;

    assert_eq!(resp.kind, MSG_CLI_RESPONSE);
    let payload = resp.payload;
    assert_eq!(
        payload.get("correlation_id").and_then(|v| v.as_str()),
        Some(cid.to_string().as_str())
    );
    assert_eq!(
        payload.get("agent_id").and_then(|v| v.as_str()),
        Some("claude-mandaforge")
    );
    // The whole 5 KB round-trips — that's the entire point of
    // the cache (orch already has a 2 KB truncated copy; it's
    // here to recover the full body).
    assert_eq!(
        payload.get("text").and_then(|v| v.as_str()).map(str::len),
        Some(5_000)
    );
    assert_eq!(
        payload.get("task_id").and_then(|v| v.as_str()),
        Some(tid.to_string().as_str())
    );
    assert_eq!(
        payload.get("during_task").and_then(|v| v.as_bool()),
        Some(true)
    );
    // Timestamp is an RFC3339 string; just confirm it's non-empty.
    assert!(
        payload
            .get("timestamp")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "timestamp must be present + non-empty"
    );
}

#[tokio::test]
async fn worker_response_get_unknown_correlation_id_returns_cli_error() {
    // Not-found path. Must be a typed cli_error envelope (so
    // Python-side DaemonError fires) rather than a silent empty
    // text — otherwise the LLM would think the response genuinely
    // came back empty and not retry / ask Fett directly.
    let server = server_for_test();
    let unknown = Uuid::new_v4();

    let env = Envelope::new(
        MSG_CLI_WORKER_RESPONSE_GET,
        serde_json::json!({"correlation_id": unknown}),
    )
    .expect("build envelope");
    let resp = server.handle_cli_message(env).await;

    assert_eq!(resp.kind, MSG_CLI_ERROR);
    let err_msg = resp
        .payload
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        err_msg.contains("not found"),
        "error prose must say 'not found'; got {:?}",
        err_msg
    );
    // Include the id in the prose so logs / LLM context can
    // distinguish which id failed when multiple fetches race.
    assert!(
        err_msg.contains(&unknown.to_string()),
        "error prose must echo the unknown correlation_id"
    );
}

#[tokio::test]
async fn msg_worker_orch_response_handler_stashes_before_broadcast() {
    // Integration: the full handler path records to the cache so
    // a subsequent fetch resolves. Separate from the
    // cache-unit-tests above (which exercise AppState directly) —
    // this one pins the MSG_WORKER_ORCH_RESPONSE → cache write
    // wire-up.
    let server = server_for_test();
    let cid = Uuid::new_v4();
    let env = Envelope::new(
        MSG_WORKER_ORCH_RESPONSE,
        WorkerOrchResponse {
            agent_id: "ignored-per-connection-bound-rule".to_string(),
            correlation_id: cid,
            text: "y".repeat(4_000),
            during_task: false,
            task_id: None,
        },
    )
    .expect("build envelope");
    server.handle_message("claude-alor", env).await;

    // Cache populated with the full body — this is what the fetch
    // RPC would return. The agent_id on the record reflects the
    // connection-bound agent (security: workers can't forge a
    // reply on behalf of another agent).
    let rec = server
        .app_state
        .get_worker_response(cid)
        .expect("handler must have recorded the response before broadcast");
    assert_eq!(rec.text.len(), 4_000);
    assert_eq!(rec.agent_id, "claude-alor");
}

// ---------------------------------------------------------------------
// memory_get file_names filter (audit 8b03cae6 fix #5)
// ---------------------------------------------------------------------

#[test]
fn is_safe_hub_basename_accepts_plain_filenames() {
    // Positive cases — things callers legitimately pass.
    assert!(super::routing::is_safe_hub_basename("index.md"));
    assert!(super::routing::is_safe_hub_basename("notes.txt"));
    assert!(super::routing::is_safe_hub_basename("README"));
    assert!(super::routing::is_safe_hub_basename("file-with-dashes.md"));
    assert!(super::routing::is_safe_hub_basename("file.with.dots.md"));
    assert!(super::routing::is_safe_hub_basename(".hidden"));
}

#[test]
fn is_safe_hub_basename_rejects_traversal_and_hierarchical() {
    // Negative cases — traversal attempts + non-basename inputs.
    assert!(!super::routing::is_safe_hub_basename(""), "empty name");
    assert!(!super::routing::is_safe_hub_basename("."), "bare dot");
    assert!(!super::routing::is_safe_hub_basename(".."), "bare dot-dot");
    assert!(!super::routing::is_safe_hub_basename("../etc/passwd"));
    assert!(!super::routing::is_safe_hub_basename("/etc/passwd"));
    assert!(!super::routing::is_safe_hub_basename("/absolute"));
    assert!(!super::routing::is_safe_hub_basename("sub/nested.md"));
    assert!(!super::routing::is_safe_hub_basename("..\\windows-style"));
    assert!(!super::routing::is_safe_hub_basename("back\\slash.md"));
    // NUL bytes: belt-and-braces rejection.
    assert!(!super::routing::is_safe_hub_basename("file\0null.md"));
}

// ---------------------------------------------------------------------
// End-to-end handler tests. These mutate XDG_DATA_HOME (the hub
// dir anchor) so they MUST be serialized — the Rust test runner
// runs tests concurrently by default, and env-var mutation is
// process-global. A single process-wide mutex shared across every
// daemon submodule keeps them from racing each other. The mutex
// lives in `crate::daemon::test_env::XDG_LOCK`; any test that
// mutates XDG_DATA_HOME (here + daemon/project.rs +
// daemon/memory.rs) must hold it.
// ---------------------------------------------------------------------

/// RAII helper: sets `XDG_DATA_HOME` for the duration of the test,
/// creates the hub dir for `project`, writes the supplied files
/// into it, and restores the prior env on drop.
struct HubFixture {
    _guard: std::sync::MutexGuard<'static, ()>,
    _tempdir: tempfile::TempDir,
    prior_xdg: Option<std::ffi::OsString>,
}

impl HubFixture {
    fn new(project: &str, files: &[(&str, &str)]) -> Self {
        let guard = crate::daemon::test_env::XDG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tempdir = tempfile::tempdir().expect("tempdir");
        let prior_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("XDG_DATA_HOME", tempdir.path());

        // ProjectDirs resolves to {XDG_DATA_HOME}/alor on Linux,
        // so the hub lives at {tempdir}/alor/hubs/{project}.
        let hub = tempdir
            .path()
            .join("alor")
            .join("hubs")
            .join(project);
        std::fs::create_dir_all(&hub).expect("mkdir hub");
        for (name, content) in files {
            std::fs::write(hub.join(name), content).expect("write fixture");
        }

        Self {
            _guard: guard,
            _tempdir: tempdir,
            prior_xdg,
        }
    }
}

impl Drop for HubFixture {
    fn drop(&mut self) {
        match &self.prior_xdg {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
    }
}

/// Invoke the MSG_CLI_MEMORY_GET handler with the given payload
/// and return the response payload as serde_json::Value.
async fn memory_get_payload(
    server: &SocketServer,
    body: serde_json::Value,
) -> serde_json::Value {
    let env = Envelope::new(MSG_CLI_MEMORY_GET, body).expect("build envelope");
    server.handle_cli_message(env).await.payload
}

#[tokio::test]
async fn memory_get_no_filter_returns_all_hub_files() {
    // Back-compat: no file_names → return everything in the hub,
    // same shape as pre-fix. No `missing` field on the response.
    let _fx = HubFixture::new(
        "alor",
        &[
            ("index.md", "# Index\nsee README"),
            ("notes.md", "# Notes\none two three"),
        ],
    );
    let server = server_for_test();

    let payload = memory_get_payload(
        &server,
        serde_json::json!({"project": "alor"}),
    )
    .await;

    let files = payload.get("files").and_then(|v| v.as_object()).expect("files");
    assert_eq!(files.len(), 2, "all hub files returned");
    assert!(files.get("index.md").is_some());
    assert!(files.get("notes.md").is_some());
    assert!(
        payload.get("missing").is_none(),
        "no-filter response must not carry `missing`"
    );
}

#[tokio::test]
async fn memory_get_with_filter_returns_only_named_files() {
    // The main fix: caller pulls one file from a hub that has
    // several.
    let _fx = HubFixture::new(
        "alor",
        &[
            ("index.md", "# Index content"),
            ("notes.md", "# Notes content"),
            ("reference.md", "# Reference content"),
        ],
    );
    let server = server_for_test();

    let payload = memory_get_payload(
        &server,
        serde_json::json!({
            "project": "alor",
            "file_names": ["index.md"],
        }),
    )
    .await;

    let files = payload.get("files").and_then(|v| v.as_object()).expect("files");
    assert_eq!(files.len(), 1, "only the named file returned");
    assert_eq!(
        files.get("index.md").and_then(|v| v.as_str()),
        Some("# Index content")
    );
    // Filter-active path emits `missing`, empty here because the
    // one requested file was found on disk.
    let missing = payload
        .get("missing")
        .and_then(|v| v.as_array())
        .expect("missing present (filter active)");
    assert!(missing.is_empty());
}

#[tokio::test]
async fn memory_get_filter_treats_traversal_as_missing() {
    // Path-traversal rejection: `../etc/passwd`, `/etc/passwd`,
    // `sub/nested.md` must all surface in `missing` rather than
    // escape the hub dir.
    let _fx = HubFixture::new(
        "alor",
        &[("index.md", "# legitimate")],
    );
    let server = server_for_test();

    let payload = memory_get_payload(
        &server,
        serde_json::json!({
            "project": "alor",
            "file_names": [
                "../etc/passwd",
                "/etc/passwd",
                "sub/nested.md",
                "..",
                "",
            ],
        }),
    )
    .await;

    let files = payload.get("files").and_then(|v| v.as_object()).expect("files");
    assert!(
        files.is_empty(),
        "no traversal-variant name must be served"
    );
    let missing: Vec<String> = payload
        .get("missing")
        .and_then(|v| v.as_array())
        .expect("missing present")
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    // All rejected names surface as missing so the caller sees a
    // consistent "I asked for X, didn't get X" shape.
    for bad in ["../etc/passwd", "/etc/passwd", "sub/nested.md", "..", ""] {
        assert!(
            missing.contains(&bad.to_string()),
            "traversal/invalid name {bad:?} must appear in missing; got {missing:?}"
        );
    }
}

#[tokio::test]
async fn memory_get_filter_reports_unknown_files_as_missing_without_panic() {
    // Unknown but well-formed basenames → empty files, name in
    // missing. No panic, no error envelope.
    let _fx = HubFixture::new(
        "alor",
        &[("index.md", "# present")],
    );
    let server = server_for_test();

    let payload = memory_get_payload(
        &server,
        serde_json::json!({
            "project": "alor",
            "file_names": ["nonexistent.md", "also-missing.md"],
        }),
    )
    .await;

    let files = payload.get("files").and_then(|v| v.as_object()).expect("files");
    assert!(files.is_empty());
    let missing: Vec<String> = payload
        .get("missing")
        .and_then(|v| v.as_array())
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    assert!(missing.contains(&"nonexistent.md".to_string()));
    assert!(missing.contains(&"also-missing.md".to_string()));
}

#[tokio::test]
async fn memory_get_over_warn_threshold_still_serves_full_response() {
    // The MEMORY_GET_WARN_BYTES threshold (10 KiB) is operational
    // telemetry — a warn-log that fires when the orch keeps
    // tripping the size ceiling without using the file_names
    // filter — NOT a cap. Pin this contract: the response is
    // still served in full even when it goes well over the
    // threshold. (Capturing the warn-log itself would require
    // another dev-dep — tracing-test — for modest value; the
    // threshold constant lives next to the handler call-site
    // so a visual mismatch there is the more likely regression
    // shape anyway.)
    //
    // 12 KiB total across two files is comfortably over the
    // 10 KiB threshold.
    let body_a = "a".repeat(6_000);
    let body_b = "b".repeat(6_000);
    let _fx = HubFixture::new(
        "alor",
        &[
            ("big-a.md", body_a.as_str()),
            ("big-b.md", body_b.as_str()),
        ],
    );
    let server = server_for_test();

    let payload = memory_get_payload(
        &server,
        serde_json::json!({"project": "alor"}),
    )
    .await;

    let files = payload.get("files").and_then(|v| v.as_object()).expect("files");
    assert_eq!(files.len(), 2);
    let got_a = files.get("big-a.md").and_then(|v| v.as_str()).expect("big-a");
    let got_b = files.get("big-b.md").and_then(|v| v.as_str()).expect("big-b");
    assert_eq!(got_a.len(), 6_000, "full content of big-a served");
    assert_eq!(got_b.len(), 6_000, "full content of big-b served");
    // Total served bytes > threshold — confirms warn is advisory
    // rather than capping.
    assert!(got_a.len() + got_b.len() > MEMORY_GET_WARN_BYTES);
}

#[tokio::test]
async fn memory_get_empty_filter_is_equivalent_to_no_filter() {
    // Empty list `[]` must fall through to the whole-hub read,
    // matching None. Different wire shape (filter_active=false)
    // → no `missing` field emitted.
    let _fx = HubFixture::new(
        "alor",
        &[("index.md", "content"), ("notes.md", "more content")],
    );
    let server = server_for_test();

    let payload = memory_get_payload(
        &server,
        serde_json::json!({"project": "alor", "file_names": []}),
    )
    .await;

    let files = payload.get("files").and_then(|v| v.as_object()).expect("files");
    assert_eq!(files.len(), 2);
    assert!(
        payload.get("missing").is_none(),
        "empty filter must be treated as no filter — no `missing` in wire shape"
    );
}

// ---------------------------------------------------------------------
// cli.memory.append handler (audit item #7 — automated hub distillation)
// ---------------------------------------------------------------------

async fn memory_append_payload(
    server: &SocketServer,
    body: serde_json::Value,
) -> Envelope {
    let env = Envelope::new(crate::wrapper::protocol::MSG_CLI_MEMORY_APPEND, body)
        .expect("build envelope");
    server.handle_cli_message(env).await
}

#[tokio::test]
async fn memory_append_creates_new_hub_file() {
    let _fx = HubFixture::new("alor", &[]);
    let server = server_for_test();

    let resp = memory_append_payload(
        &server,
        serde_json::json!({
            "project": "alor",
            "file_name": "scratch.md",
            "text": "first line of curated note",
        }),
    )
    .await;
    assert_eq!(resp.kind, MSG_CLI_RESPONSE, "append returned success envelope");
    let payload = resp.payload;
    assert_eq!(payload["project"], "alor");
    assert_eq!(payload["file_name"], "scratch.md");
    assert_eq!(payload["bytes_written"], 26);

    // Follow up with memory.get to confirm the file is visible.
    let gotten = memory_get_payload(
        &server,
        serde_json::json!({"project": "alor", "file_names": ["scratch.md"]}),
    )
    .await;
    let content = gotten["files"]["scratch.md"].as_str().expect("file content");
    assert_eq!(content, "first line of curated note\n");
}

#[tokio::test]
async fn memory_append_appends_to_existing_file_with_newline_separator() {
    let _fx = HubFixture::new("alor", &[("notes.md", "seed line\n")]);
    let server = server_for_test();

    memory_append_payload(
        &server,
        serde_json::json!({
            "project": "alor",
            "file_name": "notes.md",
            "text": "follow-up line",
        }),
    )
    .await;

    let gotten = memory_get_payload(
        &server,
        serde_json::json!({"project": "alor", "file_names": ["notes.md"]}),
    )
    .await;
    let content = gotten["files"]["notes.md"].as_str().expect("content");
    assert_eq!(
        content, "seed line\nfollow-up line\n",
        "append preserves seed and separates entries with LF"
    );
}

#[tokio::test]
async fn memory_append_rejects_traversal_in_file_name() {
    let _fx = HubFixture::new("alor", &[]);
    let server = server_for_test();

    let resp = memory_append_payload(
        &server,
        serde_json::json!({
            "project": "alor",
            "file_name": "../escape.md",
            "text": "shouldn't land",
        }),
    )
    .await;
    assert_eq!(
        resp.kind, crate::wrapper::protocol::MSG_CLI_ERROR,
        "traversal attempt returns cli.error, not a silent success"
    );
    let err = resp.payload["error"].as_str().expect("error string");
    assert!(
        err.contains("unsafe file_name"),
        "error names the guard: {err:?}"
    );
}

#[tokio::test]
async fn memory_append_rejects_oversized_payload() {
    let _fx = HubFixture::new("alor", &[]);
    let server = server_for_test();

    let oversized = "x".repeat(crate::daemon::memory::MEMORY_APPEND_MAX_BYTES + 1);
    let resp = memory_append_payload(
        &server,
        serde_json::json!({
            "project": "alor",
            "file_name": "bloated.md",
            "text": oversized,
        }),
    )
    .await;
    assert_eq!(resp.kind, crate::wrapper::protocol::MSG_CLI_ERROR);
    let err = resp.payload["error"].as_str().expect("error");
    assert!(
        err.contains("MEMORY_APPEND_MAX_BYTES"),
        "error names the cap so the caller knows to split: {err:?}"
    );
}

#[tokio::test]
async fn task_complete_auto_distills_to_automation_log() {
    // End-to-end: a task.complete carrying a project + summary
    // should land a line in the hub's automation_log.md without
    // the caller doing anything explicit. This is the auto-
    // distillation surface described in
    // src-tauri/src/daemon/memory.rs::auto_distill_task_completion.
    let _fx = HubFixture::new("alor", &[]);
    let server = server_for_test();

    // Seed a task with a project so the auto-distill branch fires.
    let mut task = Task::new("[T1] fix the thing", "desc");
    task.project = Some("alor".to_string());
    let task_id = task.id;
    server.app_state.add_task(task);
    server
        .app_state
        .transition_task(task_id, TaskState::Assigned)
        .expect("assign");
    server
        .app_state
        .transition_task(task_id, TaskState::Accepted)
        .expect("accept");

    let complete_env = Envelope::new(
        MSG_TASK_COMPLETE,
        TaskComplete {
            task_id,
            summary: Some("Done. Verdict line goes here.".to_string()),
            details: Some("Full report body".to_string()),
            output: None,
        },
    )
    .expect("build envelope");
    // Register an agent_id on the connection (the handler uses
    // whatever `agent_id` the per-connection loop resolved).
    server
        .handle_message("claude-alor", complete_env)
        .await;

    // Read back the automation log via memory.get.
    let gotten = memory_get_payload(
        &server,
        serde_json::json!({
            "project": "alor",
            "file_names": [crate::daemon::memory::AUTOMATION_LOG_BASENAME],
        }),
    )
    .await;
    let content = gotten["files"][crate::daemon::memory::AUTOMATION_LOG_BASENAME]
        .as_str()
        .expect("log content present");
    assert!(content.starts_with("- "), "bullet entry shape");
    assert!(
        content.contains(" · claude-alor · "),
        "entry names the completing agent"
    );
    assert!(
        content.contains(" · [T1] fix the thing — Done. Verdict line goes here."),
        "entry carries title + em-dash + summary: {content:?}"
    );
}

#[tokio::test]
async fn task_complete_without_project_skips_auto_distill() {
    // No project → no hub to write to → no distill attempt. Silent
    // no-op (the handler just skips the block); we assert nothing
    // landed in any hub file.
    let _fx = HubFixture::new("alor", &[]);
    let server = server_for_test();

    let task = Task::new("projectless task", "desc");
    assert!(task.project.is_none(), "precondition: no project on task");
    let task_id = task.id;
    server.app_state.add_task(task);
    server
        .app_state
        .transition_task(task_id, TaskState::Assigned)
        .expect("assign");
    server
        .app_state
        .transition_task(task_id, TaskState::Accepted)
        .expect("accept");

    let complete_env = Envelope::new(
        MSG_TASK_COMPLETE,
        TaskComplete {
            task_id,
            summary: Some("Done.".to_string()),
            details: None,
            output: None,
        },
    )
    .expect("build envelope");
    server
        .handle_message("claude-alor", complete_env)
        .await;

    // Hub dir SHOULD still be empty (HubFixture seeded it empty).
    let gotten = memory_get_payload(
        &server,
        serde_json::json!({"project": "alor"}),
    )
    .await;
    let files = gotten["files"].as_object().expect("files");
    assert!(
        files.is_empty(),
        "no auto-distill fired: hub still empty: {files:?}"
    );
}

#[tokio::test]
async fn task_accept_is_idempotent_on_replay() {
    // Same shape for task.accept: Accepted → Accepted is otherwise an
    // illegal transition, which used to log a warn on every replay.
    let server = server_for_test();
    let task = Task::new("t", "d");
    let id = task.id;
    server.app_state.add_task(task);
    server
        .app_state
        .transition_task(id, TaskState::Assigned)
        .expect("assign");
    server
        .app_state
        .transition_task(id, TaskState::Accepted)
        .expect("accept");

    let replay = Envelope::new(MSG_TASK_ACCEPT, TaskAccept { task_id: id })
        .expect("build envelope");
    server.handle_message("test-agent", replay).await;

    let t = server.app_state.get_task(id).expect("present");
    assert_eq!(t.state, TaskState::Accepted);
}

// ---------------------------------------------------------------------------
// Accept-handshake watchdog (2026-04-20 follow-up to 8965b0ca).
//
// The state-transition layer is tested in `daemon::state::tests` —
// `revert_assignment_on_accept_timeout`, the new TaskState transitions,
// accept_attempts serde. Here we test the EVENT + LIFECYCLE layer:
// `handle_accept_timeout` (the watchdog body factored out so we don't
// wait 5s for the sleep), and the MSG_TASK_ACCEPT-cancels-watchdog path
// (`register_and_spawn_accept_watchdog` + handle_message("task.accept")
// round-trip).
// ---------------------------------------------------------------------------

/// Seed a task in Assigned state owned by the named agent. Mirrors
/// what `cli.assign` would persist before spawning the watchdog —
/// used by the handle_accept_timeout-without-real-timer tests below.
fn assigned_task_for_watchdog(state: &AppState, agent_id: &str, attempts: u32) -> Uuid {
    let mut t = Task::new("watchdog-test", "body");
    t.state = TaskState::Assigned;
    t.assigned_to = Some(agent_id.to_string());
    t.accept_attempts = attempts;
    let id = t.id;
    state.add_task(t);
    id
}

#[tokio::test]
async fn handle_accept_timeout_below_cap_reverts_to_pending() {
    let server = server_for_test();
    let id = assigned_task_for_watchdog(&server.app_state, "claude-alor", 0);

    // Pre-register a pending-accept entry so the handler's cleanup
    // step has something to remove (exercises that branch).
    {
        let (tx, _rx) = tokio::sync::oneshot::channel::<()>();
        server.pending_accepts.lock().await.insert(id, tx);
    }

    server.handle_accept_timeout(id, "claude-alor").await;

    let t = server.app_state.get_task(id).expect("present");
    assert_eq!(
        t.state,
        TaskState::Pending,
        "below cap → bounce to Pending"
    );
    assert_eq!(t.assigned_to, None, "assigned_to cleared on revert");
    assert_eq!(t.accept_attempts, 1);

    // Entry removed from pending_accepts.
    let pending = server.pending_accepts.lock().await;
    assert!(!pending.contains_key(&id), "pending_accepts entry cleaned");
}

#[tokio::test]
async fn handle_accept_timeout_at_cap_parks_in_acceptfailed() {
    let server = server_for_test();
    // Already at MAX_ACCEPT_ATTEMPTS - 1 == 2 from prior cycles; this
    // next timeout exhausts.
    let id = assigned_task_for_watchdog(&server.app_state, "claude-alor", 2);

    server.handle_accept_timeout(id, "claude-alor").await;

    let t = server.app_state.get_task(id).expect("present");
    assert_eq!(
        t.state,
        TaskState::AcceptFailed,
        "at cap → park as AcceptFailed (terminal)"
    );
    assert_eq!(t.accept_attempts, 3);
    // AcceptFailed is terminal.
    assert!(t.state.is_terminal());
}

#[tokio::test]
async fn handle_accept_timeout_swallows_accept_race_silently() {
    // Task already got transitioned to Accepted (by a MSG_TASK_ACCEPT
    // that arrived between the watchdog's timer-fire and its state-
    // grab). The watchdog must NOT clobber the successful accept.
    let server = server_for_test();
    let mut t = Task::new("accept race", "body");
    t.state = TaskState::Accepted;
    t.assigned_to = Some("claude-alor".to_string());
    let id = t.id;
    server.app_state.add_task(t);

    server.handle_accept_timeout(id, "claude-alor").await;

    let after = server.app_state.get_task(id).expect("present");
    assert_eq!(
        after.state,
        TaskState::Accepted,
        "race: Accepted state must survive watchdog timeout"
    );
    assert_eq!(after.accept_attempts, 0, "no spurious attempt increment");
    assert_eq!(
        after.assigned_to.as_deref(),
        Some("claude-alor"),
        "assignment must not be cleared"
    );
}

#[tokio::test]
async fn msg_task_accept_cancels_registered_watchdog() {
    // End-to-end: register a watchdog, fire a MSG_TASK_ACCEPT,
    // confirm the pending entry is gone and the task is Accepted.
    // Spawns the real watchdog (5s sleep) — test finishes in ms via
    // the ack path, so the sleep is never observed.
    let server = server_for_test();
    let agent_id = "claude-alor";

    let mut t = Task::new("ack-cancels-watchdog", "body");
    t.state = TaskState::Assigned;
    t.assigned_to = Some(agent_id.to_string());
    let id = t.id;
    server.app_state.add_task(t);

    // Spawn the real watchdog.
    server
        .register_and_spawn_accept_watchdog(id, agent_id.to_string())
        .await;
    assert!(
        server.pending_accepts.lock().await.contains_key(&id),
        "register: watchdog entry present"
    );

    // Fire the ack.
    let accept_env = Envelope::new(
        MSG_TASK_ACCEPT,
        crate::wrapper::protocol::TaskAccept { task_id: id },
    )
    .expect("build accept env");
    server.handle_message(agent_id, accept_env).await;

    // Task transitioned; watchdog entry cleaned.
    let after = server.app_state.get_task(id).expect("present");
    assert_eq!(after.state, TaskState::Accepted);
    assert!(
        !server.pending_accepts.lock().await.contains_key(&id),
        "ack cleaned the pending_accepts entry"
    );

    // Give the watchdog task one tick to notice the sender drop
    // and exit — if it were still running it'd blow up on the next
    // line (`server` is Clone, so its Arcs are still alive; but the
    // watchdog's select should already have resolved via the
    // `_ = rx` arm).
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    // Post-cleanup: task is still Accepted. If the watchdog had
    // raced and run the revert anyway, this would be Pending.
    let after2 = server.app_state.get_task(id).expect("present");
    assert_eq!(
        after2.state,
        TaskState::Accepted,
        "watchdog must NOT revert after ack cancelled it"
    );
    assert_eq!(after2.accept_attempts, 0, "no stray attempt increment");
}

#[tokio::test]
async fn duplicate_watchdog_registration_replaces_prior() {
    // Pathological case: two cli.assign calls for the same task
    // (shouldn't happen under normal flow, but the guard matters
    // for two racing orchs). Second registration must REPLACE
    // the first's sender — which drops the first, signalling its
    // watchdog to exit silently.
    let server = server_for_test();
    let agent = "claude-alor";

    let mut t = Task::new("duplicate watchdog", "body");
    t.state = TaskState::Assigned;
    t.assigned_to = Some(agent.to_string());
    let id = t.id;
    server.app_state.add_task(t);

    server
        .register_and_spawn_accept_watchdog(id, agent.to_string())
        .await;
    server
        .register_and_spawn_accept_watchdog(id, agent.to_string())
        .await;

    // Only one live entry — the latest overwrote the prior.
    let pending = server.pending_accepts.lock().await;
    assert_eq!(pending.len(), 1);
    assert!(pending.contains_key(&id));
    drop(pending);

    // Clean up so the live watchdog doesn't fire during suite tail.
    let mut pending = server.pending_accepts.lock().await;
    pending.remove(&id);
}

// ---------------------------------------------------------------------------
// Worker-exception → task.blocked → slot freeing (2026-04-20 codex-alor fix).
//
// The Python worker now fires `task.blocked` from `run_task`'s
// except-Exception arm (in addition to the pre-existing
// `wrapper.error` telemetry). These tests pin the daemon-side
// invariant that consumer relies on: receiving MSG_TASK_BLOCKED
// against an Accepted task must transition it to Blocked AND
// free the agent slot (drop it out of agent_active_task_count).
// Without that, max_concurrent=1 agents stay stuck at capacity
// after every worker bug.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn msg_task_blocked_on_accepted_transitions_and_frees_slot() {
    let server = server_for_test();
    let agent_id = "claude-alor";

    // Seed an agent + task in the Accepted state — exactly the
    // shape the daemon sees right before a worker exception fires.
    let mut agent = crate::daemon::state::Agent::new(agent_id, agent_id);
    agent.max_concurrent = 1;
    server.app_state.register_agent(agent);

    let mut t = Task::new("blocked-on-exception", "body");
    t.state = TaskState::Accepted;
    t.assigned_to = Some(agent_id.to_string());
    let id = t.id;
    server.app_state.add_task(t);

    // Pre-condition: slot at capacity.
    assert_eq!(
        server.app_state.agent_active_task_count(agent_id),
        1,
        "Accepted task must count as active before the blocked event",
    );

    // Deliver MSG_TASK_BLOCKED — mirrors what worker.py::run_task
    // now sends after catching an SDK-turn exception.
    let blocked_env = Envelope::new(
        crate::wrapper::protocol::MSG_TASK_BLOCKED,
        crate::wrapper::protocol::TaskBlocked {
            task_id: id,
            reason: "worker exception: RuntimeError('synthetic SDK failure')".to_string(),
            waiting_for: None,
        },
    )
    .expect("build blocked env");
    server.handle_message(agent_id, blocked_env).await;

    // Post-condition: state flipped to Blocked.
    let after = server.app_state.get_task(id).expect("present");
    assert_eq!(
        after.state,
        TaskState::Blocked,
        "MSG_TASK_BLOCKED must transition Accepted → Blocked",
    );

    // Slot is freed — agent_active_task_count drops Blocked tasks
    // (they're non-terminal in the Cancelled/Completed sense but
    // they're also NOT "actively running work", so the slot is
    // reassignable). Verify via the same counter that `cli.assign`
    // checks against max_concurrent.
    assert_eq!(
        server.app_state.agent_active_task_count(agent_id),
        0,
        "Blocked task must drop out of active count (slot freed for reassignment)",
    );
}

#[tokio::test]
async fn msg_task_blocked_carries_reason_into_broadcast_payload() {
    // Smoke: the reason string the worker sends must survive the
    // broadcast event's `reason` field so the orch can inject
    // something meaningful into its SDK context. We can't easily
    // assert broadcast-subscribed bytes in this test shape (no
    // subscriber), but the handler's transition + reason forward
    // happen before broadcast so the state-side fields are the
    // interesting part. Assert them directly.
    let server = server_for_test();
    let agent_id = "codex";

    let mut t = Task::new("reason-forward", "body");
    t.state = TaskState::Accepted;
    t.assigned_to = Some(agent_id.to_string());
    let id = t.id;
    server.app_state.add_task(t);

    let blocked_env = Envelope::new(
        crate::wrapper::protocol::MSG_TASK_BLOCKED,
        crate::wrapper::protocol::TaskBlocked {
            task_id: id,
            reason: "worker exception: ValueError('bad input')".to_string(),
            waiting_for: Some("operator to fix input".to_string()),
        },
    )
    .expect("build env");
    server.handle_message(agent_id, blocked_env).await;

    // State transition happened.
    let after = server.app_state.get_task(id).expect("present");
    assert_eq!(after.state, TaskState::Blocked);
}
