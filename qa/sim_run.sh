#!/bin/sh
# FL3-C1..C3 reproducible runner: deterministic, CPU-only, no network.
# Usage: sh qa/sim_run.sh          (from the repository root)
#        cd qa && sh sim_run.sh    (works too; the script self-locates)
#
# Each simulation prints its full numeric tables when run directly:
#   cd qa && python3 sim_placement.py
#   cd qa && python3 sim_pipeline.py
#   cd qa && python3 sim_faults.py
# Output is a deterministic function of the declared inputs only (no
# wall-clock, no hardware probes).
set -e
cd "$(dirname "$0")"

echo "### python simulation smoke (deterministic re-runs)"
for sim in sim_placement sim_pipeline sim_faults; do
    python3 "$sim.py" > /dev/null
    echo "ok: $sim ran without error (deterministic output)"
done

echo
echo "### model unit tests"
python3 -m unittest \
    test_cost_model test_network_sim \
    test_sim_placement test_sim_pipeline test_sim_faults \
    2>&1 | tail -3
