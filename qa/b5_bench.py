#!/usr/bin/env python3
"""FL3-B5 loopback benchmark: integrate/fastlink-v3-m1 vs main dcaa511.

CPU-only. Binds and connects to 127.0.0.1 only. Does not use a GPU.

Cold/warm/delta for 8 MiB and 64 MiB follow qa/e4_harness.py:
  mixed pattern, seed 42, delta = 1% of bytes at offset 1 MiB.
  Throughput is file MiB / wall seconds around the send process.
  The server is restarted on the same cache between cold, warm, and delta.

The 20x1 MiB loop follows qa/e5_harness.py's file generator and seed 0xE5E5,
with 20 files instead of 50. It runs on the integration build only.

Codec counts are a second encode_chunk pass on the same files, after the
timed send. They count unique BLAKE3 chunk digests. Main's encoder is a
deterministic length check. The integration encoder times CPU, so a second
pass can disagree with the timed encoded_payload_bytes. The timed CLI does
not print per-chunk codec IDs. Warm transfers send no chunks. This is not a
packet capture.

Reference rates are copied from qa/results/e4_latest.md (network-sim e9c96ad).
This script does not re-measure that tree.
"""
from __future__ import annotations

import hashlib
import os
import random
import shutil
import signal
import socket
import subprocess
import sys
import time
from pathlib import Path

INTEGRATE = Path(__file__).resolve().parents[1]
MAIN_SHA = "dcaa5116b74a11d893377c97fba9a74101a16cae"
MAIN_WT = Path("/tmp/fl3b5-main")
CODEC_ROOT = Path("/tmp/fl3b5-codec")
DATA_ROOT = Path("/tmp/fl3b5-data")
RESULTS = INTEGRATE / "qa" / "results" / "b5_integration_bench.md"

# Published FL3-E4 wall rates (qa/results/e4_latest.md). Not re-measured here.
E4_COLD_MIB_S = 73.266439
E4_WARM_MIB_S = 143.221057
E4_COMMIT = "e9c96ad"

SEED = 42
DELTA_FRACTION = 0.01
DELTA_OFFSET = 1024 * 1024
SIZES = (8 * 1024 * 1024, 64 * 1024 * 1024)
CHUNK = 64 * 1024
SUSTAINED_N = 20
SUSTAINED_BYTES = 1024 * 1024
SUSTAINED_SEED = 0xE5E5
CODEC_NAMES = {0: "RAW", 1: "RLE", 2: "LZ4", 3: "ZSTD"}

INT_FIELDS = {
    "file_bytes",
    "chunks_total",
    "chunks_reused",
    "chunks_transferred",
    "raw_missing_bytes",
    "encoded_payload_bytes",
    "handshake_wire_bytes",
    "manifest_wire_bytes",
    "need_wire_bytes",
    "chunk_framing_wire_bytes",
    "status_wire_bytes",
    "headers_and_framing_wire_bytes",
    "wire_bytes_tx",
    "wire_bytes_rx",
    "wire_bytes_total",
    "payload_bytes_avoided_vs_full_file",
    "wire_bytes_avoided_vs_full_file",
    "handshake_us",
    "manifest_us",
    "transfer_us",
    "elapsed_us",
    "elapsed_ms",
}
FLOAT_FIELDS = {"wire_mib_per_s", "logical_mib_per_s"}


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for block in iter(lambda: f.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def make_e4_file(path: Path, size: int, seed: int) -> None:
    """Same mixed generator as qa/e4_harness.py."""
    rng = random.Random(seed)
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("wb") as f:
        remaining = size
        while remaining > 0:
            n = min(remaining, 64 * 1024)
            if rng.random() < 0.5:
                value = rng.randrange(256)
                run = rng.randint(64, min(n, 4096))
                block = (bytes([value]) * run + rng.randbytes(max(0, n - run)))[:n]
            else:
                block = rng.randbytes(n)
            f.write(block)
            remaining -= n


def mutate_delta(src: Path, dst: Path, fraction: float, offset: int, seed: int) -> int:
    data = bytearray(src.read_bytes())
    n = max(1, int(len(data) * fraction))
    start = min(offset, max(0, len(data) - n))
    rng = random.Random(seed ^ 0xDE17A)
    for i in range(start, start + n):
        data[i] = (data[i] + 1 + rng.randrange(255)) & 0xFF
    dst.write_bytes(data)
    return n


def make_e5_file(path: Path, size: int, seed: int) -> None:
    """Same generator as qa/e5_harness.py."""
    rng = random.Random(seed)
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("wb") as f:
        remaining = size
        while remaining:
            n = min(remaining, 65536)
            buf = bytearray(n)
            for i in range(0, n, 256):
                value = rng.randrange(256)
                run = min(256, n - i)
                if rng.random() < 0.4:
                    buf[i : i + run] = bytes([value]) * run
                else:
                    buf[i : i + run] = rng.randbytes(run)
            f.write(buf)
            remaining -= n


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_port(port: int, timeout: float = 15.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return True
        except OSError:
            time.sleep(0.05)
    return False


def start_server(binary: Path, port: int, data_dir: Path, cwd: Path) -> subprocess.Popen:
    data_dir.mkdir(parents=True, exist_ok=True)
    return subprocess.Popen(
        [str(binary), "serve", f"127.0.0.1:{port}", str(data_dir)],
        cwd=cwd,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        start_new_session=True,
    )


def stop_server(proc: subprocess.Popen) -> None:
    try:
        os.killpg(proc.pid, signal.SIGTERM)
    except OSError:
        pass
    try:
        proc.communicate(timeout=5)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except OSError:
            pass
        proc.communicate(timeout=3)


def rss_kb(pid: int) -> int | None:
    try:
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            if line.startswith("VmRSS:"):
                return int(line.split()[1])
    except OSError:
        return None
    return None


def parse_cli(text: str) -> dict:
    fields: dict = {}
    for line in text.splitlines():
        if ":" not in line:
            continue
        key, _, value = line.partition(":")
        key = key.strip().lstrip(" .")
        value = value.strip()
        if key in INT_FIELDS:
            try:
                fields[key] = int(value)
            except ValueError:
                fields[key] = value
        elif key in FLOAT_FIELDS:
            try:
                fields[key] = float(value)
            except ValueError:
                fields[key] = value
    return fields


def run_send(binary: Path, file_path: Path, port: int, cwd: Path, timeout: float) -> dict:
    command = [str(binary), "send", str(file_path), f"127.0.0.1:{port}"]
    started = time.perf_counter()
    cp = subprocess.run(command, cwd=cwd, capture_output=True, text=True, timeout=timeout)
    wall_ms = (time.perf_counter() - started) * 1000.0
    output = (cp.stdout or "") + (("\n" + cp.stderr) if cp.stderr else "")
    fields = parse_cli(output)
    size = fields.get("file_bytes") or file_path.stat().st_size
    mib = size / (1024 * 1024)
    return {
        "command": " ".join(command),
        "ok": cp.returncode == 0,
        "returncode": cp.returncode,
        "wall_ms": round(wall_ms, 3),
        "throughput_mib_s_wall": round(mib / (wall_ms / 1000.0), 6) if wall_ms > 0 else None,
        "fields": fields,
        "output": output.rstrip() + "\n",
    }


def ensure_main_worktree() -> None:
    if MAIN_WT.exists():
        subprocess.run(
            ["git", "worktree", "remove", "--force", str(MAIN_WT)],
            cwd=INTEGRATE,
            check=False,
        )
        shutil.rmtree(MAIN_WT, ignore_errors=True)
    subprocess.run(
        ["git", "worktree", "add", "--detach", str(MAIN_WT), MAIN_SHA],
        cwd=INTEGRATE,
        check=True,
    )


def build_release(cwd: Path) -> Path:
    subprocess.run(["cargo", "build", "--release", "--quiet"], cwd=cwd, check=True)
    binary = cwd / "target" / "release" / "fastlink"
    if not binary.exists():
        raise SystemExit(f"missing binary {binary}")
    return binary


def write_codec_tool(dest: Path, lib_path: Path) -> Path:
    if dest.exists():
        shutil.rmtree(dest)
    dest.mkdir(parents=True)
    (dest / "Cargo.toml").write_text(
        "[package]\nname = \"b5codec\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"
        "publish = false\n\n[dependencies]\n"
        f'fastlink = {{ path = "{lib_path}" }}\n'
    )
    src = dest / "src"
    src.mkdir()
    (src / "main.rs").write_text(
        "fn main() {\n"
        "    let path = std::env::args().nth(1).expect(\"path\");\n"
        "    let data = std::fs::read(&path).expect(\"read\");\n"
        "    let mut off = 0usize;\n"
        "    let mut index = 0u32;\n"
        "    while off < data.len() {\n"
        "        let end = (off + fastlink::CHUNK_BYTES).min(data.len());\n"
        "        let chunk = &data[off..end];\n"
        "        let (codec, encoded) = fastlink::encode_chunk(chunk);\n"
        "        let dig = fastlink::hex(&fastlink::digest(chunk));\n"
        "        println!(\"{index}\\t{codec}\\t{}\\t{dig}\", encoded.len());\n"
        "        off = end;\n"
        "        index += 1;\n"
        "    }\n"
        "}\n"
    )
    subprocess.run(["cargo", "build", "--release", "--quiet"], cwd=dest, check=True)
    return dest / "target" / "release" / "b5codec"


def codec_rows(tool: Path, path: Path) -> list[tuple[int, int, int, str]]:
    cp = subprocess.run([str(tool), str(path)], check=True, capture_output=True, text=True)
    rows = []
    for line in cp.stdout.splitlines():
        index_s, codec_s, length_s, digest = line.split("\t")
        rows.append((int(index_s), int(codec_s), int(length_s), digest))
    return rows


def histogram(rows: list[tuple[int, int, int, str]], digests: set[str] | None) -> dict:
    """Count the first occurrence of each digest. digests=None counts every unique row digest."""
    seen: set[str] = set()
    counts = {name: 0 for name in CODEC_NAMES.values()}
    encoded = 0
    unknown = 0
    for _index, codec, length, digest in rows:
        if digest in seen:
            continue
        if digests is not None and digest not in digests:
            continue
        seen.add(digest)
        name = CODEC_NAMES.get(codec)
        if name is None:
            unknown += 1
        else:
            counts[name] += 1
        encoded += length
    counts["chunks"] = len(seen)
    counts["encoded_bytes"] = encoded
    counts["unknown"] = unknown
    return counts


def scenario(binary: Path, cwd: Path, src: Path, cache: Path, timeout: float) -> dict:
    port = free_port()
    proc = start_server(binary, port, cache, cwd)
    if not wait_port(port):
        stop_server(proc)
        raise SystemExit(f"server did not listen on 127.0.0.1:{port}")
    try:
        result = run_send(binary, src, port, cwd, timeout)
    finally:
        stop_server(proc)
    out = cache / "outputs" / src.name
    result["src_sha256"] = sha256_file(src)
    result["out_sha256"] = sha256_file(out) if out.exists() else None
    result["byte_exact"] = result["out_sha256"] == result["src_sha256"]
    result["output_exists"] = out.exists()
    return result


def percentile(values: list[float], pct: float) -> float:
    ordered = sorted(values)
    if not ordered:
        return 0.0
    index = min(len(ordered) - 1, int(round((pct / 100.0) * (len(ordered) - 1))))
    return ordered[index]


def sustained(binary: Path, cwd: Path, root: Path) -> dict:
    sources = []
    for i in range(SUSTAINED_N):
        path = root / "src" / f"b5-{i:03d}.bin"
        make_e5_file(path, SUSTAINED_BYTES, SUSTAINED_SEED + i)
        sources.append((path, sha256_file(path)))
    port = free_port()
    cache = root / "cache"
    proc = start_server(binary, port, cache, cwd)
    if not wait_port(port):
        stop_server(proc)
        raise SystemExit("sustained server failed to listen")
    passes = {}
    rss_samples = []
    try:
        for phase in ("cold", "warm"):
            rows = []
            for i, (path, digest) in enumerate(sources):
                result = run_send(binary, path, port, cwd, timeout=180)
                out = cache / "outputs" / path.name
                out_digest = sha256_file(out) if out.exists() else None
                result["byte_exact"] = out_digest == digest
                result["idx"] = i
                rss = rss_kb(proc.pid)
                result["server_rss_kb"] = rss
                rss_samples.append({"phase": phase, "i": i, "rss_kb": rss})
                rows.append(result)
            walls = [row["wall_ms"] for row in rows]
            elapsed = [row["fields"].get("elapsed_ms") for row in rows]
            total_s = sum(walls) / 1000.0
            passes[phase] = {
                "rows": rows,
                "n": len(rows),
                "all_ok": all(row["ok"] for row in rows),
                "all_byte_exact": all(row["byte_exact"] for row in rows),
                "all_zero_transferred": all(row["fields"].get("chunks_transferred") == 0 for row in rows),
                "wall_ms_min": min(walls),
                "wall_ms_max": max(walls),
                "wall_ms_mean": round(sum(walls) / len(walls), 3),
                "wall_ms_p50": percentile(walls, 50),
                "client_elapsed_ms": elapsed,
                "aggregate_mib_s_wall": round((SUSTAINED_N * SUSTAINED_BYTES / (1024 * 1024)) / total_s, 6)
                if total_s > 0
                else None,
                "per_run_mib_s_median": round(percentile([1.0 / (w / 1000.0) for w in walls], 50), 6),
            }
    finally:
        stop_server(proc)
    rss_vals = [sample["rss_kb"] for sample in rss_samples if sample["rss_kb"] is not None]
    stable = rss_vals[2:] if len(rss_vals) > 4 else rss_vals
    return {
        "passes": passes,
        "rss_samples": rss_samples,
        "rss_min_kb": min(stable) if stable else None,
        "rss_max_kb": max(stable) if stable else None,
        "rss_delta_kb": (max(stable) - min(stable)) if stable else None,
    }


def fmt_field(fields: dict, key: str) -> str:
    if key not in fields:
        return "not reported"
    return str(fields[key])


def render(report: dict) -> str:
    lines = [
        "# FL3-B5 integration loopback benchmark",
        "",
        "CPU-only. Every server and client address in the commands below is `127.0.0.1`. No GPU was used.",
        "",
        "## Command",
        "",
        "```",
        "python3 qa/b5_bench.py",
        "```",
        "",
        "The script builds `--release` in this worktree and in a detached worktree at",
        f"`/tmp/fl3b5-main` (`{MAIN_SHA}`), runs the transfers, writes this file, and removes that worktree.",
        "",
        "## Host",
        "",
        f"- integrate HEAD: `{report['integrate_sha']}`",
        f"- main checkout: `{report['main_sha']}`",
        f"- started: {report['started']}",
        f"- finished: {report['finished']}",
        f"- uname: {report['uname']}",
        f"- nproc: {report['nproc']}",
        f"- profile: release",
        "",
        "## Method",
        "",
        "- 8 MiB and 64 MiB files use the `qa/e4_harness.py` mixed generator, seed 42.",
        "- Delta mutates 1% of bytes at offset 1048576 with the e4 mutation (`seed ^ 0xDE17A`).",
        "- Cold, warm, and delta each start a new `fastlink serve` on the same cache directory.",
        "- `wall_ms` is `time.perf_counter` around the `fastlink send` process.",
        "- `MiB/s (wall)` is `file_bytes / 1048576 / (wall_ms / 1000)`.",
        "- Client `elapsed_ms`, `logical_mib_per_s`, and `wire_mib_per_s` are copied from CLI stdout when that binary prints them.",
        "- Each sized scenario is one run.",
        "- Main `dcaa511` prints encoded payload bytes and elapsed_ms. It does not print total wire bytes.",
        "- Codec counts are a second `encode_chunk` pass on the same files (unique BLAKE3 chunk digests), not a capture of the timed send.",
        "- Main `encode_chunk` chooses RLE only when the encoded length plus 32 is below the raw length. A matching `encoded_bytes` value is that run's codec mix.",
        "- Integration `encode_chunk` calls `encode_chunk_adaptive`, which times RLE, LZ4, and sometimes ZSTD. A later pass can encode a different byte count. The CLI does not print per-chunk codec IDs.",
        "- Warm sends no chunks.",
        "",
        "## Published e4 reference (not re-measured)",
        "",
        f"From `qa/results/e4_latest.md` on network-sim `{E4_COMMIT}`: one 8 MiB mixed seed-42 loopback release run.",
        f"Cold wall {E4_COLD_MIB_S} MiB/s (109.191 ms, encoded payload 8388608, 128 chunks).",
        f"Warm wall {E4_WARM_MIB_S} MiB/s (55.858 ms, encoded payload 0, 0 chunks).",
        "That run is a different binary (network-sim RAW/RLE) from both trees measured below.",
        "",
        "## Transfer table",
        "",
        "| tree | size | scenario | byte_exact | chunks_transferred | encoded_payload_bytes | wire_bytes_total | wall_ms | client_elapsed_ms | MiB/s wall | logical_mib_per_s | wire_mib_per_s |",
        "|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for row in report["transfers"]:
        fields = row["fields"]
        lines.append(
            "| {tree} | {size} | {scenario} | {byte_exact} | {chunks} | {enc} | {wire} | {wall} | {elapsed} | {mib} | {logical} | {wmib} |".format(
                tree=row["tree"],
                size=row["size_bytes"],
                scenario=row["scenario"],
                byte_exact=row["byte_exact"],
                chunks=fmt_field(fields, "chunks_transferred"),
                enc=fmt_field(fields, "encoded_payload_bytes"),
                wire=fmt_field(fields, "wire_bytes_total"),
                wall=row["wall_ms"],
                elapsed=fmt_field(fields, "elapsed_ms"),
                mib=row["throughput_mib_s_wall"],
                logical=fmt_field(fields, "logical_mib_per_s"),
                wmib=fmt_field(fields, "wire_mib_per_s"),
            )
        )
    lines += [
        "",
        "## Second encode_chunk pass (not a capture of the timed send)",
        "",
        "The timed CLI does not print per-chunk codec IDs.",
        "This table is a later `encode_chunk` pass on the same files. It counts the first occurrence of each BLAKE3 chunk digest.",
        "Warm rows are empty because the timed send transferred 0 chunks.",
        "Main `dcaa511` chooses RLE only when the encoded length plus 32 is below the raw length, with no CPU timer. When `encoded_bytes` equals the timed `encoded_payload_bytes`, that histogram is the timed codec mix.",
        "Integration `encode_chunk` times CPU inside `encode_chunk_adaptive`. A row whose `encoded_bytes` differs from the timed payload is a different sample, not the timed codec mix.",
        "",
        "| tree | size | scenario | RAW | RLE | LZ4 | ZSTD | chunks | encoded_bytes | timed_encoded_payload_bytes | encoded_bytes_minus_timed |",
        "|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|",
    ]
    timed_payload = {
        (row["tree"], row["size_bytes"], row["scenario"]): row["fields"].get("encoded_payload_bytes")
        for row in report["transfers"]
    }
    for row in report["codecs"]:
        counts = row["counts"]
        timed = timed_payload.get((row["tree"], row["size_bytes"], row["scenario"]))
        if timed is None:
            delta = "n/a"
            timed_text = "n/a"
        else:
            timed_text = str(timed)
            delta = str(int(counts["encoded_bytes"]) - int(timed))
        lines.append(
            f"| {row['tree']} | {row['size_bytes']} | {row['scenario']} | {counts['RAW']} | {counts['RLE']} | "
            f"{counts['LZ4']} | {counts['ZSTD']} | {counts['chunks']} | {counts['encoded_bytes']} | "
            f"{timed_text} | {delta} |"
        )
    sus = report["sustained"]
    lines += [
        "",
        "## Sustained 20x1 MiB (integration build only)",
        "",
        "Generator and seed match `qa/e5_harness.py` (`0xE5E5` plus the file index). N is 20, not 50.",
        "One server stays up for the cold pass and the warm pass. Aggregate MiB/s is `20 / sum(wall seconds)`.",
        "",
        "`all_chunks_transferred_zero` is true only when every send in that phase reported `chunks_transferred` 0.",
        "",
        "| phase | all_ok | all_byte_exact | all_chunks_transferred_zero | wall_ms min | mean | p50 | max | aggregate MiB/s wall | per-run MiB/s median |",
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for phase in ("cold", "warm"):
        item = sus["passes"][phase]
        lines.append(
            f"| {phase} | {item['all_ok']} | {item['all_byte_exact']} | {item['all_zero_transferred']} | "
            f"{item['wall_ms_min']} | {item['wall_ms_mean']} | {item['wall_ms_p50']} | {item['wall_ms_max']} | "
            f"{item['aggregate_mib_s_wall']} | {item['per_run_mib_s_median']} |"
        )
    lines += [
        "",
        f"Server VmRSS after dropping the first 2 samples: min {sus['rss_min_kb']} kB, "
        f"max {sus['rss_max_kb']} kB, delta {sus['rss_delta_kb']} kB.",
        "",
        "Per-run wall_ms and client elapsed_ms:",
        "",
        "| phase | idx | wall_ms | elapsed_ms | chunks_transferred | encoded_payload_bytes | wire_bytes_total | byte_exact | rss_kb |",
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for phase in ("cold", "warm"):
        for row in sus["passes"][phase]["rows"]:
            fields = row["fields"]
            lines.append(
                f"| {phase} | {row['idx']} | {row['wall_ms']} | {fmt_field(fields, 'elapsed_ms')} | "
                f"{fmt_field(fields, 'chunks_transferred')} | {fmt_field(fields, 'encoded_payload_bytes')} | "
                f"{fmt_field(fields, 'wire_bytes_total')} | {row['byte_exact']} | {row['server_rss_kb']} |"
            )
    lines += ["", "## Comparison with the published e4 rates and with main dcaa511", ""]
    for label, measured in report["comparisons"]:
        lines.append(f"- {label}: measured {measured}")
    lines += ["", "## Raw CLI stdout", ""]
    for row in report["transfers"]:
        lines.append(f"### {row['tree']} {row['size_bytes']} {row['scenario']}")
        lines.append("")
        lines.append("```")
        lines.append(row["command"])
        lines.append(row["output"].rstrip())
        lines.append("```")
        lines.append("")
    lines.append("Sustained runs use the same `fastlink send FILE 127.0.0.1:PORT` command. ")
    lines.append("Their parsed fields are in the per-run table. One cold sample and one warm sample follow.")
    lines.append("")
    for phase, idx in (("cold", 0), ("warm", 0)):
        row = sus["passes"][phase]["rows"][idx]
        lines.append(f"### sustained {phase} idx {idx}")
        lines.append("")
        lines.append("```")
        lines.append(row["command"])
        lines.append(row["output"].rstrip())
        lines.append("```")
        lines.append("")
    return "\n".join(lines) + "\n"


def main() -> int:
    started = time.strftime("%Y-%m-%dT%H:%M:%S%z")
    integrate_sha = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=INTEGRATE, text=True).strip()
    print(f"[b5] integrate {integrate_sha}", flush=True)
    print("[b5] add detached main worktree", flush=True)
    ensure_main_worktree()
    try:
        main_sha = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=MAIN_WT, text=True).strip()
        if main_sha != MAIN_SHA:
            raise SystemExit(f"main worktree is {main_sha}, expected {MAIN_SHA}")
        print("[b5] release build integrate", flush=True)
        integrate_bin = build_release(INTEGRATE)
        print("[b5] release build main", flush=True)
        main_bin = build_release(MAIN_WT)
        print("[b5] codec tools", flush=True)
        integrate_codec = write_codec_tool(CODEC_ROOT / "integrate", INTEGRATE)
        main_codec = write_codec_tool(CODEC_ROOT / "main", MAIN_WT)
        if DATA_ROOT.exists():
            shutil.rmtree(DATA_ROOT)
        DATA_ROOT.mkdir(parents=True)
        trees = (
            ("integrate", integrate_bin, INTEGRATE, integrate_codec),
            ("main", main_bin, MAIN_WT, main_codec),
        )
        transfers = []
        codecs = []
        for size in SIZES:
            label = DATA_ROOT / f"size-{size}"
            src = label / "data.bin"
            make_e4_file(src, size, SEED)
            delta = label / "data-delta.bin"
            mutate_delta(src, delta, DELTA_FRACTION, DELTA_OFFSET, SEED)
            for tree, binary, cwd, tool in trees:
                cache = label / f"cache-{tree}"
                cold_rows = codec_rows(tool, src)
                delta_rows = codec_rows(tool, delta)
                cold_digests = {row[3] for row in cold_rows}
                delta_new = {row[3] for row in delta_rows} - cold_digests
                print(f"[b5] {tree} {size} cold", flush=True)
                cold = scenario(binary, cwd, src, cache, timeout=600)
                print(f"[b5] {tree} {size} warm", flush=True)
                warm = scenario(binary, cwd, src, cache, timeout=600)
                print(f"[b5] {tree} {size} delta", flush=True)
                delta_run = scenario(binary, cwd, delta, cache, timeout=600)
                for scenario_name, result in (("cold", cold), ("warm", warm), ("delta", delta_run)):
                    transfers.append(
                        {
                            "tree": tree,
                            "size_bytes": size,
                            "scenario": scenario_name,
                            **result,
                        }
                    )
                codecs.append(
                    {
                        "tree": tree,
                        "size_bytes": size,
                        "scenario": "cold",
                        "counts": histogram(cold_rows, None),
                    }
                )
                codecs.append(
                    {
                        "tree": tree,
                        "size_bytes": size,
                        "scenario": "warm",
                        "counts": histogram([], None),
                    }
                )
                codecs.append(
                    {
                        "tree": tree,
                        "size_bytes": size,
                        "scenario": "delta",
                        "counts": histogram(delta_rows, delta_new),
                    }
                )
        print("[b5] sustained 20x1MiB", flush=True)
        sustained_report = sustained(integrate_bin, INTEGRATE, DATA_ROOT / "sustained")
        comparisons = []
        for row in transfers:
            if row["tree"] == "integrate" and row["size_bytes"] == 8 * 1024 * 1024 and row["scenario"] == "cold":
                comparisons.append(
                    (
                        "integrate 8 MiB cold wall MiB/s vs e4 cold 73.266439",
                        f"{row['throughput_mib_s_wall']} (difference {round(row['throughput_mib_s_wall'] - E4_COLD_MIB_S, 6)})",
                    )
                )
            if row["tree"] == "integrate" and row["size_bytes"] == 8 * 1024 * 1024 and row["scenario"] == "warm":
                comparisons.append(
                    (
                        "integrate 8 MiB warm wall MiB/s vs e4 warm 143.221057",
                        f"{row['throughput_mib_s_wall']} (difference {round(row['throughput_mib_s_wall'] - E4_WARM_MIB_S, 6)})",
                    )
                )
        by_key = {(row["tree"], row["size_bytes"], row["scenario"]): row for row in transfers}
        for size, label in ((8 * 1024 * 1024, "8 MiB"), (64 * 1024 * 1024, "64 MiB")):
            for scenario_name in ("cold", "warm", "delta"):
                integ = by_key[("integrate", size, scenario_name)]["throughput_mib_s_wall"]
                main_rate = by_key[("main", size, scenario_name)]["throughput_mib_s_wall"]
                comparisons.append(
                    (
                        f"integrate {label} {scenario_name} wall MiB/s vs main dcaa511",
                        f"{integ} (main {main_rate}, difference {round(integ - main_rate, 6)})",
                    )
                )
        cold_agg = sustained_report["passes"]["cold"]["aggregate_mib_s_wall"]
        warm_agg = sustained_report["passes"]["warm"]["aggregate_mib_s_wall"]
        comparisons.append(
            (
                "integrate 20x1 MiB cold aggregate wall MiB/s vs e4 single 8 MiB cold 73.266439 (different workload)",
                f"{cold_agg} (difference {round(cold_agg - E4_COLD_MIB_S, 6)})",
            )
        )
        comparisons.append(
            (
                "integrate 20x1 MiB warm aggregate wall MiB/s vs e4 single 8 MiB warm 143.221057 (different workload)",
                f"{warm_agg} (difference {round(warm_agg - E4_WARM_MIB_S, 6)})",
            )
        )
        finished = time.strftime("%Y-%m-%dT%H:%M:%S%z")
        uname = subprocess.check_output(["uname", "-srvm"], text=True).strip()
        nproc = subprocess.check_output(["nproc"], text=True).strip()
        report = {
            "integrate_sha": integrate_sha,
            "main_sha": main_sha,
            "started": started,
            "finished": finished,
            "uname": uname,
            "nproc": nproc,
            "transfers": transfers,
            "codecs": codecs,
            "sustained": sustained_report,
            "comparisons": comparisons,
        }
        RESULTS.parent.mkdir(parents=True, exist_ok=True)
        RESULTS.write_text(render(report))
        print(f"[b5] wrote {RESULTS}", flush=True)
        failed = [row for row in transfers if not row["ok"] or not row["byte_exact"]]
        sus_fail = any(
            not sustained_report["passes"][phase]["all_ok"]
            or not sustained_report["passes"][phase]["all_byte_exact"]
            for phase in ("cold", "warm")
        )
        if failed or sus_fail or not sustained_report["passes"]["warm"]["all_zero_transferred"]:
            print(f"[b5] failures: {len(failed)} transfer rows, sustained_fail={sus_fail}", flush=True)
            return 2
        return 0
    finally:
        subprocess.run(
            ["git", "worktree", "remove", "--force", str(MAIN_WT)],
            cwd=INTEGRATE,
            check=False,
        )
        shutil.rmtree(CODEC_ROOT, ignore_errors=True)
        shutil.rmtree(DATA_ROOT, ignore_errors=True)
        shutil.rmtree(MAIN_WT, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
