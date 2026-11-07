import json
import tempfile
import unittest
from pathlib import Path

from wire_bench import parse_sender_summary, summarize_cases, write_json_output


class WireBenchTests(unittest.TestCase):
    def test_parses_sender_payload_metrics_without_claiming_tcp_stream_bytes(self):
        text = """FASTLINK sync complete: blob.bin
  file_bytes: 1024
  chunks_total: 1
  chunks_reused: 0
  chunks_transferred: 1
  raw_missing_bytes: 1024
  encoded_payload_bytes: 900
  elapsed_ms: 12
  note: payload counts exclude wire framing, hashes, and TCP overhead
"""
        parsed = parse_sender_summary(text)
        self.assertEqual(parsed["encoded_payload_bytes"], 900)
        self.assertEqual(parsed["elapsed_ms"], 12)
        self.assertNotIn("wire_bytes", parsed)

    def test_summary_reports_tcp_stream_and_payload_separately(self):
        cases = [
            {"name": "cold", "tcp_stream_bytes": 1200, "encoded_payload_bytes": 900,
             "elapsed_ms": 12, "ok": True},
            {"name": "warm", "tcp_stream_bytes": 200, "encoded_payload_bytes": 0,
             "elapsed_ms": 3, "ok": True},
        ]
        summary = summarize_cases(cases)
        self.assertEqual(summary["case_count"], 2)
        self.assertEqual(summary["tcp_stream_bytes_total"], 1400)
        self.assertEqual(summary["payload_bytes_total"], 900)
        self.assertEqual(summary["elapsed_ms_median"], 7.5)
        self.assertEqual(summary["all_correct"], True)

    def test_json_output_creates_missing_parent_directory(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "nested" / "result.json"
            write_json_output(output, {"status": "ok"})
            self.assertEqual(json.loads(output.read_text()), {"status": "ok"})

    def test_summary_rejects_failed_scenario(self):
        with self.assertRaises(ValueError):
            summarize_cases([{"name": "bad", "tcp_stream_bytes": 1,
                              "encoded_payload_bytes": 0, "elapsed_ms": 1,
                              "ok": False}])

    def test_parses_full_wire_byte_accounting_and_speed_metrics(self):
        text = """FASTLINK sync complete: tensor-fixture.bin
  file_bytes: 1048576
  chunks_total: 16
  chunks_reused: 0
  chunks_transferred: 16
  raw_missing_bytes: 1048576
  encoded_payload_bytes: 1048576
  handshake_wire_bytes: 12
  manifest_wire_bytes: 549
  need_wire_bytes: 68
  chunk_framing_wire_bytes: 144
  status_wire_bytes: 1
  headers_and_framing_wire_bytes: 774
  wire_bytes_tx: 1049275
  wire_bytes_rx: 75
  wire_bytes_total: 1049350
  handshake_us: 120
  manifest_us: 450
  transfer_us: 8500
  elapsed_us: 9070
  elapsed_ms: 9
  wire_mib_per_s: 110.312
  logical_mib_per_s: 110.231
"""
        parsed = parse_sender_summary(text)
        self.assertEqual(parsed["wire_bytes_total"], 1049350)
        self.assertEqual(parsed["wire_bytes_tx"], 1049275)
        self.assertEqual(parsed["wire_bytes_rx"], 75)
        self.assertEqual(parsed["headers_and_framing_wire_bytes"], 774)
        self.assertEqual(parsed["manifest_wire_bytes"], 549)
        self.assertAlmostEqual(parsed["wire_mib_per_s"], 110.312)
        self.assertAlmostEqual(parsed["logical_mib_per_s"], 110.231)

    def test_summary_exposes_cold_and_warm_wire_bytes_and_mib_per_s(self):
        cases = [
            {
                "name": "random_cold",
                "file_bytes": 1048576,
                "tcp_stream_bytes": 1049350,
                "wire_bytes_total": 1049350,
                "encoded_payload_bytes": 1048576,
                "elapsed_ms": 100,
                "wire_mib_per_s": 10.007,
                "logical_mib_per_s": 10.0,
                "ok": True,
            },
            {
                "name": "random_warm",
                "file_bytes": 1048576,
                "tcp_stream_bytes": 566,
                "wire_bytes_total": 566,
                "encoded_payload_bytes": 0,
                "elapsed_ms": 25,
                "wire_mib_per_s": 0.022,
                "logical_mib_per_s": 40.0,
                "ok": True,
            },
        ]
        summary = summarize_cases(cases)
        self.assertEqual(summary["wire_bytes_total"], 1049916)
        self.assertEqual(summary["headers_and_framing_bytes_total"], 1340)
        self.assertEqual(summary["cold_wire_bytes"], 1049350)
        self.assertAlmostEqual(summary["cold_wire_mib_per_s"], 10.007)
        self.assertEqual(summary["warm_wire_bytes"], 566)
        self.assertAlmostEqual(summary["warm_wire_mib_per_s"], 0.022)
        self.assertAlmostEqual(summary["warm_logical_mib_per_s"], 40.0)


if __name__ == "__main__":
    unittest.main()
