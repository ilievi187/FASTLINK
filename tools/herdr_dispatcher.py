"""FASTLINK resumable task queue driven by the local Herdr event stream.

Design goal: a persistent, dependency-aware task queue plus a continuous
dispatcher that reacts to Herdr `pane.agent_status_changed` events for
explicitly allowlisted workers, and submits only manifest-scoped tasks to
idle matching panes. It never controls or interrupts unrelated agents.

Safety invariants (enforced here and documented in docs/HERDR_ORCHESTRATION.md):

- Tasks come from a deterministic JSON manifest with field ownership
  (owner, branch, worktree, owned_paths) and argv-list-only test commands.
- The persistent socket loop may submit one manifest-defined task to an explicitly allowlisted live idle worker; it never prompts an unrelated agent.
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

Verified Herdr v0.9.1 wire facts (live session; implemented verbatim):

  request frame:  {"id": "fastlink-dispatch-subscribe", "method": "events.subscribe",
                   "params": {"subscriptions": [
                       {"type": "pane.agent_status_changed", "pane_id": "w2:pG"}]}}
  success ack:    {"id": "fastlink-dispatch-subscribe",
                   "result": {"type": "subscription_started"}}
  pushed event:   {"event": "pane.agent_status_changed",
                   "data": {"agent": "opencode", "agent_status": "working",
                            "pane_id": "w2:pG", "workspace_id": "w2"}}

An event acts on a task only when the event's pane_id equals that task's
active lease pane_id AND the event agent equals the task owner. Only
`pane.agent_status_changed` is acted on: subscription acks, other response
frames and malformed lines are never treated as events. The status
vocabulary is idle/working/blocked/done/unknown.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import posixpath
import subprocess
import sys
import time
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
_READ_TIMEOUT = object()
MAX_EVENT_FRAME_BYTES = 1_048_576
MAX_PENDING_EVENTS = 256
DEFAULT_SUBSCRIBE_ACK_TIMEOUT_S = 10.0
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


def _snapshot_body(snapshot: dict) -> dict:
    """Accept either Herdr's CLI response or its inner session snapshot."""
    if not isinstance(snapshot, dict):
        return {}
    result = snapshot.get("result")
    if isinstance(result, dict):
        snapshot = result.get("snapshot", result)
    if not isinstance(snapshot, dict):
        return {}
    nested = snapshot.get("snapshot")
    return nested if isinstance(nested, dict) else snapshot


def find_allowlisted_live_workers(snapshot: dict, manifest: dict) -> dict:
    """Map explicitly allowlisted worker names to unique live pane records.

    A duplicated name, missing pane, or malformed entry is excluded instead
    of guessed. The snapshot must be current; this function never consults a
    remembered pane ID on its own.
    """
    body = _snapshot_body(snapshot)
    rows = body.get("agents")
    if not isinstance(rows, list):
        return {}
    allowed = set(manifest.get("allowed_workers", []))
    matches = {}
    ambiguous = set()
    for row in rows:
        if not isinstance(row, dict):
            continue
        name = row.get("name") or row.get("agent")
        pane_id = row.get("pane_id")
        if name not in allowed or not isinstance(pane_id, str) or not pane_id:
            continue
        if name in matches:
            ambiguous.add(name)
        else:
            matches[name] = row
    return {name: row for name, row in matches.items() if name not in ambiguous}


def build_task_prompt(task: dict) -> str:
    """Render the manifest fields into a bounded, file-scoped agent handoff."""
    objective = task.get("prompt") or task.get("title") or task["id"]
    paths = "\n".join(f"- {path}" for path in task["owned_paths"])
    tests = "\n".join(f"- {argv!r}" for argv in task.get("test_cmds", [])) or "- none"
    return (
        f"FASTLINK task {task['id']}: {task.get('title', task['id'])}\n"
        f"Objective: {objective}\n"
        f"Repository/worktree: {task['worktree']}\n"
        f"Required branch: {task['branch']}\n"
        f"Owned paths (edit only these):\n{paths}\n"
        f"Acceptance tests (argv):\n{tests}\n"
        "Do not modify other worktrees, production, active model services, or GPUs. "
        "Do not merge. Report the changed files, test results, and blockers."
    )


def _current_branch(worktree: str) -> str | None:
    proc = subprocess.run(
        ["git", "-C", worktree, "branch", "--show-current"],
        capture_output=True, text=True, timeout=10,
    )
    return proc.stdout.strip() if proc.returncode == 0 else None


def herdr_prompt_agent(agent: str, text: str) -> dict:
    """Submit one prompt through the supported Herdr CLI and parse its receipt."""
    proc = subprocess.run(
        ["herdr", "agent", "prompt", agent, text],
        capture_output=True, text=True, timeout=30,
    )
    if proc.returncode != 0:
        raise DispatcherError(proc.stderr.strip() or "Herdr rejected agent prompt")
    try:
        frame = json.loads(proc.stdout)
    except ValueError as exc:
        raise DispatcherError("Herdr prompt returned invalid JSON") from exc
    result = frame.get("result") if isinstance(frame, dict) else None
    if not isinstance(result, dict) or result.get("type") != "agent_prompted":
        raise DispatcherError("Herdr prompt did not return an agent_prompted receipt")
    return result


def dispatch_ready_tasks(
    state: dict,
    manifest: dict,
    snapshot: dict,
    *,
    now: float,
    prompt_agent,
    branch_for=None,
    persist=None,
) -> list[str]:
    """Hand at most one READY task to each allowlisted, live idle worker.

    The matching live pane is leased before prompting. Ambiguous snapshots,
    busy workers, wrong branches, and uncertain prompt outcomes never trigger
    a second blind submission.
    """
    workers = find_allowlisted_live_workers(snapshot, manifest)
    branch_for = branch_for or _current_branch
    persist = persist or (lambda: None)
    assigned = []
    busy_owners = {
        task["owner"]
        for task in state["tasks"].values()
        if task.get("lease") is not None
        and (task["status"] in (STATUS_ACTIVE, STATUS_BLOCKED, STATUS_TESTING)
             or (task["status"] == STATUS_READY
                 and not task["lease"].get("needs_retry")))
    }
    for owner, worker in workers.items():
        if owner in busy_owners:
            continue
        if worker.get("agent_status") not in ("idle", "done"):
            continue
        if worker.get("interactive_ready") is False:
            continue
        pane_id = worker["pane_id"]
        ready = next((task for task in state["tasks"].values()
                      if task["owner"] == owner and task["status"] == STATUS_READY
                      and (task.get("lease") is None
                           or task["lease"].get("needs_retry"))), None)
        if ready is None:
            continue
        if branch_for(ready.get("worktree", "")) != ready.get("branch"):
            _transition(state, ready, STATUS_BLOCKED, "branch_mismatch", now)
            _needs_attention(ready, "branch_mismatch", now)
            persist()
            continue
        retry = ready.get("lease") is not None
        if retry:
            lease = ready["lease"]
            if lease.get("pane_id") != pane_id:
                _transition(state, ready, STATUS_BLOCKED, "retry_pane_mismatch", now)
                _needs_attention(ready, "retry_pane_mismatch", now)
                persist()
                continue
            lease["needs_retry"] = False
            lease["acquired_at"] = now
            lease["expires_at"] = now + DEFAULT_LEASE_TTL_S
            lease["state_change_seq"] = worker.get("state_change_seq")
            lease["observed_working"] = False
            lease["settled_event_seen"] = False
            lease["dispatch_status"] = "submitting"
            _transition(state, ready, STATUS_ACTIVE, "bounded_retry_handoff", now)
        else:
            if not assign(state, ready["id"], manifest, pane_id=pane_id, now=now):
                continue
            lease = ready["lease"]
            lease["state_change_seq"] = worker.get("state_change_seq")
            lease["dispatch_status"] = "submitting"
        persist()  # durable lease before the external prompt side effect
        try:
            receipt = prompt_agent(owner, build_task_prompt(ready))
        except Exception:
            _transition(state, ready, STATUS_BLOCKED, "prompt_outcome_uncertain", now)
            _needs_attention(ready, "prompt_uncertain", now)
            persist()
            continue
        if not isinstance(receipt, dict) or receipt.get("type") != "agent_prompted":
            _transition(state, ready, STATUS_BLOCKED, "prompt_not_accepted", now)
            _needs_attention(ready, "prompt_uncertain", now)
            persist()
            continue
        lease["dispatch_status"] = "submitted"
        lease["dispatch_receipt"] = receipt
        lease["prompted_at"] = now
        _bump(state)
        persist()
        assigned.append(ready["id"])
        busy_owners.add(owner)
    return assigned


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
        "observed_working": False,
        "settled_event_seen": False,
    }
    _transition(state, task, STATUS_ACTIVE, "explicit_handoff", now)
    return True


def approve(state: dict, task_id: str, manifest: dict) -> bool:
    task = state["tasks"].get(task_id)
    if task is None or task["status"] != STATUS_REVIEW:
        return False
    _transition(state, task, STATUS_DONE, "explicit_operator_approval", 0.0)
    return True


def resume(state: dict, task_id: str, now: float = 0.0) -> bool:
    """Operator-confirmed retry after inspecting a parked BLOCKED task."""
    task = state["tasks"].get(task_id)
    if task is None or task["status"] != STATUS_BLOCKED or not task.get("needs_attention"):
        return False
    task["lease"] = None
    task["needs_attention"] = None
    _transition(state, task, STATUS_READY, "resumed_by_operator", now)
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


def _returncode(result):
    """Extract an integer exit code from a subprocess result object.

    Handles real `subprocess.CompletedProcess` (which is not a dict — an
    earlier version treated that as "all tests passed"), recorded dict
    stubs, and bare ints. Anything without a usable integer returncode
    (including None) yields None, which is never interpreted as success.
    """
    if isinstance(result, subprocess.CompletedProcess):
        rc = result.returncode
    elif isinstance(result, dict):
        rc = result.get("returncode")
    elif isinstance(result, (int, float)) and not isinstance(result, bool):
        rc = int(result)
    else:
        rc = None
    if rc is None:
        return None
    try:
        return int(rc)
    except (TypeError, ValueError):
        return None


def _run_testing(
    state: dict,
    task: dict,
    manifest: dict,
    now: float,
    runner,
    changed_paths,
    max_attempts: int,
    *,
    persist=None,
    event_signature=None,
) -> bool:
    persist = persist or (lambda: None)
    _transition(state, task, STATUS_TESTING, "settled_agent_turn", now)
    task["attempts"] += 1
    if event_signature is not None:
        task["last_event_signature"] = event_signature
    if task.get("lease") is not None:
        task["lease"]["settled_event_seen"] = True
        task["lease"]["observed_working"] = False
    _bump(state)
    persist()  # durable TESTING lease before any external test command
    for argv in task.get("test_cmds", []):
        try:
            result = runner(argv, task.get("cwd", task["worktree"]),
                            timeout=DEFAULT_TEST_TIMEOUT_S)
        except TestCommandTimeout:
            _transition(state, task, STATUS_BLOCKED, "test_timeout", now)
            _needs_attention(task, "test_timeout", now)
            persist()
            return True
        except Exception:
            _transition(state, task, STATUS_BLOCKED, "test_execution_error", now)
            _needs_attention(task, "test_execution_error", now)
            persist()
            return True
        rc = _returncode(result)
        if rc is None:
            _transition(state, task, STATUS_BLOCKED, "test_probe_indeterminate", now)
            _needs_attention(task, "test_probe_indeterminate", now)
            persist()
            return True
        if rc != 0:
            if task["attempts"] >= max_attempts:
                _transition(
                    state, task, STATUS_FAILED, "tests_failed_after_budget", now
                )
                _needs_attention(task, "retry_budget_exhausted", now)
            else:
                # Bounded retry: keep the exact pane lease, but require the
                # dispatcher to submit one new prompt before accepting a
                # later settled event.
                task["lease"]["needs_retry"] = True
                _transition(state, task, STATUS_READY, "test_failed_bounded_retry", now)
            persist()
            return True
    try:
        changed = list(changed_paths(task["worktree"]))
    except Exception:
        _transition(state, task, STATUS_BLOCKED, "scope_unverified", now)
        _needs_attention(task, "scope_unverified", now)
        persist()
        return True
    violations = [p for p in changed if not inside_owned(p, task["owned_paths"])]
    if violations:
        _transition(state, task, STATUS_FAILED, "scope_violation", now)
        _needs_attention(task, "scope_violation", now)
        persist()
        return True
    _transition(state, task, STATUS_REVIEW, "tests_passed_scope_verified", now)
    persist()
    return True


# ---------------------------------------------------------------------------
# Event application


def _event_fields(event: dict):
    """Unwrap a v0.9.1 notification into (status, agent, pane_id, workspace).

    Real pushed frame (verified live):

        {"event": "pane.agent_status_changed",
         "data": {"agent": "opencode", "agent_status": "working",
                  "pane_id": "w2:pG", "workspace_id": "w2"}}

    Events recorded to files (replay) may carry the same fields at top
    level; that flat shape is a deliberate lenient fallback for recorded
    events only, and the exact matching rules below are unchanged either
    way.
    """
    if event.get("event") != EVENT_NAME:
        return None
    data = event.get("data")
    if data is not None and not isinstance(data, dict):
        return None
    if data is None:
        data = event
    status = data.get("agent_status") or data.get("status")
    agent = data.get("agent") or data.get("agent_name") or data.get("name")
    pane = data.get("pane_id")
    if not pane and isinstance(event.get("pane"), dict):
        pane = event["pane"].get("pane_id")
    workspace = data.get("workspace_id") or event.get("workspace_id")
    return status, agent, pane, workspace


def apply_event(
    state: dict,
    event: dict,
    manifest: dict,
    now: float,
    *,
    runner=None,
    changed_paths=None,
    max_attempts: int = DEFAULT_MAX_ATTEMPTS,
    persist=None,
) -> bool:
    fields = _event_fields(event) if isinstance(event, dict) else None
    if fields is None:
        return False
    status, agent, pane, _workspace = fields
    if not isinstance(agent, str) or not agent:
        return False
    if agent not in manifest["allowed_workers"]:
        return False
    if not isinstance(pane, str) or not pane:
        # Exact pane identity is mandatory; nothing acts on unknown panes.
        return False
    candidates = [
        entry
        for entry in state["tasks"].values()
        if entry["owner"] == agent
    ]
    if not candidates:
        return False
    # Exact owner + lease + pane matching: an event acts on a task only
    # when the event's pane_id equals that task's active lease pane_id,
    # and the event agent already equals the task owner. Events for
    # unleased or differently-leased panes are ignored; this path never
    # manufactures an assignment.
    actionable = [
        entry
        for entry in candidates
        if entry.get("lease")
        and entry["lease"].get("pane_id") == pane
        and (
            entry["status"] in AUTO_STATUSES
            or (entry["status"] == STATUS_READY and entry.get("lease"))
        )
    ]
    if not actionable:
        return False
    entry = actionable[0]
    # Parked tasks require an explicit operator resume; agent traffic cannot
    # silently clear uncertainty or re-run a task after a crash.
    if entry.get("needs_attention"):
        return False
    event_signature = hashlib.sha256(
        json.dumps(event, sort_keys=True, separators=(",", ":")).encode("utf-8")
    ).hexdigest()
    if not isinstance(status, str):
        return False
    lease = entry.get("lease")
    payload = event.get("data") if isinstance(event.get("data"), dict) else event
    sequence = payload.get("state_change_seq") if isinstance(payload, dict) else None
    if sequence is None and isinstance(event, dict):
        sequence = event.get("state_change_seq")
    if isinstance(sequence, bool) or not isinstance(sequence, int):
        sequence = None
    if status == "working" and sequence is not None:
        previous = lease.get("state_change_seq") if lease else None
        if isinstance(previous, int) and sequence <= previous:
            return False
        if lease is not None:
            lease["state_change_seq"] = sequence
    seen_signatures = entry.get("seen_event_signatures", [])
    if status in _SETTLED:
        lease = entry.get("lease")
        if not lease or not lease.get("observed_working") or \
                lease.get("settled_event_seen"):
            return False
        if event_signature in seen_signatures:
            return False
        if sequence is not None:
            previous = lease.get("state_change_seq")
            if isinstance(previous, int) and sequence <= previous:
                return False
            lease["state_change_seq"] = sequence

    def record_event(changed: bool) -> bool:
        if changed:
            if status in _SETTLED:
                signatures = entry.setdefault("seen_event_signatures", [])
                signatures.append(event_signature)
                if len(signatures) > 128:
                    del signatures[:-128]
            _bump(state)
        return changed

    if status == "unknown":
        return record_event(_needs_attention(entry, "uncertain_status", now))
    if status == "blocked":
        if entry["status"] == STATUS_BLOCKED:
            return record_event(_needs_attention(entry, "agent_blocked", now))
        _transition(state, entry, STATUS_BLOCKED, "agent_blocked", now)
        _needs_attention(entry, "agent_blocked", now)
        return record_event(True)
    if status == "working":
        refreshed = False
        if entry.get("lease") is not None:
            entry["lease"]["expires_at"] = now + DEFAULT_LEASE_TTL_S
            entry["lease"]["observed_working"] = True
            entry["lease"]["settled_event_seen"] = False
            refreshed = True
        if entry["status"] != STATUS_ACTIVE:
            _transition(state, entry, STATUS_ACTIVE, "working_again", now)
            refreshed = True
        cleared = _needs_attention(entry, None, now)
        return record_event(refreshed or cleared)
    if status in _SETTLED:
        if runner is None or changed_paths is None:
            raise DispatcherError("runner and changed_paths required for TESTING")
        return record_event(_run_testing(
            state, entry, manifest, now, runner, changed_paths, max_attempts,
            persist=persist, event_signature=event_signature,
        ))
    return False


# ---------------------------------------------------------------------------
# Bounded snapshot reconciliation


def reconcile_snapshot(
    state: dict,
    snapshot: dict,
    manifest: dict,
    now: float,
    *,
    runner=None,
    changed_paths=None,
    persist=None,
) -> bool:
    """Reconcile leases against a live Herdr snapshot when one is available.

    Missing/malformed snapshots never release an expired lease for blind
    retry. A real snapshot must match both the leased pane and allowlisted
    owner; missing workers are parked for operator attention.
    """
    persist = persist or (lambda: None)
    changed = _propagate_backlog(state, manifest, now)
    for task in state["tasks"].values():
        if task["status"] == STATUS_TESTING:
            # A durable TESTING state means the prior process may have died
            # mid-command; never rerun that command from a replayed event.
            _transition(state, task, STATUS_BLOCKED,
                        "test_interrupted_after_restart", now)
            _needs_attention(task, "test_interrupted", now)
            changed = True
    body = _snapshot_body(snapshot)
    rows = body.get("agents")
    if not isinstance(rows, list):
        for task in state["tasks"].values():
            lease = task.get("lease")
            if lease and now > lease.get("expires_at", now):
                if task["status"] != STATUS_BLOCKED:
                    _transition(state, task, STATUS_BLOCKED,
                                "snapshot_unavailable_stale_lease", now)
                if _needs_attention(task, "snapshot_unavailable", now):
                    changed = True
                changed = True
        if changed:
            persist()
        return changed

    by_pane = {}
    ambiguous = set()
    for row in rows:
        if not isinstance(row, dict):
            continue
        pane_id = row.get("pane_id")
        if not isinstance(pane_id, str) or not pane_id:
            continue
        if pane_id in by_pane:
            ambiguous.add(pane_id)
        else:
            by_pane[pane_id] = row

    for task in state["tasks"].values():
        lease = task.get("lease")
        if lease is None or task["status"] in (STATUS_REVIEW, STATUS_DONE, STATUS_FAILED):
            continue
        if task.get("needs_attention"):
            continue  # a parked lease requires explicit operator resume
        if task["status"] == STATUS_READY and lease.get("needs_retry"):
            continue  # retries require a new dispatcher prompt before reconciliation
        pane_id = lease.get("pane_id")
        row = None if pane_id in ambiguous else by_pane.get(pane_id)
        if row is None:
            if task["status"] != STATUS_BLOCKED:
                _transition(state, task, STATUS_BLOCKED, "lease_agent_missing", now)
                changed = True
            if _needs_attention(task, "lease_agent_missing", now):
                changed = True
            continue
        owner = row.get("name") or row.get("agent")
        if owner != task["owner"]:
            if task["status"] != STATUS_BLOCKED:
                _transition(state, task, STATUS_BLOCKED, "lease_owner_mismatch", now)
                changed = True
            if _needs_attention(task, "lease_owner_mismatch", now):
                changed = True
            continue
        status = row.get("agent_status")
        if not isinstance(status, str):
            if _needs_attention(task, "uncertain_status", now):
                changed = True
            continue
        if status in _SETTLED:
            before = lease.get("state_change_seq")
            current = row.get("state_change_seq")
            sequence_advanced = (
                isinstance(before, int) and not isinstance(before, bool)
                and isinstance(current, int) and not isinstance(current, bool)
                and current > before
            )
            if not sequence_advanced:
                # A lagging/stale settled snapshot is not a failed task. Keep
                # the lease live until expiry; only then park for inspection.
                if now >= lease.get("expires_at", now):
                    reason = ("prompt_not_observed" if not lease.get("observed_working")
                              else "settled_state_not_advanced")
                    if task["status"] != STATUS_BLOCKED:
                        _transition(state, task, STATUS_BLOCKED, reason, now)
                        changed = True
                    if _needs_attention(task, reason, now):
                        changed = True
                continue
            # The pane's sequence advanced after our last observed state. This
            # recovers a lost intermediate `working` event without treating a
            # replayed settled frame as a new completion.
            if not lease.get("observed_working"):
                lease["observed_working"] = True
                changed = True
        if status in _SETTLED and (runner is None or changed_paths is None):
            if task["status"] != STATUS_BLOCKED:
                _transition(state, task, STATUS_BLOCKED,
                            "snapshot_settled_without_test_runner", now)
                changed = True
            if _needs_attention(task, "test_runner_unavailable", now):
                changed = True
            continue
        event = {"event": EVENT_NAME, "data": {
            "agent": owner,
            "agent_status": status,
            "pane_id": pane_id,
            "workspace_id": row.get("workspace_id"),
            "state_change_seq": row.get("state_change_seq"),
        }}
        if apply_event(
            state, event, manifest, now,
            runner=runner, changed_paths=changed_paths, persist=persist,
        ):
            changed = True

    if _propagate_backlog(state, manifest, now):
        changed = True
    if changed:
        persist()
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
    """JSONL protocol client over an injected socket-like transport.

    The transport must provide send_frame(text) and
    next_line(timeout=None) -> str | None (None on EOF, `_READ_TIMEOUT`
    on timeout). Unit tests inject a scripted fake; the live socket is used
    only by the explicitly invoked `socket` command.

    Wire behaviour verified against Herdr v0.9.1 (module docstring):
    `subscribe` sends the exact EventsSubscribeParams frame (one filter
    per allowlisted pane) and REQUIRES the matching
    {"id":..., "result": {"type": "subscription_started"}} ack; any other
    reply — or an error — aborts with DispatcherError before any event is
    consumed. Response/ack frames are never yielded as events; the
    iterator yields only {"event": ...} notification frames.
    """

    SUBSCRIBE_ID = "fastlink-dispatch-subscribe"

    def __init__(
        self, transport, read_timeout=None,
        ack_timeout_s=DEFAULT_SUBSCRIBE_ACK_TIMEOUT_S, clock=None,
        transcript=None,
    ):
        self._transport = transport
        self._read_timeout = read_timeout
        self._ack_timeout_s = ack_timeout_s
        self._clock = clock or time.monotonic
        self._transcript = transcript
        self._subscribed = False
        self._pending = []  # bounded notifications buffered during the ack handshake

    def _next_line(self):
        try:
            return self._transport.next_line(timeout=self._read_timeout)
        except TypeError:
            # Tolerate transports written before the timeout parameter.
            return self._transport.next_line()

    def _record_line(self, line):
        if self._transcript is not None and isinstance(line, str):
            self._transcript.write(line if line.endswith("\n") else line + "\n")
            self._transcript.flush()

    def subscribe(self, pane_ids) -> None:
        subscriptions = [
            {"type": EVENT_NAME, "pane_id": pane}
            for pane in pane_ids or []
        ]
        if not subscriptions:
            raise DispatcherError(
                "events.subscribe requires at least one pane_id filter"
            )
        frame = {
            "id": self.SUBSCRIBE_ID,
            "method": "events.subscribe",
            "params": {"subscriptions": subscriptions},
        }
        self._ack_deadline = self._clock() + self._ack_timeout_s
        self._transport.send_frame(json.dumps(frame, sort_keys=True) + "\n")
        self._await_ack()
        self._subscribed = True

    def _await_ack(self) -> None:
        while True:
            if self._clock() >= self._ack_deadline:
                raise DispatcherError("timed out waiting for subscription_started ack")
            line = self._next_line()
            if line is _READ_TIMEOUT:
                continue
            if line is None:
                raise DispatcherError("EOF before subscription_started ack")
            self._record_line(line)
            text = line.strip()
            if not text:
                continue
            try:
                reply = json.loads(text)
            except ValueError:
                continue
            if isinstance(reply, dict) and reply.get("event") is not None:
                # A notification may slip in before the ack; bound the buffer
                # so a nonresponsive peer cannot grow memory without limit.
                if len(self._pending) >= MAX_PENDING_EVENTS:
                    raise DispatcherError("subscription pre-ack event buffer exceeded")
                self._pending.append(reply)
                continue
            if not isinstance(reply, dict) or reply.get("id") != self.SUBSCRIBE_ID:
                continue  # unrelated response frames are skipped
            result = reply.get("result")
            if isinstance(result, dict) and \
                    result.get("type") == "subscription_started":
                return
            raise DispatcherError(f"invalid subscription ack: {reply!r}")

    def __iter__(self):
        while self._pending:
            yield self._pending.pop(0)
        while True:
            line = self._next_line()
            if line is _READ_TIMEOUT:
                yield None
                continue
            if line is None:
                return
            self._record_line(line)
            text = line.strip()
            if not text:
                continue
            try:
                frame = json.loads(text)
            except ValueError:
                continue
            if isinstance(frame, dict) and frame.get("event"):
                yield frame

    def close(self) -> None:
        close = getattr(self._transport, "close", None)
        if callable(close):
            close()


# ---------------------------------------------------------------------------
# Default production adapters (CPU-only; not used from unit tests)


def subprocess_runner(argv, cwd, timeout=None):
    try:
        return subprocess.run(argv, cwd=cwd, capture_output=True, timeout=timeout)
    except subprocess.TimeoutExpired as exc:
        raise TestCommandTimeout(f"test command timed out: {argv!r}") from exc


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
    """Open the Herdr JSONL Unix socket; line-framed buffered transport."""
    import socket as _socket

    sock = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
    sock.connect(path)

    class _Transport:
        def __init__(self, s):
            self._sock = s
            self._buf = b""

        def send_frame(self, text):
            self._sock.sendall(text.encode("utf-8"))

        def next_line(self, timeout=None):
            while b"\n" not in self._buf:
                try:
                    self._sock.settimeout(timeout)
                    data = self._sock.recv(65536)
                except (_socket.timeout, TimeoutError):
                    return _READ_TIMEOUT
                if not data:
                    return None
                self._buf += data
                if len(self._buf) > MAX_EVENT_FRAME_BYTES and b"\n" not in self._buf:
                    raise DispatcherError("Herdr JSONL frame exceeded size limit")
            line, self._buf = self._buf.split(b"\n", 1)
            if len(line) > MAX_EVENT_FRAME_BYTES:
                raise DispatcherError("Herdr JSONL frame exceeded size limit")
            return line.decode("utf-8", errors="replace") + "\n"

        def close(self):
            self._file.close() if hasattr(self, "_file") else None
            self._sock.close()

    return _Transport(sock)


def herdr_session_snapshot() -> dict:
    """Read a single bounded live Herdr snapshot through the supported CLI."""
    proc = subprocess.run(
        ["herdr", "api", "snapshot"], capture_output=True, text=True, timeout=15
    )
    if proc.returncode != 0:
        raise DispatcherError(proc.stderr.strip() or "Herdr snapshot request failed")
    try:
        result = json.loads(proc.stdout)
    except ValueError as exc:
        raise DispatcherError("Herdr snapshot returned invalid JSON") from exc
    if not isinstance(result, dict) or not isinstance(_snapshot_body(result).get("agents"), list):
        raise DispatcherError("Herdr snapshot lacks an agents list")
    return result


def _run_socket_loop(
    args,
    manifest,
    state,
    *,
    state_path,
    runner,
    changed_paths,
    snapshot_provider,
    prompt_agent,
    feed_factory,
    branch_for,
    sleep_fn,
    now_fn,
):
    if args.max_events < 1 or args.max_runtime_seconds <= 0 or args.max_reconnects < 0:
        raise DispatcherError("socket bounds must be positive (reconnects may be zero)")
    start = time.monotonic()
    deadline = start + args.max_runtime_seconds
    seen = 0
    reconnects = 0

    def persist():
        save_state(state_path, state)

    while seen < args.max_events and time.monotonic() < deadline:
        feed = None
        try:
            snapshot = snapshot_provider()
            if reconcile_snapshot(
                state, snapshot, manifest, now_fn(),
                runner=runner, changed_paths=changed_paths, persist=persist,
            ):
                persist()
            workers = find_allowlisted_live_workers(snapshot, manifest)
            panes = [row["pane_id"] for row in workers.values()]
            if not panes:
                raise DispatcherError("no allowlisted live agent panes in Herdr snapshot")
            feed = feed_factory(args.socket)
            feed.subscribe(panes)
        except Exception as exc:
            persist()
            if feed is not None:
                feed.close()
            reconnects += 1
            if reconnects > args.max_reconnects:
                raise DispatcherError(f"Herdr connect/snapshot failed: {exc}") from exc
            sleep_fn(min(0.1 * (2 ** (reconnects - 1)), 1.0))
            continue

        try:
            dispatch_ready_tasks(
                state, manifest, snapshot, now=now_fn(), prompt_agent=prompt_agent,
                branch_for=branch_for, persist=persist,
            )
            for event in feed:
                if event is None:  # socket read timeout; check runtime bound
                    if time.monotonic() >= deadline:
                        break
                    continue
                seen += 1  # count every notification, even unrelated/no-op events
                fields = _event_fields(event)
                snapshot = None
                if fields is not None:
                    status, owner, pane, _workspace = fields
                    if owner in manifest["allowed_workers"] and pane in panes:
                        try:
                            snapshot = snapshot_provider()
                        except Exception:
                            persist()
                            continue
                        current_workers = find_allowlisted_live_workers(snapshot, manifest)
                        worker = current_workers.get(owner)
                        if worker is None or worker.get("pane_id") != pane:
                            continue
                        current_status = worker.get("agent_status")
                        if status in _SETTLED and current_status not in _SETTLED:
                            if reconcile_snapshot(
                                state, snapshot, manifest, now_fn(),
                                runner=runner, changed_paths=changed_paths, persist=persist,
                            ):
                                persist()
                            continue  # stale settled notification; snapshot wins
                        payload = event.get("data")
                        if isinstance(payload, dict):
                            if status == "working" and current_status in _SETTLED:
                                # Snapshot advanced beyond this working notification;
                                # let reconciliation consume the settled sequence.
                                payload.pop("state_change_seq", None)
                            else:
                                payload["state_change_seq"] = worker.get("state_change_seq")
                        else:
                            event["state_change_seq"] = worker.get("state_change_seq")
                changed = apply_event(
                    state, event, manifest, now_fn(), runner=runner,
                    changed_paths=changed_paths, persist=persist,
                )
                if changed:
                    persist()
                if snapshot is not None:
                    if reconcile_snapshot(
                        state, snapshot, manifest, now_fn(),
                        runner=runner, changed_paths=changed_paths, persist=persist,
                    ):
                        persist()
                    dispatch_ready_tasks(
                        state, manifest, snapshot, now=now_fn(),
                        prompt_agent=prompt_agent, branch_for=branch_for,
                        persist=persist,
                    )
                if seen >= args.max_events or time.monotonic() >= deadline:
                    break
        except Exception:
            # A test or state-processing error is not a transport error: save
            # the last durable state and fail rather than rerunning work.
            persist()
            raise
        finally:
            feed.close()

        if seen >= args.max_events or time.monotonic() >= deadline:
            break
        # EOF is a reconnect boundary. Persisted leases are reconciled against
        # a fresh snapshot before any subsequent handoff.
        reconnects += 1
        if reconnects > args.max_reconnects:
            persist()
            raise DispatcherError("Herdr event socket closed after reconnect budget")
        sleep_fn(min(0.1 * (2 ** (reconnects - 1)), 1.0))
    persist()
    return seen


def _materialize_state(state_path, manifest):
    if Path(state_path).exists():
        return load_state(state_path)
    return new_state(manifest)


def run_cli(
    argv, *, runner=None, changed_paths=None, now=None,
    snapshot_provider=None, prompt_agent=None, feed_factory=None,
    branch_for=None, sleep_fn=None,
) -> int:
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
    stream.add_argument("--max-runtime-seconds", type=float, default=600.0)
    stream.add_argument("--max-reconnects", type=int, default=3)

    for name in ("assign", "approve", "resume"):
        parser_sub = sub.add_parser(name)
        parser_sub.add_argument("--tasks", required=True)
        parser_sub.add_argument("--state", required=True)
        parser_sub.add_argument("--task", required=True)

    status = sub.add_parser("status")
    status.add_argument("--state", required=True)

    args = parser.parse_args(argv)

    clock = time.time() if now is None else now
    now_fn = (lambda: time.time()) if now is None else (lambda: now)

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
            ok = resume(state, args.task, now=clock)
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

    if args.command == "socket":
        runner_fn = runner if runner is not None else subprocess_runner
        changed_fn = changed_paths if changed_paths is not None else git_status_changed_paths
        snapshot_fn = snapshot_provider if snapshot_provider is not None else herdr_session_snapshot
        prompt_fn = prompt_agent if prompt_agent is not None else herdr_prompt_agent
        feed_fn = feed_factory or (
            lambda socket_path: JsonlSocketFeed(
                _open_herdr_transport(socket_path), read_timeout=1.0
            )
        )
        event_count = _run_socket_loop(
            args, manifest, state, state_path=args.state,
            runner=runner_fn, changed_paths=changed_fn,
            snapshot_provider=snapshot_fn, prompt_agent=prompt_fn,
            feed_factory=feed_fn, branch_for=branch_for,
            sleep_fn=sleep_fn or time.sleep, now_fn=now_fn,
        )
        summary = {tid: e["status"] for tid, e in state["tasks"].items()}
        print(json.dumps({"command": "socket", "events": event_count,
                          "revision": state["revision"], "tasks": summary},
                         sort_keys=True))
        return 0

    recorder = {"any": False}
    if args.snapshot:
        with open(args.snapshot, "r", encoding="utf-8") as handle:
            snap = json.load(handle)
        if not isinstance(snap, dict):
            raise StateError("snapshot must be a JSON object")
        if reconcile_snapshot(state, snap, manifest, clock,
                              runner=runner, changed_paths=changed_paths,
                              persist=lambda: save_state(args.state, state)):
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
            persist=lambda: save_state(args.state, state),
        ):
            recorder["any"] = True

    save_state(args.state, state)
    summary = {tid: e["status"] for tid, e in state["tasks"].items()}
    print(json.dumps({"command": args.command, "revision": state["revision"],
                      "tasks": summary}, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(run_cli(sys.argv[1:]))
