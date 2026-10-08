# Tooling (`tools/`) — Differential Gate & Benchmark Driver

## Intent
The two in-repo tooling crates that make the Rust ERTS rewrite **verifiable** and **measurable**:
- `differential/` — the **differential-test compatibility gate** (Stage 0, Part II §H; the safety net for everything else, per Part I §G.7 "no compatibility regressions"). For a case it runs the real system ERTS (the **oracle**) **and** the Rust runtime (`beam_rt`), then compares the observable results.
- `bench/` — the **benchmark driver** (Part I §G.7/§I success metrics: startup latency, p99 tail latency, throughput on named benchmarks, memory footprint). It drives the external target projects named in `../../ERTS_RUST_REWRITE_TEST_PROJECTS.md` and **always** records the system-ERTS baseline.

Neither tool modifies the runtime or the external repos — both are pure drivers/observers.

## API Surface (CLI)
Both crates ship a **library + a binary**: the library exists so `cargo test` can assert the harness itself (against the oracle / against fixture binaries), and the binary is what a human or CI drives. `--help` must be usable on every subcommand.

### `differential`
- `differential run <case-id> [--json] [--out <dir>]` — run one built-in conformance case both ways; print the comparison.
- `differential compare <expr> [--pa <dir>] [--json] [--out <dir>]` — ad-hoc: evaluate an Erlang expression under both runtimes and compare.
- `differential suite [--strict] [--json] [--out <dir>]` — run the built-in conformance corpus.
- `differential list [--by-contract] [--json] [--out <dir>]` — list corpus case ids + the compatibility contract each covers (`--by-contract` adds a per-contract case-count rollup).
- Exit codes: `0` all executed cases agreed (skips/unsupported reported, not fatal); `1` ≥1 **mismatch**; `2` usage/env error. `--strict` escalates `unsupported`/`skipped` to a failure exit.

### `bench`
- `bench list [--json]` — list workloads + their prerequisite-probe status.
- `bench run [--quick|--full] [--workloads <ids,csv>] [--runtime <sys|rust|all>] [--out <dir>] [--out-file <path>] [--repo <id>=<path>] [--json] [--verbose]` — run workloads, write results JSON.
- `bench baseline [--quick]` — measure the system-ERTS baseline only.
- `bench compare <a.json> <b.json> [--json]` — diff two result sets.
- Exit codes: `0` run completed (per-workload `skipped(reason)` is not fatal); `2` usage/env error. A workload that *errors* is recorded `failed` in JSON.

## Shared Conventions (binding on both crates)
- **Runtime resolution — documented, overridable, no baked absolute paths:**
  - **Oracle** = `$ERTS_ORACLE` if set, else `erl` resolved on `PATH`. Missing ⇒ loud env error (exit 2).
  - **Rust runtime** = `$BEAM_RT` if set, else the workspace-relative `target/{release,debug}/beam_rt` (relative to `../..`). If neither exists ⇒ cases report `unsupported`.
  - **Case kind drives the argv.** *Eval* cases give both runtimes the **same argv** (`["-noshell", <extra…>, "-eval", <probe>]`); *Module* cases give them **per-side argv** — the Rust side runs `beam_rt run <file.beam> <mod>:<fun>/<arity> <ints…>` (`run` action), the oracle runs `erl -noshell -pa <dir> -eval …`. Either way `beam_rt` is a drop-in `erl` (contract A.6); only the *program path* is set, internals are never assumed.
- **Honest reporting:** a case/workload that cannot run is `unsupported` / `skipped(<reason>)` — **never** `pass`, and oracle output/metrics are never fabricated.
- **Machine-readable results:** JSON with a stable schema, written under the crate's gitignored `out/` dir (`differential/out/`, `bench/out/`) unless `--out` overrides.
- **No heavy new deps:** `serde`/`serde_json` (already workspace deps) for JSON; **hand-rolled `std`-only arg parsing** (no `clap`) to stay offline-safe. Failures are loud (nonzero exit / explicit status), never silently swallowed.
- Files target ~1000 lines; split by responsibility. Apache-2.0.

## Constraints
- **Do not couple to `crates/*` internals.** The only contract with `beam_rt` is its documented external CLI (A.6). `beam_rt` is a working runtime: a real `-eval` evaluator (`erl_eval`-style tree-walk over the `beam_bifs` `erlang` registry), a global-atom code server (`code:*`/`init:*` native floor), native `os:cmd/1`, and a `run` action that executes real `erlc`-compiled `.beam`s.
- **The two `beam_rt` capability-gap signals — both must classify as `unsupported`, never `mismatch`, and both tools must never crash on them:**
  1. **escaped** — the capability-gap raise escaped the program, so the CLI exits `3` and prints exactly one `beam_rt: unsupported: <reason>` stderr line (an out-of-grammar `-eval` expression, an `unsupported_opcode`/`unsupported_bif` reason);
  2. **probe-caught** — a probe-wrapped (eval) case's probe *caught* the raise and printed it as its single `exception <class> <reason>` stdout line, exiting `1`, where `<reason>` (after stripping one pair of surrounding `'…'` quotes) follows the runtime's documented `unsupported` reason convention (`unsupported`, `unsupported: X`, `unsupported_opcode: …`, `unsupported_bif`).
  The probe-caught classification is **deliberately narrow**: only the documented `unsupported` prefix (with a following `:`/`_`/whitespace, or nothing) triggers it, so a genuine Erlang reason (`badarg`, `system_limit`, `function_clause`, `'erlang:open_port/2 is not a BIF'`, …) stays an *observation* ⇒ `mismatch`.
- `bench` investigates the external repos **read-only** (`/home/bill/Source/{elixir,cowboy,khepri,emqx,Programming-Language-Benchmarks}`): it may build/run them but must **not** commit into them.
- Keep default runs fast (`--quick`); full suites are milestone-only (Part I §C; root §6).
- `unsafe` is forbidden in both crates.

## Status
- **Compatibility gate:** the corpus covers all nine contracts A.1–A.9 — `summary: total=794 agree=785
  mismatch=0 unsupported=9 skipped=0` (per-contract counts under "Gate coverage" in Notes for Agents).
  `wire/new_fun_ext_encode` is unclosable by design; the other eight are the five `erlang:statistics/1`
  counters the runtime does not serve and the two `erlang:monitor/2,3` gap shapes.
- **`beam_rt` gap signalling is honoured:** cases the runtime cannot run classify as `unsupported`,
  never `mismatch`.
- **CI:** `.github/workflows/erts_rust.yml` builds/tests the workspace and runs `differential/gate.sh`
  (exit-code policy in Notes for Agents); `bench` stays a manual/opt-in command.
- **Benchmarks:** current numbers and their caveats live in `bench/RESULTS.md` (authoritative; raw JSON
  under `bench/results/`); the driver is read-only against the external repos. The runtime-side hot-path
  figures (registered-BIF dispatch ~116 ns/call for a non-inlinable BIF vs ~10.6 µs before the dispatch
  handles were gated; inlinable `gc_bif` arithmetic at ~2.5 ns/iter) live in `../crates/beam_rt/CONTEXT.md`,
  measured via `beam_rt`'s `examples/perf_bif.rs`.

## Routing Table
- `differential/` → differential-test compatibility gate: `Eval`/`EvalEnv` cases (shared argv) + `Module` cases (per-side argv, `erlc`-compiled, optional `cc`-built NIF/driver `.so`), oracle runner, Rust-runtime runner (`run` action), observable-result comparison (**stdout bytes + exit status always; stderr classified for `Eval` cases only** — the `term_to_binary` / exception-class+reason / `system_info` observables are encoded into stdout by the probe wrapper, not separate diff fields), the two-signal ⇒ `unsupported` classification (escaped exit-`3` **and** probe-caught `exception` reason), built-in conformance corpus (embedded Rust registry, 780 cases), JSON reports
- `bench/` → benchmark driver: pluggable workload plugins (Elixir compiler, PLB kernels, Cowboy, Khepri, EMQX), system-ERTS baseline, result store + `compare`

## Notes for Agents
- Build/test via the committed wrapper (read-only `$HOME`): from this dir `../scripts/cargo.sh test -p differential` / `../scripts/cargo.sh test -p bench`; from the repo root add `--manifest-path erts_rust/Cargo.toml`. The cache is shared across worktrees; known deps (`serde`, `serde_json`, `libc`) resolve offline — avoid adding new crates.
- Oracle is the system Erlang on `PATH` (OTP 29.1.1 / erts-17.1). Verified: `erl -noshell -eval 'io:format("~w~n",[term_to_binary({ok,42})]), halt().'` → `<<131,104,2,119,2,111,107,97,42>>`.
- **Oracle = the system `erl`, never the in-tree `erts/` C build** (`differential/src/runtime.rs::resolve_oracle`: `$ERTS_ORACLE` else `erl` scanned on `$PATH`).
- **Gate coverage (verified against code):** `differential suite` reports `summary: total=794 agree=785
  mismatch=0 unsupported=9 skipped=0` (a case may declare several contracts, so the per-contract totals
  below exceed `summary.total`). All nine contracts A.1–A.9 have corpus cases — A.1 154/154 · A.2 176/176
  · A.3 34/34 · A.4 64/64 · A.5 79 (78 agree + 1 unsupported) · A.6 44/44 · A.7 322 (315 agree + 7
  unsupported) · A.8 64/64 · A.9 66 (65 agree + 1 unsupported). A.4 (driver C ABI) is covered end to end by the `driver_c` group
  (a real C driver `.so` driven through the port lifecycle and the `port_command` echo round trip),
  alongside the built-in spawn-driver `driver/` cases and the structural C-ABI check (`tests/abi.rs`). The
  genuine end-to-end agreement core is the module-run groups (`modules`/`mexec`/`nif_so`/`nif_res`/`driver_c`/`records`).
  Two contracts stay shallow: A.5 (the `wire` group covers the *ETF wire codec* only — a live handshake /
  `DOP_*` loopback needs two connected nodes and is not expressible through a single CLI process) and A.9
  (build) through *boot/version observability* only (no self-hosted build). The 9 `unsupported` are
  `wire/new_fun_ext_encode` (an `erl_eval` lambda's closure identity, unclosable without porting
  `erl_eval`/`erl_scan`/`erl_parse`), the five `erlang:statistics/1` counters the runtime does not serve
  (`proc_signals/statistics_*_unsupported`) and the two `erlang:monitor/2,3` gap shapes
  (`proc_signals/monitor_3_tag_option_unsupported`, `proc_signals/monitor_port_unsupported`).
- **CI wiring:** `.github/workflows/erts_rust.yml` (path-triggered on `erts_rust/**`) builds and tests the
  workspace and runs `differential/gate.sh`, honouring its exit-code policy (`1` mismatch / `2` env error
  fail the job; `3` unsupported-only is non-blocking). `bench` remains a manual/opt-in command.
- **Other oracle-backed harnesses live outside this node** (each spawns the system `erl` directly, not
  through this crate): `../crates/beam_codec/tests/interop.rs` (ETF codec), `../crates/beam_ets/tests/differential.rs`
  + `../crates/beam_ets/tests/differential/battery/*` (the ETS battery), `../crates/beam_dist/tests/dop_roundtrip.rs`,
  `../crates/beam_engine/tests/stdlib_sweep.rs`, `../crates/beam_loader/tests/real_modules.rs`.
- **`bench` target gaps in this sandbox:** `plb` (Benchmarks-Game, needs `dotnet`) and `emqx` (no root `rebar.config` — it is a `make`/`mix` project) are wired but report honest `skipped(<reason>)`; `khepri` needs `rebar3`, available only via the opt-in `$BENCH_TRY_NIX`.

## See Also
- `../CONTEXT.md` §6 (test strategy) and §"Notes for Agents" — the differential gate and the external targets.
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §H (Stage 0 mandates the harness).
- `../../ERTS_RUST_REWRITE_EVALUATION.md` §G.7 / §I (success metrics, differential suite gate).
- `../../ERTS_RUST_REWRITE_TEST_PROJECTS.md` — the concrete benchmark targets + build/bench entry points.
