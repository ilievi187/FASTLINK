"""TDD unit tests for tools/herdr_dispatcher.py (FASTLINK task queue + Herdr
event dispatcher).

Every test is CPU-only and hermetic: fake JSONL event feeds, mock subprocess
runners and temporary task directories only. No live Herdr socket, no real
subprocess beyond Python itself, no GPU, no network, and no running services
are contacted while proving RED -> GREEN.

Wire facts pinned here are the verified Herdr v0.9.1 session payloads:

  request frame: {"id": "fastlink-dispatch-subscribe", "method": "events.subscribe",
                  "params": {"subscriptions": [
                      {"type": "pane.agent_status_changed", "pane_id": "..."}]}}
  success ack:   {"id": "fastlink-dispatch-subscribe",
                  "result": {"type": "subscription_started"}}
  pushed event:  {"event": "pane.agent_status_changed",
                  "data": {"agent": "opencode", "agent_status": "working",
                           "pane_id": "w2:pG", "workspace_id": "w2"}}
"""
import json
import subprocess
import sys
import tempfile
import time
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

    def fake_event(self, status, agent="herdr-dispatcher", pane="w1:p1", extra=None):
        """Real Herdr v0.9.1 notification envelope (user-verified payload)."""
        data = {
            "agent": agent,
            "agent_status": status,
            "pane_id": pane,
            "workspace_id": pane.split(":")[0],
        }
        if extra:
            data.update(extra)
        return {"event": "pane.agent_status_changed", "data": data}


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
        hd.apply_event(
            self.state, self.fake_event("working"), self.manifest, NOW,
            runner=RunnerHarness(), changed_paths=inside_paths([]),
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

    def test_parked_task_requires_explicit_operator_resume(self):
        task = self.assigned()
        hd.apply_event(self.state, self.fake_event("blocked"), self.manifest, NOW,
                       runner=RunnerHarness(), changed_paths=inside_paths([]))
        changed = hd.apply_event(
            self.state, self.fake_event("working"), self.manifest, NOW + 5,
            runner=RunnerHarness(), changed_paths=inside_paths([]))
        self.assertFalse(changed)
        self.assertEqual(task["status"], "BLOCKED")
        self.assertEqual(task["needs_attention"], "agent_blocked")
        self.assertTrue(hd.resume(self.state, "t1", now=NOW + 6))
        self.assertEqual(task["status"], "READY")
        self.assertIsNone(task["lease"])
        self.assertIsNone(task["needs_attention"])

    def test_settled_event_requires_new_state_change_sequence(self):
        task = self.assigned()
        task["lease"]["state_change_seq"] = 42
        stale = self.fake_event("done")
        stale["state_change_seq"] = 42
        runner = RunnerHarness()
        self.assertFalse(hd.apply_event(
            self.state, stale, self.manifest, NOW,
            runner=runner, changed_paths=inside_paths(["qa/out.py"])))
        self.assertEqual(task["attempts"], 0)
        fresh = self.fake_event("done")
        fresh["state_change_seq"] = 43
        self.assertTrue(hd.apply_event(
            self.state, fresh, self.manifest, NOW + 1,
            runner=runner, changed_paths=inside_paths(["qa/out.py"])))
        self.assertEqual(task["status"], "REVIEW")
        self.assertEqual(task["attempts"], 1)

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
        self.seq = 10

    def event(self, status="done"):
        event = self.fake_event(status)
        event["state_change_seq"] = self.seq
        self.seq += 1
        return event

    def apply(self, runner, changed):
        hd.apply_event(
            self.state, self.event("working"), self.manifest, NOW,
            runner=runner, changed_paths=changed,
        )
        return hd.apply_event(
            self.state, self.event("done"), self.manifest, NOW,
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
        self.assertEqual(self.task["status"], "BLOCKED")
        self.assertEqual(self.task["needs_attention"], "test_timeout")
        self.assertEqual(self.task["attempts"], 1)
        self.assertEqual(runner.calls, [["python", "-c", "print('ok')"]])

    def test_runner_exception_blocks_instead_of_promoting(self):
        def broken_runner(*_args, **_kwargs):
            raise RuntimeError("runner crashed")
        self.apply(broken_runner, inside_paths(["qa/out.py"]))
        self.assertEqual(self.task["status"], "BLOCKED")
        self.assertEqual(self.task["needs_attention"], "test_execution_error")

    def test_scope_violation_fails_task(self):
        self.apply(RunnerHarness(), inside_paths(["src/main.rs"]))
        self.assertEqual(self.task["status"], "FAILED")
        self.assertEqual(self.task["needs_attention"], "scope_violation")

    def test_scope_probe_error_parks_without_promotion(self):
        def broken(_worktree):
            raise RuntimeError("not a git tree")

        self.apply(RunnerHarness(), broken)
        self.assertEqual(self.task["status"], "BLOCKED")
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

    def test_expired_lease_without_snapshot_is_blocked_not_retried(self):
        task = self.state["tasks"]["t1"]
        stale = NOW - hd.DEFAULT_LEASE_TTL_S - 1
        task["lease"]["acquired_at"] = stale
        task["lease"]["expires_at"] = stale + hd.DEFAULT_LEASE_TTL_S
        self.assertTrue(hd.reconcile_snapshot(self.state, {}, self.manifest, NOW))
        self.assertEqual(task["status"], "BLOCKED")
        self.assertEqual(task["needs_attention"], "snapshot_unavailable")
        self.assertIsNotNone(task["lease"])
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


ACK = {"id": "fastlink-dispatch-subscribe", "result": {"type": "subscription_started"}}


class ProtocolSchemaTests(unittest.TestCase):
    """Exact Herdr v0.9.1 JSONL protocol: subscribe frame, ack, envelope."""

    class FakeSocket:
        """Scripted JSONL transport: send_frame records; next_line pops."""

        def __init__(self, lines):
            self._lines = list(lines)
            self.sent = []

        def send_frame(self, text):
            self.sent.append(text)

        def next_line(self, timeout=None):
            return self._lines.pop(0) if self._lines else None

    def feed_with(self, lines):
        sock = self.FakeSocket(lines)
        return sock, hd.JsonlSocketFeed(sock)

    def real_event(self, agent="opencode", status="working", pane="w2:pG"):
        return {
            "event": "pane.agent_status_changed",
            "data": {"agent": agent, "agent_status": status,
                     "pane_id": pane, "workspace_id": "w2"},
        }

    def test_subscribe_frame_is_exact_v091(self):
        sock, feed = self.feed_with([json.dumps(ACK) + "\n"])
        feed.subscribe(["w2:pG"])
        self.assertEqual(json.loads(sock.sent[0]), {
            "id": "fastlink-dispatch-subscribe",
            "method": "events.subscribe",
            "params": {"subscriptions": [
                {"type": "pane.agent_status_changed", "pane_id": "w2:pG"},
            ]},
        })

    def test_subscribe_filters_by_pane_and_needs_a_pane(self):
        sock, feed = self.feed_with([json.dumps(ACK) + "\n"])
        feed.subscribe(["w2:pG", "w2:pB"])
        subs = json.loads(sock.sent[0])["params"]["subscriptions"]
        self.assertEqual(subs, [
            {"type": "pane.agent_status_changed", "pane_id": "w2:pG"},
            {"type": "pane.agent_status_changed", "pane_id": "w2:pB"},
        ])
        with self.assertRaises(hd.DispatcherError):
            self.feed_with([])[1].subscribe([])

    def test_valid_subscription_started_ack_is_accepted(self):
        _, feed = self.feed_with([json.dumps(ACK) + "\n"])
        feed.subscribe(["w2:pG"])  # must not raise

    def test_bad_ack_raises_and_is_never_served_as_event(self):
        def check(bad_ack):
            lines = [json.dumps(bad_ack) + "\n",
                     json.dumps(self.real_event()) + "\n"]
            _, feed = self.feed_with(lines)
            with self.assertRaises(hd.DispatcherError):
                feed.subscribe(["w2:pG"])
            self.assertEqual(list(feed), [self.real_event()])

        check({"id": "fastlink-dispatch-subscribe", "result": {"type": "subscribed_other"}})
        check({"id": "wrong-request", "result": {"type": "subscription_started"}})
        check({"id": "fastlink-dispatch-subscribe", "error": {"message": "denied"}})

    def test_missing_ack_at_eof_raises(self):
        _, feed = self.feed_with([])
        with self.assertRaises(hd.DispatcherError):
            feed.subscribe(["w2:pG"])

    def test_subscribe_frame_id_is_a_string(self):
        # Live finding (2026-11-07): an integer request id made the v0.9.1
        # server close the connection before any ack; only a string
        # correlation id completed the handshake.
        sock, feed = self.feed_with([json.dumps(ACK) + "\n"])
        feed.subscribe(["w2:pG"])
        rid = json.loads(sock.sent[0])["id"]
        self.assertIsInstance(rid, str)
        self.assertTrue(rid)

    def test_integer_ack_id_never_matches_string_request_id(self):
        # The integer id from the failed live probe is still not a valid ack
        # for a string-id subscription request, and it is not an event.
        lines = [
            json.dumps({"id": 1, "result": {"type": "subscription_started"}})
            + "\n",
            json.dumps(self.real_event()) + "\n",
        ]
        _, feed = self.feed_with(lines)
        with self.assertRaises(hd.DispatcherError):
            feed.subscribe(["w2:pG"])
        self.assertFalse(feed._subscribed)
        self.assertEqual(feed._pending, [self.real_event()])

    def test_subscription_ack_wait_is_bounded(self):
        times = iter([0.0, 0.0, 0.5, 1.1])
        sock = self.FakeSocket([hd._READ_TIMEOUT, hd._READ_TIMEOUT])
        feed = hd.JsonlSocketFeed(
            sock, ack_timeout_s=1.0, clock=lambda: next(times))
        with self.assertRaisesRegex(hd.DispatcherError, "timed out"):
            feed.subscribe(["w2:pG"])

    def test_pre_ack_event_buffer_is_bounded(self):
        event_line = json.dumps(self.real_event()) + "\n"
        lines = [event_line] * (hd.MAX_PENDING_EVENTS + 1)
        sock = self.FakeSocket(lines)
        feed = hd.JsonlSocketFeed(sock, ack_timeout_s=100)
        with self.assertRaisesRegex(hd.DispatcherError, "buffer exceeded"):
            feed.subscribe(["w2:pG"])
        self.assertEqual(len(feed._pending), hd.MAX_PENDING_EVENTS)

    def test_transcript_records_raw_ack_and_notifications(self):
        import io
        sink = io.StringIO()
        sock = self.FakeSocket(
            [json.dumps(ACK) + "\n", json.dumps(self.real_event()) + "\n"]
        )
        feed = hd.JsonlSocketFeed(sock, read_timeout=None, transcript=sink)
        feed.subscribe(["w2:pG"])
        events = list(feed)
        self.assertEqual(events, [self.real_event()])
        self.assertEqual(
            sink.getvalue(),
            json.dumps(ACK) + "\n" + json.dumps(self.real_event()) + "\n",
        )

    def test_iterator_yields_only_event_envelopes(self):
        lines = [
            json.dumps(ACK) + "\n",
            json.dumps(self.real_event()) + "\n",
            "not-json\n",
            json.dumps({"id": "unrelated-response", "result": {"ok": True}}) + "\n",
            json.dumps({"unrelated": True}) + "\n",
        ]
        _, feed = self.feed_with(lines)
        feed.subscribe(["w2:pG"])
        self.assertEqual(list(feed), [self.real_event()])


class EventEnvelopeTests(TempRepoTest):
    """Envelope unwrapping + exact owner/pane/lease matching on events."""

    def setUp(self):
        super().setUp()
        self.manifest = base_manifest()
        self.state = hd.new_state(self.manifest)
        hd.assign(self.state, "t1", self.manifest, pane_id="w1:p1", now=NOW)
        hd.apply_event(self.state, self.fake_event("working"), self.manifest, NOW,
                       runner=RunnerHarness(), changed_paths=inside_paths([]))
        self.task = self.state["tasks"]["t1"]

    def test_real_envelope_settles_task_to_review(self):
        runner = RunnerHarness()
        changed = hd.apply_event(
            self.state, self.fake_event("done"), self.manifest, NOW,
            runner=runner, changed_paths=inside_paths(["qa/out.py"]),
        )
        self.assertTrue(changed)
        self.assertEqual(self.task["status"], "REVIEW")
        self.assertEqual(runner.calls, [["python", "-c", "print('ok')"]])

    def test_lease_pane_mismatch_is_ignored(self):
        runner = RunnerHarness()
        changed = hd.apply_event(
            self.state, self.fake_event("done", pane="w2:pG"),
            self.manifest, NOW,
            runner=runner, changed_paths=inside_paths(["qa/out.py"]),
        )
        self.assertFalse(changed)
        self.assertEqual(self.task["status"], "ACTIVE")
        self.assertEqual(runner.calls, [])

    def test_agent_owner_mismatch_with_lease_pane_is_ignored(self):
        runner = RunnerHarness()
        changed = hd.apply_event(
            self.state, self.fake_event("done", agent="codex"),
            self.manifest, NOW,
            runner=runner, changed_paths=inside_paths(["qa/out.py"]),
        )
        self.assertFalse(changed)
        self.assertEqual(self.task["status"], "ACTIVE")
        self.assertEqual(runner.calls, [])

    def test_event_without_pane_id_is_ignored(self):
        event = {
            "event": "pane.agent_status_changed",
            "data": {"agent": "herdr-dispatcher", "agent_status": "done",
                     "workspace_id": "w1"},
        }
        runner = RunnerHarness()
        self.assertFalse(
            hd.apply_event(self.state, event, self.manifest, NOW,
                           runner=runner, changed_paths=inside_paths([]))
        )
        self.assertEqual(runner.calls, [])

    def test_non_dict_data_is_ignored(self):
        runner = RunnerHarness()
        self.assertFalse(
            hd.apply_event(
                self.state,
                {"event": "pane.agent_status_changed", "data": 42},
                self.manifest, NOW,
                runner=runner, changed_paths=inside_paths([]),
            )
        )
        self.assertEqual(runner.calls, [])

    def test_real_snapshot_style_extra_fields_are_tolerated(self):
        changed = hd.apply_event(
            self.state, self.fake_event(
                "working", extra={"display_agent": "opencode",
                                  "state_labels": (), "title": "ignored-here"}),
            self.manifest, NOW + 5,
            runner=RunnerHarness(), changed_paths=inside_paths([]),
        )
        self.assertTrue(changed)
        self.assertEqual(self.task["status"], "ACTIVE")
        self.assertEqual(self.task["lease"]["expires_at"], NOW + 5 +
                         hd.DEFAULT_LEASE_TTL_S)

    def test_legacy_flat_recorded_event_still_replays(self):
        event = {
            "event": "pane.agent_status_changed",
            "pane_id": "w1:p1",
            "agent": "herdr-dispatcher",
            "status": "done",
        }
        changed = hd.apply_event(
            self.state, event, self.manifest, NOW,
            runner=RunnerHarness(), changed_paths=inside_paths(["qa/out.py"]),
        )
        self.assertTrue(changed)
        self.assertEqual(self.task["status"], "REVIEW")


class CompletedProcessRunner:
    """Runner that returns real subprocess.CompletedProcess objects."""

    def __init__(self, returncodes):
        self.returncodes = list(returncodes)
        self.calls = []

    def __call__(self, argv, cwd, timeout=None):
        self.calls.append(list(argv))
        rc = self.returncodes.pop(0) if self.returncodes else 0
        return subprocess.CompletedProcess(
            args=[str(a) for a in argv], returncode=rc, stdout="", stderr=""
        )


class CompletedProcessTests(TempRepoTest):
    """subprocess.CompletedProcess handling (review blocker #4)."""

    def setUp(self):
        super().setUp()
        self.manifest = base_manifest()
        self.state = hd.new_state(self.manifest)
        hd.assign(self.state, "t1", self.manifest, pane_id="w1:p1", now=NOW)
        self.task = self.state["tasks"]["t1"]
        self.seq = 10

    def apply(self, runner):
        working = self.fake_event("working")
        working["state_change_seq"] = self.seq
        self.seq += 1
        hd.apply_event(self.state, working, self.manifest, NOW,
                       runner=runner, changed_paths=inside_paths(["qa/out.py"]))
        done = self.fake_event("done")
        done["state_change_seq"] = self.seq
        self.seq += 1
        return hd.apply_event(
            self.state, done, self.manifest, NOW,
            runner=runner, changed_paths=inside_paths(["qa/out.py"]),
        )

    def test_returncode_helper_reads_completed_process(self):
        self.assertEqual(
            hd._returncode(subprocess.CompletedProcess(args=[], returncode=3)), 3)
        self.assertEqual(hd._returncode({"returncode": 0}), 0)
        self.assertIsNone(hd._returncode({"nope": 1}))
        self.assertIsNone(hd._returncode(None))

    def test_subprocess_timeout_is_converted_to_task_timeout(self):
        from unittest.mock import patch
        with patch("tools.herdr_dispatcher.subprocess.run",
                   side_effect=subprocess.TimeoutExpired(cmd=["test"], timeout=1)):
            with self.assertRaises(hd.TestCommandTimeout):
                hd.subprocess_runner(["test"], cwd=self.root, timeout=1)

    def test_zero_completed_process_reaches_review(self):
        runner = CompletedProcessRunner([0])
        self.assertTrue(self.apply(runner))
        self.assertEqual(self.task["status"], "REVIEW")

    def test_nonzero_completed_process_is_bounded_failure(self):
        runner = CompletedProcessRunner([1, 1, 1])
        self.apply(runner)
        self.assertEqual(self.task["status"], "READY")
        self.apply(runner)
        self.assertEqual(self.task["status"], "READY")
        self.apply(runner)
        self.assertEqual(self.task["status"], "FAILED")
        self.assertEqual(self.task["attempts"], 3)
        self.assertEqual(len(runner.calls), 3)

    def test_missing_returncode_parks_indeterminate(self):
        runner = CompletedProcessRunner([None])
        self.apply(runner)
        self.assertEqual(self.task["status"], "BLOCKED")
        self.assertEqual(self.task["needs_attention"], "test_probe_indeterminate")
        self.assertEqual(self.task["attempts"], 1)
        self.assertEqual(len(runner.calls), 1)


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
        event = self.fake_event("done")
        state = hd.new_state(manifest)
        hd.reconcile_snapshot(state, {}, manifest, NOW)
        hd.assign(state, "t1", manifest, pane_id="w1:p1", now=NOW)
        hd.apply_event(state, self.fake_event("working"), manifest, NOW,
                       runner=runner, changed_paths=inside_paths(["qa/out.py"]))
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
        events_path = self.write_events(
            "events.jsonl", [self.fake_event("working"), self.fake_event("done")])
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


class SocketDispatchCliTests(TempRepoTest):
    class FakeFeed:
        def __init__(self, events):
            self.events = list(events)
            self.pane_ids = None
            self.closed = False
            self.yielded = 0
        def subscribe(self, pane_ids):
            self.pane_ids = list(pane_ids)
        def __iter__(self):
            for event in self.events:
                self.yielded += 1
                yield event
        def close(self):
            self.closed = True

    def test_disconnect_reconnect_recovers_snapshot_and_ignores_duplicate(self):
        task = make_task("reconnect")
        task["owner"] = "opencode"
        manifest_path = self.write_manifest(
            base_manifest(tasks=[task], allowed_workers=["opencode"]))
        state_path = self.root / "state.json"
        snapshots = iter([
            {"snapshot": {"agents": [{"agent": "opencode", "name": "opencode",
             "agent_status": "idle", "pane_id": "w2:pG", "workspace_id": "w2",
             "interactive_ready": True, "state_change_seq": 42}]}},
            {"snapshot": {"agents": [{"agent": "opencode", "name": "opencode",
             "agent_status": "working", "pane_id": "w2:pG", "workspace_id": "w2",
             "interactive_ready": True, "state_change_seq": 43}]}},
            {"snapshot": {"agents": [{"agent": "opencode", "name": "opencode",
             "agent_status": "idle", "pane_id": "w2:pG", "workspace_id": "w2",
             "interactive_ready": True, "state_change_seq": 44}]}},
            {"snapshot": {"agents": [{"agent": "opencode", "name": "opencode",
             "agent_status": "idle", "pane_id": "w2:pG", "workspace_id": "w2",
             "interactive_ready": True, "state_change_seq": 44}]}},
        ])
        done = {"event": "pane.agent_status_changed", "data": {
            "agent": "opencode", "agent_status": "done", "pane_id": "w2:pG",
            "workspace_id": "w2"}}
        feeds = [self.FakeFeed([]), self.FakeFeed([done, dict(done)])]
        prompts, runner = [], RunnerHarness()
        code = hd.run_cli(
            ["socket", "--tasks", str(manifest_path), "--state", str(state_path),
             "--socket", "fake", "--max-events", "2", "--max-runtime-seconds", "5",
             "--max-reconnects", "1"],
            runner=runner, changed_paths=inside_paths(["qa/out.py"]), now=NOW,
            snapshot_provider=lambda: next(snapshots),
            prompt_agent=lambda agent, text: prompts.append((agent, text)) or
                {"type": "agent_prompted", "agent": agent, "pane_id": "w2:pG"},
            feed_factory=lambda _path: feeds.pop(0), branch_for=lambda _: "feat/test",
            sleep_fn=lambda _seconds: None,
        )
        state = hd.load_state(state_path)
        self.assertEqual(code, 0)
        self.assertEqual(len(prompts), 1)  # no second handoff after reconnect
        self.assertEqual(runner.calls, [["python", "-c", "print('ok')"]])
        self.assertEqual(state["tasks"]["reconnect"]["status"], "REVIEW")
        self.assertEqual(state["tasks"]["reconnect"]["attempts"], 1)

    def test_unrelated_events_still_consume_event_budget(self):
        task = make_task("noise")
        task["owner"] = "opencode"
        manifest_path = self.write_manifest(
            base_manifest(tasks=[task], allowed_workers=["opencode"]))
        state_path = self.root / "noise-state.json"
        snapshot = {"snapshot": {"agents": [{"agent": "opencode", "name": "opencode",
            "agent_status": "idle", "pane_id": "w2:pG", "workspace_id": "w2",
            "interactive_ready": True, "state_change_seq": 42}]}}
        feed = self.FakeFeed([{"event": "unrelated"}] * 10)
        prompts = []
        code = hd.run_cli(
            ["socket", "--tasks", str(manifest_path), "--state", str(state_path),
             "--socket", "fake", "--max-events", "2", "--max-runtime-seconds", "5",
             "--max-reconnects", "0"],
            runner=RunnerHarness(), changed_paths=inside_paths([]), now=NOW,
            snapshot_provider=lambda: snapshot,
            prompt_agent=lambda agent, text: prompts.append(agent) or
                {"type": "agent_prompted"},
            feed_factory=lambda _path: feed, branch_for=lambda _: "feat/test",
            sleep_fn=lambda _seconds: None,
        )
        state = hd.load_state(state_path)
        self.assertEqual(code, 0)
        self.assertEqual(feed.yielded, 2)
        self.assertEqual(prompts, ["opencode"])
        self.assertEqual(state["tasks"]["noise"]["status"], "ACTIVE")

    def test_open_socket_runtime_limit_terminates_timeout_stream(self):
        task = make_task("timebound")
        task["owner"] = "opencode"
        manifest_path = self.write_manifest(
            base_manifest(tasks=[task], allowed_workers=["opencode"]))
        state_path = self.root / "time-state.json"
        snapshot = {"snapshot": {"agents": [{"agent": "opencode", "name": "opencode",
            "agent_status": "idle", "pane_id": "w2:pG", "workspace_id": "w2",
            "interactive_ready": True, "state_change_seq": 42}]}}
        class OpenTimeoutFeed:
            def __init__(self):
                self.closed = False
            def subscribe(self, _pane_ids):
                return None
            def __iter__(self):
                while True:
                    yield None
            def close(self):
                self.closed = True
        feed = OpenTimeoutFeed()
        started = time.monotonic()
        code = hd.run_cli(
            ["socket", "--tasks", str(manifest_path), "--state", str(state_path),
             "--socket", "fake", "--max-events", "100", "--max-runtime-seconds", "0.01",
             "--max-reconnects", "0"],
            runner=RunnerHarness(), changed_paths=inside_paths([]), now=NOW,
            snapshot_provider=lambda: snapshot,
            prompt_agent=lambda *_: {"type": "agent_prompted"},
            feed_factory=lambda _path: feed, branch_for=lambda _: "feat/test",
            sleep_fn=lambda _seconds: None,
        )
        self.assertEqual(code, 0)
        self.assertLess(time.monotonic() - started, 1.0)
        self.assertTrue(feed.closed)
        self.assertTrue(state_path.exists())

    def test_connect_retry_budget_is_enforced(self):
        task = make_task("connect-fail")
        task["owner"] = "opencode"
        manifest_path = self.write_manifest(
            base_manifest(tasks=[task], allowed_workers=["opencode"]))
        state_path = self.root / "connect-state.json"
        with self.assertRaisesRegex(hd.DispatcherError, "connect/snapshot failed"):
            hd.run_cli(
                ["socket", "--tasks", str(manifest_path), "--state", str(state_path),
                 "--socket", "fake", "--max-events", "2", "--max-runtime-seconds", "5",
                 "--max-reconnects", "0"],
                snapshot_provider=lambda: (_ for _ in ()).throw(RuntimeError("snapshot down")),
                prompt_agent=lambda *_: {"type": "agent_prompted"},
                feed_factory=lambda _path: self.fail("feed must not be opened"),
                sleep_fn=lambda _seconds: None,
            )
        self.assertTrue(state_path.exists())

    def test_lagging_settled_snapshot_after_working_event_recovers(self):
        task = make_task("lagging")
        task["owner"] = "opencode"
        manifest_path = self.write_manifest(
            base_manifest(tasks=[task], allowed_workers=["opencode"]))
        state_path = self.root / "lagging-state.json"
        snapshots = iter([
            {"snapshot": {"agents": [{"agent": "opencode", "name": "opencode",
             "agent_status": "idle", "pane_id": "w2:pG", "workspace_id": "w2",
             "interactive_ready": True, "state_change_seq": 42}]}},
            {"snapshot": {"agents": [{"agent": "opencode", "name": "opencode",
             "agent_status": "idle", "pane_id": "w2:pG", "workspace_id": "w2",
             "interactive_ready": True, "state_change_seq": 42}]}},
            {"snapshot": {"agents": [{"agent": "opencode", "name": "opencode",
             "agent_status": "idle", "pane_id": "w2:pG", "workspace_id": "w2",
             "interactive_ready": True, "state_change_seq": 44}]}},
        ])
        feed = self.FakeFeed([
            {"event": "pane.agent_status_changed", "data": {
                "agent": "opencode", "agent_status": "working", "pane_id": "w2:pG",
                "workspace_id": "w2"}},
            {"event": "pane.agent_status_changed", "data": {
                "agent": "opencode", "agent_status": "done", "pane_id": "w2:pG",
                "workspace_id": "w2"}},
        ])
        runner = RunnerHarness()
        code = hd.run_cli(
            ["socket", "--tasks", str(manifest_path), "--state", str(state_path),
             "--socket", "fake", "--max-events", "2", "--max-runtime-seconds", "5",
             "--max-reconnects", "0"],
            runner=runner, changed_paths=inside_paths(["qa/out.py"]), now=NOW,
            snapshot_provider=lambda: next(snapshots),
            prompt_agent=lambda *_: {"type": "agent_prompted"},
            feed_factory=lambda _path: feed, branch_for=lambda _: "feat/test",
            sleep_fn=lambda _seconds: None,
        )
        state = hd.load_state(state_path)
        task_state = state["tasks"]["lagging"]
        self.assertEqual(code, 0)
        self.assertEqual(task_state["status"], "REVIEW")
        self.assertEqual(task_state["attempts"], 1)
        self.assertEqual(task_state["lease"]["state_change_seq"], 44)
        self.assertIsNone(task_state["needs_attention"])
        self.assertEqual(len(runner.calls), 1)

    def test_socket_mode_dispatches_persists_and_reaches_review(self):
        task = make_task("smoke")
        task["owner"] = "opencode"
        manifest_path = self.write_manifest(
            base_manifest(tasks=[task], allowed_workers=["opencode"]))
        state_path = self.root / "state.json"
        snapshots = iter([
            {"snapshot": {"agents": [{"agent": "opencode", "name": "opencode",
             "agent_status": "idle", "pane_id": "w2:pG", "workspace_id": "w2",
             "interactive_ready": True, "state_change_seq": 42}]}},
            {"snapshot": {"agents": [{"agent": "opencode", "name": "opencode",
             "agent_status": "working", "pane_id": "w2:pG", "workspace_id": "w2",
             "interactive_ready": True, "state_change_seq": 43}]}},
            {"snapshot": {"agents": [{"agent": "opencode", "name": "opencode",
             "agent_status": "idle", "pane_id": "w2:pG", "workspace_id": "w2",
             "interactive_ready": True, "state_change_seq": 44}]}},
        ])
        feed = self.FakeFeed([
            {"event": "pane.agent_status_changed", "data": {
                "agent": "opencode", "agent_status": "working", "pane_id": "w2:pG",
                "workspace_id": "w2"}},
            {"event": "pane.agent_status_changed", "data": {
                "agent": "opencode", "agent_status": "done", "pane_id": "w2:pG",
                "workspace_id": "w2"}},
        ])
        prompts = []
        runner = RunnerHarness()
        code = hd.run_cli(
            ["socket", "--tasks", str(manifest_path), "--state", str(state_path),
             "--socket", "fake", "--max-events", "2", "--max-runtime-seconds", "5",
             "--max-reconnects", "0"],
            runner=runner,
            changed_paths=inside_paths(["qa/out.py"]),
            now=NOW,
            snapshot_provider=lambda: next(snapshots),
            prompt_agent=lambda agent, text: prompts.append((agent, text)) or
                {"type": "agent_prompted"},
            feed_factory=lambda _path: feed,
            branch_for=lambda _worktree: "feat/test",
            sleep_fn=lambda _seconds: None,
        )
        state = hd.load_state(state_path)
        self.assertEqual(code, 0)
        self.assertEqual(feed.pane_ids, ["w2:pG"])
        self.assertTrue(feed.closed)
        self.assertEqual([agent for agent, _ in prompts], ["opencode"])
        self.assertEqual(state["tasks"]["smoke"]["status"], "REVIEW")
        self.assertEqual(len(runner.calls), 1)
        self.assertEqual(feed.yielded, 2)


class LiveSnapshotReconcileTests(TempRepoTest):
    def setUp(self):
        super().setUp()
        task = make_task("t1")
        task["owner"] = "opencode"
        self.manifest = base_manifest(tasks=[task], allowed_workers=["opencode"])
        self.state = hd.new_state(self.manifest)
        hd.assign(self.state, "t1", self.manifest, pane_id="w2:pG", now=NOW)
        self.task = self.state["tasks"]["t1"]
        self.task["lease"]["state_change_seq"] = 42
        self.task["lease"]["dispatch_status"] = "submitted"

    def snapshot(self, status, seq=43):
        return {"id": "snapshot", "result": {"snapshot": {"agents": [
            {"agent": "opencode", "name": "opencode", "agent_status": status,
             "pane_id": "w2:pG", "workspace_id": "w2", "state_change_seq": seq}
        ]}}}

    def test_snapshot_refreshes_exact_active_worker_lease(self):
        self.assertTrue(hd.reconcile_snapshot(
            self.state, self.snapshot("working"), self.manifest, NOW + 10))
        self.assertEqual(self.task["status"], "ACTIVE")
        self.assertTrue(self.task["lease"]["observed_working"])
        self.assertGreater(self.task["lease"]["expires_at"], NOW + 10)

    def test_snapshot_settled_after_observed_working_runs_tests(self):
        self.task["lease"]["observed_working"] = True
        runner = RunnerHarness()
        self.assertTrue(hd.reconcile_snapshot(
            self.state, self.snapshot("idle", seq=44), self.manifest, NOW + 20,
            runner=runner, changed_paths=inside_paths(["qa/out.py"])))
        self.assertEqual(self.task["status"], "REVIEW")
        self.assertEqual(len(runner.calls), 1)

    def test_snapshot_sequence_recovers_settled_status_after_lost_working_event(self):
        runner = RunnerHarness()
        self.assertTrue(hd.reconcile_snapshot(
            self.state, self.snapshot("idle", seq=44), self.manifest, NOW + 20,
            runner=runner, changed_paths=inside_paths(["qa/out.py"])))
        self.assertEqual(self.task["status"], "REVIEW")
        self.assertEqual(len(runner.calls), 1)

    def test_same_sequence_snapshot_waits_then_parks_after_expiry(self):
        runner = RunnerHarness()
        self.assertFalse(hd.reconcile_snapshot(
            self.state, self.snapshot("idle", seq=42), self.manifest, NOW + 20,
            runner=runner, changed_paths=inside_paths(["qa/out.py"])))
        self.assertEqual(self.task["status"], "ACTIVE")
        self.assertIsNone(self.task["needs_attention"])
        self.assertEqual(self.task["attempts"], 0)
        self.assertTrue(hd.reconcile_snapshot(
            self.state, self.snapshot("idle", seq=42), self.manifest, NOW + 3700,
            runner=runner, changed_paths=inside_paths(["qa/out.py"])))
        self.assertEqual(self.task["status"], "BLOCKED")
        self.assertEqual(self.task["needs_attention"], "prompt_not_observed")
        self.assertEqual(runner.calls, [])

    def test_stale_settled_snapshot_after_working_event_does_not_park(self):
        working = {"event": "pane.agent_status_changed", "data": {
            "agent": "opencode", "agent_status": "working", "pane_id": "w2:pG",
            "workspace_id": "w2"}}
        self.assertTrue(hd.apply_event(
            self.state, working, self.manifest, NOW + 1,
            runner=RunnerHarness(), changed_paths=inside_paths([])))
        self.assertFalse(hd.reconcile_snapshot(
            self.state, self.snapshot("idle", seq=42), self.manifest, NOW + 10,
            runner=RunnerHarness(), changed_paths=inside_paths(["qa/out.py"])))
        self.assertEqual(self.task["status"], "ACTIVE")
        self.assertTrue(self.task["lease"]["observed_working"])
        self.assertIsNone(self.task["needs_attention"])
        self.assertEqual(self.task["attempts"], 0)

    def test_retry_snapshot_requires_a_new_dispatch_handoff(self):
        self.task["lease"]["observed_working"] = True
        self.task["lease"]["state_change_seq"] = 42
        done = {"event": "pane.agent_status_changed", "data": {
            "agent": "opencode", "agent_status": "done", "pane_id": "w2:pG",
            "workspace_id": "w2", "state_change_seq": 43}}
        runner = RunnerHarness(["fail"])
        self.assertTrue(hd.apply_event(
            self.state, done, self.manifest, NOW + 1,
            runner=runner, changed_paths=inside_paths(["qa/out.py"])))
        self.assertEqual(self.task["status"], "READY")
        self.assertTrue(self.task["lease"]["needs_retry"])
        snapshot = self.snapshot("idle", seq=44)
        self.assertFalse(hd.reconcile_snapshot(
            self.state, snapshot, self.manifest, NOW + 2,
            runner=runner, changed_paths=inside_paths(["qa/out.py"])))
        self.assertEqual(len(runner.calls), 1)
        prompts = []
        self.assertEqual(hd.dispatch_ready_tasks(
            self.state, self.manifest, snapshot, now=NOW + 3,
            prompt_agent=lambda owner, text: prompts.append(owner) or
                {"type": "agent_prompted"},
            branch_for=lambda _path: "feat/test", persist=lambda: None), ["t1"])
        self.assertEqual(prompts, ["opencode"])
        self.assertEqual(self.task["status"], "ACTIVE")
        self.assertFalse(self.task["lease"]["needs_retry"])
        self.assertEqual(self.task["lease"]["state_change_seq"], 44)
        self.assertEqual(len(runner.calls), 1)

    def test_restarted_durable_testing_state_is_blocked_not_reexecuted(self):
        task = self.task
        task["status"] = "TESTING"
        task["attempts"] = 1
        path = self.root / "state.json"
        hd.save_state(path, self.state)
        resumed = hd.load_state(path)
        runner = RunnerHarness()
        self.assertTrue(hd.reconcile_snapshot(
            resumed, self.snapshot("idle", seq=44), self.manifest, NOW + 30,
            runner=runner, changed_paths=inside_paths(["qa/out.py"])))
        self.assertEqual(resumed["tasks"]["t1"]["status"], "BLOCKED")
        self.assertEqual(resumed["tasks"]["t1"]["needs_attention"], "test_interrupted")
        self.assertEqual(resumed["tasks"]["t1"]["attempts"], 1)
        self.assertEqual(runner.calls, [])

    def test_state_survives_restart_snapshot_recovery_and_duplicate_replay(self):
        self.task["lease"]["dispatch_status"] = "submitted"
        self.task["lease"]["dispatch_receipt"] = {"type": "agent_prompted"}
        working = {"event": "pane.agent_status_changed", "data": {
            "agent": "opencode", "agent_status": "working", "pane_id": "w2:pG",
            "workspace_id": "w2", "state_change_seq": 43}}
        self.assertTrue(hd.apply_event(
            self.state, working, self.manifest, NOW + 1,
            runner=RunnerHarness(), changed_paths=inside_paths([])))
        path = self.root / "restart-state.json"
        hd.save_state(path, self.state)

        resumed = hd.load_state(path)  # simulate process restart after disconnect
        runner = RunnerHarness()
        snapshot = self.snapshot("idle", seq=44)
        self.assertTrue(hd.reconcile_snapshot(
            resumed, snapshot, self.manifest, NOW + 10, runner=runner,
            changed_paths=inside_paths(["qa/out.py"]),
            persist=lambda: hd.save_state(path, resumed)))
        self.assertEqual(resumed["tasks"]["t1"]["status"], "REVIEW")
        self.assertEqual(resumed["tasks"]["t1"]["attempts"], 1)
        self.assertEqual(len(runner.calls), 1)

        recovered = hd.load_state(path)
        duplicate = {"event": "pane.agent_status_changed", "data": {
            "agent": "opencode", "agent_status": "done", "pane_id": "w2:pG",
            "workspace_id": "w2", "state_change_seq": 44}}
        self.assertFalse(hd.apply_event(
            recovered, duplicate, self.manifest, NOW + 11, runner=runner,
            changed_paths=inside_paths(["qa/out.py"])))
        self.assertFalse(hd.reconcile_snapshot(
            recovered, snapshot, self.manifest, NOW + 11, runner=runner,
            changed_paths=inside_paths(["qa/out.py"])))
        self.assertEqual(len(runner.calls), 1)
        hd.save_state(path, recovered)
        final = hd.load_state(path)
        self.assertEqual(final["tasks"]["t1"]["status"], "REVIEW")
        self.assertEqual(final["tasks"]["t1"]["attempts"], 1)

    def test_missing_live_lease_pane_is_parked_not_retried(self):
        snapshot = {"snapshot": {"agents": []}}
        self.assertTrue(hd.reconcile_snapshot(
            self.state, snapshot, self.manifest, NOW + 20))
        self.assertEqual(self.task["status"], "BLOCKED")
        self.assertEqual(self.task["needs_attention"], "lease_agent_missing")
        self.assertIsNotNone(self.task["lease"])


class LiveDispatchHelperTests(TempRepoTest):
    """Auto-dispatch is allowlist-, workspace-, and lease-bound."""

    def setUp(self):
        super().setUp()
        task = make_task("smoke")
        task["owner"] = "opencode"
        task["title"] = "Create a scoped smoke marker"
        task["prompt"] = "Create exactly qa/marker.txt containing PASS."
        self.manifest = base_manifest(tasks=[task], allowed_workers=["opencode"])
        self.state = hd.new_state(self.manifest)
        self.snapshot = {"snapshot": {"agents": [
            {"agent": "opencode", "name": "opencode", "agent_status": "idle",
             "pane_id": "w2:pG", "workspace_id": "w2", "interactive_ready": True,
             "state_change_seq": 42},
            {"agent": "grok", "name": "grok", "agent_status": "idle",
             "pane_id": "w2:p8", "workspace_id": "w2", "interactive_ready": True,
             "state_change_seq": 17},
        ]}}

    def test_snapshot_selects_only_allowlisted_live_worker(self):
        self.assertTrue(callable(getattr(hd, "find_allowlisted_live_workers", None)))
        workers = hd.find_allowlisted_live_workers(self.snapshot, self.manifest)
        self.assertEqual(list(workers), ["opencode"])
        self.assertEqual(workers["opencode"]["pane_id"], "w2:pG")

    def test_ready_task_is_handed_to_only_idle_allowlisted_worker(self):
        calls, saves = [], []
        handed = hd.dispatch_ready_tasks(
            self.state, self.manifest, self.snapshot, now=NOW,
            prompt_agent=lambda agent, text: calls.append((agent, text)) or
                {"type": "agent_prompted"},
            branch_for=lambda worktree: "feat/test",
            persist=lambda: saves.append(self.state["revision"]),
        )
        self.assertEqual(handed, ["smoke"])
        self.assertEqual([agent for agent, _ in calls], ["opencode"])
        task = self.state["tasks"]["smoke"]
        self.assertEqual(task["status"], "ACTIVE")
        self.assertEqual(task["lease"]["pane_id"], "w2:pG")
        self.assertEqual(task["lease"]["state_change_seq"], 42)
        self.assertEqual(task["lease"]["dispatch_receipt"]["type"], "agent_prompted")
        self.assertIn("feat/test", calls[0][1])
        self.assertIn("qa/marker.txt", calls[0][1])
        self.assertGreaterEqual(len(saves), 2)

    def test_busy_or_unready_worker_is_not_prompted(self):
        calls = []
        self.snapshot["snapshot"]["agents"][0]["agent_status"] = "working"
        handed = hd.dispatch_ready_tasks(
            self.state, self.manifest, self.snapshot, now=NOW,
            prompt_agent=lambda *args: calls.append(args),
            branch_for=lambda _: "feat/test", persist=lambda: None,
        )
        self.assertEqual(handed, [])
        self.assertEqual(calls, [])
        self.snapshot["snapshot"]["agents"][0]["agent_status"] = "idle"
        self.snapshot["snapshot"]["agents"][0]["interactive_ready"] = False
        self.assertEqual(hd.dispatch_ready_tasks(
            self.state, self.manifest, self.snapshot, now=NOW,
            prompt_agent=lambda *args: calls.append(args),
            branch_for=lambda _: "feat/test", persist=lambda: None,
        ), [])
        self.assertEqual(calls, [])

    def test_failed_prompt_is_parked_without_blind_retry(self):
        handed = hd.dispatch_ready_tasks(
            self.state, self.manifest, self.snapshot, now=NOW,
            prompt_agent=lambda *_: {"type": "error", "message": "uncertain"},
            branch_for=lambda _: "feat/test", persist=lambda: None,
        )
        task = self.state["tasks"]["smoke"]
        self.assertEqual(handed, [])
        self.assertEqual(task["status"], "BLOCKED")
        self.assertEqual(task["needs_attention"], "prompt_uncertain")

    def test_wrong_branch_is_parked_instead_of_prompted(self):
        calls = []
        hd.dispatch_ready_tasks(
            self.state, self.manifest, self.snapshot, now=NOW,
            prompt_agent=lambda *args: calls.append(args),
            branch_for=lambda _: "main", persist=lambda: None,
        )
        self.assertEqual(calls, [])
        self.assertEqual(self.state["tasks"]["smoke"]["status"], "BLOCKED")
        self.assertEqual(self.state["tasks"]["smoke"]["needs_attention"],
                         "branch_mismatch")

    def test_no_unallowlisted_or_duplicate_worker_dispatch(self):
        second = make_task("second")
        second["owner"] = "opencode"
        self.manifest["tasks"].append(second)
        hd.validate_manifest(self.manifest)
        self.state = hd.new_state(self.manifest)
        calls = []
        handed = hd.dispatch_ready_tasks(
            self.state, self.manifest, self.snapshot, now=NOW,
            prompt_agent=lambda agent, text: calls.append(agent) or
                {"type": "agent_prompted"},
            branch_for=lambda _: "feat/test", persist=lambda: None,
        )
        self.assertEqual(handed, ["smoke"])
        self.assertEqual(calls, ["opencode"])
        self.assertEqual(self.state["tasks"]["second"]["status"], "READY")


class DuplicateEventTests(TempRepoTest):
    def test_duplicate_settled_event_does_not_repeat_failed_tests(self):
        task = make_task("t1")
        task["owner"] = "opencode"
        manifest = base_manifest(tasks=[task], allowed_workers=["opencode"])
        state = hd.new_state(manifest)
        hd.assign(state, "t1", manifest, pane_id="w2:pG", now=NOW)
        working_event = {"event": "pane.agent_status_changed", "data": {
            "agent": "opencode", "agent_status": "working", "pane_id": "w2:pG",
            "workspace_id": "w2"}}
        hd.apply_event(state, working_event, manifest, NOW,
                       runner=RunnerHarness(), changed_paths=inside_paths([]))
        event = {"event": "pane.agent_status_changed", "data": {
            "agent": "opencode", "agent_status": "done", "pane_id": "w2:pG",
            "workspace_id": "w2"}}
        runner = CompletedProcessRunner([1, 0])
        kwargs = {"runner": runner, "changed_paths": inside_paths(["qa/out.py"])}
        self.assertTrue(hd.apply_event(state, event, manifest, NOW, **kwargs))
        self.assertEqual(state["tasks"]["t1"]["attempts"], 1)
        hd.apply_event(state, working_event, manifest, NOW + 1, **kwargs)
        self.assertFalse(hd.apply_event(state, event, manifest, NOW + 2, **kwargs))
        self.assertEqual(state["tasks"]["t1"]["attempts"], 1)
        self.assertEqual(len(runner.calls), 1)


if __name__ == "__main__":
    unittest.main()
