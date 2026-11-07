# Herdr orchestration — resumable FASTLINK task dispatcher

Status: implemented, unit-proven on fakes; live-Herdr integration is declared
but not yet exercised (see acceptance criteria below). No file in this
document is an observed performance measurement.

## What this module is

`tools/herdr_dispatcher.py` combines:

1. a **resumable, dependency-aware task queue** persisted in a single JSON
   state file with atomic writes, unique task ids, monotonic revision, and
   idempotent event replay (crash-safe recovery from stale leases);
2. a **Herdr event-driven dispatcher** that reacts to
   `pane.agent_status_changed` events consumed from a JSONL Unix-socket
   feed (`events.subscribe`), with one bounded snapshot reconciliation pass
   as the deterministic fallback.

It never prompts, interrupts, or controls any live agent; dispatch happens
exclusively through explicit operator handoffs recorded in state.

## Task manifest (`orchestration/tasks.json`)

- `manifest_version`, `allowed_workers` (explicit allowlist), `tasks[]`.
- Each task: unique `id`, `owner` (must be allowlisted), `deps`, `branch`,
  `worktree`, `owned_paths`, `test_cmds` (argv lists only — never shell
  strings), optional `cwd` (defaults to `worktree`).
- Validation rejects duplicates, orphan/missing deps, dependency cycles,
  empty owned paths, non-argv commands, and non-allowlisted owners.

## State machine

```
BACKLOG --deps DONE/FAILED/BLOCKED--> READY / BLOCKED   (snapshot reconcile)
READY ---explicit `assign` (allowlisted owner)--> ACTIVE (+lease)
ACTIVE --owner pane done/idle--> TESTING   (lease-bound)
ACTIVE --owner pane blocked--> BLOCKED (needs_attention=agent_blocked)
ACTIVE --owner pane unknown--> parked (needs_attention=uncertain_status; no retry, no DONE)
ACTIVE --owner pane working--> lease refreshed (hearts stays ACTIVE)
TESTING --all argv cmds exit 0 AND changed paths ⊆ owned_paths--> REVIEW   (terminal for automation)
TESTING --definite failure, attempts<3--> READY (lease kept; next settled event retries)
TESTING --definite failure, attempts=3--> FAILED (retry_budget_exhausted)
TESTING --timeout--> parked (needs_attention=test_timeout; operator `resume` required)
TESTING --changed path outside owned_paths--> FAILED (scope_violation)
TESTING --changed-path probe error--> parked (needs_attention=scope_unverified)
REVIEW --explicit `approve`--> DONE   (merging is out of scope)
ACTIVE/READY --lease older than TTL at reconcile--> READY (stale_lease_recovered)
```

All numbers in history/leases are wall-clock seconds supplied by the caller;
nothing here is inferred from hardware telemetry.

## Safety invariants

- **Allowlist only.** Assignment and event handling both re-check the owner
  against `allowed_workers`; unknown/unrelated agents are never assigned.
- **Explicit handoffs only.** `assign TASK:PANE` (or `--assign TASK:PANE`)
  is the sole path to ACTIVE; events can never promote a task with no lease.
- **Scope fence.** After manifest-listed tests pass, every changed path must
  normalize inside `owned_paths` (relative, no `..`, no absolute paths);
  violations fail the task instead of resting in REVIEW.
- **No blind retries / no premature DONE.** Timeouts and uncertain agent
  states park with `needs_attention`; only `resume`/`approve` clear them,
  and `approve` is never reached by automation beyond REVIEW.
- **Atomic persistence.** `save_state` writes temp + fsync + `os.replace`;
  revision increments exactly once per state-creating transition, so
  replaying events against a saved state is byte-identical.

## Run modes

```bash
# Deterministic bounded pass (temporary state; no services touched):
python3 tools/herdr_dispatcher.py once \
  --tasks orchestration/tasks.json --state /tmp/fastlink-herdr-state.json \
  --events events-sample.jsonl --assign herdr-dispatcher-core:w1:pN

# Continuous bounded run (declared, experimental; do not run near live
# agents without explicit owner authorization — dispatches no prompts):
python3 tools/herdr_dispatcher.py socket \
  --tasks orchestration/tasks.json --state state.json \
  --socket <herdr unix socket path> --max-events 64

# Operator verbs:
... assign --task <id>:<pane>   # explicit allowlisted handoff + lease
... approve --task <id>         # REVIEW -> DONE (explicit only)
... resume --task <id>          # clear needs_attention after inspection
... status                      # print all task statuses
```

Runtime state (e.g. `state.json`) is an operator artifact created under
`--state`; it is not part of the committed deliverables and should stay out
of version control.

## Unit-verified (qa/test_herdr_dispatcher.py, 33 tests, all green)

Manifest validation matrix; dependency resolution/cycle detection; explicit
assignment + refusals (non-allowlisted owner, double assign, BACKLOG target);
no-event-only-dispatch invariant; done/idle→TESTING→REVIEW with manifest argv
tests; uncertain parked; blocked→working recovery; unrelated-agent filtering;
bounded retries (fail/fail/fail→FAILED at attempts=3); timeout parked without
retry or DONE; scope violation → FAILED; scope probe error parked; REVIEW
terminal until explicit approve; stale-lease vs live-lease reconcile with
zero attempt cost; JSONL feed (one subscribe frame, malformed lines skipped,
EOF stop); state round-trip; idempotent event replay saving identical bytes;
`--once` CLI determinism with injected clock and mocked runner. All tests use
fake event feeds, mock runners and temp dirs; no live socket is contacted.

## Acceptance criteria met / not yet met

Met in this worktree: dispatcher + queue + tests + docs as owned files.

Not yet met (needs a separate, explicitly authorized session — deliberately
not attempted here because live agents/services must not be controlled):
- a live-Herdr E2E subscription run (`socket` mode) against a real
  `events.subscribe` feed, including arrived-status vocabulary verification;
- real handoff loop with an allowlisted worker pane, from `assign` through
  REVIEW with a real changed-path probe;
- stochastic feed soak (network drop/reconnect semantics) once the real
  socket reconnection policy is defined;
- wiring into a FASTLINK GPU/production task plan — blocked by the repo's
  standing GPU/owner-approval gates.
