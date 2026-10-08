# Archived benchmark results (`erts_rust/tools/bench/results`)
## Intent
Durable, reviewable copies of the raw `bench` `Report` JSON (and the `bench compare` diffs) behind
`../RESULTS.md`. `out/` is gitignored, so a measurement would otherwise vanish with the worktree;
anything committed here is a number someone may quote, so **only real harness output belongs here** —
never a hand-written or estimated figure.
The `m3-*` files are the **current** run; the `m1-*` files are the **superseded** Milestone-1 snapshot,
kept verbatim for reference. The prefix is the milestone the run belongs to.
## API Surface (what each file is)
### Current run (`m3-*`)
- `m3-bench-xrun-baseline-slice-foreign.json` — **the primary cross-runtime report**:
  `run --full --runtime all --workloads baseline,slice,foreign`. Both sides see the same machine
  conditions, so this is the source of every sys-vs-rust table in `../RESULTS.md` §3.
- `m3-bench-xrun-baseline-slice-foreign-run2.json` — an identical repetition; the source of the
  cross-runtime stability figures in `../RESULTS.md` §7.
- `m3-bench-full-runtimes-all.json` — **the primary full report**: `run --full --runtime all`, all 9
  workloads × both runtimes. Source of the sys-only tables (`../RESULTS.md` §4).
- `m3-bench-full-runtimes-all-run2.json` — a second full run, taken under different host load; it is
  the evidence for the contention spread in `../RESULTS.md` §7.
- `m3-bench-quick-runtimes-all.json` — `run --quick --runtime all`, all 9 workloads (the fast sweep).
- `m3-bench-full-nix-khepri-emqx.json` — `BENCH_TRY_NIX=1 … run --full --runtime sys
  --workloads khepri,emqx`: the `khepri` wall time and EMQX's verified `skipped(not-a-rebar3-project)`.
- `m3-bench-quick-nix-plb-attempt.json` — the bounded PLB attempt; `failed(plb-tool-failed)` with the
  `dotnet` build error in its notes (the verified absence of a PLB number).
- `m3-compare-cross-runtime.txt` — `bench compare` of the primary cross-runtime report (sys vs rust).
- `m3-compare-xrun-run1-vs-run2.txt` — `bench compare` of the two cross-runtime runs (76 rows).
- `m3-compare-full-runB-vs-runA.txt` — `bench compare` of the two full runs (the noise bound).
### Superseded snapshot (`m1-*`)
The Milestone-1 report, its repetition, the nix/`plb` runs and the two M1 compare diffs, under the
original filenames with an `m1-` prefix. **Do not quote them** — the `beam_rt` per-BIF-call defect they
measure has since been fixed, so their `foreign` per-call rows (703×–9 891×) and their "not run"
`erlang:memory` row for Rust are both obsolete. See `../RESULTS.md` §3 for the current values.
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
- Absolute performance depends on the host **and on its current load**: the archived full runs differ by
  up to 11× on `cowboy` throughput. The environment and the observed `/proc/loadavg` are pinned in
  `../RESULTS.md` §1/§7; prefer the focused `m3-bench-xrun-*` pair for cross-runtime comparisons.
## See Also
- `../RESULTS.md` → the summary tables, exact commands and caveats.
- `../CONTEXT.md` → the runbook and the workload definitions.
