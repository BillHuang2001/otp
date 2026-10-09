# `beam_engine` Integration Test Suite (`tests/`)

## Intent
Black-box integration tests for the `beam_engine` crate: each `tests/<name>.rs` compiles as its
own crate and drives the engine through its **public** API. The loader appears only through
committed `.beam` fixtures, and `src/**` is never touched from here.

## API Surface (what a test crate may rely on)
- `beam_engine::{Machine, MinimalContext, ExecutionContext, RootSet, RunOutcome, Step, Tier,
  TraceEvent, unsupported, MAX_REGISTER, MAX_FLOAT_REGISTER, ...}` and `beam_engine::terms::*`
  (tuple/cons/float/map builders and readers).
- `beam_loader::{Module, Instr, Operand, HeapNeed, Label, TestKind, load_file, ...}` and
  `beam_term::{Eterm, make_small, make_atom, NIL, ...}`.
- `support::*` — the shared host: the multi-module `ModuleHost` `ExecutionContext`, the
  shared-atom loader, OTP discovery, the local BIF floor, and term assertion helpers.

## Constraints
- Read-only consumers: never edit `src/**` (owned elsewhere); never edit the crate-root docs.
- Failures are typed `beam_common::BeamError` values; never `unwrap()` engine runtime data;
  a harness-only invariant may panic with a message.
- Every `.rs` carries the OTP Apache-2.0 `// %CopyrightBegin% … // %CopyrightEnd%` header block;
  files target ~1000 lines.
- Always the wrapper, never a bare `cargo`, never `cd`:
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine [--test <name>]`.

## Notes for Agents
- `TEST_NOTES.md` (crate-root, not owned here) is the authoritative per-suite enumeration; do not
  duplicate it here.
- A suite needing the system toolchain (a real `.beam`, the `erl` oracle, or an `erlc` fixture
  build) must SKIP LOUDLY **only when the tool is absent** — a spawn failure
  (`Command::new("erl"/"erlc").output()` returned `Err`, no tool on PATH); a green run without
  `erl` proves nothing about corpus parity. A tool that RAN and exited non-zero is a HARNESS
  failure and must PANIC with its captured stdout/stderr, never be folded into "no oracle".
  Reference: `trace.rs::oracle_eval`.
- The `fixtures/` directory holds stand-in modules (`lists.beam`, `recv.beam`, …) that SHADOW
  stdlib when PREPENDED, so an oracle invocation must APPEND it (`erl -pz <dir>`), never prepend
  (`-pa`, which crashes the emulator at boot with an EMPTY stdout+stderr).
- `support/` is shared by every test crate; its facade re-exports the whole surface, so a
  re-export an individual crate does not use is not an unused import.

## Routing Table
- `support/` → shared multi-module `ExecutionContext` host: OTP discovery (`otp_lib_root`/`all_otp_beams`), the shared atom universe, the BIF floor, assertion helpers
- `fixtures/` → committed system-`erlc`-produced `.beam` fixtures + `.erl` sources + target lists
- `rooted_alloc.rs` → proves the engine's term-construction instruction arms route storage through `ExecutionContext::alloc_words_rooted` (whole-root hand-off; a counting context whose `alloc_words` stays the typed-error default so any fallback fails loudly; a moving-collector relocation proof)
- `type_sizes.rs` → observes the frozen IR size guards (`Instr` ≤ 64 / `Operand` ≤ 16 / `HeapNeed` ≤ 24) over the public types (authoritative pins are the compile-time asserts in `beam_loader/src/instr.rs`)
- `census.rs` → the `Instr`-variant / `TestKind` execution census + whole-OTP corpus reachability
- `reductions.rs` → per-instruction reduction charging (exactly-once, host pre-emption) and the suspend/resume slice boundary
- `gc_roots.rs`, `gc_arena.rs`, `bif_alloc.rs` → GC root-set hand-off, the chunked arena, BIF-driven allocation
- `jit.rs`, `jit_real_module.rs` → native baseline tier equivalence (synthetic + real-`erlc` modules); `jit_linking.rs`, `jit_cache.rs`, `jit_code_map.rs`, `jit_compare.rs`, `jit_coverage.rs` → block linking, the cross-run cache, the host code map, and baseline-vs-interpreter coverage; `jit_bench.rs` → the thin driver over the `crate::jit::bench` measurement harness; `jit_cranelift.rs`, `jit_cranelift_real.rs` → the feature-gated Cranelift hot tier
- everything else (`stdlib_*`, `apps_lists`, `coverage`, `opcodes`, `tuples_lists`, `maps`, `funs`, `floats`, `records`, `bitstrings*`, `exceptions*`, `receive`, `tailcall`, `external_call`, `cross_module_catch`, `clause_dispatch`, `func_info_stacktrace`, `current_module`, `process_info`, `wait_deadline`, `trace`, `gaps`, `bignum_compiled`, `slice`, and the `*_bench`/`*_cost` measurement suites) → per-family / per-contract execution and error-surface suites; see `TEST_NOTES.md`.
