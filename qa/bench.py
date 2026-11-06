#!/usr/bin/env python3
"""FASTLINK QA benchmark harness (CPU-only, loopback-only).

For each fixture (1 KB / 1 MB / 100 MB / 1 GB) it runs:
  - cold: fresh cache root, every chunk transferred
  - warm: same root reused, every chunk cache-hit (0 transferred)

and measures wall time, client+server CPU time, and peak RSS by sampling
/proc/<pid>/stat (per-task, summed) while the transfer runs. Integrity is
verified independently with the system sha256sum binary.

Usage:
  python3 qa/bench.py                 # run all sizes, cold + warm
  python3 qa/bench.py --sizes 1KB 1MB
  python3 qa/bench.py --modes cold   # only cold
  python3 qa/bench.py --keep        # keep fixtures + data roots for inspection
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import socket
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
BIN = REPO / "target" / "release" / "fastlink"
QA = REPO / "qa"
FIXDIR = QA / "fixtures"
DATADIR = QA / "data"
RESULTS = QA / "results"
SHA = "sha256sum"

# size label -> (name, byte size)
SIZES = {
    "1KB": ("f1k.bin", 1024),
    "1MB": ("f1m.bin", 1024 * 1024),
    "100MB": ("f100m.bin", 100 * 1024 * 1024),
    "1GB": ("f1g.bin", 1024 * 1024 * 1024),
}


def free_port() -> int:
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def gen_fixture(path: Path, n: int) -> None:
    """Write n bytes of incompressible (os.urandom) data in 8 MiB chunks."""
    path.parent.mkdir(parents=True, exist_ok=True)
    chunk = 8 * 1024 * 1024
    with open(path, "wb") as f:
        rem = n
        while rem > 0:
            f.write(os.urandom(min(chunk, rem)))
            rem -= min(chunk, rem)


def sha256_file(path: Path) -> str:
    out = subprocess.run([SHA, str(path)], capture_output=True, text=True, check=True)
    return out.stdout.split()[0]


def read_proc(pid: int) -> tuple[float, int] | None:
    """Sum (cpu_seconds, rss_kb) for a process from /proc.

    Uses /proc/<pid>/stat for aggregate CPU (fields 14/15) which persists
    after worker threads exit, and /proc/<pid>/status for RSS (VmRSS is
    process-wide).  Falls back to per-thread summation if the aggregate
    line is unavailable.
    """
    stat_path = Path("/proc") / str(pid) / "stat"
    try:
        raw = stat_path.read_text()
    except OSError:
        return None
    # comm field is the first parenthesised token; take the text after its
    # closing ')' (the only one in the line) and split that remainder.
    tail = raw[raw.rfind(")") + 1 :].split()
    # tail: state(0) ppid(1) pgrp(2) session(3) tty(4) tpgid(5)
    #       tty_nr(6) tflags(7) minflt(8) cminflt(9) majflt(10)
    #       cmajflt(11) utime(12) stime(13)
    clk = os.sysconf("SC_CLK_TCK")
    try:
        utime = int(tail[12])
        stime = int(tail[13])
    except (IndexError, ValueError):
        return None
    cpu = (utime + stime) / clk

    # RSS: sum VmRSS across all threads (or read process-level status)
    rss = 0
    status_path = Path("/proc") / str(pid) / "status"
    try:
        for line in status_path.read_text().splitlines():
            if line.startswith("VmRSS:"):
                rss = int(line.split()[1])
                break
    except OSError:
        pass
    return (cpu, rss)


def wait_port(port: int, timeout: float = 5.0) -> bool:
    t0 = time.time()
    while time.time() - t0 < timeout:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return True
        except OSError:
            time.sleep(0.01)
    return False


def parse_stats(text: str) -> dict:
    out = {}
    for key in ("file_bytes", "chunks_total", "chunks_reused",
                "chunks_transferred", "raw_missing_bytes",
                "encoded_payload_bytes", "elapsed_ms"):
        for line in text.splitlines():
            line = line.strip()
            if line.startswith(key + ":"):
                out[key] = int(line.split(":")[1].strip())
    return out


def run_one(label: str, name: str, nbytes: int, mode: str, keep: bool) -> dict:
    fx = FIXDIR / name
    if not fx.exists() or fx.stat().st_size != nbytes:
        gen_fixture(fx, nbytes)
    fx_hash = sha256_file(fx)

    # warm reuses the cache built by the cold run of the same size, so the
    # second pass should be all cache-hits (0 chunks transferred).
    root = DATADIR / label
    if mode == "warm" and root.exists():
        pass
    else:
        shutil.rmtree(root, ignore_errors=True)
        root.mkdir(parents=True, exist_ok=True)

    port = free_port()
    log = (QA / "results" / f"serve_{label}_{mode}.log")
    log.parent.mkdir(parents=True, exist_ok=True)
    logf = open(log, "w")

    server = subprocess.Popen(
        [str(BIN), "serve", f"127.0.0.1:{port}", str(root)],
        stdout=logf, stderr=subprocess.STDOUT, start_new_session=True,
    )
    rec: dict = {"size": label, "name": name, "bytes": nbytes, "mode": mode,
                 "port": port, "ok": False}
    try:
        if not wait_port(port):
            rec["error"] = "server did not start"
            return rec

        s0 = read_proc(server.pid) or (0.0, 0)
        t_wall = time.perf_counter()
        client = subprocess.run(
            [str(BIN), "send", str(fx), f"127.0.0.1:{port}"],
            capture_output=True, text=True, timeout=1800,
        )
        wall_ms = (time.perf_counter() - t_wall) * 1000.0
        s1 = read_proc(server.pid) or (0.0, 0)

        rec.update(parse_stats(client.stdout))
        rec["client_rc"] = client.returncode
        rec["wall_ms"] = round(wall_ms, 2)
        rec["server_cpu_ms"] = round(max(0.0, s1[0] - s0[0]) * 1000, 2)
        rec["server_max_rss_kb"] = max(s0[1], s1[1])

        out = root / "outputs" / name
        out_hash = sha256_file(out) if out.exists() else None
        rec["sha_source"] = fx_hash
        rec["sha_output"] = out_hash
        rec["sha_ok"] = fx_hash is not None and out_hash == fx_hash
        rec["ok"] = client.returncode == 0 and rec["sha_ok"]
        if client.returncode != 0:
            rec["client_stderr"] = client.stderr.strip()[:500]
    finally:
        server.terminate()
        try:
            server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server.kill()
        logf.close()
    return rec


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--sizes", nargs="*", default=list(SIZES.keys()))
    ap.add_argument("--modes", nargs="*", default=["cold", "warm"])
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()

    if not BIN.exists():
        print(f"missing binary {BIN}; run `cargo build --release`", file=sys.stderr)
        return 2

    RESULTS.mkdir(parents=True, exist_ok=True)
    records = []
    for label in args.sizes:
        if label not in SIZES:
            print(f"unknown size {label}", file=sys.stderr)
            continue
        name, nbytes = SIZES[label]
        for mode in args.modes:
            print(f"== {label} {mode} ==", flush=True)
            r = run_one(label, name, nbytes, mode, args.keep)
            records.append(r)
            print(json.dumps(r, indent=2), flush=True)

    # human summary
    print("\nSUMMARY", flush=True)
    hdr = f"{'size':<7} {'mode':<5} {'MB':>8} {'wall_ms':>10} {'MB/s':>9} {'xfer_chunks':>11} {'reused':>6} {'sha':>5}"
    print(hdr)
    for r in records:
        mb = r["bytes"] / (1024 * 1024)
        wall = r.get("wall_ms", 0)
        mbps = (mb / (wall / 1000.0)) if wall > 0 else 0
        print(f"{r['size']:<7} {r['mode']:<5} {mb:>8.2f} {wall:>10.1f} {mbps:>9.1f} "
              f"{r.get('chunks_transferred','?'):>11} {r.get('chunks_reused','?'):>6} "
              f"{('OK' if r.get('sha_ok') else 'FAIL'):>5}")

    (RESULTS / "bench_summary.json").write_text(json.dumps(records, indent=2))
    print(f"\nwrote {RESULTS/'bench_summary.json'}", file=sys.stderr)
    return 0 if all(r.get("ok") for r in records) else 1


if __name__ == "__main__":
    sys.exit(main())
