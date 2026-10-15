# m12 measurement pass — load record & results pointer

Measurement-and-archive record for the **m12** pass (the full cross-runtime run of the six
`Runtimes::Both` workloads under both runtimes, the interpreter-tier control, the sys-only external-target
run, the `foreign` A/B, and the `run`-route latency probes).
Facts only: the exact commands, the `/proc/loadavg` observed before/after each run, the commit measured,
and the sys-vs-rust ratio per workload.
The numbers themselves live in the committed artifacts beside this file:

- `m12-bench-xrun.json` — the full cross-runtime `Report` (schema_version 1, profile `full`, **12/12 rows
  `ok`**). This is the report the cross-runtime ratios below are read from.
- `m12-bench-xrun-interp.json` — the same six workloads, `--runtime rust` only, with
  `BEAM_RT_TIER=interpreter` pinned (the **Rust-vs-Rust tier-effect** control — it contains **no** `sys`
  rows and is NOT a system-ERTS comparison). Profile `quick`.
- `m12-bench-xrun-quick.json` — the **matched-form** control: the identical six workloads, `--runtime
  rust`, the **default** (baseline-JIT) tier, profile `quick` — the *same form* as the interpreter
  control, so the tier table is read off two `--quick` runs.
- `m12-compare-cross-runtime.txt` — `bench compare` for the cross-runtime report (sys vs rust inside one file).
- `m12-compare-tier-effect.txt` — `bench compare m12-bench-xrun-interp.json m12-bench-xrun.json`
  (Rust interpreter `--quick` → Rust default `--full`; **form mismatch**).
- `m12-compare-tier-effect-quick.txt` — the **matched-form** `--quick` ↔ `--quick` tier control (§6).
- `m12-compare-vs-m11-xrun.txt` / `m12-compare-vs-m11-external.txt` — `bench compare` against the m11 record.
- `m12-bench-external.json` — the sys-only external targets in one `--full` report
  (`erlc_compiler`, `elixir_compiler`, `cowboy`, `khepri`; **4/4 `ok`**).
- `m12-bench-plb.json` — `ok` (the version-aware tool resolution picks `dotnet` 9.0.318).
- `m12-bench-emqx.json` — `skipped(emqx-requires-otp-28)`.
- `m12-foreign-ab-head.json` / `m12-foreign-ab-m10.json` — the matched-`N` `foreign` A/B (b54/b55 shape);
  the narrative is `m12-foreign-regression.md`.
- `m12-run-route-latency.md` — the **OPEN** `run`-route boot / blocked-round latency record.

> **Commit measured.** The driver **and** `beam_rt` were both built from the worktree tree
> **`ee3e426db0`** (`--release -p beam_rt -p bench`) immediately before the pass; the m12 measurement
> artifacts were committed as **`bdadd02333`**. The m10-baseline binary used for the A/B is `beam_rt` from
> **`b4593818b3`**. A sibling agent may rebuild `beam_rt`/`beam_engine` concurrently, so re-record the
> commit if the figures are re-run.

> **What is new at m12 vs m11 (the deliverable of this milestone).** (1) The Θ(N³) `foreign` per-slice
> off-heap registration regression is **FIXED** (commits `07a5720639` + `7d84b12b44`) — the corrected
> `foreign` rust/sys band is **38.9×–66.9×** (m11: 60.2×–367.1×); see `m12-foreign-regression.md`.
> (2) The `run` action now drives the full `bootstrap/bin/start` boot script (~2.1 s fixed), so **boot
> completes** — `beam_rt -boot bootstrap/bin/start` **exits 0** (m11 recorded exit 3 / "directive 18 of 22")
> — but that same change leaves an **OPEN** per-blocked-round ~25.5 ms scheduler cost and a Θ(atom-table)
> ETF-decode inflation on the `run` route; see `m12-run-route-latency.md`. (3) `khepri` and `plb` now
> **measure** (`ok`); `emqx` still skips honestly.

> **Host-load caveat (read this first).** The benchmark host is a shared, usually-oversubscribed 14-core
> machine. The m12 cross-runtime pass started at 1-min `/proc/loadavg` **65.28** and the load fell to
> **6.93** during the run; across the whole session the 1-min value ran **3.91 → 65.28**. Every **absolute
> latency/wall/throughput** figure in the artifacts is therefore **inflated** by CPU contention and must
> not be read as a machine-speed constant. The load-robust signals are the **per-op / per-element / per-call
> marginals** (`*_ns_per_op`, `*_ns_per_call`, `*_ns_per_element`) and the **deterministic counts**
> (`*_ops`, `*_elements`, `*_calls`); the per-runtime loop count `N` is fixed per case, so the per-unit
> figures are the comparable ones across the two runtimes.
> **A runtime-major report can be biased by drifting load:** the runner emits all `sys` rows first, then
> all `rust` rows, so if the load drifts during a run the two sides are measured in different conditions.
> The full cross-runtime pass below **fell** from 65.28 to 6.93, i.e. the later (`rust`) side ran under a
> *lighter* load — read the ratios, not the absolute pair.

## 1. Environment

- Commit measured: `ee3e426db0` (runtime+driver tree); artifacts committed as `bdadd02333`.
- Oracle (`sys`): system `erl` **29** (OTP 29.1.1 / erts-17.1, `erlang:system_info(version)` = `17.1`) at
  `/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/lib/erlang/erts-17.1/bin/erl`.
- Rust (`rust`): `erts_rust/target/release/beam_rt` (reports OTP `30`), self-check passing.
- Both runtimes report `supported: true` in the report.
- Host: NixOS 26.11 (Zokor), Linux 7.2.9 (`x86_64`), Intel(R) Core(TM) Ultra 7 356H, `nproc` = 14,
  `MemTotal` = 65 317 432 kB (~62.3 GiB).
- Toolchain: `rustc 1.98.1 (48a229cea 2026-09-01)`, `cargo 1.98.0`; `clang` 21.1.8 on `PATH`.
- `erlc` = the oracle's `erlc`; `elixirc`/`mix` **1.20.4** ("compiled with Erlang/OTP 29").
- `dotnet` **9.0.318** is on `PATH` (`/run/current-system/sw/bin/dotnet`); the store also holds two
  8.0.425 SDKs, which the version-aware resolver rejects for `plb` (min `9`).
- `rebar3` is **not** on `PATH` — resolved from the Nix store by the driver's store scan
  (`rebar3-3.27.1`), so `khepri` runs with no `PATH` prefix.
- `$BENCH_TRY_NIX` and `$BENCH_TOOL_PATH` are **unset**; `$ERTS_ORACLE` / `$BEAM_RT` / `$BEAM_RT_TIER`
  are **unset** for every run except the interpreter control (which exports `BEAM_RT_TIER=interpreter`)
  and the `foreign` A/B (which exports `BEAM_RT`).
- `$HOME` is read-only; `/usr/bin/time` is **absent** (the driver's own timers and `/proc/<pid>`
  `VmHWM` polling are used).
- `erts_rust/target/release/bench list --json`: **all 12 workloads `probe_ready: true`, 0 skip reasons.**

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

# C. FULL sys-only external targets (one report; wall 184 s)
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --full --runtime sys --workloads erlc_compiler,elixir_compiler,cowboy,khepri \
    --out-file erts_rust/tools/bench/results/m12-bench-external.json --verbose

# D. PLB (quick, wall 95 s)   E. EMQX (quick, skips immediately, wall 1 s)
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime sys --workloads plb  --out-file erts_rust/tools/bench/results/m12-bench-plb.json  --verbose
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q --release -p bench -- \
    run --quick --runtime sys --workloads emqx --out-file erts_rust/tools/bench/results/m12-bench-emqx.json --verbose

# F. Step-5 run-route latency probes (direct beam_rt CLI; not a report — m12-run-route-latency.md)
#    HEAD vs the m10 binary vs HEAD-with-empty-boot, send_recv / send_recv_plain / etf / ets / -boot
# G. Step-4 foreign A/B + growth ladder (direct beam_rt run on bench_foreign_driver:enc/1,
#    interleaved 5 passes at N=5000/10000/25000/60000, bracketed by two N=0) and the b54/b55-shape
#    driver A/B pair (m12-foreign-ab-{head,m10}.json) — m12-foreign-regression.md

# H. compare artifacts (the tool diffs the mean)
erts_rust/target/release/bench compare erts_rust/tools/bench/results/m12-bench-xrun.json > .../m12-compare-cross-runtime.txt
erts_rust/target/release/bench compare .../m12-bench-xrun-interp.json .../m12-bench-xrun.json            > .../m12-compare-tier-effect.txt
erts_rust/target/release/bench compare .../m12-bench-xrun-interp.json .../m12-bench-xrun-quick.json      > .../m12-compare-tier-effect-quick.txt
erts_rust/target/release/bench compare .../m11-bench-xrun.json     .../m12-bench-xrun.json             > .../m12-compare-vs-m11-xrun.txt
erts_rust/target/release/bench compare .../m11-bench-external.json .../m12-bench-external.json         > .../m12-compare-vs-m11-external.txt
```

Workloads were run **serially** — this driver was the only benchmark process, one run at a time (A → B →
C → D → E → F → G). The artifacts were written to `results/` with only the two mechanical path
normalisations (the run-host worktree prefix in `runtimes[].program` → `erts_rust/target/release/beam_rt`;
a scratch `$TMPDIR` prefix in `notes` → `$TMPDIR/`). **No metric value was altered.**

## 3. `/proc/loadavg` record (1-min value first, in the order the passes were run)

| pass | before (1-min) | after (1-min) | wall | artifact |
|---|---|---|---|---|
| **A. FULL cross-runtime pass** | `65.28` | `6.93` | 1795 s (29m55s) | `m12-bench-xrun.json` |
| **B. tier controls** (interpreter `--quick`, then matched default `--quick`) | `3.91` | `11.69` | 391 s + 488 s | `m12-bench-xrun-interp.json`, `m12-bench-xrun-quick.json` |
| **C. FULL external-targets pass** | `10.91` | `9.34` | 184 s | `m12-bench-external.json` |
| **D. PLB quick pass** | `9.34` | `7.85` | 95 s | `m12-bench-plb.json` |
| **E. EMQX quick pass** (skips immediately) | `7.85` | `7.54` | 1 s | `m12-bench-emqx.json` |
| **F. run-route latency probes** (§9; not a report) | `10.26` | `8.86` | ≈174 s | `m12-run-route-latency.md` |
| **G. `foreign` A/B + growth ladder** (§3.4; not a report) | `6.72` | `5.73` | ≈249 s | `m12-foreign-regression.md`, `m12-foreign-ab-{head,m10}.json` |

The archived artifacts' own `generated_at_unix_ms` (UTC) bookend the session:
`m12-bench-xrun.json` **14:29:57**, `m12-bench-xrun-interp.json` **14:38:42**, `m12-bench-xrun-quick.json`
**14:46:50**, `m12-bench-external.json` **14:49:58**, `m12-bench-plb.json` **14:51:33**,
`m12-bench-emqx.json` **14:51:34**. Session span (step 1 start 14:00:02 → last probe 15:03:33):
**2026-10-15 14:00–15:03 UTC** (= 2026-10-15 22:00–23:03 +0800). The 1-min load across the session ranged
**3.91 → 65.28**. The step-B batch's two sub-runs share the window 14:30Z → 14:46:50Z (the default-tier
quick run's end logged `loadavg=11.69 6.42 12.36`).

## 4. Per-workload status (full cross-runtime pass, `m12-bench-xrun.json`)

Every (workload × runtime) pair ran: **12/12 rows `ok`, 0 `skipped`, 0 `unsupported`, 0 `failed`.**

| workload | category | sys | rust | cases |
|---|---|---|---|---|
| `baseline`   | startup   | `ok` | `ok` | — |
| `slice`      | compute   | `ok` | `ok` | 9/9 ok on each side |
| `foreign`    | compute   | `ok` | `ok` | 4/4 ok on each side |
| `list_walk`  | compute   | `ok` | `ok` | 1/1 ok on each side |
| `copy_bound` | messaging | `ok` | `ok` | **sys 4/4 ok; rust 3 ok, 0 unsupported (of 4)** — `send_recv_plain` hit the 60 s `RUN_DEADLINE` (§9, `m12-run-route-latency.md`) |
| `gc_bound`   | gc        | `ok` | `ok` | 1/1 ok on each side |

No rust row is fabricated: every rust row carries real `Summary`/scalar metrics (no `null`-only rows, no
`unsupported` classification, no crash/exit-3). Every copy/churn/walk case validated its own round-trip
with a bound-variable pattern on both sides and printed the deterministic constant (recorded in the
report's `observed stdout:` notes), so the two runtimes executed identical work.

The tier controls are **6 rust rows each, all `ok`** (no `sys` rows by construction). The full external
pass (`m12-bench-external.json`) is **4 rows, all `ok`**. `m12-bench-plb.json` is **1 `ok`** row;
`m12-bench-emqx.json` is **1 `skipped`** row (no metric).

## 5. Cross-runtime summary — one line per workload (headline metric, sys vs rust)

`rust/sys` > 1 means the Rust side costs **more** per unit (its ns value is higher). The marginals
(`*_ns_*`) are the load-robust cross-runtime signal; the wall/startup rows are inflated by the load above
and are shown for completeness. **Both sides of every ratio are named** — `sys` is the system ERTS
(OTP 29.1.1 / erts-17.1), `rust` is `beam_rt` at its **default** (baseline-JIT) tier.

| workload | headline cross-runtime metric | sys | rust | rust/sys |
|---|---|---|---|---|
| `baseline`   | `startup_ms` (mean, whole-process) | 120.6697 ms | 2 298.6279 ms | **19.05×** (rust dearer — the fixed boot; §9) |
| `baseline`   | `vm_memory_total_bytes` | 36 211 168 B | 15 892 346 B | **0.439×** (rust 2.28× lower) |
| `slice`      | `case_<name>_wall_ms` (mean, per case) | 132.08–152.71 ms | 2 097.64–2 366.64 ms | **13.7×–17.5×** (rust dearer — whole-process wall, boot-dominated) |
| `foreign`    | `case_<name>_ns_per_call` | 231.62–419.77 ns | 14 919.34–23 429.88 ns | **38.9×–66.9×** (rust dearer; the Θ(N³) fix narrowed it from m11's 60.2×–367.1×) |
| `list_walk`  | `case_list_walk_ns_per_element` | 7.6021 ns | 110.2061 ns | **14.50×** (rust dearer) |
| `copy_bound` | `case_<name>_ns_per_op` | 289.51–1 584.36 ns | 2 883.75–25 632 422.23 ns | **9.96×–16 178.4×** (rust dearer; `send_recv` is the boot/blocked-round defect, §9) |
| `gc_bound`   | `case_churn_ns_per_op` | 4 658.7133 ns | 184 398.8576 ns | **39.58×** (rust dearer) |

### 5.1 Per-case detail

`baseline` (whole-process cost):

| metric | sys | rust | rust/sys |
|---|---|---|---|
| `startup_ms` (mean, n=50) | 120.6697 | 2 298.6279 | 19.05× |
| `startup_ms` (p50 / p99) | 116.3115 / 177.8934 | 2 281.1683 / 2 741.1984 | — |
| `compute_loop_ms` (mean, n=5) | 181.7306 | 2 272.2808 | 12.50× |
| `compute_ops_per_sec` | 5 502 651.77 | 440 086.45 | rust 0.080× |
| `vm_memory_total_bytes` | 36 211 168 | 15 892 346 | rust 2.28× lower |
| `peak_memory_bytes` (RSS) | 134 971 392 (128.7 MiB) | 408 072 192 (389.2 MiB) | rust 3.02× higher |

`slice` (one call per process: `case_<name>_wall_ms` mean; peak RSS p50):

| case | sys wall_ms | rust wall_ms | rust/sys | sys RSS (MiB) | rust RSS (MiB) |
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

`slice` whole-workload `peak_memory_bytes`: sys 72 368 128 (69.0 MiB) → rust 395 730 944 (377.4 MiB).
**Both walls now include the ~2.1 s `run`-route boot on the rust side (§9), so the ratio is boot-dominated,
not an execution-throughput measure; the rust RSS is the boot's resident preloaded-module set.**

`foreign` (real Cowboy + EMQX modules; startup-cancelled `ns_per_call`):

| case | sys ns/call | rust ns/call | rust/sys | sys startup p50 (ms) | rust startup p50 (ms) | sys RSS p50 (MiB) | rust RSS p50 (MiB) |
|---|---|---|---|---|---|---|---|
| `bstr_to_lower` | 419.7725 | 23 429.8831 | 55.82× | 130.61 | 2 094.86 | 66.34 | 438.46 |
| `bstr_capitalize_token` | 361.5712 | 22 901.0619 | 63.34× | 127.54 | 2 072.84 | 66.18 | 441.27 |
| `emqx_base62_encode` | 231.6157 | 15 502.6789 | 66.93× | 135.24 | 2 089.43 | 66.34 | 427.41 |
| `emqx_base62_decode` | 383.9592 | 14 919.3416 | 38.86× | 163.25 | 2 086.47 | 66.38 | 427.98 |

`list_walk` (BIF-free recursion — the per-element dispatch signal):

| metric | sys | rust | rust/sys |
|---|---|---|---|
| `case_list_walk_ns_per_element` | 7.6021 | 110.2061 | 14.50× |
| `case_list_walk_elements` (deterministic) | 100 000 000 | 1 000 000 | (per-runtime `N`, not a ratio) |
| `case_list_walk_peak_rss_bytes` p50 | 65.74 MiB | 376.48 MiB | boot-inflated |

`copy_bound` (term-copy boundary; the `send_recv_plain` rust case **timed out** — no rust metric):

| case | sys ns/op | rust ns/op | rust/sys | ops sys / rust | sys RSS p50 (MiB) | rust RSS p50 (MiB) |
|---|---|---|---|---|---|---|
| `etf_roundtrip` | 738.6382 | 35 369.2113 | **47.88×** | 1 500 000 / 5 000 | 66.32 | 382.61 |
| `ets_copy` | 289.5147 | 2 883.7548 | **9.96×** | 2 000 000 / 100 000 | 66.28 | 450.01 |
| `send_recv` | 1 584.3631 | 25 632 422.2283 | **16 178.4×** | 500 000 / 2 000 | 66.37 | 384.32 |
| `send_recv_plain` | 662.1843 | **— (failed: 60 s deadline)** | — | 1 000 000 / — | 66.43 | — |

`copy_bound` whole-workload `peak_memory_bytes`: sys 71 745 536 (68.4 MiB) → rust 472 117 248 (450.2 MiB).
`etf_roundtrip`'s rust cost is the **Θ(atom-table) `binary_to_term`** boot effect and `send_recv`'s is the
**~25.5 ms blocked-round** boot effect (§9, `m12-run-route-latency.md`) — both OPEN, both `beam_rt`.

`gc_bound`:

| metric | sys | rust | rust/sys |
|---|---|---|---|
| `case_churn_ns_per_op` | 4 658.7133 | 184 398.8576 | 39.58× |
| `case_churn_ops` (deterministic) | 200 000 | 2 000 | (per-runtime `N`) |
| `case_churn_peak_rss_bytes` p50 | 66.54 MiB | 376.53 MiB | boot-inflated |

Deterministic counters (contention-immune), unchanged per case from m11: `case_*_ops` = 1 500 000 /
2 000 000 / 500 000 / 1 000 000 (sys copy_bound), 5 000 / 100 000 / 2 000 (rust copy_bound);
`case_*_calls` = 1 500 000 / 1 500 000 / 3 000 000 / 3 000 000 (sys foreign), 20 000 / 15 000 / 25 000 /
15 000 (rust foreign); `case_list_walk_elements` = 1e8 sys / 1e6 rust; `case_churn_ops` = 200 000 sys /
2 000 rust. All observed stdout matched the `N × 1` / `N × 500500` constant on both sides.

## 6. Tier effect (Rust vs Rust — **not** a system-ERTS comparison)

`rust` default tier / `rust` interpreter tier, read from the **matched-form `--quick`** pair
(`m12-compare-tier-effect-quick.txt`: `a` = interpreter, `b` = default). `> 1` means the default
(baseline-JIT) tier is **cheaper**; `< 1` means the interpreter is cheaper.

| workload | metric | interpreter | default (baseline JIT) | interp/default |
|---|---|---|---|---|
| `baseline` | `startup_ms` | 1 959.5795 | 2 086.4559 | 0.939× (default marginally dearer) |
| `baseline` | `compute_loop_ms` | 1 991.3159 | 2 137.2065 | 0.932× (noise; native `lists:seq/sum` floor) |
| `baseline` | `vm_memory_total_bytes` | 15 895 354 | 15 892 346 | 1.000× (parity) |
| `list_walk` | `ns_per_element` | 235.0513 | **absent** ("marginal below timer resolution") | — (see note) |
| `gc_bound` | `churn` ns/op | 308 835.7985 | **148 964.3747** | **2.07×** (default cheaper) |
| `copy_bound` | `etf_roundtrip` ns/op | 44 170.6459 | 44 687.5942 | 0.988× (parity) |
| `copy_bound` | `ets_copy` ns/op | 2 996.3663 | 3 014.0485 | 0.994× (parity) |
| `copy_bound` | **`send_recv` ns/op** | 25 516 698.9983 | **25 537 116.8888** | **0.999× — parity** |
| `copy_bound` | **`send_recv_plain` ns/op** | **timed out (60 s)** | **timed out (60 s)** | — (no metric either tier) |
| `foreign` | 4 cases ns/call | 21 458–37 786 | 15 449–21 014 | 1.03×–1.80× (default cheaper) |
| `slice` | 9 cases wall_ms | 2 021–2 104 | 2 101–2 165 | 0.95×–1.06× (boot-dominated; no stable tier effect) |

Three facts to carry forward for the tier table:

- **`list_walk`'s `ns_per_element` is absent in the default-tier `--quick` run** — the workload emitted
  the honest note `case list_walk: marginal below timer resolution; no ns_per_element metric` (the loop
  marginal is swamped by the ~2.1 s boot's jitter at only 3 samples). The interpreter quick run *did*
  measure **235.05 ns/element**, and the `--full` cross-runtime default run measured **110.21 ns/element**
  (§5) — so the direction (default cheaper) is established from those two, not from a matched pair.
- **`send_recv_plain` timed out in both tier runs** (the same 60 s `RUN_DEADLINE`, §9), so there is no
  tier pair for it.
- **The m11 interpreter pathology is GONE.** At m11 the interpreter tier was super-linear on the `foreign`
  cross-module cases (up to **4 178 832 ns/call**, a `--full` pass ≈50 min); at m12 the interpreter
  `foreign` rows are **21 458–37 786 ns/call** (1.03×–1.80× the default) and the whole interpreter
  `--quick` control completed in **391 s**. That is consistent with the Θ(N³) registration regression
  having been **fixed** (`m12-foreign-regression.md`) — the m11 interpreter pathology was the *same*
  shared `beam_rt`/`beam_process` defect, not an interpreter-tier defect.

**The requested `copy_bound` rows, with direction:** `send_recv` **25 516 699 → 25 537 117 ns/op
(Δ = +20 418, +0.08 %)** — **parity** (and now dominated by the 25.5 ms blocked-round defect, identical
in both tiers). `send_recv_plain` has **no metric in either tier** (both timed out).

The as-specified artifact `m12-compare-tier-effect.txt` (interpreter `--quick` vs the default `--full`
`m12-bench-xrun.json`) gives the same directions where both tiers have a value: `send_recv`
25 516 699 → 25 632 422 (+0.45 %), `list_walk` −53.11 %, `gc_bound` −40.29 %, `foreign` −19.9 % to
−51.9 %, `copy_bound etf_roundtrip` −19.93 %, and the `sys` rows `only-in-b` by construction.

## 7. Sys-only external targets (`m12-bench-external.json`, `--full`)

| workload | status | headline | sys peak RSS |
|---|---|---|---|
| `erlc_compiler` | `ok` | wall 6 842.18 ms; 8.769 files/s (60 produced over 46 503 LOC) | 134 492 160 B |
| `elixir_compiler` | `ok` | wall 50 304.34 ms; 4.433 files/s (105 attempted / 223 produced); compiler startup 297.64 ms | 211 509 248 B |
| `cowboy` | `ok` | 165 489.21 req/s; p50 0.0779 ms / p99 0.7424 ms (827 888 requests, 0 errors); build 42 527.11 ms | 166 875 136 B |
| `khepri` | `ok` | `rebar3 eunit` wall 77 564.34 ms | 374 722 560 B |
| `plb` (`m12-bench-plb.json`) | `ok` | wall 70 648.46 ms; `test_time_ms` p50 595.13 / p99 3 565.99 (n=10); resolved `dotnet` 9.0.318 | 171 573 248 B |
| `emqx` (`m12-bench-emqx.json`) | `skipped(emqx-requires-otp-28)` | no number | — |

**Every one of these rows is executed by its own system-ERTS toolchain** (`erlc`, `elixirc`, the Cowboy
release on the oracle `erl`, `rebar3`'s OTP-28 Erlang, the `.NET` tool for PLB). **None** is executed by
the Rust runtime — they are **reference** figures for the sys-only surfaces, not Rust-runtime results.

## 8. m12 vs m11 (from `m12-compare-vs-m11-xrun.txt` / `m12-compare-vs-m11-external.txt`)

The tool diffs the **mean**; `a` = m11, `b` = m12. Rust-default-tier rows first (the interesting ones):

| (workload, metric) | m11 | m12 | Δ |
|---|---|---|---|
| `foreign` `emqx_base62_encode` ns/call **rust** | 77 167.3229 | **15 502.6789** | **−79.91 %** (the Θ(N³) fix) |
| `foreign` `bstr_capitalize_token` ns/call **rust** | 28 757.0043 | **22 901.0619** | −20.36 % |
| `foreign` `emqx_base62_decode` ns/call **rust** | 19 306.3883 | **14 919.3416** | −22.72 % |
| `foreign` `bstr_to_lower` ns/call **rust** | 23 751.0676 | **23 429.8831** | −1.35 % |
| `baseline` `startup_ms` **rust** | 15.2122 | **2 298.6279** | **+15 010 %** (the fixed boot; §9) |
| `baseline` `compute_loop_ms` **rust** | 58.6524 | **2 272.2808** | +3 774 % (boot) |
| `baseline` `vm_memory_total_bytes` **rust** | 805 458 | **15 892 346** | +1 873 % (boot's resident set) |
| `copy_bound` `send_recv` ns/op **rust** | 21 522.2389 | **25 632 422.2283** | **+118 997 %** (the blocked-round defect; §9) |
| `copy_bound` `etf_roundtrip` ns/op **rust** | 7 033.3943 | **35 369.2113** | +402.88 % (Θ(atom-table) decode; §9) |
| `copy_bound` `ets_copy` ns/op **rust** | 2 435.0664 | **2 883.7548** | +18.43 % |
| `copy_bound` `send_recv_plain` (all rows) | present | **failed (60 s deadline)** | rust metrics dropped |
| `gc_bound` `churn` ns/op **rust** | 128 524.4542 | **184 398.8576** | +43.47 % |
| `list_walk` ns/element **rust** | 45.9731 | **110.2061** | +139.72 % (boot-inflated startup subtraction) |
| `slice` rust walls | 15.35–37.85 ms | **2 097.64–2 366.64 ms** | the fixed boot (one process per case) |

`sys`-side rows moved only with the host (`baseline` `startup_ms` +6.96 %, `foreign` ns/call +10.2 % to
+21.7 %, `copy_bound` ns/op −1.5 % to +22.2 %, `gc_bound` churn +9.10 %, `list_walk` ns/element +214 %
because the sys startup row 118.70 → 157.33 ms is subtracted from a ~360–918 ms wall) — that
flatness±load is the cross-check that the rust moves are not oracle drift.

External targets (m11 → m12): `erlc_compiler` wall 7 905.65 → 6 842.18 ms (**−13.45 %**, files/s +15.54 %);
`elixir_compiler` wall 47 667.62 → 50 304.34 ms (+5.53 %, files/s −5.24 %); `cowboy` `requests_per_sec`
190 626.72 → 165 489.21 (**−13.19 %**) and mean latency +24.34 %; `khepri` wall 83 402.90 → 77 564.34 ms
(−7.00 %). `plb` (read manually from `m11-bench-plb.json` → `m12-bench-plb.json`, no tool diff archived):
wall 68 318.69 → 70 648.46 ms (**+3.41 %**). All are on the shared host and none is a like-for-like
quiet-host comparison.

**Three real, code-attributable m12 moves** (owners **outside this node**, `beam_rt`):
1. **`foreign` Θ(N³) fix** — the four rust `ns_per_call` rows fell (encode −79.91 %), and the corrected
   cross-runtime band is **38.9×–66.9×** (`m12-foreign-regression.md`).
2. **The `run`-route boot (`85ee33b2f8`)** — +~2.1 s fixed startup on every `run`/`-eval`/`-boot` action
   (`baseline`/`slice` walls, rust RSS ≈377–450 MiB), the Θ(atom-table) `binary_to_term` inflation
   (`etf_roundtrip` +402.88 %), and the ~25.5 ms-per-blocked-round scheduler cost that pushes `send_recv`
   to 25.6 ms/op and `send_recv_plain` past its 60 s deadline — **all OPEN** (§9,
   `m12-run-route-latency.md`).
3. **Boot now completes** — `beam_rt -boot bootstrap/bin/start` **exits 0** (m11: exit 3, "directive 18 of
   22"). The m11 boot-status claim is obsolete (§9).

## 9. Runtime boot status & the OPEN `run`-route defect at this commit

A factual probe, not a workload — full narrative in `results/m12-run-route-latency.md` (**OPEN**):

```
$ erts_rust/target/release/beam_rt -boot bootstrap/bin/start
exit_code=0        wall ≈2.07–2.09 s
Erlang/OTP 30 [erts-17.1] [source] [64-bit]
```

- **Boot completes** (m11 recorded exit 3 on `opcode: run: process blocked (no message available)` at
  directive 18 of 22) — the m11 boot-status fact is **obsolete**.
- **Fixed `run`-route startup ≈2.07–2.13 s** (HEAD) vs 17–19 ms (m10) vs 18–19 ms (HEAD with a synthetic
  empty boot script); `-version` still costs 2 ms.
- **~25.5 ms per blocked round** with ~2 % CPU (`send_recv` N=2 000: wall 53 141 ms, user CPU 1.249 s;
  ≈220× m10) — the booted kernel's `os:set_signal/2` self-pipe keeps `PortRegistry::has_nothing_to_poll()`
  false, so every blocked round sleeps the full `PORT_POLL_SLICE` = 25 ms (`runtime/scheduler.rs`, `port.rs`).
- **Θ(atom-table) ETF decode** — boot grows the atom table ~70 → ~4 100, so `binary_to_term`'s
  `prefix_matches` validation is Θ(table) (the `etf_roundtrip` inflation).
- Root cause owned by `beam_rt` (commit `85ee33b2f8`); already documented at
  `erts_rust/crates/beam_rt/src/runtime/CONTEXT.md:150` and `results/b55-foreign-regression-fix.md`.
  **Not** attributable to any target project.

## 10. Honesty check

- Rust rows: all `ok` with real metrics; the single exception is `copy_bound`'s `send_recv_plain` rust
  case, which is recorded as **failed (60 s deadline)** — no metric fabricated, and it does **not** change
  the row status (the row carries `3 ok, 0 unsupported (of 4)`).
- `sys`/`rust` use identical source programs (embedded modules compiled once with the system `erlc`); each
  run's stdout was checked against the deterministic constant on both sides.
- The `foreign`/`list_walk`/`copy_bound`/`gc_bound` figures are **startup-cancelled** marginals
  `(mean wall@N − mean wall@0) / N`, so per-process startup (including the ~2.1 s rust boot) cancels on
  both sides. The `send_recv_plain` rust case cannot produce one, so it is reported as a failure, not a
  number.
- The tier tables are labelled **Rust-vs-Rust**; every cross-runtime ratio names `sys` (system ERTS) and
  `rust` (`beam_rt` default tier) explicitly. The two tier runs are both `--quick` (matched form); the
  as-specified `m12-compare-tier-effect.txt` is accompanied by `m12-compare-tier-effect-quick.txt`.
- **The loop workloads' rust peak RSS (≈377–450 MiB) is boot-dominated and NOT like-for-like**, and the
  `foreign`/`copy_bound` per-op rows are additionally `N`-dependent on the rust side — neither is a clean
  memory context (`slice` remains the per-process metric in principle, but at m12 its rust RSS is also the
  boot's resident set).
- `emqx` is the only non-`ok` row, and it carries a verified reason and **no** number.
- All committed artifact paths are normalised (no worktree/`$TMPDIR` absolutes).
