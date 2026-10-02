#!/usr/bin/env bash
#
# setup-searxng.sh — local-first web search for grok-local.
#
# Ensures a working SearXNG on http://127.0.0.1:8888 (what the grok-local
# web_search tool expects):
#   1. venv at $SEARXNG_VENV (default ~/searxng-env), created if missing
#   2. SearXNG source at $SEARXNG_SRC (default ~/searxng), cloned at the
#      pinned commit when missing (never in a temp/Downloads dir — that
#      dangling-editable failure is exactly what this script prevents)
#   3. non-editable pip install into the venv (survives source deletion)
#   4. settings at $SEARXNG_SETTINGS (default ~/.searxng/settings.yml):
#      created with a generated secret when missing, preserved otherwise
#   5. health check against /search?format=json
#   6. with --persist (macOS): install + load a LaunchAgent so search
#      starts at login and stays up (KeepAlive)
#
# Idempotent and fail-open: every step reports clearly; run again any time.
# Usage: scripts/setup-searxng.sh [--persist] [--port 8888]

set -euo pipefail

SEARXNG_PIN="74f1ca203"  # verified commit (searxng 2026.4.22)
SEARXNG_REPO="https://github.com/searxng/searxng"
VENV="${SEARXNG_VENV:-$HOME/searxng-env}"
SRC="${SEARXNG_SRC:-$HOME/searxng}"
SETTINGS="${SEARXNG_SETTINGS:-$HOME/.searxng/settings.yml}"
PORT="${SEARXNG_PORT:-8888}"
PERSIST=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --persist) PERSIST=1; shift ;;
    --port) PORT="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

fail() { echo "setup-searxng: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || fail "need '$1' on PATH"; }
need python3; need git; need curl

# 1. venv ---------------------------------------------------------------
if [[ ! -x "$VENV/bin/python" ]]; then
  echo "creating venv at $VENV ..."
  python3 -m venv "$VENV" || fail "venv creation failed"
fi
PY="$VENV/bin/python"
"$PY" -m pip --version >/dev/null 2>&1 || fail "venv has no pip"

# 2. source -------------------------------------------------------------
if [[ ! -f "$SRC/searx/__init__.py" ]]; then
  echo "cloning searxng@$SEARXNG_PIN to $SRC ..."
  git clone --quiet "$SEARXNG_REPO" "$SRC" || fail "clone failed"
  (cd "$SRC" && git checkout --quiet "$SEARXNG_PIN") || fail "checkout failed"
else
  echo "source present at $SRC"
fi

# 3. install (non-editable: baked into the venv) -------------------------
if ! "$PY" -c "import searx, sys; sys.exit(0 if 'site-packages' in searx.__file__ else 1)" 2>/dev/null; then
  echo "installing searxng into the venv (non-editable) ..."
  (cd "$SRC" && "$PY" -m pip install --quiet --no-deps --no-build-isolation .) \
    || fail "pip install failed"
else
  echo "searxng already baked into the venv"
fi
"$PY" -c "import searx; print('searx OK:', searx.__file__)"

# 4. settings -----------------------------------------------------------
if [[ ! -f "$SETTINGS" ]]; then
  echo "writing fresh settings to $SETTINGS ..."
  mkdir -p "$(dirname "$SETTINGS")"
  SECRET=$(openssl rand -hex 32 2>/dev/null || "$PY" -c "import secrets; print(secrets.token_hex(32))")
  "$PY" - "$SRC/searx/settings.yml" "$SETTINGS" "$SECRET" "$PORT" <<'EOF'
import sys
src, dst, secret, port = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
text = open(src).read()
text = text.replace('secret_key: "ultrasecretkey"', f'secret_key: "{secret}"')
text = text.replace("port: 8888", f"port: {port}")
text = text.replace('bind_address: "0.0.0.0"', 'bind_address: "127.0.0.1"')
# bing is the reliable default engine on captcha-walled networks
text = text.replace("  - name: bing\n    engine: bing\n    shortcut: bi\n    disabled: true\n",
                    "  - name: bing\n    engine: bing\n    shortcut: bi\n")
open(dst, "w").write(text)
print("settings written")
EOF
else
  echo "settings present at $SETTINGS (preserved)"
  if grep -q 'secret_key: "ultrasecretkey"' "$SETTINGS"; then
    echo "WARNING: settings still use the default secret key; searxng will refuse to start." >&2
    echo "  Fix: replace secret_key in $SETTINGS with: $(openssl rand -hex 32 2>/dev/null || echo '<random-hex>')" >&2
  fi
fi

# 5. health check (server may already run via LaunchAgent/systemd) -------
export SEARXNG_SETTINGS_PATH="$SETTINGS"
if curl -sf -o /dev/null --max-time 5 "http://127.0.0.1:${PORT}/" 2>/dev/null; then
  RESULTS=$(curl -sf --max-time 30 "http://127.0.0.1:${PORT}/search?q=test&format=json" 2>/dev/null \
    | "$PY" -c "import json,sys; print(json.load(sys.stdin).get('number_of_results', '?'))" 2>/dev/null || echo "?")
  echo "search OK at http://127.0.0.1:${PORT} (test query: ${RESULTS} results)"
else
  echo "server not running yet at http://127.0.0.1:${PORT}"
  echo "  start now:  SEARXNG_SETTINGS_PATH=$SETTINGS $VENV/bin/searxng-run"
  echo "  persist:    $0 --persist   (macOS LaunchAgent, starts at login)"
fi

# 6. persist (macOS LaunchAgent) ------------------------------------------
if [[ "$PERSIST" == "1" ]]; then
  [[ "$(uname)" == "Darwin" ]] || fail "--persist currently supports macOS only"
  AGENT="$HOME/Library/LaunchAgents/ai.grok.searxng.plist"
  mkdir -p "$(dirname "$AGENT")"
  cat > "$AGENT" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>ai.grok.searxng</string>
  <key>ProgramArguments</key>
  <array>
    <string>$VENV/bin/searxng-run</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>SEARXNG_SETTINGS_PATH</key>
    <string>$SETTINGS</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>/tmp/searxng.log</string>
  <key>StandardErrorPath</key>
  <string>/tmp/searxng.log</string>
</dict>
</plist>
EOF
  launchctl bootout "gui/$(id -u)/ai.grok.searxng" >/dev/null 2>&1 || true
  launchctl bootstrap "gui/$(id -u)" "$AGENT" || fail "launchctl bootstrap failed"
  sleep 6
  curl -sf -o /dev/null --max-time 5 "http://127.0.0.1:${PORT}/" \
    && echo "LaunchAgent loaded and serving at http://127.0.0.1:${PORT}" \
    || fail "agent loaded but server not responding (see /tmp/searxng.log)"
fi

echo "setup-searxng: done"
