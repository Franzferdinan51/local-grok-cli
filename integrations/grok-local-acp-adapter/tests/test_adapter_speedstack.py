#!/usr/bin/env python3
"""Unit tests for the grok-local ACP adapter's SystemOne routing legs
(effort caps, opt-in model switching, MCP server suggestions).

Stdlib only (unittest + unittest.mock). Run from the adapter dir:
    python3 -m unittest discover -s tests -v
"""
import copy
import importlib.util
import json
import pathlib
import sys
import tempfile
import unittest
from unittest import mock

ADAPTER = pathlib.Path(__file__).resolve().parent.parent / "grok_local_acp_adapter.py"


def load_adapter():
    spec = importlib.util.spec_from_file_location("grok_local_acp_adapter", ADAPTER)
    mod = importlib.util.module_from_spec(spec)
    sys.modules["grok_local_acp_adapter"] = mod
    spec.loader.exec_module(mod)
    return mod


mod = load_adapter()


class FakeResp:
    def __init__(self, payload):
        self._body = json.dumps(payload).encode()

    def read(self):
        return self._body

    def __enter__(self):
        return self

    def __exit__(self, *args):
        return False


class Proc:
    def __init__(self, rc=0, out="", err=""):
        self.returncode = rc
        self.stdout = out
        self.stderr = err


def base_decision(**kw):
    d = {"source": "systemone", "effort": "medium", "max_turns": 6,
         "ralph_cap": 8, "permission_mode": "auto", "tier": "balanced",
         "local_model": "ornith-1.5-9b", "loaded_model": "ornith-1.5-9b",
         "task_labels": [], "suggested_mcp_servers": [],
         "suggestion_detail": [], "suggestion_note": mod._SUGGESTION_NOTE}
    d.update(kw)
    return d


class SpeedStackTest(unittest.TestCase):
    def setUp(self):
        mod._speed_config = copy.deepcopy(mod.SPEED_DEFAULTS)
        mod._systemone_cache.clear()
        self.cfg = mod.speed_config()
        self._patches = []
        # Hermetic by default: no network, no LM Studio, no real inventory.
        self.patch("lmstudio_loaded_model", lambda: "ornith-1.5-35b-a3b")
        self.patch("_pick_local_model", lambda tier, mid: "ornith-1.5-9b")
        self.inv_patch = self.patch("_mcp_server_inventory", lambda: [])

    def tearDown(self):
        for p in self._patches:
            p.stop()

    def patch(self, name, new):
        p = mock.patch.object(mod, name, new)
        self._patches.append(p)
        p.start()
        return p

    # --- A1: effort map -------------------------------------------------
    def test_tier_caps_defaults(self):
        self.assertEqual(mod._tier_caps("economy", self.cfg),
                         {"effort": "low", "max_turns": 4, "ralph_cap": 4})
        self.assertEqual(mod._tier_caps("edge", self.cfg),
                         {"effort": "low", "max_turns": 4, "ralph_cap": 4})
        self.assertEqual(mod._tier_caps("balanced", self.cfg),
                         {"effort": "medium", "max_turns": 6, "ralph_cap": 8})
        self.assertEqual(mod._tier_caps("heavy", self.cfg),
                         {"effort": "high", "max_turns": 10, "ralph_cap": 12})

    def test_tier_caps_legacy_string(self):
        cfg = copy.deepcopy(self.cfg)
        cfg["tier_efforts"] = {"economy": "low", "balanced": "medium", "heavy": "high"}
        self.assertEqual(mod._tier_caps("economy", cfg),
                         {"effort": "low", "max_turns": 4, "ralph_cap": 4})
        self.assertEqual(mod._tier_caps("heavy", cfg),
                         {"effort": "high", "max_turns": 10, "ralph_cap": 12})

    def test_tier_caps_unknown_tier_fail_open(self):
        self.assertEqual(mod._tier_caps("whatever", self.cfg),
                         {"effort": "high", "max_turns": 10, "ralph_cap": 12})
        self.assertEqual(mod._tier_caps(None, self.cfg),
                         {"effort": "high", "max_turns": 10, "ralph_cap": 12})

    def test_tier_caps_partial_override(self):
        cfg = copy.deepcopy(self.cfg)
        cfg["tier_efforts"]["economy"] = {"effort": "low", "max_turns": 5}
        self.assertEqual(mod._tier_caps("economy", cfg),
                         {"effort": "low", "max_turns": 5, "ralph_cap": 4})

    # --- fail-open -------------------------------------------------------
    def test_fail_open_shim_down(self):
        import urllib.error
        with mock.patch("urllib.request.urlopen",
                        side_effect=urllib.error.URLError("down")):
            d = mod.systemone_route("do a thing")
        self.assertEqual(d["source"], "fail-open")
        self.assertEqual(d["effort"], "high")
        self.assertEqual(d["max_turns"], 10)      # high tier caps
        self.assertEqual(d["ralph_cap"], 12)
        self.assertEqual(d["task_labels"], [])
        self.assertEqual(d["suggested_mcp_servers"], [])
        self.assertIn("Advisory only", d["suggestion_note"])
        self.assertIsNotNone(d["error"])

    # --- shim contract (defensive) ---------------------------------------
    def _shim(self, route):
        return mock.patch("urllib.request.urlopen",
                          return_value=FakeResp({"route": route}))

    def test_shim_effort_and_labels_consumed(self):
        with self._shim({"tier": "economy", "effort": "high",
                         "task_labels": ["git", "ci"],
                         "model_id": "ornith-1.5-9b",
                         "rationale": "r", "confidence": 0.9}):
            d = mod.systemone_route("run the CI pipeline")
        self.assertEqual(d["source"], "systemone")
        self.assertEqual(d["tier"], "economy")
        self.assertEqual(d["effort"], "high")      # explicit shim effort wins
        self.assertEqual(d["max_turns"], 10)
        self.assertEqual(d["ralph_cap"], 12)
        self.assertEqual(d["task_labels"], ["git", "ci"])

    def test_old_shim_derives_locally(self):
        with self._shim({"tier": "balanced", "model_id": "ornith-1.5-9b"}):
            d = mod.systemone_route("refactor the auth module")
        self.assertEqual(d["effort"], "medium")   # derived from tier
        self.assertEqual(d["max_turns"], 6)
        self.assertEqual(d["ralph_cap"], 8)
        self.assertEqual(d["task_labels"], [])

    def test_shim_bad_effort_ignored(self):
        with self._shim({"tier": "heavy", "effort": "ultra"}):
            d = mod.systemone_route("big task")
        self.assertEqual(d["effort"], "high")      # tier-derived fallback
        self.assertEqual(d["max_turns"], 10)

    # --- A2: auto_model_switch -------------------------------------------
    def test_auto_model_switch_default_off(self):
        self.assertFalse(self.cfg["auto_model_switch"])
        self.patch("systemone_route",
                   lambda *a, **k: base_decision(loaded_model="ornith-1.5-35b-a3b"))
        switch = mock.patch.object(mod, "_maybe_switch_model",
                                   side_effect=AssertionError("must not switch"))
        switch.start()
        self._patches.append(switch)
        calls = []

        def fake_run(cmd, **kw):
            calls.append(cmd)
            if cmd[0] == mod.LMS_BIN:
                raise AssertionError("lms must not be called when switch is off")
            return Proc(0, "grok output", "")

        with mock.patch("subprocess.run", fake_run), tempfile.TemporaryDirectory() as td:
            out = mod._grok_one_shot("hello", td, effort="auto")
        self.assertEqual(out, "grok output")
        self.assertTrue(all(c[0] != mod.LMS_BIN for c in calls))

    def test_auto_model_switch_enabled_unload_load_verify(self):
        self.cfg["auto_model_switch"] = True
        self.patch("systemone_route",
                   lambda *a, **k: base_decision(local_model="ornith-1.5-9b",
                                                loaded_model="ornith-1.5-35b-a3b"))
        self.patch("lmstudio_loaded_model", lambda: "ornith-1.5-9b")  # post-load state
        calls = []

        def fake_run(cmd, **kw):
            calls.append(cmd)
            return Proc(0, "ok", "")

        with mock.patch("subprocess.run", fake_run), tempfile.TemporaryDirectory() as td:
            out = mod._grok_one_shot("hello", td, effort="auto")
        self.assertEqual(out, "ok")
        self.assertEqual(calls[0][:3], [mod.LMS_BIN, "unload", "ornith-1.5-35b-a3b"])
        self.assertEqual(calls[1][:3], [mod.LMS_BIN, "load", "ornith-1.5-9b"])
        self.assertNotEqual(calls[2][0], mod.LMS_BIN)  # then the grok spawn

    def test_maybe_switch_noop_when_already_loaded(self):
        calls = []
        with mock.patch("subprocess.run",
                        lambda cmd, **kw: calls.append(cmd) or Proc()):
            detail = mod._maybe_switch_model(base_decision())
        self.assertFalse(detail["switched"])
        self.assertEqual(calls, [])

    # --- A3: suggestion scorer --------------------------------------------
    FIXTURE = [
        {"name": "github", "description": "GitHub repos, pull requests, issues"},
        {"name": "postgres", "description": "PostgreSQL database queries"},
        {"name": "filesystem", "description": "read and write local files"},
    ]

    def test_suggest_ranking(self):
        self.patch("_mcp_server_inventory", lambda: list(self.FIXTURE))
        got = mod._suggest_mcp_servers("fix the failing postgres migration query",
                                       ["database"])
        self.assertTrue(got)
        self.assertEqual(got[0]["name"], "postgres")
        # postgres(name, 2) + database(desc, 1) = 3 (no stemming: query != queries)
        self.assertEqual(got[0]["score"], 3)
        self.assertEqual(got[0]["matched"], ["database", "postgres"])
        names = [s["name"] for s in got]
        self.assertNotIn("github", names)

    def test_suggest_labels_boost(self):
        self.patch("_mcp_server_inventory", lambda: list(self.FIXTURE))
        got = mod._suggest_mcp_servers("do some stuff", ["github"])
        self.assertEqual([s["name"] for s in got], ["github"])
        self.assertEqual(got[0]["score"], 2)  # name token match

    def test_suggest_empty_means_use_everything(self):
        self.patch("_mcp_server_inventory", lambda: list(self.FIXTURE))
        self.assertEqual(mod._suggest_mcp_servers("zxqw florp blibbet", []), [])

    def test_suggest_no_inventory_fail_open(self):
        self.patch("_mcp_server_inventory", lambda: [])
        self.assertEqual(mod._suggest_mcp_servers("query the database", ["db"]), [])

    def test_inventory_skips_disabled(self):
        parsed = {"mcp_servers": {
            "on": {"command": "x", "description": "database helper"},
            "off": {"command": "y", "enabled": False, "description": "database helper"},
        }}
        self.inv_patch.stop()  # undo the hermetic setUp patch for this test
        self._patches.remove(self.inv_patch)
        self.patch("_parse_toml_subset", lambda text: parsed)
        inv = mod._mcp_server_inventory()
        self.assertEqual([s["name"] for s in inv], ["on"])

    # --- A1 consumers ------------------------------------------------------
    def test_one_shot_consumes_route_flags(self):
        self.patch("systemone_route", lambda *a, **k: base_decision())
        seen = []

        def fake_run(cmd, **kw):
            seen.append(cmd)
            return Proc(0, "ok", "")

        with mock.patch("subprocess.run", fake_run), tempfile.TemporaryDirectory() as td:
            mod._grok_one_shot("hello", td, effort="auto")
        cmd = seen[0]
        self.assertEqual(cmd[cmd.index("--reasoning-effort") + 1], "medium")
        self.assertEqual(cmd[cmd.index("--max-turns") + 1], "6")

    def test_one_shot_explicit_max_turns_wins(self):
        self.patch("systemone_route", lambda *a, **k: base_decision())
        seen = []

        def fake_run(cmd, **kw):
            seen.append(cmd)
            return Proc(0, "ok", "")

        with mock.patch("subprocess.run", fake_run), tempfile.TemporaryDirectory() as td:
            mod._grok_one_shot("hello", td, effort="auto", max_turns=9)
        cmd = seen[0]
        self.assertEqual(cmd[cmd.index("--max-turns") + 1], "9")

    def test_ralph_consumes_caps(self):
        self.patch("systemone_route",
                   lambda *a, **k: base_decision(effort="low", max_turns=4,
                                                ralph_cap=4, tier="economy"))
        seen = []
        self.patch("_grok_one_shot",
                   lambda prompt, cwd, **kw: seen.append(kw) or "iter output DONE")
        with tempfile.TemporaryDirectory() as td:
            res = mod._ralph_run({"task": "t", "cwd": td,
                                  "progress_dir": td,
                                  "progress_file": td + "/ralph.md"})
        self.assertEqual(res["stopped"], "done_marker")
        self.assertEqual(res["iterations"], 1)
        self.assertEqual(res["max_iterations"], 4)   # route ralph_cap
        self.assertEqual(res["max_turns"], 4)         # route max_turns
        self.assertEqual(seen[0]["max_turns"], 4)
        self.assertEqual(seen[0]["effort"], "low")

    def test_ralph_explicit_iterations_win(self):
        self.patch("systemone_route",
                   lambda *a, **k: base_decision(ralph_cap=4))
        self.patch("_grok_one_shot", lambda prompt, cwd, **kw: "iter output DONE")
        with tempfile.TemporaryDirectory() as td:
            res = mod._ralph_run({"task": "t", "cwd": td, "max_iterations": 9,
                                  "progress_dir": td,
                                  "progress_file": td + "/ralph.md"})
        self.assertEqual(res["max_iterations"], 9)

    # --- protocol surface guard ---------------------------------------------
    def test_tool_surface_unchanged(self):
        self.assertEqual(len(mod.TOOLS), 24)
        for t in mod.TOOLS:
            self.assertIn("name", t)
            self.assertIn("description", t)
            self.assertIn("inputSchema", t)

    def test_systemone_tool_returns_new_fields(self):
        self.patch("systemone_route", lambda *a, **k: base_decision())
        res = mod.handle("grok_local_systemone_route", {"task": "x"})
        for field in ("effort", "max_turns", "ralph_cap", "task_labels",
                      "suggested_mcp_servers", "suggestion_note"):
            self.assertIn(field, res)


if __name__ == "__main__":
    unittest.main(verbosity=2)
