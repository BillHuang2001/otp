# Rust-ERTS benchmark results — current measurement run (m12)

Reproducible numbers for the Rust-ERTS rewrite (`erts_rust/`) measured against the target projects, produced by the `bench` driver (`erts_rust/tools/bench`).
Everything below is **measured, never modelled**.
A workload that could not run is reported with a *verified* reason and **no number** — never an estimate.

Measured on the host in §1 on **2026-10-15 14:00–15:03 UTC** (= 2026-10-15 22:00–23:03 +0800) from the tree **`ee3e426db0`**, with the driver and `beam_rt` both built from that tree (`--release -p beam_rt -p bench`); the m12 measurement artifacts were committed as **`bdadd02333`**, the milestone narrative as **`a63099d43f`**.
Raw harness JSON is committed under `results/` as `m12-*`; the previous runs are kept as `m11-*` (2026-10-14), `m10-*` (2026-10-13), `m9-*` (2026-10-13), `m6-*` (2026-10-11), `m5-*` (2026-10-09) and `m4-*` (2026-10-08) and are the regression baselines for the delta section (§7).

**What is new at m12 (the deliverable of this milestone).** (1) The **Θ(N³) `foreign` per-call regression is FIXED** (commits `07a5720639` + `7d84b12b44`, owned by `beam_rt`/`beam_process`, **outside this node**): the corrected cross-runtime `foreign` band is **38.9×–66.9×** (m11: 60.2×–367.1×), and the correction is re-derived here from a fresh interleaved A/B against the m10 binary — `results/m12-foreign-regression.md`, `results/m12-foreign-ab-{head,m10}.json` (§3.4). (2) The `run` action now **drives the full `bootstrap/bin/start` boot script** (commit `85ee33b2f8`), so **boot completes** — `beam_rt -boot bootstrap/bin/start` **exits 0** (m11 recorded exit 3 / "directive 18 of 22") — but that same change leaves an **OPEN** `run`-route latency defect: **≈2.1 s fixed startup** on every `run`/`-eval`/`-boot` action, a **~25.5 ms per-blocked-round** scheduler cost (the booted kernel's `os:set_signal/2` self-pipe keeps `PortRegistry::has_nothing_to_poll()` false, so every blocked round sleeps the full `PORT_POLL_SLICE` = 25 ms), and a **Θ(atom-table) `binary_to_term`** inflation. Both are `beam_rt` defects, **not** target-project defects — `results/m12-run-route-latency.md` (§3.9, §3.10, §8 item 1). (3) `khepri` and `plb` **now measure** (`ok`); `emqx` still **skips honestly**.

> **Naming both sides of every ratio (read this before quoting a number).** `sys` = the **system ERTS** (OTP **29.1.1** / erts-17.1, the oracle `erl`, a 14-scheduler SMP VM). `rust` = **`beam_rt`**, the Rust runtime, at its **default execution tier** (the native baseline JIT). A third, distinct column exists — **`rust` with `BEAM_RT_TIER=interpreter`** — and it is a **Rust-vs-Rust tier effect, NOT a system-ERTS comparison** (§3.9, §7). The m9 record once published a `foreign` figure of **0.71×–0.84×** as "of the system ERTS" when it was actually that Rust-vs-Rust tier effect; against the system ERTS the same loop was **46.2×–74.2×** dearer. **Every ratio in this document names both sides.**
> **The target applications are NOT run by the Rust runtime.** `cowboy`, `khepri`, `plb`, `erlc_compiler`, `elixir_compiler` and `emqx` are executed by their **own system-ERTS / system-toolchain** (`erlc`, `elixirc`, the Cowboy release on the oracle `erl`, `rebar3`'s Erlang, the `.NET` tool). They are **reference** figures for the sys-only surfaces (§4), never Rust-runtime results.

> **This is a shared, usually-loaded host** (§1, §6.1): absolute latencies/throughputs move several-fold with load, so **ratios are the signal** and a single contended sample is never a constant. The m12 passes ran at 1-min loads `65.28 → 6.93` (cross-runtime, `--full`), `3.91 → 11.69` (both tier controls, `--quick`), `10.91 → 9.34` (external targets), `9.34 → 7.85` (PLB), `7.85 → 7.54` (EMQX), `10.26 → 8.86` (the run-route probes) and `6.72 → 5.73` (the `foreign` A/B). The cross-runtime pass **fell** during the run, so its later (`rust`) side ran under a *lighter* load; the `rust` absolute walls are additionally **boot-dominated at m12** (§3.9), which is why only the per-op/per-element/per-call marginals and the deterministic counters are read as cross-runtime signal.

## Verdict

**(a) / (b) Which targets run, which skip.** Five of the six external target projects run; one (`emqx`) skips with a verified toolchain reason (never a fabricated number). All five running rows are executed by their **own system toolchain**, not by the Rust runtime.

| target project | runs? | executed by | result / verified reason |
|---|---|---|---|
| `khepri` (tree DB) | **runs** | `rebar3`'s Erlang (OTP 28 escript; §4.2) | `ok` — `rebar3 eunit` wall 77 564.34 ms |
| `cowboy` (HTTP server) | **runs** | the oracle `erl` release | `ok` — 165 489.21 req/s, p50 0.0779 ms / p99 0.7424 ms |
| `erlc_compiler` (Erlang compiler) | **runs** | system `erlc` (OTP 29) | `ok` — 8.769 files/s over the stdlib+Elixir corpus |
| `elixir_compiler` (Elixir compiler) | **runs** | installed `elixirc` 1.20.4 | `ok` — 4.433 files/s over 105 sources / 223 `.beam` |
| `plb` (Benchmarks Game kernels, .NET) | **runs** | the `.NET` tool (`dotnet` 9.0.318) | `ok` — wall 70 648.46 ms; `test_time_ms` p50 595.13 / p99 3 565.99 (n=10) |
| `emqx` (MQTT broker) | **skips** | — | `skipped(emqx-requires-otp-28)` — EMQX pins OTP **28.5.0.7-2** (`env.sh:3`/`.tool-versions:1`), the host `erl` is OTP 29.1.1 / erts-17.1; not satisfiable without a full OTP-28 toolchain swap (§4.3, §5) |

The six workloads the Rust runtime itself can host (`Runtimes::Both`) — `baseline`, `slice`, `foreign`, `list_walk`, `copy_bound`, `gc_bound` — all ran: **12/12 rows `ok`** (0 skipped, 0 unsupported, 0 failed). Every other workload is `SysOnly` and would report `skipped(runtime-unsupported)` under `rust` (§5).

**(c) Ratio to the system-ERTS oracle, per cross-runtime workload.** `rust/sys` is the **Rust default tier** cost as a multiple of the **system-ERTS** cost (`<1` ⇒ Rust cheaper, `>1` ⇒ Rust dearer). Values are the m12 `xrun` (§3, `--full`, 1-min load 65.28 → 6.93).

| workload | surface | headline metric | sys (system ERTS 29.1.1) | rust (`beam_rt`, default tier) | rust/sys | reading |
|---|---|---|---|---|---|---|
| `baseline` | startup / CLI | `startup_ms` (mean, whole process) | 120.6697 ms | 2 298.6279 ms | **19.05×** | Rust dearer — **the OPEN `run`-route boot (~2.1 s fixed)**; m11 was 0.135× |
| `baseline` | VM memory | `vm_memory_total_bytes` | 36 211 168 B | 15 892 346 B | **0.439×** | Rust **2.28× lower** VM memory |
| `slice` (9 programs) | compute | `case_*_wall_ms` (mean, per case) | 132.08–152.71 ms | 2 097.64–2 366.64 ms | **13.7×–17.5×** | Rust dearer — whole-process wall, **boot-dominated** (one process per call) |
| `foreign` (4 cases) | compute | `case_*_ns_per_call` | 231.62–419.77 ns | 14 919.34–23 429.88 ns | **38.9×–66.9×** | Rust dearer — **the Θ(N³) fix** (m11: 60.2×–367.1×) |
| `list_walk` | compute | `case_list_walk_ns_per_element` | 7.6021 ns | 110.2061 ns | **14.50×** | Rust dearer (per-element dispatch) |
| `copy_bound` (4 cases) | messaging | `case_*_ns_per_op` | 289.51–1 584.36 ns | 2 883.75–25 632 422.23 ns | **9.96×–16 178.4×** | `send_recv`'s ratio is the **~25.5 ms blocked-round defect**; `send_recv_plain` rust **timed out** |
| `gc_bound` | gc | `case_churn_ns_per_op` | 4 658.7133 ns | 184 398.8576 ns | **39.58×** | Rust dearer (allocate-and-abandon churn) |

**(d) Tier effect (Rust vs Rust — never a system-ERTS comparison).** Read from the matched-form `--quick` pair (`m12-compare-tier-effect-quick.txt`, `a` = interpreter, `b` = default): `gc_bound churn` **2.07× cheaper** on the baseline JIT; `foreign` 1.03×–1.80× cheaper; `send_recv` **0.999× = parity**; `etf`/`ets`/`startup`/`vm_memory` parity; `slice` 0.95×–1.06× (boot-dominated); `list_walk`'s default-tier marginal was below timer resolution in the `--quick` form (§3.9). **The m11 interpreter pathology on `foreign` is gone** (m11 interpreter up to 4 178 832 ns/call; m12 interpreter 21 458–37 786 ns/call) — it was the *same* shared `beam_rt`/`beam_process` Θ(N³) defect, not an interpreter-tier defect.

## Coverage — workload ↔ rewrite surface

| workload | category | contracts | runtimes | what it exercises |
|---|---|---|---|---|
| `baseline` | startup | A.6, A.7 | both | CLI/`-eval` path, startup latency, `erlang:memory/0,1`, fixed compute loop |
| `slice` | compute | A.1, A.2 | both | real `erlc`-compiled `.beam` execution, stdlib calls (`calendar`/`queue`/`gb_trees`/`base64`/`ordsets`/`string`) |
| `foreign` | compute | A.1, A.2 | both | real Cowboy (`cowboy_bstr`) + EMQX (`emqx_base62`) modules, per-call dispatch |
| `list_walk` | compute | A.1, A.2 | both | BIF-free recursive list walk — per-element dispatch/return |
| `copy_bound` | messaging | A.1, A.5 | both | ETF wire codec, ETS copy boundary, two-process send/receive (with and without the codec) |
| `gc_bound` | gc | A.1 | both | allocate-and-abandon garbage churn |
| `erlc_compiler` | compiler | A.9 | sys-only | system `erlc` throughput |
| `elixir_compiler` | compiler | A.1, A.9 | sys-only | installed `elixirc` throughput |
| `plb` | compute | — | sys-only | Benchmarks-Game kernels through the `.NET` tool |
| `cowboy` | io | — | sys-only | Cowboy `hello_world` release + HTTP/1.1 load |
| `khepri` | gc | — | sys-only | Khepri `rebar3 eunit` |
| `emqx` | messaging | — | sys-only | EMQX umbrella build (`make`/`mix`) |

## Host contention (read before quoting any absolute number)

`/proc/loadavg` is **host-wide** and this machine is shared: across the m12 session the 1-min value ran from `3.91` to `65.28`. The cross-runtime pass started at `65.28` (the batch's start bracket) and ended at `6.93`; the external-target pass sat at `10.91 → 9.34`; PLB `9.34 → 7.85`; the `foreign` A/B `6.72 → 5.73`. Absolute latencies/walls/throughputs are therefore **inflated** and must not be read as machine-speed constants; the load-robust signals are the **per-op / per-element / per-call marginals** and the **deterministic counters** (`*_ops`, `*_elements`, `*_calls`). At m12 the `rust` **whole-process walls** are additionally dominated by the OPEN ~2.1 s boot, so `startup_ms`/`slice wall_ms` are *not* execution-throughput measures at this commit. Full before/after record: `results/m12-load-record.md` §3.

## 1. Environment (recorded with the run)

- **Commit measured:** runtime+driver tree `ee3e426db0`; artifacts committed as `bdadd02333`; milestone narrative `a63099d43f`. The m10-baseline binary used in §3.4 is `beam_rt` from `b4593818b3`.
- **Oracle (`sys`):** system `erl` — **OTP 29.1.1** (`erlang:system_info(version)` = `17.1`) at `/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/lib/erlang/erts-17.1/bin/erl`.
- **Rust (`rust`):** `erts_rust/target/release/beam_rt`, `erlang:system_info(otp_release)` = `30`, **default (baseline-JIT) tier**, self-check passing. Both runtimes report `supported: true` in every report.
- **Host:** NixOS 26.11 (Zokor), Linux 7.2.9 `x86_64`; Intel(R) Core(TM) Ultra 7 356H, `nproc` = **14**, `MemTotal` = 65 317 432 kB (~62.3 GiB).
- **Toolchain:** `rustc 1.98.1 (48a229cea 2026-09-01)`, `cargo 1.98.0`; `clang` **21.1.8**; `erlc` = the oracle's; `elixirc`/`mix` **1.20.4** ("compiled with Erlang/OTP 29").
- **Tools:** `dotnet` **9.0.318** on `PATH` (`/run/current-system/sw/bin/dotnet`; two 8.0.425 store SDKs are rejected by `command_for_min`'s min-`9` rule for `plb`); `rebar3` **not on `PATH`** — resolved from the Nix store by the driver's version-ranked scan (`rebar3-3.27.1`) ⇒ `khepri` runs; **`/usr/bin/time` absent** (the driver's own timers + `/proc/<pid>` `VmHWM` polling are used); network available (`cowboy`'s build fetches cowlib/ranch).
- **Env vars:** `$HOME` read-only; `$BENCH_TRY_NIX`/`$BENCH_TOOL_PATH` unset; `$ERTS_ORACLE`/`$BEAM_RT`/`$BEAM_RT_TIER` unset for every run except the interpreter control (`BEAM_RT_TIER=interpreter`) and the `foreign` A/B (`BEAM_RT`).
- `erts_rust/target/release/bench list --json`: **all 12 workloads `probe_ready: true`, 0 probe reasons.**

## 2. Exact commands

```sh
# build (once; the wrapper supplies a writable CARGO_HOME — $HOME is read-only)
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt -p bench

# probe status
erts_rust/target/release/bench list --json

# A. FULL cross-runtime run — the six Runtimes::Both workloads, --full, both runtimes (wall 1795 s)
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime all --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/results/m12-bench-xrun.json --verbose

# B. tier controls — same six workloads, rust only (wall 391 s interpreter / 488 s default)
BEAM_RT_TIER=interpreter erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime rust --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/results/m12-bench-xrun-interp.json
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime rust --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/results/m12-bench-xrun-quick.json

# C. FULL sys-only external targets (one report; wall 184 s) — khepri now MEASURES
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime sys --workloads erlc_compiler,elixir_compiler,cowboy,khepri \
    --out-file erts_rust/tools/bench/results/m12-bench-external.json --verbose

# D. PLB (quick, wall 95 s, ok)   E. EMQX (quick, skips immediately, wall 1 s)
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime sys --workloads plb  --out-file erts_rust/tools/bench/results/m12-bench-plb.json  --verbose
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime sys --workloads emqx --out-file erts_rust/tools/bench/results/m12-bench-emqx.json --verbose

# F. §3.10/§3.9 run-route latency probes — direct beam_rt CLI (HEAD vs the m10 binary vs HEAD-with-empty-boot;
#    send_recv / send_recv_plain / etf / ets / -boot / -version); NOT a report — see results/m12-run-route-latency.md
# G. §3.4 foreign A/B + growth ladder — direct beam_rt run on bench_foreign_driver:enc/1, interleaved 5 passes
#    at N=5000/10000/25000/60000 bracketed by two N=0 runs per pass, head vs $TMPDIR/m10wt (b4593818b3);
#    plus the b54/b55-shape driver A/B pair — see results/m12-foreign-regression.md

# H. compare artifacts (the tool diffs the mean)
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m12-bench-xrun.json
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m12-bench-xrun-interp.json \
    erts_rust/tools/bench/results/m12-bench-xrun.json            # as-specified tier diff (form mismatch)
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m12-bench-xrun-interp.json \
    erts_rust/tools/bench/results/m12-bench-xrun-quick.json      # MATCHED-form quick <-> quick
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m11-bench-xrun.json \
    erts_rust/tools/bench/results/m12-bench-xrun.json
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m11-bench-external.json \
    erts_rust/tools/bench/results/m12-bench-external.json
```

Each diff's stdout was written to the correspondingly named `results/m12-compare-*.txt`. Workloads were run **serially** — one benchmark process at a time (A → B → C → D → E → F → G). Archived copies carry only the two mechanical normalisations (`runtimes[].program` worktree prefix → `erts_rust/target/release/beam_rt`; a scratch `$TMPDIR` prefix in `notes` → `$TMPDIR/`). **No metric value was altered.**

## 3. Cross-runtime results (system ERTS vs Rust ERTS)

Source: `m12-bench-xrun.json` (`--full --runtime all`, six `Runtimes::Both` workloads, both runtimes, **12/12 rows `ok`**, wall 1 795 s, load 65.28 → 6.93). The marginals (`*_ns_*`) are the load-robust signal; the `rust` walls are boot-dominated (§3.9/§3.10).

### 3.1 Startup latency — `baseline` (n = 50, ms)

| statistic | sys (system ERTS) | rust (`beam_rt`, default tier) | rust/sys |
|---|---|---|---|
| mean | 120.6697 | 2 298.6279 | **19.05×** |
| p50 | 116.3115 | 2 281.1683 | — |
| p99 | 177.8934 | 2 741.1984 | — |

**This row is reversed from m11 (0.135×) and the single largest m12 move**: the `run`-route boot now drives the full `bootstrap/bin/start` script, adding **~2.1 s fixed** to every rust process (§3.10). It is **boot, not execution** — the per-op marginals below cancel it (§3.8) — and it is **OPEN** (`beam_rt`).

### 3.2 Fixed compute loop + VM memory — `baseline` (n = 5)

| metric | sys | rust | ratio |
|---|---|---|---|
| `compute_loop_ms` (mean) | 181.7306 | 2 272.2808 | 12.50× (boot-dominated) |
| `compute_ops_per_sec` | 5 502 651.77 | 440 086.45 | rust **0.080×** |
| `vm_memory_total_bytes` | 36 211 168 B | 15 892 346 B | rust **2.28× lower** |
| `peak_memory_bytes` (process RSS) | 134 971 392 B (128.7 MiB) | 408 072 192 B (389.2 MiB) | rust 3.02× higher |

The compute loop is dominated by the natively-served `lists:seq/2` + `lists:sum/1` **inside an ~2.1 s boot**; read it as the native floor plus boot, not as execution throughput (that is `list_walk`/`foreign`). `vm_memory_total_bytes` is the runtime's *own* view (`erlang:memory(total)`; `beam_rt` serves it through its `MemoryHook` seam) and stays **2.28× lower** than the oracle's.

### 3.3 `slice` — the same nine compiled programs under both runtimes (n = 25/case)

`case_<name>_wall_ms` mean (one process per case, so the whole wall includes the rust boot):

| case | sys | rust | rust/sys | sys RSS p50 (MiB) | rust RSS p50 (MiB) |
|---|---|---|---|---|---|
| `add` | 141.6530 | 2 270.3911 | 16.03× | 66.08 | 376.25 |
| `fib` | 134.3435 | 2 356.7838 | 17.54× | 66.32 | 376.27 |
| `pick` | 141.0300 | 2 366.6399 | 16.78× | 66.32 | 376.27 |
| `calendar_last_day` | 137.1116 | 2 290.9699 | 16.71× | 66.30 | 376.25 |
| `queue_from_size` | 132.3285 | 2 175.5374 | 16.44× | 66.65 | 376.33 |
| `gb_trees_size` | 132.0817 | 2 148.8423 | 16.27× | 66.38 | 376.26 |
| `base64_encode_size` | 136.3259 | 2 140.5912 | 15.70× | 66.40 | 377.30 |
| `ordsets_from_size` | 135.4466 | 2 101.3880 | 15.51× | 66.21 | 376.25 |
| `string_str_length` | 152.7097 | 2 097.6393 | 13.74× | 66.49 | 376.68 |

Whole-workload `peak_memory_bytes`: sys 72 368 128 B (69.0 MiB) → rust 395 730 944 B (377.4 MiB). **Every rust wall is ~2.1 s + ~0–0.3 s**: the spread across the nine cases (2 097–2 367 ms) is the *execution* cost and is small, while the constant is the boot (§3.10). The rust RSS is the boot's resident preloaded-module set. `slice`'s 9/9 `ok` on both sides confirms every case's stdout (recorded in `observed stdout:` notes).

### 3.4 `foreign` — real Cowboy / EMQX modules, per-call cost under both runtimes

Each case is startup-cancelled: `ns_per_call = (mean wall@N − mean wall@0)/N × 1e6` over `--full` (n = 10 samples/case; `N` per case per runtime, recorded as `case_*_calls`).

| case | sys ns/call | rust ns/call | **rust/sys** | calls sys / rust | sys RSS p50 | rust RSS p50 |
|---|---|---|---|---|---|---|
| `bstr_to_lower` | 419.7725 | 23 429.8831 | **55.82×** | 1 500 000 / 20 000 | 66.34 MiB | 438.46 MiB |
| `bstr_capitalize_token` | 361.5712 | 22 901.0619 | **63.34×** | 1 500 000 / 15 000 | 66.18 MiB | 441.27 MiB |
| `emqx_base62_encode` | 231.6157 | 15 502.6789 | **66.93×** | 3 000 000 / 25 000 | 66.34 MiB | 427.41 MiB |
| `emqx_base62_decode` | 383.9592 | 14 919.3416 | **38.86×** | 3 000 000 / 15 000 | 66.38 MiB | 427.98 MiB |

**The Θ(N³) regression is FIXED and the band is corrected to 38.9×–66.9×** (m11 published 60.2×–367.1×). Two independent instruments agree:

- **The driver `--quick` pair re-derived this milestone** (`m12-foreign-ab-{head,m10}.json`, b54/b55 shape, matched `N`): `bstr_to_lower` 18 525.83 vs 18 820.20 (**0.98×**), `bstr_capitalize_token` 22 245.98 vs 24 798.49 (**0.90×**), `emqx_base62_encode` 16 628.83 vs 18 286.00 (**0.91×**), `emqx_base62_decode` 15 341.64 vs 17 587.02 (**0.87×**) — i.e. **head ≈ 0.87×–0.98× of the m10 binary**, flat, no super-linearity.
- **The direct `bench_foreign_driver:enc/1` ladder** (interleaved 5 passes, head vs `$TMPDIR/m10wt`, each pass bracketed by two `N=0` startup runs so startup cancels; load `6.72 → 5.73`) gives startup-cancelled µs/call at `N` = 5 000 / 10 000 / 25 000 / 60 000: **m10** 13.64 / 12.46 / 17.31 / 28.44 (mean) vs **head** 16.20 / 14.72 / 17.15 / 28.23 ⇒ **head/m10 = 1.19 / 1.18 / 0.99 / 0.99** (median **0.99** at both 25 000 and 60 000); sub-cases at `N` = 25 000: `cap` 22.64 vs 24.36 µs (**0.93×**), `dec` 15.19 vs 17.00 µs (**0.87×**). At m11 the same head showed ≈729 µs/call at `N` = 25 000 — a **~30× collapse**.

The residual `rust` cost is now **flat in `N`** (the fix) and consists of the per-call Rust host-context cost (the `beam_rt` per-call dispatch, owned by `../../crates/beam_rt/PERF.md`), not a per-slice registration loop. **Caveat kept:** `ns_per_call` is a snapshot at the declared `N`, and the rust peak RSS (~427–441 MiB) is **boot-dominated**, not a like-for-like memory comparison (§6.7). Full narrative: `results/m12-foreign-regression.md`.

### 3.5 `list_walk` — the per-element dispatch-throughput metric (n = 10/case)

| metric | sys | rust | rust/sys |
|---|---|---|---|
| `case_list_walk_ns_per_element` | 7.6021 | 110.2061 | **14.50×** |
| `case_list_walk_elements` (deterministic) | 100 000 000 | 1 000 000 | per-runtime `N` |
| `case_list_walk_peak_rss_bytes` p50 | 65.74 MiB | 376.48 MiB | boot-inflated |

A BIF-free, non-tail-recursive `sum/1` over a 1000-element list (~7 dispatched instructions per element). The rust marginal **rose** vs m11 (45.97 → 110.21 ns/element, +139.72 %) although the loop's per-element path did not change at this commit: the startup-cancelled subtraction now removes an **~2.1 s, high-jitter** startup from a short wall, so a few ms of jitter dominate at `--full`'s sample count — an instrument-side inflation of the **OPEN** boot defect, not a dispatch regression (the interp tier control measured 235.05 ns/element under the same subtraction).

### 3.6 `copy_bound` — the copy boundary: ETF, ETS and process send/receive (**four** cases, n = 10/case)

| case | sys ns/op | rust ns/op | **rust/sys** | ops sys / rust | sys RSS p50 | rust RSS p50 |
|---|---|---|---|---|---|---|
| `etf_roundtrip` | 738.6382 | 35 369.2113 | **47.88×** | 1 500 000 / 5 000 | 66.32 MiB | 382.61 MiB |
| `ets_copy` | 289.5147 | 2 883.7548 | **9.96×** | 2 000 000 / 100 000 | 66.28 MiB | 450.01 MiB |
| `send_recv` | 1 584.3631 | 25 632 422.2283 | **16 178.4×** | 500 000 / 2 000 | 66.37 MiB | 384.32 MiB |
| `send_recv_plain` | 662.1843 | **— (failed: 60 s `RUN_DEADLINE`)** | — | 1 000 000 / — | 66.43 MiB | — |

Whole-workload `peak_memory_bytes`: sys 71 745 536 B (68.4 MiB) → rust 472 117 248 B (450.2 MiB). The row is `ok` with **3 ok, 0 unsupported (of 4)** on the rust side and 4/4 on the sys side; `send_recv_plain`'s rust case is recorded as a **failure** (verified 60 s deadline), so **no metric is fabricated**. Its wall in the probe ladder was 77 586 ms with exit 1 and empty stdout.

`send_recv`'s 25.6 ms/op is the **~25.5 ms per-blocked-round** boot effect, and `etf_roundtrip`'s +402.88 % vs m11 is the **Θ(atom-table) `binary_to_term`** boot effect (the atom table grows ~70 → ~4 100) — both **OPEN**, both `beam_rt` (§3.10). `ets_copy` (9.96×) is the only copy case that is *not* boot-limited; its rust RSS (450 MiB, the largest) is the `Nursery.retired` per-op retention (§6.7).

### 3.7 `gc_bound` — allocate-and-abandon churn (n = 10/case)

| metric | sys | rust | rust/sys |
|---|---|---|---|
| `case_churn_ns_per_op` | 4 658.7133 | 184 398.8576 | **39.58×** |
| `case_churn_ops` (deterministic) | 200 000 | 2 000 | per-runtime `N` |
| `case_churn_peak_rss_bytes` p50 | 66.54 MiB | 376.53 MiB | boot-inflated |

Each iteration builds and abandons three 256-element structures (a pure-Erlang list, a 256-byte binary, a tuple) and checks the two O(1)-checkable sizes — a silently dropped allocation fails loudly. The row measures **allocate-and-abandon wall time and claims no collection count**.

### 3.8 Reading the cross-runtime numbers

- **The marginals are the cross-runtime signal.** `foreign`/`list_walk`/`copy_bound`/`gc_bound` are startup-cancelled per-unit costs, so the ~2.1 s rust boot cancels on both sides *by construction*; that is why they remain the only clean rust-vs-sys comparison at this commit.
- **The `rust` whole-process walls are boot-dominated** (`baseline startup_ms`, `slice wall_ms`): read them as "boot + execution", not execution. At m11 the same rows read 0.135×/0.127×–0.311×; the change is entirely the OPEN boot defect (§3.10), not a slowdown of the workload programs.
- **`vm_memory_total_bytes` is the runtime's own accounting** and is not comparable with `peak_memory_bytes` (external `VmHWM`): the first is 2.28× *lower* for rust, the second 3.02× *higher* (boot's resident set).
- **Deterministic counters** (contention-immune): `copy_bound ops` = 1 500 000 / 2 000 000 / 500 000 / 1 000 000 (sys) and 5 000 / 100 000 / 2 000 (rust); `foreign calls` = 1 500 000 / 1 500 000 / 3 000 000 / 3 000 000 (sys) and 20 000 / 15 000 / 25 000 / 15 000 (rust); `list_walk elements` = 1e8 (sys) / 1e6 (rust); `churn ops` = 200 000 (sys) / 2 000 (rust). Per-runtime `N` differs by design (the rust loop count is chosen so the run stays affordable).

### 3.9 The **tier effect** — what the JIT default does and does not improve (Rust vs Rust)

Two `--quick` runs, same six workloads, `--runtime rust` only, read like-for-like
(`m12-compare-tier-effect-quick.txt`; `a` = interpreter, `b` = default). **No `sys` rows exist in either file** — this is a tier effect, never a system-ERTS comparison.

| workload | metric | interpreter | default (baseline JIT) | interp/default |
|---|---|---|---|---|
| `baseline` | `startup_ms` | 1 959.5795 | 2 086.4559 | 0.939× |
| `baseline` | `compute_loop_ms` | 1 991.3159 | 2 137.2065 | 0.932× (native floor; noise) |
| `baseline` | `vm_memory_total_bytes` | 15 895 354 | 15 892 346 | **1.000× (parity)** |
| `list_walk` | `ns_per_element` | 235.0513 | **absent** (note: `marginal below timer resolution`) | — |
| `gc_bound` | `churn` ns/op | 308 835.7985 | **148 964.3747** | **2.07× (default cheaper)** |
| `copy_bound` | `etf_roundtrip` ns/op | 44 170.6459 | 44 687.5942 | 0.988× (parity) |
| `copy_bound` | `ets_copy` ns/op | 2 996.3663 | 3 014.0485 | 0.994× (parity) |
| `copy_bound` | **`send_recv` ns/op** | 25 516 698.9983 | 25 537 116.8888 | **0.999× — parity** |
| `copy_bound` | `send_recv_plain` ns/op | **timed out (60 s)** | **timed out (60 s)** | — (no metric, either tier) |
| `foreign` | 4 cases ns/call | 21 458–37 786 | 15 449–21 014 | 1.03×–1.80× (default cheaper) |
| `slice` | 9 cases wall_ms | 2 021–2 104 | 2 101–2 165 | 0.95×–1.06× (boot-dominated) |

Three facts to carry forward:

1. **`list_walk`'s default-tier `--quick` marginal is absent** — the honest note `case list_walk: marginal below timer resolution; no ns_per_element metric` is emitted (the marginal is swamped by the ~2.1 s boot's jitter at only 3 quick samples). The interpreter measured **235.05 ns/element** in the same form, and the `--full` default run measured **110.21** (§3.5): the direction (default cheaper) is established from those, not from a matched pair.
2. **`send_recv_plain` timed out in both tiers** (the same 60 s deadline), so no tier pair exists for it (§3.10).
3. **The m11 interpreter pathology is GONE.** At m11 the interpreter tier was super-linear on the `foreign` cross-module cases (up to 4 178 832 ns/call; a `--full` pass ≈50 min, hence m11's `--quick` control). At m12 the interpreter `foreign` rows are 21 458–37 786 ns/call and the whole interpreter `--quick` control finished in **391 s** — consistent with the Θ(N³) registration defect having been **fixed**: the m11 interpreter pathology was the *same* shared `beam_rt`/`beam_process` defect, not an interpreter-tier defect. The as-specified `m12-compare-tier-effect.txt` (interpreter `--quick` vs default `--full`) shows the same directions (`send_recv` +0.45 %, `list_walk` −53.11 %, `gc_bound` −40.29 %, `foreign` −19.9 % to −51.9 %, `etf_roundtrip` −19.93 %) and labels the `sys` rows `only-in-b` by construction.

### 3.10 `beam_rt`'s boot status and the **OPEN** `run`-route latency defect at this commit

Factual probe (full narrative, evidence and reproduction: `results/m12-run-route-latency.md`, **OPEN**):

```
$ erts_rust/target/release/beam_rt -boot bootstrap/bin/start
exit_code=0        wall ≈2.07–2.09 s
Erlang/OTP 30 [erts-17.1] [source] [64-bit]
```

- **Boot COMPLETES** — exit **0**. (m11 recorded exit **3** on `opcode: run: process blocked (no message available)` at directive 18 of 22; **that m11 fact is obsolete**.)
- **Fixed `run`-route startup ≈2.07–2.13 s** on HEAD (measured 2 111 / 2 101 / 2 084 ms for `run … 0`) vs **17–19 ms** on the m10 binary vs **18 ms** on HEAD with a synthetic empty boot script; `-version` still costs 2 ms. The delta is the boot script drive (commit `85ee33b2f8`).
- **~25.5 ms per blocked round** at ~2 % CPU: `send_recv/1` N=0/200/400/2000 → 2 098 / 7 164 / 12 274 / 53 141 ms (user CPU 1.249 s at N=2 000), i.e. ≈25.5 ms per round; the m10 binary gave 19/50/80/248 ms. The booted kernel's `os:set_signal/2` self-pipe keeps `PortRegistry::has_nothing_to_poll()` false, so every blocked round sleeps the full `PORT_POLL_SLICE` = 25 ms (`crates/beam_rt/src/runtime/scheduler.rs`, `port.rs`). **This is what pushes `copy_bound send_recv` to 25.6 ms/op and `send_recv_plain` past its 60 s deadline** (§3.6).
- **Θ(atom-table) ETF decode:** boot grows the atom table ~70 → ~4 100, so `binary_to_term`'s `prefix_matches` validation becomes Θ(table) — `copy_bound etf/1` N=5 000 took 2 404 ms on HEAD (0.642 s CPU) vs 56 ms with the empty boot and 79 ms on m10. This is the `etf_roundtrip` +402.88 % move (§7.1).
- **Attribution:** root-caused to commit **`85ee33b2f8`** (boot drive) in `beam_rt`; already documented at `erts_rust/crates/beam_rt/src/runtime/CONTEXT.md:150` and `results/b55-foreign-regression-fix.md`. It is being fixed in parallel by another agent. **NOT attributable to any target project.**

## 4. System-ERTS target-project results (sys-only workloads)

Source: `m12-bench-external.json` (`--full`, `erlc_compiler`, `elixir_compiler`, `cowboy`, `khepri`; **4/4 `ok`**), `m12-bench-plb.json` (`ok`), `m12-bench-emqx.json` (`skipped`). Per-target notes: `results/m12-tool-targets-notes.md`.

### 4.1 `plb` — Programming-Language-Benchmarks (dotnet)

`ok`: `wall_ms=70 648.460241` (two stages: `--task build` 23 330 ms + `--task bench`), `test_time_ms` n=10 min `0.0` mean `1 245.27` p50 `595.13` p90 `3 408.24` p99 `3 565.99` max `3 583.52`, `test_mem_bytes=94 777 344`, `peak_memory_bytes=171 573 248`. `--quick`; `test_time_ms` is PLB's own per-test time across its 10 elixir kernels and `wall_ms` is dominated by building their escripts. Versus m11 wall 68 318.69 → 70 648.46 ms (**+3.41 %**, read manually — no tool diff archived for `plb`): flat within the host band. **Tool resolution re-verified in the row's notes:** `resolved dotnet 9.0.318 at /run/current-system/sw/bin/dotnet (PATH)` with `rejected dotnet 8.0.425 … (nix-store): requires >= 9` twice.

### 4.2 `khepri` — Khepri `rebar3 eunit`

`ok`: `wall_ms=77 564.335456`, `peak_memory_bytes=374 722 560` (m11 83 402.90 ms / 357 818 368 B ⇒ **−7.00 %**). **Caveat:** the store `rebar3` is an escript whose shebang points at an Erlang/OTP 28 `escript`, so the suite runs under **OTP 28** while this `SysOnly` row's `runtimes[]` entry describes the driver's oracle (`erl` on `PATH`, OTP 29). The wall time is real; the reported runtime version describes the driver's runtime, not necessarily the Erlang the tool chose.

### 4.3 `emqx` — EMQX umbrella build (`make`/`mix` through `env.sh`) — **`skipped(emqx-requires-otp-28)`**

Unchanged from m9/m10/m11 and re-verified this pass (immediate skip, 1 s wall, **no number**). The checkout pins OTP 28 (`/home/bill/Source/emqx/.tool-versions:1` → `erlang 28.5.0.7-2`; `env.sh:3` → `export OTP_VSN=28.5.0.7-2`) while the host `erl` is OTP 29.1.1 / erts-17.1. The umbrella build aborts in `:ezstd`'s NIF makefile, which generates `env.mk` with `erl -noshell -s init stop -eval '…'`; on OTP 29 `init stop` halts **before** the `-eval` (verified: exit 0, no output), so `mix compile` fails with `No rule to make target '…/env.mk'`. The OTP-28 store `erl` cannot be paired with the OTP-29-built Elixir 1.20.4/`mix` the build needs, and the pin lives in the read-only target — not satisfiable without a full toolchain swap (out of scope).

### 4.4 `cowboy` — hello_world HTTP (in-process HTTP/1.1 load generator, 16 connections × 5 s — `--full`)

`ok`: `build_ms=42 527.108981` (relx release; network fetch of cowlib/ranch), `requests=827 888`, `errors=0`, `requests_per_sec=165 489.213469`, `latency_ms` n=20 000 min `0.023990` mean `0.105668` p50 `0.077852` p90 `0.137818` p99 `0.742380` max `3.268676`, `peak_memory_bytes=166 875 136`. Versus m11: throughput 190 626.72 → 165 489.21 req/s (**−13.19 %**), mean latency **+24.34 %**, p99 0.223380 → 0.742380 ms — the shared host's load (pass bracketed by loadavg 10.91 → 9.34), not a server change.

### 4.5 `erlc_compiler` — `erlc` over the stdlib + Elixir corpus slice

`ok`: `wall_ms=6 842.177446`, `files_compiled=60`, `files_per_sec=8.769138`, `loc_per_sec=6 796.520606`, `peak_memory_bytes=134 492 160`, note `corpus: 60 files (46503 LOC) attempted from stdlib+elixir; 60 produced .beam (46503 LOC)`. The rates are computed from the **produced** `.beam`/LOC counts, so a partial run cannot overstate them. Versus m11: wall 7 905.65 → 6 842.18 ms (**−13.45 %**), files/s 7.589 → 8.769 (**+15.54 %**) — a host-load move with the same unmodified system `erlc`, not a compiler change.

### 4.6 `elixir_compiler` — installed system `elixirc` over the checkout's `.ex` corpus

`ok`: `wall_ms=50 304.338719`, `files_attempted=105`, `files_compiled=223`, `files_per_sec=4.433017`, `loc_per_sec=2 225.732469`, `compiler_startup_ms=297.643093`, `peak_memory_bytes=211 509 248`. Notes: `batch compile failed; per-file fallback (version skew)`; `compiler: Elixir 1.20.4 (compiled with Erlang/OTP 29)`; `97/105 compiled; 223 .beam produced`; `223 .beam file(s) produced from 105 source file(s) (an Elixir source may define several modules)`. **`files_compiled` (223) exceeding `files_attempted` (105) is expected and documented by the workload itself**; the throughput rate uses the produced count. Versus m11: wall 47 667.62 → 50 304.34 ms (+5.53 %), files/s −5.24 %, compiler startup **−8.23 %**.

## 5. Workloads that did **not** run in the m12 measurement run — verified reasons

| workload | status | exact reason |
|---|---|---|
| `emqx` | `skipped(emqx-requires-otp-28)` | the checkout pins OTP 28.5.0.7-2 while the host `erl` is OTP 29.1.1 (§4.3) — the only non-`ok` row in the whole milestone |
| `copy_bound` / `send_recv_plain` (**rust case only**) | recorded as a **failure** inside an `ok` row | hit the driver's 60 s `RUN_DEADLINE` — the OPEN ~25.5 ms blocked-round defect (§3.10); the **sys** case ran normally (662.18 ns/op), and **no rust number is fabricated** (row summary: `3 ok, 0 unsupported (of 4)`) |

Nothing else skipped: `bench list --json` reports all 12 workloads `probe_ready: true`, and the six `Runtimes::Both` workloads produced 12/12 `ok` rows.

## 6. Reproducibility, stability and known measurement caveats

### 6.1 The host is shared and loaded, and the two runtimes are measured in different windows
The runner is runtime-major (all `sys` rows, then all `rust` rows), so a drifting load biases a cross-runtime comparison. The m12 cross-runtime pass **fell** 65.28 → 6.93, i.e. the `rust` side ran under a lighter load. Read ratios, and prefer the load-robust marginals. Re-measure on a quiet host before quoting absolutes; `results/m12-load-record.md` §3 has the before/after record.

### 6.2 `beam_rt` resolves its code path natively from the filesystem
`crates/beam_rt/src/codeserver.rs` probes (`$ERTS_STDLIB_DIR` → the OTP layout under the `erl` on `$PATH`, *resolved not executed* → this executable's ancestors → `$ERL_TOP` → standard prefixes); the system `erl` is only *run* as a last resort. `bench` never sets `$ERTS_STDLIB_DIR` — it is a pure driver/observer.

### 6.3 `elixir_compiler` measures the installed `elixirc` 1.20.4
The in-repo checkout is unbuilt and read-only, so a version skew with the checkout is expected and the per-file fallback runs (§4.6).

### 6.4 `cowboy` re-fetches cowlib/ranch in a fresh scratch copy on every run (network)
The build is therefore host/network-dependent (42.5 s at m12 vs 45.2 s at m11 vs 39.5 s at m10) and is excluded from the latency figures (which come from the load phase only).

### 6.5 Peak memory is Linux `VmHWM` polling of the child (`peak_memory_bytes`)
Unreadable ⇒ `null` + a note; never fabricated. It is never conflated with `vm_memory_total_bytes` (`erlang:memory(total)`, §3.2/§3.8).

### 6.6 `foreign` / `list_walk` / `copy_bound` / `gc_bound` `wall_ms` are not comparable across runtimes
They loop `N` ops **per process** at a per-runtime `N`; only the startup-cancelled per-unit marginals and the deterministic counters are cross-runtime comparable. At m12 both sides also carry their own startup offset — which the marginal cancels **by construction**, which is precisely why the marginal is the right instrument under the OPEN boot defect (§3.10).

### 6.7 The rust peak RSS of the loop workloads is **boot-dominated** at this commit (and was `N`-dependent before)
The `foreign`/`list_walk`/`copy_bound`/`gc_bound` rust peak RSS rows (~376–450 MiB) are now dominated by the **~2.1 s boot's resident preloaded-module set** rather than the loop count: `list_walk` (N = 1 000) and `gc_bound` (N = 2 000) and `ets_copy` (N = 100 000) all sit at ≈376 / 376 / 450 MiB, i.e. the growth with `N` documented at m11 is now masked by a ≈370 MiB constant. The underlying per-op retention mechanism (BIF-result copy-out → `Process::grow_nursery` → `Nursery::grow` → `Nursery.retired`, cleared only by a real collection) is unchanged and still owned by `beam_rt`/`beam_process`/`beam_gc` — the **memory row is not a like-for-like comparison**, and `slice` (one call per process) is equally boot-dominated at m12. `vm_memory_total_bytes` (§3.2) remains the clean rust-vs-sys memory comparison (**2.28× lower**).

### 6.8 Contract A.7 — the `beam_rt` boot status at this commit **COMPLETES**, with an OPEN latency defect
`beam_rt -boot bootstrap/bin/start` now **exits 0** (m11: exit 3 at directive 18 of 22). What remains is **fidelity and latency**, not completion: **no self-hosted build**, no live `code_server`/`erl_prim_loader` process in a plain runtime, and the OPEN `run`-route boot/blocked-round/ETF-decode defect of §3.10 (owner `beam_rt`, being fixed in parallel). No boot-completion claim beyond the above is made anywhere.

## 7. Deltas vs the previous run (`m11`; `m10`/`m9`/`m6`/`m5`/`m4` earlier)

Source: `m12-compare-vs-m11-xrun.txt`, `m12-compare-vs-m11-external.txt` (the tool diffs the **mean**; `a` = m11, `b` = m12).

### 7.1 m12 vs m11 — the cross-runtime marginals (rust side)

| (workload, metric) | m11 | m12 | Δ |
|---|---|---|---|
| `foreign` `emqx_base62_encode` ns/call **rust** | 77 167.3229 | **15 502.6789** | **−79.91 %** (the Θ(N³) fix) |
| `foreign` `emqx_base62_decode` ns/call **rust** | 19 306.3883 | **14 919.3416** | −22.72 % |
| `foreign` `bstr_capitalize_token` ns/call **rust** | 28 757.0043 | **22 901.0619** | −20.36 % |
| `foreign` `bstr_to_lower` ns/call **rust** | 23 751.0676 | **23 429.8831** | −1.35 % |
| `baseline` `startup_ms` **rust** | 15.2122 | **2 298.6279** | **+15 010 %** (the fixed boot; §3.10) |
| `baseline` `compute_loop_ms` **rust** | 58.6524 | **2 272.2808** | +3 774 % (boot) |
| `baseline` `vm_memory_total_bytes` **rust** | 805 458 | **15 892 346** | +1 873 % (boot's resident set) |
| `copy_bound` `send_recv` ns/op **rust** | 21 522.2389 | **25 632 422.2283** | **+118 997 %** (blocked-round defect; §3.10) |
| `copy_bound` `etf_roundtrip` ns/op **rust** | 7 033.3943 | **35 369.2113** | +402.88 % (Θ(atom-table) decode; §3.10) |
| `copy_bound` `ets_copy` ns/op **rust** | 2 435.0664 | **2 883.7548** | +18.43 % |
| `copy_bound` `send_recv_plain` (rust rows) | present | **failed (60 s deadline)** | rust metrics dropped |
| `gc_bound` `churn` ns/op **rust** | 128 524.4542 | **184 398.8576** | +43.47 % |
| `list_walk` ns/element **rust** | 45.9731 | **110.2061** | +139.72 % (boot-subtraction jitter; §3.5) |
| `slice` rust walls | 15.35–37.85 ms | **2 097.64–2 366.64 ms** | the fixed boot (one process per case) |

The `sys` side moved only with the host (`startup_ms` +6.96 %; `foreign` ns/call +10.2 % to +21.7 %; `copy_bound` ns/op −1.5 % to +22.2 %; `gc_bound` churn +9.10 %; `list_walk` +214 % because the sys startup row 118.70 → 157.33 ms is subtracted from a ~360–918 ms wall) — that flatness ± load is the cross-check that the rust moves are not oracle drift.

### 7.2 Real, code-attributable changes since m11 (owners **outside this node**)

1. **`foreign` Θ(N³) fix** (commits `07a5720639` + `7d84b12b44`) — the rust `ns_per_call` rows fell (encode −79.91 %) and the cross-runtime band narrowed to **38.9×–66.9×** from 60.2×–367.1×; independently confirmed by the A/B at **0.87×–0.99×** of the m10 binary (§3.4).
2. **The `run`-route boot** (commit `85ee33b2f8`) — boot now **completes** (exit 0) and `beam_rt -boot …` prints `Erlang/OTP 30 [erts-17.1]`, but every `run`/`-eval`/`-boot` action pays **~2.1 s fixed** startup, **~25.5 ms per blocked round**, and a **Θ(atom-table) `binary_to_term`**; that is the whole of the `startup_ms`/`slice`/`send_recv`/`etf_roundtrip` m11→m12 moves — **OPEN** (§3.10).

### 7.3 Load-attributable / not comparable
External targets: `erlc_compiler` wall −13.45 % (files/s +15.54 %); `elixir_compiler` wall +5.53 % (files/s −5.24 %, startup −8.23 %); `cowboy` req/s −13.19 %, mean latency +24.34 %; `khepri` wall −7.00 %; `plb` wall +3.41 %. All on the shared host, none a like-for-like quiet-host comparison. The `strip` of `list_walk`'s rust marginal and part of `gc_bound`'s +43.47 % are startup-subtraction jitter amplified by the ~2.1 s boot, not execution changes.

### 7.4 Unchanged / still-open
- `ets_copy`'s remainder and `gc_bound`'s churn marginal remain real rust execution costs (term-copy / allocate-and-abandon), independent of the boot.
- The per-op retention mechanism behind the loop workloads' rust RSS (§6.7) is unchanged (owner outside this node).
- No Rust-runtime row exists for the six `SysOnly` targets (§4, §5); `emqx` is the one exact mechanical skip.

## 8. Fresh ranked bottleneck list (from the m12 data)

Ranked by `(rust/sys)` magnitude × how load-robust the measurement is, with the m12 evidence and the owner.

1. **`copy_bound send_recv` = 16 178.4× (25.6 ms/op) and `send_recv_plain` past its 60 s deadline — the OPEN ~25.5 ms blocked-round defect.** *Evidence:* `send_recv` N=0/200/400/2000 → 2 098 / 7 164 / 12 274 / 53 141 ms (≈25.5 ms/round, ~2 % CPU); the same shape in both tiers. *Cause:* the booted kernel's `os:set_signal/2` self-pipe keeps `PortRegistry::has_nothing_to_poll()` false, so each blocked round sleeps `PORT_POLL_SLICE` = 25 ms. *Owner:* `beam_rt` (`crates/beam_rt/src/runtime/scheduler.rs`, `port.rs`), commit `85ee33b2f8`; **being fixed in parallel**. *This is the single largest number in the record and it is a scheduler defect, not a messaging-cost measurement.*
2. **Every rust whole-process wall (`baseline startup_ms` 19.05×, `slice` 13.7×–17.5×) — the OPEN ~2.1 s `run`-route boot.** *Evidence:* 2 084–2 111 ms for `run … 0` on HEAD vs 17–19 ms on m10 vs 18 ms with an empty boot script; boot script drive at commit `85ee33b2f8` (m11 measured 0.135× on the same row). *Owner:* `beam_rt`. *Fix shape:* make the boot drive's cost proportional to what it actually needs (or lazy/optional on the bare `run` action).
3. **`foreign` — the residual per-call cost is now flat in `N`; the rust/sys gap is 38.9×–66.9×.** The Θ(N³) growth is **CLOSED** (A/B 0.87×–0.99× of m10; `enc` ladder 473→28.44 µs at N=25 000 vs 729 µs at m11). What remains is the flat Rust per-call host-context cost, owned by `../../crates/beam_rt/PERF.md` (the registered-BIF dispatch path), **not** by this node.
4. **`copy_bound etf_roundtrip` = 47.88× — now the Θ(atom-table) `binary_to_term` boot effect.** *Evidence:* `etf/1` N=5 000 → 2 404 ms HEAD (0.642 s CPU) vs 56 ms empty-boot vs 79 ms m10; the atom table grows ~70 → ~4 100 during boot, so `prefix_matches` validation is Θ(table). *Owner:* `beam_rt` (codec/atom table). *Fix shape:* an atom-table-aware lookup index for decode validation. **Highest-value non-scheduler fix** (it is 4× the m11 `etf` figure).
5. **`gc_bound churn` = 39.58× (184.40 µs/op) — the allocate-and-abandon execution cost, plus the unchanged retired-heap retention behind the loop workloads' rust RSS.** The **default JIT is 2.07× cheaper than the interpreter** on the same loop (a Rust-vs-Rust tier effect), so the tier is doing its job; the residual is execution + retention. *Owner:* `beam_rt`/`beam_gc` (`Nursery.retired` collapsed only by a real collection; §6.7).
6. **`list_walk` = 14.50× (110.21 ns/element) — partly an instrument artefact.** *Evidence:* the marginal **rose** 139.72 % while the loop code did not change; the startup-cancelled subtraction now removes a high-jitter ~2.1 s boot. *Owner:* `beam_rt` (boot); the **dispatch signal itself is unchanged in shape** (§3.5). **Re-measure after the boot fix** before treating 110.21 ns as the per-element cost.
7. **`baseline compute_loop_ms` 12.50× / `compute_ops_per_sec` 0.080×** — dominated by the boot wrapping the natively-served `lists:seq/2`+`lists:sum/1`; falls with item 2. **`vm_memory_total_bytes` stays 2.28× LOWER than the oracle** — the one cross-runtime memory comparison that is clean at m12.
8. **`ets_copy` = 9.96× — the narrowest copy ratio and the only one that is not boot-limited** (289.51 → 2 883.75 ns/op). De-prioritised: it is the stable term-copy-band member (cf. m11's 10.28×).
9. **Contract A.9 / self-hosted build** — `beam_rt` still does not compile or run itself, and no compiler workload is executed by `beam_rt` (§4.5/§4.6 are system-toolchain reference rows). The `run`-route boot fix (item 2) is a prerequisite for making an in-VM compile loop affordable.
10. **External sys-only targets remain reference-only** — no Rust-runtime row exists for them (§4, §5); `emqx` is the one exact mechanical skip (`emqx-requires-otp-28`).

## Honesty audit — measurement-integrity notes

No workload returns a hard-coded or estimated metric; a workload that fails to build/run is always `failed`/`skipped`, never a number (verified across `src/workloads/*.rs`, `src/compare.rs`, `src/report.rs`, `src/runtime.rs`). No `todo!`/`unimplemented!`/non-doc `unreachable!`, no `#[ignore]`.

- **The `send_recv_plain` rust case is reported as a failure, not a number.** The `copy_bound` row stays `ok` (3 ok / 0 unsupported of 4) and carries the deadline failure; the sys side of the same case ran and produced 662.18 ns/op. The failure is attributed to `beam_rt`'s blocked-round defect (§3.10) with the probe evidence, so it is a deadline/defect record — not a claim about the target program.
- **`foreign` classifies a staging failure honestly** (`src/workloads/foreign.rs`: `StageError` + `stage_outcome`): genuinely absent foreign `.erl` sources ⇒ `skipped(sources-missing)`; a failure to compile sources we *did* stage (the embedded driver or a present foreign source) ⇒ `failed(module-compile-failed)` — matching `list_walk`/`copy_bound`/`gc_bound`. `slice` treats a failed fixture rebuild as `skipped(fixture-missing)`.
- **`erlc_compiler`'s `files_per_sec`/`loc_per_sec` are computed from the *produced* counts**, so a partial `erlc` run cannot overstate throughput next to `files_compiled`. `elixir_compiler` likewise uses the produced count (§4.6).
- **A version-constrained tool is resolved by version, and the choice is auditable.** `plb` calls `tool::command_for_min(TOOL, TOOL_NIX_ATTR, MIN_DOTNET_SDK)` (min `9`), which probes every candidate's `--version` and selects the greatest satisfying one; the resolved binary + version + source and every rejected candidate's reason are in the row's `notes` (§4.1). `khepri`'s `runtimes[]` version describes the driver's oracle, not necessarily the Erlang the store `rebar3` escript chose (§4.2).
- `proc.rs`'s drain-thread join (`h.join().unwrap_or_default()`) is infallible **by construction** — the drain threads run only `read_to_end` + a lossy UTF-8 conversion, neither of which can panic — so the `unwrap_or_default` masks nothing real; the invariant is documented at the call site.
- `baseline.rs`'s docs correctly state that both runtimes serve `erlang:memory/0,1` (`beam_rt` via its `MemoryHook` seam), recording a real `vm_memory_total_bytes` for `rust`; `is_capability_gap` survives only as a fallback classifier.
- The `foreign`/`list_walk`/`copy_bound`/`gc_bound` startup-cancelled methodology is implemented exactly as this document states (startup-cancelled `(mean wall@N − mean wall@0) / N`), and each of those workloads emits its deterministic companion (`case_*_ops` / `*_calls` / `*_elements`) so the loop count behind every marginal is machine-visible. The runtime-side hot-path figures (registered-BIF dispatch ≈79–96 ns/call; `pb:tr/2`'s `trunc/1` ≈76 ns/iter) are `beam_rt` measurements (`../../crates/beam_rt/PERF.md`, `../../crates/beam_rt/TRACE_AND_PERF.md`), **not** produced here.
- **Boot status is recorded as fact, not as a metric:** `beam_rt -boot bootstrap/bin/start` exits **0** (wall ≈2.07–2.09 s) and `run … 0` costs ≈2.1 s (§3.10). The OPEN defect is labelled OPEN and attributed to `beam_rt`; no fix is claimed.
- **The two m12 tier controls are both `--quick`** (matched form), and the as-specified `m12-compare-tier-effect.txt` (interpreter `--quick` vs default `--full`) is labelled a form mismatch and accompanied by `m12-compare-tier-effect-quick.txt`. Every tier table is labelled **Rust-vs-Rust**; every cross-runtime ratio names `sys` (system ERTS 29.1.1 / erts-17.1) and `rust` (`beam_rt` default tier) explicitly.
- **`m12` pass integrity:** the driver and `beam_rt` were built from the tree `ee3e426db0`; every run's stdout was checked against the deterministic constant on both sides; the tier control is separate artifacts so the Rust tiers cannot be conflated with the `sys` column; the `foreign` A/B used an **interleaved** head/m10 schedule (each pass bracketed by its own `N=0` startups) so the comparison is drift-resistant; all committed artifact paths are normalised (`runtimes[].program` → `erts_rust/target/release/beam_rt`; a scratch `$TMPDIR` prefix in `notes` → `$TMPDIR/`). **No metric value was altered.**

**Reproducibility note (PLB / tool resolution).** `plb`'s outcome is *tool- and environment* state, not repository state. The resolver picks the **greatest `dotnet` ≥ 9.0** across `PATH` + `$BENCH_TOOL_PATH` + the Nix store, so `plb` runs wherever any 9.x+ SDK is reachable — even with an older `dotnet` first on `PATH`. `khepri` needs only the `rebar3` store entry and runs as-is. `emqx` remains a genuine toolchain skip on any host without an OTP-28 Erlang that can pair with the Elixir/`mix` the umbrella build uses.

## Archived artifacts

| file | contents |
|---|---|
| `results/m12-bench-xrun.json` | **current cross-runtime run**: `--full --runtime all`, the six `Runtimes::Both` workloads × both runtimes, **12/12 rows `ok`** (load 65.28 → 6.93, wall 1 795 s) — the source of §3 |
| `results/m12-bench-xrun-interp.json` | **the tier control (interpreter)**: the same six workloads, `rust` only, `BEAM_RT_TIER=interpreter`, 6/6 rows `ok`, **profile `quick`** (wall 391 s). **Rust-vs-Rust only; no `sys` rows.** |
| `results/m12-bench-xrun-quick.json` | **the matched-form default-tier control**: the same six workloads, `rust` only, default (baseline-JIT) tier, `--quick` (wall 488 s) — so the tier table is read off two `--quick` runs. **Rust-vs-Rust only.** |
| `results/m12-compare-cross-runtime.txt` | `bench compare` — sys vs rust inside `m12-bench-xrun.json` (the tool diffs the **mean**) |
| `results/m12-compare-tier-effect.txt` | `bench compare m12-bench-xrun-interp.json m12-bench-xrun.json` — the as-specified interpreter(`--quick`) → default(`--full`) pair; **form mismatch** |
| `results/m12-compare-tier-effect-quick.txt` | `bench compare m12-bench-xrun-interp.json m12-bench-xrun-quick.json` — the **matched-form** `--quick` ↔ `--quick` tier control (§3.9) |
| `results/m12-compare-vs-m11-xrun.txt` / `results/m12-compare-vs-m11-external.txt` | the tool's two-file diffs against the m11 record — the source of §7 |
| `results/m12-bench-external.json` | the external sys-only targets in one `--full` report: `erlc_compiler`, `elixir_compiler`, `cowboy`, `khepri` (**4/4 `ok`**; §4.2, §4.4–§4.6) |
| `results/m12-bench-plb.json` | `plb` → `ok`, `wall_ms` 70 648.46, `dotnet` 9.0.318 resolved from `PATH` (§4.1) |
| `results/m12-bench-emqx.json` | EMQX → `skipped(emqx-requires-otp-28)` (§4.3) |
| `results/m12-foreign-ab-head.json` | the **re-derived** `foreign` A/B (HEAD side): `bench run --quick --runtime rust --workloads foreign` with `$BEAM_RT` = the HEAD `beam_rt` — matched `N` against the m10 artifact below (§3.4) |
| `results/m12-foreign-ab-m10.json` | the A/B m10 side: the same command with `$BEAM_RT` = the m10 (`b4593818b3`) `beam_rt` (program normalised to `$TMPDIR/m10wt/erts_rust/target/release/beam_rt`) |
| `results/m12-foreign-regression.md` | the `foreign` **Θ(N³) fix** record: the interleaved 5-pass `enc/1` ladder (head vs m10, µs/call at N = 5 000…60 000, both tiers), the sub-case ladders, the driver A/B pair, and the corrected cross-runtime band |
| `results/m12-run-route-latency.md` | the **OPEN** `run`-route record: the ≈2.1 s fixed startup, the ~25.5 ms blocked round, the Θ(atom-table) decode, the `-boot` exit-0 fact, and the root cause (`85ee33b2f8`) |
| `results/m12-load-record.md` | the m12 pass: commit measured, exact commands, the `/proc/loadavg` before/after each run, per-workload status, per-case detail, the tier table and the boot probe |
| `results/m12-tool-targets-notes.md` | the factual per-target status/blocker notes and the environment findings |
| `results/m11-*` | the **previous** run (2026-10-14 05:47–06:57 UTC): the cross-runtime report, the interpreter + matched-quick controls, the per-target reports, the cross-runtime/tier/vs-m10 diffs, and the `m11-load-record.md`/`m11-tool-targets-notes.md` records — **regression baseline for §7** |
| `results/b55-foreign-regression-fix.md` + `results/b55-foreign-ab-{head,m10}.json` | the fixing agent's committed evidence for the Θ(N³) fix (superseded by the m12 re-derivation above; kept as history) |
| `results/b54-foreign-ab-{m10,head}.json` + `results/b54-foreign-growth-curve.md` | the batch-54 controlled experiments that root-caused the Θ(N³) regression (kept as history) |
| `results/m10-*` / `results/m9-*` / `results/m6-*` / `results/m5-*` / `results/m4-*` | earlier runs (2026-10-13 / 2026-10-13 / 2026-10-11 / 2026-10-09 / 2026-10-08), kept as earlier regression baselines |

The archived copies are the harness output except for two mechanical normalisations that strip environment-specific absolute paths: the run-host worktree prefix in `runtimes[].program` is replaced with the workspace-relative `erts_rust/target/release/beam_rt`, and a scratch `$TMPDIR` prefix in `notes` is replaced with `$TMPDIR/`. **No metric value is altered.**
