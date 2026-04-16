use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::daemon;

pub fn schema() -> Value {
    json!([
        {
            "name": "task_create",
            "description": "Create a new task for the Alor daemon to track. Returns a task_id. Use this to queue work before assigning to an agent.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Short task title" },
                    "description": { "type": "string", "description": "What the worker needs to do. Be specific — this is the worker's brief." },
                    "project": { "type": "string", "description": "Project name (e.g. 'Alor', 'MandaSpace'). Optional but strongly preferred — pulls in project profile + TASK BRIEF." }
                },
                "required": ["title", "description"]
            }
        },
        {
            "name": "task_assign",
            "description": "Assign an existing task to a worker agent. The agent must be connected. Use agent_list() first if unsure.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "description": "UUID returned by task_create" },
                    "agent_id": { "type": "string", "description": "Agent id from agent_list()" }
                },
                "required": ["task_id", "agent_id"]
            }
        },
        {
            "name": "task_get",
            "description": "Get full state of a task including proposal_brief, proposal_diff, user_intervened flag, state.",
            "input_schema": {
                "type": "object",
                "properties": { "task_id": { "type": "string" } },
                "required": ["task_id"]
            }
        },
        {
            "name": "task_list",
            "description": "List all tasks the daemon knows about. Use sparingly — prefer task_get by id when you have one.",
            "input_schema": { "type": "object", "properties": {} }
        },
        {
            "name": "task_cancel",
            "description": "Cancel a task. Use only when the user explicitly asks or the task is clearly obsolete.",
            "input_schema": {
                "type": "object",
                "properties": { "task_id": { "type": "string" } },
                "required": ["task_id"]
            }
        },
        {
            "name": "agent_list",
            "description": "List known worker agents with connection status. Returns agents (id, name, connected, tmux_session) and current tasks.",
            "input_schema": { "type": "object", "properties": {} }
        },
        {
            "name": "agent_send_message",
            "description": "Inject text into a connected agent's tmux pane. Use this to answer questions an agent has asked (after Fett approves the answer) or deliver follow-up instructions without creating a new task. Text is typed into the agent's session as keystrokes.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string" },
                    "text": { "type": "string", "description": "Exact text to send. Do NOT include a trailing newline unless you want to submit." }
                },
                "required": ["agent_id", "text"]
            }
        },
        {
            "name": "project_get",
            "description": "Read a project's profile: description, stack, root_dir, key_files, doc_paths, memory_index, memory_hub, notes. Use this to understand which worker + which brief to dispatch before creating a task.",
            "input_schema": {
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }
        },
        {
            "name": "project_list",
            "description": "List all project names the daemon knows about.",
            "input_schema": { "type": "object", "properties": {} }
        },
        {
            "name": "memory_get",
            "description": "Read the Memory Hub contents for a project — cross-agent shared notes. Useful when a task needs context that may already be captured from a prior session.",
            "input_schema": {
                "type": "object",
                "properties": { "project": { "type": "string" } },
                "required": ["project"]
            }
        }
    ])
}

pub async fn dispatch(name: &str, input: &Value) -> Result<String> {
    let result: Value = match name {
        "task_create" => {
            let title = input.get("title").and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("task_create: title required"))?;
            let description = input.get("description").and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("task_create: description required"))?;
            let project = input.get("project").and_then(|v| v.as_str());
            let id = daemon::task_create(title, description, project).await?;
            json!({ "task_id": id.to_string() })
        }
        "task_assign" => {
            let task_id = parse_uuid(input, "task_id")?;
            let agent_id = input.get("agent_id").and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("task_assign: agent_id required"))?;
            daemon::task_assign(task_id, agent_id).await?
        }
        "task_get" => {
            let task_id = parse_uuid(input, "task_id")?;
            daemon::task_get(task_id).await?
        }
        "task_list" => daemon::task_list().await?,
        "task_cancel" => {
            let task_id = parse_uuid(input, "task_id")?;
            daemon::task_cancel(task_id).await?
        }
        "agent_list" => daemon::status().await?,
        "agent_send_message" => {
            let agent_id = input.get("agent_id").and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("agent_send_message: agent_id required"))?;
            let text = input.get("text").and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("agent_send_message: text required"))?;
            daemon::agent_send_message(agent_id, text).await?
        }
        "project_get" => {
            let name = input.get("name").and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("project_get: name required"))?;
            daemon::project_get(name).await?
        }
        "project_list" => daemon::project_list().await?,
        "memory_get" => {
            let project = input.get("project").and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("memory_get: project required"))?;
            daemon::memory_get(project).await?
        }
        other => return Err(anyhow!("unknown tool: {}", other)),
    };
    Ok(serde_json::to_string(&result)?)
}

fn parse_uuid(input: &Value, field: &str) -> Result<Uuid> {
    let s = input.get(field).and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("{} required", field))?;
    Ok(Uuid::parse_str(s)?)
}
