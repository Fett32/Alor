use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use uuid::Uuid;

pub const DAEMON_SOCKET: &str = "/tmp/alor/daemon.sock";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    #[serde(rename = "type")]
    pub kind: String,
    pub correlation_id: Uuid,
    pub payload: Value,
}

impl Envelope {
    pub fn new(kind: &str, payload: impl Serialize) -> Result<Self> {
        Ok(Self {
            kind: kind.to_string(),
            correlation_id: Uuid::new_v4(),
            payload: serde_json::to_value(payload)?,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct EventMessage {
    pub event: String,
    pub data: Value,
    #[serde(default)]
    pub timestamp: Option<String>,
}

pub async fn send_request(envelope: &Envelope) -> Result<Envelope> {
    tokio::time::timeout(REQUEST_TIMEOUT, send_request_inner(envelope))
        .await
        .map_err(|_| anyhow!("daemon request timed out after {}s", REQUEST_TIMEOUT.as_secs()))?
}

async fn send_request_inner(envelope: &Envelope) -> Result<Envelope> {
    let stream = UnixStream::connect(DAEMON_SOCKET)
        .await
        .with_context(|| format!("connect {}", DAEMON_SOCKET))?;
    let (read_half, mut write_half) = stream.into_split();
    let mut line = serde_json::to_string(envelope)?;
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    let mut reader = BufReader::new(read_half);
    let mut response_line = String::new();
    let n = reader.read_line(&mut response_line).await?;
    if n == 0 {
        return Err(anyhow!("daemon closed connection without response"));
    }
    let resp: Envelope = serde_json::from_str(response_line.trim())?;
    if resp.kind == "error" || resp.kind == "cli.error" {
        let err = resp
            .payload
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown daemon error");
        return Err(anyhow!("daemon error: {}", err));
    }
    Ok(resp)
}

pub async fn task_create(
    title: &str,
    description: &str,
    project: Option<&str>,
) -> Result<Uuid> {
    let env = Envelope::new(
        "cli.task.create",
        json!({
            "title": title,
            "description": description,
            "project": project,
        }),
    )?;
    let resp = send_request(&env).await?;
    let id_str = resp
        .payload
        .get("task_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("task.create: missing task_id"))?;
    Ok(Uuid::parse_str(id_str)?)
}

pub async fn task_assign(task_id: Uuid, agent_id: &str) -> Result<Value> {
    let env = Envelope::new(
        "cli.assign",
        json!({ "task_id": task_id, "agent_id": agent_id }),
    )?;
    Ok(send_request(&env).await?.payload)
}

pub async fn task_get(task_id: Uuid) -> Result<Value> {
    let env = Envelope::new("cli.task.get", json!({ "task_id": task_id }))?;
    Ok(send_request(&env).await?.payload)
}

pub async fn task_list() -> Result<Value> {
    let env = Envelope::new("cli.task.list", json!({}))?;
    Ok(send_request(&env).await?.payload)
}

pub async fn task_cancel(task_id: Uuid) -> Result<Value> {
    let env = Envelope::new("cli.task.cancel", json!({ "task_id": task_id }))?;
    Ok(send_request(&env).await?.payload)
}

pub async fn status() -> Result<Value> {
    let env = Envelope::new("cli.status", json!({}))?;
    Ok(send_request(&env).await?.payload)
}

pub async fn project_get(name: &str) -> Result<Value> {
    let env = Envelope::new("cli.project.get", json!({ "name": name }))?;
    Ok(send_request(&env).await?.payload)
}

pub async fn project_list() -> Result<Value> {
    let env = Envelope::new("cli.project.list", json!({}))?;
    Ok(send_request(&env).await?.payload)
}

pub async fn agent_send_message(agent_id: &str, text: &str) -> Result<Value> {
    let env = Envelope::new(
        "cli.agent.send_message",
        json!({ "agent_id": agent_id, "text": text }),
    )?;
    Ok(send_request(&env).await?.payload)
}

pub async fn memory_get(project: &str) -> Result<Value> {
    let env = Envelope::new("cli.memory.get", json!({ "project": project }))?;
    Ok(send_request(&env).await?.payload)
}

pub fn spawn_event_stream() -> mpsc::Receiver<EventMessage> {
    let (tx, rx) = mpsc::channel::<EventMessage>(64);
    tokio::spawn(async move {
        loop {
            if let Err(e) = run_event_stream(&tx).await {
                eprintln!("[orchestrator] event stream error: {e:#}; reconnecting in 2s");
                tokio::time::sleep(Duration::from_secs(2)).await;
            } else {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            if tx.is_closed() {
                break;
            }
        }
    });
    rx
}

async fn run_event_stream(tx: &mpsc::Sender<EventMessage>) -> Result<()> {
    let stream = UnixStream::connect(DAEMON_SOCKET).await?;
    let (read_half, mut write_half) = stream.into_split();
    let env = Envelope::new("cli.event.stream", json!({}))?;
    let mut line = serde_json::to_string(&env)?;
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    let mut reader = BufReader::new(read_half);
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader.read_line(&mut buf).await?;
        if n == 0 {
            return Err(anyhow!("event stream closed by daemon"));
        }
        let env: Envelope = match serde_json::from_str(buf.trim()) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let evt: EventMessage = match serde_json::from_value(env.payload) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if tx.send(evt).await.is_err() {
            return Ok(());
        }
    }
}
