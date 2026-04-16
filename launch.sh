#!/bin/bash
# Launch Alor — kills stale wrappers, starts the Tauri app.
# The app builds and runs via cargo tauri dev.

# Load full environment (cargo, rustup, etc.)
source "$HOME/.bashrc" 2>/dev/null
source "$HOME/.cargo/env" 2>/dev/null

# Kill any orphaned wrapper processes from previous runs.
pkill -f alor-wrapper 2>/dev/null

# Get the directory where this script is located.
SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
cd "$SCRIPT_DIR"
exec cargo tauri dev
