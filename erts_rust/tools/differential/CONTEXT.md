# differential — Rust ERTS vs System ERTS Compatibility Gate

## Intent
The in-repo **differential-test harness** (Stage 0; `../../../ERTS_RUST_REWRITE_DEEPDIVE.md` §H).
For a given Erlang expression (or module call) it runs the **real system ERTS** (the *oracle*) **and**
the **Rust runtime** (`beam_rt`) with **identical argv**, then compares the **observable results**.

This is the *compatibility gate* of the rewrite (`../../../ERTS_RUST_REWRITE_EVALUATION.md` §G.7/§I):
no behavior change ships without a differential case. It is a pure **driver/observer** — it never
modifies the runtime and never inspects its internals. The only contract with `beam_rt` is its
documented external CLI (compat contract **A.6**). It honors every shared convention in `../CONTEXT.md`.

Cargo **binary + library** crate named `differential`: the library exists so `cargo test` can assert
the harness itself (oracle golden checks, oracle-vs-oracle equality, diff detection); the binary is
what a human or CI drives.

## Observable results (what is compared)
Per case, each runtime is observed identically and compared field-by-field:
1. **stdout bytes** (exact, byte-wise).
2. **exit status** (process exit code; `None` if signalled).
3. **`term_to_binary/1` of the evaluated result** — the harness *encodes the result* it observes,
   so the canonical ETF bytes are a first-class diff field (contract A.2).
4. **exception class + reason** — surfaced because `erl -noshell -eval` prints **nothing** on an
   uncaught error; the harness always wraps the expression in `try … catch` (see *Probe protocol*).
5. **selected `erlang:system_info/1` keys** — covered by corpus cases (contract A.8).

## Probe protocol (binding)
A case carries a plain Erlang **expression**. The harness wraps it into a self-terminating probe:

- **`Probe::TermToBinary`** (default): `try (EXPR) of V -> io:format("ok ~w~n",[term_to_binary(V)]), halt(0) catch C:R -> io:format("exception ~w ~w~n",[C,R]), halt(1) end.`
- **`Probe::Value`**: same shape but prints `~w` of `V` (textual rendering, no ETF encode).

Success ⇒ one stdout line `ok <repr>`, exit `0`. Failure ⇒ one line `exception <class> <reason>`,
exit `1`. The probe is the *only* thing the runtime sees; the harness never parses internals, only
this line. Cases may add **extra argv** (e.g. `-<flag> <value>`) to exercise CLI flag parsing (A.6).

Invocation argv is built once and used for **both** runtimes (drop-in `erl`, A.6):
`["-noshell", <extra…>, "-eval", <probe>, ]` (plus any `-pa <dir>` for module cases).

## API Surface

### CLI (hand-rolled `std`-only arg parsing — **no `clap`**; `--help` usable on every subcommand)
- `differential run <case-id> [--json] [--out <dir>]` — run one corpus case both ways; print the comparison.
- `differential compare <expr> [--pa <dir>]… [--json] [--out <dir>]` — ad-hoc: evaluate an expression
  (or a module call with `-pa` code paths) under both runtimes and compare.
- `differential suite [--strict] [--json] [--out <dir>]` — run the whole built-in corpus.
- `differential list [--json]` — list corpus case ids + the contract each covers.
- `differential --help` / `-h`; `differential <sub> --help`.
- **Exit codes:** `0` all executed cases agreed (skips/`unsupported` reported, not fatal);
  `1` ≥1 **mismatch**; `2` usage/env error. `--strict` escalates `unsupported`/`skipped` to failure.

### Library (`src/lib.rs`) — the public API `cargo test` asserts against
- `error` — `Error` (`Usage`/`Env`/`Io`, all → exit `2`), `Result<T>`.
  Tool-level errors are host-tool errors; BEAM exceptions are *observed **data*** (never the
  harness's own failure mode), so `beam_common::BeamError` does not model this crate's failures.
- `cli` — `Command` enum (`Run{case_id,…}`, `Compare{expr,pa,…}`, `Suite{strict,…}`, `List{…}`,
  `Help{…}`), `parse(args)`, `usage(topic)`.
- `runtime` — `RuntimeId::{Oracle,Rust}`, `RuntimeSource::{Env,Path,WorkspaceDefault}`,
  `RuntimeSpec{id,program,source}`, `Observation{runtime,program,argv,stdout,stderr,exit_code}`,
  `resolve_oracle()`, `resolve_rust() -> Ok(Option<RuntimeSpec>)`, `eval_argv(expr,extra)`, `run(spec,argv)`.
- `compare` — `SideResult::{Observed(Observation),Unsupported{reason}}`, `FieldDiff{field,oracle,rust}`,
  `GoldenCheck{matched,diffs}`, `Verdict::{Agree,Mismatch,Unsupported,Skipped}`,
  `compare_observations(&Oracle,&Rust) -> Vec<FieldDiff>`, `check_golden(&Observation,&Golden) -> GoldenCheck`.
- `corpus` — `Contract` (A.1–A.9 ids + labels), `Probe`, `Golden{stdout,exit_code}`, `Case{id,
  description,contracts,expr,extra_argv,probe,golden}`, `cases()`, `find_case(id)`, `probe_expr(expr,probe)`.
- `harness` — `run_case(&Case) -> CaseOutcome`, `compare_expr(expr,pa) -> CaseOutcome`,
  `run_suite(strict) -> SuiteOutcome`; each computes the `Verdict` and the process exit code.
- `report` — `CaseReport`, `RunReport` (serde, stable `schema: "differential/1"`), `write_json`,
  `render_human`.

## Runtime resolution (shared convention; no baked absolute paths without an overridable default)
- **Oracle** = `$ERTS_ORACLE` if set, else `erl` resolved on `PATH`. Missing ⇒ loud `Error::Env` (exit 2).
- **Rust runtime** = `$BEAM_RT` if set (must exist, else `Error::Env`), else the workspace-relative
  default `<CARGO_MANIFEST_DIR>/../../target/{release,debug}/beam_rt`. Neither present ⇒ the Rust side
  reports `unsupported` (never a crash, never a fabricated pass).
- Both runtimes are invoked with **the same argv** as `erl`; only the program path differs.

## Constraints
- **Honest reporting:** a side that cannot run is `unsupported` / `skipped(<reason>)` — **never** `pass`,
  and oracle observations are **never** fabricated. A case whose *oracle* disagrees with its golden is
  itself a failure (corpus drift), surfaced as a mismatch — the harness is self-testing.
- **No coupling to `crates/*` internals** (crate deps are `serde` + `serde_json` only). No `clap`; arg
  parsing is hand-rolled `std`. `unsafe` is **forbidden**.
- **Loud failures:** stderr messages + nonzero exit; never silently swallow an error.
- Machine-readable JSON, stable schema, written under the gitignored `out/` (or `--out <dir>`).
- Files target ~1000 lines; split by responsibility. Apache-2.0.
- Keep the default corpus **small + fast** (smoke level); heavy suites are milestone-only (root §6).

## Design Decisions
- **Probe wrapper is mandatory** because `erl -noshell -eval '<expr with uncaught error>'` emits *no*
  stdout and exits 1 — without the wrapper, exceptions would be invisible to a stdout diff.
- **Golden per case** lets the harness self-test: today the oracle reproduces the golden while the Rust
  side (`beam_rt` is a version-banner **stub**) reports `unsupported`. If the oracle ever stops matching
  its golden, that is a real failure (drift), not a skip.
- **Volatile `system_info` keys use structural goldens** (`is_integer(process_count)` → `true`,
  `is_list(otp_release)` → `true`) and `version` → `<<"17.1">>` (the frozen erts target). `otp_release`
  is deliberately **not** asserted by exact value: the oracle is OTP 29 while the rewrite targets OTP 30,
  so an exact string would be a guaranteed false mismatch once the Rust runtime lands.
- **`--strict`** exists so CI can forbid `unsupported` once the Rust runtime is functional; the default
  keeps today's stub-driven `unsupported` non-fatal.
- **A golden is the probe's stdout *line*, without the `~n` line break**: the probe always prints
  exactly one line, so `compare::check_golden` normalizes the observation with
  `normalize_probe_stdout` (strips one trailing `\n`, and a preceding `\r`) before comparing it to
  `Golden::stdout`. This keeps a golden diff readable instead of hiding a newline inside it.
- **Reports are always written** (`--out <dir>` sets the directory, default `out/`); `--json` only
  decides what goes to stdout. An ad-hoc `compare` asserts no golden, so its report carries
  `matched_golden: null` (the Rust side always does).

## Known Issues / Notes for Agents
- **`beam_rt` is currently a stub** that only prints a version banner (see `../../crates/beam_rt`); it
  cannot execute cases, so every Rust-side observation is `unsupported` today. This is expected and
  must never be reported as agreement.
- Build/test via the committed wrapper (read-only `$HOME`):
  `../../scripts/cargo.sh test --manifest-path ../../Cargo.toml -p differential`.
  Known deps (`serde`, `serde_json`) resolve **offline** — add no new crates.
- Verified oracle (system Erlang on `PATH`, OTP 29.1.1 / erts-17.1): `{ok,42}` under the default probe
  ⇒ `ok <<131,104,2,119,2,111,107,97,42>>`, exit 0.

## Test Strategy
- `#[cfg(test)]` in `cli` (arg parsing + `--help`), `compare` (pure diff + golden logic; differing
  fixtures ⇒ `Mismatch`; a fabricated `-pa`-style case exercising `Verdict`), `corpus` (unique ids,
  every case has ≥1 contract, goldens non-empty).
- `tests/` integration: **oracle-backed** — the oracle reproduces the `{ok,42}` golden
  (`ok <<131,104,2,119,2,111,107,97,42>>`); oracle-vs-oracle on the same case ⇒ `Verdict::Agree`;
  a Rust runtime forced to a nonexistent path ⇒ `unsupported` (never `pass`). These **skip with a loud
  note** (not a silent pass) when `erl` is unavailable, so `cargo test` stays honest everywhere.
- Gate: `cargo test -p differential` GREEN and `differential suite --json` runs end-to-end (Rust side
  `unsupported`, oracle vs golden agreement).

## Routing Table
- `src/lib.rs` → crate docs, module wiring, public re-exports (the public API surface)
- `src/error.rs` → `Error` (`Usage`/`Env`/`Io`) + `Result` + exit-code mapping
- `src/cli.rs` → `Command`, hand-rolled `parse`, `--help`/usage text
- `src/runtime.rs` → runtime resolution (`$ERTS_ORACLE`/`erl`, `$BEAM_RT`/target), `Observation`, argv build, process spawn
- `src/compare.rs` → observable diff, golden check, `Verdict`
- `src/corpus/mod.rs` → `Case`/`Contract`/`Probe`/`Golden` + registry (`cases`, `find_case`, `probe_expr`)
- `src/corpus/etf.rs` → `.beam`/ETF cases (A.2): `term_to_binary` bytes, `binary_to_term` round-trip
- `src/corpus/terms.rs` → language-semantics cases (A.1): term ordering/equality, arithmetic
- `src/corpus/bifs.rs` → BIF/OTP-app cases (A.7): `length`, `integer_to_binary`, `lists:reverse`
- `src/corpus/system_info.rs` → introspection cases (A.8): `system_info/1` keys
- `src/corpus/flags.rs` → CLI/config cases (A.6): `-noshell` + custom `init:get_argument` flags
- `src/harness.rs` → orchestration: `run_case`/`compare_expr`/`run_suite`, exit-code derivation
- `src/report.rs` → JSON report schema (serde) + writer + human rendering
- `src/main.rs` → thin binary: parse args → dispatch → `process::exit(code)`
- `out/` → gitignored JSON report output (default `--out`)

## See Also
- `../CONTEXT.md` — the shared conventions this crate must follow (runtime resolution, JSON, exit codes).
- `../../CONTEXT.md` §6 / §7 — the differential gate is the compatibility gate (Stage 0).
- `../../../ERTS_RUST_REWRITE_DEEPDIVE.md` §H — Stage 0 mandates this harness.
- `../../../ERTS_RUST_REWRITE_EVALUATION.md` §G.7 / §I — differential suite = the success-metric gate.
- `../../../ERTS_RUST_REWRITE_TEST_PROJECTS.md` — the contracts the corpus must exercise.
