"""TDD unit tests for tools/herdr_dispatcher.py (FASTLINK task queue + Herdr
event dispatcher).

Every test is CPU-only and hermetic: fake JSONL event feeds, mock subprocess
runners and temporary task directories only. No live Herdr socket, no real
subprocess beyond Python itself, no GPU, no network, and no running services
are contacted while proving RED -> GREEN.
"""
import json
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO_ROOT))

from tools import herdr_dispatcher as hd  # noqa: E402

NOW = 1_000_000.0
TEST_WORKTREE = tempfile.gettempdir()


def make_task(task_id, deps=()):
    return {
        "id": task_id,
        "title": f"task {task_id}",
        "owner": "herdr-dispatcher",
        "deps": list(deps),
        "branch": "feat/test",
        "worktree": TEST_WORKTREE,
        "owned_paths": ["qa/out.py"],
        "test_cmds": [["python", "-c", "print('ok')"]],
        "cwd": TEST_WORKTREE,
    }


def base_manifest(tasks=None, allowed_workers=None):
    return {
        "manifest_version": 1,
        "allowed_workers": list(allowed_workers or ["herdr-dispatcher"]),
        "tasks": list(tasks) if tasks is not None else [
            make_task("t1"),
            make_task("t2", ["t1"]),
        ],
    }


class RunnerHarness:
    """Deterministic in-process stand-in for subprocess.run(argv, cwd, ...)."""

    def __init__(self, behavior=None):
        self.calls = []
        self.behavior = list(behavior or [])

    def __call__(self, argv, cwd, timeout=None):
        self.calls.append(list(argv))
        mode = self.behavior.pop(0) if self.behavior else "ok"
        if mode == "timeout":
            raise hd.TestCommandTimeout(f"timed out: {argv}")
        return {"returncode": 0 if mode == "ok" else 1, "stdout": "", "stderr": ""}


def inside_paths(paths):
    return lambda worktree: list(paths)


class TempRepoTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        self.root = Path(self.dir.name)

    def write_manifest(self, manifest):
        path = self.root / "tasks.json"
        path.write_text(json.dumps(manifest))
        return path

    def write_events(self, name, events):
        path = self.root / name
        path.write_text("".join(json.dumps(e) + "\n" for e in events))
        return path

    def fake_event(self, status, agent="herdr-dispatcher", pane="w1:p1"):
        return {
            "event": "pane.agent_status_changed",
            "pane_id": pane,
            "agent": agent,
            "status": status,
        }


class ManifestTests(TempRepoTest):
    def test_valid_manifest_loads(self):
        path = self.write_manifest(base_manifest())
        self.assertEqual(hd.load_manifest(path)["allowed_workers"], ["herdr-dispatcher"])

    def test_duplicate_task_ids_rejected(self):
        manifest = base_manifest()
        manifest["tasks"].append(dict(manifest["tasks"][0]))
        with self.assertRaises(hd.ManifestError):
            hd.validate_manifest(manifest)

    def test_owner_outside_allowlist_rejected(self):
        manifest = base_manifest()
        manifest["tasks"][0]["owner"] = "unrelated-agent"
        with self.assertRaises(hd.ManifestError):
            hd.validate_manifest(manifest)

    def test_orphan_dependency_rejected(self):
        manifest = base_manifest(tasks=[make_task("t2", ["t1"])])
        with self.assertRaises(hd.ManifestError):
            hd.validate_manifest(manifest)

    def test_dependency_cycle_rejected(self):
        a = make_task("a", ["b"])
        b = make_task("b", ["a"])
        with self.assertRaises(hd.ManifestError):
            hd.validate_manifest(base_manifest(tasks=[a, b]))

    def test_shell_string_in_test_cmds_rejected(self):
        manifest = base_manifest()
        manifest["tasks"][0]["test_cmds"] = ["bash -c 'somestring'"]
        with self.assertRaises(hd.ManifestError):
            hd.validate_manifest(manifest)

    def test_empty_owned_paths_rejected(self):
        manifest = base_manifest()
        manifest["tasks"][0]["owned_paths"] = []
        with self.assertRaises(hd.ManifestError):
            hd.validate_manifest(manifest)


class NewStateAndAssignmentTests(TempRepoTest):
    def setUp(self):
        super().setUp()
        self.manifest = base_manifest()
        self.state = hd.new_state(self.manifest)

    def test_dependency_free_ready_dep_task_backlog(self):
        self.assertEqual(self.state["tasks"]["t1"]["status"], "READY")
        self.assertEqual(self.state["tasks"]["t2"]["status"], "BACKLOG")
        self.assertEqual(self.state["revision"], 0)

    def test_explicit_assignment_records_lease(self):
        self.assertTrue(
            hd.assign(self.state, "t1", self.manifest, pane_id="w1:p1", now=NOW)
        )
        task = self.state["tasks"]["t1"]
        self.assertEqual(task["status"], "ACTIVE")
        self.assertEqual(task["lease"]["pane_id"], "w1:p1")
        self.assertEqual(task["attempts"], 0)

    def test_assignment_refused_for_owner_outside_allowlist(self):
        # Manifest-level validation rejects such owners outright (see
        # ManifestTests); here we probe the assign-time defence in depth on
        # a hand-built state so the guard cannot silently regress.
        manifest = base_manifest()
        state = {
            "state_version": 1,
            "revision": 0,
            "tasks": {
                "t1": dict(
                    make_task("t1"),
                    owner="stranger",
                    status="READY",
                    attempts=0,
                    lease=None,
                    needs_attention=None,
                    history=[],
                )
            },
        }
        self.assertFalse(hd.assign(state, "t1", manifest, pane_id="w1:p1", now=NOW))
        self.assertEqual(state["tasks"]["t1"]["status"], "READY")
        self.assertIsNone(state["tasks"]["t1"]["lease"])

    def test_assignment_refused_twice_and_for_backlog(self):
        self.assertTrue(hd.assign(self.state, "t1", self.manifest, pane_id="w1:p1", now=NOW))
        self.assertFalse(hd.assign(self.state, "t1", self.manifest, pane_id="w1:p2", now=NOW))
        self.assertEqual(self.state["tasks"]["t1"]["lease"]["pane_id"], "w1:p1")
        self.assertFalse(hd.assign(self.state, "t2", self.manifest, pane_id="w1:p2", now=NOW))
        self.assertEqual(self.state["tasks"]["t2"]["status"], "BACKLOG")

    def test_event_dispatch_never_assigns_without_explicit_handoff(self):
        runner = RunnerHarness()
        changed = hd.apply_event(
            self.state, self.fake_event("working"), self.manifest, NOW,
            runner=runner, changed_paths=inside_paths([]),
        )
        self.assertFalse(changed)
        self.assertEqual(self.state["tasks"]["t1"]["status"], "READY")
        self.assertEqual(runner.calls, [])


class EventTransitionTests(TempRepoTest):
    def setUp(self):
        super().setUp()
        self.manifest = base_manifest()
        self.state = hd.new_state(self.manifest)

    def assigned(self, task_id="t1"):
        self.assertTrue(
            hd.assign(self.state, task_id, self.manifest, pane_id="w1:p1", now=NOW)
        )
        return self.state["tasks"][task_id]

    def test_done_event_runs_manifest_tests_and_stops_at_review(self):
        task = self.assigned()
        runner = RunnerHarness()
        changed = hd.apply_event(
            self.state, self.fake_event("done"), self.manifest, NOW,
            runner=runner, changed_paths=inside_paths(["qa/out.py"]),
        )
        self.assertTrue(changed)
        self.assertEqual(task["status"], "REVIEW")
        self.assertEqual(runner.calls, [["python", "-c", "print('ok')"]])
        self.assertEqual(task["attempts"], 1)

    def test_idle_event_treated_as_settled_and_reaches_review(self):
        task = self.assigned()
        hd.apply_event(
            self.state, self.fake_event("idle"), self.manifest, NOW,
            runner=RunnerHarness(), changed_paths=inside_paths(["qa/out.py"]),
        )
        self.assertEqual(task["status"], "REVIEW")

    def test_uncertain_status_parks_without_retry_or_done(self):
        task = self.assigned()
        runner = RunnerHarness()
        hd.apply_event(
            self.state, self.fake_event("unknown"), self.manifest, NOW,
            runner=runner, changed_paths=inside_paths([]),
        )
        self.assertEqual(task["status"], "ACTIVE")
        self.assertEqual(task["needs_attention"], "uncertain_status")
        self.assertEqual(runner.calls, [])
        self.assertEqual(task["attempts"], 0)

    def test_blocked_event_requires_attention_not_failure(self):
        task = self.assigned()
        hd.apply_event(
            self.state, self.fake_event("blocked"), self.manifest, NOW,
            runner=RunnerHarness(), changed_paths=inside_paths([]),
        )
        self.assertEqual(task["status"], "BLOCKED")
        self.assertEqual(task["needs_attention"], "agent_blocked")

    def test_working_after_blocked_recovers_lease(self):
        task = self.assigned()
        hd.apply_event(self.state, self.fake_event("blocked"), self.manifest, NOW,
                       runner=RunnerHarness(), changed_paths=inside_paths([]))
        hd.apply_event(self.state, self.fake_event("working"), self.manifest, NOW + 5,
                       runner=RunnerHarness(), changed_paths=inside_paths([]))
        self.assertEqual(task["status"], "ACTIVE")
        self.assertIsNone(task["needs_attention"])

    def test_unrelated_agent_event_ignored(self):
        self.assigned()
        changed = hd.apply_event(
            self.state, self.fake_event("done", agent="other-agent"), self.manifest, NOW,
            runner=RunnerHarness(), changed_paths=inside_paths([]),
        )
        self.assertFalse(changed)
        self.assertEqual(self.state["tasks"]["t1"]["status"], "ACTIVE")

    def test_events_for_tasks_never_assigned_noop(self):
        runner = RunnerHarness()
        changed = hd.apply_event(
            self.state, self.fake_event("done"), self.manifest, NOW,
            runner=runner, changed_paths=inside_paths([]),
        )
        self.assertFalse(changed)
        self.assertEqual(runner.calls, [])
        self.assertEqual(self.state["tasks"]["t1"]["status"], "READY")


class TestingPhaseTests(TempRepoTest):
    def setUp(self):
        super().setUp()
        self.manifest = base_manifest()
        self.state = hd.new_state(self.manifest)
        hd.assign(self.state, "t1", self.manifest, pane_id="w1:p1", now=NOW)
        self.task = self.state["tasks"]["t1"]

    def event(self, status="done"):
        return self.fake_event(status)

    def apply(self, runner, changed):
        return hd.apply_event(
            self.state, self.event(), self.manifest, NOW,
            runner=runner, changed_paths=changed,
        )

    def test_definite_failures_retry_bounded_then_failed(self):
        runner = RunnerHarness(behavior=["fail", "fail", "fail"])
        self.apply(runner, inside_paths([]))
        self.assertEqual(self.task["status"], "READY")
        self.apply(runner, inside_paths([]))
        self.assertEqual(self.task["status"], "READY")
        self.apply(runner, inside_paths([]))
        self.assertEqual(self.task["status"], "FAILED")
        self.assertEqual(self.task["attempts"], 3)

    def test_timeout_parks_without_blind_retry_or_done(self):
        runner = RunnerHarness(behavior=["timeout"])
        self.apply(runner, inside_paths([]))
        self.assertEqual(self.task["status"], "TESTING")
        self.assertEqual(self.task["needs_attention"], "test_timeout")
        self.assertEqual(self.task["attempts"], 1)
        self.assertEqual(runner.calls, [["python", "-c", "print('ok')"]])

    def test_scope_violation_fails_task(self):
        self.apply(RunnerHarness(), inside_paths(["src/main.rs"]))
        self.assertEqual(self.task["status"], "FAILED")
        self.assertEqual(self.task["needs_attention"], "scope_violation")

    def test_scope_probe_error_parks_without_promotion(self):
        def broken(_worktree):
            raise RuntimeError("not a git tree")

        self.apply(RunnerHarness(), broken)
        self.assertEqual(self.task["status"], "TESTING")
        self.assertEqual(self.task["needs_attention"], "scope_unverified")

    def test_review_is_terminal_until_explicit_approval(self):
        self.apply(RunnerHarness(), inside_paths(["qa/out.py"]))
        self.assertEqual(self.task["status"], "REVIEW")
        self.assertFalse(
            self.apply(RunnerHarness(), inside_paths(["qa/out.py"]))
        )
        self.assertEqual(self.task["status"], "REVIEW")
        self.assertTrue(hd.approve(self.state, "t1", self.manifest))
        self.assertEqual(self.task["status"], "DONE")
        self.assertFalse(hd.approve(self.state, "t1", self.manifest))


class SnapshotReconcileTests(TempRepoTest):
    def setUp(self):
        super().setUp()
        self.manifest = base_manifest()
        self.state = hd.new_state(self.manifest)
        hd.assign(self.state, "t1", self.manifest, pane_id="w1:p1", now=NOW)

    def test_stale_lease_recovers_to_ready_without_attempt_cost(self):
        task = self.state["tasks"]["t1"]
        stale = NOW - hd.DEFAULT_LEASE_TTL_S - 1
        task["lease"]["acquired_at"] = stale
        task["lease"]["expires_at"] = stale + hd.DEFAULT_LEASE_TTL_S
        self.assertTrue(hd.reconcile_snapshot(self.state, {}, self.manifest, NOW))
        self.assertEqual(task["status"], "READY")
        self.assertIsNone(task["lease"])
        self.assertEqual(task["attempts"], 0)

    def test_live_lease_survives_empty_snapshot(self):
        task = self.state["tasks"]["t1"]
        self.assertFalse(hd.reconcile_snapshot(self.state, {}, self.manifest, NOW))
        self.assertEqual(task["status"], "ACTIVE")
        self.assertIsNotNone(task["lease"])

    def test_backlog_becomes_ready_when_dependency_done(self):
        self.state["tasks"]["t1"]["status"] = "DONE"
        self.assertTrue(hd.reconcile_snapshot(self.state, {}, self.manifest, NOW))
        self.assertEqual(self.state["tasks"]["t2"]["status"], "READY")

    def test_backlog_blocked_when_dependency_failed(self):
        self.state["tasks"]["t1"]["status"] = "FAILED"
        self.assertTrue(hd.reconcile_snapshot(self.state, {}, self.manifest, NOW))
        self.assertEqual(self.state["tasks"]["t2"]["status"], "BLOCKED")


class SocketFeedTests(unittest.TestCase):
    class FakeSocket:
        """Stub transport: send_frame writes; next_line yields scripted bytes."""

        def __init__(self, lines):
            self._lines = list(lines)
            self.sent = []

        def send_frame(self, text):
            self.sent.append(text)

        def next_line(self):
            return self._lines.pop(0) if self._lines else None

    def test_subscribe_sent_once_and_jsonl_parsed(self):
        sock = self.FakeSocket([
            json.dumps({"event": "pane.agent_status_changed",
                        "agent": "herdr-dispatcher", "status": "done"}) + "\n",
            "not-json\n",
            json.dumps({"unrelated": True}) + "\n",
        ])
        feed = hd.JsonlSocketFeed(sock)
        feed.subscribe()
        self.assertEqual(len(sock.sent), 1)
        self.assertEqual(json.loads(sock.sent[0])["method"], "events.subscribe")
        events = list(feed)
        self.assertEqual(events[0]["status"], "done")
        self.assertEqual(events[1], {"unrelated": True})
        self.assertEqual(len(events), 2)


class PersistenceTests(TempRepoTest):
    def test_state_round_trip_is_lossless(self):
        manifest = base_manifest()
        path = self.root / "state.json"
        state = hd.new_state(manifest)
        hd.save_state(path, state)
        self.assertEqual(hd.load_state(path), state)

    def test_idempotent_replay_saves_identical_bytes(self):
        manifest = base_manifest()
        path = self.root / "state.json"
        runner = RunnerHarness()
        event = {
            "event": "pane.agent_status_changed",
            "pane_id": "w1:p1",
            "agent": "herdr-dispatcher",
            "status": "done",
        }
        state = hd.new_state(manifest)
        hd.reconcile_snapshot(state, {}, manifest, NOW)
        hd.assign(state, "t1", manifest, pane_id="w1:p1", now=NOW)
        hd.apply_event(state, event, manifest, NOW,
                       runner=runner, changed_paths=inside_paths(["qa/out.py"]))
        hd.save_state(path, state)
        first_bytes = path.read_bytes()

        replay = hd.load_state(path)
        self.assertFalse(
            hd.apply_event(replay, event, manifest, NOW,
                           runner=RunnerHarness(),
                           changed_paths=inside_paths(["qa/out.py"]))
        )
        hd.save_state(path, replay)
        self.assertEqual(path.read_bytes(), first_bytes)

    def test_save_state_is_atomic_and_revision_monotonic(self):
        manifest = base_manifest()
        path = self.root / "state.json"
        state = hd.new_state(manifest)
        hd.assign(state, "t1", manifest, pane_id="w1:p1", now=NOW)
        self.assertEqual(state["revision"], 1)
        hd.save_state(path, state)
        self.assertEqual(hd.load_state(path)["tasks"]["t1"]["status"], "ACTIVE")


class CliOnceTests(TempRepoTest):
    def test_once_is_deterministic_and_bounded(self):
        manifest_path = self.write_manifest(base_manifest())
        events_path = self.write_events("events.jsonl", [
            {"event": "pane.agent_status_changed", "pane_id": "w1:p1",
             "agent": "herdr-dispatcher", "status": "done"},
        ])
        state_path = self.root / "state.json"
        runner = RunnerHarness()

        def run_once():
            code = hd.run_cli(
                ["once", "--tasks", str(manifest_path), "--state", str(state_path),
                 "--events", str(events_path), "--assign", "t1:w1:p1"],
                runner=runner,
                changed_paths=inside_paths(["qa/out.py"]),
                now=NOW,
            )
            self.assertEqual(code, 0)
            return json.loads(state_path.read_text())

        first_snapshot = run_once()
        self.assertEqual(runner.calls, [["python", "-c", "print('ok')"]])
        state_path.unlink()
        second_snapshot = run_once()
        self.assertEqual(first_snapshot, second_snapshot)
        self.assertEqual(first_snapshot["tasks"]["t1"]["status"], "REVIEW")


if __name__ == "__main__":
    unittest.main()
