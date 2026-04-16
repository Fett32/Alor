#!/usr/bin/env bash
# Alor bootstrap
# Run once to install Rust + Tauri prerequisites, then build the project.
set -euo pipefail

echo "=== Alor bootstrap ==="

# --------------------------------------------------------------------------
# 1. Rust
# --------------------------------------------------------------------------
if ! command -v cargo &>/dev/null; then
  echo "[rust] installing via rustup..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
  source "$HOME/.cargo/env"
else
  echo "[rust] already installed: $(cargo --version)"
fi

# Make sure cargo is on PATH for the rest of this script
export PATH="$HOME/.cargo/bin:$PATH"

# --------------------------------------------------------------------------
# 2. System dependencies (Tauri on Linux)
# --------------------------------------------------------------------------
echo "[apt] installing Tauri system deps..."
sudo apt-get update -q
sudo apt-get install -y --no-install-recommends \
  libwebkit2gtk-4.1-dev \
  build-essential \
  curl \
  wget \
  file \
  libssl-dev \
  libayatana-appindicator3-dev \
  librsvg2-dev \
  pkg-config

# --------------------------------------------------------------------------
# 3. Node (for the frontend dev server + tauri CLI)
# --------------------------------------------------------------------------
if ! command -v node &>/dev/null; then
  echo "[node] installing via nvm..."
  curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.39.7/install.sh | bash
  export NVM_DIR="$HOME/.nvm"
  # shellcheck disable=SC1091
  source "$NVM_DIR/nvm.sh"
  nvm install --lts
else
  echo "[node] already installed: $(node --version)"
fi

# --------------------------------------------------------------------------
# 4. npm install
# --------------------------------------------------------------------------
cd "$(dirname "$0")"
echo "[npm] installing JS dependencies..."
npm install

# --------------------------------------------------------------------------
# 5. Cargo check (fast compile check without full build)
# --------------------------------------------------------------------------
echo "[cargo] checking src-tauri..."
cargo check --manifest-path src-tauri/Cargo.toml

echo ""
echo "=== Bootstrap complete ==="
echo "To start dev mode:  npm run tauri dev"
echo "To build release:   npm run tauri build"
