# `beam_engine` — Typed-IR Execution Engine (interpreter + JIT tiers)

## Intent
Executes the **frozen typed IR** produced by `beam_loader`: a **direct-threaded interpreter** that runs a widened instruction set end to end (this crate's Stage-1 deliverable), behind a real **execution-tier seam** (`src/jit/`: `Tier` / `JitBackend` / `available_tiers()` / `Machine::run_with_tier`) the native JIT tiers plug into. The **native baseline tier is built** (`src/jit/baseline.rs`: each basic block is lowered to a real x86-64 `extern "C"` function and an export runs through a driver loop that deopts to one interpreter step per instruction it does not lower natively). The **Cranelift hot tier is not** (`jit-cranelift` is a typed `unsupported` error, and its deps sit behind a default-off feature). Both tiers stay **measurement-gated** and off the critical path (Part II §F): the baseline backend is a correctness-first native *mechanism*, not an optimiser — inlining and cross-run code caching are later work.
Implements the Stage-1 engine half of Part II §H with the IR contract of §E and the interpreter/JIT trade-offs of §F (`../../CONTEXT.md` §4).
This crate is on the **critical path of the vertical slice**: it loads a real `.beam` (via `beam_loader`) and runs it end to end.
The dispatch loop (`src/interpreter.rs`) has explicit arms for the core opcodes and routes every other instruction to a per-instruction-family seam (`src/families/`), so a whole opcode family is implemented inside its own module without touching the loop.

**The engine depends only on `beam_loader` (+ `beam_term`/`beam_common`)** — never on the Stage-5 runtime core.
Everything the runtime must supply (BIF dispatch, fun application, heap/GC, process/stack limits) is abstracted behind the [`ExecutionContext`] trait; `beam_rt` provides the concrete implementation.
This keeps the Stage-1/Stage-5 DAG acyclic and lets the engine be tested with a minimal context.

Ground truth (read-only, never modify): the interpreter dispatch `../../erts/emulator/beam/emu/beam_emu.c`, the instruction bodies `../../erts/emulator/beam/emu/{instrs,macros,bif_instrs,select_instrs,arith_instrs,map_instrs,float_instrs,bs_instrs}.tab`, and the call/return/allocate, error and `next_catch` conventions of `../../erts/emulator/beam/beam_common.c`.

## API Surface (FROZEN — the runtime and tests design against this)
Re-exported at the crate root and via `beam_engine::prelude::*`.
**This crate does NOT embed a BIF table**: all computation goes through `ExecutionContext::call_bif`, and fun application through `ExecutionContext::apply_fun`.

### The context seam (`src/context.rs`)
```rust
pub trait ExecutionContext {
    /// Dispatch `module:function/arity`; failure is return-based `Err(BeamError)`.
    fn call_bif(&mut self, module: &str, function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm>;

    /// Ensure `heap_need` words; the live root set is `x_regs[0..live)` + the whole `stack`.
    fn ensure_heap(&mut self, heap_need: u32, live: u32, x_regs: &mut [Eterm], stack: &mut [Eterm]) -> Result<()>;

    /// Heap words currently available (introspection). Default: unbounded.
    fn heap_available(&self) -> usize { usize::MAX }

    /// Allocate `n` uninitialized words; valid only until the next `ensure_heap`/GC.
    /// Default: a typed `error::unsupported`.
    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm>;

    /// The general `apply`/`call_ext` hook. Default: forwards to `call_bif`.
    fn apply(&mut self, module: &str, function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm>;

    /// Apply a fun/closure value (the `call_fun`/`call_fun2` path). Default: `error::unsupported`.
    fn apply_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm>;
}
```
- `call_bif` failure is an `Err(BeamError)` (a `BadArith`/`BadArg`/`Raise`/…): the engine turns it into a guard branch or a raise, never a panic.
- `ensure_heap(heap_need, live, x_regs, stack)` makes the root-set contract concrete: the live roots are `x_regs[0..live)` plus the whole `stack`; a real context (`beam_rt`, Stage 4/5) runs the collector here and may **move terms** in place, updating `x_regs`/`stack` without frame liveness maps.
- `alloc_words(n)` hands out a stable pointer to `n` **uninitialized** words, valid only until the next `ensure_heap`/GC (mirrors `HAlloc`/`heap_top`); the default body is a typed `error::unsupported`.
- `apply` is the general external-function hook (default forwards to `call_bif`), and `apply_fun` is the fun-application hook (default a typed `error::unsupported`, since the engine keeps no closure registry).
- `pub type BifHook = Box<dyn FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm>>`.
- `pub type ApplyHook = Box<dyn FnMut(Eterm, &[Eterm]) -> Result<Eterm>>`.
- `MinimalContext` — the test/embedding context: an unbounded heap (`ensure_heap` is a no-op `Ok(())`), a **pluggable BIF hook** and an **optional apply hook**.
- `MinimalContext::new()` installs a default BIF hook that rejects every BIF with `error::unsupported`; `with_bif_hook(hook)`/`set_bif_hook(hook)` replace it.
- `MinimalContext::with_apply_hook(hook)`/`set_apply_hook(hook)` install the apply hook that serves `apply_fun`; without one `apply_fun` is a typed `error::unsupported`.
- `MinimalContext::alloc_words` hands out words from a pointer-stable arena (`Vec<Box<[Eterm]>>`); the boxes are never freed before the context drops, so a handed-out pointer stays valid for the context's lifetime.
- The context **owns the heap** and must outlive every boxed term the engine builds through it.

### The machine (`src/machine.rs`)
- `pub struct Machine` — the process-state core: the **x-register file**, the **float-register (FR) file**, the **Erlang stack**, the **call-frame stack** and the **exception catch-frame stack**.
- `pub const MAX_REGISTER: u32 = 1024;` — the x-register file size (`beam_emu.c` `MAX_REG` bound).
- `pub const MAX_FLOAT_REGISTER: u32 = 1024;` — the FR file size, mirroring C's `f_reg_array` (`FloatDef d[MAX_REG]`, `erl_process.h`; `MAX_REG = 1024`, `erl_vm.h:49`).
- The FR file (`fr: Vec<Eterm>`, (re)filled by `reset`) holds **`Eterm` term slots** at M1 (see Design Decisions).
- `Machine` also binds the **running module's decoded literal area** (`literals: Option<SharedLiteralArea>`, re-bound by `run_loop` from the `&Module` being executed, cleared by `reset`), exposed as `Machine::literal(index: u32) -> Option<Eterm>` (delegates to `LiteralArea::get`). The binding cannot go stale mid-run: labels are module-local and a cross-module call leaves the loop through `CallExt` into the host context, so the executing module is fixed for a run.- `Machine::new()` / `Machine::default()` / `Machine::with_max_register(u32)`; `get_fr(n) -> Option<Eterm>` / `set_fr(n, Eterm) -> Result<()>` are bounds-checked FR access (`bad_register` on overflow).
- `pub fn run_export(&mut self, module: &Module, function: Atom, arity: u32, args: &[Eterm], ctx: &mut dyn ExecutionContext) -> Result<Eterm>` — the primary entry: resolve `function/arity`'s export entry label, seed `x[0..arity]` from `args`, push the bottom frame, run to `return`, yield `x(0)`.
- `pub fn run_label(&mut self, module: &Module, label: Label, arity: u32, args: &[Eterm], ctx) -> Result<Eterm>` — run a raw entry label (used by `run_export`); a missing export/label is a typed error.
- The dispatch loop itself is `pub(crate) fn run_loop` / `run_loop_with_map` (implemented in `src/interpreter.rs`).
- Introspection (read-only): `x(n) -> Option<Eterm>`, `get_fr(n) -> Option<Eterm>`, `frame_depth() -> usize`, `stack_depth() -> usize`, `catch_frame_depth() -> usize`, `pending_exception() -> Option<(ExceptionClass, Eterm)>`, `pending_class() -> Option<ExceptionClass>`, `resumption_addresses() -> Vec<(u32, u32)>` (every live call frame's suspended `(func, pc)` CP slot, innermost first — used by `build_stacktrace`).
- `pub fn reset(&mut self)` restores the initial state: NIL-filled register and FR files, empty stack and frame stacks, no pending exception and no resolved class atoms.
- `pub struct CatchFrame { pub func: u32, pub pc: u32, pub frame_index: usize }` (+ `CatchFrame::new`) — one `try`/`catch` handler target: the handler's `(func, pc)` and the call frame that **owns** it (the frame the machine unwinds back to).
- `pub fn push_catch_frame(&mut self, frame: CatchFrame)` / `pub fn pop_catch_frame(&mut self) -> Option<CatchFrame>` — the public catch-stack accessors.
- `pub(crate) fn throw(&mut self, class, reason) -> Result<Step>` / `throw_error(reason) -> Result<Step>` / `throw_with_trace(class, reason, trace) -> Result<Step>` — **the one uniform raise/unwind entry point** the families call (see the exception model below).

### The `Step` protocol (`src/step.rs`)
- `pub(crate) enum Step { Next, Goto { func: u32, pc: u32 }, Done }` — the control-flow contract between the dispatch loop and the instruction families.
- An instruction body performs its register/heap effect and then returns the `Step` telling the loop where to continue; it never mutates the loop's `func`/`pc` directly.
- `Step::Next` continues at `pc + 1`; `Step::Goto { func, pc }` jumps within the module; `Step::Done` finishes the run and yields `x(0)`.
- A raise is **not** a `Step`: it stays an `Err(beam_common::BeamError)`, or the exception layer folds it into a `Step::Goto` to a live catch handler.

### The families seam (`src/families/`)
- Every `Instr` the core loop does not execute natively is routed to `families::dispatch(machine, module, labels, instr, func, pc, ctx) -> Result<Step>`.
- `dispatch` tries the eight families in a fixed order — `tuples`, `records`, `lists`, `funs`, `exceptions`, `floats`, `maps`, `bitstrings` — and returns the first family's `Step`.
- Each family exposes `pub(crate) fn exec(machine: &mut Machine, module: &Module, labels: &BTreeMap<Label, (u32, u32)>, instr: &Instr, func: u32, pc: u32, ctx: &mut dyn ExecutionContext) -> Result<Option<Step>>`.
- `Ok(Some(step))` means "this family executed the instruction"; `Ok(None)` means "not this family's variant"; `Err` means the family raised.
- When **no** family claims an instruction, `dispatch` returns `error::unsupported("widened opcode not yet executed")` — the same documented sentinel the loop emitted before the seam existed.
- The seam exists for **edit locality**: a family is implemented or extended entirely inside its own module, and adding a family is a one-line change in `families/mod.rs`.
- The `families/` module docs list each family's owned variants; keep that list and this crate's Routing Table in sync when a family changes.

### Term helpers (`src/terms.rs`) — the SAFETY-annotated builder/reader
Constructors take the seam (`ctx: &mut dyn ExecutionContext`, allocating via `alloc_words`):
- `make_tuple(ctx, &[Eterm]) -> Result<Eterm>` (arityval header + elements; empty → `tuple0`; > `MAX_ARITYVAL` → `SystemLimit`).
- `make_cons(ctx, head, tail) -> Result<Eterm>` (one header-less `[CAR,CDR]` cell; the tail need not be a list).
- `make_list(ctx, &[Eterm]) -> Result<Eterm>` (cons cells; empty → `NIL`).
- `badmatch_reason(ctx, module, value) -> Result<Eterm>` (`{badmatch, Value}`, the shared raise reason; `BeamError::BadMatch` when the module's atom table lacks `badmatch`).
- `make_float(ctx, f64) -> Result<Eterm>`.
- `make_flatmap(ctx, &[(Eterm, Eterm)]) -> Result<Eterm>` (>32 pairs → typed `unsupported("map > 32 pairs (no HAMT builder)")`; no crate in the rewrite implements the HAMT builder — see Known Issues).
- `make_fun(ctx, arity: u32, uniq: u64, index: u32, free: &[Eterm]) -> Result<Eterm>` (minimal local closure; M1 inlines the out-of-line fun-entry descriptor — see Design Decisions).
- `make_heap_binary(ctx, &[u8]) -> Result<Eterm>` (heap binary/bitstring, ≤64 bytes).

Readers (no `ctx`; wrong type / out-of-range → `BeamError::BadArg`):
`tuple_arity`, `tuple_element`, `cons_head`, `cons_tail`, `list_elements`, `float_value`, `map_entries` (flatmap **and** hashmap/HAMT — walks the frozen `erl_map.h` node tree), `binary_bytes`, `is_fun`, `fun_arity`, `fun_num_free`, `fun_uniq`, `fun_index`, `fun_free`.
Ownership model (documented at the module top): the context owns the heap, `alloc_words` hands out stable raw pointers, and **only this module dereferences them**.

### The JIT tier seam (`src/jit/mod.rs`)
- `pub enum Tier { Interpreter, BaselineJit, Cranelift }` (`Clone, Copy, Debug, PartialEq, Eq`; `Tier::ALL` is the roster order, interpreter first): `Tier::name() -> &'static str` (`"interpreter"` / `"baseline-jit"` / `"jit-cranelift"`), `Tier::from_name(&str) -> Option<Tier>` (unknown name ⇒ `None`, never a default tier) and `Tier::is_available() -> bool` (the tier is in this build's roster).
- `pub trait JitBackend` — the seam a **native** tier implements:
  ```rust
  pub trait JitBackend {
      type Compiled;
      fn name(&self) -> &'static str;
      fn available(&self) -> bool;
      fn compile(&mut self, module: &Module) -> Result<Self::Compiled>;
      fn run_export(&mut self, machine: &mut Machine, module: &Module, function: Atom,
                    arity: u32, args: &[Eterm], ctx: &mut dyn ExecutionContext) -> Result<Eterm>;
  }
  ```
  `&mut self` because a backend owns its code buffer/caches; `Compiled` is the backend's opaque per-module artifact. The interpreter is deliberately **not** a `JitBackend` (nothing to compile).
- `pub fn available_tiers() -> &'static [&'static str]` — the tiers compiled into this build: exactly `["interpreter", "baseline-jit"]`, plus `"jit-cranelift"` when the **`jit-cranelift`** feature is enabled. Interpreter always first; the roster is a prefix of `Tier::ALL` mapped to `Tier::name`.
- `Machine::run_with_tier(..., tier: Tier) -> Result<Eterm>` (`src/machine.rs`) is the tier-selecting entry: `Interpreter` delegates to `run_export` (which stays the default interpreter entry), `BaselineJit` runs the `src/jit/baseline.rs` backend, `Cranelift` is `error::unsupported("jit-cranelift tier not built")` — a typed error, never a panic and never a silent fallback.
- Cargo features: **`jit-cranelift` (default OFF)** gates the optional Cranelift deps. With it off the crate builds and tests with **no Cranelift dependency at all** — the build and the vertical slice never depend on Cranelift codegen. `jit-beamasm` is a **documented alias for the baseline tier** (the baseline tier is in every build's roster, so the feature adds nothing; it is kept so `--features jit-beamasm` keeps working).
- `src/jit/emitter.rs` is the encodable **building block** the native tiers are built on: an `#[allow(unsafe_code)]` x86-64 encoder plus an executable `CodeBuffer`. It exposes `Reg` (16 GPRs), `Cond` (16 condition codes), `Label`, `CodeBuffer::{new, with_capacity, label, bind, seal, is_sealed, len, capacity, code_ptr, as_fn_ptr}`, and the instruction set `mov_rr`/`mov_ri` (movabs)/`mov_rm`/`mov_mr`/`lea`, `add_rr`/`sub_rr`/`cmp_rr`/`test_rr`/`and_rr`/`or_rr`/`xor_rr`, `push_r`/`pop_r`, `jmp`/`jcc`/`call_label`/`call_r`/`ret`, `nop`/`int3`. Everything returns `beam_common::Result` (`Io` for a failed `mmap`/`mprotect`, `SystemLimit` for a full buffer or an unrepresentable `rel32`, `BadArg` for misuse: an unbound label at `seal`, a foreign/doubly-bound label, or emitting after sealing). The mapping is `mmap(PROT_READ|PROT_WRITE)` → `mprotect(PROT_READ|PROT_EXEC)` on `seal` (never RWX), released with `munmap` in `Drop`. It assumes the **System V AMD64** ABI and emits `REX.W` + the REX.R/B extension bits for `r8`–`r15`, the `SIB` byte an `rsp`/`r12` base forces, and a uniform `mod=10` `disp32`. It is **not wired into the tier seam yet**: nothing in `Tier`/`Machine`/`JitBackend` refers to it, and its own `#[cfg(test)]` tests call the emitted code directly.
- The native backends are not implemented: a follow-up task adds `src/jit/baseline.rs` (which lowers the IR onto `emitter`) and `src/jit/cranelift.rs` as submodules of this one, each implementing `JitBackend`.

### Error interop (`src/error.rs`)
- Engine error constructors over `beam_common::BeamError` (never a crate-local enum): `unsupported(what: &str)` (a `Raise` with reason atom `unsupported_opcode: <what>`), `bad_register(n: u32)` (`BadArg`), `unknown_label(Label)` (`BadArg`).

## Execution model (frozen semantics — reproduce, don't reinvent)
### Registers, stack & frames (`beam_emu.c`, `emu/macros.tab:116-147`, `:236-247`)
- **X registers**: `x[0..MAX_REGISTER)`, args in `x[0..arity)`, result in `x(0)`; the root set is `x[0..live)` (per `Instr::..{live}`).
- **Stack frame**: `allocate`/`allocate_heap`/… push a frame of **`stack_need + 1` words** — a *continuation-pointer slot* followed by `stack_need` Y-registers; `y(k)` is the slot after the CP slot (`E[1+k]`).
- The engine models the CP slot as a typed `Continuation` (`Address{func,pc}` or `Halt`) on the frame rather than a raw stack word; the Y slots live in the stack.
- `call` (non-tail) stores the continuation in the *current* frame's CP slot, then jumps.
- `return` reads the continuation from the *current* frame's CP slot and jumps there; a frame-less leaf (e.g. `add/2`) uses a bare `return` and reads the *caller's* CP slot.
- `deallocate(n)` / `call_last{n}` / `call_ext_last{n}` pop the frame (the `n` Y slots **and** the CP slot, via `pop_frame_checked`); a following `return` then reads the caller's CP slot.
- Reaching the **bottom frame** (`Continuation::Halt`) halts execution and yields `x(0)`.
- `trim{n}` reproduces C's `i_trim` (`emu/instrs.tab:593`, `E += n`): it **discards the lowest `n` Y slots** and renumbers the survivors down (`y(k) → y(k−n)`), which is exactly what the compiler's `beam_trim.erl` assumes (`Eliminated = [{y,N} || N <- seq(0, Trim-1)]`, `expand_recipe` remaps `Dst−Trim`). The `remaining` operand is JIT-only and ignored by the interpreter.
- `test_heap{heap_need, live}` calls `ctx.ensure_heap(heap_need.words(), live, x, stack)` and leaves the stack unchanged; `allocate`/`allocate_heap` recover the frame's word count through `beam_loader::HeapNeed::words()`.

### The dispatch loop (`src/interpreter.rs`)
- Each iteration reads `module.functions[func].code[pc]` (typed `Instr`) and dispatches with a safe `match` (no computed-goto; the IR is pre-decoded, so typed dispatch is equivalent to a jump table).
- Core arms mutate the loop's `func`/`pc` in place; the catch-all arm calls `families::dispatch` and folds its `Step` back via the local `apply_step!`-style mapping.
- The module-wide `Label -> (function index, code index)` map is built from every `Function::labels` (`build_label_map`), so a call/fail target may live in another function (the slice's `fib/1` tail-calls into `fib/3`); a missing label is `error::unknown_label`.
- `import_target` resolves an `ImportRef` to `(module name, function name, arity)` through `Module::imports` + `Module::atom_name`.

### Instruction subset — **executed by the core loop**
`Label`, `Line`, `FuncInfo`, `IntCodeEnd`, `ExecutableLine`, `DebugLine` (metadata/no-ops: they do not change control flow).
`Move`, `Swap`, `InitYregs` (`init_yregs` writes `NIL` into the listed Y registers).
`Test` (**all** `TestKind`s, see below), `SelectVal` (exact `=:=` match against each `(value, label)`, else `fail`, else a raise when `fail == NO_FAIL`).
`Jump`, `Call`, `CallLast`, `CallOnly`, `CallExt`, `CallExtOnly`, `CallExtLast`, `Allocate`, `AllocateHeap`, `TestHeap`, `Deallocate`, `Trim`, `Return`.
`GcBif`, `Bif0`, `Bif`.
`Instr::Generic { op, .. }` is **not** executed: it returns `error::unsupported(op.name())`.
- **`GcBif`/`Bif` failure semantics**: evaluate the arg operands, call `ctx.call_bif`. On `Ok`, store into `dst`. On `Err` with a live `fail` label, **jump to `fail`** (guard failure). On `Err` with `fail == NO_FAIL`, **raise** via `raise_bif_error` (mirrors `FAIL_HEAD_OR_BODY`, `emu/macros.tab:266`).
- **`Bif0`/`CallExt*` failure semantics**: these carry no fail label, so **every** error is a raise through `raise_bif_error`.
- **`raise_bif_error`**: with no live catch frame (or a `BeamError::Raise`), the error propagates unchanged. With a live catch frame, the error's BEAM atom name (`BeamError::as_atom()`: `badarg`/`badarith`/`badarity`/`badmatch`/`system_limit`/`out_of_memory`) is resolved in the running module's atom table, turned into a term and raised with class `error` — the class every BIF failure carries. An error the engine cannot name as a term (no atom, a `Raise`, `TermNotFound`, `Io`, or an atom absent from the module) propagates unchanged rather than being substituted by a *different* atom.
- **`Test` failure semantics**: the test *succeeds* ⇒ fall through; it *fails* ⇒ jump to `fail`, or raise `error::unsupported("test/no_fail")` when `fail == NO_FAIL`.
- **`Test` comparison kinds**: `IsLt`/`IsGe`/`IsEq`/`IsNe` compare `args[0]` vs `args[1]` with arithmetic equality (`cmp_arith`); `IsEqExact`/`IsNeExact` use exact equality (`cmp`).
- **`Test` type kinds**: `IsInteger`, `IsFloat`, `IsNumber`, `IsAtom`, `IsPid` (internal or external), `IsReference` (internal or external), `IsPort` (internal or external), `IsNil`, `IsBinary`, `IsBitstr`, `IsList` (`[]` or a cons cell), `IsNonemptyList`, `IsTuple`, `IsMap`, `IsBoolean` (the atom `true`/`false`), `IsFunction`, `IsFunction2` (`args[0]` is a fun of `args[1]` arity), `IsTaggedTuple` (`args[0]` is a tuple of `args[1]` arity whose first element is `args[2]`), `TestArity` (`args[0]` is a tuple of `args[1]` arity).
- **`NifStart`/`OnLoad` are NOT executed** by the core loop and are claimed by no family, so reaching them yields the documented typed `unsupported` error.

### Operand subset — **supported**
- `X(n)`, `Y(n)` (bounds-checked; out of range → `error::bad_register`).
- `Typed{reg, ty}` (read/write `reg`, ignore the type annotation).
- `FloatReg(n)` (read/write the [`Machine`] FR file; out of range → `bad_register`).
- `Integer(i)`/`Character(c)` (`make_small`), `Atom(a)` (`make_atom`), `Nil` (`NIL`).

### Operand subset — **NOT supported by the general reader/writer**
- `Literal` (an index into the running module's decoded literal area) **is** read via `crate::operand::read_operand` (`Machine::literal`); only `Label`, `List` and `Allocation` are unsupported there → a typed `error::unsupported` from `read_operand`. `write_operand` rejects every non-register destination, `Literal` included (a literal has no addressable slot).

### Instruction families — **executed**
`families/tuples.rs`: `PutTuple2` (`put_tuple2`), `GetTupleElement` (`get_tuple_element`), `SelectTupleArity` (`select_tuple_arity` — a bad shape/arity raises `{badmatch, Value}` on the no-fail path).
`families/records.rs`: `UpdateRecord` (`update_record` — copy the `size`-arity source tuple and overwrite the element at each **1-based** `(offset, value)` pair, writing to `dst`). Copy-then-patch order, so an update value that aliases `dst` reads the *original* tuple; the compiler's `copy`/`reuse`/`inplace` hint is ignored (M1 never updates in place — equivalent, since `inplace` is only emitted for a unique/dead source). A non-tuple source, a size/arity mismatch, a non-integer offset, or an offset outside `1..=size` is a typed `unsupported`.
`families/lists.rs`: `PutList` (`put_list`), `GetList` (`get_list`), `GetHd` (`get_hd`), `GetTl` (`get_tl` — a non-cons source raises `{badmatch, Value}`).
`families/funs.rs`: `MakeFun3` (`make_fun3`; the closure's arity is the descriptor arity − `num_free`), `CallFun` (`call_fun`; fun in `x(arity)`, args `x(0..arity)`), `CallFun2` (`call_fun2`; fun is a `func` operand, the `tag` hint is ignored), `Apply` (`apply`; M in `x(arity)`, F in `x(arity+1)`, args `x(0..arity)`), `ApplyLast` (`apply_last`; pops the frame first, then follows the continuation). A non-atom `apply` target is a `badarg` raise routed through `Machine::throw_error` (`raise_badarg`), which lands in a live catch frame or propagates as the typed `BeamError::BadArg`.
`families/exceptions.rs`: `Badmatch` (`{badmatch,V}`), `CaseEnd` (`{case_clause,V}`), `TryCaseEnd` (`{try_clause,V}`), `IfEnd` (`if_clause`), `Badrecord` (`{badrecord,V}`), `Raise` (re-raise with the class currently being handled), `Try`/`Catch` (push a catch frame), `TryCase`/`TryEnd`/`CatchEnd` (the handler opcodes, each consuming exactly one catch frame), `BuildStacktrace` (cook `x(0)`: `NIL` ⇒ `NIL`; a non-`NIL` raw trace ⇒ an engine-derived symbolic trace from the live frames), `RawRaise` (class `error`/`exit`/`throw` in `x(0)` ⇒ raise `x(1)` with trace `x(2)`; any other class ⇒ `x(0) = badarg` and fall through — see Known Issues).
`families/floats.rs`: `Fmove` (`fmove`), `Fconv` (`fconv`), `Fadd`/`Fsub`/`Fmul`/`Fdiv`, `Fnegate` (the FR file keeps `Eterm` slots; a non-float operand or a non-finite result is the instruction's arithmetic failure — a guard branch to `fail` when one is live, else a `badarith` raise routed through `Machine::throw_error` (`raise_badarith`), which lands in a live catch frame or propagates as the typed `BeamError::BadArith`).
`families/maps.rs`: `GetMapElements`, `HasMapFields`, `PutMapAssoc`, `PutMapExact`. Sources are read in **both** layouts (`terms::map_entries` handles a flat map and a hashmap/HAMT); key matching delegates to `beam_term::cmp`, so `1` and `1.0` are distinct keys; a non-map source is a `fail` jump on the compiled forms, else `{badmap, Source}`. **Construction** goes through `make_flatmap` and is capped at 32 pairs — a larger result is a typed `unsupported` (no HAMT builder exists; see Known Issues).
`families/bitstrings/{mod,matching,create}.rs`: `BsStartMatch3`/`BsStartMatch4`, `BsMatch` (the flattened command list: `ensure_at_least`, `ensure_exactly`, `skip`, `integer`, `binary`, `get_tail`, `=:=`), `BsMatchString`, `BsTestTail2`, `BsGetTail`, `BsGetPosition`, `BsSetPosition`, `BsGetInteger2` (any width — ≤64 bits in a machine word, >64 as a `beam_term` bignum, big/little-endian and signed), `BsGetFloat2` (0/16/32/64 bits), `BsGetBinary2`, `BsSkipBits2`, the UTF reads/skips `BsGetUtf8`/`BsGetUtf16`/`BsGetUtf32` + `BsSkipUtf8`/`BsSkipUtf16`/`BsSkipUtf32`, and `BsCreateBin` (`integer` — any width —, `binary`, `float`, `utf8`/`utf16`/`utf32` segments; `string` constant segments are a documented gap).
- The match-context is an on-heap boxed term in the C `ErlSubBits` shape `[HEADER_SUB_BITS, 0, start, end, orig]`; all state is re-derived from the term at every opcode, so matching survives calls and branches without a side table.

### Raising opcodes and the catch-frame model (`families/exceptions.rs`, `src/machine.rs`)
- `try`/`catch` (`Try`/`Catch`) push a `CatchFrame { func, pc, frame_index }` (the handler's location plus the **current** call frame, i.e. the frame that owns the handler) and blank their frame slot.
- A raise (a family's `throw`/`throw_error`, or `raise_bif_error` for a BIF/ext-call error) records `pending_exception = (class, reason)`, then consults the **innermost** live catch frame.
- To land, the machine unwinds the call-frame stack back to `CatchFrame::frame_index` (dropping the Y slots of every deeper frame) and writes the handler registers exactly as C's `handle_error` does: `x(0) = THE_NON_VALUE`, `x(1) = reason`, `x(2) = trace` (`NIL` at M1), `x(3) = class atom`.
- A landed raise returns `Ok(Step::Goto { func, pc })` for the handler; with no live catch frame (or an unresolved class atom) it returns the typed `Err(BeamError::Raise(..))` — never a panic.
- **The raise never pops the catch frame**: it *reads* the innermost entry and unwinds the call frames, but leaves the entry in place; the handler opcode that runs next (`try_case`/`try_end`/`catch_end`) consumes exactly one entry (on the normal or the exceptional path, never both).
- `try_case` blanks its frame slot, requires the `THE_NON_VALUE` marker in `x(0)`, and moves the class atom from `x(3)` into `x(0)`.
- `try_end` just blanks its frame slot; `catch_end` leaves a normal result untouched but folds a landed exception into the caught term — the thrown value for class `throw`, `{'EXIT',Reason}` otherwise.
- The class atoms (`error`/`exit`/`throw`) are installed from the running module's atom table before a raise (`Machine::set_class_atom`); an atom absent from the module is never substituted by a *different* atom (an uncaught raise still propagates as a typed error, and a caught raise fails with a typed `unsupported`).
- `pending_class` is the class a `raise` opcode re-raises with, reproducing C's `i_raise` keeping `freason`.

## Constraints
- **Errors: `beam_common::{Result, BeamError}` only** — no crate-local error enum (workspace §3); an unsupported opcode/operand is a documented `BeamError` (`error::unsupported`), and the engine **never panics, never `unwrap()`s loaded data, never produces a wrong result**.
- **Frozen term layout**: term construction/inspection is **only** `beam_term` (`use beam_term::prelude::*;`); comparison uses `beam_term::{cmp, eq, cmp_arith}` — the engine never reimplements term ordering.
- The crate is `#![deny(unsafe_code)]` (and `#![warn(missing_docs)]`) with exactly **four** `#[allow(unsafe_code)]` modules:
  - `src/predicates.rs` — wraps the `beam_term` boxed-term predicates the `Test`/`test_arity` opcodes need (`is_integer`, `is_number`, `is_float`, `is_tuple`, `is_tuple_arity`, `is_map`, `is_binary`, `is_bitstring`, `is_internal_ref`, `is_external_pid`/`_port`/`_ref`), which are `unsafe fn` because they dereference a term's header word — there is no safe classifier.
  - `src/terms.rs` — the crate's single point of **raw-heap boxed-term construction/reading** behind the `ExecutionContext::alloc_words` seam (incl. the HAMT reader for `map_entries`).
  - `src/families/bitstrings/raw.rs` — the `ErlSubBits`/`ErlHeapBits` raw-layout accessors for the `bs_*` opcodes (reading a heap bitstring's data words, reading/writing the match-context fields, materialising a result bitstring with an explicit bit count, and the `>64`-bit bignum build/read helpers).
  - `src/jit/emitter.rs` — the baseline JIT's x86-64 instruction encoder and the crate's **only executable-memory mapping** (`mmap` RW → `mprotect` RX on `seal`, `munmap` in `Drop`); carries a file-local `#![allow(unsafe_code)]` rather than a declaration-site attribute.
  - Each `unsafe` block carries a `// SAFETY:` rationale (the term is a live machine word — an immediate is handled by the safe fast path first; a boxed term comes from `ctx.call_bif`/machine/`alloc_words` state, never unvalidated raw memory); registers/stack stay safe `Vec<Eterm>` indexing and `unsafe_op_in_unsafe_fn` stays clean.
- **Deps:** `beam_common`, `beam_term`, `beam_loader` (+ `libc`); **`cranelift-*` are optional** behind the `jit-cranelift` feature (default off). Adding a dependency edge/size change is reported to the crate-layer node (`../`).
- **Files ~1000 lines**, split by responsibility; a legitimately long file (`tests/opcodes.rs`, the comprehensive synthetic-opcode module) documents why.
- Licensing Apache-2.0.

## Test Strategy
- Per-module `#[cfg(test)]` unit tests: `context` (hook dispatch, default-hook unsupported, the `alloc_words` arena pointer stability/aliasing, `apply`/`apply_fun` forwarding and their defaults, the apply hook); `machine` (x/stack/FR accessors, frame push/pop/return, `MAX_REGISTER` bound, catch-frame push/pop and the throw/unwind/landing paths, `pending_exception`/`pending_class`, `resumption_addresses`); `step`; `terms` (every constructor↔reader round-trip through `MinimalContext`, raw-word layout proofs, the HAMT reader over hand-built nodes, and the error cases: flatmap >32 / fun field >255 / binary >64 → `unsupported`/`SystemLimit`, wrong-type readers → `BadArg`); `operand` (read/write each supported operand kind incl. `FloatReg`↔FR file + out-of-range, `Literal` via `Machine::literal`, and a missing/unbound literal → typed error; `Label`/`List`/`Allocation` → unsupported); `interpreter` (the label map + resolution); `jit` (the roster invariants — interpreter first, `"baseline-jit"` always present, `"jit-cranelift"` iff the feature is on, the roster is a `Tier::ALL` prefix; `Tier::name`↔`from_name` round-trips + unknown/empty/miscased names ⇒ `None`; `run_with_tier` on the reused `slice.beam` fixture — `Interpreter` equals `run_export`, `BaselineJit` equals it too, `Cranelift` is a typed error); `jit::emitter` (the encoder is proven by *executing* what it emits: `add(a,b)`, a six-argument sum through `r8`/`r9`, `r12`- and `rsp`-based addressing, `lea`, `max` via `cmp`+`jcc`, an indirect `call_r` into a second buffer, forward/backward/conditional label patching, `seal` flips `/proc/self/maps` from `rw-p` to `r-xp`, plus the error paths — unbound label at `seal`, emit-after-`seal`, foreign/double-bound label, exhausted capacity, repeated map/seal/drop); `families/*` (per-family unit tests alongside each family body).
- **Per-family integration tests over committed real `.beam` fixtures** (produced by the system `erlc`, the differential oracle) — see `tests/fixtures/README.md` for the regeneration recipe and the opcode coverage of each fixture:
  - `tests/tuples_lists.rs` — `fixtures/{tuples,lists}.beam` (`put_tuple2`/`get_tuple_element`/`select_tuple_arity`, `put_list`/`get_list`/`get_hd`/`get_tl`) plus synthetic modules for the failure paths.
  - `tests/exceptions.rs` — `fixtures/{exceptions,stacktrace}.beam` (`try`/`catch`/`try_case`/`try_end`/`catch_end`/`badmatch`/`case_end`/`try_case_end`/`if_end`, nested `try`, a cross-frame unwind, plus `build_stacktrace`/`raw_raise` over the `stacktrace` fixture).
  - `tests/floats.rs` — `fixtures/floats.beam` (`fmove`/`fconv`/`fadd`/`fsub`/`fmul`/`fdiv`/`fnegate`, float literals, FR temporaries, the `badarith` cases).
  - `tests/funs.rs` — `fixtures/funs.beam` (`make_fun3` with/without free vars, `call_fun`, `call_fun2`, `apply`, `apply_last`), driven through the context's **apply hook** and BIF hook.
  - `tests/maps.rs` — `fixtures/mapfx.beam` (`get_map_elements`/`has_map_fields`/`put_map_assoc`/`put_map_exact`, incl. the `{badkey, a}` case) plus hand-built frozen-layout HAMT terms for the hashmap read path.
  - `tests/records.rs` — `fixtures/records.beam` (`update_record` under the `reuse`/`copy`/`inplace` hints, `dst` ≠ src and dst-aliasing, the `badrecord` raise) plus synthetic modules for the failure paths (non-tuple source, size/arity mismatch, offset 0 / out of range).
  - `tests/bitstrings.rs` — `fixtures/bitstrings.beam` (`bs_create_bin` integer/binary/string segments, the match side, the position ops).
  - `tests/bitstrings_extra_create.rs` — `fixtures/bscreate.beam` (`bs_create_bin` float 16/32/64 BE+LE, utf8/utf16/utf32 segments, and the invalid-size / non-scalar-code-point rejections).
  - `tests/bitstrings_extra_match.rs` — `fixtures/bsmatch.beam` (`bs_match_string`, the `=:=` command, `bs_get_float2` 16/32/64 BE+LE, the UTF reads/skips + their malformed-input failures).
  - `tests/bitstrings_extra_big.rs` — `fixtures/bsbig.beam` (integer fields wider than 64 bits on both the match and the create side: 65/68/72/128/256 bits, signed/unsigned, BE/LE) — every expected value oracle-derived.
- **Synthetic-module opcode tests** (`tests/opcodes.rs`): hand-built `Module`s (the loader is not involved) covering the rest of the executed subset — every `TestKind` (success and failure), `SelectVal`, the frame instructions (`Allocate`/`AllocateHeap`/`TestHeap`/`Deallocate`/`Trim`), the call family (`Call`/`CallLast`/`CallOnly`/`CallExt`/`CallExtOnly`/`CallExtLast`), the `GcBif` guard-fail/raise paths, the catch-frame landing of a raising BIF (`gc_bif`/`bif`/`bif0`/`call_ext*` with no fail label inside a live `try`) and the documented error paths; a comprehensive test module, legitimately long.
- **Vertical-slice integration** (`tests/slice.rs`): load the committed fixture `../beam_loader/tests/fixtures/slice.beam` (reused, **not duplicated**, via `concat!(env!("CARGO_MANIFEST_DIR"), "/../beam_loader/tests/fixtures/slice.beam")`), build a `MinimalContext` whose hook implements `erlang:'+'/2` and `erlang:'-'/2` over small ints, and assert `slice:add(2,3) == 5`, `slice:fib(10) == 55`, `slice:pick(a) == 1`, `slice:pick(b) == 2`.
- **The headline stdlib integration tests** (`tests/stdlib_lists.rs` + `tests/stdlib_more.rs` + the shared `tests/support/mod.rs` host): load **real system stdlib `.beam` modules** (located at test time from `$ERTS_STDLIB_DIR` or `erl -noshell -eval 'code:lib_dir(stdlib)'`; every test **skips loudly** when stdlib is absent) and assert exact oracle results. The host re-runs the loader's public pipeline (`container::parse` → `chunk::parse` → `decode::decode_stream` → `lower::build_module`) with **one shared [`AtomTable`]** so cross-module atom identity (`true`/`false`/`undefined`) holds, keeps **one shared term arena** (`MinimalContext`), dispatches `call_ext*` from `call_bif` (local BIF floor → external module load + `run_export`), and resolves `call_fun`/`call_fun2` to the defining module through `apply_fun` (fun `uniq`+`index` → `Module::funs()`, entry run with `free ++ args`). `stdlib_lists.rs` proves `lists:reverse/1`, `lists:seq/2`, `lists:map/2`, `lists:sort/1`, `lists:filter/2`; `ordsets:from_list/union/intersection/subtract/is_element`; `proplists:get_value/2,3`, `proplists:lookup_all/2`, `proplists:get_bool/2`; plus the committed `stdlists_harness` fixture's `reverse/1`, `seq/2`, `map_double/1`, `sort/1`, `filter_pos/1` (the last two build an anonymous fun and drive it through `lists:map`/`lists:filter` — the cross-module closure path). `stdlib_more.rs` adds real `orddict`, `queue`, `gb_trees`, `gb_sets`, `ordsets` and further `lists`/`proplists` functions through the `stdlib_more_harness` fixture. The BIF floor those modules needed: `erlang:'+'/'-'/'*'/div`, `==`/`/=`/`=:=`/`=/=`/`<`/`=<`/`>`/`>=`, `element/2`, `tuple_size/1`, `length/1`, `++/2`, `lists:reverse/2` (a NIF in `lists.beam`), `error/1..3`/`nif_error/1` (typed `Err` — reached only on invalid-argument paths the tests avoid); `stdlib_more` adds `erlang:'--'/2`, `bsl/2`, `bsr/2`, `max/2`, `min/2` and `lists:member/2` (exact `=:=` for `--`/`member`; arithmetic ordering for `max`/`min`, returning `args[0]` on equality).
- **Gate (GREEN required):** `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine` and `... --workspace` must pass, and `-p beam_engine --features jit-cranelift` must still build (the JIT feature must not break the build).
- Always go through `erts_rust/scripts/cargo.sh` (`$HOME` is read-only in the sandbox), never `cd`, never touch `../tools/` or `../../erts/`.
- Full differential cross-checks (`tools/differential`) are the project-level M1 **compatibility gate**; this crate's tests are the engine-level evidence.

## Design Decisions
- **`ExecutionContext` seam with the root-set contract made concrete.** `call_bif` isolates BIF dispatch, `apply_fun` isolates fun application, and `ensure_heap(heap_need, live, &mut x, &mut stack)` hands the exact GC root set (`x[0..live)` + the whole stack, §4) to the context, so a Stage-4 collector can move terms in place without frame liveness maps. `MinimalContext` is the test/embedding impl with an unbounded heap.
- **A per-instruction-family seam keeps the loop small and edit-local.** The dispatch `match` has one arm per *core* opcode; the catch-all arm routes everything else to `families::dispatch`, so a whole opcode family is implemented without ever touching `interpreter.rs` again.
- **Safe `match`-based dispatch, not computed-goto.** The IR is pre-decoded (typed `Instr` per opcode with operands resolved at load time), so dispatch is one `match` arm per op — the direct-threaded *model* of §F, implemented without `unsafe`/`goto`. A future asm/JIT tier may use threaded code; the interpreter need not.
- **`Step` is the loop/family control-flow contract.** A family returns `Next`/`Goto`/`Done` and never mutates the loop's `func`/`pc`; a raise stays an `Err(BeamError)` (or is folded into a `Goto` by the exception layer).
- **A typed frame CP slot, not a raw stack word.** `allocate` reserves `stack_need + 1` words and the CP slot is modeled as a typed `Continuation` (`Address{func,pc}` or `Halt`) on the frame; the Y slots are the stack words. This reproduces the C frame exactly while staying type-safe.
- **The catch-frame stack mirrors C's `next_catch` + `c_p->catches` split.** A `CatchFrame` stores the handler location and the owning frame index; a raise reads the innermost entry and unwinds the call frames (C's `c_p->stop = prev`), while the handler opcode pops exactly one entry (C's `c_p->catches--`).
- **`Machine::throw` is the one raise/unwind entry point** every family calls, so the landing convention (`x(0..=3)`) and the catch/unwind bookkeeping live in exactly one place.
- **Module-wide label index.** `Call*`/`CallLast`/`CallOnly` targets and `fail` labels are module-local and can cross functions (`fib/1` → `fib/3`), so the machine builds a `Label -> (function index, code index)` map from `Module.functions[*].labels` per run (labels are module-unique code addresses).
- **A real tier seam, with the JIT tiers off the critical path.** `Tier`/`JitBackend`/`available_tiers` are the selection surface; `Machine::run_with_tier` dispatches to the interpreter today and the native backends land as `jit` submodules. Cranelift is an optional feature (default OFF); with it off the crate and the entire build/test flow carry no Cranelift codegen. JIT compilation is measurement-gated (Part II §F) with a bounded-latency, cheap-deopt design.
- **Reuse the loader fixture; do not duplicate it.** The slice `.beam` lives with `beam_loader` (single source of truth for the oracle image); `beam_engine` reads it by relative path.
- **Unsupported ⇒ `Raise` with reason atom `unsupported_opcode`.** `beam_common::BeamError` has no dedicated variant and must not gain one from here; `error::unsupported` is the documented sentinel, consistent with "no silent swallowing, no panic".
- **The FR file stores `Eterm` term slots at M1.** The C float-register file is raw doubles (`FloatDef`), but the engine's operand seam is `Eterm`-typed, so a term-slot FR file lets `read_operand`/`write_operand` support `FloatReg` uniformly; the raw-double model is a deferred refinement.
- **Term construction lives behind `alloc_words` + `terms`.** The contract for a boxed term is the sole responsibility of `terms`, and its single `unsafe` module is the only place that dereferences raw heap storage; the bitstring family adds its own `raw` module for the two frozen bit layouts `terms` does not own.
- **M1 fun layout inlines the descriptor.** C's boxed fun points out-of-line at an `ErlFunEntry`/`Export` (code address + `uniq`/`index`); M1 has no code pointers, so `make_fun` inlines the descriptor as two words (`uniq`, `index`) after the header — the same documented "M1 substitution" pattern as the external-pid `node` slot (`erl_term.h` `MAKE_FUN_HEADER` + `erl_fun.h` `ErlFunThing`).
- **`MinimalContext` fun application goes through an apply hook.** The engine keeps no closure registry, so `call_fun`/`call_fun2` reach the host through `ExecutionContext::apply_fun`, whose default body is a typed `unsupported`; tests install an `ApplyHook` that knows the closures the fixture builds.
- **The HAMT reader lives in `terms.rs`.** The crate's designated raw-layout `unsafe` module owns `map_entries` for both the flatmap and the HAMT layouts (an explicit work stack bounded by `HAMT_MAX_LEVEL`, no hash function — keys are matched by `beam_term::cmp` afterwards); no builder is duplicated and no dependency edge to `beam_codec` was added.
- **A `build_stacktrace` trace is derived from the machine's live frames.** With no saved raw `StackTrace` object, the symbolic trace is reconstructed from the executing `(func, pc)` plus `Machine::resumption_addresses()` (the suspended call frames' CP slots), rather than a raise-time snapshot — the honest M1 substitute documented in Known Issues.
- **`update_record` always copies.** The compiler's `copy`/`reuse`/`inplace` hint only authorises an in-place update for a unique/dead source; a copy is observationally equivalent and needs no uniqueness analysis, so M1 ignores the hint.

## Known Issues / Intentional M1 gaps
- **No garbage collection.** `MinimalContext` has an unbounded, never-freed arena; `test_heap`/`allocate_heap` call `ensure_heap` (a no-op there). The real collector + heap sizing is Stage 4 (`beam_gc`); the engine allocates boxed terms only through `terms`/`alloc_words` (BIFs allocate through the context), and `MinimalContext` frees nothing until it drops.
- **`build_stacktrace`/`raw_raise` are executed, but a captured trace is engine-derived.** `raw_raise` is oracle-exact for all three classes (`error`/`exit`/`throw`) and for the unknown-class fall-through (`x(0) = badarg`, no raise). `build_stacktrace` is oracle-exact for a `NIL` raw trace — the only path a compiler-emitted program takes today, since `catch`/`try` land with `x(2) = NIL` and `catch_end` appends no stacktrace element; for a non-`NIL` trace it substitutes a symbolic trace derived from the machine's **live** call frames (`Machine::resumption_addresses`), not from a saved raw `StackTrace`, so unwound frames, out-of-run host frames, saved argument lists, and `error_info` entries are absent, and the innermost item always carries the arity.
- **`catch_end` needs the `EXIT` atom.** A module whose atom table lacks `EXIT` returns a typed `unsupported` rather than substituting a wrong atom.
- **`raise_bif_error` can only raise a nameable error.** A BIF failure whose atom is absent from the running module's atom table (or a `BeamError` with no BEAM atom: `Raise`, `TermNotFound`, `Io`) propagates unchanged, because the `Module` reaches the interpreter read-only (unlike C's global atom table).
- **Message/receive opcodes are not executed.** `Send`, `LoopRec`, `LoopRecEnd`, `Wait`, `WaitTimeout`, `Timeout`, `RemoveMessage`, `RecvMarkerReserve/Bind/Clear/Use`, `NifStart`/`OnLoad`, and every other named variant no family owns fall through `families::dispatch` to `unsupported("widened opcode not yet executed")`.
- **The OTP-29 native-record opcodes are not lowered to typed IR** by `beam_loader`: `is_any_native_record`/`is_native_record`/`get_record_elements`/`put_record`/`is_record_accessible`/`get_record_field`/`get_record_elements_id`/`update_record_id` fall through to `Instr::Generic`, so the engine reports `unsupported(<opcode name>)` for them. `update_record` (the classic form) **is** executed. Adding the native-record family needs new `beam_loader` `Instr` variants plus the native-record term layout (`../../erts/emulator/beam/erl_record.{h,c}`) — a `beam_loader` handoff.
- **Every remaining `Instr::Generic` opcode is `unsupported`**, carrying `Opcode::name()`.
- **Bitstring gaps (each a typed `unsupported`).** `BsInitWritable` and the `append`/`private_append` `bs_create_bin` segments — no writable/off-heap ref-counted binary exists in `beam_term`/`beam_alloc`; results larger than the 64-byte on-heap binary limit and **non-heap-binary subjects** (an off-heap ref-counted binary or a stale match context) — an off-heap result binary is never built at M1. `bs_create_bin` **`string` constant segments** are a typed gap because `beam_loader`'s decode maps the `{string, Bs}` operand to a literal whose payload is not a heap binary (loader-side blocker). A ≤64-bit **unsigned** field whose value exceeds `MAX_SMALL` is also a typed error (`make_small_checked`); only the `>64` path builds a bignum. A `bs_create_bin` construct failure with `no_fail` reports a typed raise but not C's exact `badarg` reason (the engine routes through `fail_step`/`{badmatch, V}`).
- **Literal operands — a loader-side decode gap plus an engine-side operand-seam limit.** The loader *does* decode the `LitT` area into terms (`Module::literal`), but (a) its decode (`beam_codec::decode_term`) does not decode the binary ETF tags (`BINARY_EXT`/`BIT_BINARY_EXT`), so a module whose literal area holds a binary — e.g. a `bs_create_bin` `{string, Bin}` constant segment, or a map literal containing a binary — fails to **load** (symptom: `beam_rt` reports `unsupported: external_term` before any instruction runs); this is a `beam_loader`/`beam_codec` gap, not this crate's. (b) a `Literal` index outside the running module's `LitT` table (or a literal read with no module bound) is the typed `operand: literal` error; the `floats` and `bitstrings` families additionally resolve their own literal operands directly through `Module::literal`.
- **Map construction is flat-map only; reads handle hashmaps.** `PutMapAssoc`/`PutMapExact` build via `make_flatmap`, capped at 32 pairs — a larger result is a typed `unsupported` because no crate in the rewrite implements a HAMT builder (it belongs in `beam_term`; `beam_codec` is a sibling the engine may not depend on). `terms::map_entries` walks the frozen `erl_map.h` HAMT node tree (explicit work stack, no hash function, keys matched by `beam_term::cmp`), so `GetMapElements`/`HasMapFields` read a hashmap source; a malformed node / `THE_NON_VALUE` / immediate item is a typed `BadArg`. Enumeration order is storage order, not canonicalized (`erts_cmp_flatmap_keys`), which affects only `term_to_binary` byte comparison, not map semantics. A >32-key **map literal** still fails to **load** (`beam_codec::decode` `Tag::Map` limit) — a separate `beam_codec`/`beam_loader` blocker.
- **A missing map key with `fail == NO_FAIL` has no ground truth** (the compiler always emits a fail label for the matching forms), so it is a documented typed `unsupported` rather than an invented exception reason.
- **`Fconv` carries no fail label and does not convert bignums.** A non-numeric source raises `badarith` through the catch machinery (`raise_badarith`: `Machine::throw_error` with the module's `badarith` atom), landing in a live catch frame or propagating the typed `BeamError::BadArith` when uncaught; a bignum source (no safe wide-integer → `f64` conversion yet) is a typed `unsupported`.
- **A failed function-clause guard is not a `function_clause` raise.** `func_info` is treated as a no-op, so a failed guard that jumps to a function's `func_info` label does not raise `function_clause` (the fixtures avoid that path deliberately).
- **No reductions / preemption / scheduler.** `run_*` executes to the function's `return`; process scheduling, reductions and trap handling are Stage 5 (`beam_process`).
- **`ensure_heap` underflow/limits are not enforced by the machine** — stack growth is an unbounded `Vec` in M1; a configurable stack limit + `system_limit` raise is a later refinement.
- **Cross-crate duplications to avoid:** operand read/write, term comparison, the frame model, the HAMT reader and the bit-layout accessors live here; term construction/comparison must stay in `beam_term` (do not copy).

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude` (`unsafe` allowances for `predicates`, `terms`, `families/bitstrings/raw`)
- `src/context.rs` → the `ExecutionContext` seam trait + `BifHook`/`ApplyHook` + `MinimalContext` (BIF/apply hooks, unbounded heap, pointer-stable arena)
- `src/machine.rs` → `Machine`: x-register file, FR file, Erlang stack, call frames, catch-frame stack, `run_export`/`run_label`, `Continuation`/`Frame`/`CatchFrame`, `resumption_addresses`, the `throw`/unwind/landing scaffolding
- `src/interpreter.rs` → the dispatch loop (`impl Machine`): one arm per core `Instr` variant, the module-wide label map, `eval_test` (every `TestKind`), `raise_bif_error`, and the `families::dispatch` route
- `src/step.rs` → the `Step` control-flow protocol (`Next`/`Goto`/`Done`)
- `src/operand.rs` → `Operand` read (→ `Eterm`) / write (register or Y slot) + unsupported-operand errors
- `src/predicates.rs` → an `#[allow(unsafe_code)]` module: SAFETY-annotated wrappers over the `unsafe fn` `beam_term` boxed-term predicates (`is_integer`, `is_tuple`, `is_map`, …) used by the `Test` opcodes
- `src/terms.rs` → the SAFETY-annotated boxed-term builder/reader (`make_tuple`/`make_cons`/`make_list`/`badmatch_reason`/`make_float`/`make_flatmap`/`make_fun`/`make_heap_binary` + `tuple_*`/`cons_*`/`list_elements`/`float_value`/`map_entries` (flatmap + HAMT)/`binary_bytes`/`fun_*` readers) over `ExecutionContext::alloc_words`
- `src/jit/mod.rs` → the execution-tier seam: `Tier`, the `JitBackend` trait, `available_tiers()`, the compiled-in roster (native backends `baseline.rs`/`cranelift.rs` are future submodules here)
- `src/jit/emitter.rs` → the baseline JIT's x86-64 encoder + executable `CodeBuffer` (the crate's only `mmap`/`mprotect` zone): `Reg`/`Cond`/`Label`, W^X `seal`, label binding/fixup patching, and the tested instruction subset (mov/lea, add/sub/cmp/test/and/or/xor, push/pop, jmp/jcc/call/ret, nop/int3)
- `src/error.rs` → engine error constructors over `beam_common::BeamError` (`unsupported`, `bad_register`, `unknown_label`)
- `src/families/mod.rs` → the `dispatch` seam: tries the eight families in a fixed order (`tuples`, `records`, `lists`, `funs`, `exceptions`, `floats`, `maps`, `bitstrings`), else the documented `unsupported`
- `src/families/tuples.rs` → `PutTuple2`/`GetTupleElement`/`SelectTupleArity`
- `src/families/records.rs` → `UpdateRecord`
- `src/families/lists.rs` → `PutList`/`GetList`/`GetHd`/`GetTl`
- `src/families/funs.rs` → `MakeFun3`/`CallFun`/`CallFun2`/`Apply`/`ApplyLast`
- `src/families/exceptions.rs` → `exception`/`raise`/`try`/`catch`/handler opcodes + the executed `BuildStacktrace`/`RawRaise` (and the engine-derived trace helper set)
- `src/families/floats.rs` → `Fmove`/`Fconv`/`Fadd`/`Fsub`/`Fmul`/`Fdiv`/`Fnegate`
- `src/families/maps.rs` → `GetMapElements`/`HasMapFields`/`PutMapAssoc`/`PutMapExact` (flatmap + hashmap sources; flatmap-only construction)
- `src/families/bitstrings/mod.rs` → the `Bs*` dispatch + the shared failure/operand/bit-arithmetic helpers + the wide (>64-bit) integer helpers + `BitWriter`
- `src/families/bitstrings/matching.rs` → match entry (`bs_start_match3/4`, `bs_match_string`), the fixed reads (`integer` incl. >64-bit bignums, `float`, `binary`, UTF-8/16/32 + skips), the position ops, and the `bs_match` command interpreter (incl. `=:=`)
- `src/families/bitstrings/create.rs` → `bs_create_bin` segments: `integer` (any width), `binary`, `float`, `utf8`/`utf16`/`utf32` (+ the `bs_init_writable`/`append`/`string` gaps)
- `src/families/bitstrings/raw.rs` → the bitstring family's `#[allow(unsafe_code)]` module: `ErlSubBits`/`ErlHeapBits` raw layouts + the `>64`-bit bignum build/read helpers
- `tests/stdlib_lists.rs` → the headline Milestone-1 evidence: runs real system stdlib `lists`/`ordsets`/`proplists` modules and the `stdlists_harness` fixture end to end
- `tests/stdlib_more.rs` → the second real-stdlib evidence module (`orddict`/`queue`/`gb_trees`/`gb_sets`/`ordsets` + further `lists`/`proplists`) over `tests/support/mod.rs`
- `tests/support/mod.rs` → the multi-module test `ExecutionContext` (`ModuleHost`): one shared arena + one shared `AtomTable`, a runtime-located stdlib code path, the local BIF floor, external-module dispatch, and cross-module fun resolution
- `tests/fixtures/stdlists_harness.{erl,beam}`, `tests/fixtures/stdlib_more_harness.{erl,beam}` → oracle-built harnesses forwarding into real stdlib
- `tests/slice.rs` → vertical-slice integration test over the reused `beam_loader` slice fixture
- `tests/opcodes.rs` → synthetic-module tests for the opcodes the fixtures do not reach (every `TestKind`, `SelectVal`, frame/call/`Deallocate`/`Trim`, `call_ext*`, the raising-BIF catch landing, error paths)
- `tests/tuples_lists.rs`, `tests/exceptions.rs`, `tests/floats.rs`, `tests/funs.rs`, `tests/maps.rs`, `tests/records.rs`, `tests/bitstrings.rs`, `tests/bitstrings_extra_create.rs`, `tests/bitstrings_extra_match.rs`, `tests/bitstrings_extra_big.rs` → per-family integration tests over the committed fixtures
- `tests/fixtures/` → the committed oracle `.beam`s (`tuples`, `lists`, `exceptions`, `stacktrace`, `floats`, `funs`, `mapfx`, `records`, `bitstrings`, `bscreate`, `bsmatch`, `bsbig`, `stdlists_harness`, `stdlib_more_harness`) + their `.erl` sources + `README.md` (the regeneration recipe and per-fixture opcode coverage)
- `../beam_loader/CONTEXT.md` → the frozen IR this crate consumes (read it fully; do not restate)
- `../../erts/emulator/beam/emu/{instrs,macros,bif_instrs,select_instrs,map_instrs,float_instrs,bs_instrs}.tab` + `beam_emu.c`, `beam_common.c` → ground-truth instruction/exception semantics (read-only)
- `../../erts/emulator/beam/emu/ops.tab`, `../../lib/compiler/src/genop.tab` → opcode roster / transforms (read-only)
- `../../erts/emulator/beam/erl_bits.c`, `../../erts/emulator/beam/erl_map.h` → bitstring and map (CHAMP/HAMT) ground truth (read-only)

## See Also
- `../CONTEXT.md` → crate-layer contracts, the seam types, the Stage-1/Stage-5 DAG, wave table.
- `../../CONTEXT.md` §4 → the engine IR seam and the root-set contract (`x[0..live)` + whole stack, no liveness maps).
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §E/§F/§G → IR seam, interpreter/JIT trade-offs, tiering rationale.
- `../beam_loader/CONTEXT.md` → the IR types (`Module`/`Function`/`Instr`/`Operand`/`TestKind`/`HeapNeed`/`LiteralArea`) and their exact meaning.
- `../beam_term/CONTEXT.md` → `Eterm`, `make_small`/`make_atom`/`NIL`, `cmp`/`eq`/`cmp_arith` (the engine's term API).
- `../beam_rt/` → the Stage-5 runtime that supplies the concrete `ExecutionContext`.
