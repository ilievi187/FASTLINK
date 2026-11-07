#!/usr/bin/env python3
"""Machine-readable/console summary for FASTLINK v4 baseline_results.json.

Reads qa/v4/baseline_results.json and prints a table of prefill_tps, decode_tps,
ttft, prompt_tokens, output_tokens per config+context. No crash on missing configs
or fields (prints placeholders).

Usage:
  python3 qa/v4/summary.py [path/to/baseline_results.json]
  python3 qa/v4/summary.py --json   # emit machine-readable JSON summary
"""
from __future__ import annotations
import json, os, pathlib, sys

DEFAULT = pathlib.Path(__file__).with_name("baseline_results.json")
COLS = ["prefill_tps", "decode_tps", "ttft_s", "prompt_tokens", "output_tokens"]


def load(path):
    try:
        with open(path) as f:
            return json.load(f)
    except FileNotFoundError:
        return {"_error": f"file not found: {path}", "results": {}}
    except Exception as e:
        return {"_error": f"failed to read {path}: {e}", "results": {}}


def pick(rec, key):
    if not isinstance(rec, dict):
        return None
    # prefer median/top-level hardened fields, fall back to nested
    if key in rec and rec[key] is not None:
        return rec[key]
    med = rec.get("median") or {}
    if key in med and med[key] is not None:
        return med[key]
    # legacy names
    if key == "ttft_s":
        for alt in ("ttft", "ttft_s"):
            if rec.get(alt) is not None: return rec[alt]
    return None


def fmt(v):
    if v is None:
        return "-"
    if isinstance(v, float):
        return f"{v:.3f}".rstrip("0").rstrip(".")
    return str(v)


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("-")]
    emit_json = "--json" in sys.argv
    path = pathlib.Path(args[0]) if args else DEFAULT
    data = load(path)
    results = data.get("results") or {}
    # stable order: A-F, E included, then numeric context
    def sort_key(k):
        cfg = k.split("_")[0] if "_" in k else k
        try: ctx = int(k.rsplit("_",1)[1])
        except Exception: ctx = 0
        return (cfg, ctx)
    keys = sorted(results.keys(), key=sort_key)
    rows = []
    for k in keys:
        rec = results.get(k) or {}
        status = rec.get("status", "?")
        row = {"config_ctx": k, "status": status}
        for c in COLS:
            row[c] = pick(rec, c)
        # before/after placeholders if present in future schema
        row["before_prefill_tps"] = rec.get("before", {}).get("prefill_tps") if isinstance(rec.get("before"), dict) else None
        row["after_prefill_tps"] = rec.get("after", {}).get("prefill_tps") if isinstance(rec.get("after"), dict) else rec.get("v4_after", {}).get("prefill_tps") if isinstance(rec.get("v4_after"), dict) else None
        row["note"] = (rec.get("error") or "")[:120] if status != "ok" else ""
        rows.append(row)
    if emit_json:
        out = {"source": str(path), "updated_at": data.get("updated_at"), "load_error": data.get("_error"), "rows": rows}
        print(json.dumps(out, indent=2, sort_keys=True))
        return 0
    # console table
    print(f"FASTLINK v4 summary — source: {path}")
    if data.get("_error"):
        print(f"WARNING: {data['_error']}")
    print(f"updated_at: {data.get('updated_at', '-')}")
    headers = ["config_ctx", "status"] + COLS + ["note"]
    widths = {h: len(h) for h in headers}
    for r in rows:
        for h in headers:
            widths[h] = max(widths[h], len(fmt(r.get(h))))
    def line(r):
        return "  ".join(fmt(r.get(h)).ljust(widths[h]) for h in headers)
    print(line({h:h for h in headers}))
    print(line({h:"-"*widths[h] for h in headers}))
    if not rows:
        print("(no result rows — missing configs/empty results handled)")
    for r in rows:
        print(line(r))
    print("\nColumns: prefill_tps, decode_tps, ttft_s, prompt_tokens, output_tokens (median/top-level; '-' = missing/unavailable). Config E = independent parallel inference (NOT distributed).")
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
