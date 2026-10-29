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
  [--runtime sys|rust|all] [--timeout <secs>] [--jobs <n>] [--out <dir>] [--out-file <path>]
  [--json] [--verbose] [--rebuild] [--khepri-src <dir>] [--stdlib-src <dir>]`
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
  `CASE <name> OK|SKIP|EXC|TIMEOUT|MISSING`. Absent `init_per_group`/`end_per_group`/
  `init_per_testcase` hooks are deliberate: a case needing them fails identically on both sides.
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
- Harness complete: 22 unit tests green, zero build warnings; both targets run end to end.
- Measured numbers, the excluded-case rationale and the root-cause classes live in
  `RESULTS.md` (artifacts under `results/`); do not duplicate counts here.

## Routing Table
- `data/` → the committed Erlang runner `ct_runner.erl` (embedded with `include_str!`).
- `results/` → committed sweep artifacts (`report.json` + `summary.md` per target) — raw evidence
  behind `RESULTS.md`.
- `src/` → the driver: `cli`/`run` (dispatch), `runtime`/`proc`/`target` (resolution, timed
  subprocesses, staging, `par_map`, the identical-invocation helper), `eunit` (summary parser),
  `khepri`/`stdlib` (the two targets), `report` (row/verdict model, JSON + Markdown), `json`.
