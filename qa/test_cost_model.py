import unittest

from cost_model import Link, Stage, decode_tokens_per_second, pipeline_latency_ms, placement_is_feasible


class CostModelTests(unittest.TestCase):
    def test_transfer_cost_includes_round_trip_serialization_and_wire_time(self):
        link = Link(rtt_ms=2, throughput_mib_s=10, serialization_ms=1)
        self.assertAlmostEqual(link.transfer_ms(0.1, round_trips=2), 16.0)

    def test_pipeline_sums_compute_queue_and_network_cost(self):
        stages = [Stage("local", 4), Stage("remote", 6, outbound_mib=0.1,
                                            rpc_round_trips=1, queue_ms=2)]
        link = Link(rtt_ms=1, throughput_mib_s=100, serialization_ms=0.5)
        self.assertAlmostEqual(pipeline_latency_ms(stages, link), 14.5)

    def test_decode_rate_is_reciprocal_serial_latency(self):
        stages = [Stage("local", 10)]
        self.assertAlmostEqual(decode_tokens_per_second(stages, Link(0, 1)), 100.0)

    def test_declared_memory_budget_respects_reserve(self):
        self.assertTrue(placement_is_feasible(5, 2, 1, 8))
        self.assertFalse(placement_is_feasible(5, 2, 1.01, 8))

    def test_invalid_link_bandwidth_is_rejected(self):
        with self.assertRaises(ValueError):
            Link(rtt_ms=1, throughput_mib_s=0)

    def test_transfer_serialization_is_charged_per_rpc_round_trip(self):
        link = Link(rtt_ms=1, throughput_mib_s=10, serialization_ms=0.5)
        self.assertAlmostEqual(link.transfer_ms(0, round_trips=2), 3.0)

    def test_non_finite_link_parameters_are_rejected(self):
        for values in ((float("nan"), 1), (1, float("inf"))):
            with self.subTest(values=values), self.assertRaises(ValueError):
                Link(rtt_ms=values[0], throughput_mib_s=values[1])

    def test_fractional_round_trip_count_is_rejected(self):
        with self.assertRaises(ValueError):
            Link(1, 10).transfer_ms(0, round_trips=1.5)

    def test_empty_pipeline_is_rejected(self):
        with self.assertRaises(ValueError):
            pipeline_latency_ms([], Link(1, 10))

    def test_non_finite_memory_declarations_are_rejected(self):
        with self.assertRaises(ValueError):
            placement_is_feasible(float("nan"), 2, 1, 8)

    def test_fractional_stage_round_trip_count_is_rejected(self):
        with self.assertRaises(ValueError):
            Stage("remote", 1, rpc_round_trips=0.5)

    def test_invalid_negative_transfer_is_rejected(self):
        with self.assertRaises(ValueError):
            Link(1, 10).transfer_ms(-0.1)


if __name__ == "__main__":
    unittest.main()
