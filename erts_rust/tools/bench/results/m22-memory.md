# m22 memory rows — OS peak RSS, both sides named (`rust` vs `sys`)

Current record: **m22** (six matched rounds, 2026-10-27 18:31–18:36 UTC). `rust` = `erts_rust/target/release/beam_rt`
at its default (native baseline-JIT) tier, `version=30`; `sys` = the system `erl` on `PATH`,
**Erlang/OTP 29.1.2 / erts-17.1.1**. Both programs are named in every archived JSON (`runtimes[]`).
A `rust/sys` ratio < 1 means the Rust runtime used **less** memory. The `sys` column is the oracle, never half of a ratio.

## Method (two independent instruments, never conflated)
1. **Workload rows** — the driver's `VmHWM` poller (`/proc/<pid>/status`, pure `std::fs`, sampled while the child runs),
   reported as the row's `peak_memory_bytes` field or its `case_<name>_peak_rss_bytes` metric. Figure quoted is the
   **median of the six per-round values**; the per-round ratio list is in `m22-xrun-ratios.txt` §D.
2. **Boot row** — an *independent* confirmation, outside the driver: `wait4` -> `ru_maxrss` (the same kernel high-water
   counter `VmHWM` reads), one fresh process per sample, **five samples per command per side**, side order alternating
   sample by sample, `beam_rt -noshell -eval 'halt().'` and `beam_rt -noshell -s init stop`, with the oracle given the
   identical argv. Raw log: the table below (5 values shown, not just the median), `/proc/loadavg` 9.11–13.34 over the 20 samples.
   `ru_maxrss` is reported in KiB by the kernel; bytes are KiB x 1024 and MiB are bytes / 1048576.

**Do not read `vm_memory_total_bytes` (the VM's own `erlang:memory(total)` accounting) as a footprint:** it is a
separate metric, and the Rust runtime charges itself only for live capacity (m22: rust 7.25 MB vs sys 36.19 MB while the
same run's OS peak RSS is rust 100.3 MB vs sys 135.2 MB). The like-for-like footprint reads are the `peak_memory_bytes`
field, the `case_*_peak_rss_bytes` metrics and the `slice` rows below.

## 1. Per-workload peak RSS (median of the six m22 rounds, bytes)

| workload / metric | rust (m22) | sys (m22) | **rust/sys** | rust (m21) | sys (m21) | rust/sys (m21) |
|---|---|---|---|---|---|---|
| `baseline` — `peak_memory_bytes` (row field of the compute runs) | **100,311,040** | **135,231,488** | **0.742** | 128,106,496 | 135,168,000 | 0.947 |
| `gc_bound` — `case_churn_peak_rss_bytes` | **68,440,064** | **67,647,488** | **1.012** | 96,722,944 | 67,661,824 | 1.429 |
| `list_walk` — `case_list_walk_peak_rss_bytes` | **68,153,344** | **67,381,248** | **1.012** | 96,694,272 | 67,600,384 | 1.430 |
| `gc_class` — `case_send_recv_peak_rss_bytes` | **68,657,152** | **67,698,688** | **1.014** | 193,597,440 | 67,624,960 | 2.862 |
| `gc_class` — `case_ets_copy_peak_rss_bytes` | **68,431,872** | **67,633,152** | **1.011** | 97,177,600 | 67,620,864 | 1.437 |
| `gc_class` — `case_compile_peak_rss_bytes` (the in-process compile) | **149,678,080** | **81,092,608** | **1.846** | 207,814,656 | 78,745,600 | 2.638 |
| `slice` — `case_add_peak_rss_bytes` (N=1, whole process) | **68,065,280** | **67,512,320** | **1.008** | — | — | — (m21 has no `slice` row) |
| `slice` — `case_string_str_length_peak_rss_bytes` (N=1, whole process) | **68,243,456** | **67,602,432** | **1.009** | — | — | — (m21 has no `slice` row) |

Reading the table:
- **The loop rows are no longer boot-dominated on the rust side.** In m21 every rust loop row read ~96.7 MB regardless of
  the loop (i.e. it was measuring the boot), so `gc_bound`/`list_walk` read 1.43x and `gc_class/send_recv` read 2.86x — the
  **boot**, not the workload. m22's boot is 69.2 MB (table 2), so the same rows now sit at **1.011–1.014x**, i.e. at parity with the oracle's
  own ~67.6 MB process. `gc_class/send_recv`'s 2.862 -> 1.014 is the largest single memory change in this record.
- `slice` is the only **like-for-like** (N=1) footprint comparison and it is new in m22 (m21 had no `slice` row): every case
  reads **1.008–1.013x**, so a whole-process `beam_rt` costs ~0.6–0.9 MB more than a whole-process `beam_erl` on this host.
  Per-case values: add 1.008x, fib 1.009x, pick 1.008x, calendar_last_day 1.013x, queue_from_size 1.009x, gb_trees_size 1.010x, base64_encode_size 1.012x, ordsets_from_size 1.009x, string_str_length 1.009x.
- `baseline`'s 100.3 MB (vs m21's 128.1 MB) is **above** the bare boot because its compute loop builds a 1 000 000-element
  cons list (`lists:sum(lists:seq(1,1000000))`); the oracle pays 135.2 MB for the same work, so rust is 0.742x there too.
- `gc_class/compile` remains the one row whose rust peak is well above the oracle (1.846x, 149.7 MB vs 81.1 MB): an
  in-process `compile:file/2` of `lib/stdlib/src/ordsets.erl` holds the compiler's working set, and that gap narrowed from
  2.638x in m21 but is **not** closed. It is a compiler-heap question, not a boot question.
- The **sys side did not move** between m21 and m22 (135.17 -> 135.23 MB for `baseline`; 67.6 -> 67.6 MB for the loop rows),
  so every memory delta in this table is a rust-side change.

## 2. Boot footprint — independent `wait4`/`ru_maxrss` confirmation (median of 5, all 5 shown)

| command | side | samples (KiB) | median KiB | median bytes | median MiB |
|---|---|---|---|---|---|
| `-noshell -eval 'halt().'` | **rust** | 67640 67572 67836 67580 67616 | 67,616 | 69,238,784 | **66.03** |
| `-noshell -eval 'halt().'` | **sys** | 62592 62656 63664 63428 62440 | 62,656 | 64,159,744 | **61.19** |
| `-noshell -s init stop` | **rust** | 67616 67716 67536 67644 67868 | 67,644 | 69,267,456 | **66.06** |
| `-noshell -s init stop` | **sys** | 62460 63360 63144 63132 62816 | 63,132 | 64,647,168 | **61.65** |

| command | **rust/sys** (median) |
|---|---|
| `-noshell -eval 'halt().'` | **1.079** |
| `-noshell -s init stop` | **1.071** |

Notes on this table:
- The boot figure is **66.03 MiB (halt) / 66.06 MiB (init stop)** on the rust side against
  **61.19 / 61.65 MiB** on the oracle side, i.e. **1.079x / 1.071x**.
  The workstream's own claim for the four changes (boot `VmHWM` 91.45 -> 66.68 MiB) is confirmed independently within 1%
  (we measure 66.03–66.06 MiB); the *before* figure is the m21-era boot and this record contains no m21 boot sample of its
  own, so the before/after pair is quoted from the workstream claim, not re-measured here.
- The samples are tightly clustered (rust 65.95–66.28 MiB over 10 spawns, sys 60.98–62.17 MiB), so the boot number is a stable
  property of the binary, not a load artifact; `/proc/loadavg` 1-min moved 9.11–13.34 across the 20 samples.
- The oracle prints its Erlang shell banner on the `-s init stop` form even with `-noshell` (`Eshell V17.1.1`, `*** Terminating
  erlang`); that is oracle output, not a harness artifact, and it does not affect the RSS comparison (identical argv per side).
- `beam_rt` is **not** at boot parity: it is ~8% dearer than the 14-scheduler SMP oracle at boot, which is where this
  workstream found its biggest win (91.45 -> 66.68 MiB) — the remaining gap is small enough that the loop rows' RSS is now
  dominated by the workload rather than the boot.

## 3. Cross-reference
| file | what it is |
|---|---|
| `m22-xrun-r{1..6}-{rust,sys}.json` | the twelve matched single-side `Report`s — every RSS figure above is read from these |
| `m22-xrun-ratios.txt` §D | the per-round RSS values and per-round ratios |
| `m22-vs-m21.txt` | the same rows diffed against m21, with the noise-band verdict per row |
| `m22-load-record.md` | the host/load record and the withheld audit |
