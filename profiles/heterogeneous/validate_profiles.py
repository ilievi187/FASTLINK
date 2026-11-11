"""Validate deterministic heterogeneous profile fixtures (stdlib only)."""
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent


def load_fixture(count):
    return json.loads((ROOT / "fixtures" / f"n{count}.json").read_text())


def validate(p):
    errors = []
    if p.get("schema_version") != 1:
        errors.append("schema_version must be 1")
    n = p.get("topology", {}).get("node_count")
    nodes = p.get("nodes", [])
    ids = [x.get("id") for x in nodes]
    if len(ids) != len(set(ids)):
        errors.append("node IDs must be unique")
    if len(nodes) != n:
        errors.append("peer count must equal topology node_count")
    if p.get("objective") not in {"minimize_latency", "maximize_throughput"}:
        errors.append("invalid objective")
    prov = p.get("provenance", {})
    if prov.get("status") != "synthetic" or prov.get("hardware_measurement") is not False:
        errors.append("synthetic fixture must not claim hardware measurement")
    if not prov.get("source"):
        errors.append("provenance source required")
    if any(x.get("opt_in") is not True for x in nodes):
        errors.append("all nodes require explicit opt-in")
    expected = n * (n - 1) // 2
    links = p.get("links", [])
    if len(links) != expected:
        errors.append("link edge count must describe complete topology")
    valid_ids = set(ids)
    seen = set()
    for edge in links:
        pair = frozenset((edge.get("source"), edge.get("target")))
        if len(pair) != 2 or not pair <= valid_ids:
            errors.append("link edge references invalid nodes")
        if pair in seen:
            errors.append("duplicate link edge")
        seen.add(pair)
    scenario = p.get("scenario", {})
    if scenario.get("evidence") not in {"simulated", "predicted", "unknown"}:
        errors.append("invalid evidence label")
    memory = scenario.get("model_memory", {})
    if memory.get("basis") != "arithmetic_only" or memory.get("total_gib") != 140:
        errors.append("70B memory must be labeled arithmetic_only and total 140 GiB")
    if scenario.get("unknown_telemetry_fallback") != "local_only":
        errors.append("unknown telemetry must fall back to local_only")
    for node in nodes:
        if node.get("telemetry") not in {"unknown", "simulated", "measured"}:
            errors.append("invalid telemetry label")
        if node.get("role") == "phone" and node.get("decode_eligible") and node.get("link_class") == "slow_wifi":
            errors.append("slow Wi-Fi phone cannot be decode eligible")
        if node.get("role") == "phone" and node.get("exclusion_reason") == "slow_wifi" and node.get("decode_eligible"):
            errors.append("slow Wi-Fi phone must be excluded from decode")
    return errors


def main():
    failures = 0
    for path in sorted((ROOT / "fixtures").glob("*.json")):
        errors = validate(json.loads(path.read_text()))
        if errors:
            failures += 1
            print(f"{path}: " + "; ".join(errors))
        else:
            print(f"{path}: valid")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
