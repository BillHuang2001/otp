# Rust ERTS Rewrite — Workspace Root (`erts_rust/`)

## Intent
A from-scratch, memory-safe Rust reimplementation of **ERTS** (the Erlang/BEAM runtime) targeting
**full drop-in compatibility** with Erlang/OTP (release `30.0-rc0`, runtime `erts-17.1`), higher
robustness, a modern codebase, and improved performance (speed + memory).

This workspace is the *new* ERTS core. The legacy C/C++ tree under `../erts/` is retained as
(a) the **differential-test oracle** and (b) the `FLAVOR=emu` bootstrap fallback; it is removed
**module-by-module** as Rust parity is proven by the differential harness. Major structural changes
are recorded in `../CHANGE.md`.

Design source of truth (must stay faithful):
`../ERTS_RUST_REWRITE_EVALUATION.md` (Part I) and `../ERTS_RUST_REWRITE_DEEPDIVE.md` (Part II),
plus `../ERTS_RUST_REWRITE_TEST_PROJECTS.md` for the concrete benchmark/test targets.

## Architectural Contracts (binding on every child node)

### 1. The nine compatibility contracts (Part I §A) — all must be honored
A.1 language semantics · A.2 `.beam` binary/asm · A.3 NIF C ABI · A.4 driver C ABI ·
A.5 distribution/ETF wire · A.6 CLI/configuration · A.7 OTP app contracts (preloaded/boot/`erts_internal`/`prim_*`) ·
A.8 introspection/tooling · A.9 build/bootstrapping. Only A.2 differs between Level 1 and Level 2;
the other eight are identical, so they are reproduced **exactly**.

### 2. Term representation is FROZEN (Level-1 drop-in)
`Eterm` is one machine word; 2-bit primary tag (`HEADER/LIST/BOXED/IMMED1`); 3-bit pointer tag on
64-bit (literal-area bit). Small ints are 60-bit immediates; atoms are 6-bit-shifted immediates;
a cons cell is a header-less 2-word `[CAR,CDR]` tagged `TAG_PRIMARY_LIST`. These are **owned by
`crates/beam_term`** and are the lowest common ancestor for term operations — no other crate may
assume a different layout. Interior layouts that are *not* externally pinned (list chunking, map
flatmap/HAMT/CHAMP thresholds, ETS tree shape) are open and documented at their node.

### 3. Error strategy — no silent swallowing
- BIF/NIF failure is **return-based** (no `setjmp`/`longjmp`). Model as `Result<T, BeamError>` at
  Rust boundaries; the engine raises via the BEAM exception mechanism (exception tag in `x(0)`).
- `beam_common` defines `BeamError`, `Result`, and the exception-reason encoding. Every crate uses it.
- `unsafe` is confined to explicitly-marked modules (term heap, GC, JIT, FFI); each carries a
  `// SAFETY:` rationale and a Miri/fuzz or differential test where feasible.

### 4. Execution-engine IR seam (Stage 1)
`beam_loader` lowers `.beam` → a **typed, block-structured IR** (BEAM types + safepoints +
failure/branch semantics + catch info + line/ranges tables). Backends consume only the IR:
`interpreter` (direct-threaded), `jit_beamasm` (Rust port of BeamAsm + block register allocation),
`jit_cranelift` (optional hot tier with bounded, off-critical-path compile + cheap deopt).
Root-set contract: `x_reg_array[0..Live)` + the whole Erlang stack; **no frame liveness maps**.
Compile latency is a first-class correctness constraint (Part I §C).

### 5. GC / memory model (Stage 4, Part II §D)
Per-process **generational semi-space Cheney copying**, barrier-free (immutability ⇒ no write
barrier), off-heap refcounted large objects (MSO). Reject MMTk/Boehm. GenImmix is a
**measurement-gated** mature-generation option only.

### 6. Test strategy (enforced per crate)
- Unit + integration tests; `cargo test --workspace` must stay green.
- `tools/differential` is **the compatibility gate**: run a module under the real system ERTS and
  under Rust ERTS and compare observable results (stdout, `term_to_binary`, exception reason,
  `system_info`). No behavior change ships without a differential case.
- `tools/bench` drives external targets (**Cowboy**, **Khepri**, **Elixir compiler**, Benchmarks-Game
  Elixir, **EMQX**) plus a system-ERTS baseline; measures startup latency, p99 tail latency,
  throughput, and memory. Full suites run only at milestones (iteration speed first).

### 7. Staging (Part II §H) → crate mapping
| Stage | Content | Crates / nodes |
|---|---|---|
| 0 Foundations | frozen ABI, ETF/dist, ABI, boot, CLI, differential harness | `beam_common`, `beam_term`, `beam_alloc`, `beam_codec`, `beam_dist`, `beam_abi`, `beam_rt`, `tools/differential` |
| 1 Execution engine | loader + typed IR + interpreter + JIT tiers | `beam_loader`, `beam_engine` |
| 2 Compiler | loop opts → fusion → FBIP (Erlang, `../lib/compiler`) | `../lib/compiler` (cross-cutting; runs on the existing SSA IR) |
| 3 Data structures | ETS B-tree/CHAMP, list interior, messaging, ETS copy-trimming | `beam_ets`, `beam_message`, `beam_term` |
| 4 GC modernization | port per-process copying GC to Rust; GenImmix (gated) | `beam_gc` |
| 5 Rust core | scheduler, processes, BIFs, I/O, drivers behind the C ABI | `beam_process`, `beam_bifs`, `beam_io` |
| 6 Level-2 clean-room | new ISA + toolchain (research track) | worked via `beam_loader`/`beam_engine`, own node when started |

## Constraints
- **No crate may break the frozen term layout or the nine contracts.** Deviations require a
  documented decision here and a `CHANGE.md` entry.
- **`beam_term` and `beam_common` are dependency sinks** (depend on nothing but `libc`/std) — they
  are the shared foundation; all other crates depend *downward* on them.
- **Never hand-edit generated artifacts.** `ops.tab`-style transforms and the `bif.tab` surface are
  source data; regenerate, don't patch output.
- Files target ~1000 lines; split by responsibility before growing past it.
- **Licensing: Apache-2.0** (matches OTP; `license = "Apache-2.0"` in every `Cargo.toml`). Every
  `*.rs` file under `erts_rust/` carries the OTP `// %CopyrightBegin% … // %CopyrightEnd%`
  Apache-2.0 header block; the repo scanner enforces it byte-exactly
  (`ERL_TOP="$PWD" escript scripts/license-header.es scan --path erts_rust`). Add the block to every
  new Rust source file (`//`-comment prefix, blank lines as bare `//`, one blank line before the
  code). External deps must be Apache-2.0/MIT only.
  `scan --path erts_rust` still reports the workspace's *non-`.rs`* files (the `CONTEXT.md` docs, other
  `.md`, `Cargo.toml`/`Cargo.lock`, `.erl`/`.c` test fixtures, generated `out/`/benchmark results) as
  header-less; only the `*.rs` sources are in scope for this requirement.

## Notes for Agents
- Iterate with `scripts/cargo.sh build|test --manifest-path erts_rust/Cargo.toml --workspace`
  (`--all-targets` for a full build) — a bare `cargo` from the repo root cannot resolve the workspace
  manifest. lld linker in a bare container, mold in the Nix devShell; `opt-level=1` dev, deps at
  `opt-level=2`.
- **Read-only `$HOME` in this sandbox:** cargo's default `CARGO_HOME` (`$HOME/.cargo`) is unwritable, so
  builds MUST set a writable one. Use the committed wrapper:
  `scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace`
  (it selects `$HOME/.cache/cargo-home`, falling back to `$TMPDIR/cargo-home`), or export
  `CARGO_HOME=/home/bill/.cache/cargo-home` before invoking `cargo`. The cache is shared across worktrees.
- **CI:** `.github/workflows/erts_rust.yml` is the only CI that touches this workspace (path-triggered;
  `main.yaml` never builds OTP or Rust together). Every Rust test and the compatibility gate therefore
  also run there on any `erts_rust/**` change; run them locally too before claiming parity.
- Do **not** run full external suites on every change — reserve them for milestones.
- The oracle runtime is the system Erlang (`ERTS_ORACLE` in the flake; `erl` on `PATH`, OTP 29/erts-17.1).
- Benchmark/test targets are the configured foreign repos: Cowboy, Khepri, EMQX, Elixir,
  Programming-Language-Benchmarks. Investigate them read-only; write through the task's tracked worktrees.

## Status
- **Crate tree complete:** 16 crates under `crates/` + 2 tooling crates under `tools/`, each with its
  own `CONTEXT.md`; all implemented and tested. Verify the node tree via the routing table below.
- **Build/test:** build with `scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace
  --all-targets` (**warning-free: 0 warnings**); test with `scripts/cargo.sh test --manifest-path
  erts_rust/Cargo.toml --workspace --no-fail-fast`. The suite is green — **0 failed** (`14 685 passed /
  99 ignored` across the crates + `bench`, with `tools/differential`'s multi-hour corpus-sweep ratchet
  tests excluded — see the caveat below). No test is red on account of the host oracle: the tree's frozen
  values (`system_info(version)` `17.1`, `stdlib` `8.1`, the 248-row instruction roster) are asserted
  unconditionally, and only the *oracle* leg tolerates a different `erts` patch, so an `erts-17.1.1`
  host is green. Two caveats: `tools/differential/tests/cli.rs` carries four end-to-end ratchet tests
  that each sweep the whole corpus serially (multi-hour, and pre-existing — the 12-way sweep below is
  the same check at ~9 min, so exclude them when iterating), and the crate-chunked route
  (`-p <crate>`) is how to keep an iteration loop short.
- **CI runs the workspace and the gate:** `.github/workflows/erts_rust.yml` (path-triggered on
  `erts_rust/**` and on the workflow file itself, plus `workflow_dispatch`) installs the pinned
  toolchain (`rust-toolchain.toml`), then runs `cargo build --workspace --all-targets`,
  `cargo test --workspace --no-fail-fast`, a **non-blocking** `cargo clippy`, and the differential gate
  `tools/differential/gate.sh` against an OTP oracle (`erlef/setup-beam`); with no `erl`/`erlc` on
  `PATH` the gate step warns and skips. `gate.sh` always passes `--strict` to the `differential`
  binary and defaults its allowlist to `tools/differential/known-gaps.txt`.
  Its exit-code policy: `0` every executed case agreed, or every gap is recorded + the allowlist is fresh
  → pass; `1` a real `mismatch` → fails the job; `2` env/usage error or an unparsable report → fails the
  job; `3` no mismatch but the **ratchet** must be updated (an unrecorded capability gap appeared, or a
  recorded one closed and went stale) → **fails the job**. Parity claims are therefore CI-checked, not
  manual-only. Two further jobs close the blind spots of a debug-only run: `rust-erts-release` builds
  and tests `beam_abi`/`beam_io`/`beam_rt` in `--release` (the profile where an `noalias`-driven lost
  store is visible), and `rust-erts-cranelift` tests `-p beam_engine --features jit-cranelift` so a
  target no default-feature build compiles cannot rot.
- **Vertical slice WORKS and now spans real stdlib:** `beam_loader` + `beam_engine` + `beam_rt` load a
  real `.beam` (produced by the system `erlc`) and execute it — `beam_rt run <slice.beam> slice:add/2 2 3` →
  `5`, `slice:fib/1 10` → `55`. Every non-obsolete `genop.tab` opcode is now modelled (the whole-OTP
  `.beam` census lowers with **zero residual `Generic` opcodes**), and dozens of real `lists`/`string`/
  `ordsets`/`proplists`/`maps`/`queue`/`gb_trees`/`calendar`/`filename`/`base64`/`unicode` functions run
  with byte-identical oracle results. Anything outside the supported surface returns a typed `BeamError`
  (never a wrong result).
- **Real-world projects run under this runtime** — the milestone's primary goal, each measured against
  the oracle with the identical command line: `jsone` **90/90** and `bbmustache` **79/79** eunit tests
  pass with byte-identical stdout/stderr (`tools/bench/RESULTS.md` §9); `cowboy` serves HTTP/1.1
  (`GET /` → 200 with the 12-byte `text/plain` body, plus HEAD/404/keep-alive/206) with **0 transport
  errors** and is the one two-sided benchmark row (§10; 8.39× to close, §B9 of `BOTTLENECKS.md`);
  `khepri`'s eunit modules and eight OTP `stdlib` `common_test` suites are driven under **both** runtimes
  with per-module/per-case counts (`tools/suites/RESULTS.md`: khepri rust `1497/4`, stdlib rust `182/21` —
  the RC-*/D* classes are the remaining divergences); `plb` runs 8 of 10 kernels and `elixirc` compiles
  through the `elixir` stage. The **`-on_load` module hook is implemented**, so `crypto` loads and
  `crypto:strong_rand_bytes/16`/`crypto:hash/2` match the oracle. Remaining per-project limits are
  recorded as `Known Issues` in the owning crate's `CONTEXT.md`; the `crypto`/`-on_load` NIF-visibility
  gap that capped `cowboy_websocket` is closed (RC-H: `-on_load` + NIF export registration).
- **Compatibility gate (`tools/differential`) — PASSES: `summary: total=3524 agree=3520 mismatch=0
  unsupported=1 skipped=0 oracle_drift=3`** with `ratchet: allowlist=known-gaps.txt allowed=1
  unauthorized=0 stale=0` and `drift: allowlist=known-drift.txt allowed=3 unauthorized=0 stale=0`
  → `gate.sh` exits 0. **No case disagrees.**
- The three drifts are host-oracle-version-specific: each golden pins the tree's `erts` version `17.1`
  (`system_info/version`, `boot_extra/system_info_version_is_frozen_17_1`,
  `nif_abi/system_info_frozen_fields`), so an `erts-17.1.1` oracle answers `<<"17.1.1">>` where this
  runtime reproduces the frozen value. On an oracle whose `erts` version matches the tree's pin all three
  reproduce and the drift ratchet reports `stale=3` (exit `3`), which is resolved there by trimming
  `known-drift.txt` or passing an empty `--drift-allowlist`; the drift file is host-scoped data and can
  never tolerate a `mismatch`.
- The optional Cranelift tier reproduces that same line (`build -p beam_rt --features cranelift`
  plus `BEAM_RT_TIER=cranelift`), so the tier changes no observable; a `cranelift` request in a
  build without the feature is refused up front (exit 3, one `beam_rt: unsupported: …` line naming
  the Cargo feature). The single recorded
  capability gap (`known-gaps.txt`, the ratchet allowlist) is `wire/new_fun_ext_encode` (A.5, an
  `erl_eval` lambda's closure identity needs `erl_eval`/`erl_scan`/`erl_parse`). `gate.sh` is
  authoritative for the live per-contract split; it always passes `--strict` and defaults its
  allowlist to `tools/differential/known-gaps.txt`. `tools/differential/CONTEXT.md` is the source of
  truth for the corpus size and the sweep recipe — do not duplicate the counts here.
  Caveat: the A.4 case `driver_extra/driver_pushqv_with_a_partial_skip_enqueues_the_tail` exposes an
  **oracle-side crash flake** (the system `erl` dies by signal on ~2 of 5 solo runs; the Rust side is
  correct) — an OTP 29.1.1 upstream defect, a corpus-harness issue, not a runtime divergence.
  The full sweep is time-boxed: a serial `gate.sh` boots every CLI case (~2.5 s each, >100 min), so
  the gate-equivalent route is `differential run <case-id>` at 12-way parallelism (~9 min for the whole
  corpus on a loaded shared host; `compare.rs::verdict_with` folds the oracle-vs-golden drift check into
  every verdict).
- **BIF surface complete:** `beam_bifs` registers **556/556 declared `bif.tab` BIFs** (registry **667**
  keys = 556 declared + 111 stdlib fast-path keys) through defaulted host seams (`RuntimeHooks::call`,
  `MemoryHook::memory`, …): **367 are served in-crate** and **189 are seam-backed** (a typed
  `unsupported` until a host installs the hook); the seam union is **222** keys (226 − 4 shared by two
  crates), 33 of them undeclared table members. The census is re-derived from code and **pinned by
  `beam_bifs/tests/roster_audit.rs` + `tests/seam_census.rs`**, so it cannot drift again.
- **Benchmarks:** every measured figure and its caveats lives in `tools/bench/RESULTS.md` (raw artifacts
  in `tools/bench/results/`) and in `BOTTLENECKS.md` §0, which names the **non-interchangeable
  records** and says which one is current — read it there rather than duplicating numbers. The
  artifact-backed records are **m19** (the full suite behind `RESULTS.md` §2/§3 and the external
  targets), **m21**/**m22** (the GC before/after pair plus the eunit rows), **m23** (the startup round
  plus the two-sided `cowboy` row, `RESULTS.md` §10) and **`r2-copybound-ab`** (the copy-path round's
  `beam_rt`-vs-`beam_rt` paired A/B); a figure quoted without one of those artifacts behind it is not a
  record. Each record
  names both sides (its own `sys` oracle and `beam_rt` at its default baseline-JIT tier) and its
  marginals use a **paired** estimator: each round samples the N-op and the 0-op pass back to back and
  takes the median paired difference, which replaced a non-interleaved schedule that had overstated
  every gap by up to ~56×. Load is recorded before/after every run: **the host is shared**, so read the
  ratios and the deterministic `case_*_ops/calls/elements` counters, never the absolute wall times.
  The **direction** is stable across records: rust is slower on the compute/`list_walk`/`copy_bound`/
  `gc_*`/`binary_append` rows and faster on the `file_io` rows (`handle_rw`, `write_read`), with `slice`
  wall and baseline peak RSS within ~1×–1.5× of the oracle. The honest residuals are published, not
  hidden: the tier glue (`Machine::step_instruction_inner` / `enter_in_block` / `jit_step_one`),
  allocator churn and the GC root walk are the next levers, and a self-recursive BIF-free tail loop that
  never pre-empts grows its off-heap payload without a park (bounded by `BEAM_RT_TIER=interpreter`) —
  see `BOTTLENECKS.md`. Target projects: `cowboy` (the only two-sided HTTP row — the Rust runtime
  serves `examples/hello_world` with 0 transport errors at 5 833 req/s against the oracle's 48 931
  = **8.39×**, p50 1.334 ms vs 0.171 ms, peak RSS 145.2 MB vs 68.1 MB, boot→ready 163.0 ms vs
  133.6 ms; `RESULTS.md` §10), `erlc`, `elixir`, `khepri`, `plb` and `jsone`/`bbmustache`, whose
  per-target figures, `sys`/`rust` rows and skip reasons live in `tools/bench/RESULTS.md`. `emqx` is
  **out of scope** (its checkout pins OTP 28 and the host lacks its native build prerequisites), so its
  row skips honestly as `skipped(emqx-requires-otp-28)`. The four `foreign/*` rows are **engine-tier**: per call they cross **0**
  BIFs (`to_lower`, `capitalize_token`), **4** (`iolist_to_binary/1`, `encode`) and **16**
  (`bit_size/1`, `decode`), so ~90–100 % of each residual is the baseline tier's per-instruction
  machinery and `beam_bifs`/`beam_term`/`beam_alloc` cannot close it — the live per-frame split is
  `crates/beam_engine/src/jit/baseline/measure/CONTEXT.md` §*The create arm's helpers*, not
  `BOTTLENECKS.md` §B3 (whose `dispatch` 12.6 %, `push_bit` 6.5 % and `alloc_impl` 17.31 % shares
  predate the create-arm landing and no longer reproduce).
  Attribution of the worst row (`binary_append`, callgrind + `perf cpu_core/instructions/u`): the
  residual is **one non-inlined opcode** — the comprehension loop's `bs_match` `integer` command
  takes the family path at ~647 instructions/iteration (~72 % of the ~897 total), while the same
  loop's `bs_create_bin` IS JIT-inlined (~200/iteration, one `jit_append_cursor` call per append) and
  the equivalent `{skip,8}` match form inlines whole (223–254/iteration). The in-scope crates are
  noise here — `beam_alloc` growth + `set_len` zero-fill ≈1.0 % of per-call instructions
  (8 `ensure_capacity` = 2 in-place grows + 6 allocate/copy/free relocations of a 256 B→64 KiB
  ladder, 64 512 B copied + 65 280 B zero-filled per call), `beam_bifs`/`beam_term`/`beam_ets` ≈0.1 %
  — so no in-scope lever exists for this workload; the fix belongs to the baseline-JIT `bs_match`
  shape (`crates/beam_engine/src/jit/baseline/emit/bitstr_inline/bs_match.rs`). Coverage caveat: no
  `tools/differential` case reaches `bs_init_writable`/`private_append` at all, so no case pins the
  bytes of a grown writable accumulator yet.
- **Contract A.9 (build/bootstrapping) — boot COMPLETES; no self-hosted build yet.** `beam_rt`
  loads the 22 preloaded modules on startup, and its boot-script executor drives the real
  `bootstrap/bin/start.boot` (OTP 30 / erts-17.1, 22 directives) **through the final
  `{progress,started}` (index 21)** — `beam_rt -noshell -boot bootstrap/bin/start` exits **0** with
  empty stdout/stderr. (Probe form: `-boot bootstrap/bin/start`; appending `.boot` doubles the
  suffix.) The kernel directive succeeds because the engine synthesizes the innermost `undef`
  stacktrace item (`{Module,Function,ArgList,Loc}`) that `logger_server:call_h/4` matches on — the
  head item rides the raise's own raw-trace word (rooted and relocated by every collection) with two
  metadata-only flags (`crates/beam_engine/src/families/exceptions/callee.rs`); when no live catch
  frame or an unreadable arg register/import ref exists the synthesis **declines** rather than
  fabricating a frame. The `{kernelProcess,…}` servers do survive their short-lived directive process
  via the unlink in `crates/beam_rt/src/boot/drive.rs`, and the `call_ext` seam is resolve-first /
  blocking-aware (`crates/beam_engine/src/families/funs/mod.rs`, pinned by
  `crates/beam_rt/tests/proc_lib_start.rs`). A.9's remaining gap is fidelity: **no self-hosted build**
  (the Rust runtime does not compile/run itself), and a plain runtime runs **no live**
  `code_server`/`erl_prim_loader` process, nor — off the `-boot` path — `application_controller` or
  `logger` (the real `start.boot` leaves `logger_server` registered as `logger`; see `crates/CONTEXT.md`).
  `code:*`/`init:*`/`application:*` queries are served by a native floor
  (`crates/beam_rt/src/native/{code,init,application}.rs`). Per-crate detail:
  `crates/CONTEXT.md` (A.9 status) and `crates/beam_rt/src/boot/CONTEXT.md`.
- **A.9 — in-process compilation: all 755 oracle-compilable `lib/*/src/*.erl` sources compile and produce the oracle's `.beam` bytes.** The sweep covers **772** sources across 34 apps; **17** fail on the oracle too (so they are not comparable) and the remaining **755** are byte-identical (whole-file md5 and every chunk). The route is OTP's default `compile:file/2` shape, and `code:get_path/0` is derived from the release layout the way `code_server.erl`'s `make_path` does it (**36** entries, matching the oracle element for element — the bundled-app/version selection, `choose_bundles`, `$ERL_LIBS`, `-pa`/`-pz` order). The mechanisms the sweep depends on: the `maps` iterator-protocol acceptance (a shared `walk_map_or_iter` backs `map/2`/`fold/3`/`filter/2`/`foreach/2`/`to_list/1`), the `bs_create_bin` bignum-into-narrow-segment write, a **libz-byte-exact** level-6 deflate behind `zlib:compress/1` (which `common_test/test_server_node`'s `compressed_literals` needs), and two native floors — `file_server_2` (`crates/beam_rt/src/native/file_server.rs`, incl. `read_handle_info/1` and the `{write_file,…}`/write-mode-`{open,…}`/`getopts` requests that `epp`'s `-moduledoc({file,…})` needs) and `code:load_binary/3`. A staged `epp → erl_scan → erl_lint → compile` chain (the compiler compiling the compiler) is byte-identical end to end; long compiles are bounded by `BLOCKING_PUMP_DEADLINE` (900 s). **No self-hosted build** (the runtime still does not compile/run *itself*).
- **Honest coverage gaps** (per-crate `Known Issues`): the Cranelift hot tier (`jit-cranelift`,
  default-OFF and not on `beam_rt`'s path — the native baseline tier is the default for every process
  entry, compiled lazily per function, with `BEAM_RT_TIER=interpreter` the only kill switch); the
  match-spec compiler beyond the core spec forms; `put_record/6`'s create form for a **foreign**
  module's record id (the running module's own records are supported) — the fix seam is a new
  *defaulted* `beam_engine::ExecutionContext` method handing back the owner module's
  `Arc<Module>` (the loaded modules ARE retained: `CodeServer`'s `modules: HashMap<String,
  Arc<Module>>`, all lowered against one shared atom table), since `beam_engine` cannot reach
  `beam_rt`/`beam_bifs` from an instruction and every tier funnels through
  `families::records::local_definition`; several
  structured-raise-reason degradations; the datagram/inet data path (`gen_udp:send/4`,
  `inet:getstat/1`, `gen_tcp:connect/3` and `listen/2`); depth of live distribution interop — `dist`/
  `dist_extra` carry the live two-node loopback now, so what remains is depth, not absence; and the
  `-eval` tree-walking evaluator's **application-nesting budget** (`beam_rt/src/eval/nesting.rs`: past
  512 nested fun applications it compiles the rest, so a 50 000-deep self-recursive named fun answers
  like the oracle, and past 16 nested *compiled* closures it raises a documented `unsupported`). Every
  one returns a typed error, never a wrong result.
- **Registered built-in drivers:** the three static `driver_tab.c` names — `tcp_inet`, `udp_inet`,
  `ram_file_drv` — are registered and `{spawn_driver,Name}` resolves through that membership gate
  (`fd`/`efile` answer `badarg` on the oracle too, and a refused name consumes no port id).
  `port_control/3` accepts both the named verbs and the decimal
  `INET_REQ_*`/`TCP_REQ_*`/`UDP_REQ_*`/`PACKET_REQ_*` opcodes; `ram_file_drv`'s reply matrix (77/77
  encodings) and `udp_inet`'s option/control surface agree with the oracle.

## Known Issues
- khepri `keep_while_fanout_probe` is correct (`deleted_count=2502` both sides); the residual is NOT fun application — it is `beam_rt`'s result-move path, not `beam_ets`.
- After hoisting a merged `ScannedBounds` fast-reject in `beam_rt::context::calls::dest::graph_reaches_scanned_heap` (the per-node `ProcessTable::pids()` scan is gone), `delete_us_per_dependent` ≈ 720 us vs the oracle ≈ 10 us (~70×), now dominated by the unconditional `copy_result_fresh` deep-copy of every `khepri_machine:apply/3` result (fix seam (a) in `beam_rt/src/context/IMPLEMENTATION.md`).
- The differential gate corpus does not pin `binary:referenced_byte_size/1` (bitbin) or khepri `projections`; those shapes are pinned instead by `beam_rt` workspace tests (`binary_part_refc_window`, `binary_part_refc_window_eval`) and the `tools/suites` harness probes (`error_info_abort_probe`, `keep_while_fanout_probe`).

## Future workstreams
- **Stage 2 (compiler):** loop opts → operation fusion → FBIP on the existing BEAM-SSA IR
  (`../lib/compiler`), per Part II §E/§H. Language-agnostic (Erlang/Elixir/LFE). Not part of the Rust
  runtime rewrite; the Rust runtime must ultimately run the compiler (contract A.9, self-hosted build)
  — that is future work.
- **Stage 4 adoption:** the copying collector IS driven by `beam_rt` at the engine heap-admission
  point (`beam_rt/src/context/gc.rs`), conservatively gated (a process is pinned once a live float
  register is seen, halted on a collection error, and a global valve disables collection once an
  escaped runtime store holds an off-heap term). `beam_process::Process` holds the strategy-agnostic
  `AnyCollector` (not a concrete `Collector`), so the mature generation is whichever strategy the
  policy's configuration resolved to — the copying default, or GenImmix above the
  `IMMIX_BREAK_EVEN_WORDS` auto-selection rule (`beam_gc/MEASURED.md`). The production fragment model
  is `beam_message`'s; `beam_gc::OffHeap` is the MSO slot store and its `sweep` is slot-stable.
- **Stage 6 (Level-2 clean-room VM):** new ISA + full toolchain — research track with an explicit exit
  criterion (Part II §H).
- **Legacy-C removal:** delete the corresponding C under `../erts/` module-by-module as each Rust crate
  reaches differential parity. Every surviving `.c` is referenced by an explicit object list in a build
  recipe (the 2-tier transitive checker `../erts/emulator/beam/check_dead_sources.py` — `--root`, STRICT
  closure, `--write-report` — finds only 1 actionable candidate, a documented false positive), so removal
  is parity-gated, never build-reference-gated. The parity matrix, the 5-point per-file gate and the
  ordered removal sequence live in `../erts/emulator/beam/CONTEXT.md`.

## Routing Table
- `crates/beam_common/` → shared error/result types, config, logging, feature flags, version constants (lowest common ancestor)
- `crates/beam_term/` → frozen tagged term representation, immediates, boxed terms, atom table, literal areas, term ops
- `crates/beam_alloc/` → allocators (`erts_alloc` replacement), carriers, process heap / bump allocator
- `crates/beam_codec/` → External Term Format (ETF) encode/decode (tags, `VERSION_MAGIC 131`)
- `crates/beam_deflate/` → zlib 1.3.2 in safe Rust (`deflate`/`inflate`/`checksum`): the one encoder behind `beam_codec`'s ETF `COMPRESSED` tag and `beam_bifs`'s `zlib` BIFs
- `crates/beam_dist/` → distribution protocol v6 (`DFLAG_*`/`DOP_*`), handshake, `epmd`
- `crates/beam_abi/` → NIF C ABI (`enif_*`) + driver C ABI (`driver_*`/`erl_drv_*`) + layout-frozen structs
- `crates/beam_loader/` → `.beam` file parsing, loader, `ops.tab` transforms, superinstructions
- `crates/beam_engine/` → typed block IR seam + interpreter + JIT backends (BeamAsm baseline, Cranelift); see also `TEST_NOTES.md`, `MEASURED.md`, `HANDOFF_bs_init_writable.md`
- `crates/beam_gc/` → per-process generational copying GC, MSO sweep, heap sizing (Stage 4)
- `crates/beam_process/` → processes, scheduler, run queues, thread progress, message queue, signals; see also `DESIGN_DECISIONS.md`
- `crates/beam_message/` → copy engine (`copy_struct`) + messaging (send/receive, fragments)
- `crates/beam_bifs/` → BIF registry (`bif.tab` surface) + implementations
- `crates/beam_ets/` → ETS tables (set/ordered_set/hash family), B-tree/CHAMP interiors; see also `ETS_SURFACE.md`
- `crates/beam_io/` → poller/reactor (epoll/kqueue/io_uring), ports, built-in drivers
- `crates/beam_rt/` → runtime binary: boot/preloaded modules, CLI flags, `system_info`, glue; see also `TRACE_AND_PERF.md`, `M1_GAPS.md`
- `tools/differential/` → differential-test harness vs real ERTS (the compatibility gate)
- `tools/bench/` → benchmark driver for the external target projects + system-ERTS baseline; measured numbers in `RESULTS.md`

## See Also
- `../CHANGE.md` → log of major structural/data-structure/algorithmic changes.
- `../erts/emulator/beam/CONTEXT.md` → the legacy C core (the reference implementation being ported).
- `../lib/compiler/CONTEXT.md` → Stage 2 compiler workstream (loop opts / fusion / FBIP).
