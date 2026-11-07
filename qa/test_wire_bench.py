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


if __name__ == "__main__":
    unittest.main()
