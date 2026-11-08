import json
import subprocess
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parent.parent


def run_v4_bench_json() -> dict:
    candidates = [
        p
        for p in (
            REPO_ROOT / "target" / "release" / "fastlink",
            REPO_ROOT / "target" / "debug" / "fastlink",
        )
        if p.exists()
    ]
    if not candidates:
        subprocess.run(
            ["cargo", "build", "--bin", "fastlink", "--quiet"],
            cwd=REPO_ROOT,
            check=True,
        )
        candidates = [REPO_ROOT / "target" / "debug" / "fastlink"]
    binary = max(candidates, key=lambda p: p.stat().st_mtime)
    proc = subprocess.run(
        [str(binary), "v4-bench", "--json"],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    return json.loads(proc.stdout.strip())


class V4ResidencyAndTransferTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.report = run_v4_bench_json()

    def test_residency_warm_passes_transfer_zero_wire_bytes(self):
        self.assertEqual(self.report["warm_9_passes_wire_bytes"], 0)
        self.assertEqual(self.report["reuse_events"], 36)
        self.assertGreater(self.report["reuse_bytes_saved"], 35_000_000)
        self.assertLess(
            self.report["v4_total_wire_bytes_sent"],
            self.report["v3_ten_passes_wire_bytes"] // 20,
        )

    def test_delta_one_chunk_of_six_transfers_one_sixth_bytes(self):
        self.assertEqual(self.report["delta_reused_chunks"], 5)
        self.assertEqual(self.report["delta_transferred_chunks"], 1)
        self.assertLess(
            self.report["delta_1_of_6_wire_bytes"],
            self.report["delta_full_wire_bytes"] // 3,
        )

    def test_worker_restart_resyncs_only_restarted_worker(self):
        self.assertEqual(self.report["restart_invalidated"], 1)
        self.assertTrue(self.report["restart_worker_a_resync_full"])
        self.assertTrue(self.report["restart_worker_b_kept_reuse"])

    def test_adaptive_codec_and_chunk_selection_matches_measured_policy(self):
        plans = self.report["plans"]
        self.assertEqual(plans["incompressible"]["codec"], "RAW")
        self.assertIn(plans["incompressible"]["chunk_bytes"], (65536, 262144))
        self.assertEqual(plans["sparse"]["codec"], "RLE")
        self.assertEqual(plans["sparse"]["chunk_bytes"], 65536)
        self.assertEqual(plans["structured"]["codec"], "LZ4")
        self.assertEqual(plans["structured"]["chunk_bytes"], 16384)
        self.assertEqual(plans["dense_numeric"]["codec"], "ZSTD")
        self.assertIn(plans["dense_numeric"]["chunk_bytes"], (4096, 16384))

    def test_wire_byte_accounting_fields_exposed_and_positive(self):
        self.assertGreater(self.report["bytes_per_token"], 0.0)
        self.assertGreater(self.report["reused_chunks"], 0)
        self.assertGreater(self.report["transferred_chunks"], 0)
        self.assertEqual(self.report["sync_events"], 42)
        self.assertGreater(self.report["encode_ms"], 0.0)
        self.assertGreater(self.report["decode_ms"], 0.0)


if __name__ == "__main__":
    unittest.main()
