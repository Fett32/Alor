//! Wrapper-protocol + CLI message routing.
//!
//! Split out of `server.rs` during the god-module decomposition.
//! Owns the two big dispatch fns:
//!
//!   * `handle_message` — wrapper-protocol messages (task.accept,
//!     task.complete, task.blocked, user.intervention, worker.*,
//!     status.response, wrapper.error). Called from
//!     `connection.rs`'s per-wrapper read loop.
//!   * `handle_cli_message` — every `cli.*` command (status,
//!     assign, kill, task_create/get/list/cancel, memory_get,
//!     project_*, agent_*, worker_response_get, etc.). Called from
//!     `connection.rs`'s one-shot CLI branch.
//!
//! Also hosts `is_safe_hub_basename` — the file-name guard used by
//! the `cli.memory.get` handler to reject path traversal / odd
//! filenames. Kept here because it's only called from
//! `handle_cli_message`; promoting it to a shared utility module
//! would overstate its scope.
//!
//! No public API — every fn is `pub(super)` or module-private.
//! Callers reach these methods via `SocketServer`'s inherent impl.

use serde_json::json;
use std::collections::HashMap;
use tokio::io::AsyncWriteExt;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::daemon::memory;
use crate::daemon::project;
use crate::daemon::state::{Task, TaskState, WorkerResponseRecord};
use crate::wrapper::protocol::{
    truncate_summary, CliAgentEnsureRunning, CliAgentSendMessage, CliAssign, CliDelete, CliKill,
    CliMemoryAppend, CliMemoryGet, CliProjectGet, CliProjectSave, CliSpawn, CliStatus,
    CliTaskCancel, CliTaskCreate,
    CliTaskComplete as CliTaskCompletePayload, CliTaskGet, CliTaskInterventionClear,
    CliTaskList, CliWorkerResponseGet, Envelope, TaskAccept, TaskAssign,
    TaskBlocked, TaskComplete, TaskPropose, UserIntervention, WorkerFrameWedged,
    WorkerOrchResponse, WorkerUserInput, WrapperError, MSG_CLI_AGENT_ENSURE_RUNNING,
    MSG_CLI_AGENT_SEND_MESSAGE, MSG_CLI_ASSIGN, MSG_CLI_DELETE,
    MSG_CLI_KILL, MSG_CLI_MEMORY_APPEND, MSG_CLI_MEMORY_GET, MSG_CLI_PROJECT_GET,
    MSG_CLI_PROJECT_LIST, MSG_CLI_PROJECT_SAVE, MSG_CLI_RESPONSE, MSG_CLI_SPAWN,
    MSG_CLI_STATUS, MSG_CLI_TASK_CANCEL, MSG_CLI_TASK_COMPLETE, MSG_CLI_TASK_CREATE,
    MSG_CLI_TASK_INTERVENTION_CLEAR,
    MSG_CLI_TASK_GET, MSG_CLI_TASK_LIST, MSG_CLI_WORKER_RESPONSE_GET, MSG_ERROR,
    MSG_CLI_INTEGRATIONS_GET, MSG_STATUS_RESPONSE, MSG_TASK_ACCEPT, MSG_TASK_ASSIGN,
    MSG_TASK_BLOCKED, MSG_TASK_COMPLETE, MSG_TASK_PROPOSE, MSG_USER_INTERVENTION,
    MSG_WORKER_FRAME_WEDGED, MSG_WORKER_ORCH_RESPONSE, MSG_WORKER_USER_INPUT,
    DEFAULT_TASK_GET_VIEW, EVENT_TEXT_INJECT_MAX_BYTES, MEMORY_GET_WARN_BYTES,
    TASK_LIST_FULL_MAX_LIMIT, TASK_SUMMARY_MAX_BYTES, WORKER_ECHO_SENTINEL_BEGIN,
    WORKER_ECHO_SENTINEL_END, ERR_CODE_FRAMED_SEND_NOT_SUPPORTED,
    ACCEPT_ACK_TIMEOUT_SECS, MAX_ACCEPT_ATTEMPTS,
};

use super::{cli_error, cli_error_coded, SocketServer};
use super::agent_lifecycle::is_framed_send_allowed;

impl SocketServer {

    /// Handle one envelope from a wrapper.
    pub(super) async fn handle_message(&self, agent_id: &str, env: Envelope) {
        match env.kind.as_str() {
            MSG_TASK_ACCEPT => {
                if let Ok(payload) = env.decode_payload::<TaskAccept>() {
                    // Cancel the accept-handshake watchdog FIRST (before
                    // the idempotent-replay guard). Dropping the
                    // oneshot sender closes the receiver in the
                    // watchdog's select arm, so the watchdog exits
                    // quietly without firing a spurious revert.
                    // Doing this unconditionally — even for replays —
                    // is safe because the map only contains entries
                    // for IN-FLIGHT handshakes: on first successful
                    // ack the entry is already gone, and the
                    // `remove()` is a no-op.
                    {
                        let mut pending = self.pending_accepts.lock().await;
                        if let Some(tx) = pending.remove(&payload.task_id) {
                            // Explicit drop for clarity — closing the
                            // channel is what signals the watchdog.
                            drop(tx);
                        }
                    }

                    // Idempotent-replay guard. Worker outboxes can re-deliver
                    // an accept the daemon already processed before its
                    // previous crash (Accepted → Accepted is otherwise an
                    // illegal transition). Silent no-op on replay.
                    if let Some(existing) = self.app_state.get_task(payload.task_id) {
                        if existing.state == TaskState::Accepted {
                            tracing::trace!(
                                agent_id,
                                task_id = %payload.task_id,
                                "ignoring task.accept for already-accepted task (outbox replay)"
                            );
                            return;
                        }
                    }
                    info!(agent_id, task_id = %payload.task_id, "task accepted");
                    if let Err(e) = self.app_state.transition_task(payload.task_id, TaskState::Accepted) {
                        warn!("transition to Accepted failed: {e}");
                    }
                    self.broadcast_event(
                        "task.accepted",
                        json!({"task_id": payload.task_id.to_string(), "agent_id": agent_id}),
                    )
                    .await;
                }
            }

            MSG_TASK_PROPOSE => {
                if let Ok(payload) = env.decode_payload::<TaskPropose>() {
                    info!(agent_id, task_id = %payload.task_id, "task proposal received");
                    if let Err(e) = self.app_state.propose_task(
                        payload.task_id,
                        payload.brief,
                        payload.diff,
                    ) {
                        warn!("propose_task failed: {e}");
                    }
                    self.broadcast_event(
                        "task.proposed",
                        json!({"task_id": payload.task_id.to_string(), "agent_id": agent_id}),
                    )
                    .await;
                }
            }

            MSG_TASK_COMPLETE => {
                if let Ok(payload) = env.decode_payload::<TaskComplete>() {
                    // Idempotent-replay guard. Worker outboxes can re-deliver
                    // a completion the daemon already processed before its
                    // previous crash. Without this, the replay would (a) log
                    // a warn from the illegal Completed → Completed
                    // transition and (b) still fire the tmux orchestrator
                    // ping below — double-notifying for the same completion.
                    // Treat as a silent no-op: no summary overwrite, no
                    // broadcast, no tmux injection.
                    if let Some(existing) = self.app_state.get_task(payload.task_id) {
                        if existing.state == TaskState::Completed {
                            tracing::trace!(
                                agent_id,
                                task_id = %payload.task_id,
                                "ignoring task.complete for already-completed task (outbox replay)"
                            );
                            return;
                        }
                    }
                    info!(agent_id, task_id = %payload.task_id, "task complete");
                    let task_title = self.app_state.get_task(payload.task_id)
                        .map(|t| t.title.clone())
                        .unwrap_or_default();

                    // Summary / details split — hot-path bloat fix (audit
                    // 8b03cae6 #1). The `summary` field is the terse one-
                    // paragraph report that gets injected into the
                    // orchestrator's SDK context on every `task.completed`
                    // event. Hard-cap at TASK_SUMMARY_MAX_BYTES (512 B)
                    // with a "… [truncated]" marker so a runaway worker
                    // can't flood orch context with a multi-KB report.
                    // The full report lives in `details` (if the worker
                    // split it out) and is reachable via `task_get`.
                    //
                    // Back-compat path: old workers send a single summary
                    // field with the whole report. They'll hit the
                    // truncation branch and log a warn — harmless, just
                    // the orch gets a clipped summary until the worker
                    // is updated to send `details` explicitly.
                    let summary_terse = payload.summary.as_ref().map(|s| {
                        if s.len() > TASK_SUMMARY_MAX_BYTES {
                            warn!(
                                agent_id,
                                task_id = %payload.task_id,
                                original_bytes = s.len(),
                                cap = TASK_SUMMARY_MAX_BYTES,
                                "task.complete summary exceeds cap; truncating for orch injection (worker should supply a terse `summary` + full `details`)"
                            );
                            truncate_summary(s, TASK_SUMMARY_MAX_BYTES)
                        } else {
                            s.clone()
                        }
                    });
                    if let Some(ref s) = summary_terse {
                        self.app_state.set_task_summary(payload.task_id, s.clone());
                    }
                    if let Some(ref d) = payload.details {
                        self.app_state.set_task_details(payload.task_id, d.clone());
                    }
                    let has_details = payload.details.is_some();
                    // Only broadcast task.completed if the transition actually
                    // succeeded — otherwise the orch hears "done" but state
                    // still says not-done.
                    //
                    // `title` is carried in the payload so the orchestrator's
                    // event formatter can render a single rich notification
                    // with agent + title + summary. A previous iteration also
                    // injected a terse "[Alor] Task completed by <agent>" line
                    // into the orchestrator's tmux session via send-keys as a
                    // belt-and-braces notification path for non-SDK orchs —
                    // but the current Claude-SDK orchestrator (main.py)
                    // already subscribes to this event, and the tmux line
                    // landed as stdin on top of the SDK injection, firing a
                    // duplicate turn per completion. Killed.
                    //
                    // `has_details` tells the orch formatter whether to
                    // append the "(full report available via task_get)"
                    // pointer. The full `details` body is NOT included in
                    // the broadcast — that's the whole point of the split.
                    match self.app_state.transition_task(payload.task_id, TaskState::Completed) {
                        Ok(_) => {
                            // Automatic Memory Hub distillation (audit
                            // item #7). Fire-and-forget: any error
                            // logs a warn but doesn't fail the task
                            // completion. Only runs when BOTH a
                            // project AND a non-empty summary are
                            // present — a summary-less completion
                            // has nothing meaningful to distill, and
                            // a projectless task has no hub to write
                            // to. See `memory::auto_distill_task_completion`
                            // for the entry shape + retention policy.
                            if let Some(task) = self.app_state.get_task(payload.task_id) {
                                if let (Some(project), Some(summary)) =
                                    (task.project.as_deref(), summary_terse.as_deref())
                                {
                                    let trimmed = summary.trim();
                                    if !project.is_empty() && !trimmed.is_empty() {
                                        if let Err(e) = memory::auto_distill_task_completion(
                                            project,
                                            agent_id,
                                            &payload.task_id,
                                            &task_title,
                                            trimmed,
                                        ) {
                                            warn!(
                                                agent_id,
                                                task_id = %payload.task_id,
                                                project,
                                                "memory auto-distill failed: {e}"
                                            );
                                        }
                                    }
                                }
                            }
                            self.broadcast_event(
                                "task.completed",
                                json!({
                                    "task_id": payload.task_id.to_string(),
                                    "agent_id": agent_id,
                                    "title": task_title,
                                    "summary": summary_terse,
                                    "has_details": has_details,
                                }),
                            )
                            .await;
                        }
                        Err(e) => {
                            warn!("transition to Completed failed: {e}; not broadcasting");
                        }
                    }
                }
            }

            MSG_TASK_BLOCKED => {
                if let Ok(payload) = env.decode_payload::<TaskBlocked>() {
                    info!(agent_id, task_id = %payload.task_id, reason = %payload.reason, "task blocked");
                    if let Err(e) = self.app_state.transition_task(payload.task_id, TaskState::Blocked) {
                        warn!("transition to Blocked failed: {e}");
                    }
                    self.broadcast_event(
                        "task.blocked",
                        json!({
                            "task_id": payload.task_id.to_string(),
                            "agent_id": agent_id,
                            "reason": payload.reason,
                        }),
                    )
                    .await;
                }
            }

            MSG_USER_INTERVENTION => {
                // Ignore the payload's agent_id — use the connection-bound id
                // so a wrapper can't flag intervention on behalf of another.
                if env.decode_payload::<UserIntervention>().is_ok() {
                    info!(agent_id, "user intervention recorded");
                    let affected = self.app_state.record_user_intervention(agent_id);
                    let task_ids: Vec<String> =
                        affected.iter().map(|id| id.to_string()).collect();
                    self.broadcast_event(
                        "user.intervention",
                        json!({
                            "agent_id": agent_id,
                            "task_ids": task_ids,
                        }),
                    )
                    .await;
                }
            }

            MSG_WORKER_USER_INPUT => {
                // SDK-worker stdin forwarded as an event. agent_id is the
                // connection-bound one so a worker can't impersonate others.
                if let Ok(payload) = env.decode_payload::<WorkerUserInput>() {
                    info!(
                        agent_id,
                        during_task = payload.during_task,
                        bytes = payload.text.len(),
                        "worker user input received"
                    );
                    self.broadcast_event(
                        "worker.user_input",
                        json!({
                            "agent_id": agent_id,
                            "text": payload.text,
                            "during_task": payload.during_task,
                            "task_id": payload.task_id.map(|t| t.to_string()),
                        }),
                    )
                    .await;
                }
            }

            MSG_WORKER_ORCH_RESPONSE => {
                // Mirror of MSG_WORKER_USER_INPUT for the orch→worker→orch
                // reply direction. The worker fires this after the SDK turn
                // triggered by a sentinel-prefixed `cli.agent.send_message`
                // completes (ResultMessage). correlation_id is the uuid the
                // daemon embedded in the outbound sentinel so orch can match
                // the reply to its originating send.
                if let Ok(payload) = env.decode_payload::<WorkerOrchResponse>() {
                    let will_truncate_in_orch =
                        payload.text.len() > EVENT_TEXT_INJECT_MAX_BYTES;
                    info!(
                        agent_id,
                        correlation_id = %payload.correlation_id,
                        during_task = payload.during_task,
                        bytes = payload.text.len(),
                        will_truncate_in_orch,
                        "worker orch response received"
                    );
                    // Stash the full text in the fetch-on-demand cache
                    // BEFORE broadcasting. Orch's event formatter caps
                    // the injected text at EVENT_TEXT_INJECT_MAX_BYTES
                    // (2 KiB) and appends a
                    // `worker_response_get(correlation_id=X)` pointer
                    // when it truncates — this record is what the
                    // subsequent `cli.worker.response.get` returns.
                    // Audit 8b03cae6 bloat fix #4.
                    self.app_state.record_worker_response(WorkerResponseRecord {
                        correlation_id: payload.correlation_id,
                        agent_id: agent_id.to_string(),
                        text: payload.text.clone(),
                        task_id: payload.task_id,
                        during_task: payload.during_task,
                        timestamp: chrono::Utc::now(),
                    });
                    self.broadcast_event(
                        "worker.orch_response",
                        json!({
                            "agent_id": agent_id,
                            "correlation_id": payload.correlation_id.to_string(),
                            "text": payload.text,
                            "during_task": payload.during_task,
                            "task_id": payload.task_id.map(|t| t.to_string()),
                        }),
                    )
                    .await;
                }
            }

            MSG_WORKER_FRAME_WEDGED => {
                // SDK worker dropped a stale BEGIN frame on nested-BEGIN
                // recovery. Log loudly — this is the telemetry that lets us
                // attribute a pending agent_send_message_await timeout to a
                // wedge instead of a routine END-loss — and broadcast so
                // orchestrator-py's await loop can surface a typed error.
                if let Ok(payload) = env.decode_payload::<WorkerFrameWedged>() {
                    warn!(
                        agent_id,
                        dropped_uuid = %payload.dropped_uuid,
                        new_uuid = %payload.new_uuid,
                        bytes_dropped = payload.bytes_dropped,
                        lines_dropped = payload.lines_dropped,
                        task_id = ?payload.task_id,
                        "worker frame wedged — stale BEGIN discarded on nested-BEGIN recovery"
                    );
                    self.broadcast_event(
                        "worker.frame_wedged",
                        json!({
                            "agent_id": agent_id,
                            "dropped_uuid": payload.dropped_uuid.to_string(),
                            "new_uuid": payload.new_uuid.to_string(),
                            "bytes_dropped": payload.bytes_dropped,
                            "lines_dropped": payload.lines_dropped,
                            "task_id": payload.task_id.map(|t| t.to_string()),
                        }),
                    )
                    .await;
                }
            }

            MSG_STATUS_RESPONSE => {
                // Could update heartbeat timestamp here
                debug!(agent_id, "status response received");
            }

            MSG_ERROR => {
                if let Ok(payload) = env.decode_payload::<WrapperError>() {
                    error!(agent_id, message = %payload.message, "wrapper error");
                    self.broadcast_event(
                        "wrapper.error",
                        json!({
                            "agent_id": agent_id,
                            "message": payload.message,
                        }),
                    )
                    .await;
                }
            }

            other => {
                warn!(agent_id, kind = other, "unknown message type");
            }
        }
    }

    /// Handle a CLI message and return a response envelope.
    pub(super) async fn handle_cli_message(&self, env: Envelope) -> Envelope {
        let correlation_id = env.correlation_id;

        match env.kind.as_str() {
            MSG_CLI_STATUS => {
                // View selection:
                //   - no payload / missing `view` / "summary" / unknown
                //     → summary (default). agents only, task_history
                //       dropped, top-level `tasks` omitted entirely.
                //   - "full" → backwards-compatible firehose.
                //
                // The orchestrator's agent_list tool calls this on
                // every routing decision; summary keeps the per-call
                // context cost from ballooning with each accumulated
                // task (dogfood saw 347k+ chars at 100+ tasks, almost
                // all of which was the top-level `tasks` array).
                // alor-cli and any consumer that wants the firehose
                // opts in explicitly with view=full.
                let payload: CliStatus = env.decode_payload().unwrap_or_default();
                let requested_view = payload.view.as_deref().unwrap_or(
                    crate::wrapper::protocol::DEFAULT_STATUS_VIEW,
                );
                // Derive `connected[]` from the per-agent `connected`
                // flag rather than from `self.writers.keys()`. The
                // writers map is the transport layer's record of
                // "sockets I can currently write to" — intentionally
                // kept around by `mark_agent_zombie` /
                // `mark_agent_killed` even after the state flag has
                // been flipped to false (so a lingering wrapper can
                // still receive SHUTDOWN). Reporting that list as
                // `connected[]` created a split-brain: cursor-alor
                // showed as "connected" in agent_list while its
                // per-agent record said `connected: false`. The
                // per-agent flag is the authoritative view; every
                // mutation of it funnels through
                // `AppState::set_agent_connected`, which means this
                // derivation can't drift.
                let connected: Vec<String> = self.app_state.connected_agent_ids();

                let response_json = match requested_view {
                    "full" => {
                        let agents = self.app_state.all_agents();
                        let tasks = self.app_state.all_tasks();
                        json!({
                            "view": "full",
                            "agents": agents,
                            "tasks": tasks,
                            "connected": connected,
                        })
                    }
                    _ => {
                        // Summary: project each agent with its set of
                        // non-terminal assigned task ids. Compute the
                        // mapping once from all_tasks then distribute
                        // per-agent so the lookup is O(tasks + agents)
                        // rather than O(tasks * agents).
                        let all_tasks = self.app_state.all_tasks();
                        let mut active_by_agent: HashMap<String, Vec<Uuid>> = HashMap::new();
                        for t in &all_tasks {
                            if t.state.is_terminal() {
                                continue;
                            }
                            if let Some(ref aid) = t.assigned_to {
                                active_by_agent
                                    .entry(aid.clone())
                                    .or_default()
                                    .push(t.id);
                            }
                        }
                        let agents: Vec<_> = self
                            .app_state
                            .all_agents()
                            .into_iter()
                            .map(|a| {
                                let current = active_by_agent
                                    .remove(&a.id)
                                    .unwrap_or_default();
                                a.summary(current)
                            })
                            .collect();
                        json!({
                            "view": "summary",
                            "agents": agents,
                            "connected": connected,
                        })
                    }
                };

                match Envelope::new(MSG_CLI_RESPONSE, response_json) {
                    Ok(mut e) => {
                        e.correlation_id = correlation_id;
                        e
                    }
                    Err(_) => cli_error(correlation_id, "failed to build status response"),
                }
            }

            MSG_CLI_TASK_LIST => {
                // Optional state_filter on the payload. Missing payload or
                // `None` / "default" → hide terminal tasks (Completed /
                // Cancelled / Rejected / TimedOut / Stale) so the orch's
                // runtime context stays lean. "all" → no filtering. Any
                // other value → exact state match (SCREAMING_SNAKE_CASE).
                //
                // Pagination: `limit` defaults to DEFAULT_TASK_LIST_LIMIT
                // (20 — sized to keep responses under the 25k-token orch
                // ceiling at typical task sizes). `limit=0` disables
                // capping for callers that explicitly want the whole
                // filtered set. `offset` defaults to 0.
                //
                // Sort before paginating so pages are stable across
                // calls: non-terminal first, then updated_at desc.
                // Matches the UI ordering so page 0 here = same tasks
                // the orch sees "at the top" of the UI.
                let payload: CliTaskList = env.decode_payload().unwrap_or_default();
                let mut tasks = self.app_state.all_tasks();
                let filter = payload.state_filter.as_deref().unwrap_or("default");
                match filter {
                    "all" => {}
                    "default" => tasks.retain(|t| !t.state.is_terminal()),
                    state_name => {
                        tasks.retain(|t| {
                            // Compare serialized form so callers can pass
                            // "COMPLETED" etc. without needing the Rust enum.
                            serde_json::to_value(&t.state)
                                .ok()
                                .and_then(|v| v.as_str().map(str::to_string))
                                == Some(state_name.to_string())
                        });
                    }
                }

                tasks.sort_by(|a, b| {
                    let a_term = a.state.is_terminal();
                    let b_term = b.state.is_terminal();
                    a_term.cmp(&b_term).then(b.updated_at.cmp(&a.updated_at))
                });

                let total = tasks.len() as u32;
                let offset = payload.offset.unwrap_or(0);
                let requested_limit = payload
                    .limit
                    .unwrap_or(crate::wrapper::protocol::DEFAULT_TASK_LIST_LIMIT);

                // Resolve view first — the clamp below depends on it.
                // Projection: default to "summary" (lean 5-field shape
                // sized for context-safe scans). "full" returns the
                // whole Task. Anything else silently falls back to
                // summary — an LLM typo mustn't crash the list.
                let requested_view = payload.view.as_deref().unwrap_or(
                    crate::wrapper::protocol::DEFAULT_TASK_LIST_VIEW,
                );
                let is_full_view = requested_view == "full";

                // Audit 8b03cae6 fix #3: hard-clamp on `view="full"` so a
                // caller that passes `limit=0` (no limit) or a large
                // limit can't land 100+ full-serialized Task objects in
                // a single orch tool response. 50 × ~1.1 KB = ~55 KB
                // worst case, bounded. Summary view remains uncapped —
                // the projection is lean enough that unlimited scans
                // are safe at realistic task counts.
                //
                // `limit = 0` on full view is the sneakiest case: the
                // existing handler treated 0 as "no limit", which let
                // the clamp escape. We unify both oversize cases here:
                // any full-view limit that would return > MAX_LIMIT
                // gets clamped (including the sentinel 0 → unlimited
                // reading).
                let (effective_limit, limit_clamped_from) = if is_full_view {
                    let would_exceed =
                        requested_limit == 0 || requested_limit > TASK_LIST_FULL_MAX_LIMIT;
                    if would_exceed {
                        warn!(
                            requested_limit,
                            clamped_to = TASK_LIST_FULL_MAX_LIMIT,
                            "task_list: clamping view=full limit (see audit 8b03cae6 fix #3; \
                             caller should paginate via offset or use view=summary for scans)"
                        );
                        (TASK_LIST_FULL_MAX_LIMIT, Some(requested_limit))
                    } else {
                        (requested_limit, None)
                    }
                } else {
                    // Summary view: leave limit=0 as "no limit" intact
                    // (cheap per-task projection; unlimited scans are
                    // fine). No clamp metadata in the response.
                    (requested_limit, None)
                };

                let start = (offset as usize).min(tasks.len());
                let end = if effective_limit == 0 {
                    tasks.len()
                } else {
                    (start + effective_limit as usize).min(tasks.len())
                };
                let page: Vec<_> = tasks[start..end].to_vec();
                let returned = page.len() as u32;
                let has_more = (start + page.len()) < tasks.len();

                // Emit the view string the server actually applied so
                // a caller that typoed (and fell through to summary)
                // sees which shape came back.
                let (tasks_json, view_emitted): (serde_json::Value, &'static str) =
                    if is_full_view {
                        (json!(page), "full")
                    } else {
                        let summaries: Vec<_> =
                            page.iter().map(|t| t.summary()).collect();
                        (json!(summaries), "summary")
                    };

                // Build response. When the full-view clamp fired we
                // add `limit_clamped_from` + `limit_applied` so the
                // caller can recognize the cap. Absent on unclamped
                // responses so the normal path's shape is unchanged
                // (back-compat for anyone eyeballing the envelope).
                let mut resp = json!({
                    "tasks": tasks_json,
                    "total": total,
                    "returned": returned,
                    "offset": offset,
                    "has_more": has_more,
                    "view": view_emitted,
                });
                if let Some(orig) = limit_clamped_from {
                    let obj = resp.as_object_mut().expect("resp is a JSON object");
                    obj.insert("limit_clamped_from".to_string(), json!(orig));
                    obj.insert("limit_applied".to_string(), json!(effective_limit));
                }
                match Envelope::new(MSG_CLI_RESPONSE, resp) {
                    Ok(mut e) => {
                        e.correlation_id = correlation_id;
                        e
                    }
                    Err(_) => cli_error(correlation_id, "failed to build task list"),
                }
            }

            MSG_CLI_TASK_GET => {
                // Audit 8b03cae6 bloat fix #2: `task_get` used to
                // unconditionally return the full Task — description +
                // summary + details + proposal_brief + proposal_diff —
                // which the orchestrator paid for on every routine
                // "has this completed?" check (2–10 KB; up to 10 MB if
                // proposal_diff was populated near its cap). Summary
                // view is the lean default now; consumers that need
                // the heavy bodies opt into `view="full"` — and the
                // `has_*` flags on the summary tell them when that's
                // worth doing.
                match env.decode_payload::<CliTaskGet>() {
                    Ok(payload) => {
                        let requested_view = payload.view.as_deref().unwrap_or(
                            DEFAULT_TASK_GET_VIEW,
                        );
                        match self.app_state.get_task(payload.task_id) {
                            Some(task) => {
                                let response_json = match requested_view {
                                    "full" => json!({
                                        "view": "full",
                                        "task": task,
                                    }),
                                    _ => json!({
                                        "view": "summary",
                                        "task": task.get_summary(),
                                    }),
                                };
                                match Envelope::new(MSG_CLI_RESPONSE, response_json) {
                                    Ok(mut e) => {
                                        e.correlation_id = correlation_id;
                                        e
                                    }
                                    Err(_) => cli_error(correlation_id, "failed to serialize task"),
                                }
                            }
                            None => cli_error(
                                correlation_id,
                                &format!("task {} not found", payload.task_id),
                            ),
                        }
                    }
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_TASK_CREATE => {
                match env.decode_payload::<CliTaskCreate>() {
                    Ok(payload) => {
                        let mut task = Task::new(&payload.title, &payload.description);
                        task.project = payload.project.clone();
                        let task_id = task.id;
                        self.app_state.add_task(task);
                        info!(task_id = %task_id, title = %payload.title, "task created via CLI");
                        self.broadcast_event(
                            "task.created",
                            json!({"task_id": task_id.to_string(), "title": payload.title}),
                        )
                        .await;
                        match Envelope::new(
                            MSG_CLI_RESPONSE,
                            json!({"task_id": task_id.to_string()}),
                        ) {
                            Ok(mut e) => {
                                e.correlation_id = correlation_id;
                                e
                            }
                            Err(_) => cli_error(correlation_id, "failed to build response"),
                        }
                    }
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_TASK_INTERVENTION_CLEAR => {
                match env.decode_payload::<CliTaskInterventionClear>() {
                    Ok(payload) => {
                        match self.app_state.clear_user_intervention(payload.task_id) {
                            Ok(task) => {
                                match Envelope::new(
                                    MSG_CLI_RESPONSE,
                                    json!({
                                        "task_id": payload.task_id.to_string(),
                                        "user_intervened": task.user_intervened,
                                    }),
                                ) {
                                    Ok(mut e) => {
                                        e.correlation_id = correlation_id;
                                        e
                                    }
                                    Err(_) => cli_error(
                                        correlation_id,
                                        "failed to build response",
                                    ),
                                }
                            }
                            Err(e) => cli_error(
                                correlation_id,
                                &format!("failed to clear intervention: {e}"),
                            ),
                        }
                    }
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_TASK_CANCEL => {
                match env.decode_payload::<CliTaskCancel>() {
                    Ok(payload) => {
                        // Try Cancelled first, fall back to Rejected
                        let result = self
                            .app_state
                            .transition_task(payload.task_id, TaskState::Cancelled)
                            .or_else(|_| {
                                self.app_state
                                    .transition_task(payload.task_id, TaskState::Rejected)
                            });
                        match result {
                            Ok(_task) => {
                                self.broadcast_event(
                                    "task.cancelled",
                                    json!({"task_id": payload.task_id.to_string()}),
                                )
                                .await;
                                match Envelope::new(
                                    MSG_CLI_RESPONSE,
                                    json!({"cancelled": payload.task_id.to_string()}),
                                ) {
                                    Ok(mut e) => {
                                        e.correlation_id = correlation_id;
                                        e
                                    }
                                    Err(_) => cli_error(correlation_id, "failed to build response"),
                                }
                            }
                            Err(e) => cli_error(
                                correlation_id,
                                &format!("failed to cancel task: {e}"),
                            ),
                        }
                    }
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_TASK_COMPLETE => {
                match env.decode_payload::<CliTaskCompletePayload>() {
                    Ok(payload) => {
                        // Stash summary *before* the transition so it is
                        // visible in the Completed task snapshot emitted by
                        // `tasks-changed` / `task.completed`.
                        if let Some(ref summary) = payload.summary {
                            self.app_state
                                .set_task_summary(payload.task_id, summary.clone());
                        }
                        match self
                            .app_state
                            .transition_task(payload.task_id, TaskState::Completed)
                        {
                            Ok(_task) => {
                                self.broadcast_event(
                                    "task.completed",
                                    json!({"task_id": payload.task_id.to_string()}),
                                )
                                .await;
                                match Envelope::new(
                                    MSG_CLI_RESPONSE,
                                    json!({"completed": payload.task_id.to_string()}),
                                ) {
                                    Ok(mut e) => {
                                        e.correlation_id = correlation_id;
                                        e
                                    }
                                    Err(_) => cli_error(correlation_id, "failed to build response"),
                                }
                            }
                            Err(e) => cli_error(
                                correlation_id,
                                &format!("failed to complete task: {e}"),
                            ),
                        }
                    }
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_ASSIGN => {
                match env.decode_payload::<CliAssign>() {
                    Ok(payload) => {
                        // Enforce max_concurrent BEFORE doing any work. Counts
                        // non-terminal, non-pending tasks already assigned to
                        // this agent. If over capacity, reject cleanly so the
                        // caller can queue or spawn a sibling slot.
                        let active = self.app_state.agent_active_task_count(&payload.agent_id);
                        let max = self
                            .app_state
                            .get_agent(&payload.agent_id)
                            .map(|a| a.max_concurrent as usize)
                            .unwrap_or(1);
                        if active >= max {
                            return cli_error(
                                correlation_id,
                                &format!(
                                    "agent {} at capacity ({}/{}); spawn a sibling slot or wait",
                                    payload.agent_id, active, max
                                ),
                            );
                        }

                        // Look up task (no lock contention).
                        let task = match self.app_state.get_task(payload.task_id) {
                            Some(t) => t,
                            None => {
                                return cli_error(
                                    correlation_id,
                                    &format!("task {} not found", payload.task_id),
                                )
                            }
                        };

                        // If the task is tied to a project, prepend a TASK BRIEF
                        // with key files + docs so the agent has context.
                        let dispatch_description = match task.project.as_deref() {
                            Some(name) => match project::load_profile(name) {
                                Ok(Some(profile)) => project::build_task_brief(
                                    &profile,
                                    &task.title,
                                    &task.description,
                                ),
                                Ok(None) => {
                                    warn!(project = %name, "project profile not found; dispatching raw description");
                                    task.description.clone()
                                }
                                Err(e) => {
                                    warn!(project = %name, "failed to load project profile: {e}; dispatching raw description");
                                    task.description.clone()
                                }
                            },
                            None => task.description.clone(),
                        };

                        // Build the envelope before locking writers.
                        let assign_env = match Envelope::new(
                            MSG_TASK_ASSIGN,
                            TaskAssign {
                                task_id: task.id,
                                title: task.title.clone(),
                                description: dispatch_description,
                                timeout_secs: None,
                            },
                        ) {
                            Ok(e) => e,
                            Err(e) => {
                                return cli_error(
                                    correlation_id,
                                    &format!("failed to build assign envelope: {e}"),
                                )
                            }
                        };

                        // Hold the writers lock around persist+send so the
                        // agent can't disconnect between the two, and so
                        // state.json has the assignment before the wrapper
                        // hears about it (persist-before-broadcast).
                        {
                            let mut writers = self.writers.lock().await;
                            if !writers.contains_key(&payload.agent_id) {
                                return cli_error(
                                    correlation_id,
                                    &format!("agent {} is not connected", payload.agent_id),
                                );
                            }

                            // Persist assignment first.
                            if let Err(e) = self
                                .app_state
                                .assign_task_to_agent(payload.task_id, &payload.agent_id)
                            {
                                return cli_error(
                                    correlation_id,
                                    &format!("failed to update task state: {e}"),
                                );
                            }

                            let writer = writers.get_mut(&payload.agent_id).unwrap();
                            let mut line = match serde_json::to_string(&assign_env) {
                                Ok(l) => l,
                                Err(e) => {
                                    return cli_error(
                                        correlation_id,
                                        &format!("failed to serialize: {e}"),
                                    );
                                }
                            };
                            line.push('\n');
                            if let Err(e) = writer.write_all(line.as_bytes()).await {
                                // Wire write failed after we persisted — mark
                                // Stale so the slot doesn't stay at capacity.
                                let _ = self
                                    .app_state
                                    .transition_task(payload.task_id, TaskState::Stale);
                                return cli_error(
                                    correlation_id,
                                    &format!("failed to send to agent: {e}"),
                                );
                            }
                            let _ = writer.flush().await;
                        }

                        // Register the accept-handshake watchdog BEFORE
                        // releasing any external attention. If the worker
                        // replies faster than this handler finishes the
                        // broadcast + response (unlikely but possible on
                        // a loopback socket), MSG_TASK_ACCEPT's remove()
                        // on an absent entry is a harmless no-op and
                        // `transition_task(Accepted)` still runs. So
                        // late-registration is correctness-neutral; the
                        // worst case is a watchdog that fires a revert
                        // on an already-accepted task, which
                        // `revert_assignment_on_accept_timeout` detects
                        // and rejects (state no longer Assigned) as a
                        // debug-level race.
                        self.register_and_spawn_accept_watchdog(
                            payload.task_id,
                            payload.agent_id.clone(),
                        )
                        .await;

                        self.broadcast_event(
                            "task.assigned",
                            json!({
                                "task_id": payload.task_id.to_string(),
                                "agent_id": payload.agent_id,
                            }),
                        )
                        .await;

                        match Envelope::new(
                            MSG_CLI_RESPONSE,
                            json!({
                                "assigned": payload.task_id.to_string(),
                                "agent_id": payload.agent_id,
                            }),
                        ) {
                            Ok(mut e) => {
                                e.correlation_id = correlation_id;
                                e
                            }
                            Err(_) => cli_error(correlation_id, "failed to build response"),
                        }
                    }
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_SPAWN => {
                match env.decode_payload::<CliSpawn>() {
                    Ok(payload) => self.handle_spawn(correlation_id, payload).await,
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_DELETE => {
                match env.decode_payload::<CliDelete>() {
                    Ok(payload) => {
                        // Refuse to delete a template or fixed-slot yaml id —
                        // only template-spawned instances are tombstone-safe.
                        let is_yaml_slot = self
                            .agent_configs
                            .iter()
                            .any(|(id, _)| id == &payload.instance);
                        if is_yaml_slot {
                            return cli_error(
                                correlation_id,
                                &format!(
                                    "refusing to delete '{}': core yaml slot. Kill it instead.",
                                    payload.instance
                                ),
                            );
                        }

                        // Make sure any running process/session is gone first —
                        // delete implies kill, so don't leave an orphan.
                        {
                            let mut spawned = self.spawned.lock().await;
                            if let Some(mut child) = spawned.remove(&payload.instance) {
                                let _ = child.kill();
                            }
                        }
                        // `=` prefix = tmux exact-match, so `alor-claude` can't
                        // accidentally kill `alor-claude-alor`.
                        let tmux_session = format!("=alor-{}", payload.instance);
                        let _ = std::process::Command::new("tmux")
                            .args(["kill-session", "-t", &tmux_session])
                            .output();

                        let removed = self.app_state.remove_agent(&payload.instance);
                        if !removed {
                            return cli_error(
                                correlation_id,
                                &format!("agent '{}' not found in state", payload.instance),
                            );
                        }

                        // Clear any worker-task spawn tracking so
                        // transition_task doesn't warn about this
                        // instance — it was explicitly released.
                        self.app_state.clear_task_spawn(&payload.instance);

                        info!(instance = %payload.instance, "agent tombstoned via CLI");
                        self.broadcast_event(
                            "agent.deleted",
                            json!({"instance": &payload.instance}),
                        )
                        .await;

                        match Envelope::new(
                            MSG_CLI_RESPONSE,
                            json!({"deleted": payload.instance}),
                        ) {
                            Ok(mut e) => {
                                e.correlation_id = correlation_id;
                                e
                            }
                            Err(_) => cli_error(correlation_id, "failed to build response"),
                        }
                    }
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_KILL => {
                match env.decode_payload::<CliKill>() {
                    Ok(payload) => {
                        // Try to kill the child process
                        let killed_child = {
                            let mut spawned = self.spawned.lock().await;
                            if let Some(mut child) = spawned.remove(&payload.instance) {
                                // Send SIGTERM
                                let _ = child.kill();
                                true
                            } else {
                                false
                            }
                        };

                        // Also kill the tmux session (exact-match target).
                        let tmux_session = format!("=alor-{}", payload.instance);
                        let _ = std::process::Command::new("tmux")
                            .args(["kill-session", "-t", &tmux_session])
                            .output();

                        // Also clear any worker-task spawn tracking —
                        // cli.kill is the worker-facing release path
                        // (agent_kill tool → cli.kill). Without this
                        // the completion-time warning would fire for
                        // instances the worker explicitly killed.
                        self.app_state.clear_task_spawn(&payload.instance);

                        // Flip state.connected → false and broadcast
                        // the disconnect. Previously only the tmux
                        // session + optional child were cleaned up,
                        // leaving `state.connected: true` stuck for
                        // agents with no tracked daemon-spawned child
                        // (e.g. wrappers that self-registered or were
                        // launched outside the daemon). Symptom was
                        // identical to a reconcile-detected zombie,
                        // except caused by the kill itself rather than
                        // organic pane death. Idempotent with the
                        // natural socket-close cleanup: if the
                        // wrapper's read loop ALSO fires disconnect
                        // (via socket drop after child.kill() or after
                        // tmux teardown cascades), the flip is a no-op
                        // on the state flag and the duplicate
                        // `agent.disconnected` broadcast is benign.
                        self.mark_agent_killed(&payload.instance).await;

                        let msg = if killed_child {
                            format!("killed process and tmux session for {}", payload.instance)
                        } else {
                            format!("killed tmux session for {} (no tracked child)", payload.instance)
                        };

                        info!(instance = %payload.instance, "{msg}");

                        match Envelope::new(MSG_CLI_RESPONSE, json!({"killed": payload.instance, "message": msg})) {
                            Ok(mut e) => {
                                e.correlation_id = correlation_id;
                                e
                            }
                            Err(_) => cli_error(correlation_id, "failed to build response"),
                        }
                    }
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_PROJECT_LIST => {
                match project::list_profiles() {
                    Ok(profiles) => {
                        let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
                        match Envelope::new(MSG_CLI_RESPONSE, json!({"projects": names})) {
                            Ok(mut e) => {
                                e.correlation_id = correlation_id;
                                e
                            }
                            Err(_) => cli_error(correlation_id, "failed to build response"),
                        }
                    }
                    Err(e) => cli_error(correlation_id, &format!("failed to list projects: {e}")),
                }
            }

            MSG_CLI_PROJECT_GET => {
                match env.decode_payload::<CliProjectGet>() {
                    Ok(payload) => match project::load_profile(&payload.name) {
                        Ok(Some(profile)) => {
                            match Envelope::new(MSG_CLI_RESPONSE, json!({"project": profile})) {
                                Ok(mut e) => {
                                    e.correlation_id = correlation_id;
                                    e
                                }
                                Err(_) => cli_error(correlation_id, "failed to serialize project"),
                            }
                        }
                        Ok(None) => cli_error(
                            correlation_id,
                            &format!("project '{}' not found", payload.name),
                        ),
                        Err(e) => cli_error(
                            correlation_id,
                            &format!("failed to load project: {e}"),
                        ),
                    },
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_PROJECT_SAVE => {
                match env.decode_payload::<CliProjectSave>() {
                    Ok(payload) => {
                        // MERGE semantics — preserve fields the
                        // payload doesn't mention. Pre-fix
                        // (2026-04-20) the handler built a FRESH
                        // ProjectProfile::new(), which defaulted
                        // `memory_hub` and `notes` to None/empty;
                        // any round-trip through the UI silently
                        // erased those fields. See
                        // `project::merge_profile` for the full
                        // merge-rule contract.
                        let existing = match project::load_profile(&payload.name) {
                            Ok(opt) => opt,
                            Err(e) => {
                                return cli_error(
                                    correlation_id,
                                    &format!(
                                        "failed to load existing project for merge: {e}"
                                    ),
                                );
                            }
                        };
                        let profile = project::merge_profile(
                            existing,
                            &payload.name,
                            payload.description.clone(),
                            payload.root_dir.clone(),
                            payload.stack.clone(),
                            payload.key_files.clone(),
                            payload.doc_paths.clone(),
                            payload.memory_index.clone(),
                        );

                        if let Err(e) = project::save_profile(&profile) {
                            return cli_error(
                                correlation_id,
                                &format!("failed to save project: {e}"),
                            );
                        }

                        let mut hub_path_str: Option<String> = None;
                        if let Some(mi) = payload.memory_index.as_deref() {
                            let agent = payload.memory_agent.as_deref().unwrap_or("claude");
                            match crate::daemon::memory::link_agent_memory(
                                &payload.name,
                                agent,
                                std::path::PathBuf::from(mi),
                            ) {
                                Ok(hub_path) => {
                                    info!(
                                        project = %payload.name,
                                        agent = %agent,
                                        hub_path = %hub_path.display(),
                                        "agent memory linked into hub"
                                    );
                                    hub_path_str = Some(hub_path.display().to_string());
                                }
                                Err(e) => {
                                    return cli_error(
                                        correlation_id,
                                        &format!("failed to link agent memory: {e}"),
                                    );
                                }
                            }
                        }

                        match Envelope::new(
                            MSG_CLI_RESPONSE,
                            json!({
                                "saved": payload.name,
                                "hub_path": hub_path_str,
                            }),
                        ) {
                            Ok(mut e) => {
                                e.correlation_id = correlation_id;
                                e
                            }
                            Err(_) => cli_error(correlation_id, "failed to build response"),
                        }
                    }
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_AGENT_ENSURE_RUNNING => {
                match env.decode_payload::<CliAgentEnsureRunning>() {
                    Ok(payload) => {
                        if payload.agent_id.is_empty()
                            || payload.agent_id.len() > 64
                            || !payload
                                .agent_id
                                .chars()
                                .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
                        {
                            return cli_error(
                                correlation_id,
                                &format!("invalid agent_id: {:?}", payload.agent_id),
                            );
                        }

                        // Already connected? No-op.
                        if self.writers.lock().await.contains_key(&payload.agent_id) {
                            match Envelope::new(
                                MSG_CLI_RESPONSE,
                                json!({
                                    "agent_id": &payload.agent_id,
                                    "spawned": false,
                                    "reason": "already connected",
                                }),
                            ) {
                                Ok(mut e) => {
                                    e.correlation_id = correlation_id;
                                    return e;
                                }
                                Err(_) => {
                                    return cli_error(correlation_id, "failed to build response")
                                }
                            }
                        }

                        // Idempotent spawn: first try yaml-declared slot, then
                        // a state-persisted instance that was spawned from a
                        // template. Everything else is an error.
                        let has_config = self
                            .agent_configs
                            .iter()
                            .any(|(id, _)| id == &payload.agent_id);

                        let spawn_payload = if has_config {
                            CliSpawn {
                                name: Some(payload.agent_id.clone()),
                                agent: payload.agent_id.clone(),
                                role: None,
                                project: None,
                                working_dir: None,
                                spawned_by_task: None,
                            }
                        } else if let Some(existing) = self.app_state.get_agent(&payload.agent_id) {
                            match existing.template.as_deref() {
                                Some(tmpl)
                                    if self
                                        .agent_configs
                                        .iter()
                                        .any(|(id, _)| id == tmpl) =>
                                {
                                    CliSpawn {
                                        name: Some(payload.agent_id.clone()),
                                        agent: tmpl.to_string(),
                                        role: None,
                                        project: existing.project.clone(),
                                        working_dir: existing.working_dir.clone(),
                                        spawned_by_task: None,
                                    }
                                }
                                _ => {
                                    return cli_error(
                                        correlation_id,
                                        &format!(
                                            "agent '{}' has no yaml config and no usable template",
                                            payload.agent_id
                                        ),
                                    )
                                }
                            }
                        } else {
                            return cli_error(
                                correlation_id,
                                &format!(
                                    "no yaml config for agent '{}'; use agent_spawn with an explicit base instead",
                                    payload.agent_id
                                ),
                            );
                        };
                        let resp = self.handle_spawn(correlation_id, spawn_payload).await;
                        // Re-label the success envelope to expose `spawned: true`.
                        if resp.kind == MSG_CLI_RESPONSE {
                            match Envelope::new(
                                MSG_CLI_RESPONSE,
                                json!({
                                    "agent_id": &payload.agent_id,
                                    "spawned": true,
                                }),
                            ) {
                                Ok(mut e) => {
                                    e.correlation_id = correlation_id;
                                    return e;
                                }
                                Err(_) => {
                                    return cli_error(correlation_id, "failed to build response")
                                }
                            }
                        }
                        resp
                    }
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_AGENT_SEND_MESSAGE => {
                match env.decode_payload::<CliAgentSendMessage>() {
                    Ok(payload) => {
                        if payload.agent_id.is_empty()
                            || payload.agent_id.len() > 64
                            || !payload
                                .agent_id
                                .chars()
                                .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
                        {
                            return cli_error(
                                correlation_id,
                                &format!("invalid agent_id: {:?}", payload.agent_id),
                            );
                        }
                        // Exact-match target — `alor-claude` must not fall
                        // through to `alor-claude-alor` and deliver to the
                        // wrong worker.  tmux's target syntax splits: bare
                        // `=name` resolves a session (what has-session
                        // wants), but send-keys takes a target-pane so we
                        // need `=name:` to pick the active pane of the
                        // exact session.  Before fixing this split,
                        // send-keys silently failed with "can't find pane"
                        // and the daemon reported success to the orch
                        // anyway — every agent_send_message was a no-op.
                        let session_target = format!("=alor-{}", payload.agent_id);
                        let pane_target = format!("{session_target}:");
                        let session_exists = tokio::process::Command::new("tmux")
                            .args(["has-session", "-t", &session_target])
                            .output()
                            .await
                            .map(|o| o.status.success())
                            .unwrap_or(false);
                        if !session_exists {
                            return cli_error(
                                correlation_id,
                                &format!("no tmux session for agent {}", payload.agent_id),
                            );
                        }
                        // Runtime-type gate. BEGIN/END framing is only
                        // safe for `claude-sdk` workers whose stdin loop
                        // implements the sentinel state machine
                        // (orchestrator-py/worker.py). For any other
                        // runtime (wrapper, future codex/gemini) the
                        // sentinel bytes would land verbatim in the
                        // CLI's pty — visible garbage to the user plus
                        // a wedged send from the orch's POV (no
                        // `worker.orch_response` will ever echo back).
                        //
                        // Fail-closed: unknown/unresolvable agents are
                        // rejected rather than silently downgraded, so
                        // a caller passing a typo'd agent_id gets a
                        // clear error instead of a framed send to an
                        // agent we can't prove is SDK-backed.
                        //
                        // Callers that genuinely want to reach a
                        // wrapper-runtime worker must set
                        // `suppress_echo: false` explicitly.
                        if payload.suppress_echo
                            && !is_framed_send_allowed(
                                &self.agent_configs,
                                &self.app_state,
                                &payload.agent_id,
                            )
                        {
                            return cli_error_coded(
                                correlation_id,
                                ERR_CODE_FRAMED_SEND_NOT_SUPPORTED,
                                &format!(
                                    "framed send (suppress_echo=true) not supported for agent '{}': \
                                     only claude-sdk runtime workers implement BEGIN/END framing. \
                                     Retry with suppress_echo=false to inject raw text.",
                                    payload.agent_id
                                ),
                            );
                        }
                        // If the caller asked to suppress the stdin echo
                        // (orch → SDK-worker path), wrap the payload in
                        // BEGIN/END framing so the worker's stdin state
                        // machine can (a) skip emitting `worker.user_input`
                        // across the whole frame, (b) accumulate all body
                        // lines before dispatching a single SDK turn, and
                        // (c) echo the same correlation_id back in the
                        // `worker.orch_response` event after the turn
                        // completes.
                        //
                        // On-wire format (fed to `tmux send-keys -l` — tmux
                        // converts each literal `\n` into an Enter
                        // keystroke, so BEGIN / each body line / END land
                        // as separate read_line calls in the worker):
                        //
                        //   {BEGIN}{uuid}\n
                        //   <multi-line body>\n
                        //   {END}{uuid}\n
                        //
                        // The trailing `\n` after END is mandatory — without
                        // it the END marker sits in prompt_toolkit's buffer
                        // unsubmitted and the worker never closes the frame.
                        // For that reason the caller's `submit` flag is
                        // ignored on the suppress_echo path; the framing
                        // submits every marker for us.
                        let send_correlation_id: Option<Uuid> = if payload.suppress_echo {
                            Some(Uuid::new_v4())
                        } else {
                            None
                        };
                        let effective_text = match send_correlation_id {
                            Some(cid) => format!(
                                "{}{}\n{}\n{}{}\n",
                                WORKER_ECHO_SENTINEL_BEGIN,
                                cid,
                                payload.text,
                                WORKER_ECHO_SENTINEL_END,
                                cid,
                            ),
                            None => payload.text.clone(),
                        };
                        let send_out = tokio::process::Command::new("tmux")
                            .args(["send-keys", "-t", &pane_target, "-l", &effective_text])
                            .output()
                            .await;
                        let send_ok = match &send_out {
                            Ok(o) if o.status.success() => true,
                            Ok(o) => {
                                return cli_error(
                                    correlation_id,
                                    &format!(
                                        "tmux send-keys rejected target {pane_target}: {}",
                                        String::from_utf8_lossy(&o.stderr).trim()
                                    ),
                                );
                            }
                            Err(e) => {
                                return cli_error(
                                    correlation_id,
                                    &format!("tmux send-keys failed: {e}"),
                                );
                            }
                        };
                        let _ = send_ok; // consume
                        // Only the non-framed (Fett-typed style) path honors
                        // the explicit submit flag — framed sends already
                        // carry their own Enter via the trailing \n above.
                        if payload.submit && send_correlation_id.is_none() {
                            let _ = tokio::process::Command::new("tmux")
                                .args(["send-keys", "-t", &pane_target, "Enter"])
                                .output()
                                .await;
                        }
                        info!(
                            agent_id = %payload.agent_id,
                            submit = payload.submit,
                            bytes = payload.text.len(),
                            suppress_echo = payload.suppress_echo,
                            send_correlation_id = ?send_correlation_id,
                            "cli.agent.send_message"
                        );
                        let response_payload = json!({
                            "sent": payload.agent_id,
                            "submit": payload.submit,
                            // Only present when suppress_echo=true — callers
                            // use this to match the subsequent
                            // `worker.orch_response` event back to this send.
                            "correlation_id": send_correlation_id.map(|c| c.to_string()),
                        });
                        match Envelope::new(MSG_CLI_RESPONSE, response_payload) {
                            Ok(mut e) => {
                                e.correlation_id = correlation_id;
                                e
                            }
                            Err(_) => cli_error(correlation_id, "failed to build response"),
                        }
                    }
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_MEMORY_GET => {
                // Audit 8b03cae6 bloat fix #5 (final item). Old
                // behavior: return every file in the hub on every
                // call. New: optional `file_names` filter pulls only
                // the named basenames. Unknown names surface under
                // `missing` so the caller can tell "file absent" from
                // "file present but empty". Path-traversal attempts
                // (`..`, `/`, `\`) are rejected as `missing` rather
                // than silently resolved — names must be leaf file
                // basenames, no hierarchical paths.
                //
                // Also: warn-log when the response exceeds
                // MEMORY_GET_WARN_BYTES (10 KiB) so we can see in
                // dev whether the orch keeps tripping the threshold
                // without using the filter (signal the docstring
                // needs more work).
                match env.decode_payload::<CliMemoryGet>() {
                    Ok(payload) => {
                        let hub_dir = match project::memory_hub_dir(&payload.project) {
                            Ok(d) => d,
                            Err(e) => {
                                return cli_error(
                                    correlation_id,
                                    &format!("invalid project: {e}"),
                                )
                            }
                        };
                        if !hub_dir.exists() {
                            match Envelope::new(
                                MSG_CLI_RESPONSE,
                                json!({"project": payload.project, "files": {}}),
                            ) {
                                Ok(mut e) => {
                                    e.correlation_id = correlation_id;
                                    return e;
                                }
                                Err(_) => {
                                    return cli_error(
                                        correlation_id,
                                        "failed to build response",
                                    )
                                }
                            }
                        }

                        // Partition the caller's requested names (if
                        // any) into `safe_names` (leaf basenames we'll
                        // actually look up) and `missing_from_filter`
                        // (traversal attempts + anything that isn't a
                        // plain filename). Traversal rejections join
                        // the `missing` list in the response so the
                        // caller gets a uniform shape — "I asked for
                        // X; X wasn't there" — rather than a security-
                        // flavored error that would require a separate
                        // code path to handle.
                        //
                        // Treat a present-but-empty filter the same as
                        // None: "whole hub" is the explicit escape
                        // valve for callers that don't know file
                        // names yet; forcing them to pass `None` vs
                        // `[]` separately is unnecessary friction.
                        let filter_active = payload
                            .file_names
                            .as_ref()
                            .map(|v| !v.is_empty())
                            .unwrap_or(false);
                        let (safe_names, mut missing_from_filter): (Vec<String>, Vec<String>) =
                            if filter_active {
                                let requested = payload.file_names.clone().unwrap_or_default();
                                let mut ok = Vec::new();
                                let mut bad = Vec::new();
                                for name in requested {
                                    if is_safe_hub_basename(&name) {
                                        ok.push(name);
                                    } else {
                                        // Path separator, `..`, empty,
                                        // or anything that isn't a leaf
                                        // basename. Quietly treat as
                                        // "missing" — the caller sees
                                        // the same shape as a genuine
                                        // non-existent file and can
                                        // retry with a corrected name.
                                        // Don't log the raw input at a
                                        // visible level; a traversal
                                        // attempt isn't necessarily
                                        // malicious (LLM typo of `../`)
                                        // but `debug!` captures it for
                                        // forensics without flooding.
                                        debug!(
                                            project = %payload.project,
                                            requested_name = %name,
                                            "memory_get: rejected non-basename from file_names filter (treated as missing)"
                                        );
                                        bad.push(name);
                                    }
                                }
                                (ok, bad)
                            } else {
                                (Vec::new(), Vec::new())
                            };

                        let entries = match std::fs::read_dir(&hub_dir) {
                            Ok(it) => it,
                            Err(e) => {
                                return cli_error(
                                    correlation_id,
                                    &format!("read hub dir: {e}"),
                                )
                            }
                        };
                        let mut files = serde_json::Map::new();
                        // Sink of names we've actually seen on disk
                        // (post-filter), used to compute the
                        // `missing` set for present-but-filtered
                        // callers. Collected from the read_dir walk
                        // rather than a pre-walk stat so a partial
                        // read (permission denied on one entry) still
                        // succeeds for the others.
                        let mut seen_on_disk: std::collections::HashSet<String> =
                            std::collections::HashSet::new();
                        for entry in entries.flatten() {
                            let path = entry.path();
                            if !path.is_file() {
                                continue;
                            }
                            let name = match path.file_name().and_then(|s| s.to_str()) {
                                Some(n) => n.to_string(),
                                None => continue,
                            };
                            seen_on_disk.insert(name.clone());
                            // If a filter is active, drop on-disk
                            // files the caller didn't ask for. This
                            // is the whole bloat fix.
                            if filter_active && !safe_names.iter().any(|n| n == &name) {
                                continue;
                            }
                            match std::fs::metadata(&path) {
                                Ok(meta) if meta.len() > 1_048_576 => {
                                    files.insert(
                                        name,
                                        json!({
                                            "truncated": true,
                                            "size": meta.len(),
                                        }),
                                    );
                                    continue;
                                }
                                _ => {}
                            }
                            match std::fs::read_to_string(&path) {
                                Ok(content) => {
                                    files.insert(name, json!(content));
                                }
                                Err(e) => {
                                    files.insert(name, json!({"error": e.to_string()}));
                                }
                            }
                        }
                        // Build the `missing` list: safe_names the
                        // caller asked for that didn't turn up on
                        // disk, plus any that tripped the traversal
                        // guard. Returned even when empty (stable
                        // shape for callers who want to branch on
                        // `missing.is_empty()`).
                        if filter_active {
                            for requested in &safe_names {
                                if !seen_on_disk.contains(requested) {
                                    missing_from_filter.push(requested.clone());
                                }
                            }
                        }

                        // Telemetry: approximate response size. Sum
                        // of file contents (the heavy part) +
                        // negligible key / JSON overhead. Log-only
                        // warning — not a cap — so the escape valve
                        // stays intact.
                        let total_bytes: usize = files
                            .values()
                            .map(|v| v.as_str().map(|s| s.len()).unwrap_or(0))
                            .sum();
                        if total_bytes > MEMORY_GET_WARN_BYTES {
                            warn!(
                                project = %payload.project,
                                total_bytes,
                                file_count = files.len(),
                                filter_active,
                                threshold = MEMORY_GET_WARN_BYTES,
                                "memory_get: response exceeds warn threshold (audit 8b03cae6 fix #5; if filter_active=false, caller should switch to the file_names filter)"
                            );
                        }

                        let mut resp = json!({
                            "project": payload.project,
                            "files": files,
                        });
                        // Only emit `missing` when a filter was
                        // actually applied — on whole-hub reads the
                        // concept doesn't apply (everything found is
                        // everything that exists), and adding a
                        // permanent empty array would churn the
                        // wire shape for existing callers.
                        if filter_active {
                            let obj = resp.as_object_mut().expect("resp is JSON object");
                            obj.insert("missing".to_string(), json!(missing_from_filter));
                        }

                        match Envelope::new(MSG_CLI_RESPONSE, resp) {
                            Ok(mut e) => {
                                e.correlation_id = correlation_id;
                                e
                            }
                            Err(_) => cli_error(correlation_id, "failed to build response"),
                        }
                    }
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_MEMORY_APPEND => {
                // Curated-write counterpart to the task.complete auto-
                // distillation. The orchestrator (or any caller via
                // alor-cli) nominates a text block for the project's
                // Memory Hub; we append it to the named file after the
                // same basename guard `memory.get` uses (no path
                // traversal, no hierarchical names), a payload-size
                // cap, and the same head-trim retention policy the
                // auto-distiller applies to `automation_log.md`. See
                // `memory::append_to_hub_file` for the write semantics
                // and `memory::MEMORY_APPEND_MAX_BYTES` /
                // `AUTOMATION_LOG_MAX_BYTES` for the caps.
                match env.decode_payload::<CliMemoryAppend>() {
                    Ok(payload) => {
                        if !is_safe_hub_basename(&payload.file_name) {
                            return cli_error(
                                correlation_id,
                                &format!(
                                    "unsafe file_name {:?} (must be a plain basename; names with `..`, `/`, `\\`, or NUL are rejected)",
                                    payload.file_name
                                ),
                            );
                        }
                        if payload.text.len() > memory::MEMORY_APPEND_MAX_BYTES {
                            return cli_error(
                                correlation_id,
                                &format!(
                                    "text exceeds MEMORY_APPEND_MAX_BYTES cap ({} B; got {} B) — split into multiple appends or use a dedicated hub file",
                                    memory::MEMORY_APPEND_MAX_BYTES,
                                    payload.text.len()
                                ),
                            );
                        }
                        // Same retention cap as the auto-distiller so a
                        // caller can't grow `automation_log.md` past the
                        // bound by hand; other hub files trim to a
                        // generous 1 MiB so curated notes (usually
                        // static reference material) aren't clobbered
                        // by a handful of appends.
                        let retention_cap = if payload.file_name == memory::AUTOMATION_LOG_BASENAME
                        {
                            Some(memory::AUTOMATION_LOG_MAX_BYTES)
                        } else {
                            Some(1024 * 1024)
                        };
                        match memory::append_to_hub_file(
                            &payload.project,
                            &payload.file_name,
                            &payload.text,
                            retention_cap,
                        ) {
                            Ok(path) => {
                                let path_display = path.display().to_string();
                                info!(
                                    project = %payload.project,
                                    file = %payload.file_name,
                                    bytes = payload.text.len(),
                                    "memory.append: wrote to hub file"
                                );
                                match Envelope::new(
                                    MSG_CLI_RESPONSE,
                                    json!({
                                        "project": payload.project,
                                        "file_name": payload.file_name,
                                        "path": path_display,
                                        "bytes_written": payload.text.len(),
                                    }),
                                ) {
                                    Ok(mut e) => {
                                        e.correlation_id = correlation_id;
                                        e
                                    }
                                    Err(_) => cli_error(
                                        correlation_id,
                                        "failed to serialize memory.append response",
                                    ),
                                }
                            }
                            Err(e) => cli_error(
                                correlation_id,
                                &format!("memory.append failed: {e:#}"),
                            ),
                        }
                    }
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_WORKER_RESPONSE_GET => {
                // Audit 8b03cae6 bloat fix #4: fetch-on-demand
                // counterpart to the orchestrator's injected-text cap.
                // The orch saw a truncated `worker.orch_response`
                // event that carried a
                // `worker_response_get(correlation_id=X)` pointer; now
                // it's asking for the full body. Returns the cached
                // `WorkerResponseRecord` or a clear error when the id
                // is unknown / has been evicted from the LRU.
                match env.decode_payload::<CliWorkerResponseGet>() {
                    Ok(payload) => match self.app_state.get_worker_response(payload.correlation_id) {
                        Some(record) => match Envelope::new(
                            MSG_CLI_RESPONSE,
                            json!({
                                "correlation_id": record.correlation_id.to_string(),
                                "agent_id": record.agent_id,
                                "text": record.text,
                                "task_id": record.task_id.map(|t| t.to_string()),
                                "during_task": record.during_task,
                                "timestamp": record.timestamp.to_rfc3339(),
                            }),
                        ) {
                            Ok(mut e) => {
                                e.correlation_id = correlation_id;
                                e
                            }
                            Err(_) => cli_error(correlation_id, "failed to serialize worker response"),
                        },
                        None => cli_error(
                            correlation_id,
                            &format!(
                                "worker response {} not found (unknown correlation_id or evicted from the {}-entry LRU)",
                                payload.correlation_id,
                                crate::daemon::state::WORKER_RESPONSE_CACHE_CAP,
                            ),
                        ),
                    },
                    Err(e) => cli_error(correlation_id, &format!("invalid payload: {e}")),
                }
            }

            MSG_CLI_INTEGRATIONS_GET => {
                let config_dir = match crate::daemon::session::config_dir() {
                    Ok(d) => d,
                    Err(e) => return cli_error(correlation_id, &format!("config dir error: {e}")),
                };
                let path = config_dir.join("integrations.yaml");

                let integrations = if path.exists() {
                    match std::fs::read_to_string(&path) {
                        Ok(content) => match serde_yaml::from_str::<serde_json::Value>(&content) {
                            Ok(v) => v,
                            Err(e) => json!({"error": format!("invalid yaml: {e}")}),
                        },
                        Err(e) => json!({"error": format!("read error: {e}")}),
                    }
                } else {
                    json!({"sources": []})
                };

                match Envelope::new(MSG_CLI_RESPONSE, integrations) {
                    Ok(mut e) => {
                        e.correlation_id = correlation_id;
                        e
                    }
                    Err(_) => cli_error(correlation_id, "failed to build response"),
                }
            }

            other => cli_error(correlation_id, &format!("unknown CLI command: {other}")),
        }
    }

    /// Register a pending-accept entry for `task_id` and spawn a
    /// background watchdog that fires `ACCEPT_ACK_TIMEOUT_SECS`
    /// later. The watchdog:
    ///
    ///   - Exits silently if the MSG_TASK_ACCEPT handler removes
    ///     the entry from `pending_accepts` (dropping the oneshot
    ///     sender, which wakes the receiver's select arm).
    ///   - Otherwise calls
    ///     `AppState::revert_assignment_on_accept_timeout` and
    ///     emits the corresponding `task.accept.timeout` +
    ///     (`task.reassign` | `task.accept_failed`) events.
    ///
    /// Broken out of the `cli.assign` arm body so the logic is
    /// testable via `SocketServer` inherent impl without going
    /// through the envelope-routing path.
    /// Promoted from `pub(super)` to `pub` so the Tauri-facing
    /// assign path in `commands.rs::assign_task` can wire up the
    /// same accept-handshake watchdog the `cli.assign` arm uses.
    /// Previously only the CLI path got timeout-revert coverage;
    /// frontend-originated assigns went unprotected.
    pub async fn register_and_spawn_accept_watchdog(
        &self,
        task_id: Uuid,
        agent_id: String,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        {
            let mut pending = self.pending_accepts.lock().await;
            // Overwrite any prior entry for this task_id. A duplicate
            // assign (shouldn't happen, but cli.assign could race
            // itself if two orchs hit the same task) replaces the
            // older sender — dropping it signals the older watchdog
            // to exit silently, which is what we want: there's only
            // ever one valid in-flight handshake per task.
            if let Some(old_tx) = pending.insert(task_id, tx) {
                drop(old_tx);
                tracing::debug!(
                    task_id = %task_id,
                    "replaced prior accept-watchdog registration"
                );
            }
        }

        let server = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(
                    ACCEPT_ACK_TIMEOUT_SECS,
                )) => {
                    // Timeout: MSG_TASK_ACCEPT didn't arrive in time.
                    server.handle_accept_timeout(task_id, &agent_id).await;
                }
                _ = rx => {
                    // Either the sender sent Ok (not currently used —
                    // we signal by drop) or the sender was dropped
                    // (by MSG_TASK_ACCEPT's `remove()`). Either way
                    // the handshake is resolved; exit silently.
                    tracing::trace!(
                        task_id = %task_id,
                        "accept-watchdog cancelled by ack"
                    );
                }
            }
        });
    }

    /// Handle the accept-watchdog timeout path: revert the task's
    /// assignment and emit the appropriate event cascade. Called
    /// from the watchdog body in
    /// `register_and_spawn_accept_watchdog`; factored out so tests
    /// can drive the timeout path synchronously without waiting on
    /// the 5s sleep.
    pub(super) async fn handle_accept_timeout(
        &self,
        task_id: Uuid,
        agent_id: &str,
    ) {
        // Clean up our own pending-accept entry whether or not the
        // revert succeeds — we're the authority on this handshake
        // ending, one way or another.
        {
            let mut pending = self.pending_accepts.lock().await;
            pending.remove(&task_id);
        }

        let outcome = match self
            .app_state
            .revert_assignment_on_accept_timeout(task_id, MAX_ACCEPT_ATTEMPTS)
        {
            Ok(o) => o,
            Err(e) => {
                // Most common cause: race with the ack that arrived
                // between timeout-fire and revert-acquire. Log at
                // debug, skip events, don't escalate.
                tracing::debug!(
                    task_id = %task_id,
                    agent_id,
                    "accept-watchdog timeout superseded: {e}"
                );
                return;
            }
        };

        warn!(
            task_id = %task_id,
            agent_id,
            attempts = outcome.attempts,
            exhausted = outcome.exhausted,
            new_state = ?outcome.new_state,
            "task.accept ack timeout; task reverted"
        );

        // Always emit the telemetry event — lets operators + the
        // orch observe the timeout even when the task goes straight
        // to AcceptFailed.
        self.broadcast_event(
            "task.accept.timeout",
            json!({
                "task_id": task_id.to_string(),
                "agent_id": agent_id,
                "attempts": outcome.attempts,
                "max_attempts": MAX_ACCEPT_ATTEMPTS,
                "prior_agent": outcome.prior_agent,
            }),
        )
        .await;

        // Terminal-event fan-out: either "please try again" or
        // "give up".
        if outcome.exhausted {
            self.broadcast_event(
                "task.accept_failed",
                json!({
                    "task_id": task_id.to_string(),
                    "agent_id": agent_id,
                    "attempts": outcome.attempts,
                    "reason": "accept_timeout",
                }),
            )
            .await;
        } else {
            self.broadcast_event(
                "task.reassign",
                json!({
                    "task_id": task_id.to_string(),
                    "prior_agent": outcome.prior_agent,
                    "attempts": outcome.attempts,
                    "max_attempts": MAX_ACCEPT_ATTEMPTS,
                    "reason": "accept_timeout",
                }),
            )
            .await;
        }
    }
}

/// Reject names that would escape (or even address anything outside)
/// the memory hub directory. Used by the `cli.memory.get` handler
/// when a `file_names` filter is passed — see the `CliMemoryGet`
/// docstring for the contract.
///
/// Accept only plain leaf basenames. Reject empty strings, `.`,
/// `..`, anything containing `/` or `\` (both matter — Windows
/// portability and raw literals that could still trip `Path::join`
/// in edge cases), and any name that parses via `Path` as
/// hierarchical (multiple components).
///
/// Deliberately stricter than strictly necessary for a Linux
/// filesystem: better to treat "odd but legal" names as missing
/// than to open up subtle traversal paths. Callers that need exotic
/// filenames can rename their hub entries.
pub(super) fn is_safe_hub_basename(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." {
        return false;
    }
    if name.contains('/') || name.contains('\\') {
        return false;
    }
    // Null bytes would also be weird but `contains('\\0')` on a
    // regular &str is unusual — Rust strings can contain NULs. Guard
    // belt-and-braces.
    if name.contains('\0') {
        return false;
    }
    // Final check: Path should see a single-component, Normal entry.
    let p = std::path::Path::new(name);
    let mut comps = p.components();
    match (comps.next(), comps.next()) {
        (Some(std::path::Component::Normal(_)), None) => true,
        _ => false,
    }
}
