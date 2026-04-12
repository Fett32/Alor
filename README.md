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

# Run UI
./run.sh ui

# Or run components separately:
./run.sh wrapper claude  # Start Claude wrapper (in terminal 1)
./run.sh wrapper codex   # Start Codex wrapper (in terminal 2)
./run.sh daemon          # Run daemon with CLI (in terminal 3)
```

## Architecture

```
┌─────────────────────────────────────────────────┐
│  Vaelkor UI (PySide6)                           │
│  - Task list (primary view)                     │
│  - Agent status with connect/start buttons      │
│  - Embedded terminal view                       │
└─────────────────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────┐
│  Control Daemon                                 │
│  - Message routing                              │
│  - Task state machine                           │
│  - Session persistence                          │
└─────────────────────────────────────────────────┘
                         │
          ┌──────────────┼──────────────┐
          ▼              ▼              ▼
    ┌──────────┐   ┌──────────┐   ┌──────────┐
    │ Wrapper  │   │ Wrapper  │   │ Wrapper  │
    │ (claude) │   │ (codex)  │   │ (cursor) │
    └──────────┘   └──────────┘   └──────────┘
          │              │              │
          ▼              ▼              ▼
       tmux           tmux           tmux
```

## Config

- Main config: `~/.config/vaelkor/vaelkor.yaml`
- Agent configs: `~/.config/vaelkor/agents/*.yaml`
- Session data: `~/.local/share/vaelkor/sessions/`

## V1 Scope

- Claude + Codex agents
- Explicit manual task routing
- Read-only proposals (agents instructed not to edit)
- Task lifecycle: ASSIGNED → ACCEPTED → COMPLETED
- Session persistence and recovery

## Development

```bash
./run.sh test  # Run tests
```
