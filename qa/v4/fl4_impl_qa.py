#!/usr/bin/env python3
"""FL4-IMPL (OpenCode) — comprehensive v4 QA: network efficiency, correctness,
reliability, regression. CPU-only, loopback-only, no GPU, no model, deterministic.

Runs against v4 worktree build (feat/fastlink-v4) and compares to v3 integration.
Outputs: qa/v4/results/fl4_impl_efficiency.json, qa/v4/results/fl4_impl_qa.md, qa/v4/results/fl4_impl_qa.json
"""
from __future__ import annotations
import hashlib, json, os, pathlib, random, signal, socket, statistics, subprocess, sys, tempfile, time
from concurrent.futures import ThreadPoolExecutor, as_completed

HERE = pathlib.Path(__file__).resolve().parent
V4_WT = HERE.parents[1]
V3_INT = V4_WT.parent / "fastlink-v3-integrate"
RES = HERE / "results"; RES.mkdir(parents=True, exist_ok=True)
SEED = 0xF4A1
CHUNK = 64*1024

def sha256p(p):
    h=hashlib.sha256()
    with open(p,'rb') as f:
        for b in iter(lambda:f.read(1<<20),b''): h.update(b)
    return h.hexdigest()

def make_file(path,size,seed):
    rng=random.Random(seed); pathlib.Path(path).parent.mkdir(parents=True,exist_ok=True)
    with open(path,'wb') as f:
        rem=size
        while rem:
            n=min(rem,65536); buf=bytearray(n)
            for i in range(0,n,256):
                if rng.random()<.45: buf[i:i+min(256,n-i)]=bytes([rng.randrange(256)])*min(256,n-i)
                else: buf[i:i+min(256,n-i)]=rng.randbytes(min(256,n-i))
            f.write(buf); rem-=n

def mutate_file(src,dst,fraction,offset,seed):
    b=bytearray(open(src,'rb').read()); n=max(1,int(len(b)*fraction)); s=min(offset,max(0,len(b)-n))
    rng=random.Random(seed)
    for i in range(s,s+n): b[i]=(b[i]+1+rng.randrange(255))&255
    pathlib.Path(dst).parent.mkdir(parents=True,exist_ok=True); open(dst,'wb').write(b)

def free_port():
    with socket.socket() as s: s.bind(('127.0.0.1',0)); return s.getsockname()[1]
def wait_port(port,timeout=15):
    d=time.time()+timeout
    while time.time()<d:
        try:
            with socket.create_connection(('127.0.0.1',port),timeout=.2): return True
        except OSError: time.sleep(.04)
    return False
def build(wt):
    r=subprocess.run(['cargo','build','--release','--quiet'],cwd=wt,capture_output=True,text=True,timeout=900)
    if r.returncode!=0: raise RuntimeError(f"build {wt} failed: {r.stderr[-600:]}")
    return wt/'target'/'release'/'fastlink'
def srv_start(bin,cwd,port,data):
    pathlib.Path(data).mkdir(parents=True,exist_ok=True)
    return subprocess.Popen([str(bin),'serve',f'127.0.0.1:{port}',str(data)],cwd=cwd,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True,start_new_session=True)
def srv_stop(p):
    try: os.killpg(p.pid,signal.SIGTERM)
    except Exception: pass
    try: p.communicate(timeout=5)
    except Exception:
        try: os.killpg(p.pid,signal.SIGKILL)
        except Exception: pass
def srv_kill(p):
    try: os.killpg(p.pid,signal.SIGKILL)
    except Exception: pass
    try: p.communicate(timeout=3)
    except Exception: pass

def parse_out(txt):
    o={}
    for line in txt.splitlines():
        if ':' in line:
            k,_,v=line.partition(':'); k=k.strip().lstrip('. '); v=v.strip()
            try: iv=int(v)
            except:
                try: iv=float(v)
                except: iv=v
            o[k]=iv
    return o

def send(bin,cwd,src,port,timeout=180):
    t0=time.perf_counter()
    r=subprocess.run([str(bin),'send',str(src),f'127.0.0.1:{port}'],cwd=cwd,capture_output=True,text=True,timeout=timeout)
    txt=(r.stdout or '')+'\n'+(r.stderr or ''); m=parse_out(txt)
    m.update({'ok':r.returncode==0,'rc':r.returncode,'wall_ms':round((time.perf_counter()-t0)*1000,3),'_tail':txt.strip()[-1200:]})
    return m

def rss_kb(pid):
    try:
        for l in open(f'/proc/{pid}/status'):
            if l.startswith('VmRSS:'): return int(l.split()[1])
    except Exception: pass
    return None

# ---- 1. Network efficiency baseline ----
def efficiency(bin,cwd,tmp,name):
    src=tmp/f'{name}-4m.bin'; make_file(src,4*1024*1024,SEED^1); dh=sha256p(src)
    dlt=tmp/f'{name}-d.bin'; mutate_file(src,dlt,0.01,1024*1024,SEED^2)
    port=free_port(); cache=tmp/f'c-{name}'; p=srv_start(bin,cwd,port,cache); assert wait_port(port)
    try:
        cold=send(bin,cwd,src,port); warm=send(bin,cwd,src,port); delta=send(bin,cwd,dlt,port)
    finally: srv_stop(p)
    def row(m,mode):
        wb=m.get('wire_bytes_total'); tot=m.get('chunks_total'); tr=m.get('chunks_transferred'); reu=m.get('chunks_reused')
        rpc= (1+1+int(tr)+1) if tr is not None else None
        return {'mode':mode,'ok':m.get('ok'),'wire_bytes_total':wb,'wire_bytes_tx':m.get('wire_bytes_tx'),'wire_bytes_rx':m.get('wire_bytes_rx'),
                'chunks_total':tot,'chunks_transferred':tr,'chunks_reused':reu,
                'cache_hit_rate':round(reu/tot,6) if (reu is not None and tot) else None,
                'cache_miss_rate':round(tr/tot,6) if (tr is not None and tot) else None,
                'encoded_payload_bytes':m.get('encoded_payload_bytes'),
                'rpc_calls_proxy_per_sync':rpc,'rpc_calls_proxy_note':'protocol proxy: 1(manifest)+1(need)+chunks_transferred+1(status), not LLM inference RPC',
                'sync_events_per_op':1,'manifest_ms':round((m.get('manifest_us') or 0)/1000,3),
                'encode_transfer_ms':round((m.get('transfer_us') or 0)/1000,3),'decode_ms':{'value':None,'provenance':'not emitted by CLI'},
                'elapsed_ms':m.get('elapsed_ms'),
                'bytes_per_logical_mib':round(wb/(4),3) if wb else None,
                'bytes_per_generated_token_PROXY':None,'per_token_provenance':'PROXY not computed: no LLM inference/model run (CPU-only QA)'}
    def vexact(srcf): 
        op=cache/'outputs'/srcf.name; return sha256p(op)==sha256p(srcf) if op.exists() else False
    return {'build':name,'cwd':str(cwd),
            'cold':{**row(cold,'v3_always_send_first_sync'),'byte_exact':vexact(src)},
            'warm_resend':{**row(warm,'chunk_cache_hit_resend'),'byte_exact':vexact(src),
                           'note':'same content resent: chunks reused (0 transferred) but manifest/need still exchanged — chunk-level cache, NOT full v4 persistent-residency session'},
            'delta':{**row(delta,'incremental_delta'),'byte_exact':vexact(dlt)}}

# ---- 2. Byte-exact determinism v4 vs v3 ----
def determinism(v4b,v4c,v3b,v3c,tmp):
    src=tmp/'det.bin'; make_file(src,2*1024*1024,SEED^3); sh=sha256p(src)
    def run(bin,cwd,tag):
        port=free_port(); cache=tmp/f'det-{tag}'; p=srv_start(bin,cwd,port,cache); assert wait_port(port)
        try: r=send(bin,cwd,src,port)
        finally: srv_stop(p)
        op=cache/'outputs'/src.name; oh=sha256p(op) if op.exists() else None
        return {'ok':r['ok'],'out_sha256':oh,'match_src':oh==sh}
    a=run(v4b,v4c,'v4'); b=run(v3b,v3c,'v3')
    return {'src_sha256':sh,'v4':a,'v3':b,'v4_eq_v3_output': a['out_sha256']==b['out_sha256'] and a['match_src'] and b['match_src'],
            'identical_deterministic_output':'PASS' if (a['out_sha256']==b['out_sha256'] and a['match_src']) else 'FAIL'}

# ---- 3. Cache invalidation ----
def cache_invalidation(bin,cwd,tmp):
    # mutate source => different hash => resync (no stale reuse); also flip cached chunk if possible
    src=tmp/'inv.bin'; make_file(src,2*1024*1024,SEED^4)
    src2=tmp/'inv2.bin'; mutate_file(src,src2,0.02,512*1024,SEED^5)
    port=free_port(); cache=tmp/'inv-c'; p=srv_start(bin,cwd,port,cache); assert wait_port(port)
    try:
        r1=send(bin,cwd,src,port)
        # corrupt one cached chunk
        chunks=[x for x in (cache/'chunks').rglob('*') if x.is_file()] if (cache/'chunks').exists() else []
        corrupted=False
        if chunks:
            tgt=sorted(chunks)[0]; b=bytearray(open(tgt,'rb').read()); b[0]^=0xFF; open(tgt,'wb').write(b); corrupted=True
        r2=send(bin,cwd,src,port) # should detect/recover or clean-fail
        op=cache/'outputs'/src.name; ok2=(sha256p(op)==sha256p(src)) if op.exists() else False
        # mutated tensor/content -> resync
        r3=send(bin,cwd,src2,port)
        op2=cache/'outputs'/src2.name; ok3=(sha256p(op2)==sha256p(src2)) if op2.exists() else False
    finally: srv_stop(p)
    silent = (r2['ok'] and not ok2)
    return {'corrupted_cached_chunk':corrupted,
            'after_corruption':{'send_ok':r2['ok'],'output_byte_exact':ok2,'retransferred_or_recovered':ok2,'silent_corruption':silent},
            'mutated_content_resync':{'send_ok':r3['ok'],'output_byte_exact':ok3,'chunks_transferred':r3.get('chunks_transferred')},
            'pass': (not silent) and ok3 and (ok2 or not corrupted),
            'note':'byte-exact or clean-fail required; no stale reuse/silent corruption'}

# ---- 4. Restart/reconnect ----
def restart_reconnect(bin,cwd,tmp):
    src=tmp/'rr.bin'; make_file(src,4*1024*1024,SEED^6); sh=sha256p(src); total=(4*1024*1024+CHUNK-1)//CHUNK; thresh=max(1,total//2)
    port=free_port(); cache=tmp/'rr-c'
    def nch(): d=cache/'chunks'; return sum(1 for _ in d.rglob('*') if _.is_file()) if d.exists() else 0
    p=srv_start(bin,cwd,port,cache); assert wait_port(port)
    cli=subprocess.Popen([str(bin),'send',str(src),f'127.0.0.1:{port}'],cwd=cwd,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
    killed=None
    for _ in range(4000):
        n=nch()
        if n>=thresh and n<total: killed=n; srv_kill(p); break
        if cli.poll() is not None: break
        time.sleep(0.01)
    else: srv_kill(p)
    try: cli.communicate(timeout=15)
    except Exception: cli.kill()
    mid = killed is not None
    port2=free_port(); p2=srv_start(bin,cwd,port2,cache); assert wait_port(port2)
    try: r2=send(bin,cwd,src,port2)
    finally: srv_stop(p2)
    op=cache/'outputs'/src.name; ok=(sha256p(op)==sh) if op.exists() else False
    return {'mid_transfer_kill':mid,'killed_at_chunks':killed,'resume_ok':r2['ok'],'byte_exact_after_resume':ok,
            'chunks_transferred_on_resume':r2.get('chunks_transferred'),
            'pass': ok and r2['ok'], 'note':'resync only affected/missing via manifest; others intact (chunk cache preserved)'}

# ---- 5. 4-concurrent sessions ----
def concurrent4(bin,cwd,tmp):
    port=free_port(); cache=tmp/'cc-c'; p=srv_start(bin,cwd,port,cache); assert wait_port(port)
    # overlapping (same content, diff names conceptually via diff files?) and unique; use 4 unique files
    srcs=[]
    for i in range(4): f=tmp/f'cc-{i}.bin'; make_file(f,1*1024*1024,SEED^0xC0^i); srcs.append((i,f,sha256p(f)))
    # also 2 overlapping same content
    ov=tmp/'cc-ov.bin'; make_file(ov,1*1024*1024,SEED^0xCC); ovh=sha256p(ov)
    srcs2=[(4,ov,ovh),(5,ov,ovh)]
    allsrc=srcs # 4 unique (clear isolation); overlapping covered by same-hash chunks in cache
    results=[]
    try:
        with ThreadPoolExecutor(max_workers=4) as ex:
            futs={ex.submit(send,bin,cwd,s,port,240):(i,s,h) for i,s,h in allsrc}
            for fu in as_completed(futs):
                i,s,h=futs[fu]
                try: r=fu.result()
                except Exception as e: r={'ok':False,'error':str(e)}
                op=cache/'outputs'/s.name; oh=sha256p(op) if op.exists() else None
                results.append({'i':i,'ok':r.get('ok'),'byte_exact':oh==h,'chunks_transferred':r.get('chunks_transferred')})
    finally: srv_stop(p)
    results.sort(key=lambda x:x['i'])
    all_ok=all(r['ok'] and r['byte_exact'] for r in results)
    return {'n':4,'sessions':results,'all_byte_exact':all_ok,'corruption_detected':not all_ok,'pass':all_ok}

# ---- 6. Memory stability ----
def mem_stability(bin,cwd,tmp,n=40):
    srcs=[]
    for i in range(n): f=tmp/f'ms-{i}.bin'; make_file(f,512*1024,SEED^0x55^i); srcs.append(f)
    port=free_port(); cache=tmp/'ms-c'; p=srv_start(bin,cwd,port,cache); assert wait_port(port)
    rss=[]
    try:
        for i,f in enumerate(srcs):
            send(bin,cwd,f,port)
            r=rss_kb(p.pid); rss.append({'i':i,'rss_kb':r})
        # warm pass
        for i,f in enumerate(srcs):
            send(bin,cwd,f,port)
            r=rss_kb(p.pid); rss.append({'i':n+i,'rss_kb':r,'phase':'warm'})
    finally: srv_stop(p)
    vals=[x['rss_kb'] for x in rss if x['rss_kb'] is not None]
    stable=vals[4:] if len(vals)>6 else vals
    delta=(max(stable)-min(stable)) if stable else None
    bounded = (delta is not None and delta <= 64*1024) # 64 MiB bound
    return {'transfers':2*n,'file_bytes_each':512*1024,'rss_samples_kb':[x['rss_kb'] for x in rss],
            'min_kb':min(stable) if stable else None,'max_kb':max(stable) if stable else None,'delta_kb':delta,
            'bound_kb':64*1024,'bounded':bounded,'pass':bounded}

# ---- 7. Regression: e4/e5/e6 + cargo tests ----
def run_harness(script,cwd,timeout=900):
    r=subprocess.run([sys.executable,str(script)],cwd=cwd,capture_output=True,text=True,timeout=timeout)
    tail=(r.stdout+'\n'+r.stderr)[-2000:]
    # try to find summary json
    ok = r.returncode==0
    return {'script':str(script),'rc':r.returncode,'ok':ok,'tail':tail}

def regression(v4bin,v4wt):
    out={}
    # cargo tests
    ct=subprocess.run(['cargo','test','--all-targets'],cwd=v4wt,capture_output=True,text=True,timeout=600)
    out['cargo_test']={'rc':ct.returncode,'ok':ct.returncode==0,'tail':(ct.stdout+'\n'+ct.stderr)[-3000:]}
    # python sims if present
    qa=v4wt/'qa'
    if (qa/'sim_run.sh').exists():
        sr=subprocess.run(['bash','qa/sim_run.sh'],cwd=v4wt,capture_output=True,text=True,timeout=300)
        out['sim_run']={'rc':sr.returncode,'ok':sr.returncode==0,'tail':(sr.stdout+'\n'+sr.stderr)[-1500:]}
    # e4/e5/e6 - run in v4 worktree (they target own WT)
    for name in ('e4_harness.py','e5_harness.py','e6_harness.py'):
        sp=qa/name
        if sp.exists():
            out[name]=run_harness(sp,v4wt,timeout=1200)
        else:
            out[name]={'ok':False,'error':'missing'}
    return out

def main():
    ts=time.strftime('%Y-%m-%dT%H:%M:%S%z')
    report={'meta':'FL4-IMPL QA (OpenCode)','timestamp':ts,'cpu_only':True,'loopback_only':True,'gpu':False,'model':False}
    v4bin=build(V4_WT); v3bin=build(V3_INT)
    v4h=subprocess.check_output(['git','rev-parse','--short','HEAD'],cwd=V4_WT,text=True).strip()
    v3h=subprocess.check_output(['git','rev-parse','--short','HEAD'],cwd=V3_INT,text=True).strip()
    diff=subprocess.run(['diff','-rq',str(V4_WT/'src'),str(V3_INT/'src')],capture_output=True,text=True)
    src_same=(diff.returncode==0 and not diff.stdout.strip())
    report['builds']={'v4_wt':{'head':v4h,'path':str(V4_WT)},'v3_integrate':{'head':v3h,'path':str(V3_INT)},'src_identical':src_same}

    with tempfile.TemporaryDirectory(prefix='fl4qa-') as td:
        tmp=pathlib.Path(td)
        print('[1] efficiency...',flush=True)
        eff_v4=efficiency(v4bin,V4_WT,tmp,'v4')
        eff_v3=efficiency(v3bin,V3_INT,tmp,'v3')
        report['efficiency']={'v4_build':eff_v4,'v3_integrate_build':eff_v3}
        print('[2] determinism...',flush=True)
        report['determinism']=determinism(v4bin,V4_WT,v3bin,V3_INT,tmp)
        print('[3] cache invalidation...',flush=True)
        report['cache_invalidation']=cache_invalidation(v4bin,V4_WT,tmp)
        print('[4] restart/reconnect...',flush=True)
        report['restart_reconnect']=restart_reconnect(v4bin,V4_WT,tmp)
        print('[5] 4-concurrent...',flush=True)
        report['concurrent4']=concurrent4(v4bin,V4_WT,tmp)
        print('[6] memory stability...',flush=True)
        report['mem_stability']=mem_stability(v4bin,V4_WT,tmp)

    print('[7] regression...',flush=True)
    report['regression']=regression(v4bin,V4_WT)

    # residency comparison summary
    cv4=eff_v4['cold']; wv4=eff_v4['warm_resend']
    report['network_efficiency_summary']={
        'mode':'v3_always_send_baseline_measured (chunk-cache reuse on warm resend observed)',
        'v4_persistent_residency_implemented': not src_same,
        'v4_vs_v3_always_send_reduction_measured': 'N/A — v4 persistent-residency not implemented in feat/fastlink-v4 src (identical to v3 integrate); no residency-vs-always-send delta measurable yet' if src_same else 'see data',
        'always_send_cold_wire_bytes_total_4MiB': cv4['wire_bytes_total'],
        'warm_resend_wire_bytes_total_4MiB': wv4['wire_bytes_total'],
        'warm_resend_cache_hit_rate': wv4['cache_hit_rate'],
        'warm_resend_chunks_transferred': wv4['chunks_transferred'],
        'cold_rpc_calls_proxy_per_sync': cv4['rpc_calls_proxy_per_sync'],
        'warm_rpc_calls_proxy_per_sync': wv4['rpc_calls_proxy_per_sync'],
        'bytes_per_generated_token':'NOT MEASURED — no LLM inference/GPU/model run (CPU-only). Proxy: bytes_per_sync_op and bytes_per_logical_mib reported.',
        'honest_note':'Persistent residency (FL4-IMPL core) absent from current src; reduction % cannot be measured. Warm resend demonstrates chunk-cache hits (0 chunks transferred, wire=manifest/need overhead) under existing always-send session model.'
    }

    passes={
        'determinism': report['determinism']['identical_deterministic_output']=='PASS',
        'cache_invalidation': report['cache_invalidation']['pass'],
        'restart_reconnect': report['restart_reconnect']['pass'],
        'concurrent4': report['concurrent4']['pass'],
        'mem_stability': report['mem_stability']['pass'],
        'cargo_test': report['regression']['cargo_test']['ok'],
        'e4': report['regression'].get('e4_harness.py',{}).get('ok',False),
        'e5': report['regression'].get('e5_harness.py',{}).get('ok',False),
        'e6': report['regression'].get('e6_harness.py',{}).get('ok',False),
    }
    report['pass_fail']=passes
    report['all_core_pass']=all(v for k,v in passes.items() if k!='sim_run')

    (RES/'fl4_impl_qa.json').write_text(json.dumps(report,indent=2,sort_keys=True)+'\n')
    (RES/'fl4_impl_efficiency.json').write_text(json.dumps({'meta':report['meta'],'timestamp':ts,'efficiency':report['efficiency'],'network_efficiency_summary':report['network_efficiency_summary']},indent=2,sort_keys=True)+'\n')
    write_md(report)
    print(json.dumps({'all_core_pass':report['all_core_pass'],'passes':passes,'src_identical':src_same},indent=2))
    return report

def write_md(r):
    p=r['pass_fail']; e=r['efficiency']['v4_build']; c=e['cold']; w=e['warm_resend']; d=e['delta']
    md=f"""# FL4-IMPL QA (OpenCode) — network efficiency + correctness/reliability/regression

- **Timestamp:** {r['timestamp']}
- **Env:** CPU-only, loopback-only, **no GPU, no model, no push**.
- **v4 worktree:** `{r['builds']['v4_wt']['path']}` @ `{r['builds']['v4_wt']['head']}` (feat/fastlink-v4)
- **v3 integrate:** `{r['builds']['v3_integrate']['path']}` @ `{r['builds']['v3_integrate']['head']}`
- **src v4 vs v3 integrate identical:** **{r['builds']['src_identical']}**

## 1) Network efficiency (measured, protocol-level; 4 MiB deterministic, loopback)

> **Honest status:** v4 *persistent remote tensor cache + adaptive incremental sync* is **NOT implemented** in `feat/fastlink-v4` src (identical to v3 integrate). Therefore **bytes/token and RPC/token reduction (v4 residency vs v3 always-send) cannot be measured as a delta**. No numbers invented. Below: **always-send baseline measured** on v4 build and v3 integrate build; v4 residency reduction = **N/A (implementation absent)**. "Per generated token" not measured (no LLM inference/model).

### v4 build (always-send session model, chunk-cache behavior observed)
| phase | wire_bytes_total | wire_tx | wire_rx | chunks total/reused/transferred | cache hit rate | RPC calls proxy/sync* | sync events | manifest ms | encode/transfer ms | decode ms | byte_exact |
|---|---:|---:|---:|---|---:|---:|---:|---:|---:|---|---:|
| cold (first sync) | {c['wire_bytes_total']} | {c['wire_bytes_tx']} | {c['wire_bytes_rx']} | {c['chunks_total']}/{c['chunks_reused']}/{c['chunks_transferred']} | {c['cache_hit_rate']} | {c['rpc_calls_proxy_per_sync']} | 1 | {c['manifest_ms']} | {c['encode_transfer_ms']} | n/a (CLI) | {c['byte_exact']} |
| warm resend (same content) | {w['wire_bytes_total']} | {w['wire_bytes_tx']} | {w['wire_bytes_rx']} | {w['chunks_total']}/{w['chunks_reused']}/{w['chunks_transferred']} | {w['cache_hit_rate']} | {w['rpc_calls_proxy_per_sync']} | 1 | {w['manifest_ms']} | {w['encode_transfer_ms']} | n/a (CLI) | {w['byte_exact']} |
| delta (1% mutated) | {d['wire_bytes_total']} | {d['wire_bytes_tx']} | {d['wire_bytes_rx']} | {d['chunks_total']}/{d['chunks_reused']}/{d['chunks_transferred']} | {d['cache_hit_rate']} | {d['rpc_calls_proxy_per_sync']} | 1 | {d['manifest_ms']} | {d['encode_transfer_ms']} | n/a (CLI) | {d['byte_exact']} |

\*RPC calls proxy = protocol-level `1(manifest)+1(need)+chunks_transferred+1(status)` per sync op. **Not LLM inference RPC/token calls.** Decode CPU ms not emitted by CLI (reported n/a with provenance).

### v4 persistent residency vs v3 always-send (requested comparison)
| metric | v3 always-send (measured, current build) | v4 persistent residency (measured) | reduction |
|---|---:|---:|---:|
| bytes/sync (cold, 4 MiB logical) | {c['wire_bytes_total']} | **N/A** | **N/A — residency not implemented** |
| RPC/sync proxy (cold) | {c['rpc_calls_proxy_per_sync']} | **N/A** | **N/A** |
| bytes/token | **N/A (no inference)** | **N/A (no inference/impl)** | **N/A** |
| RPC/token | **N/A (no inference)** | **N/A** | **N/A** |
| warm resend cache hit rate | {w['cache_hit_rate']} (chunk cache; 0 chunks transferred, manifest/need still exchanged) | **N/A** | **N/A** |

Modelled projection (clearly labeled, not measured): {r['network_efficiency_summary']['honest_note']}

## 2) Correctness
- **Byte-exact deterministic outputs (v4 vs v3 integrate, same input):** {r['determinism']['identical_deterministic_output']} (src_sha={r['determinism']['src_sha256'][:16]}…, v4==v3 output, match src)
- **Cache invalidation (corrupt cached chunk -> recover/resync; mutated content -> resync, no stale reuse):** {'PASS' if p['cache_invalidation'] else 'FAIL'} — {json.dumps(r['cache_invalidation'], sort_keys=True)}
- **Worker restart/reconnect (mid-transfer kill, restart, resume; no partial):** {'PASS' if p['restart_reconnect'] else 'FAIL'} — mid_kill={r['restart_reconnect']['mid_transfer_kill']}, byte_exact_after_resume={r['restart_reconnect']['byte_exact_after_resume']}, chunks_transferred_on_resume={r['restart_reconnect']['chunks_transferred_on_resume']}

## 3) Concurrency (4 sessions) + memory stability
- **4-concurrent sessions (unique, byte-exact, 0 corruption):** {'PASS' if p['concurrent4'] else 'FAIL'} — all_byte_exact={r['concurrent4']['all_byte_exact']}, corruption_detected={r['concurrent4']['corruption_detected']}
- **Memory stability (RSS bounded, 80 transfers, 512 KiB files):** {'PASS' if p['mem_stability'] else 'FAIL'} — min/max/delta={r['mem_stability']['min_kb']}/{r['mem_stability']['max_kb']}/{r['mem_stability']['delta_kb']} kB, bound={r['mem_stability']['bound_kb']} kB, bounded={r['mem_stability']['bounded']}

## 4) Regression vs v3 (v4 build)
- **cargo test --all-targets (v4 worktree):** {'PASS' if p['cargo_test'] else 'FAIL'} (rc={r['regression']['cargo_test']['rc']})
- **e4_harness.py:** {'PASS' if p['e4'] else 'FAIL'} (rc={r['regression'].get('e4_harness.py',{}).get('rc')})
- **e5_harness.py:** {'PASS' if p['e5'] else 'FAIL'} (rc={r['regression'].get('e5_harness.py',{}).get('rc')})
- **e6_harness.py:** {'PASS' if p['e6'] else 'FAIL'} (rc={r['regression'].get('e6_harness.py',{}).get('rc')})
{f"- **sim_run.sh:** {'PASS' if r['regression'].get('sim_run',{}).get('ok') else 'FAIL/SKIP'}" if 'sim_run' in r['regression'] else ''}

## Summary (pass/fail)
```json
{json.dumps(p, indent=2)}
```
**all_core_pass = {r['all_core_pass']}**

## Honesty / limitations
- CPU-only, loopback-only. **No GPU, no model inference.** “Per generated token” and LLM RPC/token counts **not measured** (proxies/n-a labeled).
- **v4 persistent-residency not implemented in current src** (identical to v3 integrate) → residency-vs-always-send **reduction % not measurable**. No invented numbers.
- Decode CPU ms not emitted by integrated CLI; encode/transfer/manifest timing reported where available.
"""
    (RES/'fl4_impl_qa.md').write_text(md)

if __name__=='__main__': main()
