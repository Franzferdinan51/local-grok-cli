#!/usr/bin/env python3
"""Expose Grok Local's single-turn agent as a small MCP stdio server.

This is intentionally an adapter, not an ACP implementation. It uses Grok's
headless single-turn mode and keeps the MCP wire protocol on stdout.
"""
import json
import os
import pathlib
import subprocess
import sys

NAME = "grok-local-adapter"
VERSION = "0.1.0"
GROK = os.environ.get("GROK_BIN", "grok")
DEFAULT_CWD = os.environ.get("GROK_ADAPTER_CWD", str(pathlib.Path.home()))
MAX_TIMEOUT = int(os.environ.get("GROK_ADAPTER_MAX_TIMEOUT", "600"))

TOOLS = [
    {
        "name": "grok_local_prompt",
        "description": "Run one bounded, headless Grok Local agent turn using its configured MCP servers and return the response.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "prompt": {"type": "string", "description": "Task or question for Grok Local."},
                "cwd": {"type": "string", "description": "Working directory; defaults to GROK_ADAPTER_CWD or the home directory."},
                "timeout": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT, "default": 300},
                "max_turns": {"type": "integer", "minimum": 1, "maximum": 10, "default": 4},
            },
            "required": ["prompt"],
        },
    },
    {
        "name": "grok_local_status",
        "description": "Return the installed Grok Local version and adapter configuration without running an agent turn.",
        "inputSchema": {"type": "object", "properties": {}},
    },
]


def reply(msg_id, result=None, error=None):
    out = {"jsonrpc": "2.0", "id": msg_id}
    out["error" if error else "result"] = error if error else result
    sys.stdout.write(json.dumps(out, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def text_result(text, is_error=False):
    return {"content": [{"type": "text", "text": text}], "isError": is_error}


def grok_prompt(args):
    prompt = args.get("prompt")
    if not isinstance(prompt, str) or not prompt.strip() or len(prompt) > 12000:
        raise ValueError("prompt must be a non-empty string of at most 12000 characters")
    cwd = args.get("cwd", DEFAULT_CWD)
    if not isinstance(cwd, str) or not cwd or not os.path.isdir(cwd):
        raise ValueError("cwd must be an existing directory")
    timeout = int(args.get("timeout", 300))
    turns = int(args.get("max_turns", 4))
    if not 1 <= timeout <= MAX_TIMEOUT:
        raise ValueError(f"timeout must be between 1 and {MAX_TIMEOUT} seconds")
    if not 1 <= turns <= 10:
        raise ValueError("max_turns must be between 1 and 10")
    cmd = [GROK, "--single", prompt, "--output-format", "plain", "--max-turns", str(turns), "--permission-mode", "auto", "--no-subagents", "--no-plan"]
    env = os.environ.copy()
    proc = subprocess.run(cmd, cwd=cwd, env=env, text=True, capture_output=True, timeout=timeout)
    output = proc.stdout.strip()
    if proc.stderr.strip():
        output += ("\n\n[stderr]\n" + proc.stderr.strip()) if output else ("[stderr]\n" + proc.stderr.strip())
    if proc.returncode:
        raise RuntimeError(f"grok exited with code {proc.returncode}:\n{output[-12000:]}")
    return output[-30000:] or "(Grok returned no text)"


def main():
    for line in sys.stdin:
        try:
            msg = json.loads(line)
            method = msg.get("method")
            msg_id = msg.get("id")
            if method == "initialize":
                reply(msg_id, {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}}, "serverInfo": {"name": NAME, "version": VERSION}})
            elif method == "notifications/initialized":
                continue
            elif method == "tools/list":
                reply(msg_id, {"tools": TOOLS})
            elif method == "tools/call":
                name = msg.get("params", {}).get("name")
                args = msg.get("params", {}).get("arguments", {}) or {}
                if name == "grok_local_prompt":
                    reply(msg_id, text_result(grok_prompt(args)))
                elif name == "grok_local_status":
                    reply(msg_id, text_result(json.dumps({"adapter": VERSION, "grok_bin": GROK, "cwd": DEFAULT_CWD, "max_timeout": MAX_TIMEOUT})))
                else:
                    reply(msg_id, error={"code": -32601, "message": f"unknown tool: {name}"})
            elif msg_id is not None:
                reply(msg_id, error={"code": -32601, "message": f"unknown method: {method}"})
        except subprocess.TimeoutExpired:
            reply(msg.get("id"), error={"code": -32001, "message": "Grok timed out"})
        except Exception as exc:
            reply(msg.get("id") if isinstance(locals().get("msg"), dict) else None, error={"code": -32000, "message": str(exc)})


if __name__ == "__main__":
    main()
