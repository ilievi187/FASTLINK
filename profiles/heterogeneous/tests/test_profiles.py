import json
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(HERE))
import validate_profiles


class ProfileTests(unittest.TestCase):
    def test_all_fixtures_validate_and_cover_required_peer_counts(self):
        profiles = [json.loads(p.read_text()) for p in sorted((HERE / "fixtures").glob("*.json"))]
        self.assertEqual({p["topology"]["node_count"] for p in profiles}, {1, 2, 3, 4, 7})
        for profile in profiles:
            self.assertEqual(validate_profiles.validate(profile), [])

    def test_rejects_duplicate_node_ids(self):
        profile = validate_profiles.load_fixture(1)
        profile["nodes"].append(dict(profile["nodes"][0]))
        self.assertIn("node IDs must be unique", validate_profiles.validate(profile))

    def test_rejects_non_opt_in_peer(self):
        profile = validate_profiles.load_fixture(2)
        profile["nodes"][1]["opt_in"] = False
        self.assertTrue(any("opt-in" in e for e in validate_profiles.validate(profile)))

    def test_rejects_missing_link_edge(self):
        profile = validate_profiles.load_fixture(2)
        profile["links"] = []
        self.assertTrue(any("edge" in e for e in validate_profiles.validate(profile)))

    def test_rejects_unknown_objective(self):
        profile = validate_profiles.load_fixture(1)
        profile["objective"] = "fastest vibes"
        self.assertTrue(any("objective" in e for e in validate_profiles.validate(profile)))

    def test_rejects_hardware_claim_for_synthetic_evidence(self):
        profile = validate_profiles.load_fixture(1)
        profile["provenance"]["status"] = "measured"
        self.assertTrue(any("synthetic" in e or "status" in e for e in validate_profiles.validate(profile)))

    def test_synthetic_fixture_cannot_mark_node_telemetry_measured(self):
        profile = validate_profiles.load_fixture(2)
        profile["nodes"][0]["telemetry"] = "measured"
        self.assertTrue(any("synthetic" in e and "telemetry" in e for e in validate_profiles.validate(profile)))

    def test_70b_arithmetic_is_simulated_and_unknown_telemetry_falls_back(self):
        profiles = [validate_profiles.load_fixture(n) for n in (1, 2, 3, 4, 7)]
        seven = next(p for p in profiles if p["topology"]["node_count"] == 7)
        self.assertEqual(seven["scenario"]["evidence"], "simulated")
        self.assertEqual(seven["scenario"]["model_memory"]["binary_gib"], 130.385160446167)
        unknown = next(p for p in profiles if p["topology"]["node_count"] == 1)
        self.assertEqual(unknown["scenario"]["unknown_telemetry_fallback"], "local_only")

    def test_70b_fp16_weight_arithmetic_uses_binary_gib_correctly(self):
        profile = validate_profiles.load_fixture(7)
        memory = profile["scenario"]["model_memory"]
        self.assertEqual(memory["parameter_count"], 70_000_000_000)
        self.assertEqual(memory["bytes_per_parameter"], 2)
        self.assertEqual(memory["total_bytes"], 140_000_000_000)
        self.assertEqual(memory["decimal_gb"], 140)
        self.assertAlmostEqual(memory["binary_gib"], 130.385160446167)
        self.assertFalse(memory["fit_claim"])
        memory["binary_gib"] = 140
        self.assertTrue(any("consistent arithmetic-only" in error for error in validate_profiles.validate(profile)))

    def test_slow_wifi_phone_excluded_from_decode(self):
        profile = validate_profiles.load_fixture(7)
        phone = next(n for n in profile["nodes"] if n["role"] == "phone")
        self.assertFalse(phone["decode_eligible"])
        self.assertEqual(phone["exclusion_reason"], "slow_wifi")


if __name__ == "__main__":
    unittest.main()
