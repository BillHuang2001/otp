# `results/` — committed sweep artifacts and their analysis

## Intent
Raw evidence behind `../RESULTS.md`: one directory per measured target, holding the `report.json` + `summary.md` the driver wrote, the per-row analysis of what those rows mean, and the hand-written probes that reduced each divergence to a smallest reproducer.
Nothing here is produced by `suites run` except `khepri/`, `stdlib/` and the two `*-notes.md` files; the `*-diagnostics/` directories are hand-run and their numbers are transcribed into the notes by hand.
Every row names both sides (oracle = system `erl` on `$PATH`, rust = `../target/release/beam_rt`) and a verdict; no case is ever reported as a pass on one side only.

## API Surface
- `stdlib/report.json` + `stdlib/summary.md` — the OTP-`stdlib` sweep (`schema suites/1`); `report.json` is authoritative and `summary.md` is rendered from it.
- `stdlib-notes.md` — analysis of that sweep: per-suite counts, the root-cause classes D1…D10, the exclusions (`agree-fail-identical`) and the harness caveats.
- `stdlib/raw/` — hand-captured probe output kept as evidence for `stdlib-notes.md` §D1/§D10; it is **not** part of the sweep artifact set.
- `khepri/report.json` + `khepri/summary.md` + `khepri-notes.md` — the same three things for Khepri's 44 eunit modules (classes RC-A…RC-D).

## Constraints
- **Counts and ratios only.** The host is shared, so a wall time here is never portable; a diagnostic may quote a same-host oracle/rust ratio and the operation counts behind it, never an absolute duration as a claim.
- **Re-derive, don't restate.** A number in `*-notes.md` must be re-derivable from the committed `report.json` (or from a committed probe), and the notes' environment block is copied from the report's own `host`/`rust`/`generated_at` fields.
- **`FINDINGS.md` supersedes a notes section it re-checks.** When a `*-diagnostics/FINDINGS.md` re-tests a class from `*-notes.md`, the FINDINGS verdict is the current one; the notes stay as the record of the sweep that produced the row counts.
- **Read-only upstreams.** Nothing here writes to `lib/stdlib/test` or the Khepri checkout; probes are compiled into `$TMPDIR` and the staged suite beams live under `$TMPDIR/erts-suites-stage/`.
- Probes are plain `.erl`/`.sh` (no crate, no build step) so they can be run by hand against any runtime.

## Routing Table
- `stdlib-diagnostics/` → hand-run probes re-checking the stdlib divergence classes D1…D11 against the current runtime, with `FINDINGS.md` as the verdict table (`D11` = the `erlang:apply/2` defect behind `queue_SUITE:oops`).
- `khepri-diagnostics/` → hand-run probes behind `khepri-notes.md` (`process_flag/2`, `external_size`, iodata handling).
- `stdlib/` → the stdlib sweep artifact (`report.json` + `summary.md`) and its `raw/` probe captures.
- `khepri/` → the Khepri sweep artifact (`report.json` + `summary.md`).
