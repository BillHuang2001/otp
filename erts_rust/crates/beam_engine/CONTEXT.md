# `beam_engine` — Typed-IR Execution Engine (interpreter + JIT tiers)

## Intent
Executes the **frozen typed IR** produced by `beam_loader`: a **direct-threaded interpreter** over the
Milestone-1 opcode subset (this crate's Stage-1 deliverable) plus the **optional JIT tiers**
(`jit_beamasm`, `jit_cranelift`) that are *measurement-gated* and kept **off the critical path**
(never implemented in M1). Implements the Stage-1 engine half of Part II §H with the IR contract of
§E and the interpreter/JIT trade-offs of §F (`../../CONTEXT.md` §4).
This crate is on the **critical path of the vertical slice**: it loads a real `.beam` (via
`beam_loader`) and runs it end to end.

**The engine depends only on `beam_loader` (+ `beam_term`/`beam_common`)** — never on the Stage-5
runtime core. Everything the runtime must supply (BIF dispatch, heap/GC, process/stack limits) is
abstracted behind the [`ExecutionContext`] trait; `beam_rt` provides the concrete implementation.
This keeps the Stage-1/Stage-5 DAG acyclic and lets the engine be tested with a minimal context.

Ground truth (read-only, never modify): the interpreter dispatch `../../erts/emulator/beam/emu/beam_emu.c`,
the instruction bodies `../../erts/emulator/beam/emu/{instrs,macros,bif_instrs,select_instrs,arith_instrs}.tab`
and the call/return/allocate conventions of `../../erts/emulator/beam/beam_common.c`.

## API Surface (FROZEN — the runtime and tests design against this)
Re-exported at the crate root and via `beam_engine::prelude::*`. **This crate does NOT embed a BIF
table**: all computation goes through `ExecutionContext::call_bif`.

### The context seam (`src/context.rs`)
```rust
pub trait ExecutionContext {
    /// Dispatch `module:function/arity` with `args`, returning the result term.
    /// Failure is return-based: `Err(BeamError)` (a `BadArith`/`BadArg`/`Raise`/… the engine
    /// turns into a guard-branch or a raise — see GcBif semantics below).
    fn call_bif(&mut self, module: &str, function: &str, arity: u32, args: &[Eterm])
        -> Result<Eterm>;

    /// Ensure `heap_need` heap words are available for the process (`test_heap`/
    /// `allocate_heap`), honoring the root-set contract: the live roots are
    /// `x_regs[0..live]` plus the whole `stack`. A real context (`beam_rt`, Stage 4/5) runs the
    /// collector here and may **move terms**, updating `x_regs`/`stack` in place.
    fn ensure_heap(&mut self, heap_need: u32, live: u32, x_regs: &mut [Eterm], stack: &mut [Eterm])
        -> Result<()>;

    /// Heap words currently available (introspection). Default: unbounded.
    fn heap_available(&self) -> usize { usize::MAX }

    /// Allocate `n` uninitialized heap words and return a stable pointer the caller
    /// (`terms`) writes boxed terms into. Default: a typed `error::unsupported`.
    /// Stability: valid only until the next `ensure_heap`/GC (mirrors `HAlloc`/`heap_top`).
    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> { /* default: unsupported */ }

    /// External-function / BIF application hook (the `apply`/`call_ext` path).
    /// Default: forwards to `call_bif`.
    fn apply(&mut self, module: &str, function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm>
        { self.call_bif(module, function, arity, args) }
}```
- `MinimalContext` — a concrete context for tests/embeddings: an unbounded heap (`ensure_heap` is a
  no-op) plus a **pluggable BIF hook**
  (`with_bif_hook(f)`, `f: FnMut(&str,&str,u32,&[Eterm]) -> Result<Eterm>`). `MinimalContext::new()`
  uses a default hook that returns `error::unsupported("bif")`. The vertical-slice test supplies a
  hook implementing `erlang:'+'/2` and `erlang:'-'/2` over small ints.
  `alloc_words` hands out words from a pointer-stable arena (`Vec<Box<[Eterm]>>`, no new dependency);
  `apply` forwards to `call_bif` (the hook).

### The machine (`src/machine.rs`)
- `pub struct Machine` — the process-state core: the **x-register file**, the **Erlang stack**, and
  the **call-frame stack**.
- `pub const MAX_REGISTER: u32 = 1024;` — the x-register file size (`beam_emu.c` `MAX_REG` bound).
- `pub const MAX_FLOAT_REGISTER: u32 = 1024;` — the **float-register (FR) file** size, mirroring the
  C `f_reg_array` (`FloatDef d[MAX_REG]`, `erl_process.h`; `MAX_REG = 1024`, `erl_vm.h:49`; JIT index
  bound `0..=1023`). The FR file (`fr: Vec<Eterm>`, (re)filled by `reset`) holds **`Eterm` term
  slots** at M1 (see Design Decisions).
- `Machine::get_fr(&self, n: u32) -> Option<Eterm>` / `Machine::set_fr(&mut self, n: u32, Eterm) -> Result<()>`
  — bounds-checked FR access (`bad_register` on overflow).
- `Machine::new() -> Machine` / `Machine::default()`; `Machine::with_max_register(u32) -> Machine`.
- `pub fn run_export(&mut self, module: &Module, function: Atom, arity: u32, args: &[Eterm],
  ctx: &mut dyn ExecutionContext) -> Result<Eterm>` — the primary entry: resolve `function/arity`'s
  export entry label, seed `x[0..arity]` from `args`, push the bottom frame, run to `return`, and
  yield `x(0)`.
- `pub fn run_label(&mut self, module: &Module, label: Label, arity: u32, args: &[Eterm],
  ctx: &mut dyn ExecutionContext) -> Result<Eterm>` — run a raw entry label (used by `run_export`).
- Introspection (read-only): `x(&self, n: u32) -> Option<Eterm>`, `get_fr(&self, n: u32) -> Option<Eterm>`,
  `frame_depth(&self) -> usize`, `stack_depth(&self) -> usize`, `reset(&mut self)` (also clears the FR file).
- `pub enum StepOutcome { ... }` / the interpreter loop entry `pub fn run_loop(...)` — the dispatch
  loop lives in `src/interpreter.rs` (`impl Machine`).

### Term helpers (`src/terms.rs`) — the SAFETY-annotated builder/reader
Constructors take the seam (`ctx: &mut dyn ExecutionContext`, allocating via `alloc_words`):
- `make_tuple(ctx, &[Eterm]) -> Result<Eterm>` (arityval header + elements)
- `make_list(ctx, &[Eterm]) -> Result<Eterm>` (cons cells; empty → `NIL`)
- `make_float(ctx, f64) -> Result<Eterm>`
- `make_flatmap(ctx, &[(Eterm, Eterm)]) -> Result<Eterm>` (>32 pairs → `unsupported`)
- `make_fun(ctx, arity: u32, uniq: u64, index: u32, free: &[Eterm]) -> Result<Eterm>` (minimal local
  closure; M1 inlines the out-of-line fun-entry descriptor — see Design Decisions)
- `make_heap_binary(ctx, &[u8]) -> Result<Eterm>` (heap binary/bitstring, ≤64 bytes)

Readers (no ctx; wrong type / out-of-range → `BeamError::BadArg`, a hash-map is `unsupported`):
`tuple_arity`, `tuple_element`, `cons_head`, `cons_tail`, `list_elements`, `float_value`,
`map_entries` (flatmap only), `binary_bytes`, `is_fun`, `fun_arity`, `fun_num_free`, `fun_uniq`,
`fun_index`, `fun_free`.
Ownership model (documented at the module top): the context owns the heap; `alloc_words` hands out
stable raw pointers; **only this module dereferences them**.

### The JIT tiers (`src/jit.rs`, feature-gated)
- `pub trait JitBackend { fn compile(...) -> Result<...>; fn available(&self) -> bool; }` (design
  placeholder) and `pub fn available_tiers() -> &'static [&'static str]` (returns `["interpreter"]`
  unless a JIT feature is enabled).
- Cargo features: **`jit-cranelift` (default OFF)** gates the optional Cranelift deps;
  `jit-beamasm` is a documented stub. With no JIT feature the crate builds and tests with **no
  Cranelift dependency at all** — the build and the vertical slice never depend on Cranelift codegen.

### Error interop (`src/error.rs`)
- Engine error constructors over `beam_common::BeamError` (never a crate-local enum):
  `unsupported(what: &str) -> BeamError` (`Raise(error, reason atom "unsupported_opcode")`),
  `bad_register(n: u32) -> BeamError`, `unknown_label(Label) -> BeamError`.

## Execution model (frozen semantics — reproduce, don't reinvent)
### Registers & stack (`beam_emu.c`, `emu/macros.tab:116-147`, `:236-247`)
- **X registers**: `x[0..MAX_REGISTER)`, args in `x[0..arity)`, result in `x(0)`. Root set is
  `x[0..live)` (per `Instr::..{live}`).
- **Stack frame**: `allocate`/`allocate_heap`/… push a frame of **`stack_need + 1` words**: a
  *continuation-pointer slot* followed by `stack_need` Y-registers. `y(k)` is the slot after the CP
  slot (`E[1+k]`; frame diagram `emu/macros.tab:121-140`). The engine models the CP slot as a typed
  [`Continuation`] on the frame rather than a raw stack word; the Y slots live in the stack.
- **`call`** (non-tail): store the continuation in the *current* frame's CP slot, then jump.
- **`return`**: read the continuation from the *current* frame's CP slot, clear it, jump there. A
  frame-less leaf (e.g. `add/2`) uses a bare `return` and reads the *caller's* CP slot.
- **`deallocate(n)`** / **`call_last{n}`** / **`call_ext_last{n}`**: pop the frame
  (`n` Y slots **+ the CP slot**); a following `return` then reads the caller's CP slot
  (`emu/instrs.tab:119-126` `dealloc_ret` pops `(n+1)` words; `beam_disasm` shows the operand as the
  Y-slot count). Reaching the **bottom frame** halts execution and yields `x(0)`.
- **`trim{n, remaining}`**: `E += n` then clear the CP slot (`emu/instrs.tab:593-604`); `remaining`
  is JIT-only and ignored by the interpreter.
- **`test_heap{heap_need, live}`**: `ctx.ensure_heap(heap_need, live, x, stack)`; no stack change.

### Instruction subset (`Instr` variants) — **executed**
`Label`, `Line`, `FuncInfo`, `IntCodeEnd` (metadata/no-ops), `Move`, `GcBif`, `Test` (**all**
`TestKind`s), `SelectVal`, `Swap`, `Call`, `CallLast`, `CallOnly`, `CallExt`, `CallExtOnly`,
`CallExtLast`, `Allocate`, `AllocateHeap`, `TestHeap`, `Deallocate`, `Trim`, `Return`.
- **`GcBif` failure semantics**: resolve `bif: ImportRef` → `Module.imports[bif]` →
  `(module_name, function_name, arity)` via `Module::atom_name`, evaluate `args`, call `ctx.call_bif`.
  On `Ok`, store in `dst`. On `Err`: if `fail != NO_FAIL` **jump to `fail`** (guard failure); if
  `fail == NO_FAIL` **propagate the error** (raise). (Mirrors `FAIL_HEAD_OR_BODY`, `emu/macros.tab:266`.)
- **`Test` failure semantics**: the test *succeeds* ⇒ fall through; it *fails* ⇒ jump to `fail`
  (which jumps to the next clause). Order: `is_lt`/`is_ge`/`is_eq`/`is_ne` compare `args[0]` vs
  `args[1]` (verified against the oracle: `{test,is_lt,F,[{x,0},{x,1}]}` fails when `not (x0 < x1)`);
  `IsEq`/`IsNe` use arithmetic equality (`cmp_arith`), `IsEqExact`/`IsNeExact`/`SelectVal` use exact
  (`cmp`). Type tests take one arg; `TestArity` takes `[value, integer]` (`test_arity t S`).
  `fail == NO_FAIL` on a *failing* test raises `error::unsupported("test/no_fail")` (never panics).
- **`SelectVal`**: exact-match (`=:=`, `cmp`) `value` against each `(v, label)`; else jump to `fail`.

### Operand subset — **supported**
`X(n)`, `Y(n)`, `Integer(i)` (`make_small`), `Atom(a)` (`make_atom`), `Nil` (`NIL`),
`Character(c)` (`make_small(c)`), `Typed{reg, ty}` (read `reg`, ignore the type annotation),
`FloatReg(n)` (read/write the [`Machine`] FR file; out-of-range → `bad_register`).

### Operand subset — **NOT supported (documented error, never a wrong result/panic)**
`Literal` (loader does not decode literals — a `beam_loader` gap), `Label`, `List`/`Allocation`
(only inside `Generic`).

### Instruction subset — **NOT executed (documented error `unsupported_opcode`)**
`Instr::Generic` (every opcode outside the named set: `put_tuple2`, `get_tuple_element`, `get_list`,
`put_list`, `get_map_elements`, `make_fun*`, `apply*`, `call_fun*`, `bs_*`, `catch`/`try_case`,
`is_boolean`/`is_constant`/…, `executable_line`, `line` variants, opcode 0, …). The error carries
`Opcode::name()`.

## Constraints
- **Errors: `beam_common::{Result, BeamError}` only** — no crate-local error enum (workspace §3).
  Unsupported opcode/operand ⇒ a documented `BeamError` (`error::unsupported`); **never panic, never
  `unwrap()` on loaded data, never a wrong result**.
- **Frozen term layout**: term construction/inspection is **only** `beam_term`
  (`use beam_term::prelude::*;`). Comparison uses `beam_term::{cmp, eq, cmp_arith}` — the engine
  never reimplements term ordering.
- **`unsafe` is confined to two helper modules.** The crate is `#![deny(unsafe_code)]` with exactly
  two `#[allow(unsafe_code)]` modules:
  - `src/predicates.rs` — wraps the `beam_term` boxed-term predicates the `Test`/`test_arity`
    opcodes need (`is_integer`, `is_number`, `is_float`, `is_tuple`, `is_tuple_arity`, `is_map`,
    `is_binary`, `is_bitstring`, `is_internal_ref`, `is_external_pid`/`_port`/`_ref`), which are
    **`unsafe fn`** because they dereference a term's header word — there is no safe classifier.
  - `src/terms.rs` — the crate's single point of **raw-heap boxed-term construction/reading** behind
    the `ExecutionContext::alloc_words` seam (see API Surface / Design Decisions).
  Each `unsafe` block carries a `// SAFETY:` rationale (the term is a live machine word — an
  immediate is handled by the safe fast path first; a boxed term comes from
  `ctx.call_bif`/machine/`alloc_words` state, never unvalidated raw memory). Registers/stack stay
  safe `Vec<Eterm>` indexing; `unsafe_op_in_unsafe_fn` stays clean.
- **Deps:** `beam_common`, `beam_term`, `beam_loader` (+ `libc`); **`cranelift-*` are optional**
  behind the `jit-cranelift` feature (default off). Adding a dependency edge/size change is reported
  to the crate-layer node (`../`).
- **Files ~1000 lines**, split by responsibility (context seam / machine+frames / interpreter / jit
  stub / error). Licensing Apache-2.0.

## Test Strategy
- Per-module `#[cfg(test)]` unit tests: `context` (hook dispatch, default-hook unsupported, the
  `alloc_words` arena pointer stability/aliasing, `apply` forwarding, and the `alloc_words`/`apply`
  defaults); `machine` (x/stack accessors, frame push/pop/return, `MAX_REGISTER` bound, and the FR
  file: length/default fill, `get_fr`/`set_fr` round-trip + bounds, `reset` clears it); `terms` (every
  constructor↔reader round-trip through `MinimalContext`, raw-word layout proofs against stack
  buffers, and the error cases: flatmap >32 / fun field >255 / binary >64 → `unsupported`/`SystemLimit`,
  wrong-type readers → `BadArg`); `operand` (read/write each supported operand kind incl. `FloatReg`↔FR
  file + out-of-range; `Literal`/`Label`/`List`/`Allocation` → unsupported); `interpreter` (per-opcode
  semantics for `Move`, `Swap`, every `TestKind`, `SelectVal`, `Allocate`/`AllocateHeap`/`TestHeap`/
  `Deallocate`/`Trim`, `Call`/`CallLast`/`CallOnly`/`CallExt*`/`Return`, `GcBif` ok + guard-fail +
  raise, `Generic` → unsupported). Synthetic modules are built by hand for the ops the fixture does
  not exercise (esp. `SelectVal`, `Allocate`/`Deallocate`/`Trim`, `call_ext*`).
- **Vertical-slice integration** (`tests/slice.rs`): load the committed fixture
  `../beam_loader/tests/fixtures/slice.beam` (reused, **not duplicated**, via
  `concat!(env!("CARGO_MANIFEST_DIR"), "/../beam_loader/tests/fixtures/slice.beam")`), build a
  `MinimalContext` whose hook implements `erlang:'+'/2` and `erlang:'-'/2` over small ints, and
  assert `slice:add(2,3) == 5`, `slice:fib(10) == 55`, `slice:pick(a) == 1`, `slice:pick(b) == 2`.
- **Gate (GREEN required):**
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine` and
  `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace`
  (+ `-p beam_engine --features jit-cranelift` must still build — the JIT feature must not break the
  build). Always go through `../../scripts/cargo.sh` (`$HOME` read-only), never `cd`, never touch
  `../tools/` or `../../erts/`.
- Full differential cross-checks (`tools/differential`) are the M1 **compatibility gate** at the
  project level; this crate's tests are the engine-level evidence.

## Design Decisions
- **`ExecutionContext` seam with the root-set contract made concrete.** `call_bif` isolates BIF
  dispatch and `ensure_heap(heap_need, live, &mut x, &mut stack)` hands the exact GC root set
  (`x[0..live)` + the whole stack, §4) to the context, so a Stage-4 collector can move terms in
  place without frame liveness maps. `MinimalContext` is the test/embedding impl with unbounded heap.
- **Safe `match`-based dispatch, not computed-goto.** The IR is pre-decoded (typed `Instr` per
  opcode with operands resolved at load time), so dispatch is one `match` arm per op — the
  direct-threaded *model* of §F, implemented without `unsafe`/`goto`. A future asm/JIT tier may use
  threaded code; the interpreter need not.
- **Typed frame CP slot, not a raw stack word.** `allocate` reserves `stack_need+1` words and the
  CP slot is modeled as a typed `Continuation` (`Address{func, pc}` or `Halt`) on the frame; Y slots
  are the stack words. This reproduces the C frame exactly while staying type-safe.
- **Module-wide label index.** `Call*`/`CallLast`/`CallOnly` targets and `fail` labels are
  module-local and can cross functions (`fib/1` → `fib/3`), so the machine builds a
  `Label → (function index, code index)` map from `Module.functions[*].labels` per `run_*` call
  (labels are module-unique code addresses).
- **JIT tiers off the critical path.** Cranelift is an optional feature (default OFF); with it off
  the crate and the entire build/test flow carry no Cranelift codegen. JIT compilation is
  measurement-gated (Part II §F) and a bounded-latency, cheap-deopt design; M1 ships stubs +
  the tiering design only.
- **Reuse the loader fixture; do not duplicate it.** The slice `.beam` lives with `beam_loader`
  (single source of truth for the oracle image); `beam_engine` reads it by relative path.
- **Unsupported ⇒ `Raise` with reason atom `unsupported_opcode`.** `beam_common::BeamError` has no
  dedicated variant and must not gain one from here; the engine's `error::unsupported` raise is the
  documented sentinel, consistent with "no silent swallowing, no panic".
- **The FR file stores `Eterm` term slots at M1.** The C float-register file is raw doubles
  (`FloatDef`), but the engine's operand seam is `Eterm`-typed, so a term-slot FR file lets
  `read_operand`/`write_operand` support `FloatReg` uniformly (the raw-double model is a deferred
  refinement). Float arithmetic still boxes/unboxes via the `terms` float helpers.
- **Term construction lives behind `alloc_words` + `terms`.** The engine previously never allocated
  heap terms itself (only BIFs, via the context); the widened interpreter needs to build boxed terms
  (tuples/lists/floats/maps/funs), so the seam grew `alloc_words` (default unsupported) and the
  SAFETY-annotated `terms` module is the sole dereferencer of that raw storage. `apply` (default →
  `call_bif`) is added so the future `call_ext`/`apply` path has a hook without a trait break.
- **M1 fun layout inlines the descriptor.** C's boxed fun points out-of-line at an `ErlFunEntry`/
  `Export` (code address + `uniq`/`index`); M1 has no code pointers, so `make_fun` inlines the
  descriptor as two words (`uniq`, `index`) after the header — the same documented "M1 substitution"
  pattern as the external-pid `node` slot (`erl_term.h` `MAKE_FUN_HEADER` + `erl_fun.h` `ErlFunThing`).

## Known Issues / Intentional M1 gaps
- **No garbage collection.** `MinimalContext` has an unbounded heap; `test_heap`/`allocate_heap` call
  `ensure_heap` (a no-op there). The real collector + heap sizing is Stage 4 (`beam_gc`); the engine
  never allocates heap terms itself (only BIFs do, through the context).
- **No in-VM exceptions/catch.** A `GcBif` error with a live fail label branches (guard failure); an
  error with `NO_FAIL` propagates out of `run_export` as `Err(BeamError)`. `try`/`catch`/`raise`/
  `try_case`/stacktrace opcodes are `Generic` ⇒ unsupported.
- **No floats** (`FloatReg` unsupported; `freg` file not modeled), **no literals**
  (`Operand::Literal` unsupported — the loader also does not decode them), **no funs/lambdas**,
  **no maps/bitstrings/records** (their opcodes are `Generic`). These are the documented M1 coverage
  edges; each returns a typed error, never a wrong result.
- **No reductions / preemption / scheduler.** `run_*` executes to the function's `return`; process
  scheduling, reductions and trap handling are Stage 5 (`beam_process`).
- **`ensure_heap` underflow/limits not enforced by the machine** — stack growth is an unbounded
  `Vec` in M1; a configurable stack limit + `system_limit` raise is a later refinement.
- **Cross-crate duplications to avoid:** operand read/write, term comparison and the frame model live
  here; term construction/comparison must stay in `beam_term` (do not copy).

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/context.rs` → the `ExecutionContext` seam trait + `MinimalContext` (BIF hook, unbounded heap)
- `src/machine.rs` → `Machine`: x-register file, Erlang stack, call frames, `run_export`/`run_label`, `Continuation`
- `src/operand.rs` → `Operand` read (→ `Eterm`) / write (register or Y slot) + unsupported-operand errors
- `src/interpreter.rs` → the dispatch loop (`impl Machine`): one arm per `Instr` variant, jump/return/allocate/trim semantics
- `src/predicates.rs` → the crate's **only** `#[allow(unsafe_code)]` module: SAFETY-annotated wrappers over the `unsafe fn` `beam_term` boxed-term predicates (`is_integer`, `is_tuple`, `is_map`, …) used by the `Test` opcodes
- `src/jit.rs` → JIT tier stubs + tiering design (Cranelift behind `jit-cranelift`, default off)
- `src/error.rs` → engine error constructors over `beam_common::BeamError` (`unsupported`, `bad_register`, `unknown_label`)
- `tests/slice.rs` → vertical-slice integration test over the `beam_loader` slice fixture
- `tests/opcodes.rs` → synthetic-module tests for the opcodes the fixture does not reach (every `TestKind`, `SelectVal`, frame/call/`Deallocate`/`Trim`, `call_ext*`, error paths); a comprehensive test module, legitimately long
- `../beam_loader/CONTEXT.md` → the frozen IR this crate consumes (read it fully; do not restate)
- `../../erts/emulator/beam/emu/{instrs,macros,bif_instrs,select_instrs}.tab` + `beam_emu.c`, `beam_common.c` → ground-truth instruction semantics (read-only)
- `../../erts/emulator/beam/emu/ops.tab`, `../../lib/compiler/src/genop.tab` → opcode roster / transforms (read-only)

## See Also
- `../CONTEXT.md` → crate-layer contracts, the seam types, the Stage-1/Stage-5 DAG, wave table.
- `../../CONTEXT.md` §4 → the engine IR seam and the root-set contract (`x[0..live)` + whole stack, no liveness maps).
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §E/§F/§G → IR seam, interpreter/JIT trade-offs, tiering rationale.
- `../beam_loader/CONTEXT.md` → the IR types (`Module`/`Function`/`Instr`/`Operand`/`TestKind`) and their exact meaning.
- `../beam_term/CONTEXT.md` → `Eterm`, `make_small`/`make_atom`/`NIL`, `cmp`/`eq`/`cmp_arith` (the engine's term API).
- `../beam_rt/` → the Stage-5 runtime that supplies the concrete `ExecutionContext`.
