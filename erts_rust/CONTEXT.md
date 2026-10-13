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
- **Crate tree complete:** 15 crates under `crates/` + 2 tooling crates under `tools/`, each with its
  own `CONTEXT.md`; all implemented and tested. Verify the node tree via the routing table below.
- **Build/test:** build with `scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace
  --all-targets` (warning-free); test with `scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml
  --workspace --no-fail-fast`. The suite is green (~10,800 passed / 0 failed; ~45 min wall on a loaded
  host, so run it in crate chunks when iterating).
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
  manual-only.
- **Vertical slice WORKS and now spans real stdlib:** `beam_loader` + `beam_engine` + `beam_rt` load a
  real `.beam` (produced by the system `erlc`) and execute it — `beam_rt run <slice.beam> slice:add/2 2 3` →
  `5`, `slice:fib/1 10` → `55`. Every non-obsolete `genop.tab` opcode is now modelled (the whole-OTP
  `.beam` census lowers with **zero residual `Generic` opcodes**), and dozens of real `lists`/`string`/
  `ordsets`/`proplists`/`maps`/`queue`/`gb_trees`/`calendar`/`filename`/`base64`/`unicode` functions run
  with byte-identical oracle results. Anything outside the supported surface returns a typed `BeamError`
  (never a wrong result).
- **Compatibility gate (`tools/differential`):** `summary: total=2163 agree=2161 mismatch=0 unsupported=2
  skipped=0 allowed=2` → `gate.sh` exits 0 (verified in both the default and the `BEAM_RT_TIER=interpreter` tier). `gate.sh` is authoritative — re-run it for the live
  per-contract split. **No case disagrees**; the two recorded capability gaps (the `known-gaps.txt`
  ratchet allowlist) are `wire/new_fun_ext_encode` (A.5, unclosable — an `erl_eval` lambda's
  closure identity needs `erl_eval`/`erl_scan`/`erl_parse`) and
  `boot_extra/init_script_id_names_the_running_otp_release` (A.9 — `init:script_id/0` on a `-eval`/`run`
  path that loads no boot script). The two A.9 `selfhost/*` in-process-compile cases now **agree**.
- **BIF surface complete:** `beam_bifs` registers **556/556 declared `bif.tab` BIFs** (registry 640
  keys = 556 declared + 84 non-table keys: 79 stdlib fast paths + `erlang:memory/0,1` + the OTP-30
  `pid_to_binary`/`ref_to_binary`/`port_to_binary`) through defaulted
  host seams (`RuntimeHooks::call`, `MemoryHook::memory`, …). Registered-but-unadopted BIFs return a
  typed `unsupported` until a host hook is installed.
- **Benchmarks:** every measured figure and its caveats lives in `tools/bench/RESULTS.md` (raw artifacts
  in `tools/bench/results/`) — read it there rather than duplicating numbers. Headlines since M1: the
  registered-BIF dispatch hot path fell from ~10.6 µs to ~116 ns/call and the registry lookup is now
  allocation-free; the baseline-JIT default took the
  `foreign` hot loop to **0.71×–0.84×** *of the Rust interpreter tier* and `gc_bound/churn` to
  **0.49×** — those are Rust-vs-Rust *tier-effect* ratios, NOT a system-ERTS comparison: against the
  system ERTS the same loops remained **46.2×–74.2×** and **42.2×** dearer at M9. The registered-BIF
  hot path is now ~79–96 ns/call. M10 then cut the ETF round trip **22.7×** (see `CHANGE.md`).
- **Contract A.9 (build/bootstrapping) — boot-to-completion achieved; no self-hosted build.** `beam_rt`
  loads the 22 preloaded modules on startup, and its boot-script executor drives the real
  `bootstrap/bin/start.boot` (OTP 30 / erts-17.1, 22 instructions) **to completion** — all 22 directives
  through the final `{progress,started}` (index 21); the `-boot` CLI also exits 0 with empty
  stdout/stderr. The last working link is instruction 20 `{apply,{c,erlangrc,[]}}`, unblocked by the
  runtime-native `file_server_2` service (`crates/beam_rt/src/native/file_server.rs`). Instruction 16's
  former `exit noproc` abort is gone: the `{kernelProcess,…}` servers survive their short-lived directive
  process via the unlink in `crates/beam_rt/src/boot/drive.rs`, and the `call_ext` seam is resolve-first /
  blocking-aware, so a cross-module `receive` suspends the caller
  (`crates/beam_engine/src/families/funs/mod.rs`, pinned by `crates/beam_rt/tests/proc_lib_start.rs`).
  A.9's remaining gaps are fidelity ones: **no self-hosted build** (the Rust runtime does not compile/run
  itself), and a plain runtime runs **no live** `code_server`/`erl_prim_loader` process, nor — off the
  `-boot` path — `application_controller` or `logger` (the real `start.boot` leaves `logger_server`
  registered as `logger`; see `crates/CONTEXT.md`). `code:*`/`init:*`/`application:*` queries are served
  by a native floor (`crates/beam_rt/src/native/{code,init,application}.rs`). Per-crate detail:
  `crates/CONTEXT.md` (A.9 status) and `crates/beam_rt/src/boot/CONTEXT.md`.
- **A.9 — in-process compilation WORKS on the no-spawn route; a scheduled-process compile does not
  yet, and there is still no self-hosted build.**
  The native `file_server_2` (`crates/beam_rt/src/native/file_server.rs`) now serves the
  `{write_file,…}` / write-mode `{open,…}` / device `getopts`/`{setopts,…}` requests, and the `compiler`
  ebin is on the default code path (`crates/beam_rt/src/codeserver.rs` `EXTRA_CODE_PATH_APPS`). The A.9
  E0 route — `compile:file(F,[no_spawn_compiler_process,deterministic,{outdir,Dir}])` — therefore runs
  **in-process** in the calling (pumping) process and produces a `.beam` byte-identical to the oracle's
  identical-option run (two `+deterministic` compilations of one source are byte-identical: the
  path-bearing `CInf`/`Line` fields are stripped and the `Code` chunk matches). OTP's default
  `compile:file/2` path reaches only `kernel` + `stdlib` (`epp` as an io-device-only client,
  `erl_features`, `erl_scan`/`erl_parse`/`erl_lint`/`erl_expand_records`, the `v3_core` → `beam_*`
  passes, and `save_binary`'s file writes) — not `logger`/`error_logger`, a live `code_server`,
  `open_port`, `crypto`/`zlib`/`beam_lib`/`ets`/`persistent_term`/`re`, or `syntax_tools`. What remains is
  fidelity: both `selfhost/*` corpus cases (`selfhost/in_process_compile_file_no_spawn_deterministic`,
  which drives the compile as a scheduled `run` process, and `selfhost/scheduled_process_compile_file_deterministic`,
  the default `compile:file/2` shape) now **agree** with the oracle. The engine call seams
  this needed (`call_ext`-encoded `erlang:apply/3`, `call_fun`/`call_fun2` for a scheduled process,
  `resolve_local_fun`) are all closed, and the bitstring-match operand-order bug (`bs_get_binary2`/
  `bs_get_integer2`/`bs_get_float2` writing `Dst` before committing the match context when `dst == ctx`)
  is fixed. **No self-hosted build** yet (the runtime does not compile/run itself).
- **Honest coverage gaps** (per-crate `Known Issues`): the Cranelift hot tier (`jit-cranelift`,
  default-OFF and not on `beam_rt`'s path — the native baseline tier is the default for every process
  entry, compiled lazily per function, with `BEAM_RT_TIER=interpreter` the only kill switch); full
  match-spec compiler; ETS concurrency; `put_record/6`'s create form for a **foreign** module's record
  id (the running module's own records are supported); several structured-raise-reason degradations;
  depth of live distribution interop. All return typed errors, never wrong results.

## Future workstreams
- **Stage 2 (compiler):** loop opts → operation fusion → FBIP on the existing BEAM-SSA IR
  (`../lib/compiler`), per Part II §E/§H. Language-agnostic (Erlang/Elixir/LFE). Not part of the Rust
  runtime rewrite; the Rust runtime must ultimately run the compiler (contract A.9, self-hosted build)
  — that is future work.
- **Stage 4 adoption:** the copying collector IS driven by `beam_rt` at the engine heap-admission
  point (`beam_rt/src/context/gc.rs`), conservatively gated (a process is pinned once a live float
  register is seen, halted on a collection error, and a global valve disables collection once an escaped
  runtime store holds an off-heap term). `beam_process::Process` holds the strategy-agnostic
  `AnyCollector` (not a concrete `Collector`), so the mature generation is whichever strategy the policy's
  configuration resolved to — the copying default, or GenImmix above the `IMMIX_BREAK_EVEN_WORDS`
  auto-selection rule (`beam_gc/MEASURED.md`). The production fragment model is `beam_message`'s;
  `beam_gc::OffHeap` is the MSO slot store and its `sweep` is slot-stable.
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
