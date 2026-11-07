#!/usr/bin/env python3
"""FL3-E5: sustained load + memory stability + 4-session concurrent stress probe.

CPU-only, loopback-only, deterministic, re-runnable.
  - 50 sequential 1 MiB transfers: cold pass then warm pass (100 transfers),
    per-transfer latency, server RSS (VmRSS kB) sampled after each transfer.
  - Asserts: all byte-exact; warm chunks_transferred==0; no unbounded RSS growth
    (reported factually; bounded if max(RSS after warmup) - min(RSS after warmup) <= 32 MiB).
  - 4-session concurrent stress: attempted against cpu-baseline branch binary if
    buildable/readable (read-only; no edits). Reported as proven/skipped with reason.

Usage (from network-sim worktree root):
  python3 qa/e5_harness.py [--results qa/results/e5_<ts>.json]
"""
from __future__ import annotations
import argparse, hashlib, json, os, random, signal, socket, subprocess, sys, tempfile, time
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path
from typing import Any

NETSIM = Path(__file__).resolve().parents[1]
CPU_BASELINE = NETSIM.parent / "fastlink-v3-cpu-baseline"  # /.../FASTLINK/.worktrees/fastlink-v3-cpu-baseline
QA_WT = NETSIM.parent.parent  # /home/nightknight/Projects/FASTLINK
SEED = 0xE5E5
N_SEQ = 50
FILE_BYTES = 1024 * 1024  # 1 MiB
CHUNK = 64 * 1024
RSS_BOUND_KB = 32 * 1024  # 32 MiB growth bound (factual assertion threshold)

def sha256_file(p: Path) -> str:
    h = hashlib.sha256()
    with p.open('rb') as f:
        for b in iter(lambda: f.read(1<<20), b''): h.update(b)
    return h.hexdigest()

def make_file(path: Path, size: int, seed: int):
    rng = random.Random(seed)
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open('wb') as f:
        rem = size
        while rem:
            n = min(rem, 65536)
            # mixed pattern (deterministic)
            buf = bytearray(n)
            for i in range(0, n, 256):
                v = rng.randrange(256); run = min(256, n-i)
                if rng.random() < 0.4: buf[i:i+run] = bytes([v])*run
                else: buf[i:i+run] = rng.randbytes(run)
            f.write(buf); rem -= n

def free_port():
    with socket.socket() as s: s.bind(('127.0.0.1',0)); return s.getsockname()[1]

def rss_kb(pid: int) -> int|None:
    try:
        for line in Path(f'/proc/{pid}/status').read_text().splitlines():
            if line.startswith('VmRSS:'): return int(line.split()[1])  # kB
    except Exception: pass
    return None

def wait_port(port, timeout=10):
    d=time.time()+timeout
    while time.time()<d:
        try:
            with socket.create_connection(('127.0.0.1',port),timeout=0.2): return True
        except OSError: time.sleep(0.03)
    return False

def start_server(binary: Path, port: int, data_dir: Path, cwd: Path):
    data_dir.mkdir(parents=True, exist_ok=True)
    return subprocess.Popen([str(binary),'serve',f'127.0.0.1:{port}',str(data_dir)],
                           cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, start_new_session=True)

def stop_server(proc):
    try: os.killpg(proc.pid, signal.SIGTERM)
    except Exception: pass
    try: proc.communicate(timeout=5)
    except Exception:
        try: os.killpg(proc.pid, signal.SIGKILL)
        except Exception: pass

def run_send(binary: Path, file_path: Path, port: int, cwd: Path, timeout=120) -> dict[str,Any]:
    t0=time.perf_counter()
    cp=subprocess.run([str(binary),'send',str(file_path),f'127.0.0.1:{port}'], cwd=cwd, capture_output=True, text=True, timeout=timeout)
    wall_ms=(time.perf_counter()-t0)*1000
    out=(cp.stdout or '')+'\n'+(cp.stderr or '')
    fields={}
    for line in out.splitlines():
        if ':' in line:
            k,_,v=line.partition(':'); k=k.strip().lstrip(' .'); v=v.strip()
            if k in {'chunks_transferred','chunks_reused','file_bytes','elapsed_ms','encoded_payload_bytes'}:
                try: fields[k]=int(v)
                except: fields[k]=v
    return {'ok':cp.returncode==0,'rc':cp.returncode,'wall_ms':round(wall_ms,3),
            'chunks_transferred':fields.get('chunks_transferred'),'output_tail':out.strip()[-400:]}

def build_release(cwd: Path) -> Path|None:
    try:
        subprocess.run(['cargo','build','--release','--quiet'], cwd=cwd, check=True, capture_output=True, text=True, timeout=600)
        b=cwd/'target'/'release'/'fastlink'
        return b if b.exists() else None
    except Exception as e:
        return None

def sustained(netsim_bin: Path, tmp: Path) -> dict[str,Any]:
    # 50 distinct files, deterministic
    srcs=[]
    for i in range(N_SEQ):
        p=tmp/'src'/f'e5-{i:03d}.bin'; make_file(p, FILE_BYTES, SEED+i); srcs.append((p, sha256_file(p)))
    port=free_port(); cache=tmp/'cache'
    srv=start_server(netsim_bin, port, cache, NETSIM)
    if not wait_port(port): stop_server(srv); return {'status':'error','reason':'server start failed'}
    rss_samples=[]; passes={}
    try:
        for phase in ('cold','warm'):
            rows=[]; ok_count=0; byte_ok=0
            for i,(p,dig) in enumerate(srcs):
                r=run_send(netsim_bin,p,port,NETSIM)
                out=cache/'outputs'/p.name
                od=sha256_file(out) if out.exists() else None
                be=(od==dig); r['byte_exact']=be; r['idx']=i; r['phase']=phase
                if r['ok']: ok_count+=1
                if be: byte_ok+=1
                rss=rss_kb(srv.pid); rss_samples.append({'phase':phase,'i':i,'rss_kb':rss})
                r['server_rss_kb']=rss
                rows.append(r)
            passes[phase]={
                'n':N_SEQ,'ok':ok_count,'byte_exact':byte_ok,
                'all_ok': ok_count==N_SEQ and byte_ok==N_SEQ,
                'all_zero_transferred': all(r.get('chunks_transferred')==0 for r in rows) if phase=='warm' else None,
                'latency_ms': {'min':min(r['wall_ms'] for r in rows),'max':max(r['wall_ms'] for r in rows),
                               'mean':round(sum(r['wall_ms'] for r in rows)/len(rows),3),
                               'p50':sorted(r['wall_ms'] for r in rows)[len(rows)//2]},
                'per_transfer': rows, # full detail (deterministic, useful)
            }
    finally:
        stop_server(srv)
    # RSS analysis (ignore first 2 samples as warmup noise)
    rss_vals=[s['rss_kb'] for s in rss_samples if s['rss_kb'] is not None]
    stable=rss_vals[2:] if len(rss_vals)>4 else rss_vals
    rss_min=min(stable) if stable else None; rss_max=max(stable) if stable else None
    rss_delta=(rss_max-rss_min) if (rss_min is not None) else None
    bounded=(rss_delta is not None and rss_delta <= RSS_BOUND_KB)
    return {
        'status':'ok',
        'params':{'n_seq':N_SEQ,'file_bytes':FILE_BYTES,'seed':SEED,'passes':'cold_then_warm (100 transfers)','rss_bound_kb':RSS_BOUND_KB},
        'passes':passes,
        'rss':{'samples':rss_samples,'min_kb':rss_min,'max_kb':rss_max,'delta_kb':rss_delta,
               'bound_kb':RSS_BOUND_KB,'bounded':bounded,
               'note':'VmRSS of server process sampled after each transfer; first 2 samples excluded from bound calc'},
        'summary':{
            'cold_all_byte_exact':passes['cold']['all_ok'],
            'warm_all_byte_exact':passes['warm']['all_ok'],
            'warm_all_zero_transferred':passes['warm']['all_zero_transferred'],
            'rss_bounded':bounded,
            'all_pass': passes['cold']['all_ok'] and passes['warm']['all_ok'] and passes['warm']['all_zero_transferred'] and bounded,
        }
    }

def concurrent_stress_cpu_baseline(tmp: Path) -> dict[str,Any]:
    # Read-only check of cpu-baseline; build may take time but does not edit source.
    if not CPU_BASELINE.exists():
        return {'status':'skipped','reason':f'cpu-baseline worktree not found at {CPU_BASELINE}'}
    try:
        head=subprocess.check_output(['git','rev-parse','--short','HEAD'], cwd=CPU_BASELINE, text=True).strip()
        log=subprocess.check_output(['git','log','--oneline','-3'], cwd=CPU_BASELINE, text=True).strip()
    except Exception as e:
        return {'status':'skipped','reason':f'git inspect failed: {e}'}
    # Check for A4/concurrent support signals
    server_rs=CPU_BASELINE/'src'/'server.rs'
    has_concurrent = server_rs.exists() and 'max_concurrent_sessions' in server_rs.read_text()
    a4_committed = 'FL3-A4' in log or 'concurrent' in log.lower().split('\n')[0]
    info={'cpu_baseline_head':head,'recent_log':log.splitlines(),'has_concurrent_infra_in_src':has_concurrent,'a4_commit_observed_in_head3':a4_committed}
    # Attempt build+run only if infra present; report honestly
    binp=build_release(CPU_BASELINE)
    if not binp:
        return {'status':'skipped','reason':'cpu-baseline release build failed or unavailable','info':info}
    # 4 distinct files, 4 parallel sends
    srcs=[]
    for i in range(4):
        p=tmp/'csrc'/f'c{i}.bin'; make_file(p, FILE_BYTES, SEED+0xC0+i); srcs.append((p,sha256_file(p)))
    port=free_port(); cache=tmp/'ccache'
    srv=start_server(binp, port, cache, CPU_BASELINE)
    if not wait_port(port,15): stop_server(srv); return {'status':'error','reason':'cpu-baseline server start failed','info':info}
    results=[]
    try:
        with ThreadPoolExecutor(max_workers=4) as ex:
            futs={ex.submit(run_send, binp, p, port, CPU_BASELINE, 180): (i,p,d) for i,(p,d) in enumerate(srcs)}
            for fut in as_completed(futs):
                i,p,d=futs[fut]
                try: r=fut.result()
                except Exception as e: r={'ok':False,'error':str(e)}
                out=cache/'outputs'/p.name; od=sha256_file(out) if out.exists() else None
                r.update({'idx':i,'src_sha256':d,'out_sha256':od,'byte_exact':od==d})
                results.append(r)
    finally:
        stop_server(srv)
    results.sort(key=lambda x:x['idx'])
    all_be=all(r.get('byte_exact') for r in results)
    all_ok=all(r.get('ok') for r in results)
    return {'status':'ok','info':info,'n':4,'results':[{k:v for k,v in r.items() if k!='output_tail'}|{'output_tail':r.get('output_tail','')[-200:]} for r in results],
            'summary':{'all_ok':all_ok,'all_byte_exact':all_be,'all_pass':all_ok and all_be},
            'honest_note':'Tested against cpu-baseline release binary at recorded HEAD (read-only build). A4 commit may be pending; result reflects actual concurrent behavior of that binary.'}

def main():
    ap=argparse.ArgumentParser(); ap.add_argument('--results',default=None)
    a=ap.parse_args()
    ts=time.strftime('%Y%m%dT%H%M%SZ',time.gmtime())
    results_path=Path(a.results) if a.results else NETSIM/'qa'/'results'/f'e5_{ts}.json'
    results_path.parent.mkdir(parents=True, exist_ok=True)
    netsim_bin=build_release(NETSIM)
    if not netsim_bin: print('FATAL: netsim build failed',file=sys.stderr); return 2
    commit=subprocess.check_output(['git','rev-parse','--short','HEAD'],cwd=NETSIM,text=True).strip()
    report={'harness':'FL3-E5','timestamp_utc':ts,'netsim_commit':commit,'netsim_bin':str(netsim_bin),
            'env':{'cpu_only':True,'loopback_only':True,'gpu_used':False,'ssh_used':False}}
    with tempfile.TemporaryDirectory(prefix='fl-e5-') as td:
        tmp=Path(td)
        print('[e5] sustained load (50x1MiB cold+warm, RSS)...',flush=True)
        report['sustained']=sustained(netsim_bin,tmp)
        print('[e5] concurrent stress probe (cpu-baseline, read-only)...',flush=True)
        report['concurrent_cpu_baseline']=concurrent_stress_cpu_baseline(tmp)
    report['summary']={'sustained':report['sustained'].get('summary'),'concurrent':report['concurrent_cpu_baseline'].get('summary') or report['concurrent_cpu_baseline'].get('status')}
    results_path.write_text(json.dumps(report,indent=2)+'\n')
    (NETSIM/'qa'/'results'/'e5_latest.json').write_text(json.dumps(report,indent=2)+'\n')
    print(f'[e5] wrote {results_path}')
    print(f'[e5] summary: {json.dumps(report["summary"])}')
    ok = report['sustained'].get('summary',{}).get('all_pass')
    return 0 if ok else 2

if __name__=='__main__': raise SystemExit(main())
