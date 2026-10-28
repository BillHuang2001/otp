# bench — measured results (m19)

This is the definitive summary of the **full-suite** measurement session (`m19`, 2026-10-23/24) for the
`erts_rust` benchmark driver — the session every §2/§3 figure and every external-target row comes from.
Every number here comes from the committed raw artifacts under `results/m19-*`; regenerate with the
Runbook (§7) rather than quoting a stale copy.
Raw JSON is authoritative — this file never edits a metric value.

Later, **narrower** sessions also exist and are archived under `results/`; they are additional measurement pairs, never replacements for an m19 §2/§3 row.
`m21` is the GC/allocator workstream's before-record (`results/m21-xrun-gc-*`, `results/m21-xrun-ratios.txt`) plus the two eunit rows (`jsone`, `bbmustache`), which are carried as §9.
`m22` is that workstream's after-record: six matched rounds over `baseline,gc_bound,gc_class,list_walk,slice`, each row read through `results/m22-vs-m21.txt` (its headline diff), `results/m22-xrun-ratios.txt`, `results/m22-load-record.md` and `results/m22-memory.md`.
§2/§3 do **not** restate m21's or m22's numbers.
`m21` and `m22` share the OTP 29.1.2 / erts-17.1.1 oracle, unlike m19's 29.1.1 / erts-17.1, so they are mutually diffable only with each other and never against a row above.
`m23` is the **newest** session and a different kind of record: the startup round's *binary A/B on four sides* (`old` / HEAD / HEAD-with-`BEAM_RT_TIER=baseline` / the oracle), read through `results/m23-boot-wall.json`, `results/m23-slice-wall.json`, `results/m23-bench-{old,new}-r{1,2,3}.json` and the differential sweep `results/m23-sweep-merged.json`; its own section is at the end of this file. Same oracle as m21/m22 (29.1.2 / erts-17.1.1).

## 0. The two sides, named

| side | program | version |
|---|---|---|
| **sys**  | the oracle `erl` on `PATH` (`/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1/lib/erlang/erts-17.1/bin/erl`) | **Erlang/OTP 29.1.1 / erts-17.1** |
| **rust** | `erts_rust/target/release/beam_rt` at its **default execution tier** (`JitMode::Auto` ⇒ the native baseline JIT, compiled **lazily per function**) | reports `otp_release` `30` |

Every "sys vs rust" row below is a comparison of those two programs. There is **no binary A/B in this
session** (no second `beam_rt` build was measured), so nothing here is a rust-vs-rust code comparison;
the last one is m17's, kept as §4 and labelled as such.

Artifacts for this session (all committed under `results/`):
`m19-xab-{rust,sys}-r{1..6}.json` (the **twelve single-side `--quick` passes = six matched rounds**
behind §2/§3), `m19-xab-load.log`, `m19-xab-ratios.txt` (the derived per-round audit table — every figure
in §2/§3 is transcribed from it), `m19-budgetprobe-all.json` + `.txt` and
`m19-budgetprobe-all-cowboy.json` + `.txt` (the latency-budget evaluation passes),
`m19-full-baseline-{rust,sys}.json` (the `--full` startup pair), the fifteen external reps
`m19-ext-{erlc_compiler,elixir_compiler,cowboy,khepri,plb}-r{1,2,3}.json` + `m19-ext-emqx.json` +
`m19-ext-load.log` + `m19-ext-notes.md`, and — from the earlier activation stage —
`m19-target-{plb,khepri,emqx,ext}.json` + `m19-target-notes.md`.

§9's session (`m21`, the two eunit workloads only) is `results/m21-eunit-{sys,rust}-r{1..6}.json` (the
twelve matched single-side `--quick` passes), `m21-eunit-ratios.txt` (the derived audit table every §9
figure is transcribed from), `m21-eunit-load.log`, and the non-timing `--runtime all` budget/ledger pass
`m21-eunit-budgetprobe-all.{json,txt}`.

Two passes in this session are **not timing sources** and are labelled so wherever they appear: the
`--runtime all` budget-evaluation pass (`m19-budgetprobe-all-cowboy.json`; runtime-major, i.e. all `sys`
rows then all `rust` rows, so it samples the two sides minutes apart and carries the documented ordering
bias) exists only so the budget rows that need **both** sides in one report can be evaluated. Every
figure in §2/§3 comes from the twelve matched single-side passes.

## 1. The host and the load record

14 logical cores, shared. `/usr/bin/time` is absent; the harness uses `date +%s%N` + `/proc/<pid>/status`
`VmHWM`/`VmRSS`. **Every matched round and both `--full` baseline passes are pinned to a P-core with
`taskset -c 3`**; the external passes are unpinned so they stay comparable with earlier sessions. Load was
recorded before and after every run:

| run | artifact(s) | pin | `/proc/loadavg` (1-min) across the run |
|---|---|---|---|
| stage 1: matched rounds 1–6 + the stage-1 budget probe (13 passes) | `m19-xab-{rust,sys}-r{1..6}.json`, `m19-budgetprobe-all.json` | `taskset -c 3` | **0.54 → 4.53** (26 readings) |
| stage 2: the 15 external reps + `emqx` + the `--full` baseline pair + the two budget-probe passes (20 passes) | `m19-ext-*.json`, `m19-full-baseline-{rust,sys}.json`, `m19-budgetprobe-all-cowboy.*` | `taskset -c 3` for the `--full` pair; **unpinned** for the external/budget passes | **0.71 → 3.73** (40 readings) |

The `--full` pair inside stage 2 read rust 1.34 → 1.27 and sys 1.27 → 1.22.
Full logs: `m19-xab-load.log`, `m19-ext-load.log`; the per-pass narrative is `m19-ext-notes.md` §0 and the
stage-1 activation record `m19-target-notes.md`. **m19 is a quiet session** — its 1-min load band is
**0.54–4.53** where **m17's was 3–92** (m16 3–76, m15 2–5). Consequences, both of which must be stated
with the numbers:

- the rust side is far more load-sensitive than the sys oracle side, so an **absolute** rust wall time
  moves several-fold between sessions while the **per-round ratio** moves by tens of percent. m19's
  figures are therefore *not* comparable with m17's absolute numbers — and, because no binary A/B was run,
  **the difference between an m19 figure and an m17 figure is not attributable to code**;
- **read the ratios and the deterministic `case_*_ops/calls/elements` counters, never a single absolute
  wall time as a constant.** The host is still shared.

## 2. `baseline` — startup distribution + vm memory (sys vs rust)

Median of the six matched rounds; the per-round ratios are §3.1.

| metric | sys | rust | rust/sys |
|---|---|---|---|
| startup `p50` (ms) | **103.55** | **260.95** | **2.519×** |
| startup `p99` (ms) | **104.21** | **263.90** | **2.513×** |
| compute loop `p50` (ms) | 164.24 | 280.16 | 1.706× |
| `erlang:memory(total)` (`vm_memory_total_bytes`) | 36 196 092 B (36.20 MB) | **7 713 447 B (7.71 MB)** | **0.213×** |
| OS peak RSS (`VmHWM`, `peak_memory_bytes`) | 135 249 920 B (135.2 MB) | 153 862 144 B (153.9 MB) | 1.137× |

`vm_memory_total_bytes` (the VM's own accounting) and `peak_memory_bytes` (OS `VmHWM`) are **separate
metrics and are never conflated**.

Two facts on this row need their caveats stated with them:

- the rust `vm_memory_total_bytes` **0.213×** is an **accounting** figure, **not a footprint**: the runtime
  charges itself only for live capacity, so its own reported total is a fifth of the oracle's while its OS
  **peak RSS is 1.137× the oracle's**. **Do not read the 0.213× as a memory win.** The like-for-like
  footprint reads are `slice`'s peak RSS (§3.2, N=1) and the `baseline` OS-RSS row above;
- the startup ratio (**2.519× p50 / 2.513× p99**) is the session's whole-process comparison, and it is
  reproduced independently by the `--full` pass (§2.1) at 2.531× / 2.539× — agreement to within 0.02×.
  m17 recorded 2.51× / 2.56× at a far heavier load, so the load-robust ratio band is unchanged; the
  absolute figures are what moved with the host, and — with no binary A/B in m19 — nothing here attributes
  a difference to code.

### 2.1 The `--full` startup pass (50 runs per side, `taskset -c 3`)

| metric | rust (`--full`) | sys (`--full`) | rust/sys |
|---|---|---|---|
| `startup_ms` `p50` | **260.985 ms** (n=50, min 258.78, mean 261.26, p90 262.40) | **103.104 ms** (n=50, min 102.14, mean 103.22, p90 103.77) | **2.531×** |
| `startup_ms` `p99` | **269.247 ms** (max 270.27) | **106.025 ms** (max 106.49) | **2.539×** |
| `compute_loop_ms` `p50` | 279.639 ms | 164.362 ms | 1.701× |
| `compute_ops_per_sec` | 3 577 642 ops/s | 6 083 583 ops/s | **0.588×** |
| `vm_memory_total_bytes` | 7 713 447 B | 36 196 168 B | 0.213× |
| OS peak RSS (`VmHWM`) | 153 853 952 B | 135 172 096 B | 1.138× |

The two-sided row of record is still the **matched single-side pair** from stage 1 (p50 2.519×, p99
2.513×); this pass is the milestone-profile confirmation and it agrees with it to within 0.02×.
`compute_ops_per_sec` is the **native floor**, not execution throughput: the fixed loop is dominated by the
natively-served `lists:seq/2` + `lists:sum/1`, so a near-parity-looking figure here says nothing about the
engine (the execution-throughput signals are `list_walk` and `foreign` in §3.1).

## 3. The matched cross-runtime record (the headline)

Twelve `--quick` single-side passes = **six matched rounds**, one side per pass, pinned, one process at a
time, over nine workloads (`baseline`, `slice`, `foreign`, `list_walk`, `copy_bound`, `gc_bound`,
`binary_append`, `file_io`, `gc_class`). The side order **alternates** round by round (this session: r1
rust→sys, r2 sys→rust, r3 rust→sys, r4 rust→sys, r5 sys→rust, r6 rust→sys), so each workload's two sides
are sampled within a couple of minutes of each other. The figure quoted is the **median of the six
rounds**; the **ratio** is the median of the six per-round ratios, and the per-round ratios are shown
because their spread *is* the stability evidence. Ratio > 1 ⇒ rust dearer.

**All 108 (6 rounds × 2 sides × 9 workloads) rows are `status: ok`, and no marginal was withheld in any
pass.** The per-round audit table is `m19-xab-ratios.txt`.

### 3.1 The ratios

| workload | case | sys | rust | **rust/sys** | per-round rust/sys |
|---|---|---|---|---|---|
| `baseline` | startup `p50` | 103.55 ms | 260.95 ms | **2.519×** | 2.517 2.521 2.525 2.507 2.533 2.511 |
| `baseline` | startup `p99` | 104.21 ms | 263.90 ms | **2.513×** | 2.509 2.577 2.518 2.481 2.534 2.345 |
| `baseline` | compute loop `p50` | 164.24 ms | 280.16 ms | 1.706× | 1.690 1.706 1.706 1.717 1.702 1.706 |
| `list_walk` | per element | 2.31 ns | 18.42 ns | **7.991×** | 8.062 7.961 7.982 8.080 7.832 8.001 |
| `copy_bound` | `etf_roundtrip` | 657.59 ns | 1 431.01 ns | **2.178×** | 2.185 2.177 2.132 2.176 2.180 2.200 |
| `copy_bound` | `ets_copy` | 222.72 ns | 1 757.15 ns | **7.909×** | 7.968 7.809 7.811 7.867 7.975 7.951 |
| `copy_bound` | `send_recv` | 1 499.10 ns | 6 272.52 ns | **4.198×** | 4.210 4.199 4.197 4.092 4.226 4.163 |
| `copy_bound` | `send_recv_plain` | 667.69 ns | 2 720.79 ns | **4.079×** | 4.102 4.093 4.027 4.028 4.065 4.114 |
| `gc_bound` | `churn` | 4 244.44 ns | 59 836.62 ns | **14.114×** | 14.105 14.021 14.146 14.084 14.144 14.123 |
| `foreign` | `bstr_to_lower` | 327.30 ns | 3 095.43 ns | **9.485×** | 9.483 9.599 9.359 9.500 9.487 9.183 |
| `foreign` | `bstr_capitalize_token` | 319.54 ns | 4 001.38 ns | **12.558×** | 12.882 12.813 12.282 12.097 12.303 12.830 |
| `foreign` | `emqx_base62_encode` | 212.33 ns | 3 598.16 ns | **16.954×** | 17.377 16.720 17.101 16.625 16.925 16.982 |
| `foreign` | `emqx_base62_decode` | 287.02 ns | 5 841.98 ns | **20.454×** | 19.966 20.635 20.273 19.672 20.641 20.677 |
| `binary_append` | per `expand/1` call | 81 749.74 ns | 1 327 024.36 ns | **16.250×** | 16.391 16.157 16.341 16.309 16.191 16.136 |
| `file_io` | `write_read` | 134 510.44 ns | 98 643.46 ns | **0.733× — RUST FASTER** | 0.732 0.770 0.739 0.733 0.728 0.727 |
| `file_io` | `handle_rw` | 140 944.46 ns | 1 529 350.61 ns | **10.848×** | 10.905 10.884 10.812 10.773 10.906 10.793 |
| `file_io` | `ram_file` | 14 885.26 ns | **absent in m19 — no metric** | — | — (m19 sys-only row) |
| `gc_class` | `send_recv` | 3 256.62 ns | 51 282.00 ns | **15.766×** | 15.925 15.712 15.680 15.781 16.032 15.751 |
| `gc_class` | `ets_copy` | 1 688.33 ns | 14 026.24 ns | **8.329×** | 8.263 8.355 8.177 8.365 8.302 8.363 |
| `gc_class` | `compile` | 16 179 064.25 ns | 333 405 219.75 ns | **20.612×** | 20.488 20.680 20.619 20.606 20.711 20.401 |
| `slice` | 9 cases, one call/process (N=1) | 114.6–115.3 ms | 261.4–264.3 ms | **≈2.283×** | 2.279–2.296 per case |

The `send_recv − send_recv_plain` gap attributes the ETF-codec component *inside* the two-process path
(the two cases differ only in whether the receiver re-encodes the payload): **3 551.7 ns/op on rust**
against **831.4 ns/op on the oracle**.

### 3.1.1 The two-sided reading — do not soften it

**The only per-unit case the Rust runtime wins is `file_io`/`write_read` at 0.733×** (26 % cheaper, and
consistent in all six rounds: 0.727–0.770), and the only other rust-favourable number in the session is
the `vm_memory_total_bytes` **0.213× accounting** figure, which is explicitly *not* a footprint (§2).

**Everything else is dearer.** Worst first:

`gc_class/compile` **20.61×** · `foreign/emqx_base62_decode` **20.45×** · `foreign/emqx_base62_encode`
**16.95×** · `binary_append` **16.25×** · `gc_class/send_recv` **15.77×** · `gc_bound/churn` **14.11×** ·
`foreign/bstr_capitalize_token` **12.56×** · `file_io/handle_rw` **10.85×** ·
`foreign/bstr_to_lower` **9.49×** · `gc_class/ets_copy` **8.33×** · `list_walk` **7.99×** ·
`copy_bound/ets_copy` **7.91×** · `copy_bound/send_recv` **4.20×** · `copy_bound/send_recv_plain`
**4.08×** · startup **2.52× p50 / 2.51× p99** · `slice` **2.28×** · `copy_bound/etf_roundtrip` **2.18×** ·
compute loop **1.71×**.

The one case with **no** rust figure at all is `file_io/ram_file` **in m19** (§8): the rust side produced no
metric, so its 14 885.26 ns/op is a **sys-only** number and no ratio exists. The runtime now serves the case
(`beam_rt`'s native ram file; see `CONTEXT.md`), so a fresh run does carry a rust figure for it.

### 3.2 Memory (like-for-like, `slice`)

`peak_memory_bytes` is the OS `VmHWM` — peak resident set — taken as the median of the six rounds, in
decimal MB.

| workload | sys peak RSS (MB) | rust peak RSS (MB) | rust/sys |
|---|---|---|---|
| `baseline` | 135.2 | 153.9 | 1.137× |
| **`slice` (N=1 — the ONE like-for-like row)** | **68.0** | **124.2** | **1.825×** |
| `list_walk` | 67.7 | 122.9 | 1.815× |
| `copy_bound` | 67.7 | 163.1 | 2.408× |
| `foreign` | 67.8 | 124.5 | 1.836× |
| `gc_bound` | 67.6 | 123.0 | 1.818× |
| `binary_append` | 67.6 | 123.4 | 1.823× |
| `file_io` | 67.8 | 387.0 | **5.718×** |
| `gc_class` | 81.7 | 313.2 | **3.832×** |

**Only `slice` is a memory comparison.** It runs one call per process (`N`=1), so its peak RSS is a
whole-process figure with no loop-length dependence: **68.0 MB (sys) vs 124.2 MB (rust) ⇒ 1.825×**. Every
other rust row is **boot-dominated** — the booted `start.boot` preloaded-module set dwarfs the workload's
own footprint — and `file_io` (5.718×) and `gc_class` (3.832×) additionally pay a **per-op heap retention
proportional to the loop length** (§8). Neither of those is a like-for-like memory comparison, and neither
should be quoted as a memory regression ratio. And, again, rust's `vm_memory_total_bytes` is an
**accounting** figure, not a footprint — never read its 0.213× (§2) as a memory win.

### 3.3 How to read a per-unit figure

Every per-unit figure is a **paired, startup-cancelled marginal**: each round runs the `N`-unit pass and
then the `0`-unit pass **back to back**, and the statistic is the **median of the per-round differences**
(`src/marginal.rs`), withheld — no metric, plus an explicit `marginal below timer resolution` note —
unless it clears `3 × max(1 ms, 1.4826·MAD)`. The sequential all-`N`-then-all-`0` mean schedule this
replaced published `etf_roundtrip` at 136.3× where the interleaved measurement gives ≈2.4×; **do not
revert it.** No marginal was withheld anywhere in m19, so every case in §3.1 is a real measurement.

`N` is **per runtime**, so the report always carries the deterministic, contention-immune companion that
names it (**sys / rust**): `etf_roundtrip_ops` 1 500 000 / 500 000, `ets_copy_ops` 2 000 000 / 250 000,
`send_recv_ops` 500 000 / 90 000, `send_recv_plain_ops` 1 000 000 / 300 000, `churn_ops` 200 000 /
10 000, `list_walk_elements` 100 000 000 / 50 000 000, `foreign` `calls` 1 500 000 / 50 000
(`bstr_to_lower`), 1 500 000 / 40 000 (`bstr_capitalize_token`), 3 000 000 / 60 000
(`emqx_base62_encode`), 3 000 000 / 60 000 (`emqx_base62_decode`), `binary_append_calls` 50 000 / 2 000
(`binary_append_bytes` 3 276 800 000 / 131 072 000 — an **m19** count: the workload is now calibrated at
`n_rust` = 4 000, so a new run reports `binary_append_calls` 4 000 and 262 144 000 bytes, and an m19
`binary_append` figure is comparable only at its own `N`), `file_io` `write_read` 200 000 / 20 000 ops,
`handle_rw` 200 000 / 1 000 ops, `ram_file` 200 000 / — (m19: sys-only), `gc_class` `send_recv` 200 000 /
25 000 ops, `ets_copy` 400 000 / 40 000 ops, `compile` 8 / 2 ops. **Never compare raw `wall_ms` across
two sides with different `N`** — only the per-unit normalization is comparable, and a `gc_class/compile`
figure must always be quoted with its `compile_ops` (§8).

## 4. What changed since the m15-era runtime (the **m17** binary A/B — **not re-run in m19**)

**m19 ran no binary A/B**, so no m19-vs-m17 difference is attributable to code: the two sessions were
taken in different load bands. The last "changed since" evidence therefore remains **m17's** set,
`m17-ab-{new,old}-r{1..5}.json`: the same `bench` binary driving two `beam_rt` builds — `new` = that
session's HEAD `4a92cf96…`, `old` = the m15-era tree `a34ca61332` (md5 `e0f031d2…`) — rust-only
`--quick`, five rounds per side, **alternating order round by round**, pinned to core 3. It is the only
valid way to say "this changed since m15"; it is reproduced here unchanged, as history.

| rust-only metric | m15-era | m17 HEAD | **m17 HEAD/m15-era** | withheld |
|---|---|---|---|---|
| `baseline` startup p50 | 385.1 ms | 407.9 ms | **1.047×** | 0 |
| `baseline` startup p99 | 439.2 ms | 468.3 ms | 1.040× | 0 |
| `baseline` `vm_memory_total` | 13.22 MB | 6.69 MB | **0.506×** (accounting) | 0 |
| `list_walk` ns/element | 31.29 | 34.67 | **1.215×** (regressed) | 0 |
| `copy_bound/etf_roundtrip` | 2 022.4 ns | 1 965.0 ns | 0.972× (parity) | 0 |
| `copy_bound/ets_copy` | 3 421.9 ns | 3 163.4 ns | **0.927×** (faster) | 0 |
| `copy_bound/send_recv` | 9 974.7 ns | 10 645.4 ns | 1.067× | 0 |
| `copy_bound/send_recv_plain` | 2 669.3 ns | 2 529.0 ns | 0.954× | 1 |
| `gc_bound/churn` | 99 816.8 ns | 101 583.4 ns | 1.025× (parity) | 1 |
| `foreign/bstr_to_lower` | 18 834.4 ns | 9 033.8 ns | **0.487×** (faster) | 0 |
| `foreign/bstr_capitalize_token` | 22 718.3 ns | 11 695.7 ns | **0.537×** (faster) | 0 |
| `foreign/emqx_base62_encode` | 25 756.7 ns | 6 517.8 ns | **0.260×** (faster) | 0 |
| `foreign/emqx_base62_decode` | 14 102.8 ns | 11 569.1 ns | **0.804×** (faster) | 1 |

Its memory rows, all five rounds: `copy_bound` **459.3 MB → 158.6 MB (0.35×)**, `foreign`
**323.2 MB → 197.3 MB (0.61×)**, `baseline` 133.2 → 150.4 MB (1.13×), `list_walk`/`gc_bound`
118.3/118.2 → 118.4/118.3 MB (1.00×). `binary_append` is absent from it by design: the m15-era runtime
needs ≈6.3–7.2 s per call and cannot finish a `run` invocation at the calibrated `N`.

That A/B is what closed m16's per-call regressions (`emqx_base62:encode` 1.14× dearer → 0.260×;
`bstr_to_lower`/`bstr_capitalize_token`; `ets_copy` 1.63× → 0.927×; `etf_roundtrip` 1.40× → 0.972×;
startup 1.17× → 1.047×) and it is the reason `list_walk` was flagged as the one metric that went the wrong
way (1.215×). **Nothing in m19 re-tests that verdict** — m19's `list_walk` cross-runtime figure (7.991×)
is a different instrument measuring a different thing (rust against the *oracle*, in a much quieter load
band than m17's 11.80×).

## 5. Rust-vs-Rust: execution-tier effect — **not re-measured in m19**

m19 ran every rust pass at the default tier; there is no `BEAM_RT_TIER=interpreter` control in this
session. The state of the question is unchanged from m16
(`results/m16-bench-xrun-interp.json` against `results/m16-xab-rust-r{1,2,3}.json`, diff
`results/m16-compare-tier-effect-quick.txt`): the default baseline JIT was cheaper than the interpreter on
**every** case it could measure, and `binary_append` is honestly `failed(no-binary-append-run)` at the
interpreter tier because its calibrated `N` exceeds `beam_rt`'s own 60 s `run` deadline. This is a
rust-vs-rust row and is **never** a system-ERTS comparison. Re-run it with the §7 command 5 before
quoting a tier figure.

## 6. External targets (all sys-only — **no rust row exists for any of them**)

Every one of these is `Runtimes::SysOnly`. The Rust runtime cannot boot an OTP release, run
`rebar3`/`mix`, or act as `erlc`/`elixirc`/a Cowboy release host, so each `--runtime sys` report carries
exactly one row: these are **whole-program figures for the target's own toolchain**, not rust-vs-sys
comparisons. Each target was measured **3 times**; the figure below is the **median of the 3 reps**, and
**all 15 reps are `status: ok`** — no rep skipped, none failed. Full per-rep table:
`results/m19-ext-notes.md` §1.

| target | median of 3 reps | instrument |
|---|---|---|
| `erlc_compiler` | **9.831 files/s**, 7 619.8 loc/s, wall 6 102.9 ms, VmHWM 131 629 056 B (reps 9.842 / 9.831 / 9.704) | system `erlc` over **60 files / 46 503 LOC** of OTP stdlib + Elixir `lib/elixir/src` |
| `elixir_compiler` | **3.414 files/s**, 2 118.1 loc/s, compiler startup 289.52 ms, wall 11 422.2 ms, VmHWM 204 984 320 B (reps 3.463 / 3.414 / 3.405) | installed system `elixirc` **1.20.4**; **per-file fallback in all 3 reps** (batch fails on checkout↔installed version skew) |
| `cowboy` | **120 383.9 req/s**, 240 926 requests, **0 errors**, latency p50 **0.0633 ms** / p99 **0.1452 ms** / max 1.379 ms, build 41 764.3 ms, VmHWM 155 131 904 B (reps 124 029.7 / 118 779.4 / 120 383.9) | real `hello_world` relx release, in-process keep-alive load (16 connections) |
| `khepri` | **wall 83 596.0 ms**, **1 501 tests / 0 failures**, suite-internal 74.986 s, VmHWM 364 507 136 B | `rebar3 eunit` in a scratch copy; `rebar3` 3.27.3 via the Nix-store scan |
| `plb` | **wall 68 266.1 ms**, build stage 22 920 ms, **8 timed kernels of 10**, `test_time_ms` min 187.19 / mean 1 469.70 / p50 929.15 / max 3 413.99 ms, `test_mem_bytes` 92 479 488 B, VmHWM 171 618 304 B | Programming-Language-Benchmarks via the `dotnet` tool, `--no-docker`, **elixir only** |
| `emqx` | **`skipped(emqx-requires-otp-28)`** — no number exists to record | gate refuses before any build (see below) |

Rep-to-rep spread is small for every target (≤ ~12 % on `khepri`'s wall time, ≤ ~5 % elsewhere) — this is
the payoff of the quiet 0.7–3.7 load band.

Caveats that travel with each of these rows:

- **`khepri`'s suite does not run under the reported oracle.** `rebar3` is an escript whose shebang names
  `/nix/store/y5db8dvib9fr41l53qrhgzpgx6sbb04b-erlang-29.1.2/bin/escript`, i.e. it ran the suite under
  **OTP 29.1.2**, while the row's `runtimes[]` version describes the driver's `erl` on `PATH`
  (**OTP 29.1.1**). The wall time is real; the version label on that row is the driver's. The 83 596.0 ms
  is a **whole-suite** figure (1 501 tests), not a per-test one.
- **`cowboy`'s throughput is bounded by the fixed concurrency** (`16 / per-request latency`), so read
  req/s **together with** `latency_ms`: p50 0.063 ms and p99 0.145 ms are an order of magnitude tighter
  than m17's contended pass (p99 3.03–3.59 ms, ~38 k req/s) and this row is the honest low-load reference.
  The build git-fetches cowlib+ranch (network).
- **`plb` drives `elixir` only** (`--no-docker`, no container runtime on this host), and **two kernels are
  excluded as untimed**: the tool reported `timeMS = 0` for **`spectral-norm/4000` and
  `spectral-norm/8000`** — its marker for a run it could not time (aborted at the input's declared
  timeout) — so `test_time_ms` is an **8-of-10-kernel aggregate** and `test_mem_bytes` the same 8.
  `dotnet` resolved to **9.0.318** on `PATH`, with **both** 8.0.425 store candidates named as rejected
  (`requires >= 9`).
- **`elixir_compiler` measures the installed `elixirc`, not the checkout**: the in-repo Elixir checkout
  (`1.21.0-dev`) is unbuilt and read-only, so the corpus is staged into `$TMPDIR` and compiled by the
  installed 1.20.4 — hence the version skew and the per-file fallback mode.
- **`emqx` is an honest skip, and the reason is two-layered.** The checkout pins OTP **28.5.0.7-2**
  (`env.sh:3` `export OTP_VSN=28.5.0.7-2`, `.tool-versions:1` `erlang 28.5.0.7-2`) while the host `erl`
  is **OTP 29.1.1 / erts-17.1**; blocker (1) is the `:ezstd` NIF makefile's
  `erl -noshell -s init stop -eval …` idiom, which produces no output under OTP ≥ 29 so `mix compile`
  fails with `No rule to make target '…/env.mk'`. **Blocker (2) is release-independent**: `automake`,
  `libtool` and `cmake` are absent (re-probed on this run; `:jq`/`:rocksdb`/`:quicer` need them) and
  `krb5.h` is only present as a store `-dev` output, off the default include path (`:sasl_auth`). An
  OTP-28 oracle *is* reachable in the store (`erlang-28.5.0.5`) and does evaluate the idiom, but swapping
  it in would only convert this honest skip into a `failed(emqx-build-failed)`. Full ladder:
  `results/m12-emqx-enablement-attempt.md`.

## 7. Runbook (reproduction)

```sh
# 0. build the driver AND the runtime (from the repo root)
erts_rust/scripts/cargo.sh build --release -p beam_rt -p bench --manifest-path erts_rust/Cargo.toml --offline

# 1. the six matched cross-runtime rounds (the §2/§3 headline) — one side per pass,
#    pinned, one process at a time, /proc/loadavg logged before AND after every pass
WL=baseline,slice,foreign,list_walk,copy_bound,gc_bound,binary_append,file_io,gc_class
taskset -c 3 setsid -w erts_rust/target/release/bench run --quick --runtime rust --workloads $WL \
    --out-file erts_rust/tools/bench/results/m19-xab-rust-r1.json --verbose
taskset -c 3 setsid -w erts_rust/target/release/bench run --quick --runtime sys  --workloads $WL \
    --out-file erts_rust/tools/bench/results/m19-xab-sys-r1.json  --verbose
# ... rounds 2–6, reversing the order each round (this session: r1 rust→sys, r2 sys→rust, r3 rust→sys,
#     r4 rust→sys, r5 sys→rust, r6 rust→sys).
# Then derive the audit table exactly as in results/m19-xab-ratios.txt: per round rust_case/sys_case,
# then the median of the six rounds (figures) and the median of the six per-round ratios (ratios).

# 2. the milestone-profile startup pass — 50 runs per side, pinned
taskset -c 3 erts_rust/target/release/bench run --full --runtime rust --workloads baseline \
    --out-file erts_rust/tools/bench/results/m19-full-baseline-rust.json
taskset -c 3 erts_rust/target/release/bench run --full --runtime sys  --workloads baseline \
    --out-file erts_rust/tools/bench/results/m19-full-baseline-sys.json

# 3. three reps of each external target (sys-only, unpinned so they stay comparable with earlier sessions),
#    one at a time, load logged before/after every pass
for id in erlc_compiler elixir_compiler cowboy khepri plb; do
  for n in 1 2 3; do
    erts_rust/target/release/bench run --quick --runtime sys --workloads "$id" \
      --out-file erts_rust/tools/bench/results/m19-ext-$id-r$n.json
  done
done
# emqx, once (expected: skipped(emqx-requires-otp-28))
erts_rust/target/release/bench run --quick --runtime sys --workloads emqx \
    --out-file erts_rust/tools/bench/results/m19-ext-emqx.json

# 4. the budget-evaluation pass — the ONLY reason to use --runtime all; it is NOT a timing source
erts_rust/target/release/bench run --quick --runtime all \
    --workloads baseline,list_walk,copy_bound,gc_bound,file_io,gc_class,cowboy \
    --out-file erts_rust/tools/bench/results/m19-budgetprobe-all-cowboy.json
# archive its verbatim verdict block:
#   sed -n '/^latency budgets/,$p' <log> > results/m19-budgetprobe-all-cowboy.txt
# `--fail-on-budget` turns an `Exceeded` into exit 1 (nothing exceeded in m19, so it never fired).

# 5. the tier control (rust-only; NOT part of m19 — the last one is m16's)
BEAM_RT_TIER=interpreter taskset -c 3 setsid -w erts_rust/target/release/bench run --quick --runtime rust \
    --workloads $WL --out-file erts_rust/tools/bench/results/<session>-bench-xrun-interp.json

# 6. diffs
erts_rust/target/release/bench compare results/m19-xab-sys-r1.json results/m19-xab-rust-r1.json

# 7. the two eunit workloads (m21, §9) — same matched-round shape, two ids, ~4 s per pass
for r in 1 2 3 4 5 6; do
  # alternate the side order round by round, exactly as in step 1
  taskset -c 3 erts_rust/target/release/bench run --quick --runtime sys  --workloads jsone,bbmustache \
      --out-file erts_rust/tools/bench/results/m21-eunit-sys-r$r.json
  taskset -c 3 erts_rust/target/release/bench run --quick --runtime rust --workloads jsone,bbmustache \
      --out-file erts_rust/tools/bench/results/m21-eunit-rust-r$r.json
done
# then the budget/ledger pass (both sides in one report; NOT a timing source) and its verbatim block:
taskset -c 3 erts_rust/target/release/bench run --quick --runtime all --workloads jsone,bbmustache \
    --out-file erts_rust/tools/bench/results/m21-eunit-budgetprobe-all.json
```

The standing warnings, unchanged:

- **never use `--runtime all` as a timing source** — it is runtime-major (all `sys` rows, then all `rust`
  rows), so it samples the two sides minutes apart and carries the documented ordering bias (m16 measured
  it at up to 18 % on `list_walk`). Its one legitimate job is putting both sides in one report so the
  ratio-shaped budget rows resolve;
- **record `/proc/loadavg` before and after every run** (`m19-*-load.log` shape) and **measure one process
  at a time**; pin every matched pass and every A/B with `taskset -c 3`;
- **a "changed since" claim needs a binary A/B** (alternating side order, pinned, ≥5 rounds). There was
  none in m19, so none is made.

Archive `out/` copies into `results/` with **only** the mechanical normalisations
(`runtimes[].program` worktree prefix → `erts_rust/target/release/beam_rt`; a scratch `$TMPDIR` prefix in
`notes` → `$TMPDIR/`; a build-run worktree prefix stripped from the `gc_class` compile-source note).
**Never edit a metric value.**

## 8. Status and still-open items

- **m19 rows:** six matched rounds × 2 sides × 9 workloads = **108/108 `ok`**, **no withheld marginal**
  anywhere, **no `failed` row in the session**; external **15/15 reps `ok`** across the five targets plus
  the single honest `emqx` skip; the `--full` baseline pair `ok` on both sides; the latency-budget verdict
  **11 met / 0 exceeded / 0 not measured**. `binary_append` produced a real rust marginal at its
  calibrated `N`=2 000.
- **The two-sided reading stands and must not be softened:** `file_io/write_read` **0.733×** (rust
  faster, all six rounds) is the *only* per-unit case rust wins; the `vm_memory_total_bytes` **0.213×** is
  an accounting figure, not a footprint; **every other measured case is dearer**, from
  `gc_class/compile` 20.61× down to the compute loop 1.71× (full ranking in §3.1.1).
- **`file_io/ram_file` carried no rust metric in m19.** The rust side recorded none then because the runtime
  exec'd a `ram_file_drv` that was not present; the row's own note is that honest reason, quoted verbatim:
  `case ram_file: absent on rust (case ram_file (rust): exited 1: …
  exec: ram_file_drv: not found … did not terminate within 60000 ms) — no metric recorded`. So its
  14 885.26 ns/op is a **sys-only** figure and no rust/sys ratio exists for it **in m19**. `beam_rt` now
  serves the case natively (`crates/beam_io/src/drivers/ram_file.rs`), so this caveat is m19-specific and a
  fresh run reports all three cases.
- **`gc_class/compile` is a runtime-execution comparison, not a compiler measurement.** Both sides resolve
  `code:which(compile)` to the **system OTP 29.1.1** compiler
  (`…/erlang-29.1.1/lib/erlang/lib/compiler-10.0.6/ebin/compile.beam`), so the 20.61× measures how fast
  each *runtime* executes those compiles — never quote it as an in-tree/`bootstrap`-compiler result. The
  cold/warm mix also differs by side: `compile_ops` is **2 (rust) vs 8 (sys)**, so the figure is only
  meaningful with its `N` attached.
- **The latency budgets are met, with headroom, and the limits were not tuned to the measurements.** The
  table and every rationale live in `src/budget.rs`; the verdict block is archived verbatim as
  `results/m19-budgetprobe-all-cowboy.txt`. From that pass: `cowboy[sys] latency_ms p99 = 0.16 ms`
  (limit ≤ 5), `baseline[rust] startup p50 2.25× / p99 2.11×` (limit ≤ 6×),
  `list_walk 18.50 ns/element` (≤ 2 000), `copy_bound/send_recv 6 209.39 ns/op` (≤ 20 000),
  `gc_bound/churn 60 567.24 ns/op` (≤ 200 000), `file_io/write_read 97 677.02 ns/op` (≤ 200 000),
  `gc_class/send_recv 50 748.51 ns/op` (≤ 200 000). These are **gross-regression alarms, not SLOs and not
  parity targets** — read the rationale in `src/budget.rs` before treating a limit as a goal. (The earlier
  pass, `m19-budgetprobe-all.txt`, read 10 met / 0 exceeded / 1 not measured because it omitted `cowboy`;
  the `-cowboy` pair is the one of record.)
- **No binary A/B in m19 ⇒ no m19-vs-m17 difference is attributable to code.** The sessions ran in
  different load bands (m19 0.54–4.53, m17 3–92). The last "changed since" evidence is m17's A/B (§4),
  and §5's tier control is m16's — both are labelled as history.
- **The memory rows need their caveats stated with them:** `slice` (1.825×, N=1) is the only like-for-like
  per-process read; the loop workloads' rust peak RSS is **boot-dominated**, and `file_io` (5.718×) /
  `gc_class` (3.832×) additionally carry a **per-op heap retention proportional to loop length**. Rust's
  `vm_memory_total_bytes` (0.213×) is an **accounting** figure, never a footprint.
- **`foreign`'s and `binary_append`'s `case_<name>_ns_per_call` remain `N`-dependent on rust** (a
  common-mode per-op heap retention), so those per-call figures are comparable **only across runs at the
  same `N`**; the counts are the ones in §3.3. `binary_append`'s `n_rust` is now **4 000**, re-derived from
  the merged runtime's measured ≈204 µs/call (deliberately the conservative figure: the driver's own
  quiet-window paired marginal reads ≈157.5 µs/call, i.e. ≈630 ms of loop work, 1.26× the floor) so the
  loop does ≈0.82 s of work and clears the ≥0.5 s rule —
  the row no longer sits at the runtime's deadline cap, though `beam_rt`'s `RUN_DEADLINE = 60 s`
  (`crates/beam_rt/src/runtime/scheduler.rs:90`) remains **non-overridable** and the maintenance rule still
  applies: re-derive `n_rust` from the measured cost before changing anything there.
- **No self-hosted build (contract A.9).** `erlc_compiler` and `elixir_compiler` are `Runtimes::SysOnly`
  and shell out to the system `erlc`/`elixirc`; nothing in the driver runs a compiler **under** `beam_rt`.
- **Honesty caveats that travel with every figure here:** the host is shared, so absolute wall times are
  host-load-sensitive even in m19's quiet band and only a binary A/B supports a "changed since" claim;
  cross-runtime ratios come from the matched rounds (§3) and are the load-robust reading; every per-unit
  case but `file_io/write_read` is dearer on rust; `foreign`/`binary_append` per-call figures are
  `N`-sensitive; `baseline`'s compute loop is native-floor dominated; the external targets are whole-program
  figures for their own toolchains, sys-only.

**History:** `m17-*` (its six matched rounds at 1-min load 3–92, its binary A/B, its external rows) and
`m16-*` (the tier control, the low-load external and `plb`/`emqx` rows) remain committed as the regression
history, as do `m15-*`, `m14-*` and `m13-*`…`m4-*`/`b54-*`/`b55-*`. Every earlier session's per-unit
figures are **superseded by the current record** (a different load band and, for some cases, a different
`N`; before m15, a different estimator) — never quote or diff them as current. Their external-target rows,
load records and `baseline`/`slice` figures remain useful provenance.

## 9. `jsone` + `bbmustache` — the two pure-Erlang eunit workloads (m21, new rows only)

Two real-world pure-Erlang projects, each driven as its **own application test suite**: `jsone` (pure-Erlang
JSON encode/decode) and `bbmustache` (pure-Erlang Mustache rendering). Both are staged read-only into a
scratch copy, compiled once with the system `erlc`, then run as `eunit:test([…])` under **both** runtimes
with the **identical argv**
(`-noshell -pa <ebin> -eval 'case eunit:test([<mods>]) of ok -> halt(0); _ -> halt(1) end.'`) — so the
suite, the OTP modules it calls and the work it does are the same on both sides, and the only variable is
the runtime.

**The suite result is a measured, verified agreement, not an assumption.** Every sampled run on every side
printed the same summary line, and the driver refuses to publish a count that differs from the intended one
(`90` / `79`) or from the other side's:

| workload | eunit summary line, **both** sides, every run | `tests_passed` |
|---|---|---|
| `jsone` | `  All 90 tests passed.` | **90** |
| `bbmustache` | `  All 79 tests passed.` | **79** |

**The two sides, named (m21).** The oracle here is a **different OTP build than m19's 29.1.1**, so these
rows are a self-consistent m21 pair — do not diff them against an m19 row and call the difference code:

| side | program | version |
|---|---|---|
| **sys** | the oracle `erl` on `PATH` (`/nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erl`) | **Erlang/OTP 29.1.2 / erts-17.1.1** |
| **rust** | `erts_rust/target/release/beam_rt`, default execution tier (native baseline JIT, lazily per function) | reports `otp_release` `30` |

**Medians over the six matched single-side rounds** (transcribed from `results/m21-eunit-ratios.txt`; ratio
= **median of the six per-round ratios**, not the ratio of medians — both are printed in that file):

| workload | metric | sys | rust | rust/sys |
|---|---|---|---|---|
| `jsone` | `wall_ms` p50 | **408.04 ms** | **506.20 ms** | **1.2216×** |
| `jsone` | `wall_ms` p99 | 434.39 ms | 513.53 ms | 1.2146× |
| `jsone` | `compile_ms` (one `erlc` stage) | 494.41 | 502.84 | 0.9941 (sanity check only) |
| `jsone` | `tests_passed` | **90** | **90** | 1.0000 |
| `jsone` | OS peak RSS (`VmHWM`) | 71 907 328 B (71.91 MB) | 189 990 912 B (189.99 MB) | **2.6417×** |
| `bbmustache` | `wall_ms` p50 | **359.93 ms** | **380.08 ms** | **1.0652×** |
| `bbmustache` | `wall_ms` p99 | 399.09 ms | 397.58 ms | 1.0254× |
| `bbmustache` | `compile_ms` (one `erlc` stage) | 296.49 | 294.72 | 0.9983 (sanity check only) |
| `bbmustache` | `tests_passed` | **79** | **79** | 1.0000 |
| `bbmustache` | OS peak RSS (`VmHWM`) | 64 380 928 B (64.38 MB) | 111 181 824 B (111.18 MB) | **1.7271×** |

**Per-round stability** (the six per-round ratios, the load evidence behind each median):
`jsone` p50 **1.1830 1.2141 1.2208 1.2224 1.2456 1.2622** (spread ±3 % of the median — the tightest
cross-runtime row in the suite), p99 1.1265–1.2656; `bbmustache` p50 **1.0325 1.0491 1.0511 1.0794 1.0853
1.1480**, p99 0.9303–1.1736 (a 3-sample quantile, so the tail row is the noisier of the two).

**How to read these rows — the caveats travel with the numbers:**

- **`wall_ms` is a whole-program time, not a per-unit figure.** One sample is one VM process running the
  entire suite (≈0.35–0.5 s, of which a meaningful part is boot), three samples per pass (`--quick`;
  `--full` takes ten). So the ratio is *dominated by suite execution but not isolated to it*, and it is
  **not** comparable with the startup-cancelled per-unit rows of §3 (`ns/element`, `ns/op`).
- **`tests_passed` is the contention-immune companion** — the deterministic counter for these workloads,
  exactly as `case_*_ops`/`_calls`/`_elements` are for the loop workloads. It is `90`/`79` on **both** sides
  of **all twelve** passes and of the `--runtime all` pass: `24/24` rows `ok`, nothing withheld, no `failed`
  row, no `skipped` row (the runtime **is** supported for both projects — no `runtime-unsupported` skip and
  no oracle-only figure).
- **`compile_ms` is the same external `erlc` on both sides**, so its ratio sits at ~1.0 by construction; it
  is recorded to prove the stage is bounded and to keep it out of `wall_ms`, never as a runtime comparison.
  jsone's `erlc` emits a `nowarn_deprecated_catch` deprecation warning at **compile** time (exit 0); the row
  quotes it in `notes` and does not treat it as a failure.
- **The rust peak-RSS gap (2.64× `jsone`, 1.73× `bbmustache`) is a real whole-process footprint** and is
  reported without softening — but it is a boot/load-dominated peak of a per-process measurement, so its
  attribution belongs to the runtime (`crates/beam_rt/PERF.md`), not to this driver.
- **The load record is not quiet:** 26 `/proc/loadavg` readings over the 13 passes span a 1-min load of
  **9.59 → 14.62** (m19's band was 0.54–4.53). Read the ratios and `tests_passed`; never quote an absolute
  wall time from this session as a constant.
- **No budget row was added for either workload, deliberately.** The one equality-shaped quantity here
  (`tests_passed` must match on both sides) does not fit the `AtMost`/`RatioAtMost` budget vocabulary, and
  the Compute class already carries its first-pass regression alarm (`list_walk`, §8). So
  `m21-eunit-budgetprobe-all.txt` honestly reads **0 met / 0 exceeded / 11 not measured**: the pass selected
  only these two workloads, and no existing limit was re-tuned.
- **Harness honesty for these two ids:** a missing checkout ⇒ `skipped(repo-missing)` (also what a bad
  `--repo`/`$BENCH_JSONE_REPO`/`$BENCH_BBMUSTACHE_REPO` override produces), `erlc` absent ⇒
  `skipped(erlc-absent)`, a checkout that will not compile ⇒ `failed(module-compile-failed)` with the
  compiler's stderr quoted — never a skip; a count that disagrees with the intended one or with the other
  side ⇒ `failed(...)` with **no metric published**.

## m23 — startup round: phase-scoped tier + shared per-image label maps + loader cost pass (2026-10-29)

A **narrow, single-topic** session: it restates no §2/§3 row and supersedes none.
It measures **what a boot costs**, on four sides, and the load-bearing figures are the **paired per-round
medians** — never an absolute.

**Provenance.** Host as §1 (14 logical cores, shared); every sample is one process at a time, pinned
`taskset -c 3`. Four sides: `old` = the **pre-round `beam_rt` build**, `new` = **HEAD** (the phase-scoped
tier rule), `newpin` = the **same HEAD binary run with `BEAM_RT_TIER=baseline`**, `sys` = the system oracle
`erl` (**OTP 29.1.2 / erts-17.1.1** — m21/m22's oracle, not m19's 29.1.1). 1-min `/proc/loadavg`: the boot
A/B is **21 rounds at 68.4–74.1**, the `slice` run **15 rounds at 66.0–67.7**, recorded before and after
every round, with the side order reshuffled per round.
Two instruments: (1) the four-side spawn A/B — wall from `perf_counter` around `fork`/`wait4`, **CPU =
`ru_utime + ru_stime`** (the load-robust column), peak RSS from `ru_maxrss`; (2) the driver's own
non-regression rounds, `bench run --quick --runtime rust --workloads slice,list_walk,copy_bound,baseline`,
three alternating rounds per side — plus `perf stat` for instructions/cycles/task-clock and the two-point
steady-state slope.

**What is NOT a valid figure here** (the caveats travel with the numbers):

- **wall at 1-min load > 20.** Three archived pairs are the round's *contended* counterparts, kept only as
  evidence and never as a timing source: `m23-boot-ab.json` and `m23-slice-ab.json` (load 454–490) and the
  first A/B, `m23-tier-{old,new}-r{1..5}.json` (`m23-tier-loads.txt`, load 73–161). Every boot/`slice` cell
  below comes from that quiet window (68.4–74.1 for the boot rounds, 66.0–67.7 for `slice`).
- **the `ns_per_*` / per-call rows.** `case_*_ns_per_*` is `N`-dependent on the rust side and the host is
  shared (§3.3 applies unchanged); the `peak_memory_bytes` rows below are boot-dominated except `slice`
  (N=1). Read the paired ratios and the deterministic `case_*` counters.
- **any absolute wall time as a constant.**

### Boot — 21 interleaved rounds, 1-min load 68.4–74.1 (`m23-boot-wall.json`)

| side | wall p50 (ms) | wall p95 (ms) | CPU p50 `utime+stime` (ms) | peak RSS p50 (MiB) |
|---|---|---|---|---|
| `old` — pre-round build | 201.5 | 322.8 | 156.6 | 58.93 |
| **HEAD — phase rule** | **166.5** | **292.1** | **127.3** | **49.62** |
| HEAD + `BEAM_RT_TIER=baseline` | 194.9 | 317.3 | 142.1 | 57.01 |
| system oracle (`erl` 29.1.2 / erts-17.1.1) | 175.3 | 425.4 | 136.4 | 59.40 |

Paired per-round medians (the reading of record — one ratio per round, median over the 21):

| pair | wall | CPU | RSS |
|---|---|---|---|
| `old` / HEAD | **1.267** | **1.242** | **1.188** |
| HEAD / `BEAM_RT_TIER=baseline` | **0.848** | **0.904** | **0.870** |
| HEAD / oracle | **0.923** | **0.930** | **0.836** |

### `slice` module run — 15 rounds, 1-min load 66–68 (`m23-slice-wall.json`)

| side | wall p50 (ms) | CPU p50 (ms) | peak RSS p50 (MiB) |
|---|---|---|---|
| `old` — pre-round build | 196.0 | 158.7 | 58.91 |
| **HEAD — phase rule** | **152.3** | **126.8** | **49.70** |
| HEAD + `BEAM_RT_TIER=baseline` | 196.9 | 143.2 | 56.98 |
| system oracle | 196.3 | 151.1 | 64.55 |

Paired per-round medians, HEAD / `BEAM_RT_TIER=baseline`: wall **0.864**, CPU **0.894**, RSS **0.872**.

### Instructions vs cycles vs task-clock, one boot (`perf stat`, 3 reps, ranges)

| side | instructions | cycles | task-clock (ms) |
|---|---|---|---|
| `old` — pre-round build | 1.089–1.091 G | 501–505 M | 160–162 |
| HEAD — phase rule | 1.193–1.199 G | 387–391 M | **123.5–125.2** |
| HEAD + `BEAM_RT_TIER=baseline` | 0.953–0.961 G | 423–444 M | 140.8–144.1 |
| HEAD + `BEAM_RT_TIER=interpreter` | 1.187–1.195 G | 360–376 M | 125.4–128.0 |
| system oracle | 0.791 G | 366–469 M | 137–185 |

**The conclusion this table supports — instructions are not time.** The interpreter boot retires *more*
instructions than the pre-round build (1.187–1.195 G vs 1.089–1.091 G) yet spends *fewer* cycles
(360–376 M vs 501–505 M) and *less* CPU (125.4–128.0 ms vs 160–162 ms), because the baseline JIT's compile
work is fault/mmap/i-cache bound rather than instruction bound: the JIT-retired instructions are cheap, the
compile path's cycles are not. An instructions-only reading of this round would rank the two boots the
wrong way round, so every claim here is stated in cycles/CPU/wall.

### Steady state — instructions per walked element (two-point slope, 1 M vs 5 M walked elements, `list_walk` shape)

| side | instructions / element |
|---|---|
| `old` — pre-round build | 350.2 |
| **HEAD — phase rule** | **352.2** |
| HEAD + `BEAM_RT_TIER=baseline` | 352.4 |
| HEAD + `BEAM_RT_TIER=interpreter` | 2 543 |

Parity within **0.6 %** across the three JIT sides: the tier rule costs nothing per element in steady
state, and the tier column is what proves the **one-way post-boot upgrade lands** — a process that starts
on the interpreter and is upgraded to the baseline tier walks elements at the baseline tier's cost, not at
the interpreter's (2 543).

### Peak RSS + deterministic counters — `bench run --runtime rust`, median of 3 alternating rounds

| workload | `old` peak RSS (MiB) | HEAD peak RSS (MiB) | HEAD/`old` |
|---|---|---|---|
| `slice` | 59.28 | 50.10 | **0.845×** |
| `list_walk` | 59.02 | 49.81 | **0.844×** |
| `copy_bound` | 60.16 | 51.23 | **0.851×** |
| `baseline` (compute) | 89.64 | 80.21 | **0.895×** |

The counters are **identical on both sides in all six passes** — `case_list_walk_elements` 50 000 000,
`case_etf_roundtrip_ops` 500 000, `case_ets_copy_ops` 250 000, `case_send_recv_ops` 90 000,
`case_send_recv_plain_ops` 300 000 — so the RSS difference is not a difference in work done.

### Retired instructions per boot across the round (the deterministic, load-robust headline)

| state | instructions / boot |
|---|---|
| pre-round | 1.088 G |
| after the phase-scoped tier (interpreting the boot instead of compiling it) | 1.300 G |
| after loader pass 1 | 1.186 G (−8.7 % on its own predecessor) |
| after loader pass 2 | 1.1196 G (−6.15 % on its own predecessor) |
| — with the tier held constant across the same boots | **1.088 G → 0.957 G** |

Read the row **with** the cycles table above: the phase-scoped tier raises retired instructions (it
interprets the boot instead of compiling it) while lowering cycles, CPU and wall, and the two loader passes
then take the count back down below the pre-round figure.

### Differential gate at this state (`m23-sweep-merged.json`, folded by `merge`, full corpus)

`summary: total=3469 agree=3465 mismatch=0 unsupported=1 skipped=0 oracle_drift=3`;
ratchet `allowed=1 unauthorized=0 stale=0`; drift `allowed=3 unauthorized=0 stale=0` ⇒ **gate PASS**.
The single `unsupported` is the recorded `wire/new_fun_ext_encode` gap; the three `oracle_drift` cases are
the version goldens the host oracle answers with its `erts-17.1.1` patch string against this tree's frozen
`17.1`.

### Artifacts

`results/m23-boot-{ab,wall}.json`, `results/m23-slice-{ab,wall}.json`, the six non-regression rounds
`results/m23-bench-{old,new}-r{1,2,3}.json`, the first (contended) A/B
`results/m23-tier-{old,new}-r{1..5}.json` + `results/m23-tier-loads.txt`, the retained `perf stat` output
of the steady-state point `results/m23-lw-perf-{old,new,newpin}.txt`, and the differential sweep
`results/m23-sweep-merged.json` + `results/m23-sweep-merge.log`.
The six `m23-bench-*` and ten `m23-tier-*` reports carry the directory's documented path normalization
(`runtimes[].program` → `erts_rust/target/release/beam_rt`, scratch prefixes → `$TMPDIR/…`); no metric value
was touched. The sweep report is **not** normalized, so its `rust.program` still names the volatile path of
this round's HEAD binary (`/tmp/genesis/ab_final_head_beam_rt` — the same build as every `new` cell above,
which no longer exists).
Two rows above are transcribed from the round's established results rather than from an archived raw file,
and are flagged here as such: the per-boot instructions/cycles/task-clock table and the 1 M-element point of
the steady-state slope. The retained `perf stat` output is the slope's **5 M-element** point only
(`m23-lw-perf-{old,new,newpin}.txt`, one `perf stat -x,` record per side at 5 000 walks × `LEN` 1 000, its
`out` checksum 2 502 500 000).
