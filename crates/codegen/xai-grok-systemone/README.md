# xai-grok-systemone — native SystemOne routing for grok-local

Bakes the SystemOne "speed stack" dispatcher directly into the `grok-local`
binary so routing works out-of-the-box with zero manual setup: no external
Python adapter to wire, no config entries to add, no shim to launch by hand.

Semantics mirror the `grok-local-acp-adapter` v0.5.2
(`integrations/grok-local-acp-adapter/`), which remains in place as a legacy
fallback.

## What happens per task (headless `--single`)

1. **Router check** — probe `http://127.0.0.1:8765/healthz`. If down and
   `auto_start_shim` is on, the binary starts the installed shim itself
   (`python3.11 -m systemone.shim --port 8765`, detached, lock-guarded,
   output to `~/.grok-local/systemone-shim.log`). The GLiClass model is
   already in the HuggingFace cache, so cold start is seconds.
2. **Route call** — POST the task (first 500 chars) to
   `/v1/systemone/route` (3s timeout, `:8765` then `:8079` fallback).
3. **Apply** — the tier maps to a reasoning effort and loop caps
   (`edge`/`economy` → low/4 turns, `balanced` → medium/6, `heavy` → high/10),
   applied only when the user did not pass `--reasoning-effort`/`--max-turns`
   explicitly. An explicit `effort` in the shim response wins ("shim knows best").
4. **MCP suggestions** — configured `[mcp_servers.*]` are scored against the
   task text + route `task_labels` (name token = 2, description token = 1).
   When the router returns the Phase 3 decision surface with
   `tool_scoring == "full"` and a non-empty `ranked_tools` list, the shim's
   hybrid (keyword + zero-shot model) tool ranking drives suggestions instead
   of the local keyword scorer. Conservative pruning is **opt-in** and engages
   only for live-router, high-confidence (≥ 0.85), cheap-tier
   (`edge`/`economy`), **non-uncertain** routes with non-empty suggestions.
   `uncertain == true` disables pruning unconditionally (noted in the evidence
   line as `prune=disabled(uncertain)`); `tool_scoring == "skipped"` or absent
   keys keep the pre-Phase-3 behavior exactly.
5. **Evidence** — one greppable `systemone: …` line on stderr proves what
   routing did (route source, tier, effort, max_turns, confidence, plus
   `uncertain=true` / `prune=…` / `model_ranking=<top>` when applicable).

## What happens per turn (interactive TUI / ACP)

Every human prompt (not slash commands, not subagent wake turns) is routed
the same way, then:

- **Reasoning effort** — the routed effort is applied to the turn's sampling
  config *unless* the user explicitly set one (slash command or config file);
  once the user chooses, the router stands down on effort for the session.
  The router never touches the model id — no switching, no unloading.
- **Turn budget** — the routed `max_turns` caps the tool loop unless the user
  passed an explicit `--max-turns`, which always wins.
- **Evidence** — the decision is logged (`shell.systemone.route`) with tier,
  effort, max_turns, source, and confidence. The status bar can show the
  shim's best-value advisory (`model:auto→<top-ranked>`) — display only, the
  session's model never changes.

## Decision surfaces (Phase 3)

Newer shims attach a scored surface to the route dict:

| Key | Meaning |
|---|---|
| `uncertain` | Shim's uncertainty verdict — disables MCP pruning unconditionally |
| `margin` | Top-1/top-2 calibrated probability margin |
| `calibrated_probabilities` | Temperature-fitted tier distribution (probability desc) |
| `ranked_models` | Top models by expected utility — **advisory only**, surfaced as `model_ranking=<top>` in the evidence line and the TUI's `model:auto→…` suffix. Never switches or unloads anything. |
| `ranked_tools` + `tool_scoring` | Hybrid tool relevance ranking; `"full"` drives MCP/server suggestions, `"skipped"` (uncertain or cheap path) keeps keyword scoring |

`POST /v1/systemone/rank-plans` (`{"task", "plans": [{"id", "text"}]}`)
scores candidate plans — advisory, fail-open (input order, `score: None` on
any failure). Older shims omit these keys entirely: absence is "not present",
never an error, and behavior is exactly the pre-Phase-3 one.

## Fail-open contract

Every failure — router unreachable, timeout, bad payload, disabled config —
leaves the session exactly as if routing did not exist. The session must
proceed with zero user-visible breakage. This is tested, not just documented.

## Kill-switches

| Control | Effect |
|---|---|
| `GROK_LOCAL_SYSTEMONE=0` | Disables ALL routing behavior (probe, shim start, route, prune) |
| `GROK_LOCAL_SYSTEMONE_NO_AUTOSTART=1` | Probe only; never start the shim |
| `GROK_LOCAL_SYSTEMONE_PRUNE=1` | Opt in to conservative MCP pruning |
| `GROK_LOCAL_SYSTEMONE_URLS=…` | Comma-separated router URL override |
| `GROK_LOCAL_SYSTEMONE_TIMEOUT_SECS=…` | Router HTTP timeout override |

`~/.grok-local/config.toml` `[systemone]` section:

```toml
[systemone]
enabled = true
urls = ["http://127.0.0.1:8765/v1/systemone/route", "http://127.0.0.1:8079/v1/systemone/route"]
timeout_secs = 3
default_effort = "high"   # fail-open effort when the router is unreachable
auto_start_shim = true
shim_port = 8765
prune_mcp_servers = false # opt-in only
prune_min_confidence = 0.85
```

Env vars always win over the file.

## Deliberate non-goals

- **Model switching is not implemented.** The router's `model_id` is advisory
  only — it is logged, never acted on. Ryan's standing rule (never unload a
  model he loaded himself) is enforced by not having a code path that does it.
- **MCP pruning in interactive sessions.** Headless sessions prune MCP
  servers at load time (safe: the allowlist gates `Session::open`). Interactive
  sessions keep all servers connected — tool definitions there carry no server
  attribution, so per-turn filtering would risk hiding tools the user needs.
  Interactive turns still get routed effort + turn budgets.
- **Non-Unix auto-start.** On Windows/macOS-non-unix the probe still runs and
  an already-running router is used; the binary just won't spawn the shim.
