mod api;
mod daemon;
mod tools;

use anyhow::{Context, Result};
use api::{ContentBlock, Message, Usage};
use std::path::PathBuf;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const DEFAULT_PROMPT_PATH: &str = ".config/alor/orchestrator_prompt.md";
const MAX_TOOL_ITERATIONS: usize = 20;

#[tokio::main]
async fn main() -> Result<()> {
    let system_prompt = load_system_prompt()?;
    let client = api::Client::from_env()?;
    let tool_schema = tools::schema();

    let mut events = daemon::spawn_event_stream();
    let session_start = Instant::now();
    let mut session_usage = Usage::default();
    let mut history: Vec<Message> = Vec::new();

    print_banner(&client);

    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    let mut line = String::new();

    loop {
        stdout.write_all(b"\x1b[1;36morch>\x1b[0m ").await.ok();
        stdout.flush().await.ok();

        line.clear();
        let input = tokio::select! {
            read = stdin.read_line(&mut line) => {
                let n = read?;
                if n == 0 {
                    println!();
                    break;
                }
                let trimmed = line.trim().to_string();
                if trimmed.is_empty() { continue; }
                if trimmed == "/quit" || trimmed == "/exit" { break; }
                if trimmed == "/reset" {
                    history.clear();
                    println!("[conversation reset]");
                    continue;
                }
                if trimmed == "/usage" {
                    print_footer(&session_usage, session_start);
                    continue;
                }
                Some(trimmed)
            }
            evt = events.recv() => {
                match evt {
                    Some(e) => {
                        print_event(&e);
                        None
                    }
                    None => { println!("[event stream closed]"); break; }
                }
            }
        };

        let Some(user_text) = input else { continue; };

        history.push(Message::user_text(user_text));

        match run_turn(&client, &system_prompt, &tool_schema, &mut history, &mut session_usage).await {
            Ok(()) => {}
            Err(e) => {
                eprintln!("[error] {e:#}");
                if let Some(last) = history.last() {
                    if last.role == "user" {
                        history.pop();
                    }
                }
            }
        }

        print_footer(&session_usage, session_start);
    }

    Ok(())
}

fn load_system_prompt() -> Result<String> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let path: PathBuf = [&home, DEFAULT_PROMPT_PATH].iter().collect();
    std::fs::read_to_string(&path)
        .with_context(|| format!("read system prompt from {}", path.display()))
}

fn print_banner(client: &api::Client) {
    println!("\x1b[1;35m╭─ Alor Orchestrator ─────────────────────────────╮\x1b[0m");
    println!("\x1b[1;35m│\x1b[0m model: {:<40} \x1b[1;35m│\x1b[0m", client.model);
    println!("\x1b[1;35m│\x1b[0m commands: /reset  /usage  /quit             \x1b[1;35m│\x1b[0m");
    println!("\x1b[1;35m╰─────────────────────────────────────────────────╯\x1b[0m");
}

fn print_event(e: &daemon::EventMessage) {
    let ts = e.timestamp.as_deref().unwrap_or("");
    println!("\x1b[2m[event {} {}] {}\x1b[0m", ts, e.event, e.data);
}

fn print_footer(u: &Usage, start: Instant) {
    let elapsed = start.elapsed().as_secs();
    let (h, m, s) = (elapsed / 3600, (elapsed % 3600) / 60, elapsed % 60);
    let cost = api::estimate_cost_usd(u);
    println!(
        "\x1b[2m  in {} · out {} · cache_w {} · cache_r {} · ${:.4} · {:02}:{:02}:{:02}\x1b[0m",
        u.input_tokens,
        u.output_tokens,
        u.cache_creation_input_tokens,
        u.cache_read_input_tokens,
        cost,
        h, m, s
    );
}

async fn run_turn(
    client: &api::Client,
    system: &str,
    tools: &serde_json::Value,
    history: &mut Vec<Message>,
    session_usage: &mut Usage,
) -> Result<()> {
    for _ in 0..MAX_TOOL_ITERATIONS {
        let resp = client.complete(system, history, Some(tools)).await?;
        session_usage.add(&resp.usage);

        let mut tool_uses = Vec::new();
        for block in &resp.content {
            match block {
                ContentBlock::Text { text } => {
                    println!("{}", text);
                }
                ContentBlock::ToolUse { id, name, input } => {
                    tool_uses.push((id.clone(), name.clone(), input.clone()));
                }
                ContentBlock::ToolResult { .. } => {}
            }
        }

        history.push(Message::assistant(resp.content.clone()));

        if tool_uses.is_empty() {
            return Ok(());
        }

        let mut tool_results = Vec::with_capacity(tool_uses.len());
        for (id, name, input) in tool_uses {
            println!("\x1b[2m  → {}({})\x1b[0m", name, input);
            let (content, is_error) = match tools::dispatch(&name, &input).await {
                Ok(s) => (s, false),
                Err(e) => (format!("error: {e:#}"), true),
            };
            tool_results.push(ContentBlock::ToolResult {
                tool_use_id: id,
                content,
                is_error,
            });
        }
        history.push(Message::user_tool_results(tool_results));

        if resp.stop_reason.as_deref() != Some("tool_use") {
            return Ok(());
        }
    }
    Err(anyhow::anyhow!("tool-use loop exceeded {} iterations", MAX_TOOL_ITERATIONS))
}
