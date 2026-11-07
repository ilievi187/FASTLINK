# FL3-E2 — Transport correctness sweep (main branch)

**Attribution:** OpenCode (Antigravity)  
**Date:** 2026-11-07  
**Code under test:** `main` branch of `/home/nightknight/Projects/FASTLINK` at **`dcaa511`** (`docs: record successful Linux and Windows CI verification`), tested via throwaway detached worktree `/tmp/fl3e2-main` (no commits made in main).  
**Environment:** CPU-only, loopback (127.0.0.1, ephemeral port), **no GPU, no SSH, no remote hosts**. v2 tree and running 27B service untouched.

## Exact commands (repro)
```bash
# throwaway worktree (from /home/nightknight/Projects/FASTLINK)
git worktree add --detach /tmp/fl3e2-main main   # -> dcaa511
cd /tmp/fl3e2-main && cargo build --release --quiet
BIN=/tmp/fl3e2-main/target/release/fastlink

# deterministic 8 MiB source (seed 0xE2E2, mixed 256-byte runs/random)
python3 - << 'PY'
import random,hashlib; from pathlib import Path
rng=random.Random(0xE2E2); p=Path('/tmp/fl3e2-work/src/data-8m.bin'); p.parent.mkdir(parents=True,exist_ok=True)
with p.open('wb') as f:
    rem=8*1024*1024
    while rem:
        n=min(rem,65536); buf=bytearray(n)
        for i in range(0,n,256):
            if rng.random()<0.5: buf[i:i+min(256,n-i)]=bytes([rng.randrange(256)])*min(256,n-i)
            else: buf[i:i+min(256,n-i)]=rng.randbytes(min(256,n-i))
        f.write(buf); rem-=n
print('sha256',hashlib.sha256(p.read_bytes()).hexdigest(),'bytes',p.stat().st_size)
PY
# source sha256: d4dc5fc16729e18e45cadd49a67cca96140ca64bfca964f747bc592105397594  bytes 8388608

# loopback server (ephemeral port 45947 in this run)
$BIN serve 127.0.0.1:45947 /tmp/fl3e2-work/cache &
# COLD
$BIN send /tmp/fl3e2-work/src/data-8m.bin 127.0.0.1:45947
# WARM (same file, same cache)
$BIN send /tmp/fl3e2-work/src/data-8m.bin 127.0.0.1:45947
# DELTA: mutate 1% at fixed offset 1 MiB (seed 0xE2DE), new basename
python3 - << 'PY'
import random,hashlib; from pathlib import Path
b=bytearray(Path('/tmp/fl3e2-work/src/data-8m.bin').read_bytes())
n=max(1,int(len(b)*0.01)); start=min(1024*1024,len(b)-n); rng=random.Random(0xE2DE)
for i in range(start,start+n): b[i]=(b[i]+1+rng.randrange(255))&255
p=Path('/tmp/fl3e2-work/src/data-8m-delta.bin'); p.write_bytes(b)
print('sha256',hashlib.sha256(b).hexdigest())
PY
# delta src sha256: 38749aa4d5eb8cc0434701d91592352d22e3a4ea75d3b1fb33444bf1ac49d845
$BIN send /tmp/fl3e2-work/src/data-8m-delta.bin 127.0.0.1:45947
```

## Recorded SHA-256 digests (byte-exact verification)
| artifact | SHA-256 |
|---|---|
| source `data-8m.bin` (8 MiB) | `d4dc5fc16729e18e45cadd49a67cca96140ca64bfca964f747bc592105397594` |
| COLD output `cache/outputs/data-8m.bin` | `d4dc5fc16729e18e45cadd49a67cca96140ca64bfca964f747bc592105397594` (**match**) |
| WARM output `cache/outputs/data-8m.bin` | `d4dc5fc16729e18e45cadd49a67cca96140ca64bfca964f747bc592105397594` (**match**) |
| source `data-8m-delta.bin` (1% mutated @ 1 MiB, seed 0xE2DE) | `38749aa4d5eb8cc0434701d91592352d22e3a4ea75d3b1fb33444bf1ac49d845` |
| DELTA output `cache/outputs/data-8m-delta.bin` | `38749aa4d5eb8cc0434701d91592352d22e3a4ea75d3b1fb33444bf1ac49d845` (**match**) |

**All 3 runs byte-exact (SHA-256 verified).**

## Chunk stats and wire/payload bytes (as reported by main `fastlink send`)
Main branch client reports `encoded_payload_bytes` and states: *"payload counts exclude wire framing, hashes, and TCP overhead"*. Full on-the-wire bytes (handshake/manifest/NEED/chunk framing/TCP/IP) are **not** reported by this main-branch CLI; values below are **encoded payload bytes** (factual, as emitted).

| run | file_bytes | chunks_total | chunks_reused | chunks_transferred | raw_missing_bytes | encoded_payload_bytes | payload_avoided_vs_full_file | elapsed_ms | RC |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| COLD | 8,388,608 | 128 | 0 | 128 | 8,388,608 | **8,221,990** | 166,618 | 140 | 0 |
| WARM | 8,388,608 | 128 | 128 | 0 | 0 | **0** | 8,388,608 | 50 | 0 |
| DELTA (1% @ 1 MiB) | 8,388,608 | 128 | 126 | 2 | 131,072 | **131,072** | 8,257,536 | 92 | 0 |

## Notes (honest)
- Server log contained two `session error: failed to fill whole buffer` lines attributable to a flawed port-readiness probe in the harness shell (connection opened/closed without speaking FLNK protocol); **all three actual `fastlink send` transfers succeeded (RC 0) and all outputs verified byte-exact**.
- "Wire bytes" reported here are the main-branch CLI's `encoded_payload_bytes` (excludes framing/hashes/TCP), as stated by the tool. No numbers invented.
- Throwaway worktree `/tmp/fl3e2-main` cleaned up after run; no commits made in main.

## Verdict
**PASS** — cold/warm/delta transfers on main `dcaa511` (loopback, 8 MiB) completed and each destination matched source SHA-256 exactly; chunk stats and encoded payload bytes recorded.
