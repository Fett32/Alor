#!/bin/bash
# Quick launcher for Vaelkor development

cd "$(dirname "$0")"
source .venv/bin/activate

case "${1:-ui}" in
    ui)
        echo "Starting Vaelkor UI..."
        python -m vaelkor.main
        ;;
    daemon)
        echo "Starting Vaelkor daemon..."
        python -m vaelkor.daemon.cli "${@:2}"
        ;;
    wrapper)
        if [ -z "$2" ]; then
            echo "Usage: ./run.sh wrapper <agent>"
            echo "Available agents: claude, codex"
            exit 1
        fi
        echo "Starting wrapper for $2..."
        python -m vaelkor.wrapper.cli "$2"
        ;;
    test)
        echo "Running tests..."
        pytest tests/ -v
        ;;
    *)
        echo "Usage: ./run.sh [ui|daemon|wrapper <agent>|test]"
        exit 1
        ;;
esac
