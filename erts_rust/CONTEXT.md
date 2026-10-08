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
- Licensing: Apache-2.0 (matches OTP); deps Apache-2.0/MIT only.

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
- Do **not** run full external suites on every change — reserve them for milestones.
- The oracle runtime is the system Erlang (`ERTS_ORACLE` in the flake; `erl` on `PATH`, OTP 29/erts-17.1).
- Benchmark/test targets are the configured foreign repos: Cowboy, Khepri, EMQX, Elixir,
  Programming-Language-Benchmarks. Investigate them read-only; write through the task's tracked worktrees.

## Status
- **Crate tree complete:** all 15 crates + 2 tools are architected (each with its own `CONTEXT.md`),
  implemented, and tested.
- **Build/test:** build with `scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace
  --all-targets`; test with `scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml --workspace
  --no-fail-fast`. The `beam_rt` failures once caused by `beam_bifs` registering the FULL declared
  `bif.tab` surface (556 keys; registry 637) shadowing `beam_rt`'s curated
  `KNOWN_UNIMPLEMENTED_ERLANG_BIFS` list are resolved: `beam_rt::is_known_unimplemented_bif` now
  intersects the live registry (`beam_rt/src/error.rs`), with drift-guard tests. Re-run the suite to
  read the current pass count.
- **No CI runs the Rust workspace or the differential gate.** `.github/` has zero references to
  `erts_rust`/`cargo`/`differential`; every Rust test and the compatibility gate run only locally
  (`scripts/cargo.sh`, `tools/differential/gate.sh`). Parity claims therefore rest on manual runs.
- **Vertical slice WORKS:** `beam_loader` + `beam_engine` + `beam_rt` load a real `.beam` (produced by
  the system `erlc`) and execute it — `beam_rt run <slice.beam> slice:add/2 2 3` → `5`,
  `slice:fib/1 10` → `55`. The supported opcode subset is documented honestly per crate; anything
  outside it returns a typed `BeamError` (never a wrong result).
- **Compatibility gate (`tools/differential`):** per `tools/differential/CONTEXT.md`, `summary: total=780
  agree=772 mismatch=0 unsupported=8 skipped=0` (`gate.sh` → exit `3`) — per contract A.1 154/154 · A.2
  176/176 · A.3 34/34 · A.4 64/64 · A.5 79 (78 agree + 1 unsupported) · A.6 44/44 · A.7 322 (315 + 7) · A.8
  64/64 · A.9 52/52. **No case disagrees.** The 8 `unsupported` are honest capability gaps, never
  `mismatch`: `wire/new_fun_ext_encode` (unclosable — an `erl_eval` lambda's closure identity needs
  `erl_eval`/`erl_scan`/`erl_parse`), the five `erlang:statistics/1` counters the runtime does not serve
  (`proc_signals/statistics_*_unsupported`), and the two `erlang:monitor/2,3` gap shapes
  (`proc_signals/monitor_3_tag_option_unsupported`, `proc_signals/monitor_port_unsupported`). (Sibling
  docs cite older totals; re-run `gate.sh` for the exact current split.)
- **BIF surface complete:** `beam_bifs` registers **556/556 declared `bif.tab` BIFs** (registry 637
  keys = 556 declared + 81 non-table keys: 79 stdlib fast paths + `erlang:memory/0,1`) through two
  defaulted seams (`RuntimeHooks::call`, `MemoryHook::memory`). Registered-but-unadopted BIFs return a
  typed `unsupported` until a host hook is installed.
- **Benchmarks:** current numbers and their measurement caveats live in `tools/bench/RESULTS.md` (its
  Milestone-1 archive is explicitly marked there). The known hot-path defect is `beam_rt`'s
  `RuntimeContext::call_registered` deep-cloning the shared `AtomTable` on **every** registered BIF
  call — ~99% of the marginal per-iteration cost of `beam_rt run …`, and the reason per-call costs on
  real Cowboy/EMQX functions measure 700×–9 000× the oracle. That defect is resolved — the whole-table
  clone in `beam_rt`'s `call_registered` is now skipped when there is nothing to commit — so the
  `RESULTS.md` figures predate the fix and need re-measuring.
- **Honest coverage gaps** (per-crate `Known Issues`): the engine tiers and the Cranelift hot tier
  (`jit-cranelift`, default-OFF) are not yet on `beam_rt`'s execution path; full match-spec compiler;
  ETS concurrency; `put_record/6`'s create form; several structured-raise-reason degradations; depth of
  live distribution interop. All return typed errors, never wrong results.

## Future workstreams
- **Stage 2 (compiler):** loop opts → operation fusion → FBIP on the existing BEAM-SSA IR
  (`../lib/compiler`), per Part II §E/§H. Language-agnostic (Erlang/Elixir/LFE). Not part of the Rust
  runtime rewrite; the Rust runtime must ultimately run the compiler (contract A.9, self-hosted build)
  — that is future work.
- **Stage 4 adoption:** the copying collector and the measurement-gated GenImmix mature generation
  (`beam_gc`) are implemented and auto-selected at/above `IMMIX_BREAK_EVEN_WORDS`; no outer crate drives
  a collector yet (`beam_rt` does not).
- **Stage 6 (Level-2 clean-room VM):** new ISA + full toolchain — research track with an explicit exit
  criterion (Part II §H).
- **Legacy-C removal:** delete the corresponding C under `../erts/emulator/beam/` module-by-module as
  each Rust crate reaches differential parity (it is retained only as oracle + `FLAVOR=emu` fallback).

## Routing Table
- `crates/beam_common/` → shared error/result types, config, logging, feature flags, version constants (lowest common ancestor)
- `crates/beam_term/` → frozen tagged term representation, immediates, boxed terms, atom table, literal areas, term ops
- `crates/beam_alloc/` → allocators (`erts_alloc` replacement), carriers, process heap / bump allocator
- `crates/beam_codec/` → External Term Format (ETF) encode/decode (tags, `VERSION_MAGIC 131`)
- `crates/beam_dist/` → distribution protocol v6 (`DFLAG_*`/`DOP_*`), handshake, `epmd`
- `crates/beam_abi/` → NIF C ABI (`enif_*`) + driver C ABI (`driver_*`/`erl_drv_*`) + layout-frozen structs
- `crates/beam_loader/` → `.beam` file parsing, loader, `ops.tab` transforms, superinstructions
- `crates/beam_engine/` → typed block IR seam + interpreter + JIT backends (BeamAsm, Cranelift)
- `crates/beam_gc/` → per-process generational copying GC, MSO sweep, heap sizing (Stage 4)
- `crates/beam_process/` → processes, scheduler, run queues, thread progress, message queue, signals
- `crates/beam_message/` → copy engine (`copy_struct`) + messaging (send/receive, fragments)
- `crates/beam_bifs/` → BIF registry (`bif.tab` surface) + implementations
- `crates/beam_ets/` → ETS tables (set/ordered_set/hash family), B-tree/CHAMP interiors
- `crates/beam_io/` → poller/reactor (epoll/kqueue/io_uring), ports, built-in drivers
- `crates/beam_rt/` → runtime binary: boot/preloaded modules, CLI flags, `system_info`, glue
- `tools/differential/` → differential-test harness vs real ERTS (the compatibility gate)
- `tools/bench/` → benchmark driver for the external target projects + system-ERTS baseline

## See Also
- `../CHANGE.md` → log of major structural/data-structure/algorithmic changes.
- `../erts/emulator/beam/CONTEXT.md` → the legacy C core (the reference implementation being ported).
- `../lib/compiler/CONTEXT.md` → Stage 2 compiler workstream (loop opts / fusion / FBIP).
