#!/usr/bin/env python3
"""FASTLINK v4 baseline benchmark runner (hardened).

Runs inference configs (A-F, E=independent parallel replicas) across context sizes,
records deterministic metrics to qa/v4/baseline_results.json.

CPU-ONLY SAFETY: this runner does NOT perform GPU inference by itself during
development/CI checks. Actual benchmarks (server start, model load, GPU work) are
executed by the commander/operator on approved hardware. Use --list / --dry-run to
inspect without starting services. --config execution requires explicit intent and
reachable infrastructure.

Configs:
  A CPU-only, NightKnightPC
  B single GPU RX 7900 XT (NightKnightPC)
  C single GPU RX 6600 XT (detti)
  D hybrid CPU/GPU offload, NightKnightPC
  E two independent replicas (one per host), measured concurrently — INDEPENDENT
    PARALLEL INFERENCE, NOT distributed/layer-split
  F distributed dual-machine inference via FASTLINK v3 RPC (layer split)
"""
from __future__ import annotations
import argparse, hashlib, json, os, pathlib, re, signal, socket, statistics, subprocess, sys, time, urllib.request
from concurrent.futures import ThreadPoolExecutor

ROOT = pathlib.Path(__file__).resolve().parents[1]
V4_DIR = pathlib.Path(__file__).resolve().parent
MODEL = "/mnt/gamez/AI/model-tests/qwen3d-sensenova/Qwen3.5-4B-3D-Coder-Q4_K_M.gguf"
MODEL_SHA256_KNOWN = "32c37c83d3ca055d30aefa679f1a1670d3e2eb889ee7cc805792d53f219cfae8"  # from docs/v4/MEMORY_BUDGET.md
LLAMA_SRV = "/mnt/gamez/AI/llama.cpp-fastlink-rpc-adapter/build-fl-gpu/bin/llama-server"
BENCH = str(V4_DIR / "bench_client.py")
RESULTS = str(V4_DIR / "baseline_results.json")
N_PREDICT = 64
DEFAULT_REPS = 3
CONTEXTS_MATRIX = [8192, 32768, 65536, 131072]


def model_sha256(path=MODEL):
    try:
        if not os.path.isfile(path):
            return {"sha256": None, "provenance": f"file not readable: {path}", "known_from_docs": MODEL_SHA256_KNOWN}
        h = hashlib.sha256()
        with open(path, "rb") as f:
            for b in iter(lambda: f.read(1024 * 1024), b""):
                h.update(b)
        return {"sha256": h.hexdigest(), "provenance": f"computed from {path}", "matches_docs": h.hexdigest() == MODEL_SHA256_KNOWN}
    except Exception as e:
        return {"sha256": None, "provenance": f"hash failed: {e}", "known_from_docs": MODEL_SHA256_KNOWN}


def llama_version(bin_path=LLAMA_SRV):
    for args in ([bin_path, "--version"], [bin_path, "-v"]):
        try:
            r = subprocess.run(args, capture_output=True, text=True, timeout=15)
            out = (r.stdout + "\n" + r.stderr).strip()
            if out:
                return {"ok": True, "version_text": out.splitlines()[0][:300], "raw_head": out[:800]}
        except Exception as e:
            last = str(e)
    return {"ok": False, "version_text": None, "error": locals().get("last", "unavailable"), "bin": bin_path}


def sys_ram_snapshot():
    snap = {}
    try:
        with open("/proc/meminfo") as f:
            for line in f:
                if ":" in line:
                    k, v = line.split(":", 1)
                    if k in ("MemTotal", "MemFree", "MemAvailable", "Buffers", "Cached", "SwapTotal", "SwapFree"):
                        snap[k + "_kB"] = int(v.strip().split()[0])
    except Exception as e:
        snap["error"] = str(e)
    return snap


def gpu_vram_snapshot():
    """Best-effort read-only VRAM query; no GPU workload started."""
    out = {"method": None, "cards": [], "rocm_smi": None}
    try:
        drm = pathlib.Path("/sys/class/drm")
        for card in sorted(drm.glob("card*/device")):
            info = {"card": card.parts[-2]}
            for name in ("mem_info_vram_total", "mem_info_vram_used", "mem_info_gtt_total", "mem_info_gtt_used"):
                p = card / name
                if p.exists():
                    try: info[name + "_B"] = int(p.read_text().strip())
                    except Exception: pass
            if len(info) > 1:
                out["cards"].append(info)
        if out["cards"]:
            out["method"] = "sysfs_drm"
    except Exception as e:
        out["sysfs_error"] = str(e)
    try:
        r = subprocess.run(["rocm-smi", "--showmeminfo", "vram", "--json"], capture_output=True, text=True, timeout=10)
        if r.returncode == 0 and r.stdout.strip():
            out["rocm_smi"] = json.loads(r.stdout)
            if not out["method"]: out["method"] = "rocm-smi"
    except Exception as e:
        out["rocm_smi_error"] = str(e)
    if not out["method"]:
        out["method"] = "unavailable"
    return out


def net_dev_snapshot():
    try:
        rows = {}
        with open("/proc/net/dev") as f:
            for line in f.readlines()[2:]:
                if ":" not in line: continue
                iface, rest = line.split(":", 1)
                cols = rest.split()
                if len(cols) >= 16:
                    rows[iface.strip()] = {"rx_bytes": int(cols[0]), "rx_packets": int(cols[1]), "tx_bytes": int(cols[8]), "tx_packets": int(cols[9])}
        return {"ok": True, "ifaces": rows}
    except Exception as e:
        return {"ok": False, "error": str(e)}


def parse_layer_placement(args_list, host):
    a = [str(x) for x in (args_list or [])]
    info = {"host": host, "ngl": None, "device": None, "tensor_split": None, "split_mode": None, "rpc": None, "local_gpu_layers": None, "remote_gpu_layers": None, "placement_provenance": "parsed from server args; actual loaded layers may differ if --fit adjusts"}
    def getflag(flag):
        if flag in a:
            i = a.index(flag)
            return a[i+1] if i+1 < len(a) else None
        return None
    info["ngl"] = getflag("-ngl") or getflag("--n-gpu-layers")
    info["device"] = getflag("--device") or getflag("-dev")
    info["tensor_split"] = getflag("-ts") or getflag("--tensor-split")
    info["split_mode"] = getflag("-sm") or getflag("--split-mode")
    info["rpc"] = getflag("--rpc")
    # heuristic: layer split 0.70,0.30 => 70% local / 30% remote when distributed
    if info["rpc"] and info["tensor_split"]:
        try:
            parts = [float(x) for x in info["tensor_split"].split(",")]
            info["tensor_split_ratios"] = parts
        except Exception:
            pass
    if info["ngl"] == "99" or info["ngl"] == "999":
        info["local_gpu_layers"] = "all (requested -ngl 99; actual subject to fit/VRAM)"
    elif info["ngl"] == "0":
        info["local_gpu_layers"] = 0
    if info["rpc"]:
        info["remote_gpu_layers"] = "via RPC worker (see rpc endpoint / worker config)"
    return info


def extract_kv_cache(args_list):
    a = [str(x) for x in (args_list or [])]
    def get(flag):
        return a[a.index(flag)+1] if flag in a and a.index(flag)+1 < len(a) else None
    return {"cache_type_k": get("--cache-type-k") or get("-ctk"), "cache_type_v": get("--cache-type-v") or get("-ctv")}


def base_args(ctx):
    return ["-m", MODEL, "--cache-type-k", "q4_0", "--cache-type-v", "q4_0",
            "-fa", "on", "-c", str(ctx), "--threads", "8", "-lv", "2",
            "--host", "127.0.0.1"]


CONFIGS = {
    "A": {"desc": "CPU-only, NightKnightPC", "host": "local", "port": 8210,
          "contexts": [8192, 32768, 65536, 131072],
          "args": lambda ctx: base_args(ctx) + ["--device", "none", "-ngl", "0"]},
    "B": {"desc": "single GPU RX 7900 XT (NightKnightPC, ~3.4GiB free)", "host": "local", "port": 8211,
          "contexts": [8192, 32768],
          "args": lambda ctx: base_args(ctx) + ["--device", "Vulkan0", "--fit", "on", "--fit-target", "0", "-ngl", "99"]},
    "C": {"desc": "single GPU RX 6600 XT (detti, ~7GB free)", "host": "detti", "port": 8212,
          "contexts": [8192, 32768, 65536, 131072],
          "args": lambda ctx: base_args(ctx) + ["--device", "Vulkan0", "-ngl", "99"]},
    "D": {"desc": "hybrid CPU/GPU offload, NightKnightPC", "host": "local", "port": 8213,
          "contexts": [8192, 32768, 65536, 131072],
          "args": lambda ctx: base_args(ctx) + ["--device", "Vulkan0", "--fit", "on", "--fit-target", "0", "-ngl", "99"]},
    "E": {"desc": "two independent replicas (NightKnightPC CPU/small + detti 6600 XT), measured concurrently — INDEPENDENT PARALLEL INFERENCE, NOT distributed/layer-split",
          "host": "multi", "port": 8215, "port_remote": 8216,
          "contexts": [8192, 32768],
          "label_note": "INDEPENDENT PARALLEL INFERENCE (separate requests to separate replicas), NOT distributed layer-split inference",
          "local_args": lambda ctx: base_args(ctx) + ["--device", "none", "-ngl", "0"],
          "remote_args": lambda ctx: ["-m", "C:/Users/dett_/models/Qwen3.5-4B-3D-Coder-Q4_K_M.gguf",
                                       "--cache-type-k", "q4_0", "--cache-type-v", "q4_0",
                                       "-fa", "on", "-c", str(ctx), "--device", "Vulkan0", "-ngl", "99",
                                       "--host", "127.0.0.1"]},
    "F": {"desc": "distributed dual-machine via FASTLINK v3 RPC (layer split 70/30)", "host": "local", "port": 8214,
          "contexts": [8192, 32768, 131072],
          "args": lambda ctx: base_args(ctx) + ["--rpc", "127.0.0.1:15123", "-dev", "Vulkan0,RPC0",
                                              "-sm", "layer", "-ts", "0.70,0.30",
                                              "-ngl", "99", "-t", "4", "-tb", "4", "-b", "128", "-ub", "128"],
          "env": {"GGML_RPC_FASTLINK_RLE": "1"}},
}


def median_record(reps):
    ok = [r for r in reps if r.get("status") == "ok" and r.get("ttft_s") is not None]
    if not ok:
        return {"status": "error", "repetitions": reps, "error": "no successful repetitions"}
    def med(key):
        vals = [r[key] for r in ok if r.get(key) is not None]
        return statistics.median(vals) if vals else None
    # aggregate per-token latency p50/p95 across reps (median of per-run p50/p95)
    def med_itl(k):
        vals = [r.get("inter_token_latency_ms", {}).get(k) for r in ok]
        vals = [v for v in vals if v is not None]
        return round(statistics.median(vals), 3) if vals else None
    m = dict(ok[len(ok)//2])  # representative base
    m["status"] = "ok"
    m["repetitions_count"] = len(reps)
    m["repetitions_ok"] = len(ok)
    m["median"] = {
        "prompt_tokens": med("prompt_tokens"),
        "output_tokens": med("output_tokens"),
        "ttft_s": round(med("ttft_s"), 6) if med("ttft_s") is not None else None,
        "total_s": round(med("total_s"), 6) if med("total_s") is not None else None,
        "prefill_tps": round(med("prefill_tps"), 3) if med("prefill_tps") is not None else None,
        "decode_tps": round(med("decode_tps"), 3) if med("decode_tps") is not None else None,
        "inter_token_latency_ms_p50": med_itl("p50"),
        "inter_token_latency_ms_p95": med_itl("p95"),
    }
    m["repetitions"] = reps
    # top-level convenience
    for k in ("prefill_tps", "decode_tps", "ttft_s", "prompt_tokens", "output_tokens"):
        if m["median"].get(k) is not None:
            m[k] = m["median"][k]
    m["inter_token_latency_ms_p50"] = m["median"]["inter_token_latency_ms_p50"]
    m["inter_token_latency_ms_p95"] = m["median"]["inter_token_latency_ms_p95"]
    return m


# --- execution helpers (used only when actually running benchmarks) ---
def health_ok(port, host="127.0.0.1"):
    try:
        with urllib.request.urlopen(f"http://{host}:{port}/health", timeout=2) as r:
            return r.status == 200
    except Exception:
        return False

def wait_health(port, timeout=240, host="127.0.0.1"):
    t0 = time.time()
    while time.time() - t0 < timeout:
        if health_ok(port, host): return True
        time.sleep(1)
    return False

def ssh_detti(args_list, timeout=120):
    cmd = ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "detti", " ".join(args_list)]
    out = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    return out.returncode, (out.stdout + out.stderr)

def bench_local(port, ctx, np_=N_PREDICT, seed=42):
    out = subprocess.run([sys.executable, BENCH, f"http://127.0.0.1:{port}", str(ctx), str(np_), "--seed", str(seed)],
                         capture_output=True, text=True, timeout=3600)
    if out.returncode != 0:
        raise RuntimeError(f"bench failed: {out.stderr[-600:]}")
    line = out.stdout.strip().splitlines()[-1]
    r = json.loads(line)
    r["status"] = "ok"
    return r

def run_local(cmd, port, env=None, logfile="/tmp/fl4_srv.log"):
    e = dict(os.environ, **(env or {}))
    with open(logfile, "w") as lf:
        proc = subprocess.Popen(cmd, stdout=lf, stderr=subprocess.STDOUT, env=e)
    if not wait_health(port, timeout=240):
        try: proc.send_signal(signal.SIGTERM)
        except Exception: pass
        tail = "".join(open(logfile).readlines()[-10:])
        raise RuntimeError(f"server on :{port} not healthy; tail: {tail[-500:]}")
    return proc

def bench_detti(port, ctx, np_=N_PREDICT):
    rc, out = ssh_detti([f"powershell -NoProfile -ExecutionPolicy Bypass -File C:/Users/dett_/models/fl4_bench.ps1 -port {port} -ctx {ctx} -np {np_}"], timeout=3600)
    if rc != 0:
        raise RuntimeError(f"detti bench failed: {out[-500:]}")
    for line in reversed(out.strip().splitlines()):
        line = line.strip()
        if line.startswith("{"):
            r = json.loads(line); r["status"] = "ok"; return r
    raise RuntimeError("no JSON from detti bench: " + out[-300:])

def dist_setup():
    rc, out = ssh_detti([f"powershell -NoProfile -Command \"if (-not (Get-NetTCPConnection -LocalAddress 127.0.0.1 -LocalPort 50123 -State Listen -ErrorAction SilentlyContinue)) {{ Start-Process -FilePath C:/Users/dett_/llama-rpc/ggml-rpc-server.exe -ArgumentList '-d','Vulkan0','127.0.0.1:50123' -RedirectStandardError C:/Users/dett_/models/fl4_rpc.log -PassThru | ForEach-Object {{ $_.Id | Out-File C:/Users/dett_/models/fl4_rpc.pid }} }}; 'rpc_started'\""], timeout=120)
    if rc != 0: raise RuntimeError(f"rpc start failed: {out[-300:]}")
    time.sleep(6)
    if not health_ok(15123):
        subprocess.Popen(["ssh", "-f", "-N", "-L", "127.0.0.1:15123:127.0.0.1:50123",
                          "-o", "BatchMode=yes", "-o", "ConnectTimeout=6", "-o", "ExitOnForwardFailure=yes", "detti"])
        time.sleep(4)
    return True

def dist_teardown():
    ssh_detti([f"powershell -NoProfile -Command \"if (Test-Path C:/Users/dett_/models/fl4_rpc.pid) {{ $p=(Get-Content C:/Users/dett_/models/fl4_rpc.pid|Select -First 1).Trim(); if ($p -match '^[0-9]+$') {{ Stop-Process -Id $p -Force -ErrorAction SilentlyContinue }} }}\""], timeout=60)
    subprocess.run(["pkill", "-f", "ssh -f -N -L 127.0.0.1:15123"], check=False)


def capture_config_f_net(before, after, out_tokens):
    """Best-effort network bytes for config F (distributed RPC tunnel 15123)."""
    res = {"method": "proc_net_dev_delta_best_effort", "provenance": "captures host /proc/net/dev delta around benchmark; attributes to total system delta (may include unrelated traffic). RPC call count/FLR3 wire metadata not exposed via llama-server HTTP in this harness; record unavailable if not parseable from logs."}
    try:
        bsum = sum(v["tx_bytes"]+v["rx_bytes"] for v in before.get("ifaces",{}).values())
        asum = sum(v["tx_bytes"]+v["rx_bytes"] for v in after.get("ifaces",{}).values())
        res["host_total_bytes_delta"] = asum - bsum
        if out_tokens: res["bytes_per_generated_token_host_total_delta"] = round((asum-bsum)/max(1,out_tokens), 3)
        res["rpc_call_count"] = {"value": None, "provenance": "unavailable: no llama-server/RPC HTTP counter exposed; requires log parsing or instrumented adapter"}
        res["flr3_wire_accounting"] = {"value": None, "provenance": "unavailable in current harness path; see RPC adapter GGML_RPC_FASTLINK_PACK/FLR3 codec metrics (docs/FASTLINK_RPC_OVERHEAD.md) — runtime per-request capture not wired"}
    except Exception as e:
        res["error"] = str(e)
    return res


def run_one_rep_local(port, ctx, seed):
    return bench_local(port, ctx, N_PREDICT, seed=seed)

def run_one_rep_detti(port, ctx):
    # detti bench script manages seed internally? we pass ctx/np; record provenance
    r = bench_detti(port, ctx, N_PREDICT)
    r.setdefault("seed_note", "detti-side fl4_bench.ps1 seed behavior as implemented on detti")
    return r


def run_config(name, ctx=None, repetitions=DEFAULT_REPS, dry_run=False, compute_model_hash=False):
    if name not in CONFIGS:
        return [{"config": name, "status": "error", "error": f"unknown config {name}"}]
    cfg = CONFIGS[name]
    ctxs = [ctx] if ctx else cfg["contexts"]
    results = []
    # static capture (safe)
    static = {
        "config": name, "desc": cfg["desc"], "host": cfg["host"],
        "llama_server_bin": LLAMA_SRV, "bench_client": BENCH,
        "model_path": MODEL,
        "model_sha256": model_sha256() if compute_model_hash else {"sha256": MODEL_SHA256_KNOWN, "provenance": "known from docs/v4/MEMORY_BUDGET.md (hash not recomputed this run; use --compute-model-hash to verify)", "known_from_docs": MODEL_SHA256_KNOWN},
        "llama_cpp_version": llama_version(),
        "n_predict": N_PREDICT, "seed": 42, "deterministic_prompt": "SEED_TEXT repeated/detokenized to (target_context - n_predict) tokens; see bench_client.build_prompt",
        "repetitions_target": repetitions,
    }
    for c in ctxs:
        rec = dict(static)
        rec.update({"context": c, "target_context_tokens": c})
        # resolve exact args
        if name == "E":
            largs = cfg["local_args"](c) + ["--port", str(cfg["port"])]
            rargs = cfg["remote_args"](c) + ["--port", str(cfg["port_remote"])]
            rec["server_args_local"] = [LLAMA_SRV] + largs
            rec["server_args_remote_detti"] = ["llama-server (detti path)"] + rargs
            rec["kv_cache"] = extract_kv_cache(largs)
            rec["layer_placement_local"] = parse_layer_placement(largs, "local")
            rec["layer_placement_remote"] = parse_layer_placement(rargs, "detti")
            rec["label"] = cfg["label_note"]
        elif cfg["host"] == "local":
            sargs = [LLAMA_SRV] + cfg["args"](c) + ["--port", str(cfg["port"])]
            rec["server_args"] = sargs
            rec["kv_cache"] = extract_kv_cache(sargs)
            rec["layer_placement"] = parse_layer_placement(sargs, "local")
            rec["server_env"] = cfg.get("env")
        else:
            sargs = base_args(c) + ["--device", "Vulkan0", "-ngl", "99", "--port", str(cfg["port"])]
            rec["server_args_detti"] = sargs
            rec["kv_cache"] = extract_kv_cache(sargs)
            rec["layer_placement"] = parse_layer_placement(sargs, "detti")
        # matrix validation metadata
        rec["matrix_validation"] = {"target_contexts": [8192,32768,65536,131072], "this_target": c, "prompt_target_tokens": max(128, c - N_PREDICT), "n_predict_reserved": N_PREDICT, "deterministic_seed": 42, "note": "actual prompt occupancy validated per repetition by bench_client.prompt_occupancy_ok"}

        if dry_run:
            rec["status"] = "dry_run"
            rec["note"] = "no server started, no inference (CPU-only safe plan)"
            results.append(rec)
            print(f"[{name} ctx={c}] dry_run", flush=True)
            continue

        try:
            ram_before = sys_ram_snapshot(); vram_before = gpu_vram_snapshot(); net_before = net_dev_snapshot() if name=="F" else None
            if name == "E":
                # INDEPENDENT PARALLEL INFERENCE, NOT distributed
                # This requires complex multi-host orchestration; capture structure and
                # attempt best-effort if infra reachable (operator-driven).
                rec["status"] = "not_executed"
                rec["error"] = "config E execution requires starting independent replicas on NightKnightPC and detti concurrently; operator/commander must run with infrastructure available. Harness supports the config definition and dry-run planning. CPU-only agent did not start GPU/remote services."
                rec["execution_note"] = cfg["label_note"]
            elif cfg["host"] == "local":
                if name == "F": dist_setup()
                cmd = [LLAMA_SRV] + cfg["args"](c) + ["--port", str(cfg["port"])]
                proc = run_local(cmd, cfg["port"], env=cfg.get("env"))
                try:
                    reps = []
                    for i in range(repetitions):
                        try: reps.append(run_one_rep_local(cfg["port"], c, seed=42+i))
                        except Exception as e: reps.append({"status":"error","error":str(e)[:400],"rep":i})
                    rec.update(median_record(reps))
                finally:
                    try: proc.send_signal(signal.SIGTERM)
                    except Exception: pass
                    for _ in range(30):
                        if proc.poll() is not None: break
                        time.sleep(0.5)
                    if proc.poll() is None:
                        try: proc.kill()
                        except Exception: pass
                if name == "F": dist_teardown()
            else:
                reps = []
                for i in range(repetitions):
                    try: reps.append(run_one_rep_detti(cfg["port"], c))
                    except Exception as e: reps.append({"status":"error","error":str(e)[:400],"rep":i})
                rec.update(median_record(reps))
            ram_after = sys_ram_snapshot(); vram_after = gpu_vram_snapshot(); net_after = net_dev_snapshot() if name=="F" else None
            rec["system_ram_before"] = ram_before; rec["system_ram_after"] = ram_after
            rec["gpu_vram_before"] = vram_before; rec["gpu_vram_after"] = vram_after
            if name == "F":
                rec["network_config_f"] = capture_config_f_net(net_before or {}, net_after or {}, rec.get("output_tokens"))
        except Exception as e:
            rec.setdefault("status", "error")
            rec["error"] = str(e)[:600]
        results.append(rec)
        keys = ("status","prompt_tokens","output_tokens","ttft_s","prefill_tps","decode_tps","inter_token_latency_ms_p50","inter_token_latency_ms_p95","error")
        print(f"[{name} ctx={c}] " + json.dumps({k: rec[k] for k in keys if k in rec}), flush=True)
    # persist (merge)
    data = {}
    if os.path.exists(RESULTS):
        try: data = json.load(open(RESULTS))
        except Exception: data = {}
    data.setdefault("results", {})
    for c, r in zip(ctxs, results):
        data["results"][f"{name}_{c}"] = r
    data.setdefault("harness", {})["name"] = "qa/v4/baseline_runner.py (hardened, FL4-HARN)"
    data["updated_at"] = time.strftime("%Y-%m-%dT%H:%M:%S%z")
    json.dump(data, open(RESULTS, "w"), indent=2)
    return results


def main():
    ap = argparse.ArgumentParser(description="FASTLINK v4 baseline runner (hardened)")
    ap.add_argument("--config", nargs="+", default=["C"], help="configs to run (A-F) or 'all'")
    ap.add_argument("--ctx", type=int, default=None)
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--dry-run", action="store_true", help="plan/capture static metadata only; no servers, no inference")
    ap.add_argument("--repetitions", type=int, default=DEFAULT_REPS)
    ap.add_argument("--compute-model-hash", action="store_true")
    a = ap.parse_args()
    if a.list:
        for k in sorted(CONFIGS):
            v = CONFIGS[k]
            extra = " [INDEPENDENT PARALLEL, NOT distributed]" if k=="E" else ""
            print(f"{k}: {v['desc']}  host={v['host']} ctxs={v['contexts']}{extra}")
        print(f"matrix: {CONTEXTS_MATRIX} target tokens; n_predict={N_PREDICT}; repetitions(default)={DEFAULT_REPS}; deterministic seed base=42")
        return 0
    names = list(CONFIGS.keys()) if "all" in a.config else a.config
    for name in names:
        if name not in CONFIGS:
            print(f"unknown config {name} (known: {','.join(sorted(CONFIGS))})", file=sys.stderr)
            continue
        print(f"=== config {name}: {CONFIGS[name]['desc']} ===", flush=True)
        run_config(name, ctx=a.ctx, repetitions=max(1,a.repetitions), dry_run=a.dry_run, compute_model_hash=a.compute_model_hash)
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
