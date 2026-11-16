"""Mocked parser-contract tests for the mmap benchmark collector.

All stdout fixtures are synthetic parser fixtures. They are never presented as
measured results, and subprocess.run is always mocked; no cargo, GPU, network,
or inference workload is started.
"""

import subprocess
import sys
import unittest
from pathlib import Path
from unittest import mock

REPO_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO_ROOT))

from benches import distributed_gpu_bench as collector  # noqa: E402

COMMAND = ["cargo", "bench", "--bench", "mmap_transfer"]

SYNTHETIC_STDOUT = "\n".join([
    "single-chunk RTT (mmap shm): payload=65536 B rounds=500 "
    "min=0.101 ms p50=0.102 ms p95=0.103 ms p99=0.104 ms mean=0.105 ms",
    "single-chunk RTT (Raw TCP) : payload=65536 B rounds=500 "
    "min=0.201 ms p50=0.202 ms p95=0.203 ms p99=0.204 ms mean=0.205 ms",
    "single-chunk RTT (FASTLINK): payload=65536 B rounds=500 "
    "min=0.301 ms p50=0.302 ms p95=0.303 ms p99=0.304 ms mean=0.305 ms",
    "mmap shm transfer (patterned) : rounds=32 elapsed=1.0 ms "
    "throughput=1111.1 MB/s bytes_per_tensor=8388608",
    "mmap shm transfer (uniform)   : rounds=32 elapsed=1.0 ms "
    "throughput=2222.2 MB/s bytes_per_tensor=8388608",
    "Raw TCP streaming (patterned) : rounds=32 elapsed=1.0 ms "
    "throughput=3333.3 MB/s bytes_per_tensor=8388608",
    "Raw TCP streaming (uniform)   : rounds=32 elapsed=1.0 ms "
    "throughput=4444.4 MB/s bytes_per_tensor=8388608",
    "FASTLINK framed (RAW codec)   : rounds=32 elapsed=1.0 ms "
    "raw=555.5 MB/s wire=555.5 MB/s wire_bytes=8389260 delta=652 B",
    "FASTLINK framed (RLE codec)   : rounds=32 elapsed=1.0 ms "
    "raw=666.6 MB/s wire=0.9 MB/s wire_bytes=77777 delta=-8321908 B",
])

COMPLETE_METRICS = {
    "mmap_shm": {
        "min_ms": 0.101,
        "p50_ms": 0.102,
        "p95_ms": 0.103,
        "p99_ms": 0.104,
        "mean_ms": 0.105,
        "patterned_tp_mbs": 1111.1,
        "uniform_tp_mbs": 2222.2,
    },
    "raw_tcp": {
        "min_ms": 0.201,
        "p50_ms": 0.202,
        "p95_ms": 0.203,
        "p99_ms": 0.204,
        "mean_ms": 0.205,
        "patterned_tp_mbs": 3333.3,
        "uniform_tp_mbs": 4444.4,
    },
    "fastlink_rpc": {
        "min_ms": 0.301,
        "p50_ms": 0.302,
        "p95_ms": 0.303,
        "p99_ms": 0.304,
        "mean_ms": 0.305,
        "patterned_raw_mbs": 555.5,
        "uniform_raw_mbs": 666.6,
        "uniform_wire_bytes": 77777,
    },
}


class MmapCollectorTests(unittest.TestCase):
    def _run_with(self, result):
        with mock.patch.object(
            collector.subprocess, "run", return_value=result,
        ) as run:
            outcome = collector.run_mmap_transport_microbench()
            run.assert_called_once_with(
                COMMAND, capture_output=True, text=True, timeout=120,
            )
            return outcome

    def test_nonzero_exit_raises_and_never_returns_metrics(self):
        result = subprocess.CompletedProcess(
            COMMAND,
            1,
            stdout=SYNTHETIC_STDOUT,
            stderr=(
                "FASTLINK mmap_transfer is unsupported on Windows: "
                "POSIX shared memory is required."
            ),
        )
        with mock.patch.object(
            collector.subprocess, "run", return_value=result,
        ) as run:
            with self.assertRaisesRegex(
                RuntimeError, r"exit code 1.*unsupported on Windows",
            ):
                collector.run_mmap_transport_microbench()
            run.assert_called_once_with(
                COMMAND, capture_output=True, text=True, timeout=120,
            )

    def test_complete_success_parses_values_from_stdout(self):
        result = subprocess.CompletedProcess(
            COMMAND, 0, stdout=SYNTHETIC_STDOUT, stderr="",
        )
        parsed, stdout = self._run_with(result)
        self.assertEqual(parsed, COMPLETE_METRICS)
        self.assertEqual(
            list(parsed["mmap_shm"]),
            [
                "p50_ms", "p95_ms", "p99_ms", "min_ms", "mean_ms",
                "patterned_tp_mbs", "uniform_tp_mbs",
            ],
        )
        self.assertEqual(
            list(parsed["fastlink_rpc"]),
            [
                "p50_ms", "p95_ms", "p99_ms", "min_ms", "mean_ms",
                "patterned_raw_mbs", "uniform_raw_mbs", "uniform_wire_bytes",
            ],
        )
        self.assertEqual(stdout, SYNTHETIC_STDOUT)

    def test_zero_exit_with_missing_metrics_raises_instead_of_defaults(self):
        incomplete = "\n".join(
            line for line in SYNTHETIC_STDOUT.splitlines()
            if "FASTLINK framed (RLE codec)" not in line
        )
        result = subprocess.CompletedProcess(
            COMMAND, 0, stdout=incomplete, stderr="",
        )
        with mock.patch.object(
            collector.subprocess, "run", return_value=result,
        ) as run:
            with self.assertRaisesRegex(
                RuntimeError, r"fastlink_rpc\.uniform_wire_bytes",
            ):
                collector.run_mmap_transport_microbench()
            run.assert_called_once_with(
                COMMAND, capture_output=True, text=True, timeout=120,
            )

    def test_zero_exit_incomplete_output_includes_stdout_diagnostic(self):
        diagnostic = (
            "mmap_transfer diagnostic: benchmark printed no RLE metrics"
        )
        incomplete = "\n".join(
            line for line in SYNTHETIC_STDOUT.splitlines()
            if "FASTLINK framed (RLE codec)" not in line
        )
        result = subprocess.CompletedProcess(
            COMMAND, 0, stdout=incomplete + "\n" + diagnostic, stderr="",
        )
        with mock.patch.object(
            collector.subprocess, "run", return_value=result,
        ) as run:
            with self.assertRaises(RuntimeError) as raised:
                collector.run_mmap_transport_microbench()
            message = str(raised.exception)
            self.assertIn("missing metrics", message)
            self.assertIn("fastlink_rpc.uniform_wire_bytes", message)
            self.assertIn(diagnostic, message)
            run.assert_called_once_with(
                COMMAND, capture_output=True, text=True, timeout=120,
            )

    def test_nonzero_exit_bounds_diagnostic_and_keeps_exit_code(self):
        diagnostic = "collector failure: " + ("x" * 10_000)
        result = subprocess.CompletedProcess(
            COMMAND, 1, stdout="", stderr=diagnostic,
        )
        with mock.patch.object(
            collector.subprocess, "run", return_value=result,
        ) as run:
            with self.assertRaises(RuntimeError) as raised:
                collector.run_mmap_transport_microbench()
            message = str(raised.exception)
            self.assertIn("exit code 1", message)
            self.assertIn("collector failure", message)
            self.assertIn("truncated", message)
            self.assertLess(len(message), 5_000)
            run.assert_called_once_with(
                COMMAND, capture_output=True, text=True, timeout=120,
            )

    def test_zero_exit_incomplete_output_includes_stderr_diagnostic(self):
        diagnostic = (
            "FASTLINK mmap_transfer is unsupported on Windows: "
            "POSIX shared memory is required."
        )
        result = subprocess.CompletedProcess(
            COMMAND, 0, stdout="", stderr=diagnostic,
        )
        with mock.patch.object(
            collector.subprocess, "run", return_value=result,
        ) as run:
            with self.assertRaises(RuntimeError) as raised:
                collector.run_mmap_transport_microbench()
            message = str(raised.exception)
            self.assertIn("missing metrics", message)
            self.assertIn("mmap_shm.min_ms", message)
            self.assertIn("unsupported on Windows", message)
            run.assert_called_once_with(
                COMMAND, capture_output=True, text=True, timeout=120,
            )

    def test_zero_exit_incomplete_output_bounds_diagnostic(self):
        diagnostic = "unsupported on Windows: " + ("x" * 10_000)
        result = subprocess.CompletedProcess(
            COMMAND, 0, stdout="", stderr=diagnostic,
        )
        with mock.patch.object(
            collector.subprocess, "run", return_value=result,
        ) as run:
            with self.assertRaises(RuntimeError) as raised:
                collector.run_mmap_transport_microbench()
            message = str(raised.exception)
            self.assertIn("missing metrics", message)
            self.assertIn("unsupported on Windows", message)
            self.assertIn("truncated", message)
            self.assertLess(len(message), 5_000)
            run.assert_called_once_with(
                COMMAND, capture_output=True, text=True, timeout=120,
            )


if __name__ == "__main__":
    unittest.main()
