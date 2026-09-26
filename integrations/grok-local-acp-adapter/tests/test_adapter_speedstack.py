#!/usr/bin/env python3
"""Unit tests for the grok-local ACP adapter's SystemOne routing legs
(effort caps, no-hard-coded-model-ids policy, plan ranking, second
opinions, decision records, MCP server suggestions).

Stdlib only (unittest + unittest.mock). Run from the adapter dir:
    python3 -m unittest discover -s tests -v
"""
import copy
import importlib.util
import json
import pathlib
import shutil
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
         "suggestion_detail": [], "suggestion_note": mod._SUGGESTION_NOTE,
         "calibrated_probabilities": {}, "margin": None, "uncertain": None,
         "ranked_models": [], "second_opinion": None}
    d.update(kw)
    return d


class SpeedStackTest(unittest.TestCase):
    def setUp(self):
        mod._speed_config = copy.deepcopy(mod.SPEED_DEFAULTS)
        mod._systemone_cache.clear()
        self.cfg = mod.speed_config()
        self._patches = []
        # Hermetic by default: no network, no LM Studio, no real inventory,
        # and decision records go to a per-test temp file (never the real
        # ~/.grok-local/systemone/decision_records.jsonl).
        self.patch("lmstudio_loaded_model", lambda: "test-model-9b")
        self.patch("_pick_local_model", lambda tier, mid: "test-model-9b")
        self.inv_patch = self.patch("_mcp_server_inventory", lambda: [])
        self._records_tmp = tempfile.mkdtemp(prefix="adapter-records-")
        self.patch("_records_path",
                   lambda: pathlib.Path(self._records_tmp) / "decision_records.jsonl")

    def tearDown(self):
        for p in self._patches:
            p.stop()
        shutil.rmtree(self._records_tmp, ignore_errors=True)

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

    # --- A2: no model switching, no hard-coded ids --------------------------
    def test_model_switch_path_removed(self):
        # The standing no-unload rule (never unload a model the user loaded)
        # is enforced structurally: the adapter has no load/unload code path
        # at all.
        self.assertFalse(hasattr(mod, "_maybe_switch_model"))
        self.assertNotIn("auto_model_switch", mod.SPEED_DEFAULTS)

    def test_one_shot_never_touches_lms(self):
        self.patch("systemone_route",
                   lambda *a, **k: base_decision(loaded_model="test-model-35b"))
        calls = []

        def fake_run(cmd, **kw):
            calls.append(cmd)
            if cmd[0] == mod.LMS_BIN:
                raise AssertionError("lms must never be called")
            return Proc(0, "grok output", "")

        with mock.patch("subprocess.run", fake_run), tempfile.TemporaryDirectory() as td:
            out = mod._grok_one_shot("hello", td, effort="auto")
        self.assertEqual(out, "grok output")
        self.assertTrue(all(c[0] != mod.LMS_BIN for c in calls))

    def test_no_hardcoded_model_ids(self):
        self.assertIsNone(self.cfg["planner_model"])
        self.assertEqual(self.cfg["tier_models"], {})
        self.assertEqual(mod.SPEED_DEFAULTS["systemone_urls"],
                         ["http://127.0.0.1:8765/v1/systemone/route"])

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
        self.assertEqual(len(mod.TOOLS), 25)   # +grok_local_rank_plans (2026-09-26)
        for t in mod.TOOLS:
            self.assertIn("name", t)
            self.assertIn("description", t)
            self.assertIn("inputSchema", t)

    def test_systemone_tool_returns_new_fields(self):
        self.patch("systemone_route", lambda *a, **k: base_decision())
        res = mod.handle("grok_local_systemone_route", {"task": "x"})
        for field in ("effort", "max_turns", "ralph_cap", "task_labels",
                      "suggested_mcp_servers", "suggestion_note",
                      "calibrated_probabilities", "margin", "uncertain",
                      "ranked_models", "second_opinion"):
            self.assertIn(field, res)

    # --- A4: second opinion (historical jeff1_second_opinion wire key) --------
    def test_second_opinion_parsed_from_historical_key(self):
        with self._shim({"tier": "economy",
                         "jeff1_second_opinion": {"tier": "balanced",
                                                 "confidence": 0.6,
                                                 "agree": False,
                                                 "rationale": "maybe heavier"}}):
            d = mod.systemone_route("do a thing")
        so = d["second_opinion"]
        self.assertIsNotNone(so)
        self.assertEqual(so["tier"], "balanced")
        self.assertEqual(so["confidence"], 0.6)
        self.assertFalse(so["agree"])
        # The advisory never changes the primary routed tier, and no
        # user-facing key mentions Jeff-1.
        self.assertEqual(d["tier"], "economy")
        self.assertNotIn("jeff1_second_opinion", d)
        self.assertTrue(all("jeff1" not in str(k).lower() for k in d.keys()))

    def test_second_opinion_absent_without_key(self):
        with self._shim({"tier": "economy"}):
            d = mod.systemone_route("do a thing")
        self.assertIsNone(d["second_opinion"])

    def test_second_opinion_malformed_ignored(self):
        with self._shim({"tier": "economy", "jeff1_second_opinion": "nonsense"}):
            d = mod.systemone_route("do a thing")
        self.assertIsNone(d["second_opinion"])

    # --- A5: decision records -------------------------------------------------
    def test_decision_record_appended_on_route(self):
        with self._shim({"tier": "balanced",
                         "calibrated_probabilities": {"balanced": 0.55,
                                                      "economy": 0.25,
                                                      "heavy": 0.2},
                         "margin": 0.3, "uncertain": False}):
            d = mod.systemone_route("route me", kind="prompt")
        path = mod._records_path()
        rows = [json.loads(line) for line in path.read_text().splitlines()]
        self.assertEqual(len(rows), 1)
        row = rows[0]
        self.assertEqual(row["type"], "choice")
        self.assertEqual(row["probs"], [0.55, 0.25, 0.2])  # sorted desc
        self.assertEqual(row["tier"], "balanced")
        self.assertEqual(row["client"], "grok-local-acp-adapter")
        self.assertNotIn("gold", row)   # no fabricated ground truth

    def test_decision_record_appended_on_fail_open(self):
        import urllib.error
        with mock.patch("urllib.request.urlopen",
                        side_effect=urllib.error.URLError("down")):
            mod.systemone_route("route me", kind="prompt")
        rows = [json.loads(line) for line in mod._records_path().read_text().splitlines()]
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["source"], "fail-open")

    def test_decision_record_fail_open_unwritable(self):
        # Logging must never break routing: an unwritable path is swallowed.
        self.patch("_records_path",
                   lambda: pathlib.Path("/proc/nowhere/decision_records.jsonl"))
        with self._shim({"tier": "economy"}):
            d = mod.systemone_route("route me")
        self.assertEqual(d["tier"], "economy")

    # --- A6: plan ranking -----------------------------------------------------
    def test_rank_plans_url_derivation(self):
        self.assertEqual(
            mod._rank_plans_url("http://127.0.0.1:8765/v1/systemone/route"),
            "http://127.0.0.1:8765/v1/systemone/rank-plans")
        self.assertIsNone(mod._rank_plans_url("http://example.com/other"))

    def test_rank_plans_picks_highest_score(self):
        plans = [{"id": "a", "text": "plan a"}, {"id": "b", "text": "plan b"}]
        payload = {"rankings": [{"id": "a", "score": 0.4, "p_success": 0.5},
                                {"id": "b", "score": 0.8, "p_success": 0.9}]}
        with mock.patch("urllib.request.urlopen", return_value=FakeResp(payload)):
            winner, rankings = mod._rank_plans("task", plans)
        self.assertEqual(winner, 1)
        self.assertEqual(rankings[1]["score"], 0.8)
        self.assertEqual(rankings[1]["p_success"], 0.9)

    def test_rank_plans_fail_open_shim_down(self):
        import urllib.error
        plans = [{"id": "a", "text": "plan a"}, {"id": "b", "text": "plan b"}]
        with mock.patch("urllib.request.urlopen",
                        side_effect=urllib.error.URLError("down")):
            winner, rankings = mod._rank_plans("task", plans)
        self.assertEqual(winner, 0)                 # original first wins
        self.assertTrue(all(r["score"] is None for r in rankings))
        self.assertEqual([r["id"] for r in rankings], ["a", "b"])  # order kept

    def test_rank_plans_tool_requires_two_plans(self):
        with self.assertRaises(ValueError):
            mod.handle("grok_local_rank_plans", {"task": "t",
                                                 "plans": [{"id": "a", "text": "x"}]})

    def test_rank_plans_tool_fail_open(self):
        import urllib.error
        with mock.patch("urllib.request.urlopen",
                        side_effect=urllib.error.URLError("down")):
            res = mod.handle("grok_local_rank_plans",
                             {"task": "t",
                              "plans": [{"id": "a", "text": "x"},
                                        {"id": "b", "text": "y"}]})
        self.assertEqual(res["winner_index"], 0)
        self.assertTrue(res["fail_open"])

    def test_plan_then_execute_candidate_plans_winner(self):
        self.patch("_rank_plans",
                   lambda task, plans: (1, [{"id": p["id"], "score": 1.0 - i}
                                                 for i, p in enumerate(plans)]))
        self.patch("systemone_route", lambda *a, **k: base_decision(effort="low"))
        self.patch("_grok_one_shot", lambda prompt, cwd, **kw: "exec output")
        with tempfile.TemporaryDirectory() as td:
            res = mod._plan_then_execute({"task": "t", "cwd": td,
                                          "plan_path": td + "/plan.md",
                                          "candidate_plans": ["first plan",
                                                              "winning plan"]})
        self.assertEqual(res["plan"], "winning plan")
        self.assertEqual(res["plan_ranking"]["winner"], "plan-1")
        self.assertEqual(res["result"], "exec output")

    def test_plan_then_execute_fail_open_uses_first_candidate(self):
        self.patch("_rank_plans",
                   lambda task, plans: (0, [{"id": p["id"], "score": None}
                                            for p in plans]))
        self.patch("systemone_route", lambda *a, **k: base_decision(effort="low"))
        self.patch("_grok_one_shot", lambda prompt, cwd, **kw: "exec output")
        with tempfile.TemporaryDirectory() as td:
            res = mod._plan_then_execute({"task": "t", "cwd": td,
                                          "plan_path": td + "/plan.md",
                                          "candidate_plans": ["first plan",
                                                              "second plan"]})
        self.assertEqual(res["plan"], "first plan")


if __name__ == "__main__":
    unittest.main(verbosity=2)
