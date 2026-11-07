#!/usr/bin/env python3
"""FASTLINK fault harness (CPU-only, loopback-only).

Drives the `fastlink serve` binary on 127.0.0.1 and sends broken v0.1
sessions. Every scenario must fail safe:

- the server stays up and logs `session error` (no panic)
- it never returns status byte 0
- pre-seeded output bytes are unchanged
- no `.fastlink-inprogress-*` or `.partial-*` file is left behind

The CLI accepts the next connection only after the current session returns,
so concurrent clients queue on one listener and are still isolated: a fault
in one session must not commit that session's output.

Re-runnable: each scenario uses a fresh temporary data root, which is removed
before the process exits. Results are rewritten to qa/results/faults_summary.json.

Usage:
  python3 qa/faults.py
  python3 qa/faults.py --only bad_digest hash_collision
  python3 qa/faults.py --bin target/debug/fastlink
"""

from __future__ import annotations

import argparse
import ctypes
import json
import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
RESULTS = REPO / "qa" / "results"
CHUNK = 65536
MAX_ENCODED = CHUNK * 2
CMD_SYNC = 1
STATUS_OK = 0
HANDSHAKE = b"FLNK" + struct.pack("<H", 1)
PRIOR = b"PRIOR"

# blake3_hasher is 1912 bytes in libblake3 1.8 (BLAKE3_MAX_DEPTH 54).
_HASHER_BYTES = 1912


class _Hasher(ctypes.Structure):
    _fields_ = [("_storage", ctypes.c_ubyte * _HASHER_BYTES)]


_LIB = ctypes.CDLL("libblake3.so")
_LIB.blake3_hasher_init.argtypes = [ctypes.POINTER(_Hasher)]
_LIB.blake3_hasher_update.argtypes = [ctypes.POINTER(_Hasher), ctypes.c_void_p, ctypes.c_size_t]
_LIB.blake3_hasher_finalize.argtypes = [ctypes.POINTER(_Hasher), ctypes.c_void_p, ctypes.c_size_t]


def blake3(data: bytes) -> bytes:
    hasher = _Hasher()
    _LIB.blake3_hasher_init(ctypes.byref(hasher))
    if data:
        buf = ctypes.create_string_buffer(data)
        _LIB.blake3_hasher_update(ctypes.byref(hasher), buf, len(data))
    out = ctypes.create_string_buffer(32)
    _LIB.blake3_hasher_finalize(ctypes.byref(hasher), out, 32)
    return out.raw


def self_check_blake3() -> None:
    empty = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
    abc = "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
    if blake3(b"").hex() != empty or blake3(b"abc").hex() != abc:
        raise SystemExit("libblake3 self-check failed")


def free_port() -> int:
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    return port


def find_bin(explicit: str | None) -> Path:
    if explicit:
        path = Path(explicit)
        if not path.is_file():
            raise SystemExit(f"binary not found: {path}")
        return path
    candidates = [REPO / "target" / "debug" / "fastlink", REPO / "target" / "release" / "fastlink"]
    existing = [p for p in candidates if p.is_file() and os.access(p, os.X_OK)]
    if not existing:
        raise SystemExit("fastlink binary not found; run cargo build in this worktree")
    return max(existing, key=lambda p: p.stat().st_mtime)


def read_exact(sock: socket.socket, n: int) -> bytes:
    buf = b""
    while len(buf) < n:
        try:
            chunk = sock.recv(n - len(buf))
        except (ConnectionResetError, BrokenPipeError) as exc:
            raise EOFError("connection reset") from exc
        if not chunk:
            raise EOFError(f"short read {len(buf)}/{n}")
        buf += chunk
    return buf


def read_until_close(sock: socket.socket, limit: int = 256) -> bytes:
    data = b""
    while len(data) < limit:
        try:
            chunk = sock.recv(limit - len(data))
        except (ConnectionResetError, BrokenPipeError):
            break
        except socket.timeout as exc:
            raise AssertionError("timed out waiting for the server to close") from exc
        if not chunk:
            break
        data += chunk
    if data == bytes([STATUS_OK]):
        raise AssertionError("server returned STATUS_OK")
    return data


def send_all(sock: socket.socket, data: bytes) -> None:
    try:
        sock.sendall(data)
    except (BrokenPipeError, ConnectionResetError):
        return


class Session:
    def __init__(self, port: int, timeout: float = 8.0):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout)
        self.sock.settimeout(timeout)

    def close(self) -> None:
        try:
            self.sock.close()
        except OSError:
            pass

    def handshake(self) -> None:
        send_all(self.sock, HANDSHAKE)
        reply = read_exact(self.sock, 6)
        if reply != HANDSHAKE:
            raise AssertionError(f"unexpected handshake reply {reply!r}")

    def reject(self) -> bytes:
        return read_until_close(self.sock)


def manifest(name: str, size: int, hashes: list[bytes]) -> bytes:
    raw = name.encode("utf-8")
    buf = struct.pack("<H", len(raw)) + raw
    buf += struct.pack("<Q", size)
    buf += struct.pack("<I", CHUNK)
    buf += struct.pack("<I", len(hashes))
    for digest in hashes:
        buf += digest
    return bytes([CMD_SYNC]) + buf


def chunk_frame(index: int, codec: int, payload: bytes) -> bytes:
    return struct.pack("<I", index) + bytes([codec]) + struct.pack("<I", len(payload)) + payload


def read_missing(sock: socket.socket) -> list[int]:
    count = struct.unpack("<I", read_exact(sock, 4))[0]
    if count > 8:
        raise AssertionError(f"server requested {count} chunks")
    return [struct.unpack("<I", read_exact(sock, 4))[0] for _ in range(count)]


def preseed(root: Path, names: list[str]) -> dict[str, bytes]:
    out = root / "outputs"
    out.mkdir(parents=True, exist_ok=True)
    (root / "chunks").mkdir(parents=True, exist_ok=True)
    for name in names:
        (out / name).write_bytes(PRIOR)
    return {name: PRIOR for name in names}


def assert_outputs_unchanged(root: Path, prior: dict[str, bytes]) -> None:
    found: dict[str, bytes] = {}
    in_progress: list[str] = []
    out = root / "outputs"
    if out.exists():
        for path in out.iterdir():
            if path.name.startswith(".fastlink-inprogress-"):
                in_progress.append(path.name)
            elif path.is_file():
                found[path.name] = path.read_bytes()
    if in_progress:
        raise AssertionError(f"in-progress output left behind: {in_progress}")
    if found != prior:
        raise AssertionError(
            "output set changed: "
            + ", ".join(sorted(set(found) | set(prior)))
        )
    partials: list[str] = []
    chunks = root / "chunks"
    if chunks.exists():
        for dirpath, _dirs, files in os.walk(chunks):
            for name in files:
                if name.startswith(".partial-"):
                    partials.append(str(Path(dirpath) / name))
    if partials:
        raise AssertionError(f"partial chunk left behind: {partials}")


def plant_cache(root: Path, digest: bytes, contents: bytes) -> None:
    hex_digest = digest.hex()
    path = root / "chunks" / hex_digest[:2] / hex_digest
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(contents)


def barrier(port: int) -> None:
    """One rejected handshake, so the serial accept loop has finished the previous session."""
    sess = Session(port)
    try:
        send_all(sess.sock, b"XXXX" + struct.pack("<H", 1))
        sess.reject()
    finally:
        sess.close()


class Server:
    def __init__(self, root: Path, bin_path: Path):
        self.root = root
        self.port = free_port()
        self.log_path = root / "server.log"
        self.logf = open(self.log_path, "w", encoding="utf-8", errors="replace")
        self.proc = subprocess.Popen(
            [str(bin_path), "serve", f"127.0.0.1:{self.port}", str(root)],
            stdout=self.logf,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        if not self._wait_port():
            tail = self.log_text()
            self.stop()
            raise RuntimeError(f"server did not bind 127.0.0.1:{self.port}\n{tail}")

    def _wait_port(self, timeout: float = 5.0) -> bool:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                return False
            try:
                sock = socket.create_connection(("127.0.0.1", self.port), 0.2)
            except OSError:
                time.sleep(0.02)
                continue
            sock.close()
            return True
        return False

    def alive(self) -> bool:
        return self.proc.poll() is None

    def log_text(self) -> str:
        self.logf.flush()
        try:
            return self.log_path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            return ""

    def stop(self) -> None:
        if self.proc.poll() is None:
            try:
                os.killpg(self.proc.pid, 15)
            except ProcessLookupError:
                pass
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(self.proc.pid, 9)
                except ProcessLookupError:
                    pass
                self.proc.wait(timeout=3)
        self.logf.close()


def open_sync(port: int, name: str, size: int, hashes: list[bytes]) -> tuple[Session, list[int]]:
    sess = Session(port)
    sess.handshake()
    send_all(sess.sock, manifest(name, size, hashes))
    missing = read_missing(sess.sock)
    return sess, missing


def scenario_malformed_frame(server: Server, root: Path) -> None:
    name = "malformed.bin"
    prior = preseed(root, [name])
    sess = Session(server.port)
    try:
        sess.handshake()
        raw = name.encode()
        body = struct.pack("<H", len(raw)) + raw
        body += struct.pack("<Q", 16)
        body += struct.pack("<I", 4096)  # not the required 65536
        body += struct.pack("<I", 1)
        body += b"\x00" * 32
        send_all(sess.sock, bytes([CMD_SYNC]) + body)
        sess.reject()
    finally:
        sess.close()
    barrier(server.port)
    assert_outputs_unchanged(root, prior)


def scenario_truncated_stream(server: Server, root: Path) -> None:
    name = "truncated.bin"
    prior = preseed(root, [name])
    sess = Session(server.port)
    try:
        sess.handshake()
        # Claim a 12-byte name and close before those bytes arrive.
        send_all(sess.sock, bytes([CMD_SYNC]) + struct.pack("<H", 12))
    finally:
        sess.close()
    barrier(server.port)
    assert_outputs_unchanged(root, prior)


def scenario_invalid_codec(server: Server, root: Path) -> None:
    name = "badcodec.bin"
    payload = b"\x11" * 16
    prior = preseed(root, [name])
    sess, missing = open_sync(server.port, name, len(payload), [blake3(payload)])
    try:
        if missing != [0]:
            raise AssertionError(f"expected to be asked for chunk 0, got {missing}")
        send_all(sess.sock, chunk_frame(0, 0x2F, payload))
        sess.reject()
    finally:
        sess.close()
    barrier(server.port)
    assert_outputs_unchanged(root, prior)


def scenario_bad_digest(server: Server, root: Path) -> None:
    name = "baddigest.bin"
    claimed = b"\x22" * 16
    sent = b"\x33" * 16
    prior = preseed(root, [name])
    sess, missing = open_sync(server.port, name, len(claimed), [blake3(claimed)])
    try:
        if missing != [0]:
            raise AssertionError(f"expected to be asked for chunk 0, got {missing}")
        send_all(sess.sock, chunk_frame(0, 0, sent))
        sess.reject()
    finally:
        sess.close()
    barrier(server.port)
    assert_outputs_unchanged(root, prior)


def scenario_hash_collision(server: Server, root: Path) -> None:
    """A cache file named for digest(real) but holding other bytes must not be reused."""
    name = "collision.bin"
    real = b"\xAB" * 16
    planted = b"\xCD" * 16
    prior = preseed(root, [name])
    claimed = blake3(real)
    plant_cache(root, claimed, planted)
    sess, missing = open_sync(server.port, name, len(real), [claimed])
    try:
        if missing != [0]:
            raise AssertionError(
                f"planted cache object was trusted or not requested: missing={missing}"
            )
        send_all(sess.sock, chunk_frame(0, 0, planted))
        sess.reject()
    finally:
        sess.close()
    barrier(server.port)
    assert_outputs_unchanged(root, prior)


def scenario_interrupted_session(server: Server, root: Path) -> None:
    name = "interrupted.bin"
    first = bytes([0x11]) * CHUNK
    second = bytes([0x22]) * 4
    size = len(first) + len(second)
    prior = preseed(root, [name])
    sess, missing = open_sync(server.port, name, size, [blake3(first), blake3(second)])
    try:
        if missing != [0, 1]:
            raise AssertionError(f"expected both chunks, got {missing}")
        send_all(sess.sock, chunk_frame(0, 0, first))
        partial = chunk_frame(1, 0, second)
        send_all(sess.sock, partial[: 8 + len(second) // 2])
    finally:
        sess.close()
    barrier(server.port)
    assert_outputs_unchanged(root, prior)


def scenario_concurrent_clients(server: Server, root: Path) -> None:
    names = [f"c{i}.bin" for i in range(4)]
    prior = preseed(root, names)
    errors: list[str] = []

    def worker(index: int) -> None:
        try:
            if index == 0:
                _malformed_body(server.port, names[index])
            elif index == 1:
                _bad_digest_body(server.port, names[index])
            elif index == 2:
                _oversized_name_body(server.port)
            else:
                _invalid_codec_body(server.port, names[index])
        except Exception as exc:
            errors.append(f"client {index}: {exc}")

    threads = [threading.Thread(target=worker, args=(i,), daemon=True) for i in range(4)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(timeout=30)
    if any(thread.is_alive() for thread in threads):
        raise AssertionError("a concurrent client is still blocked")
    if errors:
        raise AssertionError("; ".join(errors))
    barrier(server.port)
    assert_outputs_unchanged(root, prior)


def scenario_oversized_name(server: Server, root: Path) -> None:
    prior = preseed(root, ["kept.bin"])
    sess = Session(server.port)
    try:
        sess.handshake()
        _oversized_name_body_on(sess)
        sess.reject()
    finally:
        sess.close()
    barrier(server.port)
    assert_outputs_unchanged(root, prior)


def scenario_oversized_chunk(server: Server, root: Path) -> None:
    name = "oversize.bin"
    payload = b"\x44" * 16
    prior = preseed(root, [name])
    sess, missing = open_sync(server.port, name, len(payload), [blake3(payload)])
    try:
        if missing != [0]:
            raise AssertionError(f"expected to be asked for chunk 0, got {missing}")
        header = struct.pack("<I", 0) + bytes([0]) + struct.pack("<I", MAX_ENCODED + 1)
        send_all(sess.sock, header)
        sess.reject()
    finally:
        sess.close()
    barrier(server.port)
    assert_outputs_unchanged(root, prior)


def scenario_resume_failure(server: Server, root: Path) -> None:
    """There is no resume opcode. A dropped session must not be continuable."""
    name = "resume.bin"
    first = bytes([0x55]) * CHUNK
    second = bytes([0x66]) * 4
    size = len(first) + len(second)
    prior = preseed(root, [name])
    sess, missing = open_sync(server.port, name, size, [blake3(first), blake3(second)])
    try:
        if missing != [0, 1]:
            raise AssertionError(f"expected both chunks, got {missing}")
        send_all(sess.sock, chunk_frame(0, 0, first))
    finally:
        sess.close()
    follow = Session(server.port)
    try:
        follow.handshake()
        send_all(follow.sock, bytes([2]) + chunk_frame(1, 0, second))
        follow.reject()
    finally:
        follow.close()
    barrier(server.port)
    assert_outputs_unchanged(root, prior)


def _malformed_body(port: int, name: str) -> None:
    sess = Session(port)
    try:
        sess.handshake()
        raw = name.encode()
        body = struct.pack("<H", len(raw)) + raw
        body += struct.pack("<Q", 16) + struct.pack("<I", 1) + struct.pack("<I", 1)
        send_all(sess.sock, bytes([CMD_SYNC]) + body)
        sess.reject()
    finally:
        sess.close()


def _bad_digest_body(port: int, name: str) -> None:
    claimed = b"\x01" * 8
    sess, missing = open_sync(port, name, len(claimed), [blake3(claimed)])
    try:
        if missing != [0]:
            raise AssertionError(f"missing={missing}")
        send_all(sess.sock, chunk_frame(0, 0, b"\x02" * 8))
        sess.reject()
    finally:
        sess.close()


def _invalid_codec_body(port: int, name: str) -> None:
    payload = b"\x03" * 8
    sess, missing = open_sync(port, name, len(payload), [blake3(payload)])
    try:
        if missing != [0]:
            raise AssertionError(f"missing={missing}")
        send_all(sess.sock, chunk_frame(0, 9, payload))
        sess.reject()
    finally:
        sess.close()


def _oversized_name_body(port: int) -> None:
    sess = Session(port)
    try:
        sess.handshake()
        _oversized_name_body_on(sess)
        sess.reject()
    finally:
        sess.close()


def _oversized_name_body_on(sess: Session) -> None:
    send_all(sess.sock, bytes([CMD_SYNC]) + struct.pack("<H", 241) + b"a" * 241)


SCENARIOS = [
    ("malformed_frame", scenario_malformed_frame),
    ("truncated_stream", scenario_truncated_stream),
    ("invalid_codec", scenario_invalid_codec),
    ("bad_digest", scenario_bad_digest),
    ("hash_collision", scenario_hash_collision),
    ("interrupted_session", scenario_interrupted_session),
    ("concurrent_clients", scenario_concurrent_clients),
    ("oversized_name", scenario_oversized_name),
    ("oversized_chunk", scenario_oversized_chunk),
    ("resume_failure", scenario_resume_failure),
]


def run_one(name: str, fn, bin_path: Path) -> dict:
    root = Path(tempfile.mkdtemp(prefix=f"fastlink-fault-{name}-"))
    server: Server | None = None
    started = time.monotonic()
    try:
        server = Server(root, bin_path)
        fn(server, root)
        if not server.alive():
            raise AssertionError(f"server exited {server.proc.returncode}")
        log = server.log_text()
        if "panicked at" in log or "stack backtrace" in log:
            raise AssertionError("server panicked")
        if "session error" not in log:
            raise AssertionError("server log has no session error")
        return {
            "name": name,
            "ok": True,
            "seconds": round(time.monotonic() - started, 3),
            "detail": "rejected, prior output unchanged, server alive",
        }
    except Exception as exc:
        tail = ""
        if server is not None:
            lines = server.log_text().splitlines()
            tail = " | ".join(lines[-8:])
        return {
            "name": name,
            "ok": False,
            "seconds": round(time.monotonic() - started, 3),
            "detail": f"{exc}",
            "log_tail": tail,
        }
    finally:
        if server is not None:
            server.stop()
        shutil.rmtree(root, ignore_errors=True)


def main() -> int:
    parser = argparse.ArgumentParser(description="FASTLINK loopback fault harness")
    parser.add_argument("--only", nargs="*", help="scenario names to run")
    parser.add_argument("--bin", help="path to the fastlink binary")
    args = parser.parse_args()
    self_check_blake3()
    bin_path = find_bin(args.bin).resolve()
    selected = SCENARIOS
    if args.only:
        wanted = set(args.only)
        selected = [item for item in SCENARIOS if item[0] in wanted]
        missing = wanted - {name for name, _fn in selected}
        if missing:
            raise SystemExit(f"unknown scenario(s): {sorted(missing)}")
    if len(SCENARIOS) < 10:
        raise SystemExit("harness defines fewer than 10 scenarios")
    results = [run_one(name, fn, bin_path) for name, fn in selected]
    passed = sum(1 for item in results if item["ok"])
    summary = {
        "binary": str(bin_path.relative_to(REPO)) if bin_path.is_relative_to(REPO) else bin_path.name,
        "listen": "127.0.0.1",
        "cpu_only": True,
        "scenario_count": len(results),
        "passed": passed,
        "failed": len(results) - passed,
        "scenarios": results,
    }
    RESULTS.mkdir(parents=True, exist_ok=True)
    out = RESULTS / "faults_summary.json"
    out.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
    for item in results:
        mark = "PASS" if item["ok"] else "FAIL"
        print(f"{mark} {item['name']} {item['seconds']}s {item['detail']}")
    print(f"{passed}/{len(results)} passed; wrote {out}")
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
