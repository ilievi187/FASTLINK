"""Unit tests for qa/sim_faults.py (FL3-C3). Model arithmetic; seeded MC.
"""
import io
import contextlib
import unittest

import sim_faults as sf
import sim_placement as sp
from sim_common import LINK_REMOTE, REMOTE_DEVICE, WORKLOAD


class ValidationTests(unittest.TestCase):
    def test_degraded_link_rejects_bad_inputs(self):
        for bad in (0.0, -0.5, float("nan"), float("inf")):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                sf.degraded_link(bad)
        with self.assertRaises(ValueError):
            sf.degraded_link(1.0, loss_p=1.5)

    def test_loss_scenario_rejects_bad_inputs(self):
        for p in (-0.1, 1.1):
            with self.subTest(p=p), self.assertRaises(ValueError):
                sf.loss_scenario("weight_chunk_64KiB", p, 0)
        for budget in (-1, 1.0, True):
            with self.subTest(budget=budget), self.assertRaises(ValueError):
                sf.loss_scenario("weight_chunk_64KiB", 0.01, budget)
        with self.assertRaises(ValueError):
            sf.loss_scenario("not_an_op", 0.01, 2)
        with self.assertRaises(ValueError):
            sf.loss_scenario("weight_chunk_64KiB", 0.01, 0, trials=True)

    def test_crash_committed_values_are_range_checked(self):
        for bad in (0, 8, 9, True, 2.5, None):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                sf.crash_scenarios(committed_values=(bad,))


class DegradationTests(unittest.TestCase):
    def setUp(self):
        self.ref = sf.reference_setup()

    def test_full_factor_matches_baseline_row(self):
        steady = sf.degraded_steady_state(self.ref, 1.0)
        self.assertAlmostEqual(steady["decode_step_ms"], self.ref.decode_step_ms,
                               places=6)
        self.assertAlmostEqual(steady["cold_load_remote_s"],
                               self.ref.cold_load_remote_s, places=6)

    def test_degradation_monotonically_slows_everything(self):
        rows = sf.degradation_rows(self.ref)
        for a, b in zip(rows, rows[1:]):
            self.assertLess(b.decode_tokens_per_s, a.decode_tokens_per_s)
            self.assertGreater(b.cold_load_remote_s, a.cold_load_remote_s)
            self.assertGreater(b.prefill_ms, a.prefill_ms)

    def test_cold_load_scales_with_inverse_throughput(self):
        steady_full = sf.degraded_steady_state(self.ref, 1.0)
        steady_1pct = sf.degraded_steady_state(self.ref, 0.01)
        wire_full = steady_full["cold_load_remote_s"]
        wire_1pct = steady_1pct["cold_load_remote_s"]
        fixed = LINK_REMOTE.rtt_ms + LINK_REMOTE.serialization_ms \
            + LINK_REMOTE.queue_ms
        self.assertAlmostEqual((wire_1pct * 1000.0 - fixed)
                               / (wire_full * 1000.0 - fixed), 100.0,
                               places=1)

    def test_decode_degradation_is_wire_term_driven(self):
        # Compute chain is unchanged; boundary wire term scales with 1/f.
        rows = sf.degradation_rows(self.ref)
        full, pct = rows[0], rows[3]
        wire = WORKLOAD.hidden_mib(1) * 1000.0 / LINK_REMOTE.throughput_mib_s
        expected = (full.decode_step_ms - 2 * wire
                    + 2 * wire * 100.0)
        self.assertAlmostEqual(pct.decode_step_ms, expected, places=6)
        self.assertAlmostEqual(pct.decode_tokens_per_s, 22.7, delta=0.1)


class RetryGridTests(unittest.TestCase):
    def test_cells_are_deterministic(self):
        a = sf.loss_scenario("ubatch_boundary_4MiB", 0.05, 2)
        b = sf.loss_scenario("ubatch_boundary_4MiB", 0.05, 2)
        self.assertEqual(a, b)

    def test_completion_increases_with_budget(self):
        for op in dict(sf.OP_CLASSES):
            rates = [
                sf.loss_scenario(op, 0.05, budget)["completion_rate"]
                for budget in sf.BUDGET_GRID
            ]
            self.assertEqual(rates, sorted(rates))

    def test_budget0_completion_tracks_binomial(self):
        scenario = sf.loss_scenario("ubatch_boundary_4MiB", 0.05, 0)
        self.assertAlmostEqual(scenario["completion_rate"], 0.95, delta=0.02)

    def test_budget2_iterations_bound_terminal_failure(self):
        # 3 attempts: terminal failure probability is p^3 = 1.25e-4, so
        # with 2000 seeded trials the completion rate must be >= 99pt9%.
        scenario = sf.loss_scenario("ubatch_boundary_4MiB", 0.05, 2)
        self.assertGreaterEqual(scenario["completion_rate"], 0.999)


class CrashTests(unittest.TestCase):
    def test_recovery_table_saving_is_committed_fraction(self):
        n_ub = -(-WORKLOAD.prompt_tokens // WORKLOAD.ubatch_tokens)
        for case in sf.crash_scenarios():
            self.assertAlmostEqual(case.saving_percent,
                                   100.0 * case.committed_ubatches / n_ub,
                                   delta=0.5)

    def test_resume_never_worse_than_full_redo(self):
        for case in sf.crash_scenarios():
            self.assertLess(case.resume_recovery_ms, case.full_redo_ms)

    def test_wasted_work_grows_with_committed_steps(self):
        cases = sf.crash_scenarios()
        self.assertGreater(cases[-1].wasted_work_ms, cases[0].wasted_work_ms)
        self.assertEqual(cases[0].committed_ubatches, 2)


class PolicyTests(unittest.TestCase):
    def test_timeout_has_floor_and_grows_with_degradation(self):
        t_full = sf.attempt_timeout_ms(4.0, 1.0)
        t_1pct = sf.attempt_timeout_ms(4.0, 0.01)
        self.assertEqual(t_full, max(sf.TIMEOUT_FLOOR_MS,
                                     3.0 * (LINK_REMOTE.queue_ms
                                            + LINK_REMOTE.rtt_ms
                                            + LINK_REMOTE.serialization_ms
                                            + 1000.0 * 4.0
                                            / LINK_REMOTE.throughput_mib_s)))
        self.assertGreater(t_1pct, t_full)
        self.assertAlmostEqual(sf.attempt_timeout_ms(0.0078125, 1.0),
                               sf.TIMEOUT_FLOOR_MS)

    def test_policy_rows_cover_all_ops_and_factors(self):
        rows = sf.policy_rows()
        self.assertEqual({r["op"] for r in rows},
                         {name for name, _ in sf.OP_CLASSES})
        for row in rows:
            self.assertEqual(sorted(row["timeouts"]),
                             sorted(sf.DEGRADATION_FACTORS))

    def test_recommendation_text_is_deterministic(self):
        a = sf.recommended_policy_text()
        b = sf.recommended_policy_text()
        self.assertEqual(a, b)


class ReportingTests(unittest.TestCase):
    def test_main_output_is_deterministic(self):
        buf_a, buf_b = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(buf_a):
            sf.main()
        with contextlib.redirect_stdout(buf_b):
            sf.main()
        self.assertEqual(buf_a.getvalue(), buf_b.getvalue())

    def test_main_output_contains_all_sections(self):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            sf.main()
        text = buf.getvalue()
        for marker in ("throughput degradation", "retry grid",
                       "worker crash mid-prefill", "R1.", "R5.",
                       "recommended fallback placement"):
            self.assertIn(marker, text)


if __name__ == "__main__":
    unittest.main()
