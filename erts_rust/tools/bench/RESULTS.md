# bench — measured results (m14)

This is the definitive summary of the **current** measurement session (`m14`, 2026-10-16) for the
`erts_rust` benchmark driver. Every number here comes from the committed raw artifacts under
`results/m14-*`; regenerate with the Runbook (§7) rather than quoting a stale copy. Raw JSON is
authoritative — this file never edits a metric value.

## 0. The two sides, named

| side | program | version |
|---|---|---|
| **sys**  | the oracle `erl` on `PATH` (`/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/lib/erlang/erts-17.1/bin/erl`) | **Erlang/OTP 29.1.1 / erts-17.1** |
| **rust** | `erts_rust/target/release/beam_rt` at its **default baseline-JIT tier** (`JitMode::Auto` ⇒ `Tier::BaselineJit`) | reports `30` |

Every "sys vs rust" row below is a comparison of those two programs. **Rust-vs-Rust rows** (the
interpreter-pin tier control §5 and the binary-vs-binary A/B §6) are labelled separately and are
**never** a system-ERTS comparison.

## 1. The host and the load record

14 logical cores, shared and usually loaded. `/usr/bin/time` is absent; the harness uses
`date +%s%N` + `/proc/<pid>/status` `VmHWM`/`VmRSS`. Load is recorded before/after every run:

| run | artifact(s) | `/proc/loadavg` (1-min) before → after |
|---|---|---|
| primary cross-runtime (`--full`) | `m14-bench-xrun.json` | 56.76 → 73.66 |
| tier controls (`--quick`, rust-only) | `m14-bench-xrun-{interp,quick}.json` | 74.70 → 43.32 → 40.66 |
| external targets (`--full`, sys-only) | `m14-bench-external.json` | 39.89 → 44.09 |
| `plb` / `emqx` (`--quick`, sys-only) | `m14-bench-plb.json`, `m14-bench-emqx.json` | 43.44 → 59.20 |
| foreign A/B (interleaved, rust-only) | `m14-ab-{head,m13,m10}-r{1,2,3}.json` | 88.99 → 145.18 |

Full logs: `m14-xrun-load.log`, `m14-tier-load.log`, `m14-ext-load.log`, `m14-ab-load.log`;
narrative in `m14-load-record.md`. **The host was heavily loaded** during the A/B (88→145) and
moderately loaded during the primary pass (57→74), so **read the ratios and the deterministic
`case_*_ops/calls/elements` counters, not the absolute wall times.**

## 2. `baseline` — startup distribution + vm memory (sys vs rust)

`bench run --full --runtime all --workloads baseline` (`m14-bench-xrun.json`).

| metric | sys | rust | note |
|---|---|---|---|
| startup `p50` (ms) | **436.5** | **1 373.6** | whole-process wall per `-noshell -eval 'halt().'` (n=50) |
| startup `p99` (ms) | **788.7** | **1 835.5** | |
| startup mean / min / max (ms) | 451.0 / 209.9 / 860.5 | 1 307.1 / 910.9 / 1 902.4 | |
| compute loop mean (ms) | 589.7 | 1 176.9 | `lists:sum(lists:seq(1,1e6))` |
| compute ops/s | 1.696e6 | 8.497e5 | native-floor dominated (`lists:seq/2`+`lists:sum/1`) |
| `erlang:memory(total)` (`vm_memory_total_bytes`) | 36.2 MB | **14.6 MB** | rust **2.48× lower** |
| OS peak RSS (`VmHWM`) | 136.4 MB | 407.4 MB | boot-dominated for rust; **not** like-for-like |

`vm_memory_total_bytes` (the VM's own accounting) and `peak_memory_bytes` (OS `VmHWM`) are
**separate metrics and are never conflated**.

## 3. Per-workload-family ratios (rust ÷ sys, primary marginal)

`m14-bench-xrun.json`, `--full`, both runtimes. Ratio > 1 ⇒ rust dearer.

| workload | case | sys | rust | **rust/sys** |
|---|---|---|---|---|
| `list_walk` | per element | 8.29 ns | 43.70 ns | **5.3×** |
| `copy_bound` | `etf_roundtrip` | 806.7 ns | 109 917.8 ns | **136.3×** |
| `copy_bound` | `ets_copy` | 292.7 ns | 8 006.3 ns | **27.4×** |
| `copy_bound` | `send_recv` | 2 022.8 ns | 213 200.4 ns | **105.4×** |
| `copy_bound` | `send_recv_plain` | 751.0 ns | 84 063.2 ns | **111.9×** |
| `gc_bound` | `churn` | 7 556.6 ns | 229 873.4 ns | **30.4×** |
| `foreign` | `bstr_to_lower` | 591.6 ns | 37 993.1 ns | **64.2×** |
| `foreign` | `bstr_capitalize_token` | 441.8 ns | 32 249.2 ns | **73.0×** |
| `foreign` | `emqx_base62_encode` | 350.1 ns | 27 860.1 ns | **79.6×** |
| `foreign` | `emqx_base62_decode` | 466.5 ns | 25 430.1 ns | **54.5×** |
| `slice` | 9 cases (one call/process) | Σ mean 4 548 ms | Σ mean 9 598 ms | ≈**2.11×** |

Per-unit figures are the startup-cancelled marginals (`(mean wall@N − mean wall@0)/units`); the
loop count `N` per runtime is calibrated so the loop's work dominates boot jitter (§4). The
`send_recv − send_recv_plain` gap attributes the ETF-codec component inside the two-process path.

`slice` runs **one call per process** (`N`=1), so its peak RSS *is* like-for-like; the loop
workloads' rust peak RSS is boot-dominated (§2 caveat).

## 4. Loop-count calibration (m14)

Rule (documented in each case table): **a runtime's timed loop must do enough work that its
marginal stands clear of that runtime's own run-to-run boot/startup jitter.** The rust boot jitter
was measured at ≈±0.1 s (boot ≈0.7 s quiet, 1.4–2.5 s under load), so the rust floor is **≥0.5 s**
of loop work and the two cases whose marginal had collapsed inside the band at m13 target **≳1.5 s**.
`n_sys` was already resolvable and is unchanged. Cross-`N` comparison stays honest only through the
per-op/per-element normalization — never compare raw wall times across different `N`.

| workload | case | `n_rust` m13 → m14 | loop work at m14 `N` |
|---|---|---|---|
| `list_walk` | `list_walk` | 1 000 → **50 000** | ≈1.6 s |
| `copy_bound` | `etf_roundtrip` | 5 000 → **40 000** | ≈2.2 s |
| `copy_bound` | `ets_copy` | 100 000 → **200 000** | ≈1.4 s |
| `copy_bound` | `send_recv` | 2 000 → **10 000** | ≈1.5 s |
| `copy_bound` | `send_recv_plain` | 2 000 → **15 000** | ≈1.7 s |
| `gc_bound` | `churn` | 2 000 → **5 000** | ≈0.7–1.6 s |

**Proof of resolution:** 43 repeated driver runs, strictly sequential — **43/43 resolved** (no
`marginal below timer resolution`), every rust marginal positive and 5×–150× the jitter band, every
sys marginal positive. Raw: `m14-recalibration.csv`; write-up: `m14-recalibration.md`. In the m14
primary pass all 12 rows are `ok` (§8), so both recalibrated cases now publish a number.

## 5. Rust-vs-Rust: execution-tier effect (interpreter pin vs default baseline JIT)

**NOT a system-ERTS comparison** — both sides are `beam_rt`, `--runtime rust` only. Read
**like-for-like**: both controls are `--quick` (`m14-bench-xrun-interp.json` at
`BEAM_RT_TIER=interpreter` vs `m14-bench-xrun-quick.json` default). `interp_over_default` > 1 ⇒ the
interpreter is dearer. Raw: `m14-tier-effect.csv`; diff `m14-compare-tier-effect-quick.txt`.

| case | interp ns | default JIT ns | interp/default |
|---|---|---|---|
| `case_list_walk_ns_per_element` | 248.41 | 36.21 | **6.86×** |
| `case_emqx_base62_decode_ns_per_call` | 91 591.5 | 44 499.4 | 2.06× |
| `case_churn_ns_per_op` | 391 058.5 | 229 110.8 | 1.71× |
| `case_ets_copy_ns_per_op` | 13 686.1 | 9 329.7 | 1.47× |
| `case_emqx_base62_encode_ns_per_call` | 52 767.8 | 39 281.5 | 1.34× |
| `case_bstr_capitalize_token_ns_per_call` | 73 225.1 | 54 545.4 | 1.34× |
| `case_etf_roundtrip_ns_per_op` | 127 562.3 | 111 020.9 | 1.15× |
| `case_bstr_to_lower_ns_per_call` | 70 927.1 | 63 492.4 | 1.12× |
| `case_send_recv_plain_ns_per_op` | 69 273.3 | 67 696.9 | 1.02× |
| `case_send_recv_ns_per_op` | 135 703.0 | 210 373.4 | **0.65×** |

The default baseline JIT is cheaper than the interpreter on every case except `send_recv`, where
the interpreter reads cheaper — a load-sensitive marginal on a heavily loaded host (interp came
after the loaded xrun). These are the whole-process per-unit costs under each tier; the difference
is a genuine runtime-cost observation, not a harness artifact. `m14-compare-tier-effect.txt` is the
as-specified interp(`--quick`)-vs-default(`--full`) form-mismatch companion and is **not** the
matched table.

## 6. External targets (all sys-only, staged in a read-only `$TMPDIR` copy)

| target | figure | instrument |
|---|---|---|
| `erlc_compiler` | **6.038 files/s**, 4 680 loc/s (60 files, wall 9 938 ms) | system `erlc` over OTP stdlib + Elixir `lib/elixir/src` |
| `elixir_compiler` | **1.825 files/s**, 916.3 loc/s (223 files, wall 122 200 ms, compiler startup 943.6 ms) | installed system `elixirc` |
| `cowboy` | **30 690 req/s**, 153 600 requests, 0 errors, latency p50 0.29 / p99 3.41 ms (build 81 790 ms) | `hello_world` relx release, in-process keep-alive load |
| `khepri` | **97 530 ms** wall | `rebar3 eunit` in a scratch copy |
| `plb` | **113 900 ms** wall; test time p50 626.4 / p99 3 939 ms; test mem 88.83 MB | dotnet tool, resolved to **dotnet 9.0.318** via `command_for_min` (rejects two 8.0.425 SDKs) |
| `emqx` | **`skipped(emqx-requires-otp-28)`** — honest skip, no number | checkout pins OTP 28.5.0.7-2; host is OTP 29.1.1 |

Details: `m14-bench-{external,plb,emqx}.json`, `m14-tool-targets-notes.md`.

## 7. Runbook (reproduction)

```sh
# 0. build the driver AND the runtime (from the repo root)
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt -p bench

# 1. primary cross-runtime report
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime all --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/results/m14-bench-xrun.json --verbose

# 2. tier controls (rust-only, BOTH --quick => matched form)
BEAM_RT_TIER=interpreter erts_rust/scripts/cargo.sh run ... run --quick --runtime rust \
    --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/results/m14-bench-xrun-interp.json
erts_rust/scripts/cargo.sh run ... run --quick --runtime rust \
    --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/results/m14-bench-xrun-quick.json

# 3. external targets (sys-only)
erts_rust/scripts/cargo.sh run ... run --full --runtime sys \
    --workloads erlc_compiler,elixir_compiler,cowboy,khepri \
    --out-file erts_rust/tools/bench/results/m14-bench-external.json --verbose
erts_rust/scripts/cargo.sh run ... run --quick --runtime sys --workloads plb \
    --out-file erts_rust/tools/bench/results/m14-bench-plb.json --verbose
erts_rust/scripts/cargo.sh run ... run --quick --runtime sys --workloads emqx \
    --out-file erts_rust/tools/bench/results/m14-bench-emqx.json --verbose

# 4. foreign A/B — interleaved rounds, $BEAM_RT per variant (head = this tree; m13 = a141d6878c;
#    m10 = b4593818b3, each release beam_rt built in its own temp worktree)
BEAM_RT=<variant>/erts_rust/target/release/beam_rt erts_rust/scripts/cargo.sh run ... \
    run --quick --runtime rust --workloads foreign \
    --out-file erts_rust/tools/bench/results/m14-ab-<variant>-r<n>.json

# 5. diffs
erts_rust/target/release/bench compare results/m14-bench-xrun.json              # cross-runtime
erts_rust/target/release/bench compare results/m14-bench-xrun-interp.json \
                                   results/m14-bench-xrun-quick.json            # matched tier diff
erts_rust/target/release/bench compare results/m13-bench-xrun.json \
                                   results/m14-bench-xrun.json                  # vs m13
```

Archive `out/` copies into `results/` with **only** the two mechanical normalisations
(`runtimes[].program` worktree prefix → `erts_rust/target/release/beam_rt`; a `$TMPDIR` prefix in
`notes` → `$TMPDIR/`). **Never edit a metric value.**

## 8. Status and still-open items

- **m14 primary pass: 12/12 rows `ok`.** External pass: 4/4 `ok`. `plb` `ok`. The only non-`ok`
  row in the whole milestone is `emqx` → `skipped(emqx-requires-otp-28)`. **No row emitted
  `marginal below timer resolution`** — the m13 driver measurement gap is **closed** by the
  recalibration (§4).
- **OPEN — `foreign: emqx_base62:encode` vs the m10 rust binary — UNDER INVESTIGATION (no cause
  attributed).** The m13 record reported the m13-era head as 3.2×–3.8× dearer than m10 on `encode`.
  The m14 interleaved A/B (`m14-ab-*`, load 88→145, `m14-ab-summary.csv`) **does not reproduce that
  at head**: within-round `head/m10` ratios are 0.68× / 2.64× / 0.76× (median 0.82×), i.e. head ≈
  m10. The **m13 binary does reproduce it** (within-round `head/m13` = 0.23× / 0.61× / 0.32×; the
  m13 binary is 2.4×–4.3× m10). The other three `foreign` cases scatter similarly
  (`capitalize` 1.74/1.08/1.30×, `lower` 1.73/1.22/1.30×, `decode` 0.89/0.92/1.43×). With the host
  loaded 88→145 the per-call marginal is noisy, so the honest reading is: **the m13 `encode`
  regression is not visible at head in this A/B; another agent is investigating — no cause is
  attributed here.**
- **Caveats carried forward:** the `foreign` per-call metric is `N`-dependent on rust (only
  comparable at matched `N`; every row emits the `case_*_calls = N` companion); the loop workloads'
  rust peak RSS is boot-dominated (not like-for-like — `slice` is); read ratios and the
  deterministic counters, not absolute wall times, on this shared host.
