"""TDD unit tests for qa/network_sim.py (CPU-only planning simulator).

Every expectation below describes the MODEL arithmetic, not a hardware
measurement. Optimistic/observed GPU numbers are out of scope.
"""
import random
import unittest

from network_sim import (
    Link,
    Stage,
    transfer,
    simulate_pipeline,
)

STATUS_OK = "ok"
STATUS_DISCONNECTED = "disconnected"
STATUS_BUDGET_EXHAUSTED = "retry_budget_exhausted"


class LinkValidationTests(unittest.TestCase):
    def test_zero_bandwidth_is_rejected(self):
        with self.assertRaises(ValueError):
            Link(rtt_ms=1, throughput_mib_s=0)

    def test_invalid_link_inputs_are_rejected(self):
        bad_kwargs = [
            dict(rtt_ms=-1, throughput_mib_s=10),
            dict(rtt_ms=0, throughput_mib_s=-10),
            dict(rtt_ms=float("nan"), throughput_mib_s=10),
            dict(rtt_ms=float("inf"), throughput_mib_s=10),
            dict(rtt_ms=1, throughput_mib_s=float("inf")),
            dict(rtt_ms=1, throughput_mib_s=10, serialization_ms=float("nan")),
            dict(rtt_ms=1, throughput_mib_s=10, serialization_ms=-0.5),
            dict(rtt_ms=1, throughput_mib_s=10, queue_ms=float("inf")),
            dict(rtt_ms=1, throughput_mib_s=10, queue_ms=-1),
            dict(rtt_ms=1, throughput_mib_s=10, loss_p=-0.1),
            dict(rtt_ms=1, throughput_mib_s=10, loss_p=1.1),
            dict(rtt_ms=1, throughput_mib_s=10, loss_p=float("nan")),
            dict(rtt_ms=1, throughput_mib_s=10, disconnect_after_rts=0),
            dict(rtt_ms=1, throughput_mib_s=10, disconnect_after_rts=-3),
            dict(rtt_ms=1, throughput_mib_s=10, disconnect_after_rts=1.5),
        ]
        for kwargs in bad_kwargs:
            with self.subTest(**kwargs), self.assertRaises(ValueError):
                Link(**kwargs)

    def test_boundary_link_values_are_accepted(self):
        Link(rtt_ms=0, throughput_mib_s=1)
        Link(rtt_ms=1, throughput_mib_s=10, loss_p=0.0)
        Link(rtt_ms=1, throughput_mib_s=10, loss_p=1.0)
        Link(rtt_ms=1, throughput_mib_s=10, disconnect_after_rts=1)


class TransferValidationTests(unittest.TestCase):
    def setUp(self):
        self.link = Link(rtt_ms=1, throughput_mib_s=10)

    def test_invalid_transfer_inputs_are_rejected(self):
        cases = [
            dict(mib=-1.0, round_trips=1),
            dict(mib=float("nan"), round_trips=1),
            dict(mib=float("inf"), round_trips=1),
            dict(mib=0.0, round_trips=-1),
            dict(mib=0.0, round_trips=1.5),
            dict(mib=0.0, round_trips=1, retry_budget=-1),
            dict(mib=0.0, round_trips=1, retry_budget=0.5),
            dict(mib=1.0, round_trips=0),
        ]
        for kwargs in cases:
            with self.subTest(**kwargs), self.assertRaises(ValueError):
                transfer(self.link, **kwargs)


class TransferArithmeticTests(unittest.TestCase):
    def test_two_round_trip_cost_matches_formula(self):
        link = Link(rtt_ms=2, throughput_mib_s=10, serialization_ms=1, queue_ms=0.5)
        result = transfer(link, mib=0.1, round_trips=2)
        # Per attempt: rtt 2 + serialization 1 + wire share 1000*(0.1/2)/10 = 5 -> 8 ms.
        # Two attempts = 16 ms; queue is charged once -> 16.5 ms total.
        self.assertEqual(result.status, STATUS_OK)
        self.assertEqual((result.attempts, result.failures), (2, 0))
        self.assertAlmostEqual(result.latency_ms, 16.5)

    def test_ack_only_round_trips_charge_rtt_and_serialization(self):
        link = Link(rtt_ms=2, throughput_mib_s=10, serialization_ms=1)
        result = transfer(link, mib=0.0, round_trips=3)
        self.assertEqual(result.status, STATUS_OK)
        self.assertEqual(result.attempts, 3)
        self.assertAlmostEqual(result.latency_ms, 9.0)

    def test_single_round_trip_wire_cost(self):
        link = Link(rtt_ms=0, throughput_mib_s=10)
        result = transfer(link, mib=1.0, round_trips=1)
        self.assertAlmostEqual(result.latency_ms, 100.0)
        self.assertEqual(result.attempts, 1)

    def test_zero_bytes_zero_round_trips_is_free(self):
        result = transfer(Link(rtt_ms=1, throughput_mib_s=10), mib=0.0, round_trips=0)
        self.assertEqual(result.status, STATUS_OK)
        self.assertAlmostEqual(result.latency_ms, 0.0)
        self.assertEqual((result.attempts, result.failures), (0, 0))

    def test_wire_bytes_must_ride_a_round_trip(self):
        with self.assertRaises(ValueError):
            transfer(Link(rtt_ms=1, throughput_mib_s=10), mib=1.0, round_trips=0)


class StageValidationTests(unittest.TestCase):
    def test_invalid_stage_is_rejected(self):
        bad_kwargs = [
            dict(name="", compute_ms=1),
            dict(name="remote", compute_ms=-1),
            dict(name="remote", compute_ms=float("nan")),
            dict(name="remote", compute_ms=1, outbound_mib=-1),
            dict(name="remote", compute_ms=1, rpc_round_trips=-1),
            dict(name="remote", compute_ms=1, rpc_round_trips=2.5),
        ]
        for kwargs in bad_kwargs:
            with self.subTest(**kwargs), self.assertRaises(ValueError):
                Stage(**kwargs)


class RetryTests(unittest.TestCase):
    def test_certain_loss_consumes_the_retry_budget(self):
        link = Link(rtt_ms=2, throughput_mib_s=10, serialization_ms=1, loss_p=1.0)
        result = transfer(link, mib=0.0, round_trips=2, retry_budget=4)
        # First round trip: 1 initial attempt + 4 retries, all failing
        # (retries_used >= budget) -> exhausted; round trip two never runs.
        self.assertEqual(result.status, STATUS_BUDGET_EXHAUSTED)
        self.assertEqual(result.attempts, 5)
        self.assertEqual(result.failures, 5)
        self.assertAlmostEqual(result.latency_ms, 15.0)

    def test_attempts_never_exceed_round_trips_plus_budget(self):
        link = Link(rtt_ms=1, throughput_mib_s=1, loss_p=0.9)
        result = transfer(
            link, mib=0.0, round_trips=5, retry_budget=3, rng=random.Random(7)
        )
        self.assertLessEqual(result.attempts, 5 + 3)
        # With zero wire share each attempt costs exactly rtt (+ser 0).
        self.assertAlmostEqual(result.latency_ms, result.attempts * 1.0)

    def test_zero_loss_makes_no_draw_and_needs_no_budget(self):
        link = Link(rtt_ms=1, throughput_mib_s=10, queue_ms=0.25)
        result = transfer(link, mib=0.1, round_trips=2, retry_budget=0)
        self.assertEqual(result.status, STATUS_OK)
        self.assertEqual((result.attempts, result.failures), (2, 0))
        # 0.25 queue + 2 * (1 rtt + 0 ser + 5 wire share) = 12.25 ms.
        self.assertAlmostEqual(result.latency_ms, 12.25)

    def test_retry_budget_is_optional(self):
        link = Link(rtt_ms=1, throughput_mib_s=10)
        result = transfer(link, mib=0.0, round_trips=2)
        self.assertEqual(result.status, STATUS_OK)
        self.assertEqual(result.attempts, 2)


class DisconnectTests(unittest.TestCase):
    def test_disconnect_stops_after_n_successful_round_trips(self):
        link = Link(rtt_ms=1, throughput_mib_s=10, disconnect_after_rts=1)
        result = transfer(link, mib=0.0, round_trips=4)
        self.assertEqual(result.status, STATUS_DISCONNECTED)
        self.assertEqual((result.attempts, result.failures), (1, 0))
        self.assertAlmostEqual(result.latency_ms, 1.0)

    def test_disconnect_boundary_at_or_after_needed_round_trips_is_harmless(self):
        link = Link(rtt_ms=1, throughput_mib_s=10, disconnect_after_rts=3)
        result = transfer(link, mib=0.0, round_trips=3)
        self.assertEqual(result.status, STATUS_OK)
        self.assertEqual(result.attempts, 3)


class PipelineTests(unittest.TestCase):
    def test_two_stage_serial_sum_without_loss(self):
        link = Link(rtt_ms=1, throughput_mib_s=10, queue_ms=0.5)
        stages = [
            Stage("local", 4.0),
            Stage("remote", 6.0, outbound_mib=0.1, rpc_round_trips=1),
        ]
        pipeline = simulate_pipeline(stages, link, seed=0)
        self.assertEqual(pipeline.status, STATUS_OK)
        # 4 local + 6 remote compute + queue 0.5 + attempt (1 + 10) = 21.5 ms.
        self.assertAlmostEqual(pipeline.latency_ms, 21.5)
        self.assertAlmostEqual(pipeline.decode_tokens_per_second, 1000.0 / 21.5)
        self.assertEqual(
            [name for name, _result in pipeline.stage_results], ["local", "remote"]
        )

    def test_failed_stage_prevents_later_stage_charges(self):
        link = Link(rtt_ms=1, throughput_mib_s=10, serialization_ms=0.5, loss_p=1.0)
        stages = [
            Stage("local", 4.0),
            Stage("remote", 6.0, outbound_mib=0.0, rpc_round_trips=1),
            Stage("never_runs", 9.0, outbound_mib=0.4, rpc_round_trips=2),
        ]
        pipeline = simulate_pipeline(stages, link, retry_budget=0, seed=0)
        self.assertEqual(pipeline.status, STATUS_BUDGET_EXHAUSTED)
        # 4 + 6 compute + first attempt (1 + 0.5) = 11.5 ms; budget 0 exhausted.
        self.assertAlmostEqual(pipeline.latency_ms, 11.5)
        self.assertIsNone(pipeline.decode_tokens_per_second)
        self.assertEqual(len(pipeline.stage_results), 2)

    def test_disconnect_mid_pipeline_keeps_late_stages_uncharged(self):
        link = Link(
            rtt_ms=1, throughput_mib_s=10, queue_ms=0.5, disconnect_after_rts=2
        )
        stages = [
            Stage("local", 4.0),
            Stage("remote", 6.0, outbound_mib=0.4, rpc_round_trips=4),
            Stage("never_runs", 9.0, outbound_mib=0.4, rpc_round_trips=2),
        ]
        pipeline = simulate_pipeline(stages, link, seed=0)
        self.assertEqual(pipeline.status, STATUS_DISCONNECTED)
        # remote: compute 6 + queue 0.5 + 2 * (1 + 0 + share 10) = 28.5; total 32.5.
        self.assertAlmostEqual(pipeline.latency_ms, 32.5)
        self.assertIsNone(pipeline.decode_tokens_per_second)
        self.assertEqual(len(pipeline.stage_results), 2)

    def test_empty_pipeline_is_rejected(self):
        with self.assertRaises(ValueError):
            simulate_pipeline([], Link(rtt_ms=1, throughput_mib_s=10))


class ReproducibilityTests(unittest.TestCase):
    def test_same_seed_reproduces_stochastic_pipeline(self):
        link = Link(rtt_ms=1, throughput_mib_s=1, loss_p=0.4)
        stages = [Stage("remote", 2.0, outbound_mib=0.5, rpc_round_trips=3)]
        first = simulate_pipeline(stages, link, retry_budget=2, seed=123)
        second = simulate_pipeline(stages, link, retry_budget=2, seed=123)
        self.assertEqual(first, second)

    def test_same_seed_reproduces_stochastic_transfer(self):
        link = Link(rtt_ms=1, throughput_mib_s=1, loss_p=0.5)
        first = transfer(
            link, mib=0.5, round_trips=3, retry_budget=2, rng=random.Random(7)
        )
        second = transfer(
            link, mib=0.5, round_trips=3, retry_budget=2, rng=random.Random(7)
        )
        self.assertEqual(first, second)


if __name__ == "__main__":
    unittest.main()
