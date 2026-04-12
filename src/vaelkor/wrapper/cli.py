"""
Wrapper CLI - runs a wrapper as a standalone process.

Usage:
    python -m vaelkor.wrapper.cli claude
    python -m vaelkor.wrapper.cli codex
"""

import argparse
import asyncio
import signal
import sys
from pathlib import Path

from ..config import load_config, SOCKET_DIR
from .base import AgentWrapper, WrapperConfig


def main():
    parser = argparse.ArgumentParser(description="Run a Vaelkor agent wrapper")
    parser.add_argument("agent", help="Agent name (e.g., claude, codex)")
    parser.add_argument("--socket-dir", type=Path, default=SOCKET_DIR)
    args = parser.parse_args()

    config = load_config()

    if args.agent not in config.agents:
        print(f"Error: Unknown agent '{args.agent}'")
        print(f"Available agents: {', '.join(config.agents.keys())}")
        sys.exit(1)

    agent_config = config.agents[args.agent]

    wrapper_config = WrapperConfig(
        agent_name=args.agent,
        socket_path=args.socket_dir / f"{args.agent}.sock",
        command=agent_config.command,
    )

    asyncio.run(run_wrapper(wrapper_config))


async def run_wrapper(config: WrapperConfig):
    """Run wrapper with graceful shutdown."""
    wrapper = AgentWrapper(config)

    loop = asyncio.get_event_loop()
    stop_event = asyncio.Event()

    def handle_signal():
        stop_event.set()

    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, handle_signal)

    print(f"Starting wrapper for {config.agent_name}...")
    print(f"Socket: {config.socket_path}")
    print(f"Command: {' '.join(config.command)}")

    await wrapper.start()
    print(f"Wrapper running. Session: {wrapper.session_name}")

    await stop_event.wait()

    print("\nShutting down...")
    await wrapper.stop()
    print("Wrapper stopped.")


if __name__ == "__main__":
    main()
