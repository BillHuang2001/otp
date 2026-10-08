# Archived benchmark results (`erts_rust/tools/bench/results`)
## Intent
Durable, reviewable copies of the raw `bench` `Report` JSON (and the `bench compare` diffs) behind
`../RESULTS.md`.
`out/` is gitignored, so a measurement would otherwise vanish with the worktree; anything committed here is a number someone may quote, so **only real harness output belongs here** — never a hand-written or estimated figure.
The `m4-` prefix is the measurement session the files belong to; all files here are the same shared-host session on 2026-10-08.
## API Surface (what each file is)
### `m4-` benchmark reports
- `m4-bench-full-runtimes-all.json` — **the primary full report**: `run --full --runtime all`, all 9 workloads × both runtimes.
  The source of every headline number in `../RESULTS.md` §3 and §4.
- `m4-bench-xrun-baseline-slice-foreign.json` — focused cross-runtime run 1: `run --full --runtime all --workloads baseline,slice,foreign`.
- `m4-bench-xrun-baseline-slice-foreign-run2.json` — focused run 2.
- `m4-bench-xrun-baseline-slice-foreign-run3.json` — focused run 3.
  Runs 1–3 were taken under heavy contention and are the reproducibility **error bar** in `../RESULTS.md` §6.
- `m4-bench-full-compilers-sys.json` — `erlc_compiler` + `elixir_compiler`, `run --full --runtime sys`.
- `m4-bench-full-compilers-sys-run2.json` — the compiler repetition (the §4.1 brackets).
- `m4-bench-full-cowboy-sys.json` — `cowboy`, `run --full --runtime sys`, taken in a quiet window (the §4.2 brackets).
- `m4-bench-full-khepri-emqx.json` — `khepri` + `emqx`, `run --full --runtime sys` with the store `rebar3` on `PATH`: the `khepri` wall time and EMQX's verified `skipped(not-a-rebar3-project)`.
- `m4-bench-quick-plb-net9-attempt.json` — the bounded PLB attempt with the .NET 9 SDK on `PATH`; `failed(plb-tool-failed)` with the tool's `DirectoryNotFoundException` in its notes (the verified absence of a PLB number).
### `m4-` compare diffs
- `m4-compare-cross-runtime.txt` — `bench compare` of the primary full report (sys vs rust inside one report).
- `m4-compare-xrun-run1-vs-run2.txt` — `bench compare` of focused runs 1 and 2 (76 rows).
- `m4-compare-xrun-run2-vs-run3.txt` — `bench compare` of focused runs 2 and 3 (76 rows).
## Constraints
- **Never edit a metric value.** The one permitted normalisation is replacing the run-host worktree
  prefix in `runtimes[].program` with `erts_rust/target/release/beam_rt`, so the artifact carries no
  environment-specific absolute path; this is applied mechanically when archiving and is disclosed in
  `../RESULTS.md`.
- Regenerate by re-running `../CONTEXT.md`'s runbook and copying the new JSON in — do not patch.
- Keep `../RESULTS.md` in sync: it is the human-readable face of these files.
## Notes for Agents
- Consume these with the driver, not by hand: `bench compare <file>` (cross-runtime) or
  `bench compare <a.json> <b.json>`.
- JSON schema is `Report` / `WorkloadResult` (`SCHEMA_VERSION = 1`); a row is `ok` (has metrics),
  `skipped` (has a `reason`, no metrics) or `failed` (has a `reason`).
- Absolute performance depends on the host **and on its current load**: the focused `m4-bench-xrun-*`
  runs were taken under `/proc/loadavg` ~22–37 and their absolute values are inflated versus the quiet
  primary run (`/proc/loadavg` 4.77 → 11.59). The environment and the observed load are pinned in
  `../RESULTS.md` §1/§6; prefer the quiet primary run and ratios over absolute values.
## See Also
- `../RESULTS.md` → the summary tables, exact commands and caveats.
- `../CONTEXT.md` → the runbook and the workload definitions.
