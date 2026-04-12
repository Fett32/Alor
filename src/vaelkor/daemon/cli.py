"""
Daemon CLI - runs the control daemon.

Usage:
    python -m vaelkor.daemon.cli
    python -m vaelkor.daemon.cli --resume
"""

import argparse
import asyncio
import signal
import sys
from pathlib import Path

from ..config import load_config, DATA_DIR
from .core import Daemon


def main():
    parser = argparse.ArgumentParser(description="Run the Vaelkor control daemon")
    parser.add_argument("--resume", action="store_true", help="Resume last session if unclean shutdown")
    parser.add_argument("--session", type=str, help="Specific session ID to resume")
    args = parser.parse_args()

    config = load_config()

    session_id = None
    if args.session:
        session_id = args.session
    elif args.resume:
        last_session_file = DATA_DIR / "last_session"
        if last_session_file.exists():
            session_id = last_session_file.read_text().strip()

    asyncio.run(run_daemon(config, session_id))


async def run_daemon(config, session_id: str | None):
    """Run daemon with graceful shutdown."""
    daemon = Daemon(
        heartbeat_interval=config.heartbeat_interval,
        task_assignment_timeout=config.task_assignment_timeout,
    )

    loop = asyncio.get_event_loop()
    stop_event = asyncio.Event()

    def handle_signal():
        stop_event.set()

    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, handle_signal)

    print("Starting Vaelkor daemon...")
    await daemon.start(session_id)
    print(f"Session: {daemon.state.session_id}")

    # Start wrappers for autoconnect agents
    for name, agent_config in config.agents.items():
        if agent_config.autoconnect:
            print(f"Connecting to wrapper: {name}")
            connected = await daemon.connect_wrapper(name)
            if connected:
                print(f"  Connected to {name}")
            else:
                print(f"  Wrapper not running: {name}")

    print("\nDaemon running. Press Ctrl+C to stop.")

    # Simple REPL for testing
    await interactive_loop(daemon, stop_event)

    print("\nShutting down...")
    await daemon.stop()
    print("Daemon stopped.")


async def interactive_loop(daemon: Daemon, stop_event: asyncio.Event):
    """Simple interactive loop for testing."""
    reader = asyncio.StreamReader()
    protocol = asyncio.StreamReaderProtocol(reader)

    loop = asyncio.get_event_loop()
    await loop.connect_read_pipe(lambda: protocol, sys.stdin)

    print("\nCommands:")
    print("  status          - Show session status")
    print("  agents          - List agents")
    print("  tasks           - List tasks")
    print("  assign <agent> <summary> - Assign a task")
    print("  transcript <agent> - Get agent transcript")
    print("  quit            - Exit")
    print()

    while not stop_event.is_set():
        try:
            line = await asyncio.wait_for(reader.readline(), timeout=0.5)
            if not line:
                continue

            cmd = line.decode().strip()
            if not cmd:
                continue

            await handle_command(daemon, cmd)

            if cmd == "quit":
                stop_event.set()
                break

        except asyncio.TimeoutError:
            continue
        except EOFError:
            break


async def handle_command(daemon: Daemon, cmd: str):
    """Handle a CLI command."""
    parts = cmd.split(maxsplit=2)
    action = parts[0] if parts else ""

    match action:
        case "status":
            if daemon.state:
                print(f"Session: {daemon.state.session_id}")
                print(f"Started: {daemon.state.started_at}")
                print(f"Agents: {len(daemon.state.agents)}")
                print(f"Tasks: {len(daemon.state.tasks)}")
                print(f"Active tasks: {len(daemon.get_active_tasks())}")

        case "agents":
            if daemon.state:
                for name, agent in daemon.state.agents.items():
                    connected = name in daemon.wrappers and daemon.wrappers[name].connected
                    conn_str = "connected" if connected else "disconnected"
                    print(f"  {name}: {agent.status} ({conn_str})")

        case "tasks":
            if daemon.state:
                for task_id, task in daemon.state.tasks.items():
                    print(f"  {task_id} [{task.state.value}] -> {task.assigned_to}")
                    print(f"    {task.summary}")

        case "assign":
            if len(parts) < 3:
                print("Usage: assign <agent> <summary>")
                return
            agent = parts[1]
            summary = parts[2]
            task = await daemon.assign_task(agent, summary)
            if task:
                print(f"Created: {task.task_id}")
            else:
                print("Failed to create task")

        case "transcript":
            if len(parts) < 2:
                print("Usage: transcript <agent>")
                return
            agent = parts[1]
            lines = await daemon.get_agent_transcript(agent, 20)
            for line in lines:
                print(f"  {line}")

        case "quit":
            print("Goodbye!")

        case _:
            print(f"Unknown command: {action}")


if __name__ == "__main__":
    main()
