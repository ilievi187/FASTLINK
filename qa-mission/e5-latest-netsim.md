# FL3-E5 sustained load + memory stability results
- netsim commit: `a50ecd8` (feat/fastlink-v3-network-sim)
- timestamp_utc: 20261107T193857Z
- CPU-only, loopback-only, no GPU/SSH

## Sustained load (50x1MiB cold + 50x1MiB warm = 100 transfers)
- cold: all byte-exact=True, latency ms min/max/mean/p50={'min': 84.79, 'max': 88.411, 'mean': 86.111, 'p50': 86.093}
- warm: all byte-exact=True, all chunks_transferred==0=True, latency ms={'min': 41.977, 'max': 44.223, 'mean': 43.014, 'p50': 43.052}
- server VmRSS (kB, sampled/transfer, first 2 excluded): min=2792 max=2792 delta=0 bound=32768 bounded=True
- **all_pass=True**

## 4-session concurrent stress (cpu-baseline deac1d0, read-only build)
- N=4 parallel 1MiB sends, distinct files, loopback: all_ok=True all_byte_exact=True **all_pass=True**
- Honest: tested against cpu-baseline at deac1d0 (FL3-A4 committed). Initial e5 run had wrong CPU_BASELINE path (missing .worktrees); concurrent stress re-run correctly at 2026-11-07T21:45+02:00, result merged.

Deterministic: seed=0xE5E5, fixed sizes.
