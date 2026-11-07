"""CPU-only localhost baseline harness for FASTLINK file sync.

This counts complete bytes crossing a local TCP forwarding proxy, including
handshake, manifest, frames and status. It is not an RPC or inference result.
"""
from __future__ import annotations

import hashlib
import json
import random
import re
import socket
import statistics
import subprocess
import tempfile
import threading
import time
from pathlib import Path
from typing import Any

METRIC_PATTERNS = {
    "file_bytes": r"^\s*file_bytes:\s*(\d+)\s*$",
    "chunks_total": r"^\s*chunks_total:\s*(\d+)\s*$",
    "chunks_reused": r"^\s*chunks_reused:\s*(\d+)\s*$",
    "chunks_transferred": r"^\s*chunks_transferred:\s*(\d+)\s*$",
    "raw_missing_bytes": r"^\s*raw_missing_bytes:\s*(\d+)\s*$",
    "encoded_payload_bytes": r"^\s*encoded_payload_bytes:\s*(\d+)\s*$",
    "elapsed_ms": r"^\s*elapsed_ms:\s*(\d+)\s*$",
}


def parse_sender_summary(text: str) -> dict[str, int]:
    """Parse existing CLI metrics; deliberately does not synthesize wire bytes."""
    result: dict[str, int] = {}
    for key, pattern in METRIC_PATTERNS.items():
        match = re.search(pattern, text, flags=re.MULTILINE)
        if not match:
            raise ValueError(f"missing sender metric: {key}")
        result[key] = int(match.group(1))
    return result


def summarize_cases(cases: list[dict[str, Any]]) -> dict[str, Any]:
    """Aggregate verified scenarios with TCP-stream bytes separate from payload."""
    if not cases:
        raise ValueError("at least one case is required")
    if any(not case.get("ok", False) for case in cases):
        raise ValueError("cannot summarize failed benchmark cases")
    return {
        "case_count": len(cases),
        "tcp_stream_bytes_total": sum(int(case["tcp_stream_bytes"]) for case in cases),
        "payload_bytes_total": sum(int(case["encoded_payload_bytes"]) for case in cases),
        "elapsed_ms_median": statistics.median(int(c["elapsed_ms"]) for c in cases),
        "all_correct": True,
    }


class CountingProxy:
    """One-connection byte-counting forwarder; loopback use only."""

    def __init__(self, upstream: tuple[str, int]):
        self.upstream = upstream
        self.listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(1)
        self.address = self.listener.getsockname()
        self.count = 0
        self._lock = threading.Lock()
        self._threads: list[threading.Thread] = []
        self.error: BaseException | None = None

    def serve_once(self) -> None:
        try:
            client, _ = self.listener.accept()
            server = socket.create_connection(self.upstream, timeout=5)
            self.listener.close()

            def relay(src: socket.socket, dst: socket.socket) -> None:
                try:
                    while True:
                        block = src.recv(65536)
                        if not block:
                            try:
                                dst.shutdown(socket.SHUT_WR)
                            except OSError:
                                pass
                            return
                        with self._lock:
                            self.count += len(block)
                        dst.sendall(block)
                except BaseException as exc:  # propagate relay failure to harness
                    self.error = exc
                    for peer in (src, dst):
                        try:
                            peer.shutdown(socket.SHUT_RDWR)
                        except OSError:
                            pass

            self._threads = [
                threading.Thread(target=relay, args=(client, server), daemon=True),
                threading.Thread(target=relay, args=(server, client), daemon=True),
            ]
            for thread in self._threads:
                thread.start()
            for thread in self._threads:
                thread.join(timeout=30)
            client.close()
            server.close()
            if any(thread.is_alive() for thread in self._threads):
                raise TimeoutError("proxy relay did not close")
            if self.error is not None:
                raise RuntimeError("proxy relay failed") from self.error
        except BaseException as exc:
            self.error = exc
            try:
                self.listener.close()
            except OSError:
                pass


def _run_case(binary: Path, source: Path, proxy: CountingProxy,
              output_dir: Path, case_name: str) -> dict[str, Any]:
    worker = threading.Thread(target=proxy.serve_once, daemon=True)
    worker.start()
    started = time.perf_counter()
    result = subprocess.run(
        [str(binary), "send", str(source), f"127.0.0.1:{proxy.address[1]}"],
        text=True, capture_output=True, timeout=120, check=False,
    )
    worker.join(timeout=35)
    if worker.is_alive():
        raise TimeoutError("byte-counting proxy did not finish")
    if proxy.error is not None:
        raise RuntimeError(f"proxy error during {case_name}") from proxy.error
    if result.returncode:
        raise RuntimeError(f"fastlink send failed in {case_name}: {result.stderr}")
    metrics = parse_sender_summary(result.stdout)
    output = output_dir / source.name
    expected_sha = hashlib.sha256(source.read_bytes()).hexdigest()
    actual_sha = hashlib.sha256(output.read_bytes()).hexdigest()
    return {
        "name": case_name,
        **metrics,
        "tcp_stream_bytes": proxy.count,
        "end_to_end_ms": round((time.perf_counter() - started) * 1000),
        "source_sha256": expected_sha,
        "output_sha256": actual_sha,
        "ok": expected_sha == actual_sha,
    }


def run_matrix(binary: Path, size_mib: int = 1) -> dict[str, Any]:
    """Run cold, warm, one-byte-delta, and sparse loopback cases."""
    if size_mib < 1 or size_mib > 32:
        raise ValueError("size_mib must be between 1 and 32")
    binary = binary.resolve(strict=True)
    if not binary.is_file():
        raise ValueError("binary must be a file")
    size = size_mib * 1024 * 1024
    with tempfile.TemporaryDirectory(prefix="fastlink-wire-bench-") as tmp:
        root = Path(tmp)
        input_dir = root / "input"
        cache_dir = root / "server-cache"
        input_dir.mkdir()
        cache_dir.mkdir()
        source = input_dir / "tensor-fixture.bin"
        source.write_bytes(random.Random(20261107).randbytes(size))
        sparse = input_dir / "sparse-fixture.bin"
        sparse_bytes = bytearray(size)
        sparse_bytes[0] = 1
        sparse_bytes[-1] = 2
        sparse.write_bytes(sparse_bytes)

        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
            probe.bind(("127.0.0.1", 0))
            server_port = probe.getsockname()[1]
        server_log = (root / "server.log").open("w+")
        server = subprocess.Popen(
            [str(binary), "serve", f"127.0.0.1:{server_port}", str(cache_dir)],
            stdout=server_log, stderr=subprocess.STDOUT,
        )
        cases: list[dict[str, Any]] = []
        try:
            deadline = time.monotonic() + 10
            while True:
                if server.poll() is not None:
                    server_log.flush()
                    raise RuntimeError("FASTLINK server exited before ready")
                try:
                    with socket.create_connection(("127.0.0.1", server_port), timeout=0.1):
                        pass
                    break
                except OSError:
                    if time.monotonic() >= deadline:
                        raise TimeoutError("FASTLINK loopback server not ready")
                    time.sleep(0.05)
            # The readiness connection is intentionally empty and should be rejected by the
            # unauthenticated server; it carries no payload and is not included in scenarios.
            for fixture, name in ((source, "random_cold"), (source, "random_warm")):
                proxy = CountingProxy(("127.0.0.1", server_port))
                cases.append(_run_case(binary, fixture, proxy, cache_dir / "outputs", name))
            # Change the already-cached same-name file only after the cold and warm runs.
            changed = bytearray(source.read_bytes())
            changed[size // 2] ^= 1
            source.write_bytes(changed)
            proxy = CountingProxy(("127.0.0.1", server_port))
            cases.append(_run_case(binary, source, proxy, cache_dir / "outputs",
                                   "random_one_byte_delta"))
            proxy = CountingProxy(("127.0.0.1", server_port))
            cases.append(_run_case(binary, sparse, proxy, cache_dir / "outputs",
                                   "sparse_cold"))
        finally:
            server.terminate()
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait(timeout=5)
            server_log.close()
        if not all(case["ok"] for case in cases):
            raise RuntimeError("output checksum mismatch")
        return {
            "classification": "CPU-only loopback FASTLINK file-sync benchmark; not inference",
            "fixture_size_mib": size_mib,
            "cases": cases,
            "summary": summarize_cases([
                {**case, "elapsed_ms": case["end_to_end_ms"]} for case in cases
            ]),
        }


def write_json_output(path: Path, data: dict[str, Any]) -> None:
    """Write a JSON artifact, creating its parent directory if needed."""
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def main() -> int:
    import argparse

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True,
                        help="path to the CPU FASTLINK CLI binary")
    parser.add_argument("--size-mib", type=int, default=1)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    result = run_matrix(args.binary, args.size_mib)
    rendered = json.dumps(result, indent=2, sort_keys=True)
    if args.output:
        write_json_output(args.output, result)
    print(rendered)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
