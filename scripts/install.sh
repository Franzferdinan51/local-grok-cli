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
