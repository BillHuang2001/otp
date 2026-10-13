# Archived benchmark results (`erts_rust/tools/bench/results`)
## Intent
Durable, reviewable copies of the raw `bench` `Report` JSON (and the `bench compare` diffs) behind
`../RESULTS.md`.
`out/` is gitignored, so a measurement would otherwise vanish with the worktree; anything committed here is a number someone may quote, so **only real harness output belongs here** — never a hand-written or estimated figure.
The `m<session>-` prefix names the measurement session the files belong to; the newest session is **current**, older ones are kept as regression baselines.
## API Surface (what each file is)
### `m9-` — the current session (2026-10-13)
- `m9-bench-xrun.json` — **the full cross-runtime report**: `run --full --runtime all`, the six `Runtimes::Both` workloads × both runtimes (12/12 rows `ok`). The source of every headline number in `../RESULTS.md` §3.
- `m9-bench-external.json` — the full sys-only external targets in one report: `erlc_compiler`, `elixir_compiler`, `cowboy`, `khepri` (4/4 `ok`). The source of `../RESULTS.md` §4.
- `m9-bench-plb.json` — the PLB observation from the measurement run: `failed(plb-build-failed)` (`NETSDK1045`; the `PATH` `dotnet` was 8.0.425 vs the `net9.0` target).
- `m9-bench-emqx.json` — the EMQX skip observation: `skipped(emqx-requires-otp-28)`.
- `m9-bench-plb-smoke.json` — the post-fix PLB smoke (14:49 UTC): `ok`, store 9.0.318 resolved from `PATH`; carries the resolved-and-rejected provenance notes.
- `m9-bench-plb-smoke-path8.json` — the post-fix PLB smoke with the defect condition reproduced (14:51 UTC): a real 8.0.425 `dotnet` forced first on `PATH` → still `ok`, store 9.0.318 resolved (the decisive proof).
- `m9-bench-emqx-smoke.json` — the post-fix EMQX smoke (14:51 UTC): re-confirmed `skipped(emqx-requires-otp-28)` with the exact pin (`28.5.0.7-2`) and mechanism note.
- `m9-compare-cross-runtime.txt` — `bench compare` of `m9-bench-xrun.json` (sys vs rust inside one report, 69 numeric rows).
- `m9-load-record.md` — the session's exact commands and the per-run `/proc/loadavg` before/after record.
- `m9-tool-targets-notes.md` — the per-target status/blocker notes and the environment findings.
### `m6-` — the previous session (2026-10-11)
The per-target reports (`m6-bench-plb.json`, `m6-bench-khepri.json`, `m6-bench-emqx.json`, `m6-bench-cowboy.json`, `m6-bench-erlc-compiler.json`, `m6-bench-elixir-compiler.json`, `m6-bench-tool-targets.json`), the focused cross-runtime report (`m6-bench-xrun.json`), its diff (`m6-compare-cross-runtime.txt`), and the `m6-load-record.md`/`m6-tool-targets-notes.md` records. The regression baseline `../RESULTS.md` §7 diffs against.
### `m5-` / `m4-` — older sessions (2026-10-09 / 2026-10-08)
`m5-`: primary full run, focused `xrun`, PLB runs, cross-runtime diff. `m4-`: primary full run
(`m4-bench-full-runtimes-all.json`), the three focused xruns (the reproducibility error bar), the
compiler/cowboy/khepri runs, the PLB attempts, and the `m4-compare-*.txt` diffs — the near-quiet reference.
## Constraints
- **Never edit a metric value.** The one permitted normalisation is replacing an environment-specific
  absolute path with a portable one — the run-host worktree prefix in `runtimes[].program` becomes
  `erts_rust/target/release/beam_rt`, and a scratch `$TMPDIR` prefix in `notes` becomes `$TMPDIR/`; this is
  applied mechanically when archiving and is disclosed in `../RESULTS.md`.
- Regenerate by re-running `../CONTEXT.md`'s runbook and copying the new JSON in — do not patch.
- Keep `../RESULTS.md` in sync: it is the human-readable face of these files.
## Notes for Agents
- Consume these with the driver, not by hand: `bench compare <file>` (cross-runtime) or
  `bench compare <a.json> <b.json>`.
- JSON schema is `Report` / `WorkloadResult` (`SCHEMA_VERSION = 1`); a row is `ok` (has metrics),
  `skipped` (has a `reason`, no metrics) or `failed` (has a `reason`).
- Absolute performance depends on the host **and on its current load**: the m9 full passes ran at
  `/proc/loadavg` 11.01 → 6.02 (cross-runtime) and 4.68 → 4.43 (external), while earlier passes in the same
  session reached 28.95. Prefer ratios over absolute values; the environment and the observed load are pinned
  in `../RESULTS.md` §1/§7 and in the per-session `m*-load-record.md`.
## See Also
- `../RESULTS.md` → the summary tables, exact commands and caveats.
- `../CONTEXT.md` → the runbook and the workload definitions.
