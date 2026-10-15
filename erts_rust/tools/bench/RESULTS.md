# Rust-ERTS benchmark results — current measurement run (m13)

Reproducible numbers for the Rust-ERTS rewrite (`erts_rust/`) measured against the target projects, produced by the `bench` driver (`erts_rust/tools/bench`).
Everything below is **measured, never modelled**.
A workload that could not run is reported with a *verified* reason and **no number** — never an estimate.

Measured on the host in §1 on **2026-10-15 20:20–21:16 UTC** (the matched A/B 20:20–20:42, the cross-runtime pass 20:44–20:58, the two tier controls 20:58–21:03, the external targets 21:03–21:09, the CLI probe 21:15) from the tree **`a141d6878c`**, with the driver and `beam_rt` both built from that tree (`--release -p beam_rt -p bench`); the m13 measurement artifacts were committed as **`7801fabaea`**.
Raw harness JSON is committed under `results/` as `m13-*`; the previous runs are kept as `m12-*` (2026-10-15 14:00–15:03 UTC), `m11-*` (2026-10-14), `m10-*` (2026-10-13), `m9-*` (2026-10-13), `m6-*` (2026-10-11), `m5-*` (2026-10-09) and `m4-*` (2026-10-08) and are the regression baselines for the delta section (§7).

**What is new at m13 (the deliverable of this milestone).** (1) **Whole-process startup fell from ~2.1 s at m12 to ≈0.72 s** — the fixed `clock_nanosleep`/`epoll_wait` waits were removed (`TimerWait::ImminentWithin` plus a non-destructive zero-timeout signal probe replacing the 25 ms `PORT_POLL_SLICE` sleep; owner `beam_rt`, **outside this node**), and the matched A/B re-derives it at **3.16×–3.46× faster than the m12 binary** (§3.8). (2) **`copy_bound: send_recv` fell from ~25.4 ms/op at m12 to tens of µs/op** (217.03 µs/op in the `xrun`; 28.14–68.93 µs/op in the A/B) and **`send_recv_plain` no longer hits the 60 s deadline** (173.71 µs/op; it was a recorded *failure* at m12) — the blocked-round defect is **CLOSED** (§3.5, §3.8, §8). (3) **The `foreign` Θ(N³) per-slice off-heap registration regression stays FIXED** (re-derived from a fresh interleaved A/B, not copied), but **a new rust regression appeared in the `foreign: emqx_base62:encode` sub-case** (head 3.2×–3.8× dearer than the m10 binary; §3.3, §8 item 1, **OPEN**). (4) **The default CLI route now boots this runtime's *own* release's `bootstrap/bin/start.boot`** instead of the installed OTP-29 script (§3.10). (5) The driver test suite is unchanged (**257 tests, green**).

> **Naming both sides of every ratio (read this before quoting a number).** `sys` = the **system ERTS** (OTP **29.1.1** / erts-17.1, the oracle `erl`, a 14-scheduler SMP VM). `rust` = **`beam_rt`**, the Rust runtime, at its **default execution tier** (the native baseline JIT). A third, distinct column exists — **`rust` with `BEAM_RT_TIER=interpreter`** — and it is a **Rust-vs-Rust tier effect, NOT a system-ERTS comparison** (§3.9, §7). **Every ratio in this document names both sides.**
> **The target applications are NOT run by the Rust runtime.** `cowboy`, `khepri`, `plb`, `erlc_compiler`, `elixir_compiler` and `emqx` are executed by their **own system-ERTS / system-toolchain** (`erlc`, `elixirc`, the Cowboy release on the oracle `erl`, `rebar3`'s Erlang, the `.NET` tool). They are **reference** figures for the sys-only surfaces (§4), never Rust-runtime results.

> **This is a shared, usually-loaded host** (§1, §6.1): absolute latencies/throughputs move several-fold with load, so **ratios are the signal** and a single contended sample is never a constant. **The whole m13 primary pass ran on a continuously loaded host** — the 1-min `/proc/loadavg` was **72.46–135.80** (median **113.13**, 55 samples 15 s apart, `results/m13-xrun-load.log`) — so the `sys` columns are materially dearer than at m12's quiet re-take and must not be compared to it directly. A quieter window (load **20.11–23.63**) was captured **only** for the CLI probe; the projected quieter re-take of the full pass was **attempted and abandoned** because the window closed (load fell to ~12 then rose to ~84) and produced no artifact. Read the **deterministic `case_*_ops/calls/elements` counters**, the **startup-cancelled marginals** and the **interleaved A/B** (§3.8) over absolute latency.

## Verdict

**(a) / (b) Which targets run, which skip.** Five of the six external target projects run; one (`emqx`) skips with a verified toolchain reason (never a fabricated number). All five running rows are executed by their **own system toolchain**, not by the Rust runtime.

| target project | runs? | executed by | result / verified reason |
|---|---|---|---|
| `khepri` (tree DB) | **runs** | `rebar3`'s Erlang (OTP 28 escript; §4.2) | `ok` — `rebar3 eunit` wall 89 131.58 ms |
| `cowboy` (HTTP server) | **runs** | the oracle `erl` release | `ok` — 61 363.95 req/s, p50 0.1460 ms / p99 2.1605 ms |
| `erlc_compiler` (Erlang compiler) | **runs** | system `erlc` (OTP 29) | `ok` — 6.199 files/s over the stdlib+Elixir corpus |
| `elixir_compiler` (Elixir compiler) | **runs** | installed `elixirc` 1.20.4 | `ok` — 3.014 files/s over 105 sources / 223 `.beam` |
| `plb` (Benchmarks Game kernels, .NET) | **runs** | the `.NET` tool (`dotnet` 9.0.318) | `ok` — wall 122 157.47 ms; `test_time_ms` p50 700.51 / p99 4 513.28 (n=10) |
| `emqx` (MQTT broker) | **skips** | — | `skipped(emqx-requires-otp-28)` — EMQX pins OTP **28.5.0.7-2** (`env.sh:3`/`.tool-versions:1`), the host `erl` is OTP 29.1.1 / erts-17.1; not satisfiable without a full OTP-28 toolchain swap (§4.3, §5) |

The six workloads the Rust runtime itself can host (`Runtimes::Both`) — `baseline`, `slice`, `foreign`, `list_walk`, `copy_bound`, `gc_bound` — all ran: **12/12 rows `ok`** (0 skipped, 0 unsupported, 0 failed). Every other workload is `SysOnly` and would report `skipped(runtime-unsupported)` under `rust` (§5).

**(c) Ratio to the system-ERTS oracle, per cross-runtime workload.** `rust/sys` is the **Rust default tier** cost as a multiple of the **system-ERTS** cost (`<1` ⇒ Rust cheaper, `>1` ⇒ Rust dearer). Values are the m13 `xrun` (§3, `--full`, load 72.46 → 135.80 — a continuously loaded host, so both columns are load-inflated and only the ratio and the counters are read).

| workload | surface | headline metric | sys (system ERTS 29.1.1) | rust (`beam_rt`, default tier) | rust/sys | reading |
|---|---|---|---|---|---|---|
| `baseline` | startup / CLI | `startup_ms` (mean, whole process) | 315.3467 ms | 1 061.4786 ms | **3.37×** | Rust dearer — the ~0.72 s boot/init (was ~2.1 s at m12, 18.44×) |
| `baseline` | VM memory | `vm_memory_total_bytes` | 36 197 840 B | 15 889 338 B | **0.439×** | Rust **2.28× lower** VM memory |
| `slice` (9 programs) | compute | `case_*_wall_ms` (mean, per case) | 247.61–465.79 ms | 905.80–1 082.15 ms | **1.96×–3.85×** | Rust dearer — whole-process wall, **boot-dominated** (one process per call) |
| `foreign` (4 cases) | compute | `case_*_ns_per_call` | 324.45–548.45 ns | 21 624.28–74 352.55 ns | **49.8×–229.2×** | Rust dearer; the top of the band is the **new `enc` regression** (§8 item 1) |
| `list_walk` | compute | `case_list_walk_ns_per_element` | 7.1059 ns | **absent** (marginal below timer resolution) | — | no matched pair in this pass — see §3.4 |
| `copy_bound` (4 cases) | messaging | `case_*_ns_per_op` | 340.09–1 787.09 ns | 15 470.90–217 031.88 ns | **45.5×–234.5×** | the blocked-round defect is **CLOSED** (m12 `send_recv` was 19 975.1×) |
| `gc_bound` | gc | `case_churn_ns_per_op` | 7 967.6115 ns | 330 493.5681 ns | **41.5×** | Rust dearer (allocate-and-abandon churn); load-sensitive |

**(d) Tier effect (Rust vs Rust — never a system-ERTS comparison).** Read from the matched-form `--quick` pair (`m13-compare-tier-effect-quick.txt`, `a` = interpreter, `b` = default): the default (baseline JIT) tier is cheaper on `ets_copy` (**1.386×**), `send_recv` (**1.564×**), `bstr_to_lower` (3.449×, load-drift-inflated) and `emqx_base62:encode` (1.066×); it is **dearer** on `gc_bound churn` (**1.191×** — reversing m12's 2.07× default advantage) and within the drift band on `etf`/`send_recv_plain`/`list_walk`; `startup`/`vm_memory` parity-to-0.914× (§3.9). **Every tier table is Rust-vs-Rust and is never read against a `sys` column.**

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

`/proc/loadavg` is **host-wide** and this machine is shared. Across the m13 session the 1-min value ran from `15.54` (the `foreign` ladder's later passes) to `135.80`; **the primary cross-runtime pass sat at 72.46–135.80 (median 113.13)** and the two tier controls at 59.30–87.12 (median 73.08) and the external-target pass at 69.46–99.87 (median 81.97). Only the CLI probe (§3.8) ran in a quiet window (20.11–23.63). The A/B's **round 3 is excluded** because a load spike (~124) contaminated it; rounds 1–2 are the robust estimator (`results/m13-load-record.md`). Absolute latencies/walls/throughputs are therefore load-sensitive and must not be read as machine-speed constants; the load-robust signals are the **per-op / per-element / per-call marginals**, the **deterministic counters** (`*_ops`, `*_elements`, `*_calls`) and the **interleaved A/B** (§3.8). The `rust` whole-process walls are still partly dominated by the ~0.72 s boot/init, so `startup_ms`/`slice wall_ms` are not pure execution-throughput measures at this commit. Full before/after record: `results/m13-load-record.md`.

## 1. Environment (recorded with the run)

- **Commit measured:** runtime+driver tree `a141d6878c`; artifacts committed as `7801fabaea`. The m12-baseline binary used in the A/B (§3.8) is `beam_rt` from the m12 record's tree `ee3e426db0`; the m10-baseline binary is `beam_rt` from `b4593818b3`.
- **Oracle (`sys`):** system `erl` — **OTP 29.1.1** (`erlang:system_info(version)` = `17.1`) at `/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/lib/erlang/erts-17.1/bin/erl` (reports version `"29"` in every report).
- **Rust (`rust`):** `erts_rust/target/release/beam_rt`, `erlang:system_info(otp_release)` = `30` (reports version `"30"`), **default (baseline-JIT) tier**, self-check passing. Both runtimes report `supported: true` in every report.
- **Host:** NixOS 26.11 (Zokor), Linux 7.2.9 `x86_64`; Intel(R) Core(TM) Ultra 7 356H, `nproc` = **14**, `MemTotal` = 65 317 432 kB (~62.3 GiB).
- **Toolchain:** `rustc 1.98.1 (48a229cea 2026-09-01)`, `cargo 1.98.0`; `clang` **21.1.8**; `erlc` = the oracle's; `elixirc`/`mix` **1.20.4** ("compiled with Erlang/OTP 29").
- **Tools:** `dotnet` **9.0.318** (`PATH`; two 8.0.425 store SDKs are rejected by `command_for_min`'s min-`9` rule for `plb`); `rebar3` **not on `PATH`** — resolved from the Nix store by the driver's version-ranked scan (`rebar3-3.27.1`) ⇒ `khepri` runs; network available (`cowboy`'s build fetches cowlib/ranch).
- **Env vars:** `$HOME` read-only; `$BENCH_TRY_NIX`/`$BENCH_TOOL_PATH` unset; `$ERTS_ORACLE`/`$BEAM_RT`/`$BEAM_RT_TIER` unset for every run except the interpreter control (`BEAM_RT_TIER=interpreter`) and the matched A/B (per-variant `$BEAM_RT`).
- **Workload registry unchanged from m12** (12 workloads; no `src/**`/`tests/**` change this batch). Each row's status is recorded in §4/§5; `emqx` is the only non-`ok` row.

## 2. Exact commands

```sh
# build (once; the wrapper supplies a writable CARGO_HOME — $HOME is read-only)
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt -p bench

# A. FULL cross-runtime run — the six Runtimes::Both workloads, --full, both runtimes.
#    Load-log window 20:44:43 -> 20:58:13 UTC (load 72.46 -> 135.80, 55 samples).
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime all --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/results/m13-bench-xrun.json --verbose

# B. tier controls — same six workloads, rust only (load-log window 20:58:34 -> 21:02:49)
BEAM_RT_TIER=interpreter erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime rust --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/results/m13-bench-xrun-interp.json
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime rust --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound \
    --out-file erts_rust/tools/bench/results/m13-bench-xrun-quick.json

# C. FULL sys-only external targets (one report; load-log window 21:03:00 -> 21:09:30)
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime sys --workloads erlc_compiler,elixir_compiler,cowboy,khepri \
    --out-file erts_rust/tools/bench/results/m13-bench-external.json --verbose

# D. PLB (quick, ok)   E. EMQX (quick, skips immediately)
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime sys --workloads plb  --out-file erts_rust/tools/bench/results/m13-bench-plb.json  --verbose
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime sys --workloads emqx --out-file erts_rust/tools/bench/results/m13-bench-emqx.json --verbose

# F. the matched A/B (§3.8) — THREE variants, same command, $BEAM_RT overridden per variant,
#    interleaved round-by-round (head r1, m12 r1, m10 r1, head r2, ...); the case table's N is
#    identical on all three sides; 3 rounds captured, round 3 dropped (load spike ~124)
for r in 1 2 3; do
  BEAM_RT=erts_rust/target/release/beam_rt erts_rust/scripts/cargo.sh run ... -- \
      run --quick --runtime rust --workloads baseline,copy_bound,foreign \
      --out-file erts_rust/tools/bench/results/m13-ab-head-r$r.json            # HEAD a141d6878c
  BEAM_RT=$TMPDIR/m12wt/erts_rust/target/release/beam_rt erts_rust/scripts/cargo.sh run ... -- \
      run --quick --runtime rust --workloads baseline,copy_bound,foreign \
      --out-file erts_rust/tools/bench/results/m13-ab-m12-r$r.json             # m12 ee3e426db0
  BEAM_RT=$TMPDIR/m10wt/erts_rust/target/release/beam_rt erts_rust/scripts/cargo.sh run ... -- \
      run --quick --runtime rust --workloads baseline,copy_bound,foreign \
      --out-file erts_rust/tools/bench/results/m13-ab-m10-r$r.json             # m10 b4593818b3
done

# G. CLI probe (§3.8) — direct beam_rt CLI, interleaved 5 reps each, cwd = each variant's own
#    worktree root; bare startup / -version / -boot bootstrap/bin/start  -> m13-cli-probe.csv
# H. foreign N-ladder (§3.3, corroboration only) — direct bench_foreign_driver run, interleaved
#    5 passes x {head,m12,m10,sys} x {enc,cap,dec,lower} x N=0/5k/10k/25k/60k -> m13-foreign-ladder.csv

# I. compare artifacts (the tool diffs the mean)
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m13-bench-xrun.json              # -> m13-compare-cross-runtime.txt
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m13-bench-xrun-interp.json \
    erts_rust/tools/bench/results/m13-bench-xrun.json              # as-specified tier diff (form mismatch) -> m13-compare-tier-effect.txt
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m13-bench-xrun-interp.json \
    erts_rust/tools/bench/results/m13-bench-xrun-quick.json        # MATCHED-form quick <-> quick -> m13-compare-tier-effect-quick.txt
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m12-bench-xrun.json \
    erts_rust/tools/bench/results/m13-bench-xrun.json              # record vs record -> m13-compare-vs-m12-xrun.txt
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m12-bench-external.json \
    erts_rust/tools/bench/results/m13-bench-external.json          # -> m13-compare-vs-m12-external.txt
```

Workloads were run serially — one benchmark process at a time. Archived copies carry only the two mechanical normalisations (`runtimes[].program` worktree prefix → `erts_rust/target/release/beam_rt`; a scratch `$TMPDIR` prefix in `notes` → `$TMPDIR/`). **No metric value was altered.**

## 3. Cross-runtime results (system ERTS vs Rust ERTS)

Source: `m13-bench-xrun.json` (`--full --runtime all`, six `Runtimes::Both` workloads, both runtimes, **12/12 rows `ok`**, load 72.46 → 135.80). The marginals (`*_ns_*`) are the load-robust signal; the `rust` walls are boot/init-dominated (§3.10). Every `case_*` row also carries its deterministic companion (`case_*_ops`/`_calls`/`_elements` = the declared per-runtime `N`) and an `observed stdout:` agreement note; **cite the `N` with every marginal.**

### 3.1 `baseline` — startup latency, fixed compute loop and VM memory

Startup latency (n = 50, ms):

| statistic | sys (system ERTS) | rust (`beam_rt`, default tier) | rust/sys |
|---|---|---|---|
| mean | 315.3467 | 1 061.4786 | **3.37×** |
| p50 | 293.2932 | 1 024.5239 | — |
| p99 | 532.0101 | 1 624.8898 | — |

Compute loop + memory (n = 5):

| metric | sys | rust | ratio |
|---|---|---|---|
| `compute_loop_ms` (mean) | 377.1932 | 1 325.9140 | 3.52× (boot/init-dominated) |
| `compute_ops_per_sec` | 2 651 161.11 | 754 196.71 | rust **0.284×** |
| `vm_memory_total_bytes` | 36 197 840 B | 15 889 338 B | rust **2.28× lower** |
| `peak_memory_bytes` (process RSS) | 137 564 160 B (131.2 MiB) | 408 158 208 B (389.3 MiB) | rust 2.97× higher |

The compute loop is dominated by the natively-served `lists:seq/2` + `lists:sum/1` **inside the ~0.72 s boot/init**; read it as the native floor plus boot, not as execution throughput (that is `list_walk`/`foreign`). The `rust` startup mean fell from m12's 2 011.95 ms to **1 061.48 ms** (A/B pair: 590–673 ms on a quieter host, §3.8) — the boot-latency fix of §3.10 — but the `sys` side of this pass is load-inflated (315.35 ms vs m12's 109.11 ms), so the **3.37× ratio understates** the fix; the matched A/B (§3.8) is the right instrument for the startup move on the rust side. `vm_memory_total_bytes` is the runtime's *own* view (`erlang:memory(total)`; `beam_rt` serves it through its `MemoryHook` seam) and stays **2.28× lower** than the oracle's (unchanged from m12).

### 3.2 `slice` — the same nine compiled programs under both runtimes (n = 25/case)

`case_<name>_wall_ms` mean (one process per case, so the whole wall includes the rust boot):

| case | sys | rust | rust/sys |
|---|---|---|---|
| `add` | 281.2829 | 1 082.1479 | 3.85× |
| `fib` | 247.6073 | 947.1394 | 3.83× |
| `pick` | 294.4713 | 1 051.5465 | 3.57× |
| `calendar_last_day` | 278.8454 | 905.9719 | 3.25× |
| `queue_from_size` | 307.4515 | 926.2598 | 3.01× |
| `gb_trees_size` | 465.7879 | 1 059.4118 | 2.27× |
| `base64_encode_size` | 462.9703 | 905.8004 | 1.96× |
| `ordsets_from_size` | 404.9216 | 856.6336 | 2.12× |
| `string_str_length` | 363.3154 | 1 078.0630 | 2.97× |

Whole-workload `peak_memory_bytes`: sys 72 790 016 B (69.42 MiB) → rust 395 657 216 B (377.32 MiB); per-case rust RSS p50 is a tight 394.5–395.6 MiB (the boot's resident preloaded-module set), per-case sys 69.4–70.0 MiB. **Every rust wall is ~0.6–0.9 s of boot/init + one short call**: the spread across the nine cases (906–1 082 ms) is the *execution* cost, while the constant is the boot (§3.10). The rust walls fell ~48 %–59 % from m12 (2 077–2 444 ms → 906–1 082 ms) and the rust/sys band narrowed from **15.7×–18.7×** to **1.96×–3.85×**, but the `sys` side is load-inflated here (247–466 ms vs m12's 122–133 ms), so the band's width is mostly `sys`-side load, not execution. `slice`'s 9/9 `ok` on both sides confirms every case's stdout (`observed stdout:` note).

### 3.3 `foreign` — real Cowboy / EMQX modules, per-call cost under both runtimes

Each case is startup-cancelled: `ns_per_call = (mean wall@N − mean wall@0)/N × 1e6` over `--full` (n = 10 samples/case; `N` per case per runtime, recorded as `case_*_calls`).

| case | sys ns/call | rust ns/call | **rust/sys** | calls sys / rust | sys RSS p50 | rust RSS p50 |
|---|---|---|---|---|---|---|
| `bstr_to_lower` | 523.8590 | 44 293.3693 | **84.55×** | 1 500 000 / 20 000 | 66.75 MiB | 438.54 MiB |
| `bstr_capitalize_token` | 548.4547 | 33 094.2801 | **60.34×** | 1 500 000 / 15 000 | 66.19 MiB | 441.26 MiB |
| `emqx_base62_encode` | 324.4488 | 74 352.5495 | **229.17×** | 3 000 000 / 25 000 | 66.39 MiB | 427.79 MiB |
| `emqx_base62_decode` | 434.0741 | 21 624.2808 | **49.82×** | 3 000 000 / 15 000 | 66.93 MiB | 427.93 MiB |

**The Θ(N³) regression stays FIXED; the rust per-call cost is flat in `N`.** The m10-vs-m11 collapse (m11 ≈ 729 µs/call at `N` = 25 000 → m10-level tens of µs) is unchanged, and the drivers' matched A/B (§3.8) reproduces it. But the **band widened from m12's 48.0×–76.9× to 49.8×–229.2× at m13** because **`emqx_base62:encode` regressed on the rust side** (74.35 µs/call, head 3.2×–3.8× dearer than the m10 binary in the A/B; §8 item 1, **OPEN**), while the other three sub-cases are at or better than m10 in the A/B (`decode` 0.55×–0.70×, `capitalize_token` 0.75×–0.98×) or in-band (`bstr_to_lower` — see the ladder caveat below). The `sys` ns/call values are load-inflated vs m12 (all four +13 % to +50 %), so read the band, not the individual ratios.

**Corroboration only — the direct `bench_foreign_driver` N-ladder** (`m13-foreign-ladder.csv`: interleaved 5 passes × {head,m12,m10,sys} × {enc,cap,dec,lower} × `N`=0/5 000/10 000/25 000/60 000, startup-cancelled marginal `(mean wall@N − mean wall@0)/N`, median over 5 passes). It is **noise-amplified under load** (each row's loadavg is recorded; it ran 89.53 → 23.29), so it is a *corroboration*, not the estimator — the robust estimator is the driver A/B (§3.8):
- `enc` head/m10 **4.515×** at `N` = 60 000 and **3.830×** at `N` = 25 000; head/m12 **4.171×** at 60 000 and **3.618×** at 25 000 — it corroborates that **`enc` is the one regressed sub-case**.
- `cap` head/m10 0.74×–0.88×; `dec` head/m10 0.67×–0.74×; `lower` head/m10 0.68×–0.98× (so in the ladder `lower` reads at parity, while the A/B reads it 1.26×–1.40× — the one sub-case whose verdict differs between the two instruments; reported as a band, not a regression).

The residual rust cost is the flat per-call Rust host-context cost (the `beam_rt` per-call dispatch, owned by `../../crates/beam_rt/PERF.md`), not a per-slice registration loop. **Caveat kept:** `ns_per_call` is a snapshot at the declared `N`, and the rust peak RSS (~428–441 MiB) is **boot-dominated**, not a like-for-like memory comparison (§6.6).

### 3.4 `list_walk` — the per-element dispatch-throughput metric (n = 10/case)

| metric | sys | rust | rust/sys |
|---|---|---|---|
| `case_list_walk_ns_per_element` | 7.1059 | **absent** — `marginal below timer resolution` | — |
| `case_list_walk_elements` (deterministic) | 100 000 000 | 1 000 000 | per-runtime `N` |
| `case_list_walk_peak_rss_bytes` p50 | 66.42 MiB | 376.45 MiB | boot-inflated |

A BIF-free, non-tail-recursive `sum/1` over a 1000-element list (~7 dispatched instructions per element). **The rust side produced no marginal**: `mean wall@1 000 walks = 929.85 ms ≤ mean wall@0 = 1 036.50 ms`, so the workload emits its honest note (`case list_walk: marginal below timer resolution; no ns_per_element metric`) rather than a number. This is a **measurement gap, not a dispatch regression** — the ~0.72 s boot/init still carries ±100 ms of jitter, which swamps the `N` = 1 000-walk (~0.1 s) loop marginal (**OPEN, owner `bench` — §8 item 3**). The `sys` side (N = 100 000 walks, a ~1.0 s loop) still resolves it at **7.1059 ns/element** (load-inflated vs m12's 3.3433 ns; the `sys` side is the one that moves with load). The interpreter tier *does* resolve the rust marginal (**419.6419 ns/element**, §3.9). **No rust/sys ratio is publishable for this workload this pass.**

### 3.5 `copy_bound` — the copy boundary: ETF, ETS and process send/receive (**four** cases, n = 10/case)

| case | sys ns/op | rust ns/op | **rust/sys** | ops sys / rust | sys RSS p50 | rust RSS p50 |
|---|---|---|---|---|---|---|
| `etf_roundtrip` | 812.0279 | **absent** (`marginal below timer resolution`) | — | 1 500 000 / 5 000 | 65.96 MiB | 382.54 MiB |
| `ets_copy` | 340.0868 | 15 470.9003 | **45.49×** | 2 000 000 / 100 000 | 66.90 MiB | 450.12 MiB |
| `send_recv` | 1 787.0913 | 217 031.8844 | **121.44×** | 500 000 / 2 000 | 66.15 MiB | 382.63 MiB |
| `send_recv_plain` | 740.7461 | 173 712.8155 | **234.51×** | 1 000 000 / 2 000 | 66.34 MiB | 382.27 MiB |

Whole-workload `peak_memory_bytes`: sys 71 573 504 B (68.25 MiB) → rust 472 092 672 B (450.22 MiB). **All four rust cases are `ok` (4 ok, 0 unsupported of 4)** — `send_recv_plain`, which **timed out at m12**, now measures (**173.71 µs/op**), and `send_recv` fell from m12's 25.45 ms/op to **217.03 µs/op**: the ~25.5 ms blocked-round defect is **CLOSED** (owner `beam_rt`, outside this node; §8 item 2). `etf_roundtrip`'s rust marginal is now **unresolvable** (m12: 40 882.45 ns/op): `mean wall@5 000 = 1 073.09 ms ≤ mean wall@0 = 1 080.52 ms`, and the same cell moves over a 42–245 µs/op spread across runs at the same `N` = 5 000 (§8 item 3). The `rust` marginals here are **load-sensitive** (`send_recv` ranged 28.14–68.93 µs/op in the A/B and 217.03 µs/op in the ×run at load 72–136); the A/B pair (§3.8) is the load-robust view. `ets_copy`'s rust residue (450 MiB RSS, the largest) is the `Nursery.retired` per-op retention (§6.6).

### 3.6 `gc_bound` — allocate-and-abandon churn (n = 10/case)

| metric | sys | rust | rust/sys |
|---|---|---|---|
| `case_churn_ns_per_op` | 7 967.6115 | 330 493.5681 | **41.48×** |
| `case_churn_ops` (deterministic) | 200 000 | 2 000 | per-runtime `N` |
| `case_churn_peak_rss_bytes` p50 | 66.03 MiB | 376.42 MiB | boot-inflated |

Each iteration builds and abandons three 256-element structures (a pure-Erlang list, a 256-byte binary, a tuple) and checks the two O(1)-checkable sizes — a silently dropped allocation fails loudly. The row measures **allocate-and-abandon wall time and claims no collection count**. This marginal is the **least stable in the report** — m12 `108 251` ns/op → m13 `330 494` ns/op (+205 %), driven partly by the heavier load and partly by the ~0.72 s boot's jitter in the startup subtraction — so read the band, not the ratio. On a Rust-vs-Rust basis the default JIT is now **1.191× dearer** than the interpreter on this loop (§3.9), reversing m12's 2.07× default advantage.

### 3.7 Reading the cross-runtime numbers

- **The marginals are the cross-runtime signal.** `foreign`/`list_walk`/`copy_bound`/`gc_bound` are startup-cancelled per-unit costs, so a fixed rust startup cancels on both sides *by construction* — but the cancellation only works when the loop marginal exceeds the startup jitter, and at `N` = 1 000 walks / `N` = 5 000 copy round-trips it now does **not** (§3.4, §3.5).
- **The rust whole-process walls are still boot/init-dominated** (`baseline startup_ms`, `slice wall_ms`): read them as "boot + execution", not execution. They fell sharply from m12 (~2.1 s → ~0.72 s) and the residual constant is the process-init cost (§3.10).
- **This pass ran at load 72.46–135.80**, so both columns are load-inflated vs m12's quiet re-take; the *ratios* move accordingly (the `sys` side lost more to contention), which is exactly why the record-vs-record deltas (§7) are labelled cross-load and the interleaved A/B (§3.8) is the move-finder.
- **`vm_memory_total_bytes` is the runtime's own accounting** and is not comparable with `peak_memory_bytes` (external `VmHWM`): the first is 2.28× *lower* for rust, the second 2.97× *higher* (boot's resident set).
- **Deterministic counters** (contention-immune): `copy_bound ops` = 1 500 000 / 2 000 000 / 500 000 / 1 000 000 (sys) and 5 000 / 100 000 / 2 000 / 2 000 (rust); `foreign calls` = 1 500 000 / 1 500 000 / 3 000 000 / 3 000 000 (sys) and 20 000 / 15 000 / 25 000 / 15 000 (rust); `list_walk elements` = 1e8 (sys) / 1e6 (rust); `churn ops` = 200 000 (sys) / 2 000 (rust). Per-runtime `N` differs by design.

### 3.8 The startup re-derivation: the CLI probe and the matched A/B

**The CLI probe** (`m13-cli-probe.csv`; interleaved, 5 reps each, loadavg **20.11–23.63** — the one quiet window; binaries identified by sha256 prefix: head `2f7f0b3c04d6`, m12 `325848e5b4d9`, m10 `49f48c054b15`):

| probe (median, range) | head `a141d6878c` | m12 `ee3e426db0` | m10 `b4593818b3` | sys |
|---|---|---|---|---|
| bare startup (`-noshell -eval 'halt().'`) | **721 ms** (698–778) | 2 101 ms (2 094–2 118) | 20 ms (20–24) | 200 ms (176–227) |
| `-version` | **3 ms** | 4 ms (2–5) | 3 ms (2–3) | 21 ms (15–25) |
| `-boot bootstrap/bin/start` | **733 ms** (708–785) | 2 113 ms (2 104–2 199) | 430 ms (395–440) | — |

All boot runs exit 0. Derived: **head is 2.91× faster than m12 on bare startup** (2 101/721) and **2.88× on the scripted boot** (2 113/733); head's scripted boot is **1.70× slower than m10's** (733/430); on the quiet probe head's bare startup is **3.6× the oracle's** (721 vs 200 ms). Structural fact: **head's bare startup (721 ms) ≈ its scripted boot (733 ms)**, so the ~0.72 s is **process-init cost**, while m10's bare startup was 20 ms because its CLI route performed no boot.

**The matched A/B** (`m13-ab-{head,m12,m10}-r{1,2,3}.json`; run as `--quick --runtime rust --workloads baseline,copy_bound,foreign` with `$BEAM_RT` per variant, interleaved round-by-round, so the case table's `N` is identical on all three sides; **rounds 1–2 are the robust estimator, round 3 was contaminated by a load spike ~124 and is excluded** — `results/m13-load-record.md`):

| metric | head r1/r2 | m12 r1/r2 | m10 r1/r2 |
|---|---|---|---|
| `startup_ms` | 590 / 673 | 2 041 / 2 125 | 21 / 16 |
| `send_recv` ns/op | 68 928 / 28 144 | 25 425 031 / 25 477 674 | 136 450 / 119 740 |
| `send_recv_plain` ns/op | absent / 132 271 | absent / absent (timed out) | 115 360 / 107 935 |
| `ets_copy` ns/op | 8 925 / 9 456 | 2 992 / 2 691 | 4 057 / 3 997 |
| `etf_roundtrip` ns/op | 42 330 / 47 853 | 44 812 / 44 036 | 11 071 / 10 807 |
| `emqx_base62_encode` ns/call | 69 057 / 78 439 | 14 983 / 19 762 | 21 489 / 20 411 |
| `emqx_base62_decode` ns/call | 15 415 / 11 695 | 18 144 / 22 066 | 22 116 / 21 416 |
| `bstr_capitalize_token` ns/call | 29 672 / 24 805 | 23 335 / 25 471 | 39 538 / 25 206 |
| `bstr_to_lower` ns/call | 26 970 / 26 163 | 30 102 / 24 971 | 21 421 / 18 700 |
| `vm_memory_total_bytes` | 15 889 338 | 15 892 346 | 805 458 (**m10's figure is a pre-boot placeholder — not comparable**) |
| peak RSS (rust) | ≈408 / 472 / 463 MB | ≈408 / 472 / 463 MB | ≈32 / 95 / 80 MB (m10 does not boot a script) |

Derived headline A/Bs (each re-derived from the matched A/B, not restated):
- **Startup:** head is **3.46× (r1) / 3.16× (r2) faster than the m12 binary** — the m12 numbers are the pre-fix ones, so this is a re-derivation of the m12→m13 boot fix.
- **`send_recv`:** head is **369× (r1) / 905× (r2) faster than m12**, and also faster than m10 (**2.0× / 4.3×**). `send_recv_plain` went from "timed out" (m12) to measured (head r2 = 132 271 ns/op; the ×run's 173 713 ns/op is one point in a 28–217 µs/op band).
- **`ets_copy`:** head is **2.20× (r1) / 2.37× (r2) dearer than m10** — the remaining rust residue (§8 item 2).
- **`emqx_base62:encode`:** head is **3.21× (r1) / 3.84× (r2) dearer than m10** and **4.61× (r1) / 3.97× (r2) dearer than m12** — the new regression (§8 item 1, **OPEN**).
- **`emqx_base62:decode`** 0.70× / 0.55× of m10 (**faster**); **`bstr_capitalize_token`** 0.75× / 0.98× of m10 (at-or-better); **`bstr_to_lower`** 1.26× / 1.40× of m10 (dearer in the A/B, but 0.68×–0.98× in the ladder — the one sub-case whose verdict differs between instruments; band, not regression).

### 3.9 The **tier effect** — what the JIT default does and does not improve (Rust vs Rust)

Two `--quick` runs, same six workloads, `--runtime rust` only, read like-for-like (`m13-compare-tier-effect-quick.txt`; `a` = interpreter, `b` = default). **No `sys` rows exist in either file** — this is a tier effect, never a system-ERTS comparison. The as-specified `m13-compare-tier-effect.txt` (interpreter `--quick` vs default `--full`) is a **form mismatch** and is kept only as the companion.

| workload | metric | interpreter | default (baseline JIT) | interp→default |
|---|---|---|---|---|
| `baseline` | `startup_ms` | 798.2448 | 873.2962 | 0.914× (default dearer) |
| `baseline` | `compute_loop_ms` | 869.8937 | 1 148.4262 | 0.758× (native floor; noise) |
| `baseline` | `vm_memory_total_bytes` | 15 889 338 | 15 889 338 | **1.000× (parity)** |
| `copy_bound` | `etf_roundtrip` ns/op | 115 978.60 | 244 878.23 | within the 42–245 µs/op noise band (§3.5) |
| `copy_bound` | `ets_copy` ns/op | 15 568.02 | 11 230.50 | **1.386× (default cheaper)** |
| `copy_bound` | `send_recv` ns/op | 127 216.80 | 81 352.08 | **1.564× (default cheaper)** |
| `copy_bound` | `send_recv_plain` ns/op | **19 878.30** | metric-only-in-a | resolves only under the interpreter |
| `gc_bound` | `churn` ns/op | 184 403.41 | 219 562.27 | **0.840× (default DEARER — reverses m12's 2.07× default advantage)** |
| `foreign` | `bstr_to_lower` ns/call | 35 966.92 | 10 429.11 | 3.449× (default cheaper; **load-drift-inflated** — see below) |
| `foreign` | other 3 cases ns/call | — | — | cap 1.410×, enc 1.066× (default cheaper); dec 0.937× |
| `list_walk` | `ns_per_element` | **419.6419** | metric-only-in-a | resolves only under the interpreter |
| `slice` | 9 cases wall_ms | 627.4–1 036.7 | 629.7–1 073.0 | 0.95×–1.06× (boot/init-dominated) |

Two facts to carry forward:
1. **`bstr_to_lower`'s 3.449× default-cheaper figure is partly load drift, not tier.** The default side's *0-iteration* mean was **1 101.75 ms** vs the interpreter side's **728.44 ms**, i.e. the default control was measured under heavier load; only part of the 3.449× is the tier.
2. **The `gc_bound churn` tier effect reversed** (m12: default 2.07× cheaper; m13: default **1.191× dearer**). This is a Rust-vs-Rust observation on a load-sensitive marginal, not a system-ERTS comparison, and no tier conclusion is drawn from one `--quick` pair.

### 3.10 `beam_rt`'s boot status at this commit — boot **COMPLETES** at ≈0.72 s

Factual probe (`m13-cli-probe.csv`; run with cwd = each variant's own worktree root):

```
$ erts_rust/target/release/beam_rt -boot bootstrap/bin/start
exit_code=0        wall median 733 ms (708–785 ms, 5 reps)
Erlang/OTP 30 [erts-17.1] [source] [64-bit]
```

- **Boot COMPLETES — exit 0** (m11 recorded exit 3 at directive 18 of 22; that fact is long obsolete).
- **The `run`-route startup is now ≈0.72 s** (bare `-noshell -eval 'halt().'` median 721 ms; `-boot …` median 733 ms), down from ≈2.1 s at m12 (median 2 101 / 2 113 ms) — **2.91× / 2.88× cheaper**; the fixed `clock_nanosleep`/`epoll_wait` waits were removed (`TimerWait::ImminentWithin` + a non-destructive zero-timeout signal probe replacing the 25 ms `PORT_POLL_SLICE` sleep). `-version` is ≈3 ms (m12 3–4 ms; the oracle 21 ms). **The CLI probe's `-boot` route boots this runtime's own release's `bootstrap/bin/start.boot`** (OTP 30 / erts-17.1), not the installed OTP-29 script.
- **Residual:** head's bare startup (721 ms) ≈ its scripted boot (733 ms), so the ~0.72 s is **process-init cost**; head's scripted boot is **1.70× m10's** (733/430 ms). Owner `beam_rt` (observation; §8 item 5).

## 4. System-ERTS target-project results (sys-only workloads)

Source: `m13-bench-external.json` (`--full`, `erlc_compiler`, `elixir_compiler`, `cowboy`, `khepri`; **4/4 `ok`**), `m13-bench-plb.json` (`ok`), `m13-bench-emqx.json` (`skipped`). Per-target notes: `results/m13-tool-targets-notes.md`. All five running rows are `SysOnly` — the target apps cannot boot under `beam_rt` — and this pass ran at load 69.46–99.87, so the walls below are load-inflated vs a quiet host.

### 4.1 `plb` — Programming-Language-Benchmarks (dotnet)

`ok`: `wall_ms=122 157.468348` (two stages: `--task build` 32 871 ms + `--task bench`), `test_time_ms` n=10 min `0.0` mean `1 326.31` p50 `700.51` p90 `3 637.37` p99 `4 513.28` max `4 610.61`, `test_mem_bytes=90 877 952`, `peak_memory_bytes=169 918 464`. `--quick`; `test_time_ms` is PLB's own per-test time across its 10 elixir kernels and `wall_ms` is dominated by building their escripts. **Tool resolution re-verified in the row's notes:** `resolved dotnet 9.0.318 at /run/current-system/sw/bin/dotnet (PATH)` with `rejected dotnet 8.0.425 … (nix-store): requires >= 9` twice. Versus m12 wall 70 648.46 → 122 157.47 ms (read manually — no tool diff archived for `plb`): the shared host, not a toolchain change.

### 4.2 `khepri` — Khepri `rebar3 eunit`

`ok`: `wall_ms=89 131.579260`, `peak_memory_bytes=387 690 496` (m12 77 564.34 ms / 374 722 560 B ⇒ **+14.91 %** wall, load-attributable). **Caveat:** the store `rebar3` is an escript whose shebang points at an Erlang/OTP 28 `escript`, so the suite runs under **OTP 28** while this `SysOnly` row's `runtimes[]` entry describes the driver's oracle (`erl` on `PATH`, OTP 29). The wall time is real; the reported runtime version describes the driver's runtime, not necessarily the Erlang the tool chose.

### 4.3 `emqx` — EMQX umbrella build (`make`/`mix` through `env.sh`) — **`skipped(emqx-requires-otp-28)`**

Unchanged from m9–m12 and re-verified this pass (immediate skip, **no number**). The checkout pins OTP 28 (`/home/bill/Source/emqx/.tool-versions:1` → `erlang 28.5.0.7-2`; `env.sh:3` → `export OTP_VSN=28.5.0.7-2`) while the host `erl` is OTP 29.1.1 / erts-17.1. The umbrella build aborts in `:ezstd`'s NIF makefile, which generates `env.mk` with `erl -noshell -s init stop -eval '…'`; on OTP 29 `init stop` halts **before** the `-eval` (verified: exit 0, no output), so `mix compile` fails with `No rule to make target '…/env.mk'`. Even with that idiom patched, the umbrella build needs host build tooling/libs absent here (`:jq`'s `autoreconf`/`configure` needs `automake`+`libtool`+`cmake`, and `:sasl_auth` needs Kerberos headers — `krb5.h`). The pin lives in the read-only target; not satisfiable without a full toolchain swap (out of scope).

### 4.4 `cowboy` — hello_world HTTP (in-process HTTP/1.1 load generator, 16 connections × 5 s — `--full`)

`ok`: `build_ms=59 013.915121` (relx release; network fetch of cowlib/ranch), `requests=307 086`, `errors=0`, `requests_per_sec=61 363.950413`, `latency_ms` n=20 000 min `0.025545` mean `0.267918` p50 `0.145991` p90 `0.520572` p99 `2.160520` max `18.531975`, `peak_memory_bytes=160 616 448`. Versus m12: throughput 165 489.21 → 61 363.95 req/s (**−62.92 %**), `build_ms` +38.77 %, mean latency +153.55 % — the shared host's load (the pass sat at 69.46–99.87), not a server change.

### 4.5 `erlc_compiler` — `erlc` over the stdlib + Elixir corpus slice

`ok`: `wall_ms=9 679.605467`, `files_compiled=60`, `files_per_sec=6.198600`, `loc_per_sec=4 804.224734`, `peak_memory_bytes=130 109 440`, note `corpus: 60 files (46503 LOC) attempted from stdlib+elixir; 60 produced .beam (46503 LOC)`. The rates are computed from the **produced** `.beam`/LOC counts, so a partial run cannot overstate them. Versus m12: wall 6 842.18 → 9 679.61 ms (**+41.47 %**), files/s 8.769 → 6.199 (**−29.31 %**) — a host-load move with the same unmodified system `erlc`.

### 4.6 `elixir_compiler` — installed system `elixirc` over the checkout's `.ex` corpus

`ok`: `wall_ms=73 993.345623`, `files_attempted=105`, `files_compiled=223`, `files_per_sec=3.013785`, `loc_per_sec=1 513.163097`, `compiler_startup_ms=531.154492`, `peak_memory_bytes=208 650 240`. Notes: `batch compile failed; per-file fallback (version skew)`; `compiler: Elixir 1.20.4 (compiled with Erlang/OTP 29)`; `97/105 compiled; 223 .beam produced`; `223 .beam file(s) produced from 105 source file(s) (an Elixir source may define several modules)`. **`files_compiled` (223) exceeding `files_attempted` (105) is expected and documented by the workload itself**; the throughput rate uses the produced count. Versus m12: wall 50 304.34 → 73 993.35 ms (+47.09 %), files/s −32.02 %, compiler startup **+78.45 %** — the load.

## 5. Workloads that did **not** run in the m13 measurement run — verified reasons

| workload | status | exact reason |
|---|---|---|
| `emqx` | `skipped(emqx-requires-otp-28)` | the checkout pins OTP 28.5.0.7-2 while the host `erl` is OTP 29.1.1 (§4.3) — the only non-`ok` row in the whole milestone |

Nothing else skipped: the six `Runtimes::Both` workloads produced **12/12 `ok`** rows, and the `copy_bound` rust cases are now **4/4 `ok`** (`send_recv_plain`, a recorded failure at m12, now measures). No rust number is fabricated anywhere.

## 6. Reproducibility, stability and known measurement caveats

### 6.1 The host is shared and loaded, and this pass was a loaded one
The runner is runtime-major (all `sys` rows, then all `rust` rows), so a drifting load biases a cross-runtime comparison. **The m13 primary pass ran at 1-min load 72.46–135.80 (median 113.13)**, the tier controls at 59.30–87.12 and the external pass at 69.46–99.87; only the CLI probe (20.11–23.63) was quiet. A quieter re-take of the full pass was **attempted and abandoned** (the window closed: load fell to ~12 then rose to ~84) and produced no artifact. Read ratios — but note several marginals moved >20 % vs m12 and two rust marginals (`list_walk`, `etf_roundtrip`) did not resolve at all (§3.4, §3.5). `results/m13-load-record.md` has the before/after record.

### 6.2 `beam_rt` resolves its code path natively from the filesystem
`crates/beam_rt/src/codeserver.rs` probes (`$ERTS_STDLIB_DIR` → the OTP layout under the `erl` on `$PATH`, *resolved not executed* → this executable's ancestors → `$ERL_TOP` → standard prefixes); the system `erl` is only *run* as a last resort. `bench` never sets `$ERTS_STDLIB_DIR` — it is a pure driver/observer.

### 6.3 `elixir_compiler` measures the installed `elixirc` 1.20.4
The in-repo checkout is unbuilt and read-only, so a version skew with the checkout is expected and the per-file fallback runs (§4.6).

### 6.4 `cowboy` re-fetches cowlib/ranch in a fresh scratch copy on every run (network)
The build is therefore host/network-dependent (59.0 s at m13 vs 42.5 s at m12) and is excluded from the latency figures (which come from the load phase only).

### 6.5 Peak memory is Linux `VmHWM` polling of the child (`peak_memory_bytes`)
Unreadable ⇒ `null` + a note; never fabricated. It is never conflated with `vm_memory_total_bytes` (`erlang:memory(total)`, §3.1/§3.7).

### 6.6 `foreign` / `list_walk` / `copy_bound` / `gc_bound` `wall_ms` are not comparable across runtimes
They loop `N` ops **per process** at a per-runtime `N`; only the startup-cancelled per-unit marginals and the deterministic counters are cross-runtime comparable. Each side also carries its own startup offset — cancelled **by construction** wherever the marginal resolves (which is precisely why the marginal is the right instrument), but `list_walk`'s and `etf_roundtrip`'s rust marginals did **not** resolve this pass, so those two cells published no number (§3.4, §3.5).

### 6.7 The rust peak RSS of the loop workloads is **boot-dominated** at this commit
The `foreign`/`list_walk`/`copy_bound`/`gc_bound` rust peak RSS rows (~382–450 MiB) are dominated by the **boot's resident preloaded-module set** rather than the loop count (`list_walk` `N` = 1 000, `gc_bound` `N` = 2 000 and `ets_copy` `N` = 100 000 all sit at ≈382 / 376 / 450 MiB). The underlying per-op retention mechanism (BIF-result copy-out → `Process::grow_nursery` → `Nursery::grow` → `Nursery.retired`, cleared only by a real collection) is unchanged and still owned by `beam_rt`/`beam_process`/`beam_gc` — the **memory row is not a like-for-like comparison**, and `slice`'s rust RSS is likewise the boot's resident set. `vm_memory_total_bytes` (§3.1) remains the clean rust-vs-sys memory comparison (**2.28× lower**).

### 6.8 Contract A.7 — the `beam_rt` boot status at this commit **COMPLETES** at ≈0.72 s
`beam_rt -boot bootstrap/bin/start` exits 0 and the CLI route boots this runtime's own release's `bootstrap/bin/start.boot`. What remains is **fidelity**, not completion: **no self-hosted build**, no live `code_server`/`erl_prim_loader` process in a plain runtime. No boot-completion claim beyond the above is made anywhere.

## 7. Deltas vs the previous run (`m13` vs `m12`; `m11`/`m10`/`m9`/`m6`/`m5`/`m4` earlier)

Source: `m13-compare-vs-m12-xrun.txt`, `m13-compare-vs-m12-external.txt` (the tool diffs the **mean**; `a` = m12, `b` = m13). **These are record-vs-record deltas across two different host loads (m12 at 5.77 → 2.09; m13 at 72.46 → 135.80), so they are cross-load, NOT matched A/Bs** — the load-robust move-finder is §3.8.

### 7.1 m13 vs m12 — the cross-runtime marginals (rust side)

| (workload, metric) | m12 | m13 | Δ |
|---|---|---|---|
| `baseline` `startup_ms` **rust** | 2 011.9543 | **1 061.4786** | **−47.24 %** (the boot fix) |
| `baseline` `compute_loop_ms` **rust** | 2 059.9596 | **1 325.9140** | −35.63 % (boot) |
| `baseline` `vm_memory_total_bytes` **rust** | 15 892 346 | **15 889 338** | −0.02 % |
| `copy_bound` `send_recv` ns/op **rust** | 25 447 011.2735 | **217 031.8844** | **−99.15 %** (blocked-round defect CLOSED) |
| `copy_bound` `send_recv_plain` (rust rows) | **failed (60 s deadline)** | **173 712.8155 ns/op** | now measured |
| `copy_bound` `etf_roundtrip` ns/op **rust** | 40 882.4466 | **absent** (`metric-only-in-a`) | marginal did not resolve (§3.5) |
| `copy_bound` `ets_copy` ns/op **rust** | 2 876.4047 | **15 470.9003** | +437.86 % (load + loader literal-tagging residue) |
| `foreign` `emqx_base62_encode` ns/call **rust** | 15 675.9526 | **74 352.5495** | **+374.31 %** (the new `enc` regression; §8 item 1) |
| `foreign` `bstr_to_lower` ns/call **rust** | 26 890.9337 | **44 293.3693** | +64.71 % |
| `foreign` `bstr_capitalize_token` ns/call **rust** | 23 283.5499 | **33 094.2801** | +42.14 % |
| `foreign` `emqx_base62_decode` ns/call **rust** | 16 851.8374 | **21 624.2808** | +28.32 % |
| `gc_bound` `churn` ns/op **rust** | 108 250.5757 | **330 493.5681** | +205.30 % (least stable marginal; load-sensitive) |
| `list_walk` ns/element **rust** | absent | **absent** | no rust figure either pass |
| `slice` rust walls | 2 076.98–2 443.59 ms | **905.80–1 082.15 ms** | the boot fix (one process per case) |

The `sys` side of the same diff moved **up** (a loaded pass vs a quiet one): `startup_ms` 109.11 → 315.35 ms (**+189.01 %**); `slice` walls 122–133 → 247–466 ms (+94 % to +276 %); `copy_bound` ns/op +4.13 % to +40.28 %; `foreign` ns/call +13.10 % to +49.84 %; `gc_bound` churn +79.09 %; `list_walk` ns/element +112.54 %. **Because both sides moved, the record-vs-record ratios understate the rust-side work** — the matched A/B (§3.8) is the correct instrument for the m12→m13 move (startup 3.16×–3.46× faster, `send_recv` 369×–905× faster, `enc` 3.97×–4.61× dearer).

### 7.2 Real, code-attributable changes since m12 (owners **outside this node**)
1. **The blocked-round / boot-latency fix** — every `run`/`-eval`/`-boot` action dropped from ~2.1 s to ≈0.72 s and the ~25.5 ms `PORT_POLL_SLICE` blocked round is gone (`send_recv` +118 136 % at m12 → −99.15 % now; `send_recv_plain` from timed-out to measured). Boot **completes** (exit 0) as at m12.
2. **The `foreign` Θ(N³) fix stays closed** — re-derived in the A/B (§3.8), not copied.
3. **New rust regression: `foreign: emqx_base62:encode`** — head 3.2×–3.8× dearer than m10 in the A/B, while the other sub-cases are at-or-better (§8 item 1, **OPEN**, owner `beam_rt`).

### 7.3 Load-attributable / not comparable
External targets: `erlc_compiler` wall +41.47 % (files/s −29.31 %); `elixir_compiler` wall +47.09 % (files/s −32.02 %, startup +78.45 %); `cowboy` req/s −62.92 %, mean latency +153.55 %, `build_ms` +38.77 %; `khepri` wall +14.91 %; `plb` wall 70 648.46 → 122 157.47 ms. All on the shared loaded host, none a like-for-like quiet comparison. Within the `xrun`, the absence of the `list_walk`/`etf_roundtrip` rust marginals and the `gc_bound` swing are startup-subtraction jitter amplified by the load, not execution changes.

### 7.4 Unchanged / still-open
- `ets_copy`'s residue and `gc_bound`'s churn marginal remain real rust execution costs.
- The per-op retention mechanism behind the loop workloads' rust RSS (§6.7) is unchanged (owner outside this node).
- No Rust-runtime row exists for the six `SysOnly` targets (§4, §5); `emqx` is the one exact mechanical skip.

## 8. Ranked bottleneck list & OPEN items (from the m13 data)

Ranked by magnitude × how load-robust the measurement is, with the evidence, the owner and the status.

1. **`foreign: emqx_base62:encode` rust regression — 229.17× vs `sys` (74.35 µs/call); head 3.21×–3.84× dearer than the m10 binary and 3.97×–4.61× dearer than the m12 binary in the matched A/B. OPEN — owner `beam_rt`.** *Evidence:* the ×run row (74 352.5495 ns/call) and the A/B (`m13-ab-{head,m12,m10}-r{1,2}`); the ladder corroborates (head/m10 **4.515×** @60k, 3.830× @25k). Explicitly **NOT** attributable to the EMQX target — its source is unchanged and its **`decode` sub-case is *faster* than m10 (0.55×–0.70×)**. *Cause (suspected):* a rust per-call cost adjacent to the same mechanism that makes the `list_walk` marginal unresolvable (item 3). *Fix shape:* a `beam_rt` per-call/`base62:encode`-path profile.
2. **`ets_copy` rust residue — 45.49× vs `sys` (15.47 µs/op); head 2.20×–2.37× dearer than m10. OPEN (fix in flight) — owner `beam_loader`'s literal-tagging step.** *Evidence:* the A/B pair (`m13-ab-*`) and the ×run row. *Cause:* `crates/beam_loader/src/literal.rs` — an untagged module-literal root makes the collector decline (being fixed in parallel this batch). *This is the only copy case not gated by a measurement gap.*
3. **Driver measurement gap at the declared rust `N` (owner: `bench`, this node) — OPEN.** At m13 the rust boot became ~2.9× cheaper, so `copy_bound::etf_roundtrip` (rust `N` = 5 000) and `list_walk` (rust `N` = 1 000) no longer lift the loop signal above the boot's run-to-run jitter: the `--full` pass could not resolve either marginal (`etf_roundtrip`: `mean wall@5 000 = 1 073.09 ≤ mean wall@0 = 1 080.52`; `list_walk`: `wall@1 000 walks = 929.85 ≤ wall@0 = 1 036.50`), and `etf` moves over a **42–245 µs/op spread** at the same `N` = 5 000 (A/B head 42.3–47.9 µs/op; the two `--quick` controls 244.9 / 116.0 µs/op). *Recommended fix (NOT done this batch):* recalibrate the rust-side loop counts (`src/workloads/copy_bound.rs`, `src/workloads/list_walk.rs`) so each loop does ≳1 s of work, then re-run — this needs a rebuild plus updating the tests that pin the case tables. **No rust-side `etf_roundtrip`/`list_walk` figure is quotable at m13 until then.**
4. **Load-sensitivity of the rust copy/foreign marginals.** `send_recv` reads 28 144 → 217 032 ns/op across the A/B and the ×run; `ets_copy` 8 925 → 15 471; the ×run at load 72–136 is the highest. Report the **band** and the A/B, never a single constant as a property of the runtime.
5. **Scripted boot vs m10 — head's `-boot bootstrap/bin/start` (733 ms) is 1.70× m10's (430 ms). Owner `beam_rt` (observation).** The ~0.72 s is process-init cost (head's bare startup 721 ms ≈ its scripted boot 733 ms), not boot-script work.
6. **Boot-dominated memory — the rust peak RSS of the loop workloads (`foreign`/`list_walk`/`copy_bound`/`gc_bound`) is ≈382–450 MB vs the oracle's ≈66–69 MB and is NOT a like-for-like memory comparison; `slice` is the comparable per-process row.** Beneath it the documented general per-op heap retention (`Nursery::grow` → `retired`, cleared only by a real collection) remains — owner `beam_rt`/`beam_process`/`beam_gc` (§6.7).
7. **`emqx` stays an honest skip, never a number** (`skipped(emqx-requires-otp-28)`; §4.3, §5).

Remaining ranking context:
- **`foreign` general band 49.8×–229.2× (excluding the `enc` outlier, 49.8×–84.6×)** — the per-call cost is flat in `N` (the Θ(N³) fix holds); the residual is the flat rust per-call host-context cost, owned by `../../crates/beam_rt/PERF.md`, not by this node.
- **Whole-process walls: `baseline startup_ms` 3.37×, `slice` 1.96×–3.85×** — boot/init-dominated (~0.72 s + one short call); the ~2.1 s boot defect is **CLOSED** (item 5 is the residual).
- **`gc_bound churn` 41.48× (330.49 µs/op)** — the least stable marginal in the report; the default JIT is now 1.191× *dearer* than the interpreter on this loop (§3.9), a Rust-vs-Rust observation.
- **Contract A.9 / self-hosted build** — `beam_rt` still does not compile or run itself, and no compiler workload is executed by `beam_rt` (§4.5/§4.6 are system-toolchain reference rows).

## Honesty audit — measurement-integrity notes

No workload returns a hard-coded or estimated metric; a workload that fails to build/run is always `failed`/`skipped`, never a number (verified across `src/workloads/*.rs`, `src/compare.rs`, `src/report.rs`, `src/runtime.rs`). No `todo!`/`unimplemented!`/non-doc `unreachable!`, no `#[ignore]`.

- **The two absent rust marginals are reported as unresolved, never as numbers.** `copy_bound::etf_roundtrip` (rust `N` = 5 000) and `list_walk` (rust `N` = 1 000) each emitted their honest `marginal below timer resolution; no ns_per_op/element metric` note; no rust-side ratio is quoted for either (§8 item 3).
- **`foreign` classifies a staging failure honestly** (`src/workloads/foreign.rs`: `StageError` + `stage_outcome`): genuinely absent foreign `.erl` sources ⇒ `skipped(sources-missing)`; a failure to compile sources we *did* stage (the embedded driver or a present foreign source) ⇒ `failed(module-compile-failed)` — matching `list_walk`/`copy_bound`/`gc_bound`. `slice` treats a failed fixture rebuild as `skipped(fixture-missing)`.
- **`erlc_compiler`'s `files_per_sec`/`loc_per_sec` are computed from the *produced* counts**, so a partial `erlc` run cannot overstate throughput next to `files_compiled`. `elixir_compiler` likewise uses the produced count (§4.6).
- **A version-constrained tool is resolved by version, and the choice is auditable.** `plb` calls `tool::command_for_min(TOOL, TOOL_NIX_ATTR, MIN_DOTNET_SDK)` (min `9`); the resolved binary + version + source and every rejected candidate's reason are in the row's `notes` (§4.1). `khepri`'s `runtimes[]` version describes the driver's oracle, not necessarily the Erlang the store `rebar3` escript chose (§4.2).
- `proc.rs`'s drain-thread join (`h.join().unwrap_or_default()`) is infallible **by construction** — the drain threads run only `read_to_end` + a lossy UTF-8 conversion, neither of which can panic — so the `unwrap_or_default` masks nothing real.
- `baseline.rs`'s docs correctly state that both runtimes serve `erlang:memory/0,1` (`beam_rt` via its `MemoryHook` seam), recording a real `vm_memory_total_bytes` for `rust`.
- The `foreign`/`list_walk`/`copy_bound`/`gc_bound` startup-cancelled methodology is implemented exactly as this document states, and each of those workloads emits its deterministic companion (`case_*_ops` / `*_calls` / `*_elements`). The runtime-side hot-path figures (the registered-BIF dispatch cost) are `beam_rt` measurements (`../../crates/beam_rt/PERF.md`), **not** produced here.
- **The two m13 tier controls are both `--quick`** (matched form), and the as-specified `m13-compare-tier-effect.txt` (interpreter `--quick` vs default `--full`) is labelled a **form mismatch** and accompanied by `m13-compare-tier-effect-quick.txt`. Every tier table is labelled **Rust-vs-Rust**; every cross-runtime ratio names `sys` (system ERTS 29.1.1 / erts-17.1) and `rust` (`beam_rt` default tier) explicitly.
- **`m13` pass integrity:** the driver and `beam_rt` were built from the tree `a141d6878c`; every run's stdout was checked against the deterministic constant on both sides; the tier control is separate artifacts so the Rust tiers cannot be conflated with the `sys` column; the A/B used an **interleaved** head/m12/m10 schedule (each variant at matched `N`, each pass bracketed by its own `N=0` startups) so the comparison is drift-resistant, with round 3 excluded for a load spike; all committed artifact paths are normalised (`runtimes[].program` → `erts_rust/target/release/beam_rt`; a scratch `$TMPDIR` prefix in `notes` → `$TMPDIR/`). **No metric value was altered.**

**Reproducibility note (PLB / tool resolution).** `plb`'s outcome is *tool- and environment* state, not repository state. The resolver picks the **greatest `dotnet` ≥ 9.0**, so `plb` runs wherever any 9.x+ SDK is reachable. `khepri` needs only the `rebar3` store entry. `emqx` remains a genuine toolchain skip on any host without an OTP-28 Erlang that can pair with the Elixir/`mix` the umbrella build uses.

## Archived artifacts

| file | contents |
|---|---|
| `results/m13-bench-xrun.json` | **current cross-runtime run**: `--full --runtime all`, the six `Runtimes::Both` workloads × both runtimes, **12/12 rows `ok`**, load 72.46 → 135.80 — the source of §3 |
| `results/m13-bench-xrun-interp.json` | the tier control (interpreter): the same six workloads, `rust` only, `BEAM_RT_TIER=interpreter`, 6/6 rows `ok`, profile `quick`. **Rust-vs-Rust only; no `sys` rows.** |
| `results/m13-bench-xrun-quick.json` | the matched-form default-tier control: the same six workloads, `rust` only, default (baseline-JIT) tier, profile `quick` — so the tier table is read off two `--quick` runs. **Rust-vs-Rust only.** |
| `results/m13-compare-cross-runtime.txt` | `bench compare` — sys vs rust inside `m13-bench-xrun.json` (the tool diffs the **mean**) |
| `results/m13-compare-tier-effect.txt` | `bench compare m13-bench-xrun-interp.json m13-bench-xrun.json` — the as-specified interpreter(`--quick`) → default(`--full`) pair; **form mismatch** |
| `results/m13-compare-tier-effect-quick.txt` | `bench compare m13-bench-xrun-interp.json m13-bench-xrun-quick.json` — the **matched-form** `--quick` ↔ `--quick` tier control (§3.9) |
| `results/m13-compare-vs-m12-xrun.txt` / `results/m13-compare-vs-m12-external.txt` | the tool's two-file diffs against the m12 record — the source of §7 (**cross-load**, not matched A/Bs) |
| `results/m13-bench-external.json` | the external sys-only targets in one `--full` report: `erlc_compiler`, `elixir_compiler`, `cowboy`, `khepri` (**4/4 `ok`**; §4.2, §4.4–§4.6) |
| `results/m13-bench-plb.json` | `plb` → `ok`, `wall_ms` 122 157.47, `dotnet` 9.0.318 resolved from `PATH` (§4.1) |
| `results/m13-bench-emqx.json` | EMQX → `skipped(emqx-requires-otp-28)` (§4.3) |
| `results/m13-ab-head-r{1,2,3}.json` | the matched A/B, **HEAD (`a141d6878c`)** side — `--quick --runtime rust --workloads baseline,copy_bound,foreign`, `$BEAM_RT` = the HEAD `beam_rt`; round 3 excluded (load spike) |
| `results/m13-ab-m12-r{1,2,3}.json` | the A/B **m12 (`ee3e426db0`)** side (program normalised to `$TMPDIR/m12wt/erts_rust/target/release/beam_rt`) — the pre-fix startup/`send_recv` reference |
| `results/m13-ab-m10-r{1,2,3}.json` | the A/B **m10 (`b4593818b3`)** side (program normalised to `$TMPDIR/m10wt/erts_rust/target/release/beam_rt`) |
| `results/m13-cli-probe.csv` | the CLI probe: interleaved 5 reps × {head,m12,m10,sys} × {bare startup, `-version`, `-boot`}, wall/exit/loadavg, binaries by sha256 prefix (§3.8, §3.10) |
| `results/m13-foreign-ladder.csv` | the `foreign` **N-ladder** (corroboration only): interleaved 5 passes × {head,m12,m10,sys} × {enc,cap,dec,lower} × `N` = 0/5k/10k/25k/60k, per-row loadavg (§3.3) |
| `results/m13-xrun-load.log` / `m13-tier-load.log` / `m13-ext-load.log` | the `/proc/loadavg` records (15 s apart) for the primary pass, the tier controls and the external pass |
| `results/m13-load-record.md` | the m13 pass record: commit measured, exact commands, the `/proc/loadavg` before/after each run, per-workload status and the A/B/exclusion notes (§1, §3.8, §6.1) |
| `results/m13-tool-targets-notes.md` | the factual per-target status/blocker notes and the environment findings (§4) |
| `results/m12-*` | the **previous** run (2026-10-15 14:00–15:03 UTC): the quiet cross-runtime re-take, the load-biased first pass, the two `--quick` tier controls, the per-target reports, the `foreign` A/B, the diffs, and the `m12-load-record.md`/`m12-tool-targets-notes.md`/`m12-run-route-latency.md`/`m12-foreign-regression.md` records — **the regression baseline for §7** |
| `results/b55-*` / `results/b54-*` | the fixing agent's committed Θ(N³) regression evidence and the batch-54 controlled experiments (kept as history) |
| `results/m11-*` / `results/m10-*` / `results/m9-*` / `results/m6-*` / `results/m5-*` / `results/m4-*` | earlier runs (2026-10-14 / 2026-10-13 / 2026-10-13 / 2026-10-11 / 2026-10-09 / 2026-10-08), kept as earlier regression baselines |

The archived copies are the harness output except for two mechanical normalisations that strip environment-specific absolute paths: the run-host worktree prefix in `runtimes[].program` is replaced with the workspace-relative `erts_rust/target/release/beam_rt`, and a scratch `$TMPDIR` prefix in `notes` is replaced with `$TMPDIR/`. **No metric value is altered.**

## See Also
- `CONTEXT.md` → this crate's node context (workload registry, tool-resolution policy, test strategy).
- `RESULTS.md` §3–§8 → the measured numbers; `results/` → the raw harness JSON behind them.
- `results/m13-load-record.md` → the m13 load/command record; `results/m13-tool-targets-notes.md` → the sys-only target notes.
- `../CONTEXT.md` → the `tools/` node's binding shared conventions (runtime resolution, exit codes, JSON, `out/`).
- `../../CONTEXT.md` §6/§7 → test strategy and the differential gate; `../../../ERTS_RUST_REWRITE_TEST_PROJECTS.md` → the target workloads.
