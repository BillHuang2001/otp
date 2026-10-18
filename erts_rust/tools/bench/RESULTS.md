# bench — measured results (m16)

This is the definitive summary of the **current** measurement session (`m16`, 2026-10-18) for the
`erts_rust` benchmark driver. Every number here comes from the committed raw artifacts under
`results/m16-*`; regenerate with the Runbook (§7) rather than quoting a stale copy.
Raw JSON is authoritative — this file never edits a metric value.

## 0. The two sides, named

| side | program | version |
|---|---|---|
| **sys**  | the oracle `erl` on `PATH` (`/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/lib/erlang/erts-17.1/bin/erl`) | **Erlang/OTP 29.1.1 / erts-17.1** |
| **rust** | `erts_rust/target/release/beam_rt` at its **default baseline-JIT tier** (`JitMode::Auto` ⇒ `Tier::BaselineJit`) | reports `30` |

Every "sys vs rust" row below is a comparison of those two programs. **Rust-vs-Rust rows** (the
interpreter-pin tier control §5 and the binary A/B §4) are labelled separately and are **never** a
system-ERTS comparison.

Artifacts for this session (all committed under `results/`):
`m16-bench-xrun.json` (primary, `--full --runtime all`, pinned), `m16-xab-{rust,sys}-r{1,2,3}.json` (the
**matched cross-runtime rounds** behind §3, `--quick`, pinned), `m16-ab-{new,old}-r{1,2,3}.json` (the
**binary A/B** against the m15-era runtime `a34ca61332` behind §4), `m16-bench-xrun-interp.json` (tier
control), `m16-bench-external.json` / `m16-bench-plb.json` / `m16-bench-emqx.json` (external targets),
`m16-pin-control-{pinned,unpinned}.json`, the diffs
`m16-compare-{cross-runtime,tier-effect-quick,vs-m15-xrun,vs-m15-external,ab-binary}.txt`, and
`m16-{xrun,xab,ab,ext}-load.log` + `m16-load-record.md`.

## 1. The host and the load record

14 logical cores, shared. `/usr/bin/time` is absent; the harness uses `date +%s%N` + `/proc/<pid>/status`
`VmHWM`/`VmRSS`. **Every A/B run is pinned to a P-core with `taskset -c 3`**; the single-side external
passes are unpinned so they stay comparable with m15. Load was recorded before and after every run:

| run | artifact(s) | pin | `/proc/loadavg` (1-min) before → after |
|---|---|---|---|
| primary cross-runtime (`--full`) | `m16-bench-xrun.json` | `taskset -c 3` | 15.49 → 9.41 |
| matched cross-runtime rounds 1–3 | `m16-xab-{rust,sys}-r{1,2,3}.json` | `taskset -c 3` | 7.62 → 8.63 (per-round values in `m16-xab-load.log`) |
| binary A/B rounds 1–3 | `m16-ab-{new,old}-r{1,2,3}.json` | `taskset -c 3` | 55.85 → 7.84 |
| tier control | `m16-bench-xrun-interp.json` | `taskset -c 3` | 7.01 → 3.04 |
| external targets (`--full`, sys-only) | `m16-bench-external.json` | none | 3.04 → 8.94 |
| `plb` / `emqx` (`--quick`, sys-only) | `m16-bench-plb.json`, `m16-bench-emqx.json` | none | 4.66 → 4.66 |

Full logs: `m16-xrun-load.log`, `m16-xab-load.log`, `m16-ab-load.log`, `m16-ext-load.log`; table +
narrative in `m16-load-record.md`.

**m16 was a loaded session (1-min load 3–76, mostly 5–15), unlike m15 (2–5).** Two consequences, both of
which must be stated with the numbers:

- absolute wall times/throughputs are **load-contaminated on both sides** and are not comparable with
  m15's absolute figures; the sys side is remarkably load-insensitive (`list_walk` 2.34 vs 2.48 ns/element,
  `etf_roundtrip` 657.5 vs 652.7 ns, `churn` 4 256.9 vs 4 266.4 ns) while the rust side absorbs most of
  the variance — so a cross-session **ratio** comparison is only valid through the **binary A/B of §4**;
- **read the ratios and the deterministic `case_*_ops/calls/elements` counters, never a single absolute
  wall time as a constant.**

**Pinning is not a confounder** (`m16-pin-control-{pinned,unpinned}.json`, back-to-back `baseline` runs at
load 28.06): startup p50 sys 105.8 vs 109.1 ms, rust 395.8 vs 402.6 ms, with the pinned p99 strictly
tighter on both sides. `taskset -c 3` is neutral for the mean and better for the tail.

## 2. `baseline` — startup distribution + vm memory (sys vs rust)

`--full`, pinned; the matched-round medians (§3) agree to within 1 % on p50 and 0.7 % on p99.

| metric | sys | rust | rust/sys |
|---|---|---|---|
| startup `p50` (ms) | **104.7** | **397.0** | **3.79×** (matched 3.76×) |
| startup `p99` (ms) | **115.4** | **432.1** | **3.74×** (matched 3.72×) |
| startup mean / min / max (ms) | 105.3 / 103.5 / 121.0 | 401.9 / 390.6 / 432.5 | |
| compute loop mean (ms) | 164.4 | 468.8 | 2.85× |
| compute ops/s | 6.083e6 | 2.133e6 | 0.35× |
| `erlang:memory(total)` (`vm_memory_total_bytes`) | 36.20 MB | **6.79 MB** | rust **5.33× lower** |
| OS peak RSS (`VmHWM`) | 135.2 MB | 171.4 MB | 1.27× |

`vm_memory_total_bytes` (the VM's own accounting) and `peak_memory_bytes` (OS `VmHWM`) are **separate
metrics and are never conflated**.

Two facts on this row need their caveats stated with them:

- the rust `vm_memory_total_bytes` fell from **13.22 MB** (m15) to **6.79 MB** while its OS peak RSS rose
  from **133.3 MB** to **171.4 MB**; the two moving in opposite directions is an **accounting** change, so
  the improved 2.74× → 5.33× ratio must not be read as a footprint reduction until the accounting is
  confirmed by the runtime's owners (handoff, §8);
- the **startup ratio regressed**, and the binary A/B of §4 attributes a real **1.17×** of it to the
  current runtime (rust startup p50 340.4 → 399.7 ms at matched conditions); the remainder of the difference
  from m15's 2.53× is host load, not code.

## 3. The matched cross-runtime record (the headline)

`m16-xab-{rust,sys}-r{1,2,3}.json` — six `--quick` passes, alternating rust-only and sys-only, pinned, one
process at a time, so each workload's two sides are sampled within a couple of minutes of each other. The
figure quoted is the **median of the three rounds**; the per-round ratios are shown because their spread is
the stability evidence. Ratio > 1 ⇒ rust dearer.

### 3.1 The ratios

| workload | case | sys | rust | **rust/sys** | per-round rust/sys |
|---|---|---|---|---|---|
| `list_walk` | per element | 2.3 ns | 23.0 ns | **9.86×** | 9.86 9.70 10.00 |
| `copy_bound` | `etf_roundtrip` | 657.2 ns | 2 110.3 ns | **3.21×** | 3.26 3.21 3.20 |
| `copy_bound` | `ets_copy` | 227.5 ns | 4 625.7 ns | **20.34×** | 20.60 20.16 20.79 |
| `copy_bound` | `send_recv` | 1 503.3 ns | 8 891.4 ns | **5.92×** | 5.92 5.91 5.97 |
| `copy_bound` | `send_recv_plain` | 670.8 ns | 1 904.8 ns | **2.84×** | 2.87 2.79 2.82 |
| `gc_bound` | `churn` | 4 260.2 ns | 72 726.5 ns | **17.07×** | 17.48 15.78 17.07 |
| `foreign` | `bstr_to_lower` | 325.0 ns | 16 003.5 ns | **49.24×** | 44.97 49.25 49.21 |
| `foreign` | `bstr_capitalize_token` | 329.1 ns | 17 746.6 ns | **53.93×** | 53.54 51.23 55.51 |
| `foreign` | `emqx_base62_encode` | 219.1 ns | 126 013.8 ns | **575.2×** | 568.99 575.42 575.44 |
| `foreign` | `emqx_base62_decode` | 298.2 ns | 18 387.8 ns | **61.65×** | 61.22 59.65 63.55 |
| `binary_append` | per `expand/1` call | 87 662.7 ns | 3 330 897.7 ns | **38.0×** | 38.00 37.27 39.19 |
| `slice` | 9 cases, one call/process | 116.1–119.9 ms | 393.7–441.4 ms | ≈**3.40×** | 3.37–3.47 |

The `send_recv − send_recv_plain` gap attributes the ETF-codec component *inside* the two-process path (the
two cases differ only in whether the receiver re-encodes the payload). Cross-check: the same six `--quick`
workloads inside the `--full` runtime-major primary pass (`m16-bench-xrun.json`) reproduce these ratios
within 6 % — except `list_walk`, which reads **11.69×** there against **9.86×** matched. That 18 % is the
largest bias the runtime-major ordering introduces (all `sys` rows, then all `rust` rows, minutes apart on a
drifting host), which is exactly why the headline table above comes from the matched rounds and why
`list_walk` above all must never be quoted from a runtime-major pass.

**No row withheld a figure in this session**: `marginal below timer resolution` appears nowhere in
`m16-bench-xrun.json`, the six `m16-xab-*` reports or the six `m16-ab-*` reports. The single non-`ok` row is
the `binary_append` interpreter-tier run of §5, which is `failed(no-binary-append-run)` with its exact
reason recorded.

### 3.2 Memory (like-for-like, `slice`)

`slice` runs one call per process (`N`=1), so its wall time and peak RSS are whole-process figures. Peak RSS
**67.5 MB (sys) vs 118.2 MB (rust)** ⇒ **1.75×** (m15: 1.69×), stable to ±0.02× across all nine cases. The
loop workloads read **1.76×** on the same metric, still **boot-dominated** (the resident preloaded-module set
dwarfs the loop's footprint) and therefore still **not** a memory comparison; the like-for-like memory
signals are `slice`'s peak RSS and `baseline`'s `vm_memory_total_bytes`.

### 3.3 How to read a per-unit figure

Every per-unit figure is a **paired, startup-cancelled marginal**: each round runs the `N`-unit pass and
then the `0`-unit pass **back to back**, and the statistic is the **median of the per-round differences**
(`src/marginal.rs`), withheld — no metric, plus an explicit `marginal below timer resolution` note — unless
it clears `3 × max(1 ms, 1.4826·MAD)`. The sequential all-`N`-then-all-`0` mean schedule this replaced
published `etf_roundtrip` at 136.3× where the interleaved measurement gives ≈2.4×; **do not revert it.**

`N` is per runtime, so the report always carries the deterministic, contention-immune companion that names
it (sys / rust): `etf_roundtrip_ops` 1 500 000 / 500 000, `ets_copy_ops` 2 000 000 / 250 000,
`send_recv_ops` 500 000 / 90 000, `send_recv_plain_ops` 1 000 000 / 300 000, `churn_ops` 200 000 / 10 000,
`list_walk_elements` 100 000 000 / 50 000 000, `foreign` `calls` 1 500 000 / 50 000 (`bstr_to_lower`),
1 500 000 / 40 000 (`bstr_capitalize_token`), 3 000 000 / 60 000 (`emqx_base62_encode`), 3 000 000 /
60 000 (`emqx_base62_decode`), `binary_append_calls` 50 000 / 2 000 (`binary_append_bytes` 3 276 800 000 /
131 072 000). **Never compare raw `wall_ms` across two sides with different `N`** — only the per-unit
normalization is comparable.

## 4. What changed since m15 (the binary A/B — this is where a regression claim is made)

`m16-ab-{new,old}-r{1,2,3}.json`: the **same `bench` binary** driving two `beam_rt` builds — `new` = HEAD,
`old` = the m15-era tree at `a34ca61332` — rust-only `--quick`, three rounds per side, **alternating order
round by round**, pinned to core 3. This isolates *code* change from host load and from the session's
different `N`-independent conditions; it is the only valid way to say "this regressed since m15".

| rust-only metric | m15-era | HEAD | **HEAD/m15-era** |
|---|---|---|---|
| `baseline` startup p50 | 340.4 ms | 399.7 ms | **1.17×** (regressed) |
| `copy_bound/etf_roundtrip` | 1 749.5 ns | 2 451.0 ns | **1.40×** (regressed) |
| `copy_bound/ets_copy` | 3 111.0 ns | 5 056.5 ns | **1.63×** (regressed) |
| `copy_bound/send_recv` | 9 324.7 ns | 9 074.2 ns | 0.97× |
| `copy_bound/send_recv_plain` | 2 356.3 ns | 1 921.8 ns | **0.82×** (faster) |
| `foreign/bstr_to_lower` | 16 745.7 ns | 16 895.0 ns | 1.01× (parity) |
| `foreign/bstr_capitalize_token` | 19 837.0 ns | 18 134.9 ns | **0.91×** (faster) |
| `foreign/emqx_base62_decode` | 10 512.0 ns | 18 545.9 ns | **1.76×** (regressed) |
| `foreign/emqx_base62_encode` | 18 525.8 ns | 132 181.7 ns | **7.14×** (regressed) |
| `gc_bound/churn` | 71 597.5 ns | 72 652.2 ns | 1.01× (parity) |
| `list_walk` | 29.0 ns | 24.3 ns | **0.84×** (faster) |

**The headline of the session is a win, and it is not in that table because the old runtime cannot finish
it**: `binary_append` (16 KiB in → 64 KiB out, one writable payload, 16 384 four-byte segments) costs
**≈6.3–7.2 s per call on the m15-era runtime and ≈4.4–4.9 ms on HEAD — ≈1 400× cheaper** at the runtime
level (direct ladder in `m16-load-record.md`; the old side hits its own `run` deadline at `N`=100). The
same change cut the rust `etf_roundtrip` peak RSS from **459 MB to 120 MB** (−74 %) and kept every
allocation-validating case (`by construction` bound-variable pattern / `byte_size` / `tuple_size`) green.

**Regressions, stated plainly, with what they are likely to be** (each is new benchmark evidence handed to
the owning crates; none is fixed from inside this node):

- `emqx_base62:encode` at **7.14×** is by far the largest; it is the one case whose **interpreter/default
  ratio is ≈1.0** (§5), i.e. the cost is *not* dispatch and *not* tier-sensitive — it scales with the
  amount of binary/list building the function does per call;
- `ets_copy` (1.63×) and `etf_roundtrip` (1.40×) regressed together, and **both are the copy-out path**
  (`ets:insert/lookup`, `term_to_binary`/`binary_to_term`) — consistent with a copy that became an
  unconditional move rather than a shared/borrowed one;
- `emqx_base62_decode` 1.76× regressed while `bstr_to_lower` stays at parity and `bstr_capitalize_token`
  improved, so this is not a uniform per-call tax;
- startup 1.17× — the boot path got dearer.

Which source change carries each of these is the runtime owners' call; this node's contribution is the
matched measurement.

## 5. Rust-vs-Rust: execution-tier effect (interpreter pin vs default baseline JIT)

**NOT a system-ERTS comparison** — both sides are `beam_rt`, rust-only `--quick` (matched form):
`m16-bench-xrun-interp.json` at `BEAM_RT_TIER=interpreter` vs the median of the three default-tier
`m16-xab-rust-r{1,2,3}.json`. `interp/default` > 1 ⇒ the interpreter is dearer. Raw diff:
`m16-compare-tier-effect-quick.txt`.

| case | interp ns | default JIT ns | interp/default |
|---|---|---|---|
| `case_list_walk_ns_per_element` | 135.0 | 23.0 | **5.87×** |
| `case_emqx_base62_decode_ns_per_call` | 64 044.3 | 18 387.8 | 3.48× |
| `case_bstr_to_lower_ns_per_call` | 42 415.5 | 16 003.5 | 2.65× |
| `case_bstr_capitalize_token_ns_per_call` | 46 333.3 | 17 746.6 | 2.61× |
| `case_churn_ns_per_op` | 148 691.5 | 72 726.5 | 2.05× |
| `case_send_recv_plain_ns_per_op` | 3 739.8 | 1 904.8 | 1.96× |
| `case_send_recv_ns_per_op` | 11 169.3 | 8 891.4 | 1.26× |
| `case_etf_roundtrip_ns_per_op` | 2 489.2 | 2 110.3 | 1.18× |
| `case_ets_copy_ns_per_op` | 5 373.0 | 4 625.7 | 1.16× |
| `case_emqx_base62_encode_ns_per_call` | 129 289.2 | 126 013.8 | 1.03× |

The default baseline JIT is cheaper than the interpreter on **every** case. `binary_append` is **not in
this table**: at the calibrated `n_rust` = 2 000 the interpreter tier does not finish one `run` invocation
inside `beam_rt`'s own 60 s limit, so the row is honestly `failed(no-binary-append-run)` with the reason
recorded — the tier control measures 10 of the 11 cases by design, not by silently substituting a number.

## 6. External targets (all sys-only, staged in a read-only `$TMPDIR` copy)

| target | figure | m16/m15 | instrument |
|---|---|---|---|
| `erlc_compiler` | **9.618 files/s**, 7 454.7 loc/s (60 files, wall 6 238 ms) | 0.97× | system `erlc` over OTP stdlib + Elixir `lib/elixir/src` |
| `elixir_compiler` | **4.897 files/s**, 2 458.9 loc/s (223 files, wall 45 534 ms, compiler startup 288.1 ms) | 1.04× | installed system `elixirc` (per-file fallback: version skew) |
| `cowboy` | **194 331 req/s**, 972 548 requests, 0 errors, latency p50 0.1 / p99 0.8 ms (build 44 358 ms) | 0.98× | `hello_world` relx release, in-process keep-alive load |
| `khepri` | **78 526 ms** wall | 0.97× | `rebar3 eunit` in a scratch copy |
| `plb` | **68 293 ms** wall (build 24 248 ms); test time p50 543.9 / p99 3 397.4 ms; test mem 91.6 MB | 1.01× | dotnet tool, resolved to **dotnet 9.0.318** via `command_for_min` (rejects two 8.0.425 SDKs) |
| `emqx` | **`skipped(emqx-requires-otp-28)`** — honest skip, no number | — | checkout pins OTP 28.5.0.7-2; host is OTP 29.1.1 |

All five measured targets land within ±4 % of m15 on a host that is running 3–15× the load, so **nothing
moved outside the noise**: these are the same numbers in the same place, restated because the record must be
current. Details: `m16-bench-{external,plb,emqx}.json`, the diff `m16-compare-vs-m15-external.txt`.

## 7. Runbook (reproduction)

```sh
# 0. build the driver AND the runtime (from the repo root)
erts_rust/scripts/cargo.sh build --release -p beam_rt -p bench --manifest-path erts_rust/Cargo.toml --offline

# 1. primary cross-runtime report (pinned; one process at a time)
taskset -c 3 erts_rust/target/release/bench run --full --runtime all \
    --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound,binary_append \
    --out-file erts_rust/tools/bench/results/m16-bench-xrun.json --verbose

# 2. the matched cross-runtime rounds (the §3 headline) — alternate sides per round
WL=baseline,slice,foreign,list_walk,copy_bound,gc_bound,binary_append
taskset -c 3 erts_rust/target/release/bench run --quick --runtime rust --workloads $WL --out-file .../m16-xab-rust-r1.json
taskset -c 3 erts_rust/target/release/bench run --quick --runtime sys  --workloads $WL --out-file .../m16-xab-sys-r1.json
# ... and rounds 2 and 3, reversing the order each round

# 3. tier control (rust-only; the default-tier partner is m16-xab-rust-r{1,2,3}.json)
BEAM_RT_TIER=interpreter taskset -c 3 erts_rust/target/release/bench run --quick --runtime rust \
    --workloads $WL --out-file .../m16-bench-xrun-interp.json

# 4. binary A/B against the m15-era runtime (build the old tree once, then alternate)
git archive a34ca61332 erts_rust | tar -x -C $TMPDIR/m15-tree
CARGO_TARGET_DIR=$TMPDIR/m15-target erts_rust/scripts/cargo.sh build --release -p beam_rt \
    --manifest-path $TMPDIR/m15-tree/erts_rust/Cargo.toml --offline
BEAM_RT=$TMPDIR/m15-target/release/beam_rt taskset -c 3 ... run --quick --runtime rust \
    --workloads baseline,list_walk,copy_bound,foreign,gc_bound --out-file .../m16-ab-old-r1.json
# `binary_append` is excluded there on purpose: the old runtime needs ≈6 s per call.

# 5. external targets (sys-only, unpinned so they stay comparable with earlier sessions)
erts_rust/target/release/bench run --full  --runtime sys --workloads erlc_compiler,elixir_compiler,cowboy,khepri --out-file ...
erts_rust/target/release/bench run --quick --runtime sys --workloads plb   --out-file ...
erts_rust/target/release/bench run --quick --runtime sys --workloads emqx  --out-file ...

# 6. diffs
erts_rust/target/release/bench compare results/m16-bench-xrun.json                     # cross-runtime
erts_rust/target/release/bench compare results/m16-bench-xrun-interp.json results/m16-xab-rust-r2.json
erts_rust/target/release/bench compare results/m16-ab-old-r2.json results/m16-ab-new-r2.json
erts_rust/target/release/bench compare results/m15-bench-xrun.json results/m16-bench-xrun.json
```

Record `/proc/loadavg` before and after every run (`m16-*-load.log` shape), measure **one process at a
time**, and pin every A/B with `taskset -c 3`.

Archive `out/` copies into `results/` with **only** the mechanical normalisations
(`runtimes[].program` worktree prefix → `erts_rust/target/release/beam_rt`; a scratch `$TMPDIR` prefix in
`notes` → `$TMPDIR/`). **Never edit a metric value.**

## 8. Status and still-open items

- **m16 rows:** primary **14/14 `ok`**, matched rounds **6/6 passes `ok`**, binary A/B **6/6 passes `ok`**,
  external **4/4 `ok`**, `plb` **`ok`**, `emqx` **`skipped(emqx-requires-otp-28)`**, and the only `failed`
  row in the session is the interpreter-tier `binary_append` (§5), which is a real limit of the
  interpreter tier at the calibrated `N`, recorded with its reason.
- **Open — three per-call regressions since m15 are real and unexplained in this node** (§4):
  `emqx_base62:encode` 7.14×, `ets_copy` 1.63×, `etf_roundtrip` 1.40×, plus `emqx_base62_decode` 1.76× and
  a 1.17× startup regression. They are handoffs to the runtime crates; the evidence is the interleaved
  binary A/B, which is load-proof.
- **Open — the rust `vm_memory_total_bytes` accounting** halved (13.22 → 6.79 MB) while OS peak RSS rose
  29 %; the metric is honest but its *meaning* changed, so the 5.33× ratio must not be quoted as a
  footprint win until the accounting is confirmed.
- **Open — the rust-vs-sys per-unit gaps themselves** (§3.1: 2.84×–575×). These are real runtime costs, not
  harness artifacts: the startup-cancelled metric reproduces from a plain shell loop calling
  `beam_rt -pa <dir> run <mod.beam> <mod>:<fun>/1 <N>`, so CLI re-entry, per-call module reloads and
  per-call GC are ruled out. The causes are `beam_rt` measurements owned by `../../crates/beam_rt/PERF.md`
  and `CONTEXT.md`, not `bench` output.
- **Open — `binary_append`'s rust per-call cost is `N`-dependent and its peak RSS grows ≈63 KB/call** on top
  of a ≈113 MB boot baseline (N=0 → 113 MB, 1 000 → 177 MB, 2 000 → 252 MB): a per-op retention, not a
  correctness problem (every call is validated by the deterministic `N × 65 536` stdout). The rust `N` is
  capped at 2 000 by `beam_rt`'s 60 s `run` deadline, so a per-call figure is comparable only at a matched
  `N`; the same caveat already governs `foreign`.
- **Open — the loop workloads' rust peak RSS is boot-dominated** and is not a like-for-like memory
  comparison; `slice` (N=1) and `baseline`'s `vm_memory_total_bytes` are.
- **Open — no self-hosted build (contract A.9).** `erlc_compiler` and `elixir_compiler` are
  `Runtimes::SysOnly` and shell out to the system `erlc`/`elixirc`; nothing in the driver runs a compiler
  under `beam_rt`.
- **Honesty caveats that travel with every figure:** the host is shared and this session ran at 1-min load
  3–76 (m15 at 2–5), so absolute times are not comparable across sessions and only the §4 binary A/B
  supports a "changed since m15" claim; cross-runtime ratios are taken from the matched rounds (§3), not
  from the runtime-major primary pass alone; `foreign`/`binary_append` per-call figures are `N`-sensitive;
  `baseline`'s compute loop is native-floor dominated.

**History:** prior sessions are the committed `results/m1{0,1,2,3,4,5}-*` artifacts (`m16-*` is current).
Every older session's per-unit figures (`copy_bound`, `gc_bound`, `foreign`, and now `binary_append`) are
**superseded by the current record** and are kept as provenance: they were taken under a different load
band, a different `N` for some cases and, before m15, with the sequential estimator — never quote or diff
them as current. Their external-target rows, load records and `baseline`/`slice` figures remain the
regression history.
