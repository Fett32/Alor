mod client;
mod detector;
mod protocol;
mod tmux;

use anyhow::{bail, Context, Result};
use detector::{pick_pending_trust_ack, AgentKind, IdleDetector};
use protocol::{
    AgentState, DaemonShutdown, Envelope, StatusRequest, StatusResponse, TaskAccept, TaskAssign,
    TaskComplete, UserIntervention, WrapperError, WrapperRegister, MSG_REGISTER, MSG_SHUTDOWN,
    MSG_STATUS_REQUEST, MSG_STATUS_RESPONSE, MSG_TASK_ACCEPT, MSG_TASK_ASSIGN, MSG_TASK_COMPLETE,
    MSG_ERROR, MSG_USER_INTERVENTION,
};
use std::collections::HashSet;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

// ---- configuration constants ------------------------------------------------

const DAEMON_SOCK: &str = "/tmp/alor/daemon.sock";
const CAPTURE_LINES: usize = 50;
/// Deeper pane capture taken at `task.complete` fire time specifically
/// for reply extraction. The idle-detection loop uses `CAPTURE_LINES`
/// (50) which is plenty to see the footer + input line, but long
/// replies (multi-paragraph prose, code blocks) scroll the top half
/// out of a 50-line window. tmux history-limit is 50000 (set by
/// `ensure_session_defaults`) so capturing 400 is essentially free.
/// Tune upward if real replies regularly truncate.
const EXTRACT_CAPTURE_LINES: usize = 400;
const POLL_INTERVAL_MS: u64 = 500;
/// Idle pattern must be stable for this many seconds before marking complete.
///
/// Tail-window sizing used to be a constant here (IDLE_TAIL = 5) but
/// cursor's TUI needed a wider window — it now lives per-runtime on
/// `AgentKind::idle_tail_window()` and flows through
/// `IdleDetector::tail_window()`. See detector.rs for rationale.
const IDLE_STABLE_SECS: f64 = 3.0;

/// Grace period after injection before user-intervention detection kicks in.
const INJECTION_GRACE_SECS: f64 = 10.0;
/// Cooldown between successive user.intervention events.
const INTERVENTION_COOLDOWN_SECS: f64 = 30.0;

/// Reconnect backoff: initial delay, max delay, multiplier.
const RECONNECT_INITIAL_MS: u64 = 500;
const RECONNECT_MAX_MS: u64 = 30_000;
const RECONNECT_MULTIPLIER: f64 = 2.0;

// ---- CLI args ---------------------------------------------------------------

struct WrapperArgs {
    agent: String,
    workdir: Option<String>,
    command: Option<String>,
    startup_file: Option<String>,
}

fn parse_args() -> Result<WrapperArgs> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut workdir: Option<String> = None;
    let mut command: Option<String> = None;
    let mut startup_file: Option<String> = None;
    let mut agent: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--workdir" => {
                if i + 1 < args.len() {
                    workdir = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    bail!("--workdir requires a value");
                }
            }
            "--command" => {
                if i + 1 < args.len() {
                    command = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    bail!("--command requires a value");
                }
            }
            "--startup-file" => {
                if i + 1 < args.len() {
                    startup_file = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    bail!("--startup-file requires a value");
                }
            }
            s if !s.starts_with('-') && agent.is_none() => {
                agent = Some(s.to_string());
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    let agent = agent.ok_or_else(|| anyhow::anyhow!(
        "Usage: alor-wrapper <agent-id> [--workdir DIR] [--command CMD] [--startup-file PATH]"
    ))?;

    Ok(WrapperArgs {
        agent,
        workdir,
        command,
        startup_file,
    })
}

// ---- helpers ----------------------------------------------------------------

fn session_name(agent: &str) -> String {
    format!("alor-{}", agent)
}

/// Start the tmux session if it doesn't already exist, running the agent CLI.
/// Returns `true` if a new session was created, `false` if reusing an existing one.
fn ensure_session(
    session: &str,
    kind: &AgentKind,
    workdir: Option<&str>,
    explicit_command: Option<&str>,
) -> Result<bool> {
    if tmux::session_exists(session) {
        info!(session, "reusing existing tmux session");
        // Ensure session defaults are applied even on pre-existing sessions.
        tmux::ensure_session_defaults(session);
        return Ok(false);
    }
    let command = explicit_command.unwrap_or(match kind {
        AgentKind::ClaudeCode => "claude",
        AgentKind::Codex => "codex",
        AgentKind::Gemini => "gemini",
        _ => "bash",
    });
    info!(session, command, "creating new tmux session");
    tmux::create_session_with_dir(session, command, workdir)
        .with_context(|| format!("could not create tmux session {session}"))?;
    Ok(true)
}

// ---- connect + register -----------------------------------------------------

/// Outcome of a registration attempt.
enum RegisterResult {
    /// Successfully registered — ready for main loop.
    Ok(client::DaemonClient),
    /// Daemon rejected us with a collision error — another wrapper with this
    /// agent_id is already connected. Retrying won't help; exit.
    Collision(String),
    /// Transient failure (network, daemon not up yet, etc.) — worth retrying.
    TransientError(anyhow::Error),
}

/// Connect to daemon and send registration. Returns the client on success.
async fn connect_and_register(agent: &str) -> RegisterResult {
    // Phase 1: connect and send registration.
    let mut client = match async {
        info!(agent, socket = DAEMON_SOCK, "connecting to daemon...");
        let mut client = client::DaemonClient::connect(DAEMON_SOCK)
            .await
            .context("connecting to daemon")?;

        info!(agent, "connected, sending registration...");
        let register_env = Envelope::new(MSG_REGISTER, WrapperRegister { agent_id: agent.to_owned() })?;
        client.send(&register_env).await?;
        Ok::<_, anyhow::Error>(client)
    }
    .await
    {
        Ok(c) => c,
        Err(e) => return RegisterResult::TransientError(e),
    };

    info!(agent, "registration sent, waiting for confirmation...");

    // Phase 2: wait briefly for a potential rejection from the daemon.
    // If the daemon accepts us, it won't send anything immediately —
    // it just starts the read loop. If it rejects us (collision), it
    // sends a wrapper.error and closes the connection.
    match tokio::time::timeout(Duration::from_secs(2), client.recv()).await {
        Ok(Ok(Some(env))) if env.kind == MSG_ERROR => {
            let message = env
                .payload
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
                .to_string();
            if message.contains("Collision") || message.contains("already connected") {
                return RegisterResult::Collision(message);
            }
            RegisterResult::TransientError(anyhow::anyhow!("daemon rejected registration: {message}"))
        }
        Ok(Ok(Some(_env))) => {
            // Got a real message (e.g. task.assign) — registration accepted.
            info!(agent, "registration accepted (got immediate message)");
            RegisterResult::Ok(client)
        }
        Ok(Ok(None)) => {
            RegisterResult::TransientError(anyhow::anyhow!("daemon closed connection after registration"))
        }
        Ok(Err(e)) => {
            RegisterResult::TransientError(anyhow::anyhow!("error reading after registration: {e:#}"))
        }
        Err(_timeout) => {
            // No error received within timeout — daemon accepted us silently.
            info!(agent, "registration accepted (no rejection within timeout)");
            RegisterResult::Ok(client)
        }
    }
}

/// Try to connect with exponential backoff.
/// Returns Ok(client) on success, or Err if the daemon rejected us with a
/// collision (fatal — retrying won't help).
async fn connect_with_backoff(agent: &str) -> Result<client::DaemonClient> {
    let mut delay_ms = RECONNECT_INITIAL_MS;

    loop {
        match connect_and_register(agent).await {
            RegisterResult::Ok(client) => {
                info!("reconnected to daemon");
                return Ok(client);
            }
            RegisterResult::Collision(msg) => {
                error!(
                    agent,
                    "agent ID collision: {msg}. Another wrapper with this ID is already connected. Exiting."
                );
                bail!("agent ID collision: {msg}");
            }
            RegisterResult::TransientError(e) => {
                warn!(
                    delay_ms,
                    "daemon connection failed ({e:#}), retrying in {delay_ms}ms"
                );
                sleep(Duration::from_millis(delay_ms)).await;
                delay_ms = ((delay_ms as f64 * RECONNECT_MULTIPLIER) as u64).min(RECONNECT_MAX_MS);
            }
        }
    }
}

// ---- main loop --------------------------------------------------------------

/// Reason the run loop exited.
enum ExitReason {
    /// Daemon sent shutdown — wrapper should exit entirely.
    Shutdown,
    /// Connection lost — wrapper should reconnect.
    Disconnected,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Default to `alor_wrapper=info`. `RUST_LOG` overrides — including
    // `RUST_LOG=alor_wrapper=debug` to enable the idle-poll diagnostic
    // trail documented on `run_loop`.
    //
    // Previous `from_default_env().add_directive("alor_wrapper=info")`
    // was buggy: tracing-subscriber's "most recently added directive
    // wins" rule meant the hard-coded `info` clobbered any env
    // override on the same target (see tracing-subscriber EnvFilter
    // docs). The builder `with_default_directive` + `from_env_lossy`
    // pattern does it the right way around — the builder default is
    // the fallback, the env layers on top and wins.
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::builder()
        .with_default_directive("alor_wrapper=info".parse().unwrap())
        .from_env_lossy();
    tracing_subscriber::fmt().with_env_filter(filter).init();

    if let Err(e) = run_main().await {
        error!("alor-wrapper fatal error: {e:#}");
        return Err(e);
    }
    Ok(())
}

async fn run_main() -> Result<()> {
    let wargs = parse_args()?;
    let agent = wargs.agent;
    let session = session_name(&agent);

    // Infer AgentKind from explicit command if provided, otherwise from agent name.
    let kind = if let Some(ref cmd) = wargs.command {
        AgentKind::from_name(cmd)
    } else {
        AgentKind::from_name(&agent)
    };
    let detector = IdleDetector::new(&kind);

    info!(agent, "alor-wrapper starting");

    // Ensure the tmux session exists (once, outside reconnect loop).
    let freshly_created = ensure_session(
        &session,
        &kind,
        wargs.workdir.as_deref(),
        wargs.command.as_deref(),
    )?;

    // First-launch setup. Two independent concerns, now cleanly split
    // so trust-ack polling runs for every wrapper-runtime agent — NOT
    // only fresh sessions (the old gating missed re-spawns with a
    // still-stuck dialog and the lib.rs auto-reclaim path where
    // freshly_created is always false).
    //
    //   1. Trust-ack polling — ALWAYS runs when the runtime has
    //      trust_prompts entries. Loop captures the pane every 500ms;
    //      on a hint match, sends the paired ack key and keeps
    //      polling so a follow-up prompt (codex update → trust) gets
    //      picked up. Fast-exits after 4 consecutive no-hit polls
    //      (~2s of stable pane) so idle sessions aren't penalized.
    //      Hard cap at 60 polls (~30s) for pathological slow boots
    //      plus multi-stage prompt chains (codex update dismissal,
    //      trust-dialog render, trust ack).
    //
    //   2. Briefing injection — STILL runs only on fresh sessions
    //      with --startup-file. Unchanged.

    // Boot grace for freshly-created sessions only: give the CLI a
    // moment to start rendering its initial screen before the first
    // poll. Re-spawns inherit a populated pane; no grace needed.
    if freshly_created {
        sleep(Duration::from_secs(3)).await;
    }

    let prompts = kind.trust_prompts();
    if !prompts.is_empty() {
        const MAX_POLLS: u32 = 60;            // 60 × 500ms = 30s max
        const STABLE_THRESHOLD: u32 = 4;      // 4 polls (~2s) with no hit → done
        let mut no_hit_streak: u32 = 0;
        let mut acked_any = false;
        // T13: single-shot-per-hint debounce. Codex's "Update
        // available!" banner lingers on the pane for ~10s after the
        // first `3` keypress dismisses it internally (TUI doesn't
        // redraw immediately), and the pre-T13 loop re-fired the ack
        // on every subsequent poll (13 total) — splatting unwanted
        // keystrokes and preventing briefing injection until the
        // banner finally cleared. The set tracks hints already acked
        // in THIS wrapper-lifetime so repeats become no-ops at the
        // helper level while still letting the search advance to
        // later prompts in the list (e.g. codex's trust-dir dialog
        // rendered after the update banner). Decision logic lives in
        // `pick_pending_trust_ack` (detector.rs) — unit-tested there.
        let mut acked_hints: HashSet<&'static str> = HashSet::new();
        for attempt in 0..MAX_POLLS {
            sleep(Duration::from_millis(500)).await;
            let lines = match tmux::capture_pane(&session, 30) {
                Ok(l) => l,
                Err(e) => {
                    warn!(
                        error = ?e,
                        attempt,
                        "capture_pane failed during trust-ack poll"
                    );
                    no_hit_streak += 1;
                    if no_hit_streak >= STABLE_THRESHOLD {
                        break;
                    }
                    continue;
                }
            };
            // Delegate to the pure helper: returns the first visible
            // AND not-yet-acked prompt (None if all visible prompts
            // are stale-acked or nothing is visible at all).
            let picked = pick_pending_trust_ack(&lines, prompts, &acked_hints);
            let mut acked = false;
            if let Some((hint, ack)) = picked {
                info!(
                    hint,
                    ack,
                    attempt,
                    "trust prompt visible; sending ack key"
                );
                if let Err(e) = tmux::send_keys(&session, ack) {
                    warn!(error = ?e, hint, "send_keys failed during trust-ack");
                }
                acked_hints.insert(*hint);
                acked = true;
                acked_any = true;
            }
            if acked {
                no_hit_streak = 0;
            } else {
                no_hit_streak += 1;
                if no_hit_streak >= STABLE_THRESHOLD {
                    break;
                }
            }
        }
        if !acked_any {
            info!(
                kind = ?kind,
                "no trust prompt observed during startup window; proceeding"
            );
        }
    }

    // Briefing injection — still gated on freshly_created AND
    // startup_file. Runs after trust-ack polling completes so the
    // briefing lands at the CLI's real input prompt, not on top of
    // a lingering trust dialog.
    if freshly_created {
        if let Some(ref startup_path) = wargs.startup_file {
            info!(startup_path, "handling startup briefing...");

            // Wait for the actual input prompt (up to 15s).
            let mut ready = false;
            for i in 0..30 {
                if let Ok(lines) = tmux::capture_pane(&session, 20) {
                    let text = lines.join("\n");
                    // Look for common prompts across the supported CLIs:
                    //   - shell-style (gemini's " > Type your message",
                    //     claude's "❯", default "$ ", zsh "% ")
                    //   - TUI input markers (cursor's "→", codex 0.120's
                    //     "›" U+203A). Pre-T11 these Unicode markers
                    //     weren't in the match list — the wrapper would
                    //     burn the full 15 s budget and `warn!("timed
                    //     out waiting for prompt, skipping briefing")`,
                    //     so startup_file-driven preambles for cursor /
                    //     codex silently never landed. T9 diagnosis
                    //     flagged the miss; T11 closes it here.
                    if text.contains(" > ")
                        || text.contains("❯")
                        || text.contains("$ ")
                        || text.contains("% ")
                        || text.contains("→")
                        || text.contains("›")
                    {
                        info!("detected prompt after {}ms, sending briefing", i * 500);
                        ready = true;
                        break;
                    }
                }
                sleep(Duration::from_millis(500)).await;
            }

            if ready {
                match std::fs::read_to_string(startup_path) {
                    Ok(contents) => {
                        if let Err(e) = tmux::send_keys(&session, &contents) {
                            error!("failed to inject startup file: {e:#}");
                        }
                    }
                    Err(e) => {
                        error!(startup_path, "failed to read startup file: {e:#}");
                    }
                }
            } else {
                warn!("timed out waiting for prompt, skipping briefing");
            }
        }
    }

    // Initial connection.
    // Use connect_with_backoff to handle initial connection failures.
    // If the daemon rejects us with a collision, exit immediately.
    let mut client = connect_with_backoff(&agent).await?;

    info!(agent, "alor-wrapper entered main loop");

    // Give the agent a moment to render its first prompt before polling.
    sleep(Duration::from_millis(1500)).await;

    // State persists across reconnects so the daemon can be re-informed.
    let mut state = AgentState::Idle;

    loop {
        let exit_reason = run_loop(&agent, &session, &detector, &mut client, &mut state).await?;

        match exit_reason {
            ExitReason::Shutdown => {
                info!("daemon sent shutdown, exiting");
                break;
            }
            ExitReason::Disconnected => {
                warn!("lost daemon connection, reconnecting...");
                client = connect_with_backoff(&agent).await?;

                // Re-report active task to daemon after reconnect.
                if let AgentState::Running { task_id } = &state {
                    info!(task_id = %task_id, "re-reporting active task after reconnect");
                    let status_env = Envelope::new(
                        MSG_STATUS_RESPONSE,
                        StatusResponse {
                            agent_id: agent.clone(),
                            task_id: Some(*task_id),
                            alive: true,
                            details: Some("running (reconnected)".to_owned()),
                        },
                    );
                    if let Ok(env) = status_env {
                        if let Err(e) = client.send(&env).await {
                            warn!("failed to re-report task: {e:#}");
                        }
                    }
                }
            }
        }
    }

    info!("alor-wrapper exiting");
    Ok(())
}

/// Run the main select loop. Returns the reason it exited.
///
/// **Debug instrumentation**: enable with
/// `RUST_LOG=alor_wrapper=debug` (or `alor_wrapper=trace` for every
/// poll cycle) to get a per-iteration trail of the idle detector's
/// decision-making. The structured fields logged on each
/// `idle_poll` event are:
///   - `is_idle`         — what the detector says about the tail.
///   - `past_idle_grace` — whether the post-injection grace window
///                          has elapsed.
///   - `idle_since_secs` — how long the pane has been continuously
///                          classified as idle (None if reset).
///   - `last_inj_secs`   — seconds since the last task-assign
///                          injection (None if never injected).
///   - `content_changed` — whether the tail fingerprint changed
///                          since the prior poll.
///   - `decision`        — fire / accumulate / reset(not-idle) /
///                          reset(grace) / reset(daemon-msg).
///
/// Motivating bug (task bd61772a, investigation at cc9e757):
/// wrapper was actively polling, pane was stable, detector should
/// have classified as idle, yet task.complete never fired. Static
/// analysis couldn't pin the block; this instrumentation is the
/// diagnostic tool for the next repro.
async fn run_loop(
    agent: &str,
    session: &str,
    detector: &IdleDetector,
    client: &mut client::DaemonClient,
    state: &mut AgentState,
) -> Result<ExitReason> {
    let mut idle_since: Option<Instant> = None;

    // User intervention detection state.
    let mut last_injection: Option<Instant> = None;
    let mut last_intervention_sent: Option<Instant> = None;
    let mut prev_fingerprint: Option<String> = None;
    // Tracks the previous poll's `is_idle` result. Intervention
    // detection fires only when the pane was idle on the PRIOR poll
    // and then changes — agent-driven animation (cursor TUI spinners,
    // codex `Working…` counters) has is_idle=false throughout, so
    // prev_is_idle stays false and no false-positive intervention
    // fires. See the live cursor-alor bug write-up: pre-fix the
    // check was `content_changed && !is_idle` which misclassified
    // every animation frame past INJECTION_GRACE as user input.
    let mut prev_is_idle: bool = false;
    let mut last_heartbeat = Instant::now();

    // Log every fresh run_loop entry — including reconnects —
    // so a post-mortem can see whether the wrapper was churning
    // through reconnect cycles (which reset idle_since to None
    // and would prevent the IDLE_STABLE_SECS threshold from
    // ever accumulating).
    debug!(
        agent,
        state = match state {
            AgentState::Idle => "idle",
            AgentState::Running { .. } => "running",
        },
        "run_loop starting (fresh local state: idle_since/last_injection/prev_fingerprint = None)"
    );

    loop {
        // ---- Heartbeat: stay alive in the UI every 3s ----
        if last_heartbeat.elapsed().as_secs() >= 3 {
            let (task_id, details) = match state {
                AgentState::Idle => (None, Some("idle".to_owned())),
                AgentState::Running { task_id } => (Some(*task_id), Some("running".to_owned())),
            };
            let hb_env = Envelope::new(
                MSG_STATUS_RESPONSE,
                StatusResponse {
                    agent_id: agent.to_owned(),
                    task_id,
                    alive: true,
                    details,
                },
            )?;

            if let Err(e) = client.send(&hb_env).await {
                warn!("heartbeat failed: {e:#}");
                return Ok(ExitReason::Disconnected);
            }
            last_heartbeat = Instant::now();
        }

        // ---- race: daemon message OR poll interval ----
        tokio::select! {
            result = client.recv() => {
                match result {
                    Ok(Some(envelope)) => {
                        let kind = envelope.kind.clone();
                        let keep_running = handle_envelope(
                            envelope,
                            agent,
                            session,
                            state,
                            client,
                            &mut last_injection,
                        )
                        .await?;
                        // Reset idle timer on any daemon message.
                        let had_timer = idle_since.is_some();
                        idle_since = None;
                        if had_timer {
                            debug!(
                                agent,
                                envelope_kind = %kind,
                                decision = "reset(daemon-msg)",
                                "idle_poll: timer reset by incoming daemon message"
                            );
                        }
                        if !keep_running {
                            return Ok(ExitReason::Shutdown);
                        }
                    }
                    Ok(None) => {
                        // EOF — daemon closed connection.
                        return Ok(ExitReason::Disconnected);
                    }
                    Err(e) => {
                        warn!("error reading from daemon: {e:#}");
                        return Ok(ExitReason::Disconnected);
                    }
                }
            }
            _ = sleep(Duration::from_millis(POLL_INTERVAL_MS)) => {
                // Timer fired — fall through to tmux poll below.
            }
        }

        // ---- poll tmux for idle pattern when a task is running ----
        if let AgentState::Running { task_id } = &state.clone() {
            let task_id = *task_id;
            match tmux::capture_pane(session, CAPTURE_LINES) {
                Ok(lines) => {
                    // Fingerprint window tracks the detector's tail
                    // window so "did the pane change?" aligns with
                    // "is the runtime idle?" — a change in only the
                    // TUI region (spinner above Composer, `Working`
                    // ticker, streaming lines above codex's `›`)
                    // correctly registers as content_changed.
                    let tail_n = detector.tail_window();
                    let tail: Vec<&str> = lines
                        .iter()
                        .rev()
                        .take(tail_n)
                        .map(|s| s.as_str())
                        .collect();
                    let fingerprint = tail.join("\n");
                    let content_changed =
                        prev_fingerprint.as_deref() != Some(&fingerprint);
                    prev_fingerprint = Some(fingerprint);

                    let is_idle = detector.is_idle_tail(&lines);
                    let now = Instant::now();
                    let past_idle_grace = last_injection
                        .map(|t| {
                            now.duration_since(t).as_secs_f64() >= INJECTION_GRACE_SECS
                        })
                        .unwrap_or(true);
                    let past_cooldown = last_intervention_sent
                        .map(|t| {
                            now.duration_since(t).as_secs_f64()
                                >= INTERVENTION_COOLDOWN_SECS
                        })
                        .unwrap_or(true);
                    let idle_accumulated_secs = idle_since
                        .map(|t| now.duration_since(t).as_secs_f64());
                    let last_inj_secs = last_injection
                        .map(|t| now.duration_since(t).as_secs_f64());

                    let inputs = PollInputs {
                        is_idle,
                        was_idle_previously: prev_is_idle,
                        content_changed,
                        past_idle_grace,
                        past_cooldown,
                        idle_accumulated_secs,
                        idle_stable_secs: IDLE_STABLE_SECS,
                    };
                    let decision = decide_poll_actions(inputs);

                    // ---- fire intervention if flagged ----
                    if decision.fire_intervention {
                        info!("detected user intervention in tmux pane");
                        let interv_env = Envelope::new(
                            MSG_USER_INTERVENTION,
                            UserIntervention { agent_id: agent.to_owned() },
                        )?;
                        client.send(&interv_env).await?;
                        last_intervention_sent = Some(now);
                    }

                    // ---- idle-timer bookkeeping + task.complete fire ----
                    if decision.keep_idle_timer {
                        // Start (or keep) the accumulation clock.
                        let first_seen = *idle_since.get_or_insert(now);
                        let elapsed = now.duration_since(first_seen).as_secs_f64();

                        if decision.fire_complete {
                            info!(
                                stable_for = format!("{elapsed:.1}s"),
                                "idle pattern stable, task complete"
                            );
                            debug!(
                                agent,
                                task_id = %task_id,
                                is_idle,
                                past_idle_grace,
                                idle_since_secs = Some(elapsed),
                                last_inj_secs,
                                content_changed,
                                decision = decision.decision,
                                "idle_poll"
                            );
                            *state = AgentState::Idle;
                            idle_since = None;

                            // Capture the assistant's reply for
                            // task.details. The stability gate above
                            // guarantees the pane has been quiet for
                            // IDLE_STABLE_SECS, so the capture should
                            // reflect a complete (not mid-stream)
                            // reply. Deeper window than the poll's
                            // CAPTURE_LINES because long replies
                            // scroll out of the 50-line view.
                            //
                            // Fallback ladder:
                            //   1. deep capture + extract → reply body
                            //   2. deep capture fails        → reuse `lines`
                            //   3. extract returns None      → synthetic
                            //      EMPTY_CAPTURE_PLACEHOLDER so the
                            //      orch sees a distinguishable
                            //      "captured nothing" signal rather
                            //      than the pre-fix null.
                            let kind = AgentKind::from_name(agent);
                            let deep_lines = match tmux::capture_pane(session, EXTRACT_CAPTURE_LINES) {
                                Ok(ls) => ls,
                                Err(e) => {
                                    warn!(error = %e, "deep pane capture failed at task.complete; falling back to idle-poll capture");
                                    lines.clone()
                                }
                            };
                            let extracted = kind.extract_reply(&deep_lines);
                            if let Some(ref body) = extracted {
                                if detector::looks_truncated(body) {
                                    warn!(
                                        agent,
                                        task_id = %task_id,
                                        "extracted reply has odd code-fence count — may be partial; consider tuning IDLE_STABLE_SECS"
                                    );
                                }
                            }
                            let body = extracted.unwrap_or_else(||
                                detector::EMPTY_CAPTURE_PLACEHOLDER.to_string()
                            );
                            let (summary, details) = detector::split_summary_details(&body);
                            // split returns (None, None) only on an
                            // all-whitespace input. Since `body` is
                            // either the placeholder string or a
                            // non-empty extracted body, one of
                            // `summary` / `details` is guaranteed
                            // Some — but the match defends against
                            // future placeholder changes that might
                            // drift the invariant.
                            let (summary, details) = match (summary, details) {
                                (None, None) => (
                                    Some(detector::EMPTY_CAPTURE_PLACEHOLDER.to_string()),
                                    None,
                                ),
                                other => other,
                            };

                            let complete_env = Envelope::new(
                                MSG_TASK_COMPLETE,
                                TaskComplete {
                                    task_id,
                                    summary,
                                    details,
                                    output: None,
                                },
                            )?;
                            client.send(&complete_env).await?;
                        } else {
                            debug!(
                                agent,
                                task_id = %task_id,
                                is_idle,
                                past_idle_grace,
                                idle_since_secs = Some(elapsed),
                                last_inj_secs,
                                content_changed,
                                decision = decision.decision,
                                "idle_poll"
                            );
                        }
                    } else {
                        // Reset the stability timer. The `decision`
                        // label narrows down which sub-condition
                        // killed it — critical for diagnosing stuck
                        // tasks (see bd61772a).
                        let had_timer = idle_since.is_some();
                        idle_since = None;
                        if had_timer {
                            debug!(
                                agent,
                                task_id = %task_id,
                                is_idle,
                                past_idle_grace,
                                last_inj_secs,
                                content_changed,
                                decision = decision.decision,
                                "idle_poll: timer reset"
                            );
                        } else {
                            tracing::trace!(
                                agent,
                                task_id = %task_id,
                                is_idle,
                                past_idle_grace,
                                last_inj_secs,
                                content_changed,
                                decision = decision.decision,
                                "idle_poll"
                            );
                        }
                    }

                    // Track is_idle for the NEXT poll's intervention
                    // gate. Agent-driven state (animation, spinner)
                    // keeps this false → no false-positive fires;
                    // genuine user input against an idle prompt
                    // transitions true → false (or true → true with
                    // content_changed) → intervention fires
                    // correctly.
                    prev_is_idle = is_idle;
                }
                Err(e) => {
                    warn!("capture_pane error: {e:#}");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Per-poll decision logic (pure)
// ---------------------------------------------------------------------------

/// Per-poll input snapshot handed to [`decide_poll_actions`]. All
/// fields are plain data — no `Instant` / I/O — so tests can drive
/// the full matrix (idle / active / grace / cooldown / content-
/// changed) deterministically from fixtures.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PollInputs {
    /// `IdleDetector::is_idle_tail(&lines)` for the current poll.
    pub is_idle: bool,
    /// Previous poll's `is_idle`. Defaults false on the very first
    /// poll — safe because `past_idle_grace` blocks intervention for
    /// `INJECTION_GRACE_SECS` anyway.
    pub was_idle_previously: bool,
    /// True if the tail-window fingerprint changed since last poll.
    pub content_changed: bool,
    /// True once `INJECTION_GRACE_SECS` has elapsed since the last
    /// task.assign injection (or if nothing was ever injected).
    pub past_idle_grace: bool,
    /// True once `INTERVENTION_COOLDOWN_SECS` has elapsed since the
    /// last user.intervention fire (or if none has fired yet).
    pub past_cooldown: bool,
    /// Seconds of idle accumulation so far. `None` if the idle
    /// timer hasn't started yet this run.
    pub idle_accumulated_secs: Option<f64>,
    /// Threshold for firing task.complete — wired from the
    /// `IDLE_STABLE_SECS` constant in production, passed through so
    /// unit tests can pick their own.
    pub idle_stable_secs: f64,
}

/// Output of [`decide_poll_actions`]. Caller interprets these flags
/// into I/O actions — send user.intervention, send task.complete,
/// reset/keep the idle timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PollDecision {
    pub fire_intervention: bool,
    pub fire_complete: bool,
    /// True → caller should `get_or_insert(now)` on the idle-timer;
    /// false → caller should clear it to `None`.
    pub keep_idle_timer: bool,
    /// Human-readable label for the `decision` field in `idle_poll`
    /// log lines. Mirrors the prior inline decisions (`reset(grace)`,
    /// `reset(not-idle)`, `accumulate`, `fire`) plus a new
    /// `reset(content-changed)` case introduced by the stability
    /// guard.
    pub decision: &'static str,
}

/// Pure per-poll rule engine. Kept free of state and time so tests
/// can assert each branch with plain `PollInputs` records.
///
/// Rules, in effect:
///   - **Intervention**: fires when the pane changed AND the runtime
///     was idle LAST poll AND we're past both grace and cooldown.
///     Crucially, the gate is `was_idle_previously` — NOT `!is_idle`
///     as before. Previously, every agent-driven animation poll
///     registered as intervention (cursor `ctrl+c to stop` etc.
///     keep is_idle false throughout generation). Now only genuine
///     idle→active transitions fire.
///
///   - **Task complete**: requires a *stable* idle — is_idle AND
///     !content_changed. On TUIs whose idle prompt stays visible
///     during streaming (codex 0.120: `›` input line + footer are
///     always on screen, with streaming tokens rendered above),
///     is_idle alone isn't enough: the wrapper was firing
///     task.complete as soon as the token stream started. Adding
///     the `!content_changed` guard gates completion on the pane
///     actually having settled.
pub(crate) fn decide_poll_actions(p: PollInputs) -> PollDecision {
    // Pane is genuinely at rest: runtime idle AND content unchanging.
    let stable_idle = p.is_idle && !p.content_changed;
    // Only accumulate the completion timer after the post-injection
    // grace window. Grace suppresses early false-fires while the
    // agent is still processing the injected prompt.
    let keep_idle_timer = stable_idle && p.past_idle_grace;

    let fire_complete = keep_idle_timer
        && p.idle_accumulated_secs
            .map(|s| s >= p.idle_stable_secs)
            .unwrap_or(false);

    // Intervention: agent was idle last poll, now something changed.
    // By requiring `was_idle_previously`, we ensure agent-driven
    // animation never counts: animating runs have is_idle=false on
    // prior polls, so prev_is_idle=false, so this branch stays cold.
    let fire_intervention = p.content_changed
        && p.was_idle_previously
        && p.past_idle_grace
        && p.past_cooldown;

    // Ordered label — "fire" wins over "accumulate", both win over
    // the reset variants. Matches the prior log-line taxonomy plus
    // the new `reset(content-changed)` case.
    let decision = if fire_complete {
        "fire"
    } else if keep_idle_timer {
        "accumulate"
    } else if !p.past_idle_grace {
        "reset(grace)"
    } else if !p.is_idle {
        "reset(not-idle)"
    } else {
        // is_idle=true but content_changed=true — the new codex-
        // streaming case. Pane pattern-matches idle but tokens are
        // still arriving, so the timer resets.
        "reset(content-changed)"
    };

    PollDecision {
        fire_intervention,
        fire_complete,
        keep_idle_timer,
        decision,
    }
}

#[cfg(test)]
mod poll_decision_tests {
    //! Unit tests for [`decide_poll_actions`]. One test per rule
    //! branch, each asserting the one output bit that branch
    //! uniquely controls — keeps regressions pinned to the specific
    //! rule that broke.

    use super::*;

    /// Baseline inputs: agent is running, post-grace, no cooldown
    /// active, idle-timer hasn't started, IDLE_STABLE_SECS = 3.0
    /// (prod value). Each test tweaks specific fields.
    fn baseline() -> PollInputs {
        PollInputs {
            is_idle: false,
            was_idle_previously: false,
            content_changed: false,
            past_idle_grace: true,
            past_cooldown: true,
            idle_accumulated_secs: None,
            idle_stable_secs: IDLE_STABLE_SECS,
        }
    }

    // ---- task.complete rules ----

    #[test]
    fn fire_complete_when_stable_idle_past_threshold() {
        let d = decide_poll_actions(PollInputs {
            is_idle: true,
            content_changed: false,
            idle_accumulated_secs: Some(IDLE_STABLE_SECS + 0.5),
            ..baseline()
        });
        assert!(d.fire_complete);
        assert!(d.keep_idle_timer);
        assert_eq!(d.decision, "fire");
    }

    #[test]
    fn accumulate_when_stable_idle_below_threshold() {
        let d = decide_poll_actions(PollInputs {
            is_idle: true,
            content_changed: false,
            idle_accumulated_secs: Some(IDLE_STABLE_SECS - 0.5),
            ..baseline()
        });
        assert!(!d.fire_complete);
        assert!(d.keep_idle_timer);
        assert_eq!(d.decision, "accumulate");
    }

    #[test]
    fn no_complete_when_content_changed_even_if_idle() {
        // This is the codex-streaming bug. The input prompt + footer
        // are always visible → is_idle=true — but tokens are
        // streaming above, so content_changed=true every poll.
        // Without the stability guard, task.complete fires as soon
        // as IDLE_STABLE_SECS elapses mid-stream.
        let d = decide_poll_actions(PollInputs {
            is_idle: true,
            content_changed: true,
            idle_accumulated_secs: Some(IDLE_STABLE_SECS + 10.0),
            ..baseline()
        });
        assert!(!d.fire_complete);
        assert!(!d.keep_idle_timer);
        assert_eq!(d.decision, "reset(content-changed)");
    }

    #[test]
    fn no_complete_during_injection_grace() {
        let d = decide_poll_actions(PollInputs {
            is_idle: true,
            content_changed: false,
            past_idle_grace: false,
            idle_accumulated_secs: Some(IDLE_STABLE_SECS + 5.0),
            ..baseline()
        });
        assert!(!d.fire_complete);
        assert!(!d.keep_idle_timer);
        assert_eq!(d.decision, "reset(grace)");
    }

    #[test]
    fn no_complete_when_not_idle() {
        let d = decide_poll_actions(PollInputs {
            is_idle: false,
            content_changed: false,
            idle_accumulated_secs: Some(IDLE_STABLE_SECS + 5.0),
            ..baseline()
        });
        assert!(!d.fire_complete);
        assert!(!d.keep_idle_timer);
        assert_eq!(d.decision, "reset(not-idle)");
    }

    // ---- user.intervention rules ----

    #[test]
    fn fire_intervention_when_idle_then_pane_changes() {
        // Canonical user intervention: agent was idle last poll
        // (prev_is_idle = true), pane changed this poll, past both
        // grace and cooldown. This is the case the original logic
        // was meant to detect.
        let d = decide_poll_actions(PollInputs {
            was_idle_previously: true,
            content_changed: true,
            ..baseline()
        });
        assert!(d.fire_intervention);
    }

    #[test]
    fn no_intervention_during_cursor_long_task_animation() {
        // The original cursor-alor false-positive bug. During a
        // long cursor task: ctrl+c-to-stop visible → is_idle=false
        // throughout → prev_is_idle stays false across many polls.
        // Every animation frame has content_changed=true, but with
        // was_idle_previously=false no intervention fires.
        let d = decide_poll_actions(PollInputs {
            is_idle: false,
            was_idle_previously: false,
            content_changed: true,
            ..baseline()
        });
        assert!(!d.fire_intervention);
    }

    #[test]
    fn no_intervention_during_injection_grace() {
        // Even if the pane changes and was idle before, grace
        // suppresses intervention while the agent is still
        // processing the injected prompt.
        let d = decide_poll_actions(PollInputs {
            was_idle_previously: true,
            content_changed: true,
            past_idle_grace: false,
            ..baseline()
        });
        assert!(!d.fire_intervention);
    }

    #[test]
    fn no_intervention_during_cooldown() {
        let d = decide_poll_actions(PollInputs {
            was_idle_previously: true,
            content_changed: true,
            past_cooldown: false,
            ..baseline()
        });
        assert!(!d.fire_intervention);
    }

    #[test]
    fn no_intervention_when_pane_unchanged() {
        let d = decide_poll_actions(PollInputs {
            was_idle_previously: true,
            content_changed: false,
            ..baseline()
        });
        assert!(!d.fire_intervention);
    }

    // ---- multi-poll sequence smoke ----

    #[test]
    fn codex_streaming_sequence_never_fires_complete() {
        // Simulate a realistic codex streaming sequence: 15 polls,
        // is_idle=true (prompt + footer visible), content_changed=true
        // every poll (new tokens streaming). IDLE_STABLE_SECS=3
        // would trigger at poll ~6 without the stability guard.
        // With the guard, no poll should fire or keep the timer.
        let mut accumulated: Option<f64> = None;
        for poll_i in 0..15 {
            let d = decide_poll_actions(PollInputs {
                is_idle: true,
                content_changed: true,
                idle_accumulated_secs: accumulated,
                ..baseline()
            });
            assert!(!d.fire_complete, "streaming poll {poll_i} must not fire");
            assert!(!d.keep_idle_timer, "streaming poll {poll_i} must not keep timer");
            // Caller would clear accumulated to None — simulate.
            accumulated = if d.keep_idle_timer {
                Some(accumulated.map(|s| s + 0.5).unwrap_or(0.0))
            } else {
                None
            };
        }
    }

    #[test]
    fn cursor_long_task_never_fires_intervention_even_over_many_frames() {
        // Simulate 60 animation frames during a long cursor task:
        // is_idle always false (ctrl+c to stop visible),
        // content_changed true every frame (spinner ticks). Must
        // produce zero interventions.
        let mut prev_is_idle = false;
        let mut intervention_count = 0;
        for _ in 0..60 {
            let d = decide_poll_actions(PollInputs {
                is_idle: false,
                was_idle_previously: prev_is_idle,
                content_changed: true,
                ..baseline()
            });
            if d.fire_intervention {
                intervention_count += 1;
            }
            prev_is_idle = false; // is_idle stayed false this poll
        }
        assert_eq!(intervention_count, 0);
    }
}

/// Handle one incoming envelope from the daemon.
/// Returns `false` to signal the main loop should exit.
async fn handle_envelope(
    env: Envelope,
    agent: &str,
    session: &str,
    state: &mut AgentState,
    client: &mut client::DaemonClient,
    last_injection: &mut Option<Instant>,
) -> Result<bool> {
    match env.kind.as_str() {
        MSG_TASK_ASSIGN => {
            let task: TaskAssign = env
                .decode_payload()
                .context("decode task.assign payload")?;
            info!(task_id = %task.task_id, title = %task.title, "received task");

            // Inject the prompt into the tmux session FIRST.
            // Run in spawn_blocking since send_keys does blocking I/O + sleep.
            let prompt = format!("{}\n{}", task.title, task.description);
            let session_owned = session.to_string();
            let inject_result = tokio::task::spawn_blocking(move || {
                tmux::send_keys(&session_owned, &prompt)
            }).await.context("send_keys task panicked")?;
            match inject_result {
                Ok(()) => {
                    *last_injection = Some(Instant::now());
                    // Only send accept AFTER successful injection.
                    let accept_env = Envelope::new(
                        MSG_TASK_ACCEPT,
                        TaskAccept { task_id: task.task_id },
                    )?;
                    client.send(&accept_env).await?;
                    *state = AgentState::Running { task_id: task.task_id };
                    info!(task_id = %task.task_id, "task accepted and injected");
                }
                Err(e) => {
                    // Injection failed - send error, don't send accept.
                    let message = format!("send_keys failed: {e:#}");
                    error!(%message);
                    let err_env = Envelope::new(
                        MSG_ERROR,
                        WrapperError { agent_id: agent.to_owned(), message },
                    )?;
                    client.send(&err_env).await?;
                }
            }
        }

        MSG_STATUS_REQUEST => {
            let req: StatusRequest = env
                .decode_payload()
                .context("decode status.request payload")?;
            let (alive, task_id, details) = match state {
                AgentState::Idle => (true, req.task_id, Some("idle".to_owned())),
                AgentState::Running { task_id } => {
                    (true, Some(*task_id), Some("running".to_owned()))
                }
            };
            let resp_env = Envelope::new(
                MSG_STATUS_RESPONSE,
                StatusResponse {
                    agent_id: agent.to_owned(),
                    task_id,
                    alive,
                    details,
                },
            )?;
            // Preserve correlation_id so daemon can match request to response.
            let mut resp_env = resp_env;
            resp_env.correlation_id = env.correlation_id;
            client.send(&resp_env).await?;
        }

        MSG_SHUTDOWN => {
            let _: DaemonShutdown = env
                .decode_payload()
                .unwrap_or(DaemonShutdown {});
            info!("received shutdown from daemon");
            return Ok(false);
        }

        other => {
            warn!(kind = other, "unrecognised message type from daemon, ignoring");
        }
    }
    Ok(true)
}
