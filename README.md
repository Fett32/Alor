# Vaelkor

Multi-agent orchestration workspace. Run Claude, Codex, and other AI agents in parallel with a unified task management UI.

## Dependencies

- Python 3.11+
- tmux (`sudo apt install tmux`)
- PySide6, pyte, PyYAML (installed via pip)

## Quick Start

```bash
# Install tmux first
sudo apt install tmux

# Install Python package
cd ~/Projects/vaelkor
python -m venv .venv
.venv/bin/pip install -e .

# Run the UI
./run.sh ui
```

### Using the UI

1. **Start a wrapper**: Click "Start" next to an agent to launch its wrapper in a new terminal
2. **Connect**: Click "Connect" to link the daemon to the running wrapper
3. **Create tasks**: Click "+ New Task" to assign work to an agent
4. **Monitor**: Watch the terminal panel and task list for progress
5. **Completion**: Tasks automatically mark complete when agents return to idle

### Running Components Separately

```bash
./run.sh wrapper claude  # Start Claude wrapper (terminal 1)
./run.sh wrapper codex   # Start Codex wrapper (terminal 2)  
./run.sh daemon          # Run daemon with interactive CLI (terminal 3)
./run.sh ui              # Run just the UI
./run.sh test            # Run tests
```

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  Vaelkor UI (PySide6, beskar-themed)                        │
│  - Task list with live status (○ assigned, ● active, ✓ done)│
│  - Agent panel with Start/Connect buttons                   │
│  - Embedded terminal (pyte + PTY, attaches to tmux)         │
│  - Input modes: override, chat, task                        │
└─────────────────────────────────────────────────────────────┘
                              │ Qt signals
                              ▼
┌─────────────────────────────────────────────────────────────┐
│  Controller (runs daemon in background thread)              │
└─────────────────────────────────────────────────────────────┘
                              │ asyncio
                              ▼
┌─────────────────────────────────────────────────────────────┐
│  Control Daemon                                             │
│  - Task state machine (ASSIGNED→ACCEPTED→COMPLETED)         │
│  - Heartbeat polling (5s) detects task completions          │
│  - Session persistence (~/.local/share/vaelkor/)            │
│  - Message routing with delivery tracking                   │
└─────────────────────────────────────────────────────────────┘
                              │ unix sockets (JSON-newline)
           ┌──────────────────┼──────────────────┐
           ▼                  ▼                  ▼
     ┌──────────┐       ┌──────────┐       ┌──────────┐
     │ Wrapper  │       │ Wrapper  │       │ Wrapper  │
     │ (claude) │       │ (codex)  │       │ (cursor) │
     └──────────┘       └──────────┘       └──────────┘
           │                  │                  │
           │ tmux send-keys   │                  │
           ▼                  ▼                  ▼
     ┌──────────┐       ┌──────────┐       ┌──────────┐
     │  tmux    │       │  tmux    │       │  tmux    │
     │ session  │       │ session  │       │ session  │
     └──────────┘       └──────────┘       └──────────┘
```

## Task Flow

1. **User creates task** → UI sends to controller
2. **Controller** → daemon.assign_task()
3. **Daemon** → creates Task(state=ASSIGNED), sends WRAPPER_SEND_TASK to wrapper
4. **Wrapper** → builds prompt with constraints, injects into tmux via send-keys
5. **Agent works** → wrapper monitors output every 2s
6. **Agent idles** → wrapper detects prompt pattern, queues completion
7. **Daemon heartbeat** → polls wrapper status, receives completed_tasks
8. **Daemon** → updates Task(state=COMPLETED), triggers callback
9. **Controller** → emits task_updated signal
10. **UI** → updates task icon (○→✓), shows status message

## Task States

```
ASSIGNED ──┬── task.accept ──→ ACCEPTED ──┬── task.result ──→ COMPLETED
           │                              ├── task.blocked ──→ BLOCKED
           ├── task.reject ──→ REJECTED   ├── task.cancel ──→ CANCELLED
           └── timeout ──→ TIMED_OUT      └── interrupt ──→ INTERRUPTED
                                                              │
                                          recovery ───────────┴──→ STALE
```

## Config

```
~/.config/vaelkor/
├── vaelkor.yaml              # Main config
└── agents/
    ├── claude.yaml           # identity: *, role: orchestrator
    └── codex.yaml            # identity: ?, role: reviewer

~/.local/share/vaelkor/
├── sessions/{session-id}/
│   ├── state.json            # Live session state
│   └── transcripts/          # Agent output logs
└── last_session              # Recovery pointer
```

### Agent Config Example

```yaml
# ~/.config/vaelkor/agents/codex.yaml
identity: "?"
role: reviewer
command:
  - codex
autostart: false
constraints:
  - no_file_edits
  - review_only
```

## Project Structure

```
src/vaelkor/
├── main.py                   # UI entry point
├── config.py                 # YAML config loading
├── daemon/
│   ├── core.py               # Daemon, heartbeat, task routing
│   ├── state.py              # Task/Session state, persistence
│   └── cli.py                # Interactive daemon CLI
├── wrapper/
│   ├── base.py               # AgentWrapper, completion detection
│   └── cli.py                # Standalone wrapper runner
├── protocol/
│   └── messages.py           # Message types, JSON serialization
└── ui/
    ├── main_window.py        # Qt window, task list, terminal
    └── controller.py         # Bridges daemon thread ↔ Qt
```

## V1 Scope

- Claude + Codex agents (two agents)
- Explicit manual task routing (user confirms all handoffs)
- Agents instructed not to edit files (proposals only, not enforced)
- Automatic completion detection via idle pattern matching
- Session persistence and crash recovery

## V2 Ideas

- Cursor agent integration
- Auto-routing with user-defined rules
- Git worktrees for true file isolation per agent
- Enforced sandboxing (read-only mounts)
- MandaSpace integration

## Development

```bash
# Run tests
./run.sh test

# Tests require tmux - integration tests skip gracefully if not installed
```

## Protocol

Daemon ↔ Wrapper communication uses newline-delimited JSON over unix sockets.

**Message envelope:**
```json
{
  "id": "msg-abc123",
  "type": "task.assign",
  "from_agent": "orchestrator", 
  "to_agent": "codex",
  "task_id": "task-001",
  "timestamp": "2026-04-12T19:10:00+00:00",
  "summary": "Review auth flow",
  "body": { "scope": ["src/auth/*"], "constraints": ["review_only"] }
}
```

**Key message types:**
- `task.assign`, `task.accept`, `task.result` - task lifecycle
- `wrapper.send_task`, `wrapper.get_status` - daemon → wrapper
- `wrapper.status`, `wrapper.task_complete` - wrapper → daemon
