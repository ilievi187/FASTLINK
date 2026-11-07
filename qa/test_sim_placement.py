"""Unit tests for qa/sim_placement.py (FL3-C1).

Every expectation below is MODEL arithmetic over the declared inputs in
qa/sim_common.py. Tests lock model semantics and regressions; they must not
be updated by re-declaring new numbers without changing the declared inputs
deliberately.
"""
import io
import contextlib
import unittest

import sim_placement as sp
from sim_common import (
    KV_LABELS,
    LINK_REMOTE,
    LOCAL_DEDICATED,
    LOCAL_MIXED,
    REMOTE_DEVICE,
    WORKLOAD,
)


class ValidationTests(unittest.TestCase):
    def test_invalid_k_local_is_rejected(self):
        for bad in (-1, 65, 1.5, "30", True, None):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                sp.compute_row(bad, 131072, "q4")

    def test_unknown_kv_mode_is_rejected(self):
        with self.assertRaises(ValueError):
            sp.compute_row(53, 131072, "q3")

    def test_non_integer_context_is_rejected(self):
        for bad in (None, 12.5, "32K", -1):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                sp.compute_row(53, bad, "q4")

    def test_matrix_row_rejects_non_finite_values(self):
        with self.assertRaises(ValueError):
            sp.MatrixRow(
                k_local=53, layers_remote=11,
                weights_local_gib=12.0, weights_remote_gib=2.5,
                kv_local_gib=6.6, kv_remote_gib=1.4,
                feasible=True, deficit_gib=0.0, cold_load_remote_s=float("nan"),
                prefill_ms=1.0, prefill_tokens_per_s=1.0,
                decode_step_ms=1.0, decode_tokens_per_s=1.0,
                boundary_overhead_ms_per_token=1.0,
            )

    def test_layer_ms_multipliers_are_validated(self):
        with self.assertRaises(ValueError):
            sp.layer_ms(LOCAL_DEDICATED, 1024, 3)


class SemanticsTests(unittest.TestCase):
    def test_all_local_split_has_no_boundary_and_no_remote_cost(self):
        row = sp.compute_row(WORKLOAD.layers, 32768, "q4",
                             LOCAL_DEDICATED, REMOTE_DEVICE)
        expected = (
            WORKLOAD.weights_gib
            + WORKLOAD.layers * 1024 / 2**30
        ) / LOCAL_DEDICATED.bandwidth_gib_s * 1000
        self.assertAlmostEqual(row.decode_step_ms, expected, places=2)
        self.assertAlmostEqual(row.boundary_overhead_ms_per_token, 0.0)
        self.assertEqual(row.cold_load_remote_s, 0.0)
        self.assertTrue(row.kv_remote_gib == 0.0)

    def test_decode_boundary_is_two_round_trips_when_split(self):
        # queue 0.2 + rtt 1.0 + ser 0.05 + wire(8 KiB) 0.0819 -> 1.3319 per trip
        row = sp.compute_row(53, 131072, "q4")
        self.assertAlmostEqual(row.boundary_overhead_ms_per_token, 2.6639,
                               places=3)

    def test_decode_step_is_sum_of_sides_plus_boundary(self):
        row = sp.compute_row(53, 131072, "q4")
        expected = (
            53 * sp.layer_ms(LOCAL_DEDICATED, 1024, 1)
            + 11 * sp.layer_ms(REMOTE_DEVICE, 1024, 1)
            + 2 * sp.boundary_ms(1)
        )
        self.assertAlmostEqual(row.decode_step_ms, expected, places=9)

    def test_prefill_scales_with_prompt_tokens_and_has_no_ctx_term(self):
        small = sp.compute_row(53, 32768, "q4")
        large = sp.compute_row(53, 131072, "q4")
        # Context only affects VRAM feasibility, not the 4096-token prefill.
        self.assertAlmostEqual(small.prefill_ms, large.prefill_ms, places=6)
        self.assertNotAlmostEqual(small.kv_local_gib, large.kv_local_gib)

    def test_prefill_is_faster_than_decode_per_token_at_split(self):
        row = sp.compute_row(53, 131072, "q4")
        self.assertGreater(row.prefill_tokens_per_s, row.decode_tokens_per_s)

    def test_cold_load_grows_with_remote_share(self):
        a = sp.compute_row(53, 131072, "q4")
        b = sp.compute_row(40, 131072, "q4")
        self.assertLess(a.cold_load_remote_s, b.cold_load_remote_s)
        self.assertAlmostEqual(b.cold_load_remote_s,
                               ((b.weights_remote_gib * 1024.0 * 1000.0)
                                / LINK_REMOTE.throughput_mib_s
                                + LINK_REMOTE.rtt_ms
                                + LINK_REMOTE.serialization_ms
                                + LINK_REMOTE.queue_ms) / 1000.0,
                               delta=1.0)

    def test_deficit_is_over_budget_amount_not_utilisation(self):
        row = sp.compute_row(37, 131072, "fp16", LOCAL_MIXED)
        self.assertFalse(row.feasible)
        self.assertAlmostEqual(
            row.deficit_gib,
            max(row.weights_local_gib + row.kv_local_gib
                - LOCAL_MIXED.budget_gib,
                row.weights_remote_gib + row.kv_remote_gib
                - REMOTE_DEVICE.budget_gib),
            places=3,
        )


class ScenarioTests(unittest.TestCase):
    def test_128k_fp16_is_infeasible_everywhere(self):
        rows = sp.compute_matrix(131072, "fp16", LOCAL_MIXED)
        self.assertEqual(sp.feasible_ks(rows), [])
        rows = sp.compute_matrix(131072, "fp16", LOCAL_DEDICATED)
        self.assertEqual(sp.feasible_ks(rows), [])

    def test_128k_q4_window_is_48_to_53_on_dedicated_device(self):
        rows = sp.compute_matrix(131072, "q4", LOCAL_DEDICATED)
        self.assertEqual(sorted(sp.feasible_ks(rows)), list(range(48, 54)))

    def test_mixed_14gib_128k_q4_is_infeasible(self):
        rows = sp.compute_matrix(131072, "q4", LOCAL_MIXED)
        self.assertEqual(sp.feasible_ks(rows), [])

    def test_best_row_none_when_infeasible(self):
        rows = sp.compute_matrix(131072, "fp16", LOCAL_MIXED)
        self.assertIsNone(sp.best_row(rows))

    def test_rank_puts_feasible_first_by_decode_rate(self):
        rows = sp.rank_rows(sp.compute_matrix(131072, "q4", LOCAL_DEDICATED))
        self.assertTrue(rows[0].feasible)
        for i in range(1, len(rows) - 1):
            a, b = rows[i], rows[i + 1]
            if a.feasible and b.feasible:
                self.assertGreaterEqual(a.decode_tokens_per_s,
                                        b.decode_tokens_per_s)
        self.assertFalse(rows[-1].feasible)

    def test_best_128k_q4_split_matches_declared_reference(self):
        row = sp.compute_row(53, 131072, "q4", LOCAL_DEDICATED)
        self.assertTrue(row.feasible)
        # Anchored model outputs for the FL3-C1 reference split.
        self.assertAlmostEqual(row.decode_step_ms, 27.866, delta=0.01)
        self.assertAlmostEqual(row.decode_tokens_per_s, 35.89, delta=0.05)
        self.assertAlmostEqual(row.cold_load_remote_s, 27.3, delta=0.2)
        self.assertAlmostEqual(row.boundary_overhead_ms_per_token, 2.66,
                               delta=0.01)
        self.assertAlmostEqual(row.prefill_ms, 103956, delta=50)
        self.assertAlmostEqual(row.prefill_tokens_per_s, 39.42, delta=0.05)


class ReportingTests(unittest.TestCase):
    def test_main_output_is_deterministic(self):
        buf_a, buf_b = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(buf_a):
            sp.main()
        with contextlib.redirect_stdout(buf_b):
            sp.main()
        self.assertEqual(buf_a.getvalue(), buf_b.getvalue())

    def test_main_output_contains_ranked_rows_and_markers(self):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            sp.main()
        text = buf.getvalue()
        self.assertIn("ranked placement matrix", text)
        self.assertIn("feasible k = 48..53", text)
        self.assertIn("64/0", text)  # all-local split appears in the matrix

    def test_max_decode_streams_uses_full_kv_share(self):
        row = sp.compute_row(53, 131072, "q4", LOCAL_DEDICATED)
        streams = sp.max_decode_streams(row, 131072, "q4", LOCAL_DEDICATED)
        self.assertEqual(streams, 1)
        row32 = sp.compute_row(64, 32768, "q4", LOCAL_DEDICATED)
        self.assertEqual(sp.max_decode_streams(row32, 32768, "q4",
                                               LOCAL_DEDICATED), 2)


if __name__ == "__main__":
    unittest.main()
