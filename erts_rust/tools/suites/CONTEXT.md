# `suites/` — real third-party / OTP test-suite driver

## Intent
Measure **someone else's** test suite under both runtimes and record honest per-module /
per-case pass counts for both sides. This is the "widen suite coverage cheaply" companion to
`../differential/` (hand-written conformance cases) and `../bench/` (performance): instead of
writing cases, run a real suite that already has thousands of assertions.

Two targets:
- **khepri** — a third-party project (`/home/bill/Source/khepri`, `$KHEPRI_SRC`), 44 eunit
  modules. The checkout is **read-only**: it is copied into
  `$TMPDIR/erts-suites-stage/khepri/proj` and `rebar3 as test compile` runs only in the copy.
- **stdlib** — OTP's own `common_test` suites under `lib/stdlib/test` (`lists_SUITE`,
  `maps_SUITE`, `proplists_SUITE`, `dict_SUITE` by default), driven per case.

Both sides get the **identical** `-pa` set and the **identical** `-eval` expression; the only
difference is the binary (`erl` vs `beam_rt`).

## API Surface (CLI)
- `suites list [--json]` — resolved runtimes (with a live self-check), target sources, defaults.
- `suites run [--target khepri|stdlib|all] [--suite <names>] [--module <names>]
  [--runtime sys|rust|all] [--timeout <secs>] [--suite-budget <secs>] [--jobs <n>] [--out <dir>]
  [--out-file <path>] [--json] [--verbose] [--rebuild] [--khepri-src <dir>] [--stdlib-src <dir>]`
  — `--timeout` is the per-module/per-case budget, `--suite-budget` the whole-suite one (stdlib);
  there is no single-case selector, narrow with `--suite`/`--module`.
- `suites help`
- Exit codes: `0` the sweep completed (per-row failures are **data**, not harness errors);
  `1` the harness failed (I/O, a target that could not be staged/compiled); `2` usage or
  environment error (no oracle, bad flag).
- Artifacts: `<out>/report.json` (stable schema `suites/1`) + `<out>/summary.md`; default
  `--out` is this crate's `results/`.

## Observation model
- **eunit** rows: one row per module, the drive is `eunit:test('<module>',[verbose]), halt(0).`
  and the counts come from eunit's own summary (`N tests passed.` / `Failed: F. Skipped: S.
  Passed: P.`). `no_tests` (helper module or a `common_test` suite such as `cluster_SUITE`) and
  `cancelled` (setup crash, missing module — the summary then reads `Passed: 0`) are **their own
  statuses**, never a 0/0 pass.
- **ct** rows: one row per `Suite:Case`. `data/ct_runner.erl` expands `Suite:all()` through
  `groups/0`, runs every case in its own `spawn_monitor` with a timeout, and prints
  `CASE <name> OK|SKIP|EXC|TIMEOUT|MISSING` (every protocol line is `\n`-prefixed, so a case that
  prints without a trailing newline cannot glue onto the next line). Absent
  `init_per_group`/`end_per_group`/`init_per_testcase` hooks are deliberate: a case needing them
  fails identically on both sides. When the suite budget expires the runner stops launching and
  prints `SUITE_DEADLINE <not-run>`, and the unlaunched cases are reported *not measured*, never as
  per-case timeouts or failures. Suites are compiled together with the helpers they import
  (`error_info_lib`, `dict_test_lib`, `binref`, `random_iolist`, `random_unicode_list`,
  `sets_test_lib`); a helper that fails to compile is not fatal.
- **Verdicts**: `agree` · `agree-fail-identical` (both sides fail the same way: excluded from
  both numerators and reported as such) · `divergence` (the parity miss) · `rust-gap` (the Rust
  side could not run it: typed `unsupported`, timeout, unparsed output) · `oracle-only-fail` ·
  `not-run`.
- Every row names both sides and the artifact records the runtime's git sha, the exact build
  command, representative invocations, and the host loadavg before/after.

## Constraints
- **Read-only upstreams.** Khepri and `lib/stdlib/test` are only ever read; the staged copy in
  `$TMPDIR` is what gets built and run.
- **No wall-time claims.** The host is shared: published numbers are pass/fail counts and
  ratios, never portable wall times (`RESULTS.md` says so explicitly).
- **`rebar3` is not on `$PATH`** in this sandbox: resolved from `$REBAR3`, `$PATH`, then the
  Nix store. `HOME`, `REBAR_CACHE_DIR`, `XDG_*` and `HEX_HOME` are redirected into the stage
  (the real `$HOME` is read-only); hex is reachable, so the `test` profile deps are fetched once
  and reused by later runs.
- **Per-module working directory.** khepri's tests create `_test.<name>` directories relative
  to the cwd, so each module (and each stdlib suite) runs in `<stage>/cwd/<name>`; both sides of
  one row share that cwd, which is what keeps the comparison fair and the repo clean.
- **`beam_rt` absence is not a failure**: every Rust cell becomes `not_run(runtime-missing)`.
- Dependency-free and `#![forbid(unsafe_code)]`; hand-rolled JSON writer and arg parsing; files
  target ~1000 lines.

## Notes for Agents
- This crate is a **standalone workspace** (`[workspace]` in `Cargo.toml`), deliberately not in
  `erts_rust/Cargo.toml`'s member list: `.agents`-level rules for this deliverable restrict
  writes to `erts_rust/tools/`. Folding it in is a one-line change for a parent agent
  (`"tools/suites",` under `# --- Tooling ---`); it needs nothing from the registry, so the
  change cannot disturb `Cargo.lock`.
- Build/test: `CARGO_HOME=$HOME/.cache/cargo-home cargo test` **in this directory** (the
  workspace wrapper `../../scripts/cargo.sh` is not needed — there is no workspace manifest to
  resolve, and `cargo` picks up `rust-toolchain.toml` by walking up).
- A full sweep: `./target/debug/suites run --jobs 4` (stdlib is cheap; khepri's first run pays
  the `rebar3` build, ~1–2 min, then reuses it — `--rebuild` forces a fresh build).
- Iterating cheaply: `--module <name>` / `--suite <name>`, `--target khepri|stdlib`.
- `--jobs` only changes wall time: every module/case is its own OS process in its own cwd, and
  `par_map` reassembles results in input order, so reports are stable.
- The oracle is the system `erl` on `$PATH` (here OTP 29.1.2 / erts-17.1.1, while the tree pins
  OTP 30.0-rc0 / erts-17.1) — **never** the in-tree `erts/` C build.

## Status
- Harness complete: 29 unit tests green, zero build warnings; both targets run end to end against a
  real oracle and a real `beam_rt`.
- Both sweeps are measured at the current runtime (runtime source commit `71ffbbb379`, tier `default`;
  no file under `erts_rust/crates` differs between it and HEAD): Khepri `1500/1` oracle vs `403/1` rust
  (21 `agree` / 8 `agree-fail-identical` / 2 `divergence` / 20 `rust-gap`), stdlib at the 10 s budget
  `209/5` oracle vs `177/37` rust (177 `agree` / 5 excluded / 32 `divergence`), and the 120 s re-check
  `129/1` vs `114/16`.
  Measured numbers, the excluded-case rationale and the root-cause classes live in `RESULTS.md`
  (artifacts under `results/`); do not duplicate counts here. The per-row analysis — smallest failing
  expression per class, both sides' observations, owning crate — lives in
  `results/{khepri,stdlib}-notes.md` next to the artifacts, and each class's current verdict in
  `results/{khepri,stdlib}-diagnostics/FINDINGS.md`, which supersedes a notes section it re-checks.
- Each row's cwd is cleared before it runs (`target::fresh_dir`): a staged `_test.*`/`data_dir` tree
  left by an earlier sweep made fixtures fail on **both** sides, which silently reddened green rows.
- The sweeps surface **real runtime defects**, and some are not a typed gap: khepri's nested-run pump
  block (RC-E, 20 rows) and a SIGSEGV on a stale compiled match spec (RC-F, 1 row) are khepri rows; on the
  stdlib side D1c's placeholder raise reason is an observable wrong value. Recording that is this crate's
  job; fixing any of it belongs to the owning crate, not here.

## Routing Table
- `data/` → the committed Erlang runner `ct_runner.erl` (embedded with `include_str!`).
- `results/` → committed sweep artifacts (`report.json` + `summary.md` per target, plus the stdlib
  `recheck-120/` budget re-check), the per-row analysis (`khepri-notes.md`, `stdlib-notes.md`) and the
  diagnostics behind them — raw evidence behind `RESULTS.md`.
- `src/` → the driver: `cli`/`run` (dispatch), `runtime`/`proc`/`target` (resolution, timed
  subprocesses, staging, `par_map`, the identical-invocation helper), `eunit` (summary parser),
  `khepri`/`stdlib` (the two targets), `report` (row/verdict model, JSON + Markdown), `json`.
