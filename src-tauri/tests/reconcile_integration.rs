//! Integration tests for the pane-reconciliation safety net.
//!
//! These tests exercise `PaneManager::reconcile_panes` + the
//! `SocketServer::mark_agent_zombie` state-flip path end-to-end
//! against a real tmux server. They're called out as "integration"
//! (rather than unit) because they:
//!   - live in the crate's `tests/` target (external-compilation),
//!     only reaching `alor_lib`'s re-exported public API;
//!   - spawn real tmux sessions to validate the tmux-shelling
//!     behavior rather than mocking it;
//!   - exercise the full cycle an agent goes through in production
//!     (connect → lose pane / lose session → reconcile → recover
//!     or get auto-flipped to disconnected).
//!
//! Tmux-free CI boxes are fine: every test that shells out to tmux
//! gracefully skips with an eprintln! when `tmux -V` doesn't
//! succeed. No-tmux environments simply see "test result: ok. N
//! passed" with all the skipped ones still reported as passed.
//!
//! Unit tests that exercise `pub(crate)` helpers (`pane_exists`,
//! `test_insert_pane`, etc.) intentionally remain in-file next to
//! the code — those test private helper behaviour, not user-
//! visible flows, and can't reach private items from here.

use alor_lib::{AppState, PaneManager, SocketServer};
use tokio::process::Command;

// ---------------------------------------------------------------------------
// Test helpers (small + self-contained; duplicated from the in-file
// test module where appropriate because Rust integration tests can't
// share code with `#[cfg(test)] mod tests` without a dedicated
// public module).
// ---------------------------------------------------------------------------

/// True if `tmux -V` runs successfully on PATH. When false, tests
/// gracefully skip (eprintln! + early return) rather than failing on
/// runners without tmux installed.
async fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Kill a tmux session by name, exact-match target. Best-effort — we
/// don't care about the exit status here, only that the session is
/// gone if it existed.
async fn kill_session(session: &str) {
    let exact = format!("={session}");
    let _ = Command::new("tmux")
        .args(["kill-session", "-t", &exact])
        .output()
        .await;
}

// ---------------------------------------------------------------------------
// Reconcile: the missing-pane repro end-to-end
// ---------------------------------------------------------------------------

/// Full repro of the original bug (task 883bdc60 / 1ed52762):
/// `wrapper.register`'s `add_agent_pane` silently fails, leaving an
/// agent marked `connected: true` with no pane in alor-main.
/// `reconcile_panes` must self-heal by adding the missing pane.
///
/// Uses a disposable alor-main-like tmux session via
/// `PaneManager::with_main_session` so the running Alor's real
/// `alor-main` (which hosts the terminal that runs this test) stays
/// untouched.
#[tokio::test]
async fn reconcile_panes_adds_missing_pane_for_connected_agent_with_live_session() {
    if !tmux_available().await {
        eprintln!("tmux not available, skipping");
        return;
    }
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let main_session = format!("alor-test-reconcile-{nonce}");
    let agent_id = format!("test-agent-{nonce}");
    let agent_session = format!("alor-{agent_id}");

    // 1. Disposable main session (placeholder pane only).
    let pm = PaneManager::with_main_session(&main_session);
    pm.ensure_main_session()
        .await
        .expect("ensure main session");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        pm.session_exists(&main_session).await,
        "test main session must exist after ensure_main_session"
    );

    // 2. Agent's own tmux session — long-running sleep so it stays
    //    alive past the test tick.
    let ok = Command::new("tmux")
        .args([
            "new-session", "-d", "-s", &agent_session, "sleep", "300",
        ])
        .output()
        .await
        .ok()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        kill_session(&main_session).await;
        panic!("failed to create agent session {agent_session}");
    }

    // 3. Register the agent as connected in AppState.
    //    set_agent_connected auto-registers with tmux_session =
    //    Some("alor-<id>") which matches our naming convention.
    let state = AppState::new();
    state
        .set_agent_connected(&agent_id, true)
        .expect("set_agent_connected");
    let stored = state.get_agent(&agent_id).expect("agent present");
    assert!(stored.connected);
    assert_eq!(
        stored.tmux_session.as_deref(),
        Some(agent_session.as_str()),
        "auto-registered tmux_session should match alor-<id> naming"
    );

    // 4. Simulate the silent-failure repro: register ran but
    //    add_agent_pane was NOT called. The panes map is empty for
    //    this agent.
    assert!(
        pm.visible_agents().await.iter().all(|v| v != &agent_id),
        "pre-reconcile: agent must not yet be a visible pane"
    );

    // 5. Reconcile. Must add the missing pane and report it.
    let report = pm.reconcile_panes(&state).await;

    assert!(
        report.added.iter().any(|id| id == &agent_id),
        "reconcile should report `{agent_id}` as added; got {:?}",
        report.added
    );
    assert!(
        report.zombies.is_empty(),
        "agent session IS alive, so it must not be a zombie; got {:?}",
        report.zombies
    );
    assert!(
        pm.visible_agents().await.iter().any(|v| v == &agent_id),
        "post-reconcile: agent must now be a visible pane"
    );

    // 6. Cleanup.
    kill_session(&main_session).await;
    kill_session(&agent_session).await;
}

// ---------------------------------------------------------------------------
// Reconcile: zombie detection surface (doesn't mutate state)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reconcile_panes_reports_zombie_for_connected_agent_without_session() {
    if !tmux_available().await {
        eprintln!("tmux not available, skipping");
        return;
    }
    let state = AppState::new();
    let zombie_id = format!("pm-zombie-test-{}", uuid::Uuid::new_v4().simple());
    state
        .set_agent_connected(&zombie_id, true)
        .expect("set_agent_connected");
    assert!(state.get_agent(&zombie_id).unwrap().connected);

    let pm = PaneManager::new();
    let report = pm.reconcile_panes(&state).await;

    assert!(
        report.zombies.iter().any(|z| z == &zombie_id),
        "reconcile should report the zombie agent_id; got {:?}",
        report.zombies
    );
    // PaneManager does NOT mutate state — separation of concerns.
    assert!(
        state.get_agent(&zombie_id).unwrap().connected,
        "reconcile_panes must not flip state directly"
    );
}

#[tokio::test]
async fn reconcile_panes_omits_zombie_when_session_exists() {
    // Inverse: agent is connected AND its tmux session exists. Must
    // NOT appear in report.zombies.
    if !tmux_available().await {
        eprintln!("tmux not available, skipping");
        return;
    }
    let state = AppState::new();
    let alive_id = format!("pm-alive-test-{}", uuid::Uuid::new_v4().simple());
    let session = format!("alor-{alive_id}");
    let ok = Command::new("tmux")
        .args(["new-session", "-d", "-s", &session, "sleep", "300"])
        .output()
        .await
        .ok()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("failed to create test session, skipping");
        return;
    }

    state
        .set_agent_connected(&alive_id, true)
        .expect("set_agent_connected");

    let pm = PaneManager::new();
    let report = pm.reconcile_panes(&state).await;

    assert!(
        !report.zombies.iter().any(|z| z == &alive_id),
        "alive agent must not appear in zombies; got {:?}",
        report.zombies
    );

    kill_session(&session).await;
}

#[tokio::test]
async fn reconcile_panes_skips_disconnected_agents_entirely() {
    // A disconnected agent (connected=false) with no tmux session
    // must NOT show up as a zombie — zombies are the "connected but
    // gone" subset, not every missing-session row.
    let state = AppState::new();
    let disc_id = format!("pm-disc-test-{}", uuid::Uuid::new_v4().simple());
    state.set_agent_connected(&disc_id, true).unwrap();
    state.set_agent_connected(&disc_id, false).unwrap();
    assert!(!state.get_agent(&disc_id).unwrap().connected);

    let pm = PaneManager::new();
    let report = pm.reconcile_panes(&state).await;

    assert!(
        !report.zombies.iter().any(|z| z == &disc_id),
        "disconnected agents must not appear as zombies"
    );
}

// ---------------------------------------------------------------------------
// Zombie auto-clear: caller follow-up via SocketServer::mark_agent_zombie
// ---------------------------------------------------------------------------

/// End-to-end of the zombie auto-clear path. reconcile_panes reports
/// zombies, caller composes with SocketServer::mark_agent_zombie
/// which flips state.connected → false. This mirrors what lib.rs
/// startup + the `pane_reconcile` Tauri command do in production.
#[tokio::test]
async fn reconcile_panes_zombie_auto_clear_flips_state_via_mark_agent_zombie() {
    if !tmux_available().await {
        eprintln!("tmux not available, skipping");
        return;
    }

    // SocketServer owns AppState + a PaneManager internally. This
    // matches how lib.rs wires them (single shared AppState handed
    // to both), which is what the zombie flow needs.
    let state = AppState::new();
    let pm = PaneManager::new();
    let server = SocketServer::with_configs(state.clone(), pm.clone(), vec![]);

    let zombie_id = format!("test-zombie-{}", uuid::Uuid::new_v4().simple());
    state
        .set_agent_connected(&zombie_id, true)
        .expect("seed connected");
    assert!(state.get_agent(&zombie_id).unwrap().connected);

    // Reconcile detects, doesn't mutate.
    let report = pm.reconcile_panes(&state).await;
    assert!(
        report.zombies.iter().any(|z| z == &zombie_id),
        "reconcile must flag the zombie; got {:?}",
        report.zombies
    );
    assert!(
        state.get_agent(&zombie_id).unwrap().connected,
        "PaneManager must not mutate state itself"
    );

    // Caller composes: mark_agent_zombie for each reported zombie.
    // (This is literally what lib.rs startup + pane_reconcile do.)
    for id in &report.zombies {
        server.mark_agent_zombie(id).await;
    }

    // Post-condition: state flipped, UI will re-fetch on
    // agents-changed event.
    assert!(
        !state.get_agent(&zombie_id).unwrap().connected,
        "post mark_agent_zombie: state must reflect reality"
    );
}

#[tokio::test]
async fn mark_agent_zombie_flips_state_and_preserves_writers() {
    // mark_agent_zombie flips connected → false without touching the
    // writers map. Matters because a zombie wrapper may still have a
    // live socket (process lingering with dead tmux session), and we
    // want to keep it reachable for SHUTDOWN until the socket drops
    // naturally.
    let state = AppState::new();
    let server = SocketServer::with_configs(state.clone(), PaneManager::new(), vec![]);
    let zombie_id = "test-zombie-mark";
    state
        .set_agent_connected(zombie_id, true)
        .expect("seed connected");
    assert!(state.get_agent(zombie_id).unwrap().connected);

    // No wrapper was actually registered, so writers is empty.
    // is_connected reads writers — confirming false before and after
    // the flip shows mark_agent_zombie doesn't inject phantom
    // writers.
    assert!(!server.is_connected(zombie_id).await);

    server.mark_agent_zombie(zombie_id).await;

    assert!(
        !state.get_agent(zombie_id).unwrap().connected,
        "mark_agent_zombie must flip connected → false"
    );
    assert!(
        !server.is_connected(zombie_id).await,
        "mark_agent_zombie must not add the agent to writers"
    );

    // Idempotent: calling on an already-disconnected agent is safe.
    server.mark_agent_zombie(zombie_id).await;
    assert!(!state.get_agent(zombie_id).unwrap().connected);
}

#[tokio::test]
async fn mark_agent_zombie_noop_for_unknown_agent() {
    // Defensive: if the caller passes an id that isn't in state
    // (stale report.zombies race), the underlying
    // set_agent_connected auto-registers it with connected=false
    // rather than panicking.
    let state = AppState::new();
    let server = SocketServer::with_configs(state.clone(), PaneManager::new(), vec![]);
    server.mark_agent_zombie("never-heard-of-this-one").await;
    let a = state
        .get_agent("never-heard-of-this-one")
        .expect("auto-registered after set_agent_connected");
    assert!(!a.connected);
}

// ---------------------------------------------------------------------------
// agent_kill → mark_agent_killed state-flip path
// ---------------------------------------------------------------------------
//
// Regression test for the "kill leaves state.connected: true" bug
// observed on cursor-alor-4 (repro 2026-04-19): `cli.kill` tore down
// the tmux session but never flipped `state.connected`, so
// `agent_list` kept reporting the agent as connected with a dead
// session. The fix routes the kill path through a new
// `SocketServer::mark_agent_killed` helper that mirrors
// `mark_agent_zombie` but broadcasts with `reason: "killed"`.

#[tokio::test]
async fn mark_agent_killed_flips_state_and_preserves_writers() {
    // Mirror of `mark_agent_zombie_flips_state_and_preserves_writers`
    // for the kill path. Same contract: flip `connected` → false,
    // do NOT touch the writers map (natural socket-close cleanup
    // handles that when the wrapper actually dies).
    let state = AppState::new();
    let server = SocketServer::with_configs(state.clone(), PaneManager::new(), vec![]);
    let killed_id = "test-killed-no-child";
    state
        .set_agent_connected(killed_id, true)
        .expect("seed connected");
    assert!(state.get_agent(killed_id).unwrap().connected);
    // No wrapper registered → writers empty.
    assert!(!server.is_connected(killed_id).await);

    server.mark_agent_killed(killed_id).await;

    assert!(
        !state.get_agent(killed_id).unwrap().connected,
        "mark_agent_killed must flip connected → false"
    );
    assert!(
        !server.is_connected(killed_id).await,
        "mark_agent_killed must not add the agent to writers"
    );
}

#[tokio::test]
async fn mark_agent_killed_is_idempotent_across_double_kill() {
    // Edge case from the brief: if a tracked wrapper child IS
    // killed, the daemon's read-loop cleanup will ALSO eventually
    // fire a disconnect when the socket closes. We want the net
    // effect to be a single observable state transition to
    // disconnected — not a flip-flop, not an error.
    //
    // `set_agent_connected(id, false)` is already idempotent on the
    // flag value (no-op when already false). Repeated
    // `mark_agent_killed` calls therefore land at the same final
    // state. Duplicate `agent.disconnected` broadcasts are
    // tolerated by design (subscribers should be disconnect-
    // idempotent too — the CLI event stream has no trouble with
    // repeats).
    let state = AppState::new();
    let server = SocketServer::with_configs(state.clone(), PaneManager::new(), vec![]);
    let killed_id = "test-killed-double";
    state
        .set_agent_connected(killed_id, true)
        .expect("seed connected");

    server.mark_agent_killed(killed_id).await;
    assert!(!state.get_agent(killed_id).unwrap().connected);

    // Second call — simulates the socket-close cleanup racing with
    // the explicit kill. Must not panic, must not re-flip anything.
    server.mark_agent_killed(killed_id).await;
    assert!(
        !state.get_agent(killed_id).unwrap().connected,
        "double mark_agent_killed must leave state disconnected"
    );
}

#[tokio::test]
async fn mark_agent_killed_noop_for_unknown_agent() {
    // Defensive parallel to mark_agent_zombie_noop_for_unknown_agent:
    // killing an agent that was never registered (e.g. a stale id
    // from an orch cache) auto-registers with connected=false
    // rather than panicking.
    let state = AppState::new();
    let server = SocketServer::with_configs(state.clone(), PaneManager::new(), vec![]);
    server.mark_agent_killed("never-registered").await;
    let a = state
        .get_agent("never-registered")
        .expect("auto-registered after set_agent_connected");
    assert!(!a.connected);
}
