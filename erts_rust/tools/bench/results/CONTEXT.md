# Archived benchmark results (`erts_rust/tools/bench/results`)

## Intent
Durable, reviewable copies of the raw `bench` `Report` JSON (and the `bench compare` diffs) behind
`../RESULTS.md`. `out/` is gitignored, so a milestone number would otherwise vanish with the
worktree; anything committed here is a number someone may quote, so **only real `--full` (or
explicitly-labelled) harness output belongs here** — never a hand-written or estimated figure.

## API Surface (what each file is)
- `bench-full-runtimes-all.json` — **the primary milestone report**: `run --full --runtime all`,
  all 9 workloads × both runtimes, on the reference host. Every table in `../RESULTS.md` §3/§4 comes
  from this file.
- `bench-full-runtimes-all-run2.json` — an identical repetition of the above; the source of the
  run-to-run stability section (`../RESULTS.md` §7).
- `bench-full-nix-khepri-emqx.json` — `BENCH_TRY_NIX=1 … run --full --runtime sys
  --workloads khepri,emqx`: the `khepri` wall time and EMQX's verified
  `skipped(not-a-rebar3-project)`.
- `bench-quick-nix-plb-attempt.json` — the bounded PLB attempt; `failed(plb-tool-failed)` with the
  `dotnet` build error in its notes (the verified absence of a PLB number).
- `compare-cross-runtime.txt` — `bench compare bench-full-runtimes-all.json` (sys vs rust within one
  report).
- `compare-run1-vs-run2.txt` — `bench compare` of the two full runs (88 numeric rows).

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
- Absolute performance depends on the host; the environment for the archived run is pinned in
  `../RESULTS.md` §1.

## See Also
- `../RESULTS.md` → the summary tables, exact commands and caveats.
- `../CONTEXT.md` → the runbook and the workload definitions.
