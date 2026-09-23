# grok-local ACP adapter (with speed stack)

`grok_local_acp_adapter.py` — a stateful **ACP-to-MCP bridge** for Grok Local
(Ryan's offline-first fork of the Grok Build CLI, backed by **local LM Studio
models** on the Mac mini — no cloud models involved).

The MCP surface exposes Grok Local's native ACP session lifecycle. The
adapter spawns the local `grok-local` binary (`~/.local/bin/grok-local` when
present, else `grok`; override with `GROK_BIN`) as
`grok-local agent --no-leader stdio` and speaks ACP to it while presenting MCP
stdio to the caller. Agent-initiated permission requests are surfaced to the
MCP caller and remain pending until an explicit
`grok_local_permission_decide` answers them.

Install: `cp grok_local_acp_adapter.py /Users/duckets/.local/bin/grok-local-mcp-adapter`
(atomic: write temp + rename, `chmod +x`; keep a dated backup).

## Tool inventory (24)

### One-shot & status
| Tool | What |
|---|---|
| `grok_local_prompt` | One bounded headless turn (`--single`), returns the response. Params: `prompt*`, `cwd`, `timeout`, `max_turns`, `permission_mode` (default `auto`), `allow_subagents`, `allow_plan`, `effort` (`auto` = ask SystemOne, fail-open to config default; else none/low/medium/high/max → `--reasoning-effort`). |
| `grok_local_status` | Adapter + grok binary + active sessions, incl. per-session stderr tail. |
| `grok_local_models` / `grok_local_sessions` / `grok_local_mcp_servers` / `grok_local_inspect` / `grok_local_doctor` | Read-only wrappers around the grok CLI. |

### Persistent ACP sessions
| Tool | What |
|---|---|
| `grok_local_session_start` / `_load` / `_resume` | Start/load/resume a persistent ACP session. `mcp_servers`: `"auto"` (default) sends an empty ACP `mcpServers` list — verified 2026-09-23 the field is required and `[]` still connects the servers configured in `~/.grok-local/config.toml`; a list of server **names** from config is resolved to per-session ACP entries (opt-in allowlist); a list of dicts is passed through raw. Returns a SystemOne routing summary cached per session id. |
| `grok_local_session_prompt` | Queue a follow-up prompt; poll `grok_local_session_events` for updates/completion/permission requests. |
| `grok_local_session_events` | Drain queued ACP updates; each pending permission is reported once until decided. |
| `grok_local_session_transcript` | Retained session/update event history (last 500). |
| `grok_local_session_cancel` / `_close` | Cancel the active turn / close the session and its bridge process. |
| `grok_local_permission_decide` | Answer a pending permission request: `accept`/`deny`/`cancel` (+ `option_id`). Decisions are recorded on the session for compaction/review. |

### Speed stack (v0.5.0)
| Tool | What |
|---|---|
| `grok_local_systemone_route` | Routing decision for a task: POSTs `~/.grok-local` SystemOne (`:8765`, fallback Jeff-1 `:8079`, 3s timeout), maps the tier to `--reasoning-effort` and a permission mode. **Fail-open**: any error/timeout → config defaults, error logged in the decision. Model ids are local LM Studio ids and advisory — the adapter never loads/unloads models. |
| `grok_local_lmstudio_models` | LM Studio library ids + currently loaded model (observational via `lms ps`; never triggers a load). |
| `grok_local_tools_batch` | Run independent tool calls concurrently (`ThreadPoolExecutor`, max 8 workers); results in input order, per-call errors captured. **Independent calls only** — no ordering guarantees between them; no nested `tools_batch`; max 16 calls. |
| `grok_local_ralph_run` | Ralph loop: each iteration spawns a **fresh** one-shot with the progress file prepended; the adapter appends each output to the progress file. Stops on `done_marker` in output (default `DONE`), `test_command` exit 0, **stall** (byte-identical output twice in a row), or `max_iterations` (default 10). Returns a summary + progress file path. |
| `grok_local_plan_then_execute` | Planner (high effort) writes a plan artifact to `~/.grok-local/plans/`; executor (SystemOne-routed effort, default low) runs it. Both roles use the loaded local model; if only one model is loaded, both roles use it. |
| `grok_local_compact_session` | Anchored compaction of the **adapter-side** transcript: writes the full raw transcript to `~/.grok-local/transcripts/<session>.jsonl` first, then replaces it with a summary preserving permission decisions, file paths touched, and error messages. Note: the agent process keeps its own context — for a true reset, close the session and start fresh seeded with the summary. |
| `grok_local_slash_verify` | `/verify`: run build then test in the session cwd (auto-detects `package.json` → npm, `Makefile` → make, `pyproject.toml` → pytest when no commands given). Returns pass/fail + output tails. |
| `grok_local_slash_review` | `/review`: heuristic findings from the recent transcript (tracebacks, errors, permission denials) plus `git diff --stat` / `git status`. |

## Config (`~/.grok-local/config.toml`, `[speed]`)

```toml
[speed]
enabled = true
systemone_urls = ["http://127.0.0.1:8765/v1/systemone/route",
                  "http://127.0.0.1:8079/v1/systemone/route"]
systemone_timeout = 3
default_effort = "high"        # fail-open effort (matches grok-local default)
permission_mode_default = "auto"
planner_model = "ornith-1.5-35b-a3b"   # advisory; never auto-loaded
planner_effort = "high"
executor_effort = "low"
default_mcp_servers = "auto"   # or a list of [mcp_servers.*] names
ralph_max_iterations = 10
ralph_done_marker = "DONE"
```

Tier mapping (code defaults, overridable in config): SystemOne
`edge`/`economy` → effort `low`, `balanced` → `medium`, `heavy` → `high`;
advisory local models `edge`/`economy` → `ornith-1.5-9b`,
`balanced`/`heavy` → `ornith-1.5-35b-a3b` (validated against the LM Studio
library; the loaded model is always acceptable).

## Speed-stack design notes

- **Effort routing, not model routing.** The box runs one local model at a
  time; the lever that actually moves is `--reasoning-effort` per task
  (SystemOne tier → effort), with fail-open to the grok-local default.
- **MCP pruning is opt-in and honest.** The grok agent merges per-session
  `mcpServers` with its configured servers (verified 2026-09-23) —
  `session/new` cannot suppress configured servers, so the name allowlist
  currently *adds* per-session servers; true pruning awaits agent support.
  Motivation: Cursor's reported ~46.9% token cut from on-demand tool loading
  (their measurement, not ours).
- **Fresh-context loops beat long contexts.** `ralph_run` keeps state in the
  progress file and starts every iteration clean — the standard Ralph
  pattern for defeating context rot.
- **Batch, don't serialize.** `tools_batch` fans out independent calls;
  MCP has no native batch primitive, so the adapter owns the fan-out.
- **Compact with an anchor.** `compact_session` archives the raw transcript
  before summarizing, and the summary preserves decisions/paths/errors —
  the bits a fresh session needs to resume.

## History

- v0.3.0 — base.
- v0.4.0 (repair, 2026-09-22): `mcp_servers` param (verified `mcpServers: []`
  keeps configured servers), pending-request leak fix, stderr tails,
  `permission_mode`/`allow_subagents`/`allow_plan` on prompt,
  `grok_local_session_transcript`.
- v0.5.1 (2026-09-22): default binary is now the LOCAL `grok-local`
  (`~/.local/bin/grok-local`, LM Studio) instead of the cloud `grok` CLI —
  Ryan's directive; the cloud CLI's xAI balance is exhausted (402 on every
  turn). `GROK_BIN` still overrides explicitly.
- v0.5.0 (speed stack, 2026-09-22): SystemOne effort dispatcher, LM Studio
  model awareness (never load/unload), `terminal` capability reverted to
  `False` (adapter doesn't implement client-RPC `terminal/create`),
  MCP name allowlist, `tools_batch`, `ralph_run`, `plan_then_execute`,
  `compact_session`, `slash_verify`, `slash_review`, `[speed]` config.
