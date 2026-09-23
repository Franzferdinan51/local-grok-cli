#!/usr/bin/env bash

# Install Grok Local without claiming the official `grok` command.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
binary=${GROK_LOCAL_BINARY:-"$repo_root/target/release/grok-local"}
install_dir=${GROK_LOCAL_INSTALL_DIR:-"$HOME/.local/bin"}

if [[ ! -x "$binary" ]]; then
  echo "Grok Local binary not found or not executable: $binary" >&2
  echo "Build it first with: cargo build -p xai-grok-pager-bin --release" >&2
  exit 1
fi

mkdir -p "$install_dir"
install -m 755 "$binary" "$install_dir/grok-local"

echo "Installed grok-local to $install_dir/grok-local"
echo "The official 'grok' command was not changed."

# --- SystemOne native routing (v0.5.0) -------------------------------------
# The binary auto-starts the router shim from $SYSTEMONE_RELEASE_DIR or
# ~/systemone-release when it is not already running. Make sure the package
# is present; routing fails open at runtime if it is not.
systemone_dir=${SYSTEMONE_RELEASE_DIR:-"$HOME/systemone-release"}
if [[ ! -f "$systemone_dir/systemone/shim.py" ]]; then
  if command -v git >/dev/null 2>&1; then
    echo "Fetching SystemOne router package to $systemone_dir ..."
    git clone --depth 1 https://github.com/Franzferdinan51/SystemOne "$systemone_dir" \
      || echo "warning: could not clone SystemOne; routing will fail open at runtime" >&2
  else
    echo "warning: git not found; skipping SystemOne setup (routing will fail open)" >&2
  fi
fi
if [[ -f "$systemone_dir/systemone/shim.py" ]]; then
  echo "SystemOne router package present at $systemone_dir"
  # Zero-setup: install the shim's Python dependencies now so the first
  # auto-start just works. Fail-open: any error only means routing stays off.
  if command -v python3 >/dev/null 2>&1; then
    echo "Installing SystemOne router dependencies (one-time) ..."
    if python3 -m pip install --quiet "$systemone_dir" 2>/tmp/systemone-pip.log; then
      echo "SystemOne router dependencies installed."
    else
      echo "warning: SystemOne pip install failed (see /tmp/systemone-pip.log); routing will fail open at runtime" >&2
    fi
  else
    echo "warning: python3 not found; skipping SystemOne deps (routing will fail open)" >&2
  fi
  echo "The GLiClass model (~400MB) downloads from HuggingFace on first router start."
else
  echo "warning: SystemOne router package not found; routing will fail open at runtime" >&2
fi
