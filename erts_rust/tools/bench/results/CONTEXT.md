# Archived benchmark results (`erts_rust/tools/bench/results`)

## Intent
Durable, reviewable copies of the raw `bench` `Report` JSON (and the `bench compare` diffs) behind
`../RESULTS.md`.
`out/` is gitignored, so a measurement would otherwise vanish with the worktree; anything committed here is a number someone may quote, so **only real harness output belongs here** — never a hand-written or estimated figure.
The `m<session>-` prefix names the measurement session the files belong to; the newest session is **current**, older ones are kept as regression baselines.

## API Surface (what each file is)

### `m10-` — the current session (2026-10-13 23:06–23:15 UTC, commit `b4593818b3a27f47e3b682a1a50fbfcc0a1785fe`)
- `m10-bench-xrun.json` — **the full cross-runtime report**: `run --full --runtime all`, the six `Runtimes::Both` workloads × both runtimes (12/12 rows `ok`). The source of every headline number in `../RESULTS.md` §3.
- `m10-bench-xrun-interp.json` — **the tier-effect control**: the same six workloads, `--runtime rust` only, with `BEAM_RT_TIER=interpreter` pinned (6 `rust` rows, **no `sys` rows**). This is a **Rust-vs-Rust** artefact and is never a system-ERTS comparison.
- `m10-compare-cross-runtime.txt` — `bench compare` of `m10-bench-xrun.json` (sys vs rust inside one report).
- `m10-compare-tier-effect.txt` — `bench compare m10-bench-xrun-interp.json m10-bench-xrun.json` (Rust interpreter → Rust default; `a` = interpreter).
- `m10-compare-vs-m9-xrun.txt` / `m10-compare-vs-m9-external.txt` — the tool's two-file diffs of the m10 record against m9 (the source of `../RESULTS.md` §7).
- `m10-bench-external.json` — the full sys-only external targets in one report: `erlc_compiler`, `elixir_compiler`, `cowboy`, `khepri` (4/4 `ok`). The source of `../RESULTS.md` §4.
- `m10-bench-plb.json` — the PLB run: `ok`, `wall_ms` 70 841.43, `dotnet` 9.0.318 resolved from `PATH` with the two 8.0.425 rejections noted.
- `m10-bench-emqx.json` — the EMQX skip observation: `skipped(emqx-requires-otp-28)`.
- `m10-load-record.md` — the session's commit, exact commands, the per-run `/proc/loadavg` before/after record, per-workload status, per-case detail and the tier-effect table.
- `m10-tool-targets-notes.md` — the per-target status/blocker notes and the environment findings.

### `m9-` — the previous session (2026-10-13 13:25–14:10 UTC)
The full cross-runtime report (`m9-bench-xrun.json`), its diff (`m9-compare-cross-runtime.txt`), the full external-target report (`m9-bench-external.json`), the PLB/EMQX observations and their post-fix smokes (`m9-bench-plb*.json`, `m9-bench-emqx*.json`), and the `m9-load-record.md`/`m9-tool-targets-notes.md` records. The regression baseline `../RESULTS.md` §7 diffs against.

### `m6-` — the session before that (2026-10-11)
The per-target reports (`m6-bench-plb.json`, `m6-bench-khepri.json`, `m6-bench-emqx.json`, `m6-bench-cowboy.json`, `m6-bench-erlc-compiler.json`, `m6-bench-elixir-compiler.json`, `m6-bench-tool-targets.json`), the focused cross-runtime report (`m6-bench-xrun.json`), its diff (`m6-compare-cross-runtime.txt`), and the `m6-load-record.md`/`m6-tool-targets-notes.md` records.

### `m5-` / `m4-` — older sessions (2026-10-09 / 2026-10-08)
`m5-`: primary full run (`m5-bench-full-runtimes-all.json`), focused `xrun`, PLB runs, cross-runtime diff. `m4-`: primary full run (`m4-bench-full-runtimes-all.json`), the three focused xruns (the reproducibility error bar), the compiler/cowboy/khepri runs, the PLB attempts, and the `m4-compare-*.txt` diffs — the near-quiet reference.

## Constraints
- **Never edit a metric value.** The one permitted normalisation is replacing an environment-specific
  absolute path with a portable one — the run-host worktree prefix in `runtimes[].program` becomes
  `erts_rust/target/release/beam_rt`, and a scratch `$TMPDIR` prefix in `notes` becomes `$TMPDIR/`; this is
  applied mechanically when archiving and is disclosed in `../RESULTS.md`.
- Regenerate by re-running `../CONTEXT.md`'s runbook and copying the new JSON in — do not patch.
- Keep `../RESULTS.md` in sync: it is the human-readable face of these files.

## Notes for Agents
- Consume these with the driver, not by hand: `bench compare <file>` (cross-runtime) or
  `bench compare <a.json> <b.json>` (two file; `a` is the baseline side of each delta). The tool diffs the
  **mean**, and everything else is left alone. A `--runtime rust` report (the `m10-*-interp` artefact) has
  no `sys` rows, so a one-path compare on it is not a cross-runtime diff — use the two-file form.
- JSON schema is `Report` / `WorkloadResult` (`SCHEMA_VERSION = 1`); a row is `ok` (has metrics),
  `skipped` (has a `reason`, no metrics) or `failed` (has a `reason`).
- Absolute performance depends on the host **and on its current load**: the m10 full passes ran at
  `/proc/loadavg` 5.43 → 7.59 (cross-runtime), 7.94 → 8.03 (tier control) and 21.12 → 10.30 (external);
  m9's ran at 11.01 → 6.02 and 4.68 → 4.43. Prefer ratios over absolute values; the environment and the
  observed load are pinned in `../RESULTS.md` §1/§7 and in the per-session `m*-load-record.md`.
- **Never compare a Rust tier column with the `sys` column without saying which is which.** The `m10`
  pair `m10-bench-xrun.json` (sys + rust default) and `m10-bench-xrun-interp.json` (rust interpreter only)
  exist precisely to keep the two Rust tiers separable from the system-ERTS column.

## See Also
- `../RESULTS.md` → the summary tables, exact commands, the m10-vs-m9 deltas and the ranked bottleneck list.
- `../CONTEXT.md` → the runbook and the workload definitions.
