mod client;
mod detector;
mod protocol;
mod tmux;

use anyhow::{bail, Context, Result};
use detector::{AgentKind, IdleDetector};
use protocol::{
    AgentState, DaemonShutdown, Envelope, StatusRequest, StatusResponse, TaskAccept, TaskAssign,
    TaskComplete, UserIntervention, WrapperError, WrapperRegister, MSG_REGISTER, MSG_SHUTDOWN,
    MSG_STATUS_REQUEST, MSG_STATUS_RESPONSE, MSG_TASK_ACCEPT, MSG_TASK_ASSIGN, MSG_TASK_COMPLETE,
    MSG_ERROR, MSG_USER_INTERVENTION,
};
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{error, info, warn};

// ---- configuration constants ------------------------------------------------

const DAEMON_SOCK: &str = "/tmp/alor/daemon.sock";
const CAPTURE_LINES: usize = 50;
const POLL_INTERVAL_MS: u64 = 500;
/// Number of trailing lines to check for the idle pattern.
const IDLE_TAIL: usize = 5;
/// Idle pattern must be stable for this many seconds before marking complete.
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("alor_wrapper=info".parse().unwrap()),
        )
        .init();

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

    // First-launch setup on freshly-created sessions. Two independent
    // concerns, now split so the trust-ack runs for every wrapper-runtime
    // agent (not only those with a startup_file attached):
    //
    //   1. Trust-dialog auto-ack — always run. Each runtime CLI phrases
    //      its trust-folder dialog differently; per-kind `trust_prompt_hints()`
    //      / `trust_ack_key()` in detector.rs own the data. ClaudeCode
    //      and Default have empty hints, which make this block a no-op.
    //
    //   2. Briefing injection — only runs when `--startup-file` is set.
    //      Same "wait for prompt, read file, send-keys" flow as before.
    if freshly_created {
        // Boot grace — give the CLI time to render its initial screen,
        // including any trust dialog, before we start capturing.
        sleep(Duration::from_secs(3)).await;

        // Trust-dialog auto-ack (runs for every fresh session).
        let hints = kind.trust_prompt_hints();
        if !hints.is_empty() {
            match tmux::capture_pane(&session, 10) {
                Ok(lines) => {
                    let hit = lines
                        .iter()
                        .any(|l| hints.iter().any(|h| l.contains(h)));
                    if hit {
                        let ack = kind.trust_ack_key();
                        info!(ack, "detected trust prompt, sending ack key");
                        let _ = tmux::send_keys(&session, ack);
                        sleep(Duration::from_millis(500)).await;
                    }
                }
                Err(e) => warn!("initial capture failed: {e:#}"),
            }
        }

        // Briefing injection (runs only when a startup file is attached).
        if let Some(ref startup_path) = wargs.startup_file {
            info!(startup_path, "handling startup briefing...");

            // Wait for the actual input prompt (up to 15s).
            let mut ready = false;
            for i in 0..30 {
                if let Ok(lines) = tmux::capture_pane(&session, 20) {
                    let text = lines.join("\n");
                    // Look for common prompts: " > ", "❯", "$ ", or "% "
                    if text.contains(" > ") || text.contains("❯") || text.contains("$ ") || text.contains("% ") {
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
    let mut last_heartbeat = Instant::now();

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
                        idle_since = None;
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
                    // -- user intervention detection --
                    let tail: Vec<&str> = lines.iter().rev().take(IDLE_TAIL).map(|s| s.as_str()).collect();
                    let fingerprint = tail.join("\n");
                    let content_changed = prev_fingerprint.as_deref() != Some(&fingerprint);
                    prev_fingerprint = Some(fingerprint);

                    if content_changed {
                        let now = Instant::now();
                        let past_injection_grace = last_injection
                            .map(|t| now.duration_since(t).as_secs_f64() >= INJECTION_GRACE_SECS)
                            .unwrap_or(true);
                        let past_cooldown = last_intervention_sent
                            .map(|t| now.duration_since(t).as_secs_f64() >= INTERVENTION_COOLDOWN_SECS)
                            .unwrap_or(true);
                        let is_idle = detector.is_idle_tail(&lines, IDLE_TAIL);

                        if past_injection_grace && past_cooldown && !is_idle {
                            info!("detected user intervention in tmux pane");
                            let interv_env = Envelope::new(
                                MSG_USER_INTERVENTION,
                                UserIntervention { agent_id: agent.to_owned() },
                            )?;
                            client.send(&interv_env).await?;
                            last_intervention_sent = Some(now);
                        }
                    }

                    // -- idle detection --
                    // Skip idle detection during injection grace period to avoid
                    // false positives while the agent processes injected text.
                    let past_idle_grace = last_injection
                        .map(|t| Instant::now().duration_since(t).as_secs_f64() >= INJECTION_GRACE_SECS)
                        .unwrap_or(true);
                    if past_idle_grace && detector.is_idle_tail(&lines, IDLE_TAIL) {
                        let now = Instant::now();
                        let first_seen = *idle_since.get_or_insert(now);
                        let elapsed = now.duration_since(first_seen).as_secs_f64();

                        if elapsed >= IDLE_STABLE_SECS {
                            info!(
                                stable_for = format!("{elapsed:.1}s"),
                                "idle pattern stable, task complete"
                            );
                            *state = AgentState::Idle;
                            idle_since = None;
                            let complete_env = Envelope::new(
                                MSG_TASK_COMPLETE,
                                TaskComplete {
                                    task_id,
                                    summary: None,
                                    output: None,
                                },
                            )?;
                            client.send(&complete_env).await?;
                        }
                    } else {
                        // Output changed — reset the stability timer.
                        idle_since = None;
                    }
                }
                Err(e) => {
                    warn!("capture_pane error: {e:#}");
                }
            }
        }
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
