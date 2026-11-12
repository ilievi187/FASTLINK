"""Unit tests for qa/sim_pipeline.py (FL3-C2). Model arithmetic only.
"""
import io
import contextlib
import unittest

import sim_pipeline as spi
import sim_placement as sp
from sim_common import REMOTE_DEVICE, WORKLOAD, kv_bytes_per_token_per_layer


class ValidationTests(unittest.TestCase):
    def test_invalid_batch_is_rejected(self):
        for bad in (0, -1, 1.5, "4", True, None):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                spi.batched_sync_ms(bad)

    def test_invalid_ubatch_is_rejected(self):
        for bad in (0, -256, 512.5, "512", None):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                spi.ubatch_major_prefill(bad)

    def test_unknown_kv_label_is_rejected(self):
        with self.assertRaises(ValueError):
            spi.prefill_kv_bpl("q3")
        with self.assertRaises(ValueError):
            spi.strict_sync_ms("int4")


class DecodeSemanticsTests(unittest.TestCase):
    def test_strict_sync_matches_c1_reference_row(self):
        row = sp.compute_row(spi.REF_SPLIT_K_LOCAL, 131072, "q4")
        self.assertAlmostEqual(spi.strict_sync_ms(), row.decode_step_ms,
                               places=9)

    def test_strict_sync_is_compute_plus_two_boundaries(self):
        expected = (spi.per_token_local_ms() + spi.per_token_remote_ms()
                    + 2 * spi.boundary_one_stream_ms())
        self.assertAlmostEqual(spi.strict_sync_ms(), expected, places=9)

    def test_batched_step_amortises_boundary_not_compute(self):
        one = spi.strict_sync_ms()
        b8 = spi.batched_sync_ms(8)
        compute_net = (spi.per_token_local_ms() + spi.per_token_remote_ms())
        # 1 stream of boundary saved per direction beyond the first batch:
        expected = 8 * compute_net + 2 * spi.sp.boundary_ms(8)
        self.assertAlmostEqual(b8, expected, places=9)
        # Batching cannot be slower than B strict steps plus its boundary.
        self.assertLess(b8, 8 * one)

    def test_batched_tok_per_s_bounded_by_the_fractional_rtt_share(self):
        asymptote = 1000.0 / (spi.per_token_local_ms()
                              + spi.per_token_remote_ms())
        for b in (2, 8, 128):
            with self.subTest(b=b):
                step = spi.batched_sync_ms(b)
                self.assertLess(1000.0 * b / step, asymptote)
                self.assertGreater(1000.0 * b / step,
                                   1000.0 / spi.strict_sync_ms())

    def test_pipelined_is_max_of_device_and_link_chains(self):
        self.assertAlmostEqual(spi.pipelined_ideal_ms(4),
                               max(4 * spi.per_token_local_ms(),
                                   4 * spi.per_token_remote_ms(),
                                   2 * spi.sp.boundary_ms(4)),
                               places=9)

    def test_pipelined_tok_per_s_never_exceeds_local_roofline(self):
        roofline = 1000.0 / spi.per_token_local_ms()
        for b in (2, 4, 8, 64):
            with self.subTest(b=b):
                res = spi.pipelined_ideal_result(
                    b, 32768, spi.REF_KV_LABEL)
                self.assertLessEqual(res.tokens_per_s, roofline + 1e-6)

    def test_vram_feasible_flag_follows_C1_stream_count(self):
        # 128K q4 k=53 -> 1 stream: batching at B>1 must be flagged.
        res = spi.batched_sync_result(2, 131072, "q4")
        self.assertFalse(res.vram_feasible)
        res = spi.pipelined_ideal_result(2, 131072, "q4")
        self.assertFalse(res.vram_feasible)
        res = spi.batched_sync_result(4, 32768, "q4")
        self.assertTrue(res.vram_feasible)

    def test_single_stream_mode_P_flagged_infeasible(self):
        res = spi.pipelined_ideal_result(1, 32768, "q4")
        self.assertFalse(res.vram_feasible)


class PrefillSemanticsTests(unittest.TestCase):
    def test_ubatch_mode_matches_C1_prefill_cost_exactly(self):
        row = sp.compute_row(spi.REF_SPLIT_K_LOCAL, 131072, "q4")
        res = spi.ubatch_major_prefill(WORKLOAD.ubatch_tokens)
        self.assertAlmostEqual(res.total_ms, row.prefill_ms, places=6)

    def test_layer_major_never_slower_than_ubatch_major(self):
        for ub in (256, 512, 1024, 2048, 4096):
            with self.subTest(ub=ub):
                ub_res = spi.ubatch_major_prefill(ub)
                lm_res = spi.layer_major_prefill(ub)
                self.assertLessEqual(lm_res.total_ms, ub_res.total_ms + 1e-9)

    def test_layer_major_saving_shrinks_as_ubatch_grows(self):
        saving_small = (spi.ubatch_major_prefill(256).total_ms
                        - spi.layer_major_prefill(256).total_ms)
        saving_large = (spi.ubatch_major_prefill(4096).total_ms
                        - spi.layer_major_prefill(4096).total_ms)
        self.assertGreater(saving_small, saving_large)
        self.assertAlmostEqual(saving_large, 0.0, places=6)

    def test_boundary_totals_equal_within_mode(self):
        ub = spi.ubatch_major_prefill(512)
        lm = spi.layer_major_prefill(512)
        self.assertAlmostEqual(ub.boundary_ms_total, lm.boundary_ms_total,
                               places=6)
        self.assertAlmostEqual(
            ub.boundary_ms_total,
            16 * spi.sp.boundary_ms(512),  # 8 ubatches x 2 directions
            places=6)


class ReportingTests(unittest.TestCase):
    def test_main_output_is_deterministic(self):
        buf_a, buf_b = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(buf_a):
            spi.main()
        with contextlib.redirect_stdout(buf_b):
            spi.main()
        self.assertEqual(buf_a.getvalue(), buf_b.getvalue())

    def test_main_output_contains_mode_tables_and_marker_lines(self):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            spi.main()
        text = buf.getvalue()
        self.assertIn("MODE S step latency", text)
        self.assertIn("LM saving ms", text)
        self.assertIn("VRAM B_max is 1 at", text)


if __name__ == "__main__":
    unittest.main()
