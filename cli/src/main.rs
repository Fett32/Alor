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
    name: Option<String> 
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
struct CliTaskGet { task_id: Uuid }

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
    Ok(serde_json::from_str(response_line.trim())?)
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
        Commands::Spawn { agent, role, name } => cmd_spawn(agent, role, name).await,
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
    let req = Envelope::new("cli.task.get", CliTaskGet { task_id })?;
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

async fn cmd_spawn(agent: String, role: String, name: Option<String>) -> Result<()> {
    let req = Envelope::new("cli.spawn", CliSpawn { agent, role, name })?;
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
