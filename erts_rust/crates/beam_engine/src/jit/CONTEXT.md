# `beam_engine::jit` — Execution-Tier Seam (interpreter / baseline x86-64 / Cranelift)

## Intent
The Stage-1 **execution-tier seam** (Part II §F) of `beam_engine`: a tier roster (`Tier`, `JitBackend`,
`available_tiers()`), a real **native baseline x86-64 backend** (`baseline/`), the W^X machine-code
emitter both backends build on (`emitter/`), a cross-run compiled-block cache (`cache.rs`), an
escalation *policy* (`tiering.rs`), and the **optional Cranelift hot tier** (`cranelift/`, behind the
default-OFF `jit-cranelift` cargo feature). All tiers are **correctness-first mechanisms, not
optimisers** (Part II §F): each tier must be **interpreter-identical** for every opcode it inlines and
**deopt one whole interpreter step** (`Machine::step_instruction`) for everything else.

The interpreter itself is **not** a `JitBackend` (nothing to compile) and lives in
`../interpreter/`; this node owns every *native* tier plus the tier-entry routing.

## API Surface (FROZEN — `beam_rt` and tests design against this)
Re-exported through `beam_engine::jit` and (for `Tier`/`available_tiers`) the crate root.
- `mod.rs`: `enum Tier { Interpreter, BaselineJit, Cranelift }` (`Tier::ALL` = roster order,
  interpreter first; `name()`/`from_name()`/`is_available()`); `trait JitBackend { type Compiled;
  name; available; compile(&mut self, &Module) -> Result<Self::Compiled>; run_compiled(&self, ..);
  run_export = compile + run_compiled }` (`run_compiled` is `&self` — the artifact is caller-owned, so
  one compiled module runs any number of times on fresh `Machine`s); `available_tiers()`;
  `pub const TIERS`/`NATIVE_TIERS`; the feature-gated `run_cranelift`/`run_cranelift_outcome`/
  `run_cranelift_label_outcome` (`mod.rs:329-423`).
- `cache.rs`: `module_fingerprint(&Module) -> u64`, `ModuleKey`, `CodeCache`
  (`get_or_compile`/`run_export`/`contains`/`len`/`clear`/`code_map`), the thread-local `CODE_CACHE`,
  and `run_baseline_cached`/`run_baseline_cached_outcome`/`run_baseline_cached_label_outcome`
  (`:363`/`:401`/`:431`); `CodeRegion { start, size, name }` + `baseline_code_map(&Module)` expose the
  **real** generated ranges (contract A.8 `+JPperf` perf map) — `start` is an actual
  `CompiledModule::entry`, never fabricated.
- `tiering.rs`: `TierPolicy { baseline_threshold, hot_threshold }` with
  `select(executions) -> Tier` (`Default` = baseline `1`, hot `10_000`) — a **policy only**, nothing
  reads it on any execution path.
- `baseline/mod.rs`: `BaselineJit` (a zero-sized `JitBackend`), `CompiledModule`, the driver
  `run_compiled`/`run_native_outcome`; entry is **refused** (native disabled for that run) when
  `machine.x.len() <= highest_native_register` (`:487-496`).
- `emitter/mod.rs` + `emitter/encode.rs`: `CodeBuffer` (`new`/`with_capacity`/`label`/`bind`/`seal`/
  `as_fn_ptr`/`len`/`code_ptr`), `Reg`, `Cond`, `Label`. `pub const DEFAULT_CAPACITY = 64 KiB`.

### The native block ABI (baseline and Cranelift share it)
`extern "C" fn(*mut JitRuntime) -> i64` returning `encode_target(func, pc) = func<<32 | pc`, or the
sentinel words `HALT = -1`, `ERROR = -2`, `WAIT_BIT = i64::MIN` (baseline `runtime.rs:94-109`).
`JitRuntime` is `#[repr(C)]` (`RUNTIME_XREGS_DISP = 8`) carrying `machine`, `x_regs`, `module`,
`labels`, `ctx`, `error`; generated code loads **only** `x_regs` (the x-register array base is `rbx`
in the baseline block convention). A suspend crosses as a `WAIT_BIT` word; a propagating
`BeamError` is left in `rt.error` and returned as `ERROR`.

## The correctness contract — why "interpreter-identical" holds (VERIFY, don't trust)
The claim is SUBSTANTIATED for every inlined fast path; the mechanism is three invariants that must be
preserved by any change here:
1. **Cache membership is block-fixed.** `emit/regalloc.rs` picks the ≤5 most-touched x-registers
   (`r12`-`r15`+`rbp`) once per block; the prologue fills them, the epilogue spills them. No opcode may
   change the resident set mid-block.
2. **Spill before every helper/GC call.** `emit_helper_call`/`emit_step_and_continue`
   (`emit/mod.rs:747-792`) spill ALL cached registers before calling `jit_step_one`, and re-`fill`
   them after a `Step::Next` (`:791`, `fill` at `:810-815`, conservative liveness). A helper (which may
   run a GC and move terms) therefore never sees a stale hardware copy.
3. **Spill on BOTH exit paths.** `Linker::emit_exit` (`emit/link.rs:166-191`) spills on the linked
   successor path AND the deopt path before `emit_frame_teardown` (`:259-265`).
⇒ **No cached hardware register is ever live across a helper/GC call or a block boundary** — the classic
"unspilled register across GC" bug class is structurally absent. Any new inlining must go through
`emit_helper_dispatch`/`emit_step_and_continue` on every rejection path.

### The inlined set (and its interpreter arm for contrast)
Every inliner is a *shape test*: on any guard rejection it branches to `emit_step_and_continue`, which
calls `jit_step_one` (`baseline/runtime.rs:192-237`) = literally `Machine::step_instruction`, so the
interpreter's exact semantics (and exact typed error) stand.
- **Arithmetic** (`emit/arith.rs:318-436`): guards `(word & 0xF) == 0xF` (small) on both operands. `+`/`-`
  do tagged-word arithmetic with `jcc(O)` on both the add/sub and the `∓15` adjustment, then a tagged
  range check against `MIN_SMALL_WORD`/`MAX_SMALL_WORD`. `*` untags BOTH operands (`sar 4`), `imul`+`jo`,
  range-checks the **untagged** product (`p*16` can wrap back into the valid interval — the shift is
  load-bearing), then `shl`/`or`. Non-small / bignum / float ⇒ deopt. Interpreter arm:
  `interpreter/dispatch.rs:249-273` `GcBif`.
- **Comparisons** (`emit/compare.rs:291-321` rule table, `:428-483` lowering): the eight
  `erlang:{'==','/=','<','=<','>','>=','=:=','=/='}/2` as word compares. Order ops refuse an
  `Immediate` constant and guard smalls; exact ops refuse bignums (`cmp_operand` `:275-287` returns
  `None` for an out-of-range `Operand::Integer`), so "const ⇒ plain word compare is exact" only fires
  for `Small`/`Immediate`. `<`/`>` are lowered **directly, never by swapping operands** (antisymmetry
  hazard). Both operands are loaded before the destination is stored (aliasing-safe). Never calls
  `ctx.call_bif`; a rejection de-opts and reproduces the fail/raise split.
- **`test`** (`emit/test.rs:347-442`): nil/atom/cons/is_list/boolean word tests + guarded word compares.
  `fail == NO_FAIL` (`:173`) and unresolved labels (`:177`) refuse outright. `IsBoolean` (`:416-422`)
  matches `interpreter/test.rs:105-123` even with only one of `true`/`false` interned;
  `IsNonemptyList == is_list` matches `list.rs:74` + `interpreter/test.rs:102`.
- **`select_val`** (`emit/select.rs:84-153`): every alternative must be an immediate
  (`constant_word`); fail must resolve and not be `NO_FAIL`. In-order compare chain + fall-through to
  fail == the interpreter's first-match scan (`interpreter/dispatch.rs:223-244`).
- **`select_tuple_arity`** (`emit/aggregate.rs:572-617`) + `emit_tuple_header_guard` (`:627-655`):
  `is_boxed` bit trick → header ptr → `test hdr, 0x3F; jne` ⇔ `is_arity_value`
  (`ARITYVAL_SUBTAG=0x00`, `TAG_HEADER_MASK=0x3F`).
- **Local tail calls** (`emit/link.rs:209-248`): `jac_tail_call` (`runtime.rs:266-284` / `jit_tail_call`)
  IS `Machine::pop_frame_checked`, the identical call the interpreter makes after `resolve`
  (`interpreter/dispatch.rs:303-304`). Target resolved at compile time first; a failed pop ⇒ `ERROR`
  ⇒ driver propagates the exact typed error. `call_only` (no frame) lowers as a `jump` with **no** pop.
  **No skipped/double frame pop.**
- **`swap`**: baseline reads both slots then stores them exchanged (aliasing `a == b` is a no-op); the
  Cranelift `inline_swap` (`cranelift/inline.rs:115-154`) does the same via SSA (both loads precede both
  stores). Matches the interpreter's `write(a, read(b)); write(b, read(a))`.
- **Deps to a tier that is not `Step`-free**: `Step::Enter` (a module-context switch, i.e. a
  `call_ext*` resolved through `resolve_external_call`) is an **honest typed error** on both native
  paths (`baseline/runtime.rs:225-230`, `baseline/mod.rs:742-746`), never a silently wrong block.

## Constraints
- **Cranelift is genuinely OFF by default and the feature-off path is a TYPED error.** `Cargo.toml:20`
  `default = []`; `:25` `jit-cranelift = ["dep:cranelift-*"]` (deps `optional = true`, `:14-17`);
  `:24` `jit-beamasm = []` is a documented no-op alias (the baseline tier is in every roster).
  Feature-off `run_cranelift*` returns `Err(error::unsupported("jit-cranelift tier not built"))`
  (`mod.rs:357`, `:391`, `:422`) — **never a silent fallback to another tier**. `cargo tree -p
  beam_engine` without the feature must list no `cranelift-*` crate.
- **No native tier reaches the production execution path.** `beam_rt/src/runner/run.rs:236,260` route
  through `run_label_outcome_with_tier(..., Tier::Interpreter, ...)` (`:242`,`:266`) with the tier
  **hardcoded**. `Machine::run_with_tier` / `run_export_outcome_with_tier` /
  `run_label_outcome_with_tier` and `run_baseline_cached*` are reached only from tests. So the
  public native-tier entries are **effectively dead in practice** — the seam is landed, not adopted.
- **Errors**: `beam_common::{Result, BeamError}` only; no crate-local enum. No production
  `panic!`/`unwrap`/`unreachable!`/`todo!` anywhere under `src/jit/` (all `unwrap`/`expect`/`panic!`
  hits are inside `#[cfg(test)]` blocks). A driver `ERROR` with an empty slot becomes
  `error::unsupported("jit: helper error")` (`baseline/mod.rs:696`).
- **`unsafe` zone**: `emitter/mod.rs` carries the crate's **only** executable-memory mapping
  (`#![allow(unsafe_code)]` is module-local; the crate root stays `#![deny(unsafe_code)]`), plus
  `baseline/{mod,runtime}.rs` + `baseline/emit/*` (reconstruct `&mut Machine`/`&mut dyn
  ExecutionContext`/`&Module`/`&LabelMap` from `JitRuntime`'s live, unaliased pointers and call a block
  entry as a transmuted `extern "C" fn`) and feature-gated `cranelift/mod.rs`. Every `unsafe` block has
  a `// SAFETY:` rationale.
- **W^X**: `mmap(PROT_READ|PROT_WRITE)` → `mprotect(PROT_READ|PROT_EXEC)` in `CodeBuffer::seal`;
  never RWX; `munmap` in `Drop`; every emitting call after `seal` is a typed `BeamError::BadArg`.
  A failed `mmap`/`mprotect` is `BeamError::Io`, capacity exhaustion `BeamError::SystemLimit`, misuse
  (foreign/doubly-bound/unbound label, emit-after-seal) `BeamError::BadArg`.
- **Files ~1000 lines**, split by responsibility (`baseline/emit/*` are separate files);
  Licensing Apache-2.0.

## Known Issues / Intentional gaps (code-backed)
- **One `#[allow(dead_code)]` under `src/jit/`: `baseline/emit/compare.rs:170`** on `enum CmpOperand`
  (`Reg`/`Small`/`Immediate`). The payloads are matched *by variant* (the guard rules need the class,
  not the value) and only `is_const()` is read, so this is deliberate — it is NOT an unimplemented
  fast path.
- **Dead in practice (seam landed, no production caller):** the whole native tier roster beyond
  `Tier::Interpreter`; `tiering.rs::TierPolicy::select` (nothing reads the escalation policy);
  the `+JPperf` perf-map plumbing (`baseline_code_map`) is wired but no host consumes it under a tier.
- **Residual, documented interpreter-vs-JIT divergences** (both acknowledged in the baseline driver
  docs; neither observable today because no host reads them under a tier):
  (i) a **natively-executed block emits no A.8 trace events** (the interpreter emits `call`/`return_from`/
  `return_to` at `../interpreter/dispatch.rs:308-310`/`:318-320`), and a **linked local tail call** emits
  neither a `call` nor a `return_to` (the full set is emitted only on the per-instruction deopt path);
  (ii) `process_info/2`'s position is **coarser** after a native block (a native block updates neither
  `Machine::current_code` nor the trace gate). Documented in `../CONTEXT.md` Handoff #9.
- **Inlining is deliberately partial:** `PutTuple2`, `BsTestTail`, `FuncInfo`, the `call_ext_*` tail
  surface, and the `call_ext_*`/`call`/`apply`/`call_fun`/exception surface all deopt. `call_ext_last`
  still round-trips through the driver; the frame-popping `call_last` stays on the helper in Cranelift
  (the pop is Rust-side `Machine` state). Residency is block-fixed. These are honest mechanism
  boundaries, not correctness holes.
- **Minor inconsistency (NOT a bug):** `select_tuple_arity`'s alternatives accept any non-negative
  `i64` with no `MAX_ARITYVAL` clamp (`emit/aggregate.rs:552-557`), while the interpreter clamps in
  `eval_test_arity` (`interpreter/test.rs:186`). Not reachable from compiled code with a well-formed
  term.

## Test Strategy
Per-module `#[cfg(test)]` unit tests (the emitter's encoding tests assert exact emitted bytes; the
`link.rs`/`baseline/tests.rs` suites assert spill/fill and interpreter-vs-baseline equivalence) plus the
crate's integration suites: `../tests/jit_real_module.rs` (the real-`erlc`-module **tier-equivalence
gate** — claimed 736/736 oracle-checked comparisons, not independently re-run here),
`../tests/jit_linking.rs`, `../tests/jit_code_map.rs`, `../tests/jit_bench.rs` (measurements —
interpreter→baseline speed-ups and the local-tail-call-linking delta), and the feature-gated
`../tests/jit_cranelift*.rs`. Run via `erts_rust/scripts/cargo.sh test --manifest-path
erts_rust/Cargo.toml -p beam_engine` (add `--features jit-cranelift` for the feature-ON roster). The
suites that load real OTP `.beam` skip loudly when `erl`/the install is absent.

## Routing Table
- `mod.rs` -> `Tier`, `JitBackend`, `available_tiers`, `TIERS`/`NATIVE_TIERS`, the feature-gated
  `run_cranelift*` entries, `Tier` unit tests.
- `cache.rs` -> `module_fingerprint` (**content-derived**, not a pointer: hashes the module name,
  atom table by name, `exports|imports|functions|funs|records|code_version|types|line` via deterministic
  `Debug`, the raw `strings`/`attributes`/`compile_info` blobs, and the decoded literal words through a
  non-randomized `DefaultHasher`), `ModuleKey`, `CodeCache`, the thread-local `CODE_CACHE`, the
  `run_baseline_cached*` entries, `baseline_code_map`/`CodeRegion`.
- `tiering.rs` -> the escalation policy only (`TierPolicy`).
- `emitter/{mod,encode,tests}.rs` -> the x86-64 encoder + the W^X `CodeBuffer` (the crate's only
  executable mapping; System V AMD64; `seal` = RW→RX `mprotect`).
- `baseline/{mod,runtime,tests}.rs` + `baseline/emit/*` -> the native baseline backend, the block ABI
  driver, `jit_step_one`/`jit_tail_call`, the per-block register picker (`emit/regalloc.rs`), and the
  inliner groups (`emit/{move,arith,compare,test,select,aggregate,link,operands}.rs`).
- `cranelift/*` -> the optional hot tier (feature-gated): one Cranelift function per IR `Function` over
  the same shared interpreter-backed helper, `inline.rs` (the shared inline set + `inline_swap`),
  `shape.rs`, `report.rs` (`CompilegenReport`); `inlined_swap_count`/`inlined_tail_call_count`.

## See Also
- `../CONTEXT.md` -> the engine's API surface, the `Step` protocol, and Handoff #9 (native-tier
  adoption by `beam_rt`) / #8 (ext tail-call linking).
- `../MEASURED.md` -> the measured JIT speed-ups and their caveats (the tiers are mechanisms, not
  optimisers); `../INSTRUCTION_SURFACE.md` -> the full instruction surface + gap list.
- `../../CONTEXT.md` -> the crate-layer contracts, the Stage-1/Stage-5 DAG, seam types;
  `../../../CONTEXT.md` §4 -> the engine IR seam and the root-set contract.
