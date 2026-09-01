# Grok Local MCP adapter

`integrations/grok-mcp-adapter/grok_mcp_adapter.py` exposes Grok Local's
headless single-turn mode as an MCP stdio server for clients such as LM Studio.

This is intentionally an MCP adapter, not ACP. Grok's native `grok agent stdio`
endpoint speaks ACP and must not be placed directly in an MCP JSON file.

## Environment

- `GROK_BIN`: absolute Grok executable path (recommended)
- `GROK_ADAPTER_CWD`: default working directory
- `GROK_ADAPTER_MAX_TIMEOUT`: maximum per-call timeout, default 600 seconds

The adapter exposes `grok_local_prompt` and `grok_local_status`. Calls are
bounded, disable subagent spawning, and use Grok's configured MCP servers.
Because Grok Local commonly uses LM Studio as its model backend, avoid calling
this adapter from the same model request path if it would create recursive
model calls. Use a separate model/backend when required.
