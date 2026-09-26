#!/usr/bin/env python3
"""Stateful Grok Local ACP-to-MCP bridge for LM Studio.

The MCP surface exposes Grok's native ACP session lifecycle. Agent-initiated
permission requests are surfaced to the MCP caller and remain pending until an
explicit grok_local_permission_decide call answers them.

v0.4.0 changes:
- grok_local_session_start/load/resume accept an optional mcp_servers param:
  "auto" (default) sends an empty ACP mcpServers list; verified 2026-09-23
  the agent REQUIRES the field and still connects its configured servers from
  ~/.grok-local/config.toml with [] (confirmed via process inspection).
  An explicit list of ACP mcpServer entries is passed through raw.
- ACP initialize now advertises the terminal capability (was False) so the
  bridged agent can run shell commands.
- Fixed the pending-request dict leak: the reader thread pops matched
  responses and attaches them to the waiter event (ev.response); send()
  reads it back via getattr(ev, "response", None). All pops are idempotent.
- Removed the dead hasattr(found, "send_response") branch in
  grok_local_permission_decide; it now calls _send_permission directly.
- Per-session stderr tail (last 50 lines) is retained and surfaced in
  grok_local_status as session_details[].last_stderr.
- permission_pending is reported only once per request until it is decided
  (no more duplicates on every grok_local_session_events poll).
- grok_local_prompt gains permission_mode (default "auto"),
  allow_subagents (default false) and allow_plan (default false) params;
  defaults preserve the legacy --permission-mode auto --no-subagents
  --no-plan flags.
- New additive tool grok_local_session_transcript returns the retained
  session/update event history (last 500) for a persistent session.

v0.5.1 changes (2026-09-22): default to the LOCAL grok-local binary
  (LM Studio) instead of the cloud grok CLI -- Ryan's directive:
  grok-local runs on LM Studio models; the cloud CLI's xAI balance is
  exhausted (402 on every turn). GROK_BIN still overrides explicitly.

v0.5.0 changes (speed stack):
- SystemOne dispatcher: grok_local_systemone_route tool plus automatic
  per-prompt effort routing for grok_local_prompt (effort="auto", fail-open to
  config defaults). Maps SystemOne tiers to the grok CLI --reasoning-effort
  flag; model ids stay local LM Studio ids and are advisory only -- the
  adapter NEVER loads or unloads models (one model at a time on this
  hardware; never unload a model Ryan loaded himself).
- terminal capability reverted to False: the adapter does not implement the
  ACP client-RPC terminal/create calls, and advertising the capability risked
  hangs/timeouts when the agent tried them; the agent runs shell commands
  through its own local terminal either way.
- mcp_servers now also accepts a list of server NAMES from
  ~/.grok-local/config.toml [mcp_servers.*] (opt-in allowlist, or via config
  default_mcp_servers). Verified 2026-09-23: the grok agent merges per-session
  entries with its configured servers -- session/new cannot suppress
  configured servers, so the allowlist currently ADDS per-session servers;
  true pruning awaits agent support. Motivation: Cursor's reported 46.9%
  token cut from on-demand tool loading (their measurement, not ours).
- New tools: grok_local_tools_batch (concurrent independent calls),
  grok_local_ralph_run (fresh-context iterations with progress file, done
  marker, test gate, stall detector), grok_local_plan_then_execute (planner
  writes a plan artifact, executor runs it; both on the loaded local model),
  grok_local_compact_session (anchored compaction; full transcript archived
  to ~/.grok-local/transcripts/<session>.jsonl first), grok_local_slash_verify
  (build+test, pass/fail plus output tails), grok_local_slash_review
  (transcript + git heuristics), grok_local_lmstudio_models (library + loaded
  model, read-only), grok_local_systemone_route (raw dispatcher, debugging).
- [speed] config section in ~/.grok-local/config.toml (see README).
"""
import json
import os
import pathlib
import queue
import subprocess
import sys
import threading
import time
import uuid
from collections import deque
import concurrent.futures
import re
import urllib.error
import urllib.request

NAME = "grok-local-adapter"
VERSION = "0.5.2"
# Ryan's directive 2026-09-22: grok-local runs on LM Studio models -- never
# default to the cloud grok CLI (its xAI balance is exhausted, so every
# turn 402s). GROK_BIN still overrides explicitly.
_grok_local_bin = pathlib.Path.home() / ".local" / "bin" / "grok-local"
GROK = os.environ.get("GROK_BIN") or (str(_grok_local_bin) if _grok_local_bin.exists() else "grok")
DEFAULT_CWD = os.environ.get("GROK_ADAPTER_CWD", str(pathlib.Path.home()))
MAX_TIMEOUT = int(os.environ.get("GROK_ADAPTER_MAX_TIMEOUT", "600"))

STDERR_TAIL_LINES = 50
TRANSCRIPT_MAX_EVENTS = 500
# ---------------------------------------------------------------------------
# Speed stack (v0.5.0). All additive; every external call is fail-open.
# ---------------------------------------------------------------------------

GROK_LOCAL_HOME = pathlib.Path.home() / ".grok-local"
CONFIG_TOML = GROK_LOCAL_HOME / "config.toml"
LMSTUDIO_URL = os.environ.get("LMSTUDIO_URL", "http://127.0.0.1:1234")
LMS_BIN = str(pathlib.Path.home() / ".lmstudio" / "bin" / "lms")

SPEED_DEFAULTS = {
    "enabled": True,
    "systemone_urls": [
        "http://127.0.0.1:8765/v1/systemone/route",
    ],
    "systemone_timeout": 3,
    # Fail-open effort matches grok-local's own default_reasoning_effort.
    "default_effort": "high",
    "permission_mode_default": "auto",
    # Local LM Studio models. No model ids are hard-coded anywhere: the
    # planner and executor run on the currently loaded model (falling back
    # to the first library model only as a label), tier picks resolve
    # against the live LM Studio library, and unknown ids fail open to the
    # loaded model. The adapter NEVER loads or unloads models (one model at
    # a time on this hardware; never unload a model the user loaded
    # themselves) -- the auto_model_switch path was removed entirely 2026-09-26.
    "planner_model": None,
    "planner_effort": "high",
    "executor_effort": "low",
    "tier_models": {},
    # Per-tier loop caps consumed by _grok_one_shot (max_turns) and
    # _ralph_run (ralph_cap). Values mirror the pre-existing loop bounds:
    # one-shot max_turns defaulted to 4, ralph iterations to 10 and
    # per-iteration turns to 8 -- low gets a small cap, medium a moderate
    # one, high a generous one. A plain effort string ("low"|"medium"|"high")
    # is still accepted for backward compat with v0.5.x configs and maps to
    # the canonical caps for that effort (see _EFFORT_CANONICAL).
    "tier_efforts": {
        "edge": {"effort": "low", "max_turns": 4, "ralph_cap": 4},
        "economy": {"effort": "low", "max_turns": 4, "ralph_cap": 4},
        "balanced": {"effort": "medium", "max_turns": 6, "ralph_cap": 8},
        "heavy": {"effort": "high", "max_turns": 10, "ralph_cap": 12},
    },
    # REMOVED 2026-09-26: there is no model-switch path anymore. The
    # standing no-unload rule — never unload a model the user loaded
    # themselves — is enforced structurally: the adapter has no
    # load/unload code path at all.
    "default_mcp_servers": "auto",
    "ralph_max_iterations": 10,
    "ralph_done_marker": "DONE",
    "ralph_progress_dir": str(GROK_LOCAL_HOME / "ralph"),
    "transcript_dir": str(GROK_LOCAL_HOME / "transcripts"),
    "plans_dir": str(GROK_LOCAL_HOME / "plans"),
}


def _parse_toml_value(val):
    if len(val) >= 2 and val[0] == '"' and val[-1] == '"':
        return val[1:-1].replace('\\"', '"').replace("\\\\", "\\")
    if len(val) >= 2 and val[0] == "'" and val[-1] == "'":
        return val[1:-1]
    if val.startswith("[") and val.endswith("]"):
        inner = val[1:-1].strip()
        if not inner:
            return []
        items, cur, in_s, q = [], "", False, ""
        for ch in inner:
            if in_s:
                cur += ch
                if ch == q:
                    in_s = False
            elif ch in "\"'":
                in_s, q, cur = True, ch, cur + ch
            elif ch == ",":
                items.append(_parse_toml_value(cur.strip()))
                cur = ""
            else:
                cur += ch
        if cur.strip():
            items.append(_parse_toml_value(cur.strip()))
        return items
    if val in ("true", "false"):
        return val == "true"
    try:
        return int(val)
    except ValueError:
        pass
    try:
        return float(val)
    except ValueError:
        pass
    return val


def _parse_toml_subset(text):
    """Minimal TOML for our config needs: [section], dotted [a.b],
    key = string | int | float | bool | [multi-line string list].
    [[array tables]] are treated as plain sections (last wins)."""
    root, section = {}, None
    buf_key, buf_val = None, ""

    def flush():
        nonlocal buf_key, buf_val
        if buf_key is not None:
            section[buf_key] = _parse_toml_value(buf_val)
            buf_key, buf_val = None, ""

    for raw in text.splitlines():
        line = raw.strip()
        if buf_key is not None:
            buf_val += " " + line
            if buf_val.count("[") <= buf_val.count("]"):
                flush()
            continue
        if not line or line.startswith("#"):
            continue
        if line.startswith("[[") and line.endswith("]]"):
            line = "[" + line[2:-2] + "]"
        if line.startswith("[") and line.endswith("]"):
            flush()
            section = root
            for part in line[1:-1].strip().split("."):
                section = section.setdefault(part.strip(), {})
            continue
        if section is None or "=" not in line:
            continue
        key, _, val = line.partition("=")
        key, val = key.strip(), val.strip()
        if val.startswith("[") and val.count("[") > val.count("]"):
            buf_key, buf_val = key, val
            continue
        section[key] = _parse_toml_value(val)
    flush()
    return root


_speed_config = None


def speed_config():
    """[speed] section of ~/.grok-local/config.toml merged over defaults."""
    global _speed_config
    if _speed_config is not None:
        return _speed_config
    cfg = dict(SPEED_DEFAULTS)
    try:
        parsed = _parse_toml_subset(CONFIG_TOML.read_text())
        speed = parsed.get("speed", {})
        if isinstance(speed, dict):
            for k, v in speed.items():
                if k in ("tier_models", "tier_efforts") and isinstance(v, dict):
                    merged = dict(SPEED_DEFAULTS[k])
                    merged.update(v)
                    cfg[k] = merged
                else:
                    cfg[k] = v
    except Exception as exc:
        cfg["_config_error"] = str(exc)
    _speed_config = cfg
    return cfg


_lmstudio_cache = {"at": 0.0, "models": []}


def lmstudio_library():
    """Model ids from LM Studio's OpenAI-compatible /v1/models (the library,
    not just loaded models). Cached 120s. Fail-open []."""
    now = time.time()
    if _lmstudio_cache["models"] and now - _lmstudio_cache["at"] < 120:
        return _lmstudio_cache["models"]
    try:
        req = urllib.request.Request(LMSTUDIO_URL + "/v1/models", method="GET")
        with urllib.request.urlopen(req, timeout=5) as r:
            data = json.loads(r.read().decode("utf-8"))
        ids = [m.get("id") for m in data.get("data", []) if m.get("id")]
        _lmstudio_cache.update(at=now, models=ids)
        return ids
    except Exception:
        return _lmstudio_cache["models"]


_loaded_model_cache = {"at": 0.0, "model": None}


def lmstudio_loaded_model():
    """Best-effort: which model LM Studio currently has loaded (via `lms ps`).
    Purely observational -- NEVER loads or unloads anything. Cached 120s."""
    now = time.time()
    if now - _loaded_model_cache["at"] < 120:
        return _loaded_model_cache["model"]
    model = None
    try:
        p = subprocess.run([LMS_BIN, "ps"], capture_output=True, text=True, timeout=15)
        for line in p.stdout.splitlines():
            line = line.strip()
            if not line or line.startswith("IDENTIFIER"):
                continue
            parts = line.split()
            if parts:
                model = parts[0]
                break
    except Exception:
        model = None
    _loaded_model_cache.update(at=now, model=model)
    return model


def _cache_route(session_id, decision):
    if session_id:
        _systemone_cache[session_id] = decision
    return decision


_systemone_cache = {}


def _pick_local_model(tier, model_id):
    """Map a SystemOne tier/registry model id to an LM Studio library model id.

    The adapter never triggers a model load; the currently loaded model is
    always acceptable. No ids are hard-coded: explicit tier_models config
    entries (if any) resolve against the live library, else the loaded model
    wins. Returns None when nothing matches."""
    cfg = speed_config()
    library = set(lmstudio_library())
    if model_id and model_id in library:
        return model_id
    mapped = (cfg.get("tier_models") or {}).get(tier or "")
    if mapped and mapped in library:
        return mapped
    loaded = lmstudio_loaded_model()
    if loaded:
        return loaded
    planner = cfg.get("planner_model")
    return planner if planner in library else None


# Canonical caps per reasoning effort. tier_efforts entries may be a plain
# effort string (legacy) or a dict overriding any of these fields.
_EFFORT_CANONICAL = {
    "low": {"effort": "low", "max_turns": 4, "ralph_cap": 4},
    "medium": {"effort": "medium", "max_turns": 6, "ralph_cap": 8},
    "high": {"effort": "high", "max_turns": 10, "ralph_cap": 12},
}

# Heuristic: keyword overlap between task text/task_labels and configured
# MCP server names/descriptions. name token = 2 pts, description token = 1 pt.
_SUGGEST_STOPWORDS = frozenset(
    "the a an and or of to in on for with as at by from is are was were be "
    "do does did will would can could should have has had it its this that "
    "these those you your we they them his her our their my me i not no yes "
    "if then than so such too very just also only use using used via per "
    "within into out up down over under again once here there when where "
    "which who whom whose what why how all any each every some more most "
    "other own same now don".split())

_SUGGESTION_NOTE = (
    "Advisory only: the grok agent merges per-session servers with its "
    "configured servers (verified 2026-09-23) -- nothing agent-side can "
    "suppress configured servers, so this is a recommendation the caller "
    "can act on, not enforcement. An empty list means no confident match: "
    "use everything (fail-open).")


def _tier_caps(tier, cfg):
    """Normalize a tier_efforts entry to {effort, max_turns, ralph_cap}.

    Accepts a plain effort string (v0.5.x legacy) or a dict. Unknown tiers
    fall back to the high tier's caps (fail-open). Never raises."""
    entry = (cfg.get("tier_efforts") or {}).get(tier or "")
    effort = entry if isinstance(entry, str) else (entry or {}).get("effort")
    caps = dict(_EFFORT_CANONICAL.get(effort, _EFFORT_CANONICAL["high"]))
    if isinstance(entry, dict):
        for key in ("max_turns", "ralph_cap"):
            if entry.get(key) is not None:
                try:
                    caps[key] = int(entry[key])
                except (TypeError, ValueError):
                    pass
    return caps


def _mcp_server_inventory():
    """Name + description for configured MCP servers from config.toml
    [mcp_servers] (the same source the grok CLI reads). Fail-open []."""
    try:
        parsed = _parse_toml_subset(CONFIG_TOML.read_text())
    except Exception:
        return []
    servers = parsed.get("mcp_servers", {}) or {}
    out = []
    for name, srv in servers.items():
        if not isinstance(srv, dict):
            continue
        if srv.get("enabled", True) is False:
            continue
        desc = srv.get("description") or srv.get("command") or ""
        out.append({"name": name, "description": str(desc)})
    return out


def _suggest_mcp_servers(task_text, task_labels):
    """Rank configured MCP servers against the task text + SystemOne
    task_labels by simple transparent keyword overlap (see _SUGGEST_STOPWORDS
    and scoring in the docstring note below).

    A server scores 2 points per overlapping token found in its name and 1
    point per token found in its description/command. Only servers with a
    score > 0 are returned, ranked by score desc (ties broken by name).
    Returns [] when nothing matches -- that means "no confident suggestion,
    use everything" (fail-open), NOT "use none".
    """
    inventory = _mcp_server_inventory()
    if not inventory:
        return []
    tokens = set(w for w in re.findall(r"[a-z0-9]{3,}", str(task_text).lower())
                 if w not in _SUGGEST_STOPWORDS)
    tokens |= set(str(lb).lower() for lb in (task_labels or []) if lb)
    scored = []
    for srv in inventory:
        name_tokens = set(re.findall(r"[a-z0-9]{3,}",
                                     srv["name"].lower().replace("_", " ").replace("-", " ")))
        desc_tokens = set(re.findall(r"[a-z0-9]{3,}", srv["description"].lower()))
        matched = sorted(tokens & (name_tokens | desc_tokens))
        score = sum(2 if t in name_tokens else 1 for t in matched)
        if score:
            scored.append({"name": srv["name"], "score": score, "matched": matched})
    scored.sort(key=lambda d: (-d["score"], d["name"]))
    return scored


def _parse_second_opinion(route):
    """Decider-backed second opinion on uncertain routes, parsed from the
    historical `jeff1_second_opinion` wire key. Advisory only -- never
    changes the routed tier. User-facing keys and text call it a "second
    opinion", never "Jeff-1"."""
    raw = route.get("jeff1_second_opinion")
    if not isinstance(raw, dict):
        return None
    tier = raw.get("tier")
    if not isinstance(tier, str) or not tier:
        return None
    conf = raw.get("confidence")
    return {
        "tier": tier,
        "confidence": conf if isinstance(conf, (int, float)) else None,
        "agree": bool(raw.get("agree", False)),
        "rationale": str(raw.get("rationale") or ""),
    }


def _records_path():
    return GROK_LOCAL_HOME / "systemone" / "decision_records.jsonl"


def _record_route_decision(decision, kind):
    """Append one SystemOne-compatible decision record (JSONL). Fail-open:
    every I/O error is swallowed -- logging never breaks the route.

    Shape follows systemone/battery/fit_types.py rows
    ({"type", "gold", "logits"|"probs"}). `gold` is omitted: the correct
    tier isn't knowable at route time, and fabricating it would poison the
    temperature fit."""
    try:
        cal = decision.get("calibrated_probabilities") or {}
        probs = [p for _, p in sorted(cal.items(), key=lambda kv: kv[1],
                                      reverse=True)
                 if isinstance(p, (int, float))]
        record = {
            "type": "choice",
            "probs": [float(p) for p in probs],
            "ts": int(time.time()),
            "client": "grok-local-acp-adapter",
            "task_kind": kind,
            "source": decision.get("source"),
            "tier": decision.get("tier"),
            "selected": decision.get("tier"),
            "confidence": decision.get("confidence"),
            "margin": decision.get("margin"),
            "uncertain": decision.get("uncertain"),
        }
        path = _records_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        with open(path, "a") as f:
            f.write(json.dumps(record) + "\n")
    except Exception:
        pass


def _rank_plans_url(route_url):
    """Derive a /v1/systemone/rank-plans URL from a configured /route URL.
    Returns None for unrecognized paths (never invent one we don't know)."""
    if "/v1/systemone/route" in route_url:
        return route_url.replace("/v1/systemone/route", "/v1/systemone/rank-plans")
    return None


def _rank_plans(task_desc, plans):
    """Rank candidate plans via SystemOne POST /v1/systemone/rank-plans.

    `plans` is a list of {"id", "text"} dicts. Returns (winner_index,
    rankings): the winner is the input index with the highest score;
    fail-open -- on any error the winner is 0 and every ranking carries
    score None (input order preserved, the original first plan runs).
    A single plan always wins; the tool requires two or more."""
    cfg = speed_config()
    fail = [({"id": p.get("id", str(i)), "score": None, "p_success": None,
              "cost_penalty": None, "est_steps": None})
            for i, p in enumerate(plans)]
    if not plans or not cfg.get("enabled", True):
        return 0, fail
    body = json.dumps({
        "task": str(task_desc)[:1500],
        "plans": [{"id": p.get("id", str(i)),
                   "text": str(p.get("text", ""))[:2000]}
                  for i, p in enumerate(plans)],
        "client": NAME,
    }).encode()
    for url in cfg.get("systemone_urls", []):
        rp_url = _rank_plans_url(url)
        if not rp_url:
            continue
        try:
            req = urllib.request.Request(rp_url, data=body, method="POST",
                                         headers={"Content-Type": "application/json"})
            with urllib.request.urlopen(req, timeout=cfg.get("systemone_timeout", 3)) as r:
                payload = json.loads(r.read().decode("utf-8"))
            ranked = payload.get("rankings") or payload.get("ranked_plans") or []
            by_id = {str(r.get("id")): r for r in ranked
                     if isinstance(r, dict) and r.get("id") is not None}
            rankings = []
            for i, p in enumerate(plans):
                pid = str(p.get("id", str(i)))
                r = by_id.get(pid, {})
                score = r.get("score")
                rankings.append({
                    "id": pid,
                    "score": score if isinstance(score, (int, float)) else None,
                    "p_success": r.get("p_success"),
                    "cost_penalty": r.get("cost_penalty"),
                    "est_steps": r.get("est_steps"),
                })
            scored = [(i, r["score"]) for i, r in enumerate(rankings)
                      if isinstance(r["score"], (int, float))]
            winner = max(scored, key=lambda t: t[1])[0] if scored else 0
            return winner, rankings
        except Exception:
            continue
    return 0, fail


def systemone_route(task_desc, kind="prompt", session_id=None):
    """Ask SystemOne for a routing decision for a task.

    Tries each configured URL (3s timeout). On ANY error/timeout returns a
    fail-open decision from config defaults and records the error. The
    decision maps the SystemOne tier to a grok CLI --reasoning-effort value,
    permission mode, and loop caps (max_turns for one-shot prompts,
    ralph_cap for ralph loop iterations); the model id is a local LM Studio
    id (advisory). The shim may also return an explicit `effort`
    ("low"|"medium"|"high") and `task_labels` -- consumed when present,
    derived locally otherwise (defensive: old shims return neither).
    Cached per session_id when given.
    """
    if session_id and session_id in _systemone_cache:
        return _systemone_cache[session_id]
    cfg = speed_config()
    fail_caps = dict(_EFFORT_CANONICAL.get(cfg.get("default_effort"),
                                           _EFFORT_CANONICAL["high"]))
    decision = {
        "source": "fail-open",
        "effort": fail_caps["effort"],
        "permission_mode": cfg.get("permission_mode_default", "auto"),
        "tier": None, "model_id": None, "rationale": None, "confidence": None,
        "local_model": None, "loaded_model": lmstudio_loaded_model(),
        "max_turns": fail_caps["max_turns"], "ralph_cap": fail_caps["ralph_cap"],
        "task_labels": [],
        "suggested_mcp_servers": [], "suggestion_detail": [],
        "suggestion_note": _SUGGESTION_NOTE,
        "calibrated_probabilities": {}, "margin": None, "uncertain": None,
        "ranked_models": [], "second_opinion": None,
        "error": None,
    }
    if not cfg.get("enabled", True):
        decision["error"] = "speed stack disabled in config"
        decision["local_model"] = decision["loaded_model"] or cfg.get("planner_model")
        _record_route_decision(decision, kind)
        return _cache_route(session_id, decision)
    body = json.dumps({"task": str(task_desc)[:500], "kind": kind,
                       "client": NAME}).encode()
    last_err = None
    for url in cfg.get("systemone_urls", []):
        try:
            req = urllib.request.Request(url, data=body, method="POST",
                                         headers={"Content-Type": "application/json"})
            with urllib.request.urlopen(req, timeout=cfg.get("systemone_timeout", 3)) as r:
                payload = json.loads(r.read().decode("utf-8"))
            route = payload.get("route", {}) or {}
            tier = str(route.get("tier", "")).lower()
            model_id = route.get("model_id") or payload.get("model")
            caps = _tier_caps(tier, cfg)
            shim_effort = route.get("effort")
            if shim_effort in _EFFORT_CANONICAL:
                # Shim knows best: an explicit effort overrides the tier caps.
                caps = dict(_EFFORT_CANONICAL[shim_effort])
            task_labels = [str(lb) for lb in (route.get("task_labels") or []) if lb]
            suggestions = _suggest_mcp_servers(task_desc, task_labels)
            cal = route.get("calibrated_probabilities")
            decision.update(
                source="systemone", url=url, tier=tier or None, model_id=model_id,
                rationale=route.get("rationale"), confidence=route.get("confidence"),
                effort=caps["effort"], max_turns=caps["max_turns"],
                ralph_cap=caps["ralph_cap"],
                local_model=_pick_local_model(tier, model_id),
                task_labels=task_labels,
                suggested_mcp_servers=[s["name"] for s in suggestions],
                suggestion_detail=suggestions,
                permission_mode=route.get("permission_mode") or cfg.get("permission_mode_default", "auto"),
                calibrated_probabilities={str(k): v for k, v in cal.items()
                                          if isinstance(v, (int, float))}
                if isinstance(cal, dict) else {},
                margin=route.get("margin") if isinstance(route.get("margin"), (int, float)) else None,
                uncertain=route.get("uncertain") if isinstance(route.get("uncertain"), bool) else None,
                ranked_models=[str(m.get("model_id")) for m in (route.get("ranked_models") or [])
                               if isinstance(m, dict) and m.get("model_id")],
                second_opinion=_parse_second_opinion(route))
            _record_route_decision(decision, kind)
            return _cache_route(session_id, decision)
        except Exception as exc:
            last_err = "%s: %s" % (url, exc)
    decision["error"] = last_err or "no systemone urls configured"
    decision["local_model"] = decision["loaded_model"] or cfg.get("planner_model")
    _record_route_decision(decision, kind)
    return _cache_route(session_id, decision)


def _grok_one_shot(prompt, cwd, effort="auto", timeout=300, max_turns=None,
                   permission_mode=None, extra_args=None, task_hint="prompt"):
    """Run one bounded headless turn. effort='auto' asks SystemOne (fail-open
    to the config default); the route decision supplies --reasoning-effort,
    --max-turns (when max_turns is unset), and ralph_cap. An explicit
    max_turns always wins. The adapter never loads or unloads models --
    inference runs on whatever model is loaded (the standing no-unload
    rule)."""
    cfg = speed_config()
    if effort == "auto":
        route = systemone_route(prompt, kind=task_hint)
        effort = route.get("effort") or cfg.get("default_effort", "high")
        if max_turns is None:
            max_turns = route.get("max_turns") or _tier_caps(None, cfg)["max_turns"]
        if permission_mode is None:
            permission_mode = route.get("permission_mode")
    if max_turns is None:
        max_turns = 4  # legacy bound for explicit-effort calls
    if permission_mode is None:
        permission_mode = cfg.get("permission_mode_default", "auto")
    if not isinstance(cwd, str) or not os.path.isdir(cwd):
        raise ValueError("cwd must be an existing directory")
    cmd = [GROK, "--single", prompt, "--output-format", "plain",
           "--max-turns", str(int(max_turns)),
           "--reasoning-effort", str(effort),
           "--permission-mode", str(permission_mode)]
    cmd.extend(extra_args or [])
    p = subprocess.run(cmd, cwd=cwd, env=os.environ.copy(), text=True,
                       capture_output=True, timeout=min(int(timeout), MAX_TIMEOUT))
    out = p.stdout.strip() + (("\n\n[stderr]\n" + p.stderr.strip()) if p.stderr.strip() else "")
    if p.returncode:
        raise RuntimeError(out[-12000:])
    return out or "(Grok returned no text)"


def _resolve_mcp_allowlist(names):
    """Resolve server NAMES from config.toml [mcp_servers.*] to ACP stdio
    entries. Servers with enabled=false are skipped. NOTE (verified
    2026-09-23): the grok agent merges per-session entries with its configured
    servers -- session/new cannot suppress configured servers, so an allowlist
    currently ADDS per-session servers rather than pruning. True pruning
    awaits agent support."""
    try:
        parsed = _parse_toml_subset(CONFIG_TOML.read_text())
    except Exception as exc:
        raise ValueError("could not read %s: %s" % (CONFIG_TOML, exc))
    servers = parsed.get("mcp_servers", {})
    entries = []
    for name in names:
        srv = servers.get(name)
        if not isinstance(srv, dict):
            raise ValueError("unknown mcp server name: %r (known: %s)" % (name, sorted(servers)))
        if srv.get("enabled", True) is False:
            continue
        entry = {"name": name}
        if srv.get("command"):
            entry["command"] = srv["command"]
        if srv.get("args"):
            entry["args"] = srv["args"]
        if srv.get("env"):
            entry["env"] = srv["env"]
        entries.append(entry)
    return entries


def _mcp_servers_arg(args):
    """Resolve the mcp_servers tool argument.

    'auto'/unset -> config default_mcp_servers ("auto" keeps repaired
    behavior). A list of all-strings -> server NAMES resolved via
    _resolve_mcp_allowlist. A list containing dicts -> raw ACP entries
    passed through (repaired v0.4.0 behavior).
    """
    mcp = args.get("mcp_servers", "auto")
    if mcp is None or mcp == "auto":
        mcp = speed_config().get("default_mcp_servers", "auto")
    if isinstance(mcp, list) and mcp and all(isinstance(x, str) for x in mcp):
        mcp = _resolve_mcp_allowlist(mcp)
    return mcp


def _safe_sid(sid):
    return re.sub(r"[^A-Za-z0-9_.-]", "_", str(sid))


_PATH_RE = re.compile(r"(?:^|[\s\"\'`(\[])([\w\-.~]+/[\w\-./]+(?:\.[\w]+)?)")
_ERROR_RE = re.compile(r"(?i)(error|failed|failure|traceback|exception|denied|timeout)[^\n]{0,200}")


def _anchored_summary(events):
    """Build an anchored compaction summary: permission decisions, file paths
    touched, error messages, plus head/tail of activity. Deterministic."""
    texts = []
    for e in events:
        try:
            texts.append(json.dumps(e.get("params", {}), ensure_ascii=False))
        except Exception:
            pass
    blob = "\n".join(texts)
    paths = []
    for m in _PATH_RE.finditer(blob):
        p = m.group(1).strip(".,;:")
        if p not in paths:
            paths.append(p)
    errors = []
    for m in _ERROR_RE.finditer(blob):
        s = m.group(0).strip()
        if s not in errors:
            errors.append(s)
    decisions = [e for e in events if e.get("type") == "permission_decision"]
    chunks = [t for t in texts if len(t) > 40]
    lines = ["# Anchored session summary", "Events summarized: %d" % len(events),
             "", "## Permission decisions"]
    for d in decisions[-20:]:
        lines.append("- %s -> %s" % (d.get("request_id"), d.get("decision")))
    if not decisions:
        lines.append("(none recorded)")
    lines += ["", "## File paths touched (%d)" % len(paths)]
    lines += ["- " + p for p in paths[:40]]
    lines += ["", "## Errors / failures (%d)" % len(errors)]
    lines += ["- " + e for e in errors[:20]]
    lines += ["", "## First activity", "\n".join(chunks[:3])[:3000],
              "", "## Most recent activity", "\n".join(chunks[-3:])[:3000]]
    return "\n".join(lines)


def _compact_session(args):
    cfg = speed_config()
    s = get_session(args.get("session_id"))
    events = list(s.transcript)
    tdir = cfg.get("transcript_dir", str(GROK_LOCAL_HOME / "transcripts"))
    os.makedirs(tdir, exist_ok=True)
    tpath = os.path.join(tdir, "%s.jsonl" % _safe_sid(s.session_id))
    with open(tpath, "w") as f:
        for e in events:
            f.write(json.dumps(e, ensure_ascii=False) + "\n")
    summary_events = events + [
        {"type": "permission_decision", "request_id": d["request_id"],
         "decision": d["decision"], "t": d["t"]} for d in s.decisions]
    summary = _anchored_summary(summary_events)
    s.transcript.clear()
    s.transcript.append({"type": "compaction_summary", "t": time.time(),
                         "summary": summary, "transcript_file": tpath,
                         "events_archived": len(events)})
    return {"session_id": s.session_id, "transcript_file": tpath,
            "events_archived": len(events), "summary": summary,
            "note": "Adapter-side transcript compacted. The agent process keeps its own "
                    "context; for a true context reset, close this session and start a "
                    "fresh one seeded with the summary."}


def _ralph_run(args):
    """Ralph loop: fresh one-shot iterations sharing a progress file.

    Each iteration starts with a fresh context; the progress file is the only
    state carrier. Stops on done_marker, a passing test_command, a stall
    (byte-identical output twice in a row), iteration error, or the cap.
    The SystemOne route decision supplies the effort, the ralph iteration
    cap (ralph_cap) and the per-iteration max_turns when the caller leaves
    them unset; explicit args always win (fail-open uses the high tier).
    """
    cfg = speed_config()
    task = args.get("task")
    if not isinstance(task, str) or not task.strip():
        raise ValueError("task must be non-empty")
    cwd = args.get("cwd", DEFAULT_CWD)
    if not isinstance(cwd, str) or not os.path.isdir(cwd):
        raise ValueError("cwd must be an existing directory")
    route = systemone_route(task, kind="ralph")
    max_iterations = int(args.get("max_iterations") or route.get("ralph_cap")
                         or cfg.get("ralph_max_iterations", 10))
    done_marker = args.get("done_marker", cfg.get("ralph_done_marker", "DONE"))
    test_command = args.get("test_command")
    progress_dir = args.get("progress_dir", cfg.get("ralph_progress_dir"))
    os.makedirs(progress_dir, exist_ok=True)
    progress_file = args.get("progress_file") or os.path.join(
        progress_dir, "ralph-%s.md" % time.strftime("%Y%m%d-%H%M%S"))
    if not os.path.exists(progress_file):
        with open(progress_file, "w") as f:
            f.write("# Ralph run\n\nTask: %s\n\nStarted: %s\n"
                    % (task.strip(), time.strftime("%Y-%m-%d %H:%M:%S")))
    effort = route.get("effort") or cfg.get("default_effort", "high")
    timeout = int(args.get("timeout", 300))
    max_turns = int(args.get("max_turns") or route.get("max_turns") or 8)
    prev_output, stopped, last_output, it = None, "max_iterations", "", 0
    for it in range(1, max_iterations + 1):
        with open(progress_file) as f:
            progress = f.read()
        prompt = (
            "You are one iteration of a Ralph loop on a long-horizon task. Read the "
            "progress context below, then do ONLY the next concrete step.\n\n"
            "## Progress so far\n%s\n\n---\n"
            "Iteration %d of %d. Task: %s\n"
            "Rules: make one focused change; verify it if you can; then append a short "
            "dated entry to the progress file at %s describing what you did and the next "
            "step. When the task is fully complete, your response MUST contain the line: %s"
            % (progress[-12000:], it, max_iterations, task.strip(), progress_file, done_marker))
        try:
            output = _grok_one_shot(prompt, cwd, effort=effort, timeout=timeout,
                                    max_turns=max_turns, task_hint="ralph")
        except Exception as exc:
            output = "[iteration %d error: %s]" % (it, exc)
        last_output = output
        with open(progress_file, "a") as f:
            f.write("\n\n## Iteration %d (%s)\n\n%s\n"
                    % (it, time.strftime("%Y-%m-%d %H:%M:%S"), output))
        if done_marker in output:
            stopped = "done_marker"
            break
        if test_command:
            tp = subprocess.run(test_command, shell=True, cwd=cwd, capture_output=True,
                                text=True, timeout=min(timeout, 600))
            with open(progress_file, "a") as f:
                f.write("\n[Test `%s` exit=%d]\n%s\n"
                        % (test_command, tp.returncode, (tp.stdout + tp.stderr)[-2000:]))
            if tp.returncode == 0:
                stopped = "test_passed"
                break
        if prev_output is not None and output == prev_output:
            stopped = "stall_no_progress"
            break
        prev_output = output
    return {"task": task.strip(), "iterations": it, "stopped": stopped,
            "progress_file": progress_file, "effort": effort,
            "max_iterations": max_iterations, "max_turns": max_turns,
            "systemone": {"tier": route.get("tier"), "source": route.get("source")},
            "last_output_tail": last_output[-3000:]}


def _plan_then_execute(args):
    """Plan-then-execute: a high-effort planner writes a plan artifact, then a
    fast executor runs it. Both roles use the loaded local LM Studio model --
    no model is ever loaded or unloaded.

    When `candidate_plans` (a list of 2+ plan texts) is supplied, the plans
    are ranked via SystemOne `POST /v1/systemone/rank-plans` and the winner
    is executed (fail-open: original-first on any ranking error). Otherwise
    the planner generates a single plan as before."""
    cfg = speed_config()
    task = args.get("task")
    if not isinstance(task, str) or not task.strip():
        raise ValueError("task must be non-empty")
    cwd = args.get("cwd", DEFAULT_CWD)
    if not isinstance(cwd, str) or not os.path.isdir(cwd):
        raise ValueError("cwd must be an existing directory")
    plans_dir = cfg.get("plans_dir", str(GROK_LOCAL_HOME / "plans"))
    os.makedirs(plans_dir, exist_ok=True)
    plan_path = args.get("plan_path") or os.path.join(
        plans_dir, "plan-%s.md" % time.strftime("%Y%m%d-%H%M%S"))
    loaded = lmstudio_loaded_model()
    candidates = args.get("candidate_plans")
    ranking = None
    if isinstance(candidates, list) and len(candidates) >= 2:
        plan_inputs = [{"id": "plan-%d" % i, "text": str(c)}
                       for i, c in enumerate(candidates) if str(c).strip()]
        if len(plan_inputs) >= 2:
            winner, rankings = _rank_plans(task.strip(), plan_inputs)
            plan = plan_inputs[winner]["text"]
            ranking = {"winner": plan_inputs[winner]["id"], "rankings": rankings}
        else:
            plan = str(candidates[0])
    elif isinstance(candidates, list) and len(candidates) == 1:
        plan = str(candidates[0])
    else:
        plan_prompt = (
            "Write a concrete, step-by-step execution plan for the task below. Output ONLY "
            "the plan as markdown (numbered steps, files involved, how to verify each step). "
            "Do not execute anything.\n\nTask: %s" % task.strip())
        plan = _grok_one_shot(plan_prompt, cwd, effort=cfg.get("planner_effort", "high"),
                              timeout=int(args.get("plan_timeout", 300)),
                              max_turns=int(args.get("plan_max_turns", 6)), task_hint="plan")
    with open(plan_path, "w") as f:
        f.write("# Plan\n\nTask: %s\n\nPlanner effort: %s\n\n%s\n"
                % (task.strip(), cfg.get("planner_effort", "high"), plan))
    exec_route = systemone_route(task, kind="execute")
    exec_effort = exec_route.get("effort") or cfg.get("executor_effort", "low")
    exec_prompt = ("Execute the plan below step by step, verifying each step as you go.\n\n"
                   "## Plan\n%s\n\n## Original task\n%s" % (plan, task.strip()))
    result = _grok_one_shot(exec_prompt, cwd, effort=exec_effort,
                            timeout=int(args.get("timeout", 600)),
                            max_turns=int(args.get("max_turns", 10)), task_hint="execute")
    planner_model = cfg.get("planner_model") or loaded
    out = {"task": task.strip(), "plan_path": plan_path, "plan": plan,
           "planner": {"model": planner_model, "effort": cfg.get("planner_effort", "high")},
           "executor": {"model": loaded or planner_model, "effort": exec_effort,
                        "systemone_tier": exec_route.get("tier"),
                        "systemone_source": exec_route.get("source")},
           "result": result,
           "note": "Planner and executor both ran on the loaded LM Studio model%s; "
                   "no model was loaded or unloaded." % (" (%s)" % loaded if loaded else "")}
    if ranking is not None:
        out["plan_ranking"] = ranking
    return out


def _slash_verify(args):
    """Build + test in the session cwd. Returns pass/fail plus output tails."""
    sid = args.get("session_id")
    cwd = args.get("cwd")
    if not cwd and sid:
        try:
            cwd = get_session(sid).cwd
        except Exception:
            cwd = None
    cwd = cwd or DEFAULT_CWD
    if not isinstance(cwd, str) or not os.path.isdir(cwd):
        raise ValueError("cwd must be an existing directory")
    build_cmd = args.get("build_command")
    test_cmd = args.get("test_command")
    if not build_cmd and not test_cmd:
        if os.path.exists(os.path.join(cwd, "package.json")):
            build_cmd, test_cmd = "npm run build", "npm test"
        elif os.path.exists(os.path.join(cwd, "Makefile")):
            build_cmd, test_cmd = "make", "make test"
        elif any(os.path.exists(os.path.join(cwd, f))
                 for f in ("pyproject.toml", "setup.py", "pytest.ini", "tox.ini")):
            test_cmd = "python3 -m pytest -x -q"
        else:
            return {"cwd": cwd, "pass": False,
                    "error": "no build/test commands given and none auto-detected "
                             "(looked for package.json, Makefile, pyproject.toml/setup.py)"}
    timeout = int(args.get("timeout", 300))
    result = {"cwd": cwd, "pass": True}
    if build_cmd:
        bp = subprocess.run(build_cmd, shell=True, cwd=cwd, capture_output=True,
                            text=True, timeout=timeout)
        bout = (bp.stdout + "\n" + bp.stderr).strip()
        result["build"] = {"command": build_cmd, "ok": bp.returncode == 0,
                           "exit": bp.returncode, "tail": bout[-4000:] or "(no output)"}
        if bp.returncode != 0:
            result["pass"] = False
            result["test"] = {"skipped": True, "reason": "build failed"}
            return result
    if test_cmd:
        tp = subprocess.run(test_cmd, shell=True, cwd=cwd, capture_output=True,
                            text=True, timeout=timeout)
        tout = (tp.stdout + "\n" + tp.stderr).strip()
        result["test"] = {"command": test_cmd, "ok": tp.returncode == 0,
                          "exit": tp.returncode, "tail": tout[-4000:] or "(no output)"}
        if tp.returncode != 0:
            result["pass"] = False
    return result


def _slash_review(args):
    """Heuristic review over the session's retained transcript + git state."""
    sid = args.get("session_id")
    s = get_session(sid) if sid else None
    cwd = args.get("cwd") or (s.cwd if s else DEFAULT_CWD)
    findings = []
    events = list(s.transcript)[-100:] if s else []
    blob = "\n".join(json.dumps(e.get("params", {}), ensure_ascii=False)[:500] for e in events)
    for pat, label in [(r"(?i)traceback[^\n]*", "traceback"),
                       (r"(?i)\berror\b[^\n]{0,120}", "error"),
                       (r"(?i)failed[^\n]{0,120}", "failure"),
                       (r"(?i)warning[^\n]{0,120}", "warning")]:
        for m in list(re.finditer(pat, blob))[:10]:
            findings.append({"kind": label, "text": m.group(0).strip()[:300]})
    if s:
        for d in s.decisions:
            if d["decision"] == "deny":
                findings.append({"kind": "permission_denied",
                                 "text": "request %s was denied" % d["request_id"]})
    diff_stat, git_status = None, None
    try:
        gp = subprocess.run(["git", "diff", "--stat"], cwd=cwd, capture_output=True,
                            text=True, timeout=30)
        if gp.returncode == 0 and gp.stdout.strip():
            diff_stat = gp.stdout.strip()[-3000:]
        sp = subprocess.run(["git", "status", "--short"], cwd=cwd, capture_output=True,
                            text=True, timeout=30)
        if sp.returncode == 0 and sp.stdout.strip():
            git_status = sp.stdout.strip()[-2000:]
    except Exception:
        pass
    return {"session_id": s.session_id if s else None, "cwd": cwd,
            "findings": findings[:40], "git_diff_stat": diff_stat,
            "git_status": git_status,
            "note": "Heuristic review over the retained adapter transcript + git state; "
                    "not a substitute for reading the code."}




def schema(properties=None, required=None):
    out = {"type": "object", "properties": properties or {}}
    if required:
        out["required"] = required
    return out


MCP_SERVERS_PROP = {
    "type": ["string", "array"],
    "default": "auto",
    "description": "'auto' (default): send an empty ACP mcpServers list so the agent uses its configured MCP servers from ~/.grok-local/config.toml (verified: the field is required and [] still connects configured servers). A list of server NAMES from config.toml [mcp_servers.*] is resolved to per-session ACP entries (opt-in allowlist; config default_mcp_servers). A list of dicts is passed through raw as ACP mcpServer entries. NOTE: the grok agent merges per-session entries with its configured servers -- session/new cannot suppress configured servers, so this ADDS per-session servers; true pruning awaits agent support.",
}


TOOLS = [
    {"name": "grok_local_prompt", "description": "Run one bounded, headless Grok Local agent turn using its configured MCP servers and return the response. When effort='auto' (default), SystemOne picks the reasoning effort AND the max_turns cap; an explicit max_turns always wins.", "inputSchema": schema({"prompt": {"type": "string"}, "cwd": {"type": "string"}, "timeout": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT, "default": 300}, "max_turns": {"type": "integer", "minimum": 1, "maximum": 10, "description": "Per-turn agent loop cap. Unset: use the SystemOne tier's max_turns (fail-open: high tier = 10)."}, "permission_mode": {"type": "string", "default": "auto"}, "allow_subagents": {"type": "boolean", "default": False}, "allow_plan": {"type": "boolean", "default": False}, "effort": {"type": "string", "default": "auto", "description": "'auto' (default): ask SystemOne for the reasoning effort and loop caps (fail-open to config defaults); or none/low/medium/high/max, passed as --reasoning-effort."}}, ["prompt"])},
    {"name": "grok_local_status", "description": "Return installed Grok Local and ACP bridge status.", "inputSchema": schema()},
    {"name": "grok_local_models", "description": "List models available to Grok Local (read-only).", "inputSchema": schema()},
    {"name": "grok_local_sessions", "description": "List recent Grok Local sessions (read-only).", "inputSchema": schema()},
    {"name": "grok_local_mcp_servers", "description": "List Grok Local's configured MCP servers (read-only).", "inputSchema": schema()},
    {"name": "grok_local_inspect", "description": "Inspect Grok Local configuration for a directory (read-only).", "inputSchema": schema({"cwd": {"type": "string"}})},
    {"name": "grok_local_doctor", "description": "Run Grok Local environment diagnostics (read-only).", "inputSchema": schema()},
    {"name": "grok_local_session_start", "description": "Start a persistent native ACP Grok Local session. mcp_servers 'auto' (default) lets the agent use its configured MCP servers; a list of ACP mcpServer entries is passed through as per-session servers.", "inputSchema": schema({"cwd": {"type": "string"}, "mcp_servers": MCP_SERVERS_PROP})},
    {"name": "grok_local_session_load", "description": "Load an existing Grok Local session by ID with history replay for follow-up work. mcp_servers 'auto' (default) lets the agent use its configured MCP servers; a list of ACP mcpServer entries is passed through as per-session servers.", "inputSchema": schema({"session_id": {"type": "string"}, "cwd": {"type": "string"}, "mcp_servers": MCP_SERVERS_PROP}, ["session_id"])},
    {"name": "grok_local_session_resume", "description": "Resume an existing Grok Local session without replaying its history when supported by ACP. mcp_servers 'auto' (default) lets the agent use its configured MCP servers; a list of ACP mcpServer entries is passed through as per-session servers.", "inputSchema": schema({"session_id": {"type": "string"}, "cwd": {"type": "string"}, "mcp_servers": MCP_SERVERS_PROP}, ["session_id"])},
    {"name": "grok_local_session_prompt", "description": "Queue a follow-up prompt on the persistent ACP session; poll events for output or approvals.", "inputSchema": schema({"prompt": {"type": "string"}, "session_id": {"type": "string"}}, ["prompt"])},
    {"name": "grok_local_session_events", "description": "Read queued ACP updates, completed turns, and pending permission requests.", "inputSchema": schema({"session_id": {"type": "string"}})},
    {"name": "grok_local_permission_decide", "description": "Explicitly accept, deny, or cancel a pending Grok tool permission request.", "inputSchema": schema({"request_id": {"type": "string"}, "decision": {"type": "string", "enum": ["accept", "deny", "cancel"]}, "option_id": {"type": "string"}}, ["request_id", "decision"])},
    {"name": "grok_local_session_cancel", "description": "Cancel the active turn in a persistent ACP session.", "inputSchema": schema({"session_id": {"type": "string"}})},
    {"name": "grok_local_session_close", "description": "Close a persistent ACP session and its local bridge process.", "inputSchema": schema({"session_id": {"type": "string"}})},
    {"name": "grok_local_session_transcript", "description": "Return the retained session/update event history (last 500) for a persistent ACP session.", "inputSchema": schema({"session_id": {"type": "string"}})},
    {"name": "grok_local_systemone_route", "description": "Ask SystemOne for a model-tier/effort routing decision for a task (fail-open to config defaults).", "inputSchema": schema({"task": {"type": "string"}, "kind": {"type": "string", "default": "prompt"}, "session_id": {"type": "string"}}, ["task"])},
    {"name": "grok_local_lmstudio_models", "description": "List LM Studio's model library and which model is currently loaded (observational only; never loads/unloads).", "inputSchema": schema()},
    {"name": "grok_local_tools_batch", "description": "Run multiple INDEPENDENT tool calls concurrently; results return in input order. No ordering guarantees between calls -- do not batch calls that depend on each other's outputs, multiple prompts to the same session, or nested tools_batch.", "inputSchema": schema({"calls": {"type": "array", "description": "List of {tool, arguments} objects. Max 16 per batch.", "items": {"type": "object"}}}, ["calls"])},
    {"name": "grok_local_ralph_run", "description": "Ralph loop: fresh one-shot iterations on a task sharing a progress file; stops on done_marker, a passing test_command, a stall (byte-identical output twice in a row), or the iteration cap. When max_iterations/max_turns are unset, SystemOne's ralph_cap/max_turns apply; explicit values always win.", "inputSchema": schema({"task": {"type": "string"}, "cwd": {"type": "string"}, "max_iterations": {"type": "integer", "description": "Iteration cap. Unset: SystemOne tier ralph_cap (fail-open: high tier = 12)."}, "done_marker": {"type": "string", "default": "DONE"}, "test_command": {"type": "string"}, "progress_file": {"type": "string"}, "timeout": {"type": "integer", "default": 300}, "max_turns": {"type": "integer", "description": "Per-iteration turn cap. Unset: SystemOne tier max_turns."}}, ["task"])},
    {"name": "grok_local_plan_then_execute", "description": "Plan-then-execute: a high-effort planner writes a plan artifact, then a fast executor runs it. Both roles use the loaded local LM Studio model; nothing is loaded or unloaded. Accepts optional candidate_plans (2+ plan texts) which are ranked via SystemOne; the winning plan is executed (fail-open: original-first).", "inputSchema": schema({"task": {"type": "string"}, "cwd": {"type": "string"}, "plan_path": {"type": "string"}, "timeout": {"type": "integer", "default": 600}, "max_turns": {"type": "integer", "default": 10}, "candidate_plans": {"type": "array", "description": "Optional 2+ candidate plan texts to rank; the winner is executed.", "items": {"type": "string"}}}, ["task"])},
    {"name": "grok_local_rank_plans", "description": "Rank 2+ candidate plans via SystemOne POST /v1/systemone/rank-plans. Returns {winner_index, rankings} with scores plus fail-open detail. On any error the winner is 0 and every ranking carries score null (input order preserved).", "inputSchema": schema({"task": {"type": "string"}, "plans": {"type": "array", "description": "Candidate plans, each {id, text}; ids must be unique.", "items": {"type": "object"}}}, ["task", "plans"])},
    {"name": "grok_local_compact_session", "description": "Anchored compaction of the adapter-side transcript: archives the full transcript to ~/.grok-local/transcripts/<session>.jsonl first, then replaces it with a summary preserving permission decisions, file paths touched, and errors.", "inputSchema": schema({"session_id": {"type": "string"}})},
    {"name": "grok_local_slash_verify", "description": "/verify: run build then test commands in the session cwd (auto-detected from package.json/Makefile/pyproject when omitted); returns pass/fail plus output tails.", "inputSchema": schema({"session_id": {"type": "string"}, "cwd": {"type": "string"}, "build_command": {"type": "string"}, "test_command": {"type": "string"}, "timeout": {"type": "integer", "default": 300}})},
    {"name": "grok_local_slash_review", "description": "/review: heuristic review pass over the session's recent transcript plus git diff/status; returns findings.", "inputSchema": schema({"session_id": {"type": "string"}, "cwd": {"type": "string"}})},
]


def reply(msg_id, result=None, error=None):
    out = {"jsonrpc": "2.0", "id": msg_id}
    out["error" if error else "result"] = error if error else result
    sys.stdout.write(json.dumps(out, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def text_result(value, is_error=False):
    if not isinstance(value, str):
        value = json.dumps(value, ensure_ascii=False)
    return {"content": [{"type": "text", "text": value}], "isError": is_error}


def _session_params(cwd, mcp_servers):
    """Build session/new|load|resume params.

    mcp_servers == "auto" (or unset): send an empty mcpServers list.
    Verified against the grok agent (2026-09-23): the mcpServers field is
    REQUIRED -- omitting it is rejected ("missing field mcpServers"), and []
    does NOT disable servers. The agent merges the list with its configured
    servers from ~/.grok-local/config.toml (confirmed via process
    inspection: both configured ssh MCP servers spawn with []). There is no
    supported way to disable configured servers via session/new
    (mcpInheritance=false was also tested and does not suppress them).
    A list is passed through raw as per-session ACP mcpServer entries
    (untagged Stdio/Http/Sse enum; exact entry schema not verified --
    consult the grok ACP docs).
    """
    params = {"cwd": cwd, "mcpServers": []}
    if mcp_servers is None or mcp_servers == "auto":
        return params
    if isinstance(mcp_servers, list):
        params["mcpServers"] = mcp_servers
        return params
    raise ValueError('mcp_servers must be "auto" or an array of ACP mcpServer entries')


class AcpSession:
    def __init__(self, cwd):
        self.cwd = cwd
        self.proc = None
        self.session_id = None
        self.next_id = 1
        self.write_lock = threading.Lock()
        self.pending = {}
        self.pending_lock = threading.Lock()
        self.events = queue.Queue()
        self.permissions = {}
        self.reported_permissions = set()
        self.stderr_tail = deque(maxlen=STDERR_TAIL_LINES)
        self.transcript = deque(maxlen=TRANSCRIPT_MAX_EVENTS)
        self.decisions = []
        self.alive = False
        self.reader = None

    def send(self, method, params=None, request=True):
        with self.write_lock:
            ident = self.next_id if request else None
            if request:
                self.next_id += 1
            msg = {"jsonrpc": "2.0", "method": method, "params": params or {}}
            if request:
                msg["id"] = ident
                ev = threading.Event()
                with self.pending_lock:
                    self.pending[ident] = {"event": ev, "method": method}
            self.proc.stdin.write(json.dumps(msg) + "\n")
            self.proc.stdin.flush()
        if not request:
            return None
        if not ev.wait(30):
            with self.pending_lock:
                self.pending.pop(ident, None)
            raise RuntimeError(f"ACP request timed out: {method}")
        # The reader thread pops the pending entry and attaches the response
        # to the waiter event before setting it; the pop below is idempotent
        # in case the response arrived after the timeout path already popped.
        response = getattr(ev, "response", None)
        with self.pending_lock:
            self.pending.pop(ident, None)
        if not response:
            raise RuntimeError(f"ACP request returned no response: {method}")
        if "error" in response:
            raise RuntimeError(json.dumps(response["error"], ensure_ascii=False))
        return response.get("result", {})

    def send_async(self, method, params=None):
        with self.write_lock:
            ident = self.next_id
            self.next_id += 1
            ev = threading.Event()
            with self.pending_lock:
                self.pending[ident] = {"event": ev, "method": method}
            self.proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": ident, "method": method, "params": params or {}}) + "\n")
            self.proc.stdin.flush()
        return ident

    def _reader(self):
        for line in self.proc.stdout:
            try:
                msg = json.loads(line)
            except Exception:
                continue
            if "id" in msg and "method" not in msg:
                # JSON-RPC response to one of our requests: pop the pending
                # entry (idempotent) and hand the response to the waiter.
                with self.pending_lock:
                    item = self.pending.pop(msg["id"], None)
                if item:
                    item["event"].response = msg
                    item["event"].set()
                    self.events.put({"type": "response", "request_id": msg["id"], "response": msg})
                continue
            method = msg.get("method", "")
            params = msg.get("params", {})
            if method == "session/request_permission":
                request_id = str(msg.get("id"))
                self.permissions[request_id] = {"rpc_id": msg.get("id"), "params": params, "created": time.time()}
                self.events.put({"type": "permission_request", "request_id": request_id, "params": params})
            elif method == "session/update":
                self.events.put({"type": "session_update", "params": params})
                self.transcript.append({"type": "session_update", "params": params, "t": time.time()})
            elif method.startswith("_") or method.startswith("x.ai/"):
                self.events.put({"type": "notification", "method": method, "params": params})
            else:
                self.events.put({"type": "notification", "method": method, "params": params})
        self.alive = False
        self.events.put({"type": "process_exit", "returncode": self.proc.poll()})

    def _spawn_agent(self):
        """Launch the grok agent subprocess and run ACP initialize."""
        env = os.environ.copy()
        env.setdefault("GROK_HOME", str(pathlib.Path.home() / ".grok-local"))
        self.proc = subprocess.Popen([GROK, "agent", "--no-leader", "stdio"], cwd=self.cwd, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1)
        self.alive = True
        self.reader = threading.Thread(target=self._reader, daemon=True)
        self.reader.start()
        threading.Thread(target=self._stderr, daemon=True).start()
        # v0.5.0: terminal False -- the adapter does not implement the ACP
        # client-RPC terminal/create calls; the agent runs shell commands
        # through its own local terminal either way.
        self.send("initialize", {"protocolVersion": 1, "clientCapabilities": {"fs": {"readTextFile": True, "writeTextFile": True}, "terminal": False}, "clientInfo": {"name": NAME, "version": VERSION}})
        self.send("notifications/initialized", {}, request=False)

    def _stderr(self):
        for line in self.proc.stderr:
            line = line.rstrip("\n")
            if line:
                self.stderr_tail.append(line)

    def close(self):
        if self.proc and self.proc.poll() is None:
            try:
                self.send("session/close", {"sessionId": self.session_id})
            except Exception:
                pass
            self.proc.terminate()

    def events_now(self):
        out = []
        while True:
            try: out.append(self.events.get_nowait())
            except queue.Empty: break
        for rid, request in self.permissions.items():
            if rid in self.reported_permissions:
                continue
            self.reported_permissions.add(rid)
            out.append({"type": "permission_pending", "request_id": rid, "params": request["params"]})
        return out


sessions = {}
sessions_lock = threading.Lock()


def get_session(session_id=None, cwd=None):
    with sessions_lock:
        if session_id and session_id in sessions: return sessions[session_id]
        if session_id: raise ValueError("unknown session_id; call grok_local_session_start or session_load")
        if sessions: return next(iter(sessions.values()))
        s = AcpSession(cwd or DEFAULT_CWD)
        try:
            s._spawn_agent()
            result = s.send("session/new", _session_params(s.cwd, "auto"))
            sid = result.get("sessionId")
            if not sid:
                raise RuntimeError("ACP session/new returned no sessionId")
            s.session_id = sid
        except Exception:
            s.close()
            raise
        sessions[sid] = s
        return s


def run_command(args, command, extra=None, timeout=60):
    args = args or {}; cwd = args.get("cwd", DEFAULT_CWD) if command == "inspect" else DEFAULT_CWD
    if not isinstance(cwd, str) or not os.path.isdir(cwd): raise ValueError("cwd must be an existing directory")
    cmd = [GROK, command] + (extra or [])
    p = subprocess.run(cmd, cwd=cwd, env=os.environ.copy(), text=True, capture_output=True, timeout=min(int(args.get("timeout", timeout)), 120))
    output = p.stdout.strip()
    if p.stderr.strip(): output += ("\n\n[stderr]\n" + p.stderr.strip()) if output else ("[stderr]\n" + p.stderr.strip())
    if p.returncode: raise RuntimeError(f"grok exited with code {p.returncode}:\n{output[-12000:]}")
    return output or "(Grok returned no output)"


def handle(name, args):
    args = args or {}
    if name == "grok_local_prompt":
        prompt = args.get("prompt"); cwd = args.get("cwd", DEFAULT_CWD)
        if not isinstance(prompt, str) or not prompt.strip(): raise ValueError("prompt must be non-empty")
        permission_mode = args.get("permission_mode", "auto")
        if not isinstance(permission_mode, str) or not permission_mode.strip():
            raise ValueError("permission_mode must be a non-empty string")
        extra = []
        if not args.get("allow_subagents", False):
            extra.append("--no-subagents")
        if not args.get("allow_plan", False):
            extra.append("--no-plan")
        return _grok_one_shot(prompt, cwd, effort=args.get("effort", "auto"),
                              timeout=args.get("timeout", 300),
                              max_turns=args.get("max_turns"),
                              permission_mode=permission_mode,
                              extra_args=extra, task_hint="prompt")
    if name == "grok_local_status":
        with sessions_lock:
            details = {sid: {"cwd": s.cwd, "alive": s.alive,
                             "last_stderr": list(s.stderr_tail)}
                       for sid, s in sessions.items()}
        return {"adapter": VERSION, "grok_bin": GROK, "cwd": DEFAULT_CWD,
                "active_sessions": len(sessions), "sessions": list(details),
                "session_details": details}
    if name == "grok_local_models": return run_command(args, "models")
    if name == "grok_local_sessions": return run_command(args, "sessions", ["list"])
    if name == "grok_local_mcp_servers": return run_command(args, "mcp", ["list"])
    if name == "grok_local_inspect": return run_command(args, "inspect", ["--json"])
    if name == "grok_local_doctor": return run_command(args, "doctor")
    if name == "grok_local_session_start":
        s = AcpSession(args.get("cwd", DEFAULT_CWD))
        try:
            s._spawn_agent()
            result = s.send("session/new", _session_params(s.cwd, _mcp_servers_arg(args)))
            sid = result.get("sessionId")
            if not sid:
                raise RuntimeError("ACP session/new returned no sessionId")
            s.session_id = sid
        except Exception:
            s.close()
            raise
        with sessions_lock: sessions[sid] = s
        route = systemone_route("interactive coding session in %s" % s.cwd,
                                kind="session", session_id=sid)
        return {"session_id": sid, "cwd": s.cwd, "status": "ready",
                "systemone": {"tier": route.get("tier"), "effort": route.get("effort"),
                              "local_model": route.get("local_model"),
                              "loaded_model": route.get("loaded_model"),
                              "source": route.get("source"), "error": route.get("error"),
                              "suggested_mcp_servers": route.get("suggested_mcp_servers"),
                              "suggestion_note": route.get("suggestion_note")}}
    if name == "grok_local_session_load":
        sid = args["session_id"]
        s = AcpSession(args.get("cwd", DEFAULT_CWD))
        # Start a fresh ACP client, then load the persisted session ID.
        try:
            s._spawn_agent()
            params = _session_params(s.cwd, _mcp_servers_arg(args))
            params["sessionId"] = sid
            s.send("session/load", params)
            s.session_id = sid
        except Exception:
            s.close()
            raise
        with sessions_lock: sessions[sid] = s
        return {"session_id": sid, "cwd": s.cwd, "status": "loaded"}
    if name == "grok_local_session_resume":
        sid = args["session_id"]
        s = AcpSession(args.get("cwd", DEFAULT_CWD))
        try:
            s._spawn_agent()
            params = _session_params(s.cwd, _mcp_servers_arg(args))
            params["sessionId"] = sid
            s.send("session/resume", params)
            s.session_id = sid
        except Exception:
            s.close()
            raise
        with sessions_lock: sessions[sid] = s
        return {"session_id": sid, "cwd": s.cwd, "status": "resumed"}
    if name == "grok_local_session_prompt":
        s = get_session(args.get("session_id")); prompt = args.get("prompt")
        if not isinstance(prompt, str) or not prompt.strip(): raise ValueError("prompt must be non-empty")
        request_id = s.send_async("session/prompt", {"sessionId": s.session_id, "prompt": [{"type": "text", "text": prompt}]})
        return {"session_id": s.session_id, "request_id": request_id, "status": "prompt_accepted", "hint": "poll grok_local_session_events for updates, completion, or permission_request"}
    if name == "grok_local_session_events":
        s = get_session(args.get("session_id")); return {"session_id": s.session_id, "alive": s.alive, "events": s.events_now()}
    if name == "grok_local_session_transcript":
        s = get_session(args.get("session_id"))
        return {"session_id": s.session_id, "count": len(s.transcript), "transcript": list(s.transcript)}
    if name == "grok_local_permission_decide":
        rid = str(args["request_id"]); decision = args["decision"]
        found = None
        with sessions_lock:
            for s in sessions.values():
                if rid in s.permissions: found = s; break
        if not found: raise ValueError("unknown or already answered request_id")
        if decision == "cancel": outcome = {"outcome": "cancelled"}
        else:
            option = args.get("option_id")
            if not option:
                opts = found.permissions[rid]["params"].get("options", [])
                kinds = {"accept": ("allow_once", "allow_always"), "deny": ("reject_once", "reject_always")}
                wanted = kinds.get(decision, ())
                option = next((o.get("optionId") for o in opts if o.get("kind") in wanted), None)
            if not option: raise ValueError("option_id is required when no permission option was offered")
            outcome = {"outcome": "selected", "optionId": option}
        found.decisions.append({"request_id": rid, "decision": decision,
                                "outcome": outcome, "t": time.time()})
        _send_permission(found, rid, outcome)
        found.permissions.pop(rid, None)
        found.reported_permissions.discard(rid)
        return {"request_id": rid, "decision": decision, "outcome": outcome}
    if name == "grok_local_session_cancel":
        s = get_session(args.get("session_id")); s.send("session/cancel", {"sessionId": s.session_id}, request=False); return {"session_id": s.session_id, "status": "cancel_requested"}
    if name == "grok_local_session_close":
        sid = args.get("session_id"); s = get_session(sid); s.close()
        with sessions_lock: sessions.pop(s.session_id, None)
        return {"session_id": sid, "status": "closed"}
    if name == "grok_local_systemone_route":
        task = args.get("task", "")
        if not isinstance(task, str) or not task.strip(): raise ValueError("task must be non-empty")
        return systemone_route(task, kind=args.get("kind", "prompt"), session_id=args.get("session_id"))
    if name == "grok_local_lmstudio_models":
        return {"library": lmstudio_library(), "loaded": lmstudio_loaded_model(), "url": LMSTUDIO_URL,
                "note": "The adapter never loads or unloads models; 'loaded' is observational only."}
    if name == "grok_local_tools_batch":
        calls = args.get("calls")
        if not isinstance(calls, list) or not calls: raise ValueError("calls must be a non-empty list")
        if len(calls) > 16: raise ValueError("calls limited to 16 per batch")
        def _one(call):
            if not isinstance(call, dict): raise ValueError("each call must be {tool, arguments}")
            tool = call.get("tool")
            if tool == "grok_local_tools_batch": raise ValueError("nested tools_batch is not allowed")
            if not any(t["name"] == tool for t in TOOLS): raise ValueError("unknown tool: %s" % (tool,))
            return handle(tool, call.get("arguments", {}))
        results = []
        with concurrent.futures.ThreadPoolExecutor(max_workers=min(8, len(calls))) as ex:
            futs = [ex.submit(_one, c) for c in calls]
            for c, f in zip(calls, futs):
                try:
                    results.append({"tool": c.get("tool"), "ok": True, "result": f.result()})
                except Exception as exc:
                    results.append({"tool": c.get("tool"), "ok": False, "error": str(exc)})
        return {"results": results}
    if name == "grok_local_ralph_run":
        return _ralph_run(args)
    if name == "grok_local_plan_then_execute":
        return _plan_then_execute(args)
    if name == "grok_local_rank_plans":
        task = args.get("task")
        if not isinstance(task, str) or not task.strip():
            raise ValueError("task must be non-empty")
        plans = args.get("plans")
        if not isinstance(plans, list) or len(plans) < 2:
            raise ValueError("plans must be a list of 2+ {id, text} candidates")
        inputs = [{"id": str(p.get("id", "plan-%d" % i)), "text": str(p.get("text", ""))}
                  for i, p in enumerate(plans) if isinstance(p, dict)]
        winner, rankings = _rank_plans(task.strip(), inputs)
        return {"task": task.strip(), "winner_index": winner,
                "winner_id": inputs[winner]["id"] if inputs else None,
                "rankings": rankings,
                "fail_open": all(r["score"] is None for r in rankings)}
    if name == "grok_local_compact_session":
        return _compact_session(args)
    if name == "grok_local_slash_verify":
        return _slash_verify(args)
    if name == "grok_local_slash_review":
        return _slash_review(args)
    raise KeyError(name)


def _send_permission(session, rid, outcome):
    rpc_id = session.permissions[rid]["rpc_id"]
    with session.write_lock:
        session.proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": rpc_id, "result": {"outcome": outcome}}) + "\n")
        session.proc.stdin.flush()


def main():
    for line in sys.stdin:
        msg = None
        try:
            msg = json.loads(line); method = msg.get("method"); mid = msg.get("id")
            if method == "initialize": reply(mid, {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}}, "serverInfo": {"name": NAME, "version": VERSION}})
            elif method == "notifications/initialized": pass
            elif method == "tools/list": reply(mid, {"tools": TOOLS})
            elif method == "tools/call":
                p = msg.get("params", {}); reply(mid, text_result(handle(p.get("name"), p.get("arguments", {}))))
            elif mid is not None: reply(mid, error={"code": -32601, "message": f"unknown method: {method}"})
        except subprocess.TimeoutExpired: reply(msg.get("id") if msg else None, error={"code": -32001, "message": "Grok timed out"})
        except Exception as exc: reply(msg.get("id") if isinstance(msg, dict) else None, error={"code": -32000, "message": str(exc)})


if __name__ == "__main__": main()
