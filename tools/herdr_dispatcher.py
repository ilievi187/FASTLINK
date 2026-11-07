"""FASTLINK resumable task queue driven by the local Herdr event stream.

Design goal: a persistent, dependency-aware task queue plus a continuous
dispatcher that reacts to Herdr `pane.agent_status_changed` events on
behalf of *allowlisted* workers only, while never controlling, prompting,
or interrupting any live agent itself.

Safety invariants (enforced here and documented in docs/HERDR_ORCHESTRATION.md):

- Tasks come from a deterministic JSON manifest with field ownership
  (owner, branch, worktree, owned_paths) and argv-list-only test commands.
- There is no automatic dispatch: tasks become ACTIVE only through an
  explicit `assign` handoff that names a manifest-allowlisted owner.
- Completed agent turns immediately run only the manifest-listed argv test
  commands in the task's declared cwd; contexts are regular argv, never
  shell strings.
- Changed-path verification must keep every new/modified path inside
  declared owned_paths before a task may rest in REVIEW; anything else is a
  scope violation and FAILED.
- Automation stops at REVIEW. `approve` is the only path to DONE and it is
  an explicit operator action; merging is out of scope for this tool.
- Timeouts and uncertain status never produce blind retries and never mark
  a task DONE: they park the task with needs_attention until an operator
  `resume`s it.
- Retries are bounded: a task is FAILED after max 3 recorded attempts.
- Persistence is a single JSON state file with atomic replace, monotonic
  revision, and strict idempotent replay of events for crash safety.
- Stale leases are released deterministically during bounded snapshot
  reconciliation (fallback), enabling safe crash recovery.

Event payload assumption (declared; the fake-feed unit tests pin behaviour):

  {"event": "pane.agent_status_changed",
   "pane_id": "w1:pN", "agent": "<owner name>", "status": "<lifecycle>"}

with lenient alternates (agent_status/agent_name, nested "pane"). Only
`pane.agent_status_changed` is acted on; any other JSON object is ignored.
"""
from __future__ import annotations

import argparse
import json
import os
import posixpath
import subprocess
import sys
from pathlib import Path

STATUS_BACKLOG = "BACKLOG"
STATUS_READY = "READY"
STATUS_ACTIVE = "ACTIVE"
STATUS_REVIEW = "REVIEW"
STATUS_TESTING = "TESTING"
STATUS_DONE = "DONE"
STATUS_BLOCKED = "BLOCKED"
STATUS_FAILED = "FAILED"

AUTO_STATUSES = (STATUS_ACTIVE, STATUS_BLOCKED)

DEFAULT_MAX_ATTEMPTS = 3
DEFAULT_TEST_TIMEOUT_S = 120
DEFAULT_LEASE_TTL_S = 3600

EVENT_NAME = "pane.agent_status_changed"
PROTOCOL_VERSION = 1
STATE_VERSION = 1

_SETTLED = ("done", "idle")


class DispatcherError(Exception):
    """Base class for dispatcher failures."""


class ManifestError(DispatcherError):
    """The task manifest is invalid or unsafe."""


class StateError(DispatcherError):
    """The persisted state file is unreadable or not a v1 state."""


class TestCommandTimeout(DispatcherError):
    """A manifest-listed test command exceeded its budget."""


# ---------------------------------------------------------------------------
# Manifest


def validate_manifest(manifest: dict) -> None:
    if not isinstance(manifest, dict):
        raise ManifestError("manifest must be a JSON object")
    if manifest.get("manifest_version") != PROTOCOL_VERSION:
        raise ManifestError("manifest_version must be 1")
    allowed = manifest.get("allowed_workers")
    if (
        not isinstance(allowed, list)
        or not allowed
        or not all(isinstance(w, str) and w for w in allowed)
    ):
        raise ManifestError("allowed_workers must be a non-empty string list")
    tasks = manifest.get("tasks")
    if not isinstance(tasks, list) or not tasks:
        raise ManifestError("tasks must be a non-empty list")
    seen = set()
    for task in tasks:
        if not isinstance(task, dict):
            raise ManifestError("each task must be a JSON object")
        task_id = task.get("id")
        if not isinstance(task_id, str) or not task_id:
            raise ManifestError("task id must be a non-empty string")
        if task_id in seen:
            raise ManifestError(f"duplicate task id: {task_id}")
        seen.add(task_id)
        owner = task.get("owner")
        if not isinstance(owner, str) or not owner:
            raise ManifestError(f"task {task_id}: owner must be a non-empty string")
        if owner not in allowed:
            raise ManifestError(
                f"task {task_id}: owner {owner!r} is not an allowlisted worker"
            )
        for field in ("branch", "worktree"):
            if not isinstance(task.get(field), str) or not task[field]:
                raise ManifestError(f"task {task_id}: {field} must be a non-empty string")
        cwd = task.get("cwd", task["worktree"])
        if not isinstance(cwd, str) or not cwd:
            raise ManifestError(f"task {task_id}: cwd must be a non-empty string")
        owned = task.get("owned_paths")
        if (
            not isinstance(owned, list)
            or not owned
            or not all(isinstance(p, str) and p for p in owned)
        ):
            raise ManifestError(f"task {task_id}: owned_paths must be a non-empty list")
        cmds = task.get("test_cmds")
        if not isinstance(cmds, list) or not all(isinstance(c, list) for c in cmds):
            raise ManifestError(
                "test_cmds must be a list of argv lists (never shell strings)"
            )
        for argv in cmds:
            if not argv or not isinstance(argv, list) or not all(
                isinstance(a, str) and a for a in argv
            ):
                raise ManifestError(
                    f"task {task_id}: each test command must be a non-empty argv list"
                )
        deps = task.get("deps")
        if not isinstance(deps, list) or not all(isinstance(d, str) for d in deps):
            raise ManifestError(f"task {task_id}: deps must be a string list")
    task_ids = {task["id"]: task for task in tasks}
    for task in tasks:
        for dep in task.get("deps", []):
            if dep not in task_ids:
                raise ManifestError(f"task {task['id']}: unknown dependency {dep!r}")
    _assert_acyclic(task_ids)


def _assert_acyclic(tasks: dict) -> None:
    colour = {}  # 0 = unvisited, 1 = in stack, 2 = done

    def visit(node):
        state = colour.get(node, 0)
        if state == 1:
            raise ManifestError(f"dependency cycle involving {node!r}")
        if state == 2:
            return
        colour[node] = 1
        for dep in tasks[node].get("deps", []):
            visit(dep)
        colour[node] = 2

    for node in tasks:
        visit(node)


def load_manifest(path: str | os.PathLike) -> dict:
    with open(path, "r", encoding="utf-8") as handle:
        manifest = json.load(handle)
    validate_manifest(manifest)
    return manifest


# ---------------------------------------------------------------------------
# State helpers


def _transition(
    state: dict, task: dict, to_status: str, reason: str, now: float
) -> None:
    task["history"].append(
        {"at": now, "from": task["status"], "to": to_status, "reason": reason}
    )
    task["status"] = to_status
    _bump(state)


def _needs_attention(task: dict, flag: str | None, now: float) -> bool:
    """Set/clear needs_attention; idempotent, returns True when changed."""
    if task.get("needs_attention") == flag:
        return False
    task["needs_attention"] = flag
    task["history"].append({"at": now, "note": f"needs_attention={flag}"})
    return True


def _bump(state: dict) -> None:
    state["revision"] += 1


def new_state(manifest: dict) -> dict:
    validate_manifest(manifest)
    tasks = {}
    for task in manifest["tasks"]:
        entry = dict(task)
        entry["status"] = STATUS_BACKLOG
        entry["attempts"] = 0
        entry["lease"] = None
        entry["needs_attention"] = None
        entry["history"] = []
        tasks[task["id"]] = entry
    state = {"state_version": STATE_VERSION, "revision": 0, "tasks": tasks}
    # Deterministic bootstrap: dependency-free tasks start READY without
    # history entries or a revision bump.
    for entry in tasks.values():
        if not [dep for dep in entry.get("deps", [])]:
            entry["status"] = STATUS_READY
    return state


def _dep_conditions_met(state: dict, task: dict):
    statuses = []
    for dep in task.get("deps", []):
        dep_entry = state["tasks"].get(dep)
        if dep_entry is None:
            raise ManifestError(f"unknown dependency {dep!r} in state")
        statuses.append(dep_entry["status"])
    return statuses


def _propagate_backlog(state: dict, manifest: dict, now: float) -> bool:
    changed = False
    for task in state["tasks"].values():
        if task["status"] != STATUS_BACKLOG:
            continue
        dep_statuses = _dep_conditions_met(state, task)
        if any(s in (STATUS_FAILED, STATUS_BLOCKED) for s in dep_statuses):
            _transition(
                state, task, STATUS_BLOCKED, "dependency_failed_or_blocked", now
            )
            changed = True
        elif all(s == STATUS_DONE for s in dep_statuses):
            _transition(state, task, STATUS_READY, "dependencies_done", now)
            changed = True
    return changed


# ---------------------------------------------------------------------------
# Explicit assignment (the only dispatch path)


def assign(
    state: dict,
    task_id: str,
    manifest: dict,
    *,
    pane_id: str,
    now: float,
    lease_ttl: float = DEFAULT_LEASE_TTL_S,
) -> bool:
    task = state["tasks"].get(task_id)
    if task is None:
        return False
    if task["owner"] not in manifest["allowed_workers"]:
        return False
    if task["status"] != STATUS_READY:
        return False
    task["lease"] = {
        "holder": task["owner"],
        "pane_id": pane_id,
        "acquired_at": now,
        "expires_at": now + lease_ttl,
    }
    _transition(state, task, STATUS_ACTIVE, "explicit_handoff", now)
    return True


def approve(state: dict, task_id: str, manifest: dict) -> bool:
    task = state["tasks"].get(task_id)
    if task is None or task["status"] != STATUS_REVIEW:
        return False
    _transition(state, task, STATUS_DONE, "explicit_operator_approval", 0.0)
    return True


def resume(state: dict, task_id: str) -> bool:
    task = state["tasks"].get(task_id)
    if task is None or not task.get("needs_attention"):
        return False
    task["needs_attention"] = None
    task["history"].append({"note": "resumed_by_operator"})
    _bump(state)
    return True


# ---------------------------------------------------------------------------
# Scope verification


def inside_owned(changed_path: str, owned_paths: list[str]) -> bool:
    cleaned = posixpath.normpath(changed_path.strip().replace("\\", "/"))
    if cleaned.startswith("/"):
        return False
    if ".." in cleaned.split("/") or cleaned in (".", ""):
        return False
    for owned in owned_paths:
        target = posixpath.normpath(owned.strip())
        if target.endswith("/"):
            target = target.rstrip("/")
        if cleaned == target or cleaned.startswith(target + "/"):
            return True
    return False


# ---------------------------------------------------------------------------
# TESTING phase


def _run_testing(
    state: dict,
    task: dict,
    manifest: dict,
    now: float,
    runner,
    changed_paths,
    max_attempts: int,
) -> bool:
    _transition(state, task, STATUS_TESTING, "settled_agent_turn", now)
    task["attempts"] += 1
    _bump(state)
    for argv in task.get("test_cmds", []):
        try:
            result = runner(argv, task.get("cwd", task["worktree"]),
                            timeout=DEFAULT_TEST_TIMEOUT_S)
        except TestCommandTimeout:
            # Parked, never blind-retried and never marked DONE.
            _needs_attention(task, "test_timeout", now)
            return True
        if isinstance(result, dict) and result.get("returncode", 1) != 0:
            if task["attempts"] >= max_attempts:
                _transition(
                    state, task, STATUS_FAILED, "tests_failed_after_budget", now
                )
                _needs_attention(task, "retry_budget_exhausted", now)
            else:
                # Bounded retry: stay bound by lease; the next settled
                # event from the same holder re-enters TESTING.
                _transition(state, task, STATUS_READY, "test_failed_bounded_retry", now)
            return True
    try:
        changed = list(changed_paths(task["worktree"]))
    except Exception:
        _needs_attention(task, "scope_unverified", now)
        return True
    violations = [p for p in changed if not inside_owned(p, task["owned_paths"])]
    if violations:
        _transition(state, task, STATUS_FAILED, "scope_violation", now)
        _needs_attention(task, "scope_violation", now)
        return True
    _transition(state, task, STATUS_REVIEW, "tests_passed_scope_verified", now)
    return True


# ---------------------------------------------------------------------------
# Event application


def _event_fields(event: dict):
    status = event.get("status") or event.get("agent_status")
    agent = (
        event.get("agent")
        or event.get("agent_name")
        or event.get("name")
    )
    pane = event.get("pane_id")
    if not pane and isinstance(event.get("pane"), dict):
        pane = event["pane"].get("pane_id")
    return status, agent, pane


def apply_event(
    state: dict,
    event: dict,
    manifest: dict,
    now: float,
    *,
    runner=None,
    changed_paths=None,
    max_attempts: int = DEFAULT_MAX_ATTEMPTS,
) -> bool:
    if not isinstance(event, dict):
        return False
    if event.get("event") != EVENT_NAME:
        return False
    status, agent, _pane = _event_fields(event)
    if not isinstance(agent, str) or not agent:
        return False
    if agent not in manifest["allowed_workers"]:
        return False
    candidates = [
        entry
        for entry in state["tasks"].values()
        if entry["owner"] == agent
    ]
    if not candidates:
        return False
    # Dispatch gate: ACTIVE/BLOCKED tasks, plus lease-bound READY tasks so a
    # bounded retry can continue without a fresh operator handoff.
    actionable = [
        entry
        for entry in candidates
        if entry["status"] in AUTO_STATUSES
        or (entry["status"] == STATUS_READY and entry.get("lease"))
    ]
    if not actionable:
        return False
    entry = actionable[0]
    if not isinstance(status, str):
        return False
    if status == "unknown":
        return _needs_attention(entry, "uncertain_status", now) or False
    if status == "blocked":
        if entry["status"] == STATUS_BLOCKED:
            return _needs_attention(entry, "agent_blocked", now)
        _transition(state, entry, STATUS_BLOCKED, "agent_blocked", now)
        _needs_attention(entry, "agent_blocked", now)
        return True
    if status == "working":
        refreshed = False
        if entry.get("lease") is not None:
            entry["lease"]["expires_at"] = now + DEFAULT_LEASE_TTL_S
            refreshed = True
        if entry["status"] != STATUS_ACTIVE:
            _transition(state, entry, STATUS_ACTIVE, "working_again", now)
            refreshed = True
        cleared = _needs_attention(entry, None, now)
        return refreshed or cleared
    if status in _SETTLED:
        if runner is None or changed_paths is None:
            raise DispatcherError("runner and changed_paths required for TESTING")
        return _run_testing(
            state, entry, manifest, now, runner, changed_paths, max_attempts
        )
    return False


# ---------------------------------------------------------------------------
# Bounded snapshot reconciliation


def reconcile_snapshot(
    state: dict, snapshot: dict, manifest: dict, now: float
) -> bool:
    """One bounded reconciliation pass (fallback when the stream is quiet)."""
    changed = _propagate_backlog(state, manifest, now)
    for entry in state["tasks"].values():
        lease = entry.get("lease")
        if lease is None:
            continue
        if now > lease.get("expires_at", now):
            if entry["status"] == STATUS_ACTIVE:
                entry["lease"] = None
                _transition(state, entry, STATUS_READY,
                            "stale_lease_recovered", now)
                _needs_attention(entry, None, now)
                changed = True
            elif entry["status"] == STATUS_READY:
                entry["lease"] = None
                entry["history"].append({"note": "stale_lease_released", "at": now})
                _bump(state)
                changed = True
    return changed


# ---------------------------------------------------------------------------
# Persistence (atomic writes, strict load)


def save_state(path: str | os.PathLike, state: dict) -> None:
    payload = json.dumps(state, sort_keys=True, indent=2) + "\n"
    target = Path(path)
    tmp_name = f"{target.name}.tmp-{os.getpid()}"
    tmp_path = target.parent / tmp_name
    with open(tmp_path, "w", encoding="utf-8") as handle:
        handle.write(payload)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(tmp_path, target)
    try:
        dir_fd = os.open(str(target.parent), os.O_RDONLY)
    except OSError:
        return
    try:
        os.fsync(dir_fd)
    except OSError:
        pass
    finally:
        os.close(dir_fd)


def load_state(path: str | os.PathLike) -> dict:
    with open(path, "r", encoding="utf-8") as handle:
        state = json.load(handle)
    if not isinstance(state, dict) or state.get("state_version") != STATE_VERSION:
        raise StateError("state file is not a v1 FASTLINK dispatcher state")
    if not isinstance(state.get("tasks"), dict):
        raise StateError("state file lacks a task map")
    return state


# ---------------------------------------------------------------------------
# Herdr JSONL Unix-socket feed (protocol layer; never used by unit tests)


class JsonlSocketFeed:
    """JSONL event feed over an injected socket-like transport.

    The transport must provide send_frame(text) and next_line() -> str|None.
    In real operation a Unix-socket adapter satisfies this; unit tests use
    a scripted fake so no live Herdr session is ever contacted from tests.
    """

    SUBSCRIBE_FRAME = {
        "id": 1,
        "method": "events.subscribe",
        "params": {"events": [EVENT_NAME]},
    }

    def __init__(self, transport):
        self._transport = transport
        self._subscribed = False

    def subscribe(self) -> None:
        if self._subscribed:
            return
        self._subscribed = True
        self._transport.send_frame(
            json.dumps(self.SUBSCRIBE_FRAME, sort_keys=True) + "\n"
        )

    def __iter__(self):
        while True:
            line = self._transport.next_line()
            if line is None:
                return
            text = line.strip()
            if not text:
                continue
            try:
                yield json.loads(text)
            except ValueError:
                continue


# ---------------------------------------------------------------------------
# Default production adapters (CPU-only; not used from unit tests)


def subprocess_runner(argv, cwd, timeout=None):
    return subprocess.run(argv, cwd=cwd, capture_output=True, timeout=timeout)


def git_status_changed_paths(worktree):
    proc = subprocess.run(
        ["git", "-C", worktree, "status", "--porcelain"],
        capture_output=True,
        text=True,
        timeout=DEFAULT_TEST_TIMEOUT_S,
    )
    if proc.returncode != 0:
        raise DispatcherError(proc.stderr.strip() or "git status failed")
    lines = []
    for line in proc.stdout.splitlines():
        line = line.strip()
        if len(line) > 3 and line[2] == " ":
            lines.append(line[3:])
        elif "->" in line:
            lines.append(line.split("->", 1)[1].strip())
    return lines


# ---------------------------------------------------------------------------
# CLI


def _read_jsonl(path: str | None):
    if not path:
        return []
    with open(path, "r", encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except ValueError:
                continue
            if isinstance(obj, dict):
                yield obj


def _open_herdr_transport(path: str):
    import socket as _socket

    sock = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
    sock.connect(path)

    class _Transport:
        def __init__(self, s):
            self._file = s.makefile("rb")

        def send_frame(self, text):
            sock.sendall(text.encode("utf-8"))

        def next_line(self):
            data = self._file.readline()
            return data.decode("utf-8") if data else None

    return _Transport(sock)


def _materialize_state(state_path, manifest):
    if Path(state_path).exists():
        return load_state(state_path)
    return new_state(manifest)


def run_cli(argv, *, runner=None, changed_paths=None, now=None) -> int:
    parser = argparse.ArgumentParser(prog="herdr_dispatcher")
    sub = parser.add_subparsers(dest="command", required=True)

    once = sub.add_parser("once", help="single bounded deterministic pass")
    once.add_argument("--tasks", required=True)
    once.add_argument("--state", required=True)
    once.add_argument("--events")
    once.add_argument("--snapshot")
    once.add_argument(
        "--assign", action="append", default=[],
        help="explicit handoff TASK:PANE (repeatable)",
    )

    stream = sub.add_parser("socket", help="bounded run against live JSONL socket")
    stream.add_argument("--tasks", required=True)
    stream.add_argument("--state", required=True)
    stream.add_argument("--socket", required=True)
    stream.add_argument("--max-events", type=int, default=64)
    stream.add_argument(
        "--assign", action="append", default=[], help="TASK:PANE explicit handoff",
    )

    for name in ("assign", "approve", "resume"):
        parser_sub = sub.add_parser(name)
        parser_sub.add_argument("--tasks", required=True)
        parser_sub.add_argument("--state", required=True)
        parser_sub.add_argument("--task", required=True)

    status = sub.add_parser("status")
    status.add_argument("--state", required=True)

    args = parser.parse_args(argv)

    import time as _time

    clock = _time.time() if now is None else now

    if args.command in ("assign", "approve", "resume"):
        manifest = load_manifest(args.tasks)
        state = _materialize_state(args.state, manifest)
        if args.command == "assign":
            if ":" not in args.task:
                parser.error("--task must name TASK:PANE for assign")
            tid, pane = args.task.split(":", 1)
            ok = assign(state, tid, manifest, pane_id=pane, now=clock)
        elif args.command == "approve":
            ok = approve(state, args.task, manifest)
        else:
            ok = resume(state, args.task)
        if not ok:
            print(json.dumps({"command": args.command, "task": args.task,
                              "result": "refused", "reason_code": 2}))
            return 2
        save_state(args.state, state)
        print(json.dumps({"command": args.command, "task": args.task,
                          "result": "ok"}))
        return 0

    if args.command == "status":
        state = load_state(args.state)
        summary = {tid: e["status"] for tid, e in state["tasks"].items()}
        print(json.dumps({"revision": state["revision"], "tasks": summary},
                         sort_keys=True))
        return 0

    manifest = load_manifest(args.tasks)
    state = _materialize_state(args.state, manifest)
    recorder = {"any": False}

    if args.command == "once":
        if args.snapshot:
            with open(args.snapshot, "r", encoding="utf-8") as handle:
                snap = json.load(handle)
            if not isinstance(snap, dict):
                raise StateError("snapshot must be a JSON object")
            if reconcile_snapshot(state, snap, manifest, clock):
                recorder["any"] = True
        for spec in args.assign:
            if ":" not in spec:
                raise DispatcherError(f"--assign must be TASK:PANE, got {spec!r}")
            tid, pane = spec.split(":", 1)
            if assign(state, tid, manifest, pane_id=pane, now=clock):
                recorder["any"] = True
        for event in _read_jsonl(args.events):
            if apply_event(
                state, event, manifest, clock,
                runner=runner if runner is not None else subprocess_runner,
                changed_paths=changed_paths if changed_paths is not None
                else git_status_changed_paths,
            ):
                recorder["any"] = True
    else:  # socket
        transport = _open_herdr_transport(args.socket)
        feed = JsonlSocketFeed(transport)
        feed.subscribe()
        applied = 0
        for event in feed:
            if applied >= args.max_events:
                break
            if apply_event(
                state, event, manifest, clock,
                runner=runner if runner is not None else subprocess_runner,
                changed_paths=changed_paths if changed_paths is not None
                else git_status_changed_paths,
            ):
                recorder["any"] = True
                applied += 1

    save_state(args.state, state)
    summary = {tid: e["status"] for tid, e in state["tasks"].items()}
    print(json.dumps({"command": args.command, "revision": state["revision"],
                      "tasks": summary}, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(run_cli(sys.argv[1:]))
