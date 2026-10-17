# Archived benchmark results (`erts_rust/tools/bench/results`)

## Intent
Durable, reviewable copies of the raw `bench` `Report` JSON (and the `bench compare` diffs) behind
`../RESULTS.md`.
`out/` is gitignored, so a measurement would otherwise vanish with the worktree; anything committed here is a number someone may quote, so **only real harness output belongs here** — never a hand-written or estimated figure.
The `m<session>-` prefix names the measurement session the files belong to; the newest session is **current**, older ones are kept as regression baselines **only when their statistic is still comparable** (see the `m14-` caveat below).

## API Surface (what each file is)

### `m15-` — the current session (2026-10-17, `/proc/loadavg` ≈2–5)
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
- **Never edit a metric value.** The one permitted normalisation is replacing an environment-specific absolute path with a portable one — the run-host worktree prefix in `runtimes[].program` becomes `erts_rust/target/release/beam_rt`, and a scratch `$TMPDIR` prefix in `notes` becomes `$TMPDIR/`; this is applied mechanically when archiving and is disclosed in `../RESULTS.md`.
- Regenerate by re-running `../RESULTS.md`'s Runbook and copying the new JSON in — do not patch.
- Keep `../RESULTS.md` in sync: it is the human-readable face of these files, and it must name the current session.

## Notes for Agents
- Consume these with the driver, not by hand: `bench compare <file>` (cross-runtime) or `bench compare <a.json> <b.json>` (two file; `a` is the baseline side of each delta). The tool diffs the **mean**, and everything else is left alone. A `--runtime rust` report (the `-interp`/`-quick` artefacts) has no `sys` rows, so a one-path compare on it is not a cross-runtime diff — use the two-file form.
- JSON schema is `Report` / `WorkloadResult` (`SCHEMA_VERSION = 1`); a row is `ok` (has metrics), `skipped` (has a `reason`, no metrics) or `failed` (has a `reason`).
- Absolute performance depends on the host **and on its current load**: m15 ran at ≈2–5 while m14 ran at 40–145, so even the absolute times moved with the host, not only with the code. Prefer ratios, the deterministic `case_*_ops`/`calls`/`elements` companions and the recorded load over any single absolute figure.
- **Never compare a Rust tier column with the `sys` column without saying which is which.** The `-interp`/`-quick` pair exists precisely to keep the two Rust tiers separable from the system-ERTS column.

## See Also
- `../RESULTS.md` → the m15 summary tables, exact commands, the paired-estimator record (§3.4) and the still-open items.
- `../CONTEXT.md` → the workload definitions, the loop-count calibration rule and the runbook pointer.
- `../src/marginal.rs` → the paired estimator that produced the m15 per-unit figures.
