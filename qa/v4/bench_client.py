#!/usr/bin/env python3
"""Deterministic llama-server benchmark client (pure stdlib; Linux + Windows).

Usage:
  python3 bench_client.py <endpoint> <target_prompt_tokens> <n_predict> [--seed 42] [--json]

Builds a deterministic prompt of ~exactly (target_prompt_tokens - n_predict) tokens
(leaving room for n_predict), validates actual prompt occupancy, runs one streaming
completion, and prints JSON metrics including per-token decode latency (p50/p95).
"""
from __future__ import annotations
import argparse, json, statistics, sys, time, urllib.request

SEED_TEXT = ("The quick brown fox jumps over the lazy dog near the river bank while the wind "
             "blows softly across the open meadow at the edge of the quiet valley. " * 400)
DEFAULT_SEED = 42


def _post(ep, path, payload, timeout=1800):
    data = json.dumps(payload).encode()
    req = urllib.request.Request(ep.rstrip('/') + path, data=data,
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def tokenize(text, ep):
    return _post(ep, "/tokenize", {"content": text})["tokens"]


def detokenize(tokens, ep):
    return _post(ep, "/detokenize", {"tokens": tokens})["content"]


def build_prompt(ep, target, n_predict, tolerance=0.02):
    """Deterministic prompt targeting (target - n_predict) tokens, leaving room for output.
    Returns (prompt, actual_prompt_tokens, prompt_target_tokens, occupancy_ok).
    """
    prompt_target = max(128, int(target) - int(n_predict))
    per = max(1, len(tokenize(SEED_TEXT, ep)))
    reps = max(1, (prompt_target // per) + 2)
    full = SEED_TEXT * reps
    toks = tokenize(full, ep)
    if len(toks) > prompt_target:
        toks = toks[:prompt_target]
        full = detokenize(toks, ep)
    actual = len(tokenize(full, ep))
    # accept small tokenizer roundtrip drift
    occ_ok = abs(actual - prompt_target) <= max(4, int(prompt_target * tolerance))
    return full, actual, prompt_target, occ_ok


def _percentile(sorted_vals, p):
    if not sorted_vals:
        return None
    k = (len(sorted_vals) - 1) * (p / 100.0)
    f = int(k); c = min(f + 1, len(sorted_vals) - 1)
    if f == c:
        return sorted_vals[f]
    return sorted_vals[f] * (c - k) + sorted_vals[c] * (k - f)


def bench(ep, prompt, n_predict, seed=DEFAULT_SEED):
    t0 = time.perf_counter()
    data = json.dumps({"prompt": prompt, "n_predict": n_predict, "stream": True,
                       "temperature": 0.0, "seed": seed, "cache_prompt": False}).encode()
    req = urllib.request.Request(ep.rstrip('/') + "/completion", data=data,
                                 headers={"Content-Type": "application/json"})
    ttft = None
    token_times = []  # perf_counter timestamps per emitted token
    toks = []
    with urllib.request.urlopen(req, timeout=3600) as r:
        for raw in r:
            line = raw.decode(errors="replace").strip()
            if not line.startswith("data:"):
                continue
            payload = line[5:].strip()
            if payload == "[DONE]":
                break
            obj = json.loads(payload)
            if obj.get("stop"):
                # may include final timing; still count content if present
                content = obj.get("content") or ""
                if content:
                    toks.append(content)
                    token_times.append(time.perf_counter())
                break
            content = obj.get("content")
            if content is None:
                content = obj.get("token_str") or ""
            if content != "" or not toks:  # count token events
                now = time.perf_counter()
                toks.append(content)
                token_times.append(now)
                if ttft is None:
                    ttft = now - t0
    total = time.perf_counter() - t0
    n_out = len(toks)
    prompt_tokens = len(tokenize(prompt, ep))
    # inter-token latencies (ms) for decode phase: gaps between consecutive emitted tokens
    itl_ms = []
    if len(token_times) >= 2:
        for a, b in zip(token_times, token_times[1:]):
            itl_ms.append((b - a) * 1000.0)
    itl_sorted = sorted(itl_ms)
    decode_time = (token_times[-1] - token_times[0]) if len(token_times) >= 2 else 0.0
    out = {
        "prompt_tokens": prompt_tokens,
        "output_tokens": n_out,
        "ttft_s": round(ttft, 6) if ttft is not None else None,
        "total_s": round(total, 6),
        "prefill_tps": round(prompt_tokens / ttft, 3) if ttft else 0.0,
        "decode_tps": round((n_out - 1) / decode_time, 3) if decode_time > 0 and n_out > 1 else 0.0,
        "decode_time_s": round(decode_time, 6),
        "inter_token_latency_ms": {
            "count": len(itl_ms),
            "p50": round(_percentile(itl_sorted, 50), 3) if itl_sorted else None,
            "p95": round(_percentile(itl_sorted, 95), 3) if itl_sorted else None,
            "mean": round(statistics.mean(itl_ms), 3) if itl_ms else None,
            "min": round(min(itl_ms), 3) if itl_ms else None,
            "max": round(max(itl_ms), 3) if itl_ms else None,
        },
        "seed": seed,
        "output_head": "".join(toks)[:240],
    }
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("endpoint")
    ap.add_argument("target_prompt_tokens", type=int)
    ap.add_argument("n_predict", type=int)
    ap.add_argument("--seed", type=int, default=DEFAULT_SEED)
    ap.add_argument("--tolerance", type=float, default=0.02)
    ap.add_argument("--json", action="store_true", default=True)
    a = ap.parse_args()
    prompt, actual, prompt_target, occ_ok = build_prompt(a.endpoint, a.target_prompt_tokens, a.n_predict, a.tolerance)
    res = bench(a.endpoint, prompt, a.n_predict, seed=a.seed)
    res["prompt_target_tokens"] = prompt_target
    res["target_context_tokens"] = a.target_prompt_tokens
    res["prompt_occupancy_ok"] = occ_ok
    res["prompt_occupancy_note"] = f"actual={actual} target={prompt_target} (target_context={a.target_prompt_tokens} minus n_predict={a.n_predict}, deterministic seed text)"
    print(json.dumps(res, sort_keys=True))


if __name__ == "__main__":
    main()
