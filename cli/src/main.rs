use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use uuid::Uuid;

const DAEMON_SOCKET: &str = "/tmp/alor/daemon.sock";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    correlation_id: Uuid,
    payload: serde_json::Value,
}

impl Envelope {
    fn new(kind: &str, payload: impl Serialize) -> Result<Self> {
        Ok(Self {
            kind: kind.to_string(),
            correlation_id: Uuid::new_v4(),
            payload: serde_json::to_value(payload)?,
        })
    }
}

#[derive(Serialize)]
struct CliStatusRequest {
    /// Daemon defaults to `view="summary"` which omits the `tasks`
    /// array entirely — breaks the CLI display which prints tasks.
    /// Request full explicitly to preserve pre-view-mode behaviour.
    view: &'static str,
}
#[derive(Serialize)]
struct CliTaskList {}
#[derive(Serialize)]
struct CliTaskCreate {
    title: String,
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
}
#[derive(Serialize)]
struct CliTaskCancel { task_id: Uuid }
#[derive(Serialize)]
struct CliTaskComplete {
    task_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
}
#[derive(Serialize)]
struct CliSpawn {
    agent: String,
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// Runtime project override for template spawns. Daemon's
    /// `CliSpawn.project` (`src-tauri/src/wrapper/protocol.rs`) maps
    /// directly; when set alongside a template `agent`, handle_spawn
    /// derives the instance id `<agent>-<project>` (e.g. cursor +
    /// alor → cursor-alor). Without this (or `working_dir`), template
    /// spawns fail with "template needs override" because bare
    /// templates can't be instantiated. Unset here when spawning a
    /// fixed yaml slot or a non-template agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    /// Runtime working-dir override. Same parameterization role as
    /// `project`; either is sufficient to satisfy the daemon's
    /// template guard. Prefer passing both when available so the
    /// spawned instance lands in the right workdir AND gets the
    /// project label for auto-distill routing.
    #[serde(skip_serializing_if = "Option::is_none")]
    working_dir: Option<String>,
}
#[derive(Serialize)]
struct CliKill { instance: String }
#[derive(Serialize)]
struct CliProjectGet { name: String }
#[derive(Serialize)]
struct CliProjectSave {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stack: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    root_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key_files: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    doc_paths: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_index: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_agent: Option<String>,
}
#[derive(Serialize)]
struct CliAssign { task_id: Uuid, agent_id: String }
#[derive(Serialize)]
struct CliTaskGet {
    task_id: Uuid,
    // alor-cli renders description in its single-task printout
    // (see cmd_task_get). Server default is now `view="summary"`
    // (bloat fix #2, commit after 7aec211) which drops description
    // to a `has_description` boolean. CLI opts into `view="full"`
    // so the TaskInfo deserialization still finds the description
    // string. Orchestrator callers don't set this; they get the
    // cheaper default.
    view: &'static str,
}

#[derive(Deserialize)]
struct AgentInfo { id: String, name: String, connected: bool, tmux_session: Option<String> }
// `description` is absent in the summary-view response (the default for
// cli.task.list as of DEFAULT_TASK_LIST_VIEW). `#[serde(default)]`
// lets the same struct deserialize either shape; `task_get` still
// returns a full Task so description is populated there.
#[derive(Deserialize)]
struct TaskInfo {
    id: Uuid,
    title: String,
    state: String,
    assigned_to: Option<String>,
    #[serde(default)]
    description: String,
}
#[derive(Deserialize)]
struct StatusResponsePayload { agents: Vec<AgentInfo>, tasks: Vec<TaskInfo> }
#[derive(Deserialize)]
struct TaskResponsePayload { task: TaskInfo }
#[derive(Deserialize)]
struct TaskListResponsePayload { tasks: Vec<TaskInfo> }
#[derive(Deserialize)]
struct SpawnResponsePayload { spawned: String, pid: u32 }
#[derive(Deserialize)]
struct ErrorResponsePayload { error: String }

#[derive(Parser)]
#[command(name = "alor", about = "Alor multi-agent orchestrator CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Init,
    Status,
    Task { #[command(subcommand)] action: TaskAction },
    Project { #[command(subcommand)] action: ProjectAction },
    Assign { task_id: Uuid, agent_id: String },
    Spawn {
        agent: String,
        #[arg(long, default_value = "impl")]
        role: String,
        #[arg(long)]
        name: Option<String>,
        /// Project label for template spawns. Derives instance id
        /// `<agent>-<project>` and seeds the spawned instance's
        /// `project` field so auto-distill routes its task.complete
        /// summaries into the right hub. Required (or `--working-dir`)
        /// when `agent` is a template yaml like `claude` / `cursor` /
        /// `codex` / `gemini`; ignored for fixed yaml slots.
        #[arg(long)]
        project: Option<String>,
        /// Working directory for the spawned instance. Alternative (or
        /// complement) to `--project` for satisfying the daemon's
        /// template-override requirement. Pass both when available so
        /// the agent lands in the right workdir AND the project label
        /// flows through to auto-distill / memory hub routing.
        #[arg(long = "working-dir")]
        working_dir: Option<String>,
    },
    Event { #[command(subcommand)] action: EventAction },
    Kill { instance: String },
    Integrations,
}

#[derive(Subcommand)]
enum TaskAction {
    List,
    Get { task_id: Uuid },
    Create {
        title: String,
        description: String,
        #[arg(long)] project: Option<String>,
    },
    Cancel { task_id: Uuid },
    Complete {
        task_id: Uuid,
        /// Optional close-out summary. Handy when retroactively completing
        /// a Cancelled task (e.g. `--summary "shipped in commit X"`).
        #[arg(long)]
        summary: Option<String>,
    },
}

#[derive(Subcommand)]
enum ProjectAction {
    List,
    Get { name: String },
    Save {
        name: String,
        #[arg(long)] description: Option<String>,
        #[arg(long)] root_dir: Option<String>,
        #[arg(long, value_delimiter = ',')] stack: Option<Vec<String>>,
        #[arg(long, value_delimiter = ',')] key_files: Option<Vec<String>>,
        #[arg(long, value_delimiter = ',')] doc_paths: Option<Vec<String>>,
        #[arg(long)] memory_index: Option<String>,
        #[arg(long)] memory_agent: Option<String>,
    },
}

#[derive(Subcommand)]
enum EventAction { Stream }

const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

async fn send_request(envelope: &Envelope) -> Result<Envelope> {
    tokio::time::timeout(REQUEST_TIMEOUT, send_request_inner(envelope))
        .await
        .map_err(|_| anyhow::anyhow!("request timed out after {}s", REQUEST_TIMEOUT.as_secs()))?
}

async fn send_request_inner(envelope: &Envelope) -> Result<Envelope> {
    let stream = UnixStream::connect(DAEMON_SOCKET).await?;
    let (read_half, mut write_half) = stream.into_split();
    let mut line = serde_json::to_string(envelope)?;
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    let mut reader = BufReader::new(read_half);
    let mut response_line = String::new();
    let n = reader.read_line(&mut response_line).await?;
    if n == 0 { anyhow::bail!("daemon closed connection"); }
    let resp: Envelope = serde_json::from_str(response_line.trim())?;
    reject_error_envelope(resp)
}

/// Convert a daemon error envelope (`cli.error` / `error`) into a Rust
/// `Err`, so command-specific success-payload deserialization sees only
/// success envelopes and doesn't misreport structured daemon errors
/// (e.g. "no config found for agent 'cursor-alor'") as generic serde
/// field-missing gibberish ("missing field `spawned`"). Mirrors the
/// Python client's handling in `orchestrator-py/daemon.py::_one_shot`
/// so every CLI surface — Rust `alor-cli` and Python orchestrator —
/// lifts daemon errors to typed caller-visible exceptions the same way.
fn reject_error_envelope(env: Envelope) -> Result<Envelope> {
    if env.kind == "cli.error" || env.kind == "error" {
        // Try to pull `payload.error` (the documented shape — see
        // `src-tauri/src/wrapper/protocol.rs::MSG_CLI_ERROR` docstring).
        // Fall back to a generic message + the raw payload when the
        // shape is unexpected, so we never mask a malformed-error with
        // a useless parse panic.
        let message = match serde_json::from_value::<ErrorResponsePayload>(env.payload.clone()) {
            Ok(e) => e.error,
            Err(_) => format!("daemon error (unparseable payload): {}", env.payload),
        };
        anyhow::bail!("{message}");
    }
    Ok(env)
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Commands::Init => cmd_init().await,
        Commands::Status => cmd_status().await,
        Commands::Task { action } => match action {
            TaskAction::List => cmd_task_list().await,
            TaskAction::Get { task_id } => cmd_task_get(task_id).await,
            TaskAction::Create { title, description, project } => cmd_task_create(title, description, project).await,
            TaskAction::Cancel { task_id } => cmd_task_cancel(task_id).await,
            TaskAction::Complete { task_id, summary } => cmd_task_complete(task_id, summary).await,
        },
        Commands::Project { action } => match action {
            ProjectAction::List => cmd_project_list().await,
            ProjectAction::Get { name } => cmd_project_get(name).await,
            ProjectAction::Save { name, description, root_dir, stack, key_files, doc_paths, memory_index, memory_agent } => {
                cmd_project_save(name, description, root_dir, stack, key_files, doc_paths, memory_index, memory_agent).await
            }
        },
        Commands::Assign { task_id, agent_id } => cmd_assign(task_id, agent_id).await,
        Commands::Event { action } => match action {
            EventAction::Stream => cmd_event_stream().await,
        },
        Commands::Spawn { agent, role, name, project, working_dir } => {
            cmd_spawn(agent, role, name, project, working_dir).await
        }
        Commands::Kill { instance } => cmd_kill(instance).await,
        Commands::Integrations => cmd_integrations().await,
    };
    if let Err(e) = result {
        eprintln!("alor: {e:#}");
        std::process::exit(1);
    }
}

async fn cmd_init() -> Result<()> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let home_path = std::path::PathBuf::from(home);
    let config_root = home_path.join(".config/alor");
    let data_root = home_path.join(".local/share/alor");
    let dirs = [
        config_root.join("alor-main/agents"),
        data_root.join("projects"),
        data_root.join("hubs"),
        data_root.join("alor-orchestrator/orchestrator"),
    ];
    for dir in &dirs {
        if !dir.exists() {
            println!("Creating {}...", dir.display());
            std::fs::create_dir_all(dir)?;
        }
    }
    println!("\nAlor directories initialized successfully.");
    Ok(())
}

async fn cmd_status() -> Result<()> {
    let req = Envelope::new("cli.status", CliStatusRequest { view: "full" })?;
    let resp = send_request(&req).await?;
    let status: StatusResponsePayload = serde_json::from_value(resp.payload)?;
    println!("=== Agents ===");
    for agent in &status.agents {
        let conn = if agent.connected { "connected" } else { "disconnected" };
        println!("  {} ({}) [{}] tmux:{}", agent.id, agent.name, conn, agent.tmux_session.as_deref().unwrap_or("-"));
    }
    println!("\n=== Tasks ===");
    for task in &status.tasks {
        let assignee = task.assigned_to.as_deref().unwrap_or("unassigned");
        println!("  {} [{}] {} → {}", task.id, task.state, task.title, assignee);
    }
    Ok(())
}

async fn cmd_task_list() -> Result<()> {
    let req = Envelope::new("cli.task.list", CliTaskList {})?;
    let resp = send_request(&req).await?;
    let list: TaskListResponsePayload = serde_json::from_value(resp.payload)?;
    for task in &list.tasks {
        let assignee = task.assigned_to.as_deref().unwrap_or("unassigned");
        println!("{} [{}] {} → {}", task.id, task.state, task.title, assignee);
    }
    Ok(())
}

async fn cmd_task_get(task_id: Uuid) -> Result<()> {
    let req = Envelope::new(
        "cli.task.get",
        CliTaskGet {
            task_id,
            view: "full",
        },
    )?;
    let resp = send_request(&req).await?;
    let info: TaskResponsePayload = serde_json::from_value(resp.payload)?;
    println!("ID:          {}\nTitle:       {}\nState:       {}\nDescription: {}", info.task.id, info.task.title, info.task.state, info.task.description);
    Ok(())
}

async fn cmd_task_create(title: String, description: String, project: Option<String>) -> Result<()> {
    let req = Envelope::new("cli.task.create", CliTaskCreate { title, description, project })?;
    let resp = send_request(&req).await?;
    println!("Created task: {}", resp.payload.get("task_id").and_then(|v| v.as_str()).unwrap_or("?"));
    Ok(())
}

async fn cmd_task_cancel(task_id: Uuid) -> Result<()> {
    let req = Envelope::new("cli.task.cancel", CliTaskCancel { task_id })?;
    send_request(&req).await?;
    println!("Task cancelled.");
    Ok(())
}

async fn cmd_task_complete(task_id: Uuid, summary: Option<String>) -> Result<()> {
    let req = Envelope::new("cli.task.complete", CliTaskComplete { task_id, summary })?;
    send_request(&req).await?;
    println!("Task completed.");
    Ok(())
}

async fn cmd_assign(task_id: Uuid, agent_id: String) -> Result<()> {
    let req = Envelope::new("cli.assign", CliAssign { task_id, agent_id })?;
    send_request(&req).await?;
    println!("Task assigned.");
    Ok(())
}

async fn cmd_project_list() -> Result<()> {
    let req = Envelope::new("cli.project.list", serde_json::json!({}))?;
    let resp = send_request(&req).await?;
    let projects = resp.payload.get("projects").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    for p in &projects { println!("  {}", p.as_str().unwrap_or("?")); }
    Ok(())
}

async fn cmd_project_get(name: String) -> Result<()> {
    let req = Envelope::new("cli.project.get", CliProjectGet { name })?;
    let resp = send_request(&req).await?;
    println!("{}", serde_json::to_string_pretty(&resp.payload)?);
    Ok(())
}

async fn cmd_project_save(
    name: String,
    description: Option<String>,
    root_dir: Option<String>,
    stack: Option<Vec<String>>,
    key_files: Option<Vec<String>>,
    doc_paths: Option<Vec<String>>,
    memory_index: Option<String>,
    memory_agent: Option<String>,
) -> Result<()> {
    let req = Envelope::new("cli.project.save", CliProjectSave {
        name: name.clone(),
        description,
        stack,
        root_dir,
        key_files,
        doc_paths,
        memory_index,
        memory_agent,
    })?;
    let resp = send_request(&req).await?;
    println!("Project saved to {}", resp.payload.get("path").and_then(|v| v.as_str()).unwrap_or("?"));
    Ok(())
}

async fn cmd_event_stream() -> Result<()> {
    let stream = UnixStream::connect(DAEMON_SOCKET).await?;
    let (read_half, mut write_half) = stream.into_split();
    let req = Envelope::new("cli.event.stream", serde_json::json!({}))?;
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    let mut reader = BufReader::new(read_half);
    let mut event_line = String::new();
    loop {
        event_line.clear();
        let n = reader.read_line(&mut event_line).await?;
        if n == 0 { break; }
        print!("{}", event_line);
    }
    Ok(())
}

async fn cmd_spawn(
    agent: String,
    role: String,
    name: Option<String>,
    project: Option<String>,
    working_dir: Option<String>,
) -> Result<()> {
    let req = Envelope::new(
        "cli.spawn",
        CliSpawn { agent, role, name, project, working_dir },
    )?;
    let resp = send_request(&req).await?;
    let info: SpawnResponsePayload = serde_json::from_value(resp.payload)?;
    println!("Spawned instance: {} (pid {})", info.spawned, info.pid);
    Ok(())
}

async fn cmd_kill(instance: String) -> Result<()> {
    let req = Envelope::new("cli.kill", CliKill { instance })?;
    send_request(&req).await?;
    println!("Killed instance.");
    Ok(())
}

async fn cmd_integrations() -> Result<()> {
    let req = Envelope::new("cli.integrations.get", serde_json::json!({}))?;
    let resp = send_request(&req).await?;
    println!("{}", serde_json::to_string_pretty(&resp.payload)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(kind: &str, payload: serde_json::Value) -> Envelope {
        Envelope {
            kind: kind.to_string(),
            correlation_id: Uuid::nil(),
            payload,
        }
    }

    #[test]
    fn success_envelope_passes_through() {
        let e = env("cli.response", serde_json::json!({"spawned": "cursor-alor", "pid": 12345}));
        let got = reject_error_envelope(e).expect("success envelope should pass through");
        assert_eq!(got.kind, "cli.response");
        assert_eq!(got.payload["spawned"], "cursor-alor");
    }

    #[test]
    fn cli_error_envelope_surfaces_error_string() {
        // Regression for T8: pre-fix the CLI tried to deserialize a
        // `cli.error` payload `{"error": "no config found for agent
        // 'cursor-alor'"}` as `SpawnResponsePayload { spawned, pid }`
        // and reported "missing field `spawned`" — hiding the real
        // daemon message. Post-fix the actual error prose is surfaced.
        let e = env(
            "cli.error",
            serde_json::json!({"error": "no config found for agent 'cursor-alor'"}),
        );
        let err = reject_error_envelope(e).expect_err("cli.error should be rejected");
        assert_eq!(err.to_string(), "no config found for agent 'cursor-alor'");
    }

    #[test]
    fn legacy_error_kind_also_rejected() {
        // Back-compat: the daemon's `cli_error` emits `cli.error`, but
        // `orchestrator-py/daemon.py` also accepts the bare `error`
        // kind for older callers. Match that tolerance on the Rust
        // side so both clients behave identically against any daemon
        // rev that happens to still emit the legacy shape.
        let e = env("error", serde_json::json!({"error": "stale-daemon legacy error"}));
        let err = reject_error_envelope(e).expect_err("legacy error kind should be rejected");
        assert!(err.to_string().contains("stale-daemon legacy error"));
    }

    #[test]
    fn malformed_error_payload_still_errs_without_panic() {
        // If the daemon ever emits a malformed `cli.error` (no `error`
        // field, or a non-object payload), the CLI must still bail
        // loudly — not panic on unwrap, and not silently succeed. The
        // message falls back to showing the raw payload so a user can
        // diagnose the drift.
        let e = env("cli.error", serde_json::json!({"unexpected": "shape"}));
        let err = reject_error_envelope(e).expect_err("malformed error payload should still bail");
        let s = err.to_string();
        assert!(s.contains("daemon error") && s.contains("unexpected"),
            "fallback should mention it's a daemon error and dump the payload: {s}");
    }

    #[test]
    fn coded_cli_error_surfaces_human_prose_not_code() {
        // Coded errors (ERR_CODE_* in the daemon protocol) carry both
        // `code` and `error`. The CLI surfaces `error` — the human
        // prose — to match what the Python client shows users. `code`
        // would be useful for typed exception routing but the CLI
        // doesn't have per-error-type handling, so prose wins.
        let e = env(
            "cli.error",
            serde_json::json!({
                "code": "framed_send_not_supported",
                "error": "wrapper runtime can't decode BEGIN/END framing",
            }),
        );
        let err = reject_error_envelope(e).expect_err("coded error should still be rejected");
        assert_eq!(err.to_string(), "wrapper runtime can't decode BEGIN/END framing");
    }
}
