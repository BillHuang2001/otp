# Archived benchmark results (`erts_rust/tools/bench/results`)

## Intent
Durable, reviewable copies of the raw `bench` `Report` JSON (and the `bench compare` diffs) behind
`../RESULTS.md`.
`out/` is gitignored, so a measurement would otherwise vanish with the worktree; anything committed here is a number someone may quote, so **only real harness output belongs here** — never a hand-written or estimated figure.
The `m<session>-` prefix names the measurement session the files belong to; the newest session is **current**, older ones are kept as regression baselines **only when their statistic is still comparable** (see the `m14-` caveat below).

## API Surface (what each file is)

### `m22-` — the current session: the GC/allocator workstream's *after* record (2026-10-27, `/proc/loadavg` 7.0–21.9)
- Six **matched** rust-only/sys-only round pairs over `baseline,gc_bound,gc_class,list_walk,slice`, side order alternating every round, every pass `--quick --verbose`, pinned `taskset -c 3 setsid -w`, one process at a time: `m22-xrun-r{1..6}-{rust,sys}.json` (12 reports, 60/60 rows `ok`, **zero withheld marginals**).
- `slice` is in this session's set for the first time in a GC-workstream record (m21 had no `slice` row); it carries the N=1 footprint/boot story.
- `m22-xrun-ratios.txt` — the derived table: per-round values, per-round `rust/sys` ratios, the median ratio, the slice p50/p99 case tables, the deterministic `case_*_ops`/`_elements` companions, the withheld audit and the runtime resolution (the oracle is **OTP 29.1.2 / erts-17.1.1**, the same store path m21 used).
- `m22-vs-m21.txt` — **the record's headline reading**: a per-row diff of the rust/sys ratios of the two records, each named, with the noise band it was judged against (`max(1.4826·MAD)` of the two records' per-round ratios) and a per-row `OUTSIDE`/`inside` verdict, plus the raw min–max ranges and the three `inside` rows spelled out.
- `m22-memory.md` — the RSS rows (`rust` vs `sys`, m21 alongside) and an **independent** `wait4`/`ru_maxrss` boot confirmation, median of 5 per command per side.
- `m22-load-record.md` — the per-pass `/proc/loadavg` before/after record, the command shape, the two load-excursion rounds (r2, r6), the withheld audit (none) and the caveats that travel with the numbers.
- `m22-budgetprobe-all.json` / `.txt` and `m22-compare-cross-runtime.txt` — the same **supplementary** `--runtime all` artefact pair as m21 (it exists only to judge the both-sides budget ratio rows and to give `bench compare` one report holding both sides; runtime-major, so never a timing source).

### `m21-` — the GC/allocator workstream's *before* record (2026-10-27, `/proc/loadavg` 5.7–19.6)
- Seven **matched** rust-only/sys-only round pairs over the group `baseline,gc_bound,gc_class,list_walk`, side order alternating every round, every pass `--quick`, pinned `taskset -c 3 setsid -w`, one process at a time: `m21-xrun-gc-r{1..7}-{rust,sys}.json` (14 reports, 56/56 rows `ok`).
- `m21-xrun-ratios.txt` — the derived table: per-round values, per-round `rust/sys` ratios, the median ratio, the deterministic `case_*_ops`/`_elements` companions, the withheld audit and the runtime resolution (the oracle here is **OTP 29.1.2 / erts-17.1.1**, not the 29.1.1 earlier records name).
- `m21-load-record.md` + `m21-xrun-load.log` — the per-pass `/proc/loadavg` before/after record, the command shape, the two spike-affected rounds, the single withheld figure (`r7/rust churn`) and the memory rows.
- `m21-budgetprobe-all.json` / `.txt` — one supplementary `--runtime all --quick` pass, run **only** so the both-sides-in-one-report budget ratio rows can be judged and so `bench compare` has a report carrying both sides; it is runtime-major and ordering-biased, so it is **not** a timing source.
- `m21-compare-cross-runtime.txt` — `bench compare m21-budgetprobe-all.json` (the driver's own cross-runtime diff of that supplementary pass).
- Its workload set is deliberately narrow: `slice` and the six other `Runtimes::Both` workloads are **not** in m21, so a footprint or per-call figure for them comes from `m22` (start-up, `slice`, memory, the four GC-workstream per-unit rows) or from `m19` (everything else).

### `m15-` — the superseded full-suite session (2026-10-17, `/proc/loadavg` ≈2–5)
- `m15-bench-xrun.json` — **the full cross-runtime report**: `run --full --runtime all`, the six `Runtimes::Both` workloads × both runtimes (12/12 rows `ok`). The source of every headline number in `../RESULTS.md` §2/§3.
- `m15-bench-xrun-interp.json` / `m15-bench-xrun-quick.json` — **the tier-effect control pair**: the same six workloads, `--runtime rust` only, `--quick` on **both** sides (a matched form) with `BEAM_RT_TIER=interpreter` vs the default baseline JIT. These are **Rust-vs-Rust** artefacts and are never a system-ERTS comparison.
- `m15-bench-external.json` — the full sys-only external targets in one report (`erlc_compiler`, `elixir_compiler`, `cowboy`, `khepri`, 4/4 `ok`).
- `m15-bench-plb.json` (`ok`, `dotnet` 9.0.318 via the version-aware resolution) and `m15-bench-emqx.json` (`skipped(emqx-requires-otp-28)`).
- `m15-compare-cross-runtime.txt` (`bench compare m15-bench-xrun.json`), `m15-compare-tier-effect-quick.txt` (interp vs default, matched `--quick`), `m15-compare-vs-m14-xrun.txt`, `m15-compare-vs-m14-external.txt` (two-file diffs, `a` = the m14 baseline side).
- `m15-load-record.md` + `m15-{xrun,tier,ext}-load.log` — the session's per-run `/proc/loadavg` before/after record, commands and per-workload status.

### `m14-` — the previous session (2026-10-16), **not a valid regression baseline for the per-unit rows**
`m14-bench-xrun.json` (+ the interp/quick tier pair), `m14-bench-{external,plb,emqx}.json`, the `m14-compare-*.txt` diffs, and the prose records `m14-recalibration.{md,csv}`, `m14-load-record.md`, `m14-tool-targets-notes.md`, `m14-tier-effect.csv`, plus the interleaved A/B set `m14-ab-{head,m13,m10}-r{1,2,3}.json` + `m14-ab-summary.csv`.
**Caveat:** m14 measured the per-unit marginals with the retired sequential schedule (all `N`-op samples, then all `0`-op samples), which absorbed boot drift; its `copy_bound`/`gc_bound`/`foreign` per-unit figures are **withdrawn** (`../RESULTS.md` §3.4) and must not be diffed against m15. Its `baseline`/`slice` rows and its startup/throughput/external figures remain usable.
The `m14-ab-*` A/B set is the evidence that closed the `foreign: emqx_base62:encode` question (parity, 0.95×–1.07× vs the m10 binary).

### `m13-` … `m4-` — older sessions (2026-10-12 … 2026-10-08)
Regression baselines from the sequential-marginal era, so their per-unit loop rows carry the same withdrawal as `m14-`.
`m13-*` also holds `m13-cli-probe.csv` and `m13-foreign-ladder.csv`; `m12-*` holds the prose records `m12-foreign-regression.md`, `m12-run-route-latency.md`, `m12-emqx-enablement-attempt.md` (+ `m12-emqx-ezstd-env-mk.patch`); the `b54-*`/`b55-*` groups are the Θ(N³) `foreign`-regression experiments and their fix.
Each session's `m*-load-record.md` / `m*-tool-targets-notes.md` pins the host conditions and per-target blockers.

## Constraints
- **Never edit a metric value.** The one permitted normalisation is replacing an environment-specific absolute path with a portable one — the run-host worktree prefix in `runtimes[].program` becomes `erts_rust/target/release/beam_rt`, any other worktree prefix is removed, and a scratch `/tmp/bench-scratch-…` (or `/tmp/genesis/task_<id>/…`) prefix in `notes` becomes `$TMPDIR/…`; this is applied mechanically when archiving and is disclosed in the session's own load record (`m21-load-record.md` for m21).
- Regenerate by re-running `../RESULTS.md`'s Runbook and copying the new JSON in — do not patch.
- Keep `../RESULTS.md` in sync: it is the human-readable face of these files, and it must name the current session.

## Notes for Agents
- Consume these with the driver, not by hand: `bench compare <file>` (cross-runtime) or `bench compare <a.json> <b.json>` (two file; `a` is the baseline side of each delta). The tool diffs the **mean**, and everything else is left alone. A `--runtime rust` report (the `-interp`/`-quick` artefacts) has no `sys` rows, so a one-path compare on it is not a cross-runtime diff — use the two-file form.
- JSON schema is `Report` / `WorkloadResult` (`SCHEMA_VERSION = 1`); a row is `ok` (has metrics), `skipped` (has a `reason`, no metrics) or `failed` (has a `reason`).
- Absolute performance depends on the host **and on its current load**: m21 ran at ≈5.7–19.6, m19 at 0.5–4.5 and m14 at 40–145, so even the absolute times moved with the host, not only with the code. Prefer ratios, the deterministic `case_*_ops`/`calls`/`elements` companions and the recorded load over any single absolute figure.
- **Never compare a Rust tier column with the `sys` column without saying which is which.** The `-interp`/`-quick` pair exists precisely to keep the two Rust tiers separable from the system-ERTS column.
- **A per-round ratio is only comparable with a record taken against the same oracle.** `m22`'s and `m21`'s `sys` side are both OTP **29.1.2 / erts-17.1.1** (the same store path), so those two records are mutually diffable row for row; m17–m19 named 29.1.1 / erts-17.1, so quote a cross-record ratio only after checking which `erl` each side names (`m22-xrun-ratios.txt` §G, `m21-xrun-ratios.txt` §G).
- **A `--runtime all` report is a supplementary artefact, never a timing source** (`BOTTLENECKS.md` §1.1 bans the runtime-major ordering as a headline): `m21-budgetprobe-all.*` exists only to judge the both-sides budget rows and to give `bench compare` one report holding both sides, because two single-side reports share no `(workload, runtime)` key and the two-file mode then reports every row `only-in-a`/`only-in-b`.
- **No `m20-*` artifact exists here**, so a provenance statement naming "m17–m20" (or "m20's record") means m17–m19 plus the artifact-less `m20` that the `erts_rust`-level docs name — every artifact-backed record in this directory is `m19-*`, `m21-*` or `m22-*`.

## See Also
- `../RESULTS.md` → the m15 summary tables, exact commands, the paired-estimator record (§3.4) and the still-open items.
- `m22-load-record.md` → the current GC/allocator record: per-pass load table, command shape, withheld audit, memory rows; its `m22-vs-m21.txt` is the before/after reading.
- `m21-load-record.md` → the same for the workstream's *before* record.
- `../CONTEXT.md` → the workload definitions, the loop-count calibration rule and the runbook pointer.
- `../src/marginal.rs` → the paired estimator that produced the per-unit figures.
