# bench — measured results (m17)

This is the definitive summary of the **current** measurement session (`m17`, 2026-10-18) for the
`erts_rust` benchmark driver. Every number here comes from the committed raw artifacts under
`results/m17-*`; regenerate with the Runbook (§7) rather than quoting a stale copy.
Raw JSON is authoritative — this file never edits a metric value.

## 0. The two sides, named

| side | program | version |
|---|---|---|
| **sys**  | the oracle `erl` on `PATH` (`/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/lib/erlang/erts-17.1/bin/erl`) | **Erlang/OTP 29.1.1 / erts-17.1** |
| **rust** | `erts_rust/target/release/beam_rt` at its **default baseline-JIT tier** (`JitMode::Auto` ⇒ `Tier::BaselineJit`) | reports `30` |

Every "sys vs rust" row below is a comparison of those two programs. The **binary A/B** of §4 compares
two `beam_rt` builds (`new` = this session's HEAD, `old` = the m15-era tree `a34ca61332`); it is
**never** a system-ERTS comparison.

Artifacts for this session (all committed under `results/`):
`m17-xab-{rust,sys}-r{1,2,3}.json` **and** `m17-xab-{rust,sys}-r{4,5,6}.json` (the **six matched
cross-runtime rounds** behind §2/§3 — rounds 1–3 ran in a rising-load window, rounds 4–6 in the
relative lull; each file is a separate `--quick` single-side pass), `m17-ab-{new,old}-r{1,2,3,4,5}.json`
(the **binary A/B** behind §4), `m17-bench-external.json` / `m17-bench-external-full.json` (the
external targets), the diff `m17-compare-ab-binary.txt`, and `m17-{xab,ab,ext}-load.log` +
`m17-load-record.md`.

**m17 ran no runtime-major primary pass** (`--runtime all`, all `sys` rows then all `rust` rows): that
ordering is a documented bias source on a drifting host (m16 measured it at up to 18 % on `list_walk`),
and every headline figure here comes from the **matched** rounds, which sample both sides within a
couple of minutes of each other.

## 1. The host and the load record

14 logical cores, shared. `/usr/bin/time` is absent; the harness uses `date +%s%N` + `/proc/<pid>/status`
`VmHWM`/`VmRSS`. **Every matched round and every A/B pass is pinned to a P-core with `taskset -c 3`**;
the single-side external passes are unpinned so they stay comparable with earlier sessions. Load was
recorded before and after every run:

| run | artifact(s) | pin | `/proc/loadavg` (1-min) across the run |
|---|---|---|---|
| matched rounds 1–3 | `m17-xab-{rust,sys}-r{1,2,3}.json` | `taskset -c 3` | 5.14 → 65.76 (rising all the way) |
| matched rounds 4–6 | `m17-xab-{rust,sys}-r{4,5,6}.json` | `taskset -c 3` | 8.25 → 51.40 (a lull, then rising again) |
| binary A/B rounds 1–5 | `m17-ab-{new,old}-r{1..5}.json` | `taskset -c 3` | 51.36 → 73.15 (per-round values in `m17-ab-load.log`) |
| external targets | `m17-bench-external.json` (`--quick`) | none | 64.13 → 53.02 |
| external targets | `m17-bench-external-full.json` (`--full`) | none | 72.66 → 56.24 |

Full logs: `m17-xab-load.log`, `m17-ab-load.log`, `m17-ext-load.log`; narrative in
`m17-load-record.md`. The host never fell below 1-min load 3 during the session and spent most of it
between 10 and 90 — **m17 is a loaded session, m16's band was 3–76 and m15's 2–5.** Consequences, both
of which must be stated with the numbers:

- absolute wall times/throughputs are **load-contaminated on both sides** and are not comparable with
  m16's absolute figures. The sys side is comparatively load-insensitive (`list_walk` 2.28 → 2.72
  ns/element across the session) while the rust side absorbs most of the variance (`list_walk` 22.6 →
  31.9 ns/element), so the **per-round ratio** column is the stability evidence and a cross-session
  "changed since" claim is only valid through the **binary A/B of §4**;
- **read the ratios and the deterministic `case_*_ops/calls/elements` counters, never a single absolute
  wall time as a constant.**

## 2. `baseline` — startup distribution + vm memory (sys vs rust)

Median of the six matched rounds; the per-round ratios are §3.1.

| metric | sys | rust | rust/sys |
|---|---|---|---|
| startup `p50` (ms) | **127.3** | **323.6** | **2.51×** |
| startup `p99` (ms) | **137.5** | **335.2** | **2.56×** |
| compute loop `p50` (ms) | 193.5 | 348.3 | 1.84× |
| `erlang:memory(total)` (`vm_memory_total_bytes`) | 36.20 MB | **6.69 MB** | rust **5.41× lower** |
| OS peak RSS (`VmHWM`) | 135.2 MB | 150.4 MB | 1.11× |

`vm_memory_total_bytes` (the VM's own accounting) and `peak_memory_bytes` (OS `VmHWM`) are **separate
metrics and are never conflated**.

Two facts on this row need their caveats stated with them:

- the rust `vm_memory_total_bytes` reads **6.69 MB** where the m15-era runtime reports **13.22 MB** —
  the binary A/B (§4) puts that at a flat **0.506×** across all five rounds — while the same A/B puts
  OS peak RSS **up 1.13×** (133.2 → 150.4 MB). The two moving in opposite directions is an
  **accounting** change (the runtime stopped charging itself for reclaimed capacity), **not** a
  footprint win: **do not read the 5.41× ratio as a memory reduction.** The like-for-like footprint
  reads are `slice`'s peak RSS (§3.2) and the same A/B's `copy_bound`/`foreign` RSS rows;
- the startup ratio (2.51× p50 / 2.56× p99) is the session's whole-process comparison and it is
  **improved**: m16 recorded 3.76× / 3.74×, and the §4 A/B puts the current runtime at **1.05× the
  m15-era build's** startup p50 (m16's A/B said 1.17×), so most of m16's startup regression is gone
  and the remainder of the difference from m15's absolute figures is host load.

## 3. The matched cross-runtime record (the headline)

Twelve `--quick` single-side passes = **six matched rounds**, alternating rust-only and sys-only,
pinned, one process at a time, so each workload's two sides are sampled within a couple of minutes of
each other. The figure quoted is the **median of the six rounds** and the **ratio** is the median of the
six per-round ratios; the per-round ratios are shown because their spread is the stability evidence.
Ratio > 1 ⇒ rust dearer.

### 3.1 The ratios

| workload | case | sys | rust | **rust/sys** | per-round rust/sys |
|---|---|---|---|---|---|
| `baseline` | startup `p50` | 127.3 ms | 323.6 ms | **2.51×** | 2.79 2.54 2.48 2.23 2.45 3.05 |
| `baseline` | startup `p99` | 137.5 ms | 335.2 ms | **2.56×** | 2.77 2.58 2.54 2.22 2.37 3.39 |
| `list_walk` | per element | 2.52 ns | 30.55 ns | **11.80×** | 9.90 13.83 10.92 10.86 13.21 12.69 |
| `copy_bound` | `etf_roundtrip` | 730.4 ns | 1 581.6 ns | **2.21×** | 2.16 2.25 1.93 2.27 2.17 2.31 |
| `copy_bound` | `ets_copy` | 279.3 ns | 2 510.7 ns | **9.28×** | 9.51 9.49 7.64 9.85 9.07 9.05 |
| `copy_bound` | `send_recv` | 1 655.8 ns | 8 898.9 ns | **5.39×** | 5.01 5.89 4.82 5.73 5.56 5.21 |
| `copy_bound` | `send_recv_plain` | 703.5 ns | 2 195.9 ns | **3.16×** | 2.70 3.33 2.66 3.42 3.08 3.24 |
| `gc_bound` | `churn` | 4 575.9 ns | 86 928.0 ns | **18.75×** | 14.68 22.32 16.29 16.87 23.45 20.64 |
| `foreign` | `bstr_to_lower` | 385.4 ns | 7 784.6 ns | **18.81×** | 15.28 22.58 18.69 17.14 22.75 18.93 |
| `foreign` | `bstr_capitalize_token` | 400.9 ns | 9 436.7 ns | **22.46×** | 19.59 31.35 19.84 20.16 28.88 24.75 |
| `foreign` | `emqx_base62_encode` | 287.4 ns | 5 913.0 ns | **20.04×** | 17.37 26.71 17.88 19.00 24.09 21.08 |
| `foreign` | `emqx_base62_decode` | 382.5 ns | 10 387.2 ns | **25.88×** | 20.54 35.16 21.29 26.08 30.90 25.69 |
| `binary_append` | per `expand/1` call | 108 253.0 ns | 4 220 897.5 ns | **40.02×** | 29.68 43.09 30.92 40.90 50.00 39.15 |
| `slice` | 9 cases, one call/process | 134.1–137.9 ms | 316.2–368.8 ms | **2.45–2.68×** (median 2.63×) | 2.45–3.04 per case |

The `send_recv − send_recv_plain` gap attributes the ETF-codec component *inside* the two-process path
(the two cases differ only in whether the receiver re-encodes the payload): **6.70 µs/op** on rust
against **0.95 µs/op** on the oracle.

**No row withheld a figure in any of the twelve matched passes** — `marginal below timer resolution`
appears nowhere in the twelve `m17-xab-*` reports, and all 42 (workload × side) rows are `ok`. The
A/B (§4) is the only place a figure was withheld (three, all on the loaded `old` side).

### 3.2 Memory (like-for-like, `slice`)

`slice` runs one call per process (`N`=1), so its wall time and peak RSS are whole-process figures. Peak
RSS **68.0 MB (sys) vs 120.0 MB (rust)** ⇒ **1.76×**, stable to ±0.02× across all nine cases. The loop
workloads read **1.75×–3.73×** on the same metric and are **not** a memory comparison: `list_walk` and
`gc_bound` (1.75×) are boot-dominated, while `copy_bound` (2.34×), `foreign` (2.91×) and
`binary_append` (3.73×) additionally pay the per-op retention of §8.

| workload | sys peak RSS (MB) | rust peak RSS (MB) | rust/sys |
|---|---|---|---|
| `baseline` | 135.2 | 150.4 | 1.11× |
| `slice` (N=1 — the like-for-like row) | 68.0 | 120.0 | **1.76×** |
| `list_walk` | 67.6 | 118.4 | 1.75× |
| `copy_bound` | 67.7 | 158.7 | 2.34× |
| `foreign` | 67.8 | 197.4 | 2.91× |
| `gc_bound` | 67.6 | 118.4 | 1.75× |
| `binary_append` | 67.5 | 252.0 | 3.73× |

### 3.3 How to read a per-unit figure

Every per-unit figure is a **paired, startup-cancelled marginal**: each round runs the `N`-unit pass and
then the `0`-unit pass **back to back**, and the statistic is the **median of the per-round differences**
(`src/marginal.rs`), withheld — no metric, plus an explicit `marginal below timer resolution` note —
unless it clears `3 × max(1 ms, 1.4826·MAD)`. The sequential all-`N`-then-all-`0` mean schedule this
replaced published `etf_roundtrip` at 136.3× where the interleaved measurement gives ≈2.4×; **do not
revert it.**

`N` is per runtime, so the report always carries the deterministic, contention-immune companion that
names it (sys / rust): `etf_roundtrip_ops` 1 500 000 / 500 000, `ets_copy_ops` 2 000 000 / 250 000,
`send_recv_ops` 500 000 / 90 000, `send_recv_plain_ops` 1 000 000 / 300 000, `churn_ops` 200 000 /
10 000, `list_walk_elements` 100 000 000 / 50 000 000, `foreign` `calls` 1 500 000 / 50 000
(`bstr_to_lower`), 1 500 000 / 40 000 (`bstr_capitalize_token`), 3 000 000 / 60 000
(`emqx_base62_encode`), 3 000 000 / 60 000 (`emqx_base62_decode`), `binary_append_calls` 50 000 / 2 000
(`binary_append_bytes` 3 276 800 000 / 131 072 000). **Never compare raw `wall_ms` across two sides with
different `N`** — only the per-unit normalization is comparable.

## 4. What changed since the m15-era runtime (the binary A/B)

`m17-ab-{new,old}-r{1..5}.json`: the **same `bench` binary** driving two `beam_rt` builds — `new` =
HEAD (`4a92cf96…`), `old` = the m15-era tree at `a34ca61332` (`md5 e0f031d2…`, unchanged across the
session) — rust-only `--quick`, five rounds per side, **alternating order round by round**, pinned to
core 3. This isolates *code* change from host load; it is the only valid way to say "this changed since
m15/m16". The figure is the median over the rounds that produced one; three marginals were withheld
(all on the loaded `old` side, rounds 1 and 5 — the raw notes are in the artifacts).

| rust-only metric | m15-era | HEAD | **HEAD/m15-era** | per-round | withheld |
|---|---|---|---|---|---|
| `baseline` startup p50 | 385.1 ms | 407.9 ms | **1.047×** | 1.04 1.05 1.27 1.03 1.42 | 0 |
| `baseline` startup p99 | 439.2 ms | 468.3 ms | 1.040× | 0.94 0.98 1.45 1.04 3.80 | 0 |
| `baseline` `vm_memory_total` | 13.22 MB | 6.69 MB | **0.506×** (accounting, §2) | 0.51 ×5 | 0 |
| `list_walk` ns/element | 31.29 | 34.67 | **1.215×** (regressed) | 1.09 1.22 1.21 1.05 1.54 | 0 |
| `copy_bound/etf_roundtrip` | 2 022.4 ns | 1 965.0 ns | 0.972× (parity) | 0.95 0.97 1.07 0.90 1.41 | 0 |
| `copy_bound/ets_copy` | 3 421.9 ns | 3 163.4 ns | **0.927×** (faster) | 0.93 0.93 0.91 0.81 1.00 | 0 |
| `copy_bound/send_recv` | 9 974.7 ns | 10 645.4 ns | 1.067× | 1.10 1.07 1.16 0.90 1.06 | 0 |
| `copy_bound/send_recv_plain` | 2 669.3 ns | 2 529.0 ns | 0.954× | 0.96 1.09 0.94 0.91 | 1 |
| `gc_bound/churn` | 99 816.8 ns | 101 583.4 ns | 1.025× (parity) | 1.04 1.04 0.82 1.01 | 1 |
| `foreign/bstr_to_lower` | 18 834.4 ns | 9 033.8 ns | **0.487×** (faster) | 0.48 0.49 0.57 0.46 0.49 | 0 |
| `foreign/bstr_capitalize_token` | 22 718.3 ns | 11 695.7 ns | **0.537×** (faster) | 0.61 0.50 0.58 0.51 0.54 | 0 |
| `foreign/emqx_base62_encode` | 25 756.7 ns | 6 517.8 ns | **0.260×** (faster) | 0.26 0.26 0.28 0.24 0.27 | 0 |
| `foreign/emqx_base62_decode` | 14 102.8 ns | 11 569.1 ns | **0.804×** (faster) | 0.79 0.83 0.67 0.82 | 1 |

The A/B's memory rows, all five rounds:

| rust-only peak RSS | m15-era | HEAD | HEAD/m15-era |
|---|---|---|---|
| `copy_bound` | 459.3 MB | 158.6 MB | **0.35×** |
| `foreign` | 323.2 MB | 197.3 MB | **0.61×** |
| `baseline` | 133.2 MB | 150.4 MB | 1.13× |
| `list_walk` / `gc_bound` | 118.3 / 118.2 MB | 118.4 / 118.3 MB | 1.00× |

**The m16-recorded regressions are closed, and the A/B is the evidence:** `emqx_base62:encode` is
**0.260×** (m16's A/B: 1.14× dearer, i.e. the 7.14× report was an accumulating per-call walk),
`bstr_to_lower`/`bstr_capitalize_token` 0.487×/0.537×, `ets_copy` 0.927× (m16: 1.63×),
`etf_roundtrip` 0.972× (m16: 1.40×), `emqx_base62_decode` 0.804× (m16: 1.76×), and startup 1.047×
(m16: 1.17×). The rust `copy_bound` peak RSS also fell **459 MB → 159 MB (−65 %)**.

**The one metric that went the other way is `list_walk`** (1.215× dearer per element than the m15-era
runtime, consistent in all five rounds and matched by the cross-runtime ratio moving 9.86× → 11.80×).
It is the only case in the A/B above parity, it is not on the copy path, and m16's own A/B had it at
0.84× — so ~10-20 % of per-element dispatch/return cost was given back somewhere in the copy-out work.
Handoff, not a `bench` defect (§8).

`binary_append` is absent from the A/B by design: the m15-era runtime needs ≈6.3–7.2 s per call, so it
cannot finish a `run` invocation at the calibrated `N` (see §8 for the current side).

## 5. Rust-vs-Rust: execution-tier effect — **not re-measured in m17**

m17 ran every rust pass at the default tier; there is no `BEAM_RT_TIER=interpreter` control in this
session. The state of the question is unchanged from m16 (`results/m16-bench-xrun-interp.json` against
`results/m16-xab-rust-r{1,2,3}.json`, diff `results/m16-compare-tier-effect-quick.txt`): the default
baseline JIT was cheaper than the interpreter on **every** case it could measure, and `binary_append`
is honestly `failed(no-binary-append-run)` at the interpreter tier because its calibrated `N` exceeds
`beam_rt`'s own 60 s `run` deadline. Re-run it with the §7 command 3 before quoting a tier figure.

## 6. External targets (all sys-only, staged in a read-only `$TMPDIR` copy)

| target | figure (`--full`) | figure (`--quick`) | instrument |
|---|---|---|---|
| `erlc_compiler` | **7.045 files/s**, 5 460.0 loc/s (60 files, wall 8 517 ms) | 6.194 files/s | system `erlc` over OTP stdlib + Elixir `lib/elixir/src` |
| `elixir_compiler` | **2.126 files/s**, 1 067.2 loc/s (105 sources → 223 beams, wall 104 912 ms, compiler startup 828.8 ms) | 1.486 files/s (25 sources → 39 beams, startup 601.3 ms) | installed system `elixirc` (per-file fallback: version skew) |
| `cowboy` | **38 297 req/s**, 191 585 requests, 0 errors, latency p50 0.211 / p99 3.03 ms (build 71 874 ms) | 15 758 req/s (31 573 requests) | `hello_world` relx release, in-process keep-alive load |
| `khepri` / `plb` / `emqx` | **not re-measured in m17** | — | see below |

**These three figures are host-starved, not regressions.** The session ran the external pass at 1-min
load 64–73; the same report carries its own control — `elixir_compiler`'s **compiler startup** reads
**828.8 ms** against m16's 288.1 ms (**2.9×**) for the identical installed `elixirc` on the identical
corpus, and `baseline`'s sys startup reads 127.3 ms against m16's 104.7 ms. The `cowboy` throughput fall
is the same effect through the driver's fixed concurrency (16 keep-alive connections: throughput is
bounded by `16 / per-request latency`, and p50 latency doubled 0.10 → 0.21 ms while p99 went
0.8 → 3.03 ms). **m16's low-load figures remain the reference for these three targets** — nothing in the
external path changed between the sessions (same driver, same project checkouts, same system `erlc`/
`elixirc`/`cowboy`).

`khepri` (needs `rebar3`), `plb` (needs a ≥ 9.0 `dotnet`) and `emqx` (needs an OTP-28 toolchain) were not
re-run in m17 — they were not in this round's core set. Their last measurements are the m16 ones
(`results/m16-bench-{external,plb,emqx}.json`); `emqx` remains an honest
`skipped(emqx-requires-otp-28)` for the reason recorded there.

## 7. Runbook (reproduction)

```sh
# 0. build the driver AND the runtime (from the repo root)
erts_rust/scripts/cargo.sh build --release -p beam_rt -p bench --manifest-path erts_rust/Cargo.toml --offline

# 1. the six matched cross-runtime rounds (the §2/§3 headline) — alternate sides per round
WL=baseline,slice,list_walk,copy_bound,foreign,gc_bound,binary_append
taskset -c 3 setsid -w erts_rust/target/release/bench run --quick --runtime rust --workloads $WL \
    --out-file erts_rust/tools/bench/results/m17-xab-rust-r1.json --verbose
taskset -c 3 setsid -w erts_rust/target/release/bench run --quick --runtime sys  --workloads $WL \
    --out-file erts_rust/tools/bench/results/m17-xab-sys-r1.json  --verbose
# ... rounds 2–6, reversing the order each round (this session: r1 rust→sys, r2 sys→rust, r3 rust→sys,
#     r4 rust→sys, r5 sys→rust, r6 rust→sys), logging /proc/loadavg before and after every pass

# 2. binary A/B against the m15-era runtime (build the old tree once, then alternate)
git archive a34ca61332 erts_rust | tar -x -C $TMPDIR/m15-tree
CARGO_TARGET_DIR=$TMPDIR/m15-target erts_rust/scripts/cargo.sh build --release -p beam_rt \
    --manifest-path $TMPDIR/m15-tree/erts_rust/Cargo.toml --offline
BEAM_RT=$TMPDIR/m15-target/release/beam_rt taskset -c 3 setsid -w erts_rust/target/release/bench run \
    --quick --runtime rust --workloads baseline,list_walk,copy_bound,foreign,gc_bound \
    --out-file erts_rust/tools/bench/results/m17-ab-old-r1.json --verbose
BEAM_RT=erts_rust/target/release/beam_rt        taskset -c 3 setsid -w erts_rust/target/release/bench run \
    --quick --runtime rust --workloads baseline,list_walk,copy_bound,foreign,gc_bound \
    --out-file erts_rust/tools/bench/results/m17-ab-new-r1.json --verbose
# ... rounds 2–5, alternating the side order round by round
# `binary_append` is excluded there on purpose: the old runtime needs ≈6 s per call.

# 3. tier control (rust-only; not part of m17 — the last one is m16's)
BEAM_RT_TIER=interpreter taskset -c 3 setsid -w erts_rust/target/release/bench run --quick --runtime rust \
    --workloads $WL --out-file erts_rust/tools/bench/results/<session>-bench-xrun-interp.json

# 4. external targets (sys-only, unpinned so they stay comparable with earlier sessions)
setsid -w erts_rust/target/release/bench run --full --runtime sys \
    --workloads erlc_compiler,elixir_compiler,cowboy \
    --out-file erts_rust/tools/bench/results/m17-bench-external-full.json --verbose

# 5. diffs
erts_rust/target/release/bench compare results/m17-ab-old-r2.json results/m17-ab-new-r2.json
```

Record `/proc/loadavg` before and after every run (`m17-*-load.log` shape), measure **one process at a
time**, and pin every A/B with `taskset -c 3`. The per-round ratio medians in §3.1 are the figures to
recompute: they come straight from the twelve `m17-xab-*` reports (no formula beyond
`rust_case / sys_case` per round, then the median).

Archive `out/` copies into `results/` with **only** the mechanical normalisations
(`runtimes[].program` worktree prefix → `erts_rust/target/release/beam_rt`, the m15-era build →
`$TMPDIR/m15-target/release/beam_rt`; a scratch `$TMPDIR` prefix in `notes` → `$TMPDIR/`).
**Never edit a metric value.**

## 8. Status and still-open items

- **m17 rows:** twelve matched passes × 7 workloads = **42/42 `ok`**, binary A/B **10/10 passes `ok`**
  (three marginals withheld, all `old`-side and load-driven), external **3/3 `ok`** in each of the
  `--full` and `--quick` passes, and **no `failed` row anywhere in the session**. `binary_append`
  produced a real rust marginal at its calibrated `N`=2 000 (m16's interpreter-tier failure was a tier
  limit, not a default-tier one).
- **Still regressed against the oracle** (every per-unit case is; these are the ones worth a line, with
  what the numbers say and who owns them — all handoffs, none fixable from inside `bench`):
  - `binary_append` **40.0×** — 4.22 ms/call against the oracle's 108 µs; the writable-append path is
    `N`-dependent on rust and its `n_rust` is **capped at 2 000 by `beam_rt`'s `RUN_DEADLINE = 60 s`**
    (`crates/beam_rt/src/runtime/scheduler.rs:90`, **not overridable**), so the loop work is ≈8.4 s and
    a larger `N` cannot be measured at all. Owner: the runtime's binary-append path.
  - `emqx_base62_decode` **25.9×**, `bstr_capitalize_token` **22.5×**, `encode` **20.0×**,
    `bstr_to_lower` **18.8×** — real foreign-module call overhead (~6–10 µs/call against 0.3–0.4 µs);
    all four are ≥2× *faster than the m15-era runtime* per §4, so what remains is per-call host-context
    cost, not the foreign code itself.
  - `gc_bound/churn` **18.75×** — per-op allocate-and-abandon cost (~87 µs/op against 4.6 µs/op);
    the A/B has it at parity with the m15-era runtime (1.025×), so this is unchanged, not new.
  - `list_walk` **11.80×** — the session's one **regression** (A/B 1.215×): the BIF-free
    per-element dispatch/return path, ~10–20 % dearer than the m15-era runtime. Owner: the engine's
    baseline tier (same `N` on both sides, so it is not an `N` artifact).
  - `copy_bound/ets_copy` **9.28×** — 2.51 µs/op against 0.28 µs; the A/B has it **0.927×** (faster than
    the m15-era build), so the copy-out work landed and this is the residual ETS copy boundary.
  - `send_recv` **5.39×** / `send_recv_plain` **3.16×** — the two-process copy boundary; A/B at 1.067× /
    0.954× (parity), so unchanged this session.
  - `etf_roundtrip` **2.21×** — the ETF codec + the term-copy round trip; A/B 0.972× (parity).
  - startup **2.51× p50 / 2.56× p99** — the whole-process cost; A/B 1.047×.
- **The `vm_memory_total_bytes` ratio is an accounting artefact, not a footprint win** (§2): the rust
  number halved (13.22 → 6.69 MB, A/B 0.506×) while its OS peak RSS rose 1.13×. A future reader must not
  read the 5.41× ratio as a reduction. The like-for-like footprint rows are `slice` (1.76×) and the
  A/B's `copy_bound` (0.35×) / `foreign` (0.61×).
- **`binary_append` is the only workload whose `N` is bounded by the runtime, not by the loop-count
  rule.** `RUN_DEADLINE = 60 s` (`crates/beam_rt/src/runtime/scheduler.rs:90`) is **non-overridable**
  and caps `n_rust` at 2 000 (≈4.2–4.6 ms/call); any future increase in per-call cost silently turns
  that row into `failed(no-binary-append-run)`, so re-derive `n_rust` from the measured cost before
  changing anything there.
- **The loop workloads' rust peak RSS is boot-dominated** (`list_walk`/`gc_bound` 1.75×) and the
  per-op-retention rows (`foreign` 2.91×, `binary_append` 3.73×) are loop-length-dependent; neither is
  a like-for-like memory comparison. `slice` (N=1) and `baseline`'s `vm_memory_total_bytes` are.
- **`foreign`'s and `binary_append`'s `case_<name>_ns_per_call` remain `N`-dependent on rust**, so they
  are comparable only across runs at the same `N`; the counts are the ones in §3.3.
- **No self-hosted build (contract A.9).** `erlc_compiler` and `elixir_compiler` are `Runtimes::SysOnly`
  and shell out to the system `erlc`/`elixirc`; nothing in the driver runs a compiler under `beam_rt`.
- **Honesty caveats that travel with every figure:** the host is shared and this session ran at 1-min
  load 3–92 (m16 3–76, m15 2–5), so absolute times are not comparable across sessions and only the §4
  binary A/B supports a "changed since" claim; cross-runtime ratios come from the matched rounds (§3),
  and m17 ran no runtime-major primary pass at all; `foreign`/`binary_append` per-call figures are
  `N`-sensitive; `baseline`'s compute loop is native-floor dominated; the external targets were measured
  starved (§6) and should be quoted from m16 at low load.

**History:** prior sessions are the committed `results/m1{0,1,2,3,4,5,6}-*` artifacts (`m17-*` is
current). Every older session's per-unit figures (`copy_bound`, `gc_bound`, `foreign`, `binary_append`)
are **superseded by the current record** and are kept as provenance: they were taken under a different
load band, a different `N` for some cases and, before m15, with the sequential estimator — never quote or
diff them as current. Their external-target rows, load records and `baseline`/`slice` figures remain the
regression history.
