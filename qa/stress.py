#!/usr/bin/env python3
"""FASTLINK QA stress harness (CPU-only, loopback-only, bounded).

Scenarios (all use a 10 MB fixture unless noted):
  1. repeated   - 20 sequential sends of the same file; verify each output
                  sha; track server RSS trend (leak check).
  2. concurrent - 8 simultaneous 10 MB sends to one server; all byte-exact.
  3. cancel     - start a 200 MB send, kill the client mid-transfer; server
                  must stay alive and produce no partial output.
  4. restart    - send (build cache), kill server, restart same root, send
                  again; second pass must be warm (0 transferred) + byte-exact.
  5. manyfiles  - 40 distinct 1 MB files sent sequentially; all byte-exact.

Every output is verified with the system sha256sum.  Servers are killed with
a timeout; no scenario may run unbounded.

Usage:
  python3 qa/stress.py                 # all scenarios
  python3 qa/stress.py --only repeated concurrent
"""
from __future__ import annotations

import argparse
import json
import shutil
import socket
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from bench import BIN, FIXDIR, free_port, gen_fixture, parse_stats, read_proc, sha256_file, wait_port  # noqa: E402

QA = Path(__file__).resolve().parent
DATA = QA / "data"
RESULTS = QA / "results"


def start_server(root: Path, port: int):
    root.mkdir(parents=True, exist_ok=True)
    log = RESULTS / f"serve_{Path(root).name}.log"
    log.parent.mkdir(parents=True, exist_ok=True)
    logf = open(log, "w")
    srv = subprocess.Popen(
        [str(BIN), "serve", f"127.0.0.1:{port}", str(root)],
        stdout=logf, stderr=subprocess.STDOUT, start_new_session=True,
    )
    return srv, logf


def stop_server(srv, timeout=6.0):
    srv.terminate()
    try:
        srv.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        srv.kill()
        try:
            srv.wait(timeout=3)
        except subprocess.TimeoutExpired:
            pass


def do_send(fx: Path, port: int, timeout=300) -> subprocess.CompletedProcess:
    return subprocess.run(
        [str(BIN), "send", str(fx), f"127.0.0.1:{port}"],
        capture_output=True, text=True, timeout=timeout,
    )


def sha_ok(src: Path, dst: Path) -> bool:
    if not dst.exists():
        return False
    return sha256_file(src) == sha256_file(dst)


def scenario_repeated(root: Path) -> dict:
    N = 20
    fx = FIXDIR / "s10m.bin"
    if not fx.exists() or fx.stat().st_size != 10 * 1024 * 1024:
        gen_fixture(fx, 10 * 1024 * 1024)
    src_hash = sha256_file(fx)
    port = free_port()
    root.mkdir(parents=True, exist_ok=True)
    srv, logf = start_server(root, port)
    out = root / "outputs" / "s10m.bin"
    rec = {"scenario": "repeated", "iterations": N, "ok": False, "rss_kb": []}
    try:
        assert wait_port(port), "server did not start"
        fails = 0
        for i in range(N):
            r = do_send(fx, port)
            if r.returncode != 0 or not (out.exists() and sha256_file(out) == src_hash):
                fails += 1
                rec.setdefault("fail_iter", i)
            rss = read_proc(srv.pid) or (0, 0)
            rec["rss_kb"].append(rss[1])
    finally:
        stop_server(srv)
        logf.close()
    rec["failures"] = fails
    rec["ok"] = fails == 0
    first, last = rec["rss_kb"][0], rec["rss_kb"][-1]
    rec["rss_first_kb"], rec["rss_last_kb"] = first, last
    rec["leak_growth_kb"] = last - first
    return rec


def scenario_concurrent(root: Path) -> dict:
    """8 DISTINCT 10 MB files sent simultaneously: true parallel sessions on
    one server, shared cache. All outputs must be byte-exact."""
    N = 8
    files = []
    for i in range(N):
        fx = FIXDIR / f"cc{i}.bin"
        if not fx.exists() or fx.stat().st_size != 10 * 1024 * 1024:
            gen_fixture(fx, 10 * 1024 * 1024)
        files.append(fx)
    port = free_port()
    root.mkdir(parents=True, exist_ok=True)
    srv, logf = start_server(root, port)
    rec = {"scenario": "concurrent", "clients": N, "ok": False}
    try:
        assert wait_port(port), "server did not start"
        procs = [
            subprocess.Popen(
                [str(BIN), "send", str(fx), f"127.0.0.1:{port}"],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                start_new_session=True,
            ) for fx in files
        ]
        rcs = []
        for p in procs:
            try:
                p.wait(timeout=120)
            except subprocess.TimeoutExpired:
                p.kill(); p.wait()
            rcs.append(p.returncode)
        sha_ok = all((root / "outputs" / fx.name).exists()
                     and sha256_file(root / "outputs" / fx.name) == sha256_file(fx)
                     for fx in files)
        rec["client_rcs"] = rcs
        rec["sha_ok"] = sha_ok
        rec["ok"] = all(rc == 0 for rc in rcs) and sha_ok
    finally:
        stop_server(srv)
        logf.close()
    return rec


def scenario_same_name(root: Path) -> dict:
    """8 sequential sends of the SAME 10 MB file (CLI serve is sequential:
    it joins each session before accepting the next). All must succeed and
    produce byte-exact output (idempotent re-send)."""
    N = 8
    fx = FIXDIR / "sn10m.bin"
    if not fx.exists() or fx.stat().st_size != 10 * 1024 * 1024:
        gen_fixture(fx, 10 * 1024 * 1024)
    src_hash = sha256_file(fx)
    port = free_port()
    root.mkdir(parents=True, exist_ok=True)
    srv, logf = start_server(root, port)
    rec = {"scenario": "same_name", "clients": N, "ok": False}
    try:
        assert wait_port(port), "server did not start"
        procs = [
            subprocess.Popen(
                [str(BIN), "send", str(fx), f"127.0.0.1:{port}"],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                start_new_session=True,
            ) for _ in range(N)
        ]
        rcs = []
        for p in procs:
            try:
                p.wait(timeout=120)
            except subprocess.TimeoutExpired:
                p.kill(); p.wait()
            rcs.append(p.returncode)
        out = root / "outputs" / "sn10m.bin"
        rec["client_rcs"] = rcs
        rec["output_exists"] = out.exists()
        rec["output_sha_ok"] = out.exists() and sha256_file(out) == src_hash
        rec["server_alive"] = srv.poll() is None
        # All 8 sequential sends must succeed and produce correct output
        rec["ok"] = all(rc == 0 for rc in rcs) and rec["output_sha_ok"] and rec["server_alive"]
    finally:
        stop_server(srv)
        logf.close()
    return rec


def scenario_cancel(root: Path) -> dict:
    fx = FIXDIR / "cancel200m.bin"
    if not fx.exists() or fx.stat().st_size != 200 * 1024 * 1024:
        gen_fixture(fx, 200 * 1024 * 1024)
    port = free_port()
    root.mkdir(parents=True, exist_ok=True)
    srv, logf = start_server(root, port)
    rec = {"scenario": "cancel", "ok": False}
    try:
        assert wait_port(port), "server did not start"
        cli = subprocess.Popen(
            [str(BIN), "send", str(fx), f"127.0.0.1:{port}"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        time.sleep(0.4)  # let it start transferring, then cancel
        cli.kill()
        try:
            cli.wait(timeout=5)
        except subprocess.TimeoutExpired:
            cli.kill()
        # server must still be alive (accept loop not killed) and produce no output
        time.sleep(0.3)
        rec["server_alive"] = srv.poll() is None
        rec["no_partial_output"] = not (root / "outputs" / "cancel200m.bin").exists()
        # server should still accept a follow-up connection
        rec["followup_ok"] = wait_port(port, 3)
        rec["ok"] = rec["server_alive"] and rec["no_partial_output"] and rec["followup_ok"]
    finally:
        stop_server(srv)
        logf.close()
    return rec


def scenario_restart(root: Path) -> dict:
    fx = FIXDIR / "r10m.bin"
    if not fx.exists() or fx.stat().st_size != 10 * 1024 * 1024:
        gen_fixture(fx, 10 * 1024 * 1024)
    src_hash = sha256_file(fx)
    out = root / "outputs" / "r10m.bin"
    rec = {"scenario": "restart", "ok": False}

    # pass 1
    p1 = free_port()
    srv, logf = start_server(root, p1)
    try:
        assert wait_port(p1), "server did not start (pass1)"
        r1 = do_send(fx, p1)
        rec["pass1_rc"] = r1.returncode
        rec["pass1"] = parse_stats(r1.stdout)
    finally:
        stop_server(srv)
        logf.close()

    # pass 2: restart on same root, cache must be warm
    p2 = free_port()
    srv, logf = start_server(root, p2)
    try:
        assert wait_port(p2), "server did not start (pass2)"
        r2 = do_send(fx, p2)
        rec["pass2_rc"] = r2.returncode
        rec["pass2"] = parse_stats(r2.stdout)
        rec["pass2_sha_ok"] = out.exists() and sha256_file(out) == src_hash
        rec["pass2_warm"] = rec["pass2"].get("chunks_transferred", -1) == 0
        rec["ok"] = (r1.returncode == 0 and r2.returncode == 0
                     and rec["pass2_sha_ok"] and rec["pass2_warm"])
    finally:
        stop_server(srv)
        logf.close()
    return rec


def scenario_manyfiles(root: Path) -> dict:
    N = 40
    files = []
    for i in range(N):
        fx = FIXDIR / f"m{i:02d}.bin"
        if not fx.exists() or fx.stat().st_size != 1 * 1024 * 1024:
            gen_fixture(fx, 1 * 1024 * 1024)
        files.append(fx)
    port = free_port()
    root.mkdir(parents=True, exist_ok=True)
    srv, logf = start_server(root, port)
    rec = {"scenario": "manyfiles", "files": N, "ok": False}
    try:
        assert wait_port(port), "server did not start"
        fails = 0
        for fx in files:
            r = do_send(fx, port, timeout=120)
            out = root / "outputs" / fx.name
            if r.returncode != 0 or not (out.exists() and sha256_file(out) == sha256_file(fx)):
                fails += 1
        rec["failures"] = fails
        rec["ok"] = fails == 0
    finally:
        stop_server(srv)
        logf.close()
    return rec


SCENARIOS = {
    "repeated": scenario_repeated,
    "concurrent": scenario_concurrent,
    "same_name": scenario_same_name,
    "cancel": scenario_cancel,
    "restart": scenario_restart,
    "manyfiles": scenario_manyfiles,
}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--only", nargs="*", default=list(SCENARIOS.keys()))
    args = ap.parse_args()

    if not BIN.exists():
        print(f"missing binary {BIN}; run `cargo build --release`", file=sys.stderr)
        return 2

    RESULTS.mkdir(parents=True, exist_ok=True)
    records = []
    for name in args.only:
        if name not in SCENARIOS:
            print(f"unknown scenario {name}", file=sys.stderr)
            continue
        print(f"== {name} ==", flush=True)
        root = DATA / f"stress_{name}"
        shutil.rmtree(root, ignore_errors=True)
        t0 = time.perf_counter()
        rec = SCENARIOS[name](root)
        rec["wall_ms"] = round((time.perf_counter() - t0) * 1000, 1)
        records.append(rec)
        print(json.dumps(rec, indent=2), flush=True)

    print("\nSUMMARY", flush=True)
    for r in records:
        print(f"{r['scenario']:<12} {'PASS' if r['ok'] else 'FAIL':<5} {r['wall_ms']:>10.1f} ms")
    (RESULTS / "stress_summary.json").write_text(json.dumps(records, indent=2))
    print(f"wrote {RESULTS/'stress_summary.json'}", file=sys.stderr)
    return 0 if all(r["ok"] for r in records) else 1


if __name__ == "__main__":
    sys.exit(main())
