//! The native **baseline JIT** tier: typed IR → x86-64 basic blocks + driver loop.
//!
//! This is the backend behind [`Tier::BaselineJit`](crate::jit::Tier::BaselineJit)
//! and the first [`JitBackend`] the engine actually builds. It is the *mechanism*,
//! not an optimiser: every basic block of the module is lowered into a real
//! `extern "C"` native function, but only the cheapest instructions are emitted as
//! machine code — everything else is executed by calling back into the
//! interpreter's per-instruction step. The result is a genuine native execution
//! path with **interpreter-identical semantics** (Part II §F: correctness first,
//! the hot-tier optimisation follows in a later task).
//!
//! # The block ABI
//!
//! Each compiled basic block is
//!
//! ```text
//! extern "C" fn(rt: *mut JitRuntime) -> i64
//! ```
//!
//! It reads and writes the process state through `rt` and returns the **next
//! control-flow target** as one `i64`:
//!
//! * `(func as i64) << 32 | (pc as i64)` — a normal continuation: keep executing at
//!   code offset `pc` of the function at index `func` (see [`encode_target`]). The
//!   encoding reserves the sign bit, so a valid target must satisfy
//!   `func < 2^31` (every real module does).
//! * [`HALT`] (`-1`) — the run finished; the driver yields `machine.finish()`
//!   (`x(0)`).
//! * [`ERROR`] (`-2`) — a raise (or any other typed error) was recorded in
//!   `rt.error`; the driver returns it.
//!
//! The driver ([`run_compiled`]) decodes that word, looks the target up in the
//! block-entry table and calls the next block, falling back to a single
//! interpreter step for any address no block was compiled for.
//!
//! # The runtime block (`JitRuntime`)
//!
//! [`JitRuntime`] is the one argument a block receives: a `#[repr(C)]` bag of raw
//! pointers at the live driver-owned objects plus the helper error slot. Generated
//! code loads exactly two of its fields: `x_regs` (offset [`RUNTIME_XREGS_DISP`]) —
//! the base of the machine's x-register array — and, through the helper scaffolding,
//! nothing else; the remaining fields are read by the Rust-side [`jit_step_one`].
//! `machine` (offset [`RUNTIME_MACHINE_DISP`]) is the [`Machine`] the helper needs;
//! a block never touches it, because `[base + n*8]` addressing must be based on the
//! **array** (`machine.x`), not on the address of the `Vec` field inside a
//! non-`repr(C)` Rust struct.
//!
//! # Register allocation and spill-across-blocks
//!
//! A block caches up to [`CACHE_REGISTERS`]` .len()` x-registers in callee-saved
//! machine registers (`r12`–`r15`); `rbx` is reserved for the machine base pointer
//! (loaded once from `rt.machine`) and the `rt` argument itself lives in the 16-byte
//! stack slot the prologue reserves. The prologue pushes every callee-saved register
//! it touches (so the Rust caller's state is preserved), loads the cached
//! x-registers out of `machine.x`, and the epilogue **spills them back at every
//! block exit**. That is the whole liveness model: within a block the cached values
//! live in registers, and at a block boundary they are always back in `machine.x`,
//! so a helper call / GC / the next block sees the interpreter's exact root set
//! (`x[0..live)` + the whole stack) with no per-frame liveness maps
//! (`../../CONTEXT.md` §4).
//!
//! A block that references more than the cached budget simply does not inline those
//! `move`s — the extras are executed by the helper, which is always correct. The
//! allocation is computed by [`allocate_registers`] from the block's *shape*-
//! inlinable `move`s, in first-appearance order, so a block's inline prefix always
//! has the registers it wants.
//!
//! # What is inlined, what is helper-executed
//!
//! **Inlined** (emitted natively, no interpreter round-trip):
//!
//! * the metadata no-ops (`label`/`line`/`func_info`/`int_code_end`/
//!   `executable_line`/`debug_line`) — nothing at all;
//! * `move Src Dst` where `Dst` is an x-register (`x`/`tr`-annotated) **and**
//!   `Src` is an x-register or a constant (`Integer`/`Character`/`Atom`/`Nil`) —
//!   a `mov` between the allocated machine registers, or a `mov` of the raw `Eterm`
//!   word (`make_small`/`make_atom`/`NIL`);
//! * `jump Label` whose target resolves in the module-wide label map — the
//!   `(func, pc)` pair is known at compile time, so it is a constant return word;
//! * the **guarded small-int arithmetic fast path** for `gc_bif` of
//!   `erlang:'+'/2` and `erlang:'-'/2`: the tagged-word arithmetic is emitted
//!   natively behind a small-int guard + overflow + range check, and *any* case
//!   the guard rejects (a non-small operand, an `i64` overflow, a result outside
//!   `MIN_SMALL..=MAX_SMALL`) jumps straight to `jit_step_one`, which runs the
//!   real BIF and produces the bignum/float/error result natively unreachable
//!   here. This is the tier's one optimisation (`../../CONTEXT.md` §7); it is
//!   conservative by construction — it can only ever produce a result the helper
//!   would have produced — see [`try_inline_gc_bif`].
//!
//! **Helper-executed** (`jit_step_one`, literally the interpreter's
//! [`Machine::step_instruction`](crate::Machine)): **every other instruction**, plus
//! a `move` with an operand outside the cached set, a `jump` to a label the
//! module does not define (so the interpreter's exact `unknown_label` error
//! surfaces rather than an invented one), and a `gc_bif` with either an
//! un-cacheable operand or a BIF outside the fast path's two (or a fast-path
//! `gc_bif` whose registers the allocator's four-register budget could not
//! cache).
//!
//! A block's *inline prefix* runs natively and the **first** non-inlinable
//! instruction is handed to the helper, after which the block returns: this is the
//! documented per-instruction deopt, and it is what gives the tier full opcode
//! coverage with interpreter-identical semantics on day one.
//!
//! # `unsafe`
//!
//! This is a designated JIT unsafe zone: it reconstructs `&mut Machine` /
//! `&mut dyn ExecutionContext` from [`JitRuntime`]'s raw pointers and calls into
//! sealed executable memory. The crate root keeps `#![deny(unsafe_code)]`; the
//! module-local `#![allow(unsafe_code)]` below is the second JIT exception
//! (alongside [`crate::jit::emitter`]). Every `unsafe` block carries a
//! `// SAFETY:` rationale.
#![allow(unsafe_code)]

use std::collections::{BTreeMap, HashMap};
use std::mem;

use beam_common::{BeamError, Result};
use beam_loader::{Atom, ImportRef, Instr, Label, Module, Operand, Reg as RegRef};
use beam_term::{
    make_atom, make_small, Eterm, MAX_SMALL, MIN_SMALL, NIL, TAG_IMMED1_MASK, TAG_IMMED1_SIZE,
    TAG_IMMED1_SMALL,
};

use crate::context::ExecutionContext;
use crate::error;
use crate::interpreter;
use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg, DEFAULT_CAPACITY};
use crate::jit::JitBackend;
use crate::machine::{Machine, MAX_REGISTER};
use crate::Step;

// ---------------------------------------------------------------------------
// The block ABI
// ---------------------------------------------------------------------------

/// The `HALT` block-return word: the run finished, the driver yields `x(0)`.
///
/// A negative word is never a valid [`encode_target`] result (the encoding is
/// `(func << 32) | pc` with `func < 2^31`, so its sign bit is clear).
pub const HALT: i64 = -1;

/// The `ERROR` block-return word: a typed error was stored in the runtime's
/// `error` slot and the driver must propagate it.
pub const ERROR: i64 = -2;

/// Encodes a `(func, pc)` continuation as the block-return word.
///
/// `func` is the function index into [`Module::functions`], `pc` the code offset
/// within it. The result is non-negative for every `func < 2^31` (which is every
/// module an engine can hold: the entry table is keyed by `u32` but the ABI keeps
/// the top bit for the negative sentinels).
pub const fn encode_target(func: u32, pc: u32) -> i64 {
    ((func as i64) << 32) | (pc as i64 & 0xFFFF_FFFF)
}

/// Decodes a block-return word into its `(func, pc)` continuation, or `None` for a
/// sentinel ([`HALT`]/[`ERROR`]) — or any other negative word, which no block the
/// compiler emits can produce.
pub const fn decode_target(word: i64) -> Option<(u32, u32)> {
    if word < 0 {
        return None;
    }
    Some(((word >> 32) as u32, (word & 0xFFFF_FFFF) as u32))
}

/// The one argument a compiled block receives.
///
/// A `#[repr(C)]` bag of raw pointers at the live driver-owned objects (valid for
/// the whole driver call, never aliased while a `&mut` derived from a field is
/// alive) plus the slot a helper stores a propagating [`BeamError`] in. Generated
/// code touches **only offset 0 and 8** ([`RUNTIME_MACHINE_DISP`],
/// [`RUNTIME_XREGS_DISP`]); the remaining fields are read by the Rust-side
/// [`jit_step_one`] helper.
///
/// `machine` is the [`Machine`] itself (what the helper needs); `x_regs` is the base
/// of its **x-register array** (`machine.x.as_ptr()`), which is what a block indexes.
/// The two are separate fields because a block's `[base + n*8]` addressing needs the
/// array's data pointer, not the address of the `Vec` field inside a non-`repr(C)`
/// Rust struct. `machine.x` is only resized by [`Machine::reset`] (which runs before
/// this pointer is taken) — `ensure_heap` hands the context a slice, so it cannot
/// reallocate the array mid-run.
///
/// The lifetime parameter bounds only the [`ExecutionContext`] trait object (it is
/// invariant, so the object's own lifetime must be named); it does not affect the
/// `#[repr(C)]` layout.
#[repr(C)]
pub(crate) struct JitRuntime<'a> {
    /// The process state a block reads and writes through (`stack`, frames, …).
    pub(crate) machine: *mut Machine,
    /// The base of the x-register array (`machine.x`) a block loads from / spills to.
    pub(crate) x_regs: *mut Eterm,
    /// The module being executed (its `code`, labels, imports, literals).
    pub(crate) module: *const Module,
    /// The module-wide `Label -> (func, pc)` map the helper resolves fail/target
    /// labels through (built once per driver call by
    /// [`interpreter::build_label_map`](crate::interpreter)).
    pub(crate) labels: *const BTreeMap<Label, (u32, u32)>,
    /// The [`ExecutionContext`] seam, handed back as `&mut dyn` to the helper.
    pub(crate) ctx: *mut (dyn ExecutionContext + 'a),
    /// Set by a helper that hit a propagating `Err`; the `ERROR` word tells the
    /// driver to take it.
    pub(crate) error: Option<BeamError>,
}

/// A compiled block's native signature.
type NativeBlock = for<'a> extern "C" fn(*mut JitRuntime<'a>) -> i64;

/// The `jit_step_one` helper's native signature (the block ABI plus the
/// instruction address).
type StepHelper = for<'a> extern "C" fn(*mut JitRuntime<'a>, u32, u32) -> i64;

/// A native tier's compiled-code lookup, consulted once per driver iteration by
/// [`run_native`].
///
/// A tier implements this for whatever it compiled: [`run_native`] hands it the
/// current `(func, pc)` and the live [`JitRuntime`], and the tier either runs that
/// address natively — returning the block-ABI word the code produced — or returns
/// `None`, which makes the driver take exactly one interpreter step. Returning
/// `None` must not touch the runtime: it is the "this tier has no code here"
/// answer, and it is what keeps an uncovered address on the interpreter's exact
/// semantics.
pub(crate) trait NativeCode {
    /// Run `(func, pc)` natively if this tier compiled it for that address.
    fn enter(&self, rt: *mut JitRuntime<'_>, func: u32, pc: u32) -> Option<i64>;
}

// ---------------------------------------------------------------------------
// The backend
// ---------------------------------------------------------------------------

/// The native baseline tier.
///
/// A zero-sized selector: it holds no per-run code state, so the artifact it
/// compiles ([`CompiledModule`]) is caller-owned and can be run any number of times
/// through [`JitBackend::run_compiled`] without recompiling. The one-shot
/// [`JitBackend::run_export`] (`run_with_tier` creates a fresh backend per run)
/// compiles once and runs immediately; callers that run a module repeatedly use the
/// two steps directly. Inlining quality and a cross-run code cache are *performance*
/// concerns deferred to a later task — this is the mechanism and its correctness.
#[derive(Clone, Copy, Debug, Default)]
pub struct BaselineJit;

impl BaselineJit {
    /// A baseline backend ready to compile and run a module.
    pub fn new() -> BaselineJit {
        BaselineJit
    }
}

impl JitBackend for BaselineJit {
    type Compiled = CompiledModule;

    fn name(&self) -> &'static str {
        "baseline-jit"
    }

    fn available(&self) -> bool {
        true
    }

    /// Compile every basic block of `module` into native code.
    fn compile(&mut self, module: &Module) -> Result<CompiledModule> {
        compile_module(module)
    }

    /// Run exported `function/arity` through the driver loop on an already-compiled
    /// artifact.
    ///
    /// The artifact is caller-owned and the backend holds no per-run code state, so
    /// one [`CompiledModule`] can be run any number of times (on fresh machines)
    /// without recompiling; [`JitBackend::run_export`] is the provided one-shot
    /// convenience over `compile` + this.
    fn run_compiled(
        &self,
        compiled: &CompiledModule,
        machine: &mut Machine,
        module: &Module,
        function: Atom,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        run_compiled(compiled, machine, module, function, arity, args, ctx)
    }
}

/// The artifact [`BaselineJit::compile`] produces: one sealed, executable
/// [`CodeBuffer`] plus the `(func, pc)` → native-entry table.
///
/// The buffer is `mmap`ped read+write, filled, and flipped to read+execute by
/// [`CodeBuffer::seal`] — never writable and executable at the same time (see
/// [`crate::jit::emitter`]). The entries are raw pointers into that mapping, so a
/// `CompiledModule` must outlive every call into it and is neither `Send` nor
/// `Sync`.
pub struct CompiledModule {
    /// The sealed code mapping every entry points into.
    buffer: CodeBuffer,
    /// The start `(func, pc)` of each compiled block → its native entry point.
    entries: HashMap<(u32, u32), *const u8>,
    /// The highest x-register index any block caches in a machine register.
    ///
    /// The driver refuses to enter native code when the running machine's register
    /// file is not at least this long (see [`run_compiled`]).
    highest_cached_register: Option<u32>,
}

impl CompiledModule {
    /// The native entry point of the block starting at `(func, pc)`, if one was
    /// compiled.
    pub fn entry(&self, func: u32, pc: u32) -> Option<*const u8> {
        self.entries.get(&(func, pc)).copied()
    }

    /// The number of compiled blocks.
    pub fn block_count(&self) -> usize {
        self.entries.len()
    }

    /// The highest x-register index cached in a machine register by any block.
    pub fn highest_cached_register(&self) -> Option<u32> {
        self.highest_cached_register
    }

    /// The number of bytes of machine code emitted.
    pub fn code_len(&self) -> usize {
        self.buffer.len()
    }

    /// The `[start, end)` address range of the executable code mapping.
    ///
    /// Every entry [`CompiledModule::entry`] returns lies inside it.
    pub fn code_address_range(&self) -> (usize, usize) {
        let start = self.buffer.code_ptr() as usize;
        (start, start + self.buffer.len())
    }
}

// ---------------------------------------------------------------------------
// The helper
// ---------------------------------------------------------------------------

/// The per-instruction deopt: execute exactly one instruction with the
/// interpreter's own semantics.
///
/// This is the block ABI's helper. A block calls it (with `rt`, the instruction's
/// `func` and `pc`) for the first instruction it could not lower natively; the
/// helper runs [`Machine::step_instruction`](crate::Machine) — *the* interpreter
/// step — and maps its result onto the same return word the block ABI uses:
///
/// * [`Step::Next`] → `encode_target(func, pc + 1)`,
/// * [`Step::Goto { func, pc }`] → `encode_target(func, pc)`,
/// * [`Step::Done`] → [`HALT`],
/// * `Err(error)` → the error is stored in `rt.error` and [`ERROR`] is returned.
pub(crate) extern "C" fn jit_step_one(rt: *mut JitRuntime<'_>, func: u32, pc: u32) -> i64 {
    // SAFETY: the driver passes `rt` as the address of a live, exclusively borrowed
    // `JitRuntime` and never aliases it for the duration of the call; every pointer
    // it holds points at an object the driver owns and keeps alive for the whole
    // loop (`machine`, `module`), or at a local (`labels`) that outlives the loop.
    let runtime = unsafe { &mut *rt };
    // SAFETY: `runtime.machine` is a non-null pointer to the driver's live `Machine`;
    // no other `&mut Machine` exists while the helper runs (the driver is blocked in
    // the block call that reached us).
    let machine = unsafe { &mut *runtime.machine };
    // SAFETY: `runtime.module` points at the `&Module` the driver is running, which
    // outlives the loop.
    let module = unsafe { &*runtime.module };
    // SAFETY: `runtime.labels` points at the driver's `labels` local, which outlives
    // the loop.
    let labels = unsafe { &*runtime.labels };
    // SAFETY: `runtime.ctx` is the driver's `&mut dyn ExecutionContext` reborrowed as
    // a raw pointer; the driver holds no `&mut` to it while the helper runs.
    let ctx = unsafe { &mut *runtime.ctx };

    match machine.step_instruction(module, labels, func, pc, ctx) {
        Ok(Step::Next) => encode_target(func, pc + 1),
        Ok(Step::Goto { func, pc }) => encode_target(func, pc),
        Ok(Step::Done) => HALT,
        Err(err) => {
            runtime.error = Some(err);
            ERROR
        }
    }
}

// ---------------------------------------------------------------------------
// Compilation
// ---------------------------------------------------------------------------

/// The callee-saved machine registers a block preserves across its prologue and
/// epilogue (the Rust ABI requires them to survive the call).
///
/// Five registers keep `rsp` 16-byte aligned at the helper call site: the entry
/// `rsp ≡ 8 (mod 16)` (`call` pushed the return address), five 8-byte pushes bring
/// it to `0 (mod 16)`, and the 16-byte `rt` slot the prologue reserves by `lea`
/// keeps it there.
const SAVED_REGISTERS: [Reg; 5] = [Reg::Rbx, Reg::R12, Reg::R13, Reg::R14, Reg::R15];

/// The machine registers a block caches x-registers in — `rbx` is reserved for the
/// machine base pointer, so four x-registers fit.
const CACHE_REGISTERS: [Reg; 4] = [Reg::R12, Reg::R13, Reg::R14, Reg::R15];

/// The byte offset of [`JitRuntime::machine`] — the field the helper scaffolding is
/// handed; generated code reads [`JitRuntime::x_regs`] instead.
const RUNTIME_MACHINE_DISP: i32 = 0;

/// The byte offset of [`JitRuntime::x_regs`] (the base of `machine.x`) — the **only**
/// field generated code loads indirectly. It is the second `#[repr(C)]` pointer
/// field, hence one pointer width past `machine`.
const RUNTIME_XREGS_DISP: i32 = RUNTIME_MACHINE_DISP + mem::size_of::<*const u8>() as i32;

/// The byte offset of the prologue-reserved `rt` slot from `rsp`.
const RUNTIME_SLOT_DISP: i32 = 0;

/// A block's register allocation: the `(x-register index, machine register)` pairs
/// the prologue loads and the epilogue spills.
#[derive(Default)]
struct Allocation {
    /// The slots, in assignment order (`src` before `dst` within a `move`).
    slots: Vec<(u32, Reg)>,
}

impl Allocation {
    /// The machine register holding x-register `x`, if it is cached.
    fn slot(&self, x: u32) -> Option<Reg> {
        self.slots
            .iter()
            .find(|(n, _)| *n == x)
            .map(|(_, reg)| *reg)
    }
}

/// Lower every `Function`'s every `Block` of `module` into a native function.
fn compile_module(module: &Module) -> Result<CompiledModule> {
    let labels = interpreter::build_label_map(module);
    let mut buffer = CodeBuffer::with_capacity(code_capacity_estimate(module))?;
    let mut entries: HashMap<(u32, u32), *const u8> = HashMap::new();
    let mut highest_cached_register: Option<u32> = None;

    for (func_index, function) in module.functions.iter().enumerate() {
        let func = func_index as u32;
        let code = &function.code;
        for block in &function.blocks {
            let start = block.start;
            let end = block.end.min(code.len());
            if start >= end {
                // An empty block has no instruction to run natively; the interpreter
                // step handles that address exactly as before.
                continue;
            }
            let allocation = allocate_registers(module, code, start, end);
            for (x, _) in &allocation.slots {
                highest_cached_register = Some(match highest_cached_register {
                    Some(prev) => prev.max(*x),
                    None => *x,
                });
            }

            let offset = buffer.len();
            emit_block(
                &mut buffer,
                module,
                &labels,
                func,
                code,
                start,
                end,
                &allocation,
            )?;
            // SAFETY: the block was fully emitted (no error), so `offset` is the
            // start of a complete native function inside the buffer;
            // `offset + emitted < capacity` was enforced by every emission call.
            let entry = unsafe { buffer.code_ptr().add(offset) };
            entries.insert((func, start as u32), entry);
        }
    }

    buffer.seal()?;
    Ok(CompiledModule {
        buffer,
        entries,
        highest_cached_register,
    })
}

/// A generous byte-count estimate for the code `module` lowers to.
///
/// A block costs a prologue/epilogue pair (~48 bytes) plus its inlined instructions
/// (~7–10 bytes each); a `gc_bif` costs either the helper call sequence (~40
/// bytes) or, when the small-int fast path lowers it, the whole guarded sequence
/// (up to ~150 bytes: two operand loads, two tag checks, the arithmetic with two
/// overflow branches, two range checks, the store and the fallback jump).
/// Under-estimating would turn a large module into a `SystemLimit` compile error,
/// so the estimate is deliberately loose — [`FAST_PATH_BYTES`] is budgeted for
/// *every* `gc_bif`, whether or not the fast path claims it, and
/// [`CodeBuffer::with_capacity`] rounds the total up to whole pages.
fn code_capacity_estimate(module: &Module) -> usize {
    let mut instructions = 0usize;
    let mut blocks = 0usize;
    let mut gc_bifs = 0usize;
    for function in &module.functions {
        instructions += function.code.len();
        blocks += function.blocks.len();
        gc_bifs += function
            .code
            .iter()
            .filter(|instr| matches!(instr, Instr::GcBif { .. }))
            .count();
    }
    64 * blocks + 16 * instructions + FAST_PATH_BYTES * gc_bifs + DEFAULT_CAPACITY
}

/// The code-size budget reserved for one `gc_bif` (see
/// [`code_capacity_estimate`]).
const FAST_PATH_BYTES: usize = 176;

/// Pick the x-registers a block caches, in first-appearance order.
///
/// Only `move`s whose *shape* could be inlined contribute (`dst` an x-register and
/// `src` an x-register or a constant), so the registers a block's actual inline
/// prefix needs are collected before any register from a later, never-inlined
/// instruction. A `gc_bif` the small-int arithmetic fast path can lower
/// ([`fast_arith`]) contributes its operands and destination the same way, so the
/// inline path always finds a register for each of them. A register index outside
/// the engine's [`MAX_REGISTER`] bound is never cached: reading it would be out of
/// bounds, whereas the interpreter step reports the faithful `bad_register` error.
fn allocate_registers(module: &Module, code: &[Instr], start: usize, end: usize) -> Allocation {
    let mut allocation = Allocation::default();
    for instr in &code[start..end] {
        if let Instr::Move { src, dst } = instr {
            let Some(dst_x) = x_register(dst) else {
                continue;
            };
            let src_x = x_register(src);
            if src_x.is_none() && constant_word(src).is_none() {
                continue;
            }
            if let Some(n) = src_x {
                push_slot(&mut allocation, n);
            }
            push_slot(&mut allocation, dst_x);
        } else if let Some(fast) = fast_arith(module, instr) {
            for x in fast.registers() {
                push_slot(&mut allocation, x);
            }
        }
    }
    allocation
}

/// Assign the next free cache register to `x`, if it is cacheable and still needed.
fn push_slot(allocation: &mut Allocation, x: u32) {
    if x >= MAX_REGISTER {
        return;
    }
    if allocation.slots.iter().any(|(n, _)| *n == x) {
        return;
    }
    if allocation.slots.len() >= CACHE_REGISTERS.len() {
        return;
    }
    allocation
        .slots
        .push((x, CACHE_REGISTERS[allocation.slots.len()]));
}

/// Emit one native function for `code[start..end]` (a basic block of `func`).
#[allow(clippy::too_many_arguments)]
fn emit_block(
    buffer: &mut CodeBuffer,
    module: &Module,
    labels: &BTreeMap<Label, (u32, u32)>,
    func: u32,
    code: &[Instr],
    start: usize,
    end: usize,
    allocation: &Allocation,
) -> Result<()> {
    // --- prologue -----------------------------------------------------------
    for reg in SAVED_REGISTERS {
        buffer.push_r(reg)?;
    }
    // Reserve the `rt` slot. `lea rsp, [rsp - 16]` is `sub rsp, 16` and keeps `rsp`
    // 16-byte aligned at the helper call site (see `SAVED_REGISTERS`).
    buffer.lea(Reg::Rsp, Reg::Rsp, -16)?;
    buffer.mov_mr(Reg::Rsp, RUNTIME_SLOT_DISP, Reg::Rdi)?;
    // rbx = rt->x_regs, the base of `machine.x` the cached slots are indexed from.
    buffer.mov_rm(Reg::Rbx, Reg::Rdi, RUNTIME_XREGS_DISP)?;
    for (x, reg) in &allocation.slots {
        buffer.mov_rm(*reg, Reg::Rbx, x_offset(*x))?;
    }

    let done: CodeLabel = buffer.label();

    // --- body ---------------------------------------------------------------
    let mut fell_through = true;
    for (i, instr) in code[start..end].iter().enumerate() {
        let pc = (start + i) as u32;
        match instr {
            // Metadata / no-ops (`ops.tab:103-108`): they never change control flow.
            Instr::Label(_)
            | Instr::Line(_)
            | Instr::FuncInfo { .. }
            | Instr::IntCodeEnd
            | Instr::ExecutableLine { .. }
            | Instr::DebugLine { .. } => {}
            Instr::Move { src, dst } => {
                if !try_inline_move(buffer, src, dst, allocation)? {
                    emit_helper(buffer, done, allocation, func, pc)?;
                    fell_through = false;
                    break;
                }
            }
            // The one optimised BIF: the small-int `erlang:'+'/2`/`'-'/2`. When
            // the guarded fast path is emitted the block keeps going; otherwise
            // (any other BIF, an un-cacheable operand, or a register the budget
            // could not cache) it is exactly the `_` arm below.
            Instr::GcBif { .. } => {
                if !try_inline_gc_bif(buffer, module, instr, allocation, done, func, pc)? {
                    emit_helper(buffer, done, allocation, func, pc)?;
                    fell_through = false;
                    break;
                }
            }
            Instr::Jump { target } => match labels.get(target) {
                Some(&(target_func, target_pc)) => {
                    spill(buffer, allocation)?;
                    buffer.mov_ri(Reg::Rax, encode_target(target_func, target_pc))?;
                    buffer.jmp(done)?;
                    fell_through = false;
                    break;
                }
                // An undefined label keeps the interpreter's exact `unknown_label`
                // error: hand the jump to the helper rather than inventing one.
                None => {
                    emit_helper(buffer, done, allocation, func, pc)?;
                    fell_through = false;
                    break;
                }
            },
            // Everything else: one interpreter step, then leave the block.
            _ => {
                emit_helper(buffer, done, allocation, func, pc)?;
                fell_through = false;
                break;
            }
        }
    }

    if fell_through {
        // The block ran off its end: hand the next address back to the driver.
        spill(buffer, allocation)?;
        buffer.mov_ri(Reg::Rax, encode_target(func, end as u32))?;
        buffer.jmp(done)?;
    }

    // --- epilogue -----------------------------------------------------------
    buffer.bind(done)?;
    buffer.lea(Reg::Rsp, Reg::Rsp, 16)?;
    for reg in SAVED_REGISTERS.iter().rev() {
        buffer.pop_r(*reg)?;
    }
    buffer.ret()
}

/// Emit `move Src Dst` natively, or report that the block must helper it.
///
/// Returns `true` when the move was emitted. The conditions are exactly the
/// documented inline set: `dst` an x-register that is cached, and `src` either a
/// cached x-register or a constant operand.
fn try_inline_move(
    buffer: &mut CodeBuffer,
    src: &Operand,
    dst: &Operand,
    allocation: &Allocation,
) -> Result<bool> {
    let Some(dst_x) = x_register(dst) else {
        return Ok(false);
    };
    let Some(dst_reg) = allocation.slot(dst_x) else {
        return Ok(false);
    };
    match x_register(src) {
        Some(src_x) => match allocation.slot(src_x) {
            Some(src_reg) => {
                buffer.mov_rr(dst_reg, src_reg)?;
                Ok(true)
            }
            None => Ok(false),
        },
        None => match constant_word(src) {
            Some(word) => {
                buffer.mov_ri(dst_reg, word)?;
                Ok(true)
            }
            None => Ok(false),
        },
    }
}

/// Emit the spill-then-helper-then-exit sequence for instruction `(func, pc)`.
///
/// The spill comes **first**: the helper is the interpreter and reads `machine.x`,
/// which must already hold the values the inline prefix produced. The `rt` slot is
/// reloaded into `rdi` (the helper's first argument); the helper preserves the
/// callee-saved registers, so the block's cached values survive it — but the block
/// must **not** spill after the call: the helper legitimately wrote `machine.x`.
fn emit_helper(
    buffer: &mut CodeBuffer,
    done: CodeLabel,
    allocation: &Allocation,
    func: u32,
    pc: u32,
) -> Result<()> {
    spill(buffer, allocation)?;
    buffer.mov_rm(Reg::Rdi, Reg::Rsp, RUNTIME_SLOT_DISP)?;
    buffer.mov_ri(Reg::Rsi, i64::from(func))?;
    buffer.mov_ri(Reg::Rdx, i64::from(pc))?;
    let helper: StepHelper = jit_step_one;
    buffer.mov_ri(Reg::R11, helper as *const () as i64)?;
    buffer.call_r(Reg::R11)?;
    buffer.jmp(done)
}

/// Write every cached x-register back into `machine.x` (`rbx` holds the base).
fn spill(buffer: &mut CodeBuffer, allocation: &Allocation) -> Result<()> {
    for (x, reg) in &allocation.slots {
        buffer.mov_mr(Reg::Rbx, x_offset(*x), *reg)?;
    }
    Ok(())
}

/// The byte offset of x-register `x` inside `machine.x` (a `[Eterm]` of 8-byte
/// words).
fn x_offset(x: u32) -> i32 {
    (x as i32) * (mem::size_of::<Eterm>() as i32)
}

/// The x-register index an operand names, if any (`X(n)` or the `tr`-annotated
/// `Typed { reg: X(n), .. }` — the `Type` annotation is irrelevant to the value).
fn x_register(operand: &Operand) -> Option<u32> {
    match operand {
        Operand::X(n) => Some(*n),
        Operand::Typed {
            reg: RegRef::X(n),
            ..
        } => Some(*n),
        _ => None,
    }
}

/// The raw `Eterm` word of a constant operand, as [`crate::operand::read_operand`]
/// evaluates it (`make_small`/`make_atom`/`NIL`).
fn constant_word(operand: &Operand) -> Option<i64> {
    match operand {
        Operand::Integer(value) => Some(make_small(*value).raw() as i64),
        Operand::Character(c) => Some(make_small(i64::from(*c)).raw() as i64),
        Operand::Atom(atom) => Some(make_atom(*atom).raw() as i64),
        Operand::Nil => Some(NIL.raw() as i64),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The small-int arithmetic fast path
// ---------------------------------------------------------------------------
//
// The tier's **one** bounded optimisation: `gc_bif` of `erlang:'+'/2` and
// `erlang:'-'/2` over small integers, emitted natively behind a guard that can
// only ever *reject* (falling back to `jit_step_one`). It cannot produce a value
// the interpreter step would not have produced:
//
// * the two operands must both be small integers — the same `is_small` the
//   `operand`/`Test` seams use (`(word & 0xF) == 0xF`, `../../beam_term/src/immediate.rs`);
// * the arithmetic is done on the tagged words, which is exact because
//   `make_small(v) == v * 16 + 15`, so `a_word + b_word - 15 == make_small(va+vb)`
//   (`- 15` becomes `+ 15` for the subtraction form);
// * any `i64` overflow of either step, and any result outside
//   `[MIN_SMALL_WORD, MAX_SMALL_WORD]`, jumps to the helper, which calls the real
//   BIF (`BeamError`/bignum/float path) — so a result that is not a small
//   integer is never fabricated here.
//
// Everything else (another BIF, another module, an operand that is not a cached
// x-register or a small constant, a destination that is not a cached x-register)
// keeps today's behaviour: one `jit_step_one` call.

/// The two BIFs the fast path lowers — `erlang:'+'/2` and `erlang:'-'/2`, and
/// nothing else.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SmallArith {
    /// `erlang:'+'/2`.
    Add,
    /// `erlang:'-'/2`.
    Sub,
}

/// `TAG_IMMED1_MASK` as an `i64` — the low-nibble mask `is_small` tests with.
const SMALL_MASK: i64 = TAG_IMMED1_MASK as i64;

/// `TAG_IMMED1_SMALL` as an `i64` — the immediate-1 small tag, which is also
/// `make_small`'s addend (`((v << TAG_IMMED1_SIZE) + TAG_IMMED1_SMALL)`).
const SMALL_TAG: i64 = TAG_IMMED1_SMALL as i64;

/// The emitted guard compares the masked word against [`SMALL_MASK`] (one
/// register holds both, per the objective's shape), so the mask and the tag must
/// be the same nibble. `beam_term` defines both as `0b1111`.
const _: () = assert!(SMALL_MASK == SMALL_TAG);

/// The shift `make_small` applies (`TAG_IMMED1_SIZE`).
const SMALL_SHIFT: u32 = TAG_IMMED1_SIZE;

/// The raw word of the smallest small integer (`make_small(MIN_SMALL)`).
///
/// `MIN_SMALL * 16` is exactly `i64::MIN`, so the (saturating, hence exact) word
/// is `i64::MIN + 15`.
const MIN_SMALL_WORD: i64 = MIN_SMALL
    .saturating_mul(1i64 << SMALL_SHIFT)
    .saturating_add(SMALL_TAG);

/// The raw word of the largest small integer (`make_small(MAX_SMALL)`) — which
/// saturates to exactly `i64::MAX`.
const MAX_SMALL_WORD: i64 = MAX_SMALL
    .saturating_mul(1i64 << SMALL_SHIFT)
    .saturating_add(SMALL_TAG);

/// The scratch machine registers the fast path uses for the two operands, the
/// tag/range temporary, the constant, and the result.
///
/// All are caller-saved (System V AMD64), so clobbering them is free, and none
/// overlaps the block's cached x-registers (`r12`–`r15`) or its reserved `rbx`
/// (x-register base) / `rsp` (the `rt` slot). `r11` is also `emit_helper`'s
/// helper-address scratch, but the helper sequence is emitted *after* the fast
/// path has finished with it.
const FAST_A: Reg = Reg::Rax;
/// The second operand's scratch register.
const FAST_B: Reg = Reg::R8;
/// The tag/range temporary.
const FAST_T: Reg = Reg::R10;
/// The tag mask / arithmetic constant.
const FAST_C: Reg = Reg::Rcx;
/// The computed result word.
const FAST_R: Reg = Reg::R11;

/// A `gc_bif` shape the fast path can lower.
struct FastArith<'a> {
    /// Which of the two BIFs is called.
    op: SmallArith,
    /// The first operand (a cacheable `x(i)` or a small-int constant).
    a: &'a Operand,
    /// The second operand.
    b: &'a Operand,
    /// The x-register index the result is written to.
    dst_x: u32,
}

impl FastArith<'_> {
    /// The x-register indices the shape needs cached, in load order (the
    /// operands first, then the destination) — exactly the order
    /// [`allocate_registers`] caches them in.
    fn registers(&self) -> Vec<u32> {
        let mut registers = Vec::with_capacity(3);
        for operand in [self.a, self.b] {
            if let Some(x) = x_register(operand) {
                registers.push(x);
            }
        }
        registers.push(self.dst_x);
        registers
    }
}

/// The fast path's shape test for `instr`, a *purely syntactic* check (it never
/// consults the register allocation, so [`allocate_registers`] and [`emit_block`]
/// agree on which `gc_bif`s the fast path targets).
///
/// The shape is the small-int `erlang:'+'/2`/`'-'/2` of `args.len() == 2` whose
/// every operand is a cacheable x-register or a small-int constant and whose
/// destination is a cacheable x-register.
fn fast_arith<'a>(module: &Module, instr: &'a Instr) -> Option<FastArith<'a>> {
    let Instr::GcBif {
        arity, bif, args, dst, ..
    } = instr
    else {
        return None;
    };
    let (module_name, function_name, import_arity) = import_names(module, *bif)?;
    if module_name != "erlang" || import_arity != 2 || *arity != 2 || args.len() != 2 {
        return None;
    }
    let op = match function_name {
        "+" => SmallArith::Add,
        "-" => SmallArith::Sub,
        _ => return None,
    };
    let dst_x = x_register(dst)?;
    if dst_x >= MAX_REGISTER || args.iter().any(|operand| !loadable(operand)) {
        return None;
    }
    Some(FastArith {
        op,
        a: &args[0],
        b: &args[1],
        dst_x,
    })
}

/// Whether `operand` is something the fast path can load into a scratch
/// register: a cacheable x-register or a small-int constant.
fn loadable(operand: &Operand) -> bool {
    match x_register(operand) {
        Some(x) => x < MAX_REGISTER,
        None => small_constant(operand).is_some(),
    }
}

/// The raw `Eterm` word of a **small-int** constant operand (`integer`/
/// `character`), or `None` for anything else — including the non-small `atom`/
/// `nil` constants, which the run-time guard would reject anyway (so refusing
/// them at compile time only saves the emission).
fn small_constant(operand: &Operand) -> Option<i64> {
    constant_word(operand).filter(|word| (word & SMALL_MASK) == SMALL_TAG)
}

/// Resolve an imported `{module,function,arity}` to its names, or `None` when
/// the [`ImportRef`] or one of its atoms is missing from `module`.
///
/// This deliberately reproduces `crate::interpreter`'s (private) `import_target`:
/// the fast path must classify a BIF by exactly the strings the interpreter's
/// `GcBif` arm hands to [`ExecutionContext::call_bif`], or it would lower a call
/// the interpreter routes somewhere else.
fn import_names<'m>(module: &'m Module, target: ImportRef) -> Option<(&'m str, &'m str, u32)> {
    let import = module.imports.get(target.0 as usize)?;
    let module_name = module.atom_name(import.module)?;
    let function_name = module.atom_name(import.function)?;
    Some((module_name, function_name, import.arity))
}

/// Where the fast path reads one operand from.
#[derive(Clone, Copy)]
enum FastSource {
    /// A cached x-register (already loaded by the prologue).
    Cached(Reg),
    /// A small-int constant, loaded as a raw `Eterm` word.
    Constant(i64),
}

/// The [`FastSource`] for `operand`, or `None` when it is neither cached nor a
/// small constant.
fn fast_source(operand: &Operand, allocation: &Allocation) -> Option<FastSource> {
    match x_register(operand) {
        Some(x) => allocation.slot(x).map(FastSource::Cached),
        None => small_constant(operand).map(FastSource::Constant),
    }
}

/// Load `source` into scratch register `reg`.
fn load_into(buffer: &mut CodeBuffer, source: FastSource, reg: Reg) -> Result<()> {
    match source {
        FastSource::Cached(src) => buffer.mov_rr(reg, src),
        FastSource::Constant(word) => buffer.mov_ri(reg, word),
    }
}

/// Lower a fast-path-eligible `gc_bif` natively, or report that the block must
/// helper it.
///
/// Returns `true` when the whole guarded sequence was emitted, leaving the
/// result in the destination's cached register and letting the block continue
/// with the next instruction. Returns `false` **without having emitted a byte**
/// when the shape matched ([`fast_arith`]) but an operand or the destination is
/// not cached — the caller then emits exactly today's `jit_step_one` fallback.
///
/// The emitted shape (see the section comment above for why it is exact):
///
/// ```text
///   mov  rax, a          ; or movabs rax, <small constant>
///   mov  r8,  b
///   mov  rcx, SMALL_MASK
///   mov  r10, rax ; and r10, rcx ; cmp r10, rcx ; jne fallback   ; a is small?
///   mov  r10, r8  ; and r10, rcx ; cmp r10, rcx ; jne fallback   ; b is small?
///   mov  r11, rax
///   add  r11, r8            ; sub for '-'
///   jo   fallback           ; not both representable as an i64 tagged word
///   mov  rcx, SMALL_TAG  ; the double-tag adjustment make_small re-adds
///   sub  r11, rcx           ; add for '-'
///   jo   fallback
///   mov  rcx, MIN_SMALL_WORD ; cmp r11, rcx ; jl fallback
///   mov  rcx, MAX_SMALL_WORD ; cmp r11, rcx ; jg fallback
///   mov  <dst>, r11         ; store the result in its cached x-register
///   jmp  fast_done
/// fallback:
///   <spill cached x-registers; call jit_step_one; jmp the block epilogue>
/// fast_done:
///   <emit the rest of the block>
/// ```
///
/// The fast path never spills: the destination's cached register is written
/// back by the next helper call or by the block's own exit spill, so the
/// spill-across-blocks discipline is unchanged.
fn try_inline_gc_bif(
    buffer: &mut CodeBuffer,
    module: &Module,
    instr: &Instr,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
) -> Result<bool> {
    let Some(fast) = fast_arith(module, instr) else {
        return Ok(false);
    };
    let Some(a_source) = fast_source(fast.a, allocation) else {
        return Ok(false);
    };
    let Some(b_source) = fast_source(fast.b, allocation) else {
        return Ok(false);
    };
    let Some(dst_reg) = allocation.slot(fast.dst_x) else {
        return Ok(false);
    };

    let fallback: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();

    // Load both operands into scratch registers.
    load_into(buffer, a_source, FAST_A)?;
    load_into(buffer, b_source, FAST_B)?;

    // Both operands must be small integers: `(word & 0xF) == 0xF`. The mask and
    // the tag are the same nibble (asserted above), so one register serves both.
    buffer.mov_ri(FAST_C, SMALL_MASK)?;
    for operand in [FAST_A, FAST_B] {
        buffer.mov_rr(FAST_T, operand)?;
        buffer.and_rr(FAST_T, FAST_C)?;
        buffer.cmp_rr(FAST_T, FAST_C)?;
        buffer.jcc(Cond::Ne, fallback)?;
    }

    // Tagged-word arithmetic; an `i64` overflow of either step means the result
    // is not representable as a tagged small word, so the helper computes it.
    buffer.mov_rr(FAST_R, FAST_A)?;
    match fast.op {
        SmallArith::Add => buffer.add_rr(FAST_R, FAST_B)?,
        SmallArith::Sub => buffer.sub_rr(FAST_R, FAST_B)?,
    }
    buffer.jcc(Cond::O, fallback)?;
    buffer.mov_ri(FAST_C, SMALL_TAG)?;
    match fast.op {
        SmallArith::Add => buffer.sub_rr(FAST_R, FAST_C)?,
        SmallArith::Sub => buffer.add_rr(FAST_R, FAST_C)?,
    }
    buffer.jcc(Cond::O, fallback)?;

    // Range check: a tagged word is a valid small integer iff it lies in
    // `[MIN_SMALL_WORD, MAX_SMALL_WORD]` (the encoding `v -> v*16 + 15` maps the
    // value range onto that interval bijectively).
    buffer.mov_ri(FAST_C, MIN_SMALL_WORD)?;
    buffer.cmp_rr(FAST_R, FAST_C)?;
    buffer.jcc(Cond::L, fallback)?;
    buffer.mov_ri(FAST_C, MAX_SMALL_WORD)?;
    buffer.cmp_rr(FAST_R, FAST_C)?;
    buffer.jcc(Cond::G, fallback)?;

    // Success: store the result and skip the fallback.
    buffer.mov_rr(dst_reg, FAST_R)?;
    buffer.jmp(fast_done)?;

    // Rejected (or not lowered): one interpreter step, then leave the block.
    buffer.bind(fallback)?;
    emit_helper(buffer, done, allocation, func, pc)?;
    buffer.bind(fast_done)?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// The driver
// ---------------------------------------------------------------------------

/// Run exported `function/arity` through the compiled blocks.
///
/// The native lookup is the compiled block table: any compiled `(func, pc)` is
/// entered, and every other address stays on the interpreter step. See
/// [`run_native`] for the shared driver the two native tiers run on.
fn run_compiled(
    compiled: &CompiledModule,
    machine: &mut Machine,
    module: &Module,
    function: Atom,
    arity: u32,
    args: &[Eterm],
    ctx: &mut dyn ExecutionContext,
) -> Result<Eterm> {
    // A block loads its cached x-registers out of `machine.x` without a bounds
    // check, so a machine whose register file is shorter than the highest cached
    // index cannot enter native code; such a run stays on the interpreter step.
    let native_ok = match compiled.highest_cached_register {
        Some(highest) => machine.x.len() > highest as usize,
        None => true,
    };
    let code = BlockEntries {
        compiled,
        native_ok,
    };
    run_native(machine, module, function, arity, args, ctx, &code)
}

/// The baseline tier's [`NativeCode`]: any compiled basic block may be entered.
struct BlockEntries<'a> {
    /// The compiled block table the addresses are looked up in.
    compiled: &'a CompiledModule,
    /// Whether the running machine's register file covers every cached slot; when
    /// it does not, no block may be entered (see [`run_compiled`]).
    native_ok: bool,
}

impl NativeCode for BlockEntries<'_> {
    fn enter(&self, rt: *mut JitRuntime<'_>, func: u32, pc: u32) -> Option<i64> {
        if !self.native_ok {
            return None;
        }
        let block_entry = self.compiled.entry(func, pc)?;
        // SAFETY: `block_entry` points just past the start of a complete native
        // function emitted by `emit_block` with exactly the `NativeBlock` signature
        // (`extern "C" fn(*mut JitRuntime) -> i64`), inside a buffer sealed
        // read+execute by `compile_module`; `rt` is the live driver-owned runtime
        // block the driver does not otherwise borrow during the call.
        let block: NativeBlock = unsafe { mem::transmute(block_entry) };
        Some(block(rt))
    }
}

/// The driver loop both native tiers run on.
///
/// Mirrors [`Machine::run_label`](crate::Machine)'s setup (reset, seed
/// `x[0..arity)` from `args`, push the bottom frame, bind the module's literal
/// area, resolve the export label and build the module-wide label map), then
/// loops:
///
/// * ask `code` (the tier's [`NativeCode`] lookup) whether `(func, pc)` can run
///   natively; when it can, the returned block-ABI word is folded exactly like a
///   block's return value — [`HALT`] finishes the run, [`ERROR`] propagates
///   `rt.error`, a `(func, pc)` pair continues there;
/// * otherwise take exactly one interpreter step
///   ([`Machine::step_instruction`](crate::Machine)), identically to the
///   interpreter's own loop.
///
/// The second path is not an emergency fallback: it is each tier's documented
/// per-instruction deopt, and it is also what happens for every address the tier
/// compiled no entry point for (and, for the baseline tier, when the running
/// machine's register file is too small — see [`run_compiled`]).
pub(crate) fn run_native(
    machine: &mut Machine,
    module: &Module,
    function: Atom,
    arity: u32,
    args: &[Eterm],
    ctx: &mut dyn ExecutionContext,
    code: &dyn NativeCode,
) -> Result<Eterm> {
    let entry = module
        .export(function, arity)
        .ok_or_else(|| error::unsupported("jit: export not found"))?;

    machine.reset();
    let n = arity.min(args.len() as u32);
    for (i, arg) in args.iter().take(n as usize).enumerate() {
        machine.set_x(i as u32, *arg)?;
    }
    machine.push_frame(0)?;
    machine.set_literals(module.literals.clone());
    let labels = interpreter::build_label_map(module);
    let (mut func, mut pc) = interpreter::resolve_label(module, entry)?;

    // The objects the runtime's raw pointers refer to. `machine_ptr`/`ctx_ptr` are
    // the same addresses the [`JitRuntime`] holds; all of them outlive the loop, and
    // no `&mut` of the driver's aliases them while a native call is in flight.
    let x_regs: *mut Eterm = machine.x.as_mut_ptr();
    let machine_ptr: *mut Machine = &mut *machine;
    let ctx_ptr = &mut *ctx as *mut dyn ExecutionContext;
    let mut rt = JitRuntime {
        machine: machine_ptr,
        x_regs,
        module: module as *const Module,
        labels: &labels as *const BTreeMap<Label, (u32, u32)>,
        ctx: ctx_ptr,
        error: None,
    };

    loop {
        if let Some(word) = code.enter(&mut rt, func, pc) {
            if word == HALT {
                // SAFETY: `machine_ptr` points at the live driver-owned `Machine`;
                // no native code is running here.
                return unsafe { (*machine_ptr).finish() };
            }
            if word == ERROR {
                return Err(rt
                    .error
                    .take()
                    .unwrap_or_else(|| error::unsupported("jit: helper error")));
            }
            match decode_target(word) {
                Some((next_func, next_pc)) => {
                    func = next_func;
                    pc = next_pc;
                    continue;
                }
                None => return Err(error::unsupported("jit: bad block target")),
            }
        }

        // SAFETY: the native call returned, so no `&mut` derived from `rt` is alive;
        // `machine_ptr`/`ctx_ptr` point at the driver's live, distinct objects
        // (`Machine` and the `ExecutionContext`), never at each other.
        let step = unsafe { (*machine_ptr).step_instruction(module, &labels, func, pc, &mut *ctx_ptr)? };
        match step {
            Step::Next => pc += 1,
            Step::Goto {
                func: next_func,
                pc: next_pc,
            } => {
                func = next_func;
                pc = next_pc;
            }
            Step::Done => {
                // SAFETY: `machine_ptr` points at the live driver-owned `Machine`.
                return unsafe { (*machine_ptr).finish() };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MinimalContext;
    use beam_loader::{Import, NO_FAIL};

    #[test]
    fn target_encoding_round_trips() {
        for (func, pc) in [(0u32, 0u32), (0, 1), (1, 0), (3, 12), (1024, 65_535)] {
            let word = encode_target(func, pc);
            assert!(word >= 0, "a valid target must be non-negative");
            assert_ne!(word, HALT);
            assert_ne!(word, ERROR);
            assert_eq!(decode_target(word), Some((func, pc)));
        }
    }

    #[test]
    fn sentinels_are_distinct_from_every_valid_target() {
        assert_eq!(HALT, -1);
        assert_eq!(ERROR, -2);
        assert_ne!(HALT, ERROR);
        assert_eq!(decode_target(HALT), None);
        assert_eq!(decode_target(ERROR), None);
        // The smallest encodable target is `(0, 0)`; it is not a sentinel.
        assert_eq!(decode_target(encode_target(0, 0)), Some((0, 0)));
        // Any negative word decodes to nothing (the compiler never emits one).
        assert_eq!(decode_target(-3), None);
        assert_eq!(decode_target(i64::MIN), None);
    }

    #[test]
    fn the_runtime_layout_matches_the_offsets_generated_code_uses() {
        // Generated code loads `rt.x_regs` from `[rt + 8]` (`RUNTIME_XREGS_DISP`) and
        // the helper is handed `rt` itself; this pins the two offsets.
        let module = Module::default();
        let labels = BTreeMap::new();
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let rt = JitRuntime {
            machine: &mut machine,
            x_regs: machine.x.as_mut_ptr(),
            module: &module,
            labels: &labels,
            ctx: &mut ctx,
            error: None,
        };
        let base = &rt as *const JitRuntime as usize;
        let machine_field = &rt.machine as *const *mut Machine as usize;
        let x_regs_field = &rt.x_regs as *const *mut Eterm as usize;
        assert_eq!(machine_field - base, RUNTIME_MACHINE_DISP as usize);
        assert_eq!(x_regs_field - base, RUNTIME_XREGS_DISP as usize);
        assert_eq!(RUNTIME_SLOT_DISP, 0);
    }

    #[test]
    fn cached_slots_index_the_x_register_array_not_the_machine_struct() {
        // The array base the driver hands the runtime is `machine.x`'s data pointer,
        // so a block's `[base + n*8]` addressing reads the x-registers.
        let mut machine = Machine::new();
        machine.set_x(3, make_small(17)).unwrap();
        let base = machine.x.as_ptr();
        // SAFETY: `base` is `machine.x`'s data pointer, the array is not resized
        // between here and the read, and index 3 is in bounds (`MAX_REGISTER` long).
        let word = unsafe { *base.add(3) };
        assert_eq!(word, make_small(17));
    }

    #[test]
    fn allocation_prefers_the_first_appearing_cacheable_registers() {
        let code = vec![
            Instr::Move {
                src: Operand::X(3),
                dst: Operand::X(1),
            },
            Instr::Move {
                src: Operand::Integer(7),
                dst: Operand::X(1),
            },
            Instr::Move {
                src: Operand::Nil,
                dst: Operand::Y(0),
            },
        ];
        let allocation = allocate_registers(&Module::default(), &code, 0, code.len());
        // x3 (src) then x1 (dst) — the `Y` destination is not cacheable.
        assert_eq!(
            allocation.slots,
            vec![(3, Reg::R12), (1, Reg::R13)]
        );
        assert_eq!(allocation.slot(3), Some(Reg::R12));
        assert_eq!(allocation.slot(1), Some(Reg::R13));
        assert_eq!(allocation.slot(2), None);
    }

    #[test]
    fn allocation_ignores_registers_outside_the_engine_bound() {
        // A register index the engine can never hold must not be cached: reading it
        // would be out of bounds, so the helper reports the faithful `bad_register`.
        let code = vec![Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::X(MAX_REGISTER),
        }];
        let allocation = allocate_registers(&Module::default(), &code, 0, code.len());
        assert!(allocation.slots.is_empty());
    }

    #[test]
    fn constant_words_match_the_operand_seam() {
        assert_eq!(constant_word(&Operand::Integer(5)), Some(make_small(5).raw() as i64));
        assert_eq!(constant_word(&Operand::Nil), Some(NIL.raw() as i64));
        assert_eq!(
            constant_word(&Operand::Character(u32::from(b'a'))),
            Some(make_small(97).raw() as i64)
        );
        assert_eq!(constant_word(&Operand::X(0)), None);
        assert_eq!(constant_word(&Operand::Y(0)), None);
    }

    #[test]
    fn x_register_reads_both_plain_and_typed_operands() {
        assert_eq!(x_register(&Operand::X(4)), Some(4));
        assert_eq!(
            x_register(&Operand::Typed {
                reg: RegRef::X(4),
                ty: beam_loader::TypeIndex(0),
            }),
            Some(4)
        );
        assert_eq!(x_register(&Operand::Y(4)), None);
        assert_eq!(x_register(&Operand::Nil), None);
    }

    // -- the small-int arithmetic fast path ---------------------------------

    /// A module carrying `imports`, enough to exercise [`fast_arith`].
    fn module_with_imports(imports: &[(&str, &str, u32)]) -> (Module, Vec<ImportRef>) {
        let mut module = Module::default();
        module.name = module.atom_table.intern("m");
        let mut refs = Vec::new();
        for (module_name, function_name, arity) in imports {
            let module_atom = module.atom_table.intern(module_name);
            let function_atom = module.atom_table.intern(function_name);
            module.imports.push(Import {
                module: module_atom,
                function: function_atom,
                arity: *arity,
            });
            refs.push(ImportRef((module.imports.len() - 1) as u32));
        }
        (module, refs)
    }

    /// A `GcBif` instruction over `bif` with the given operands.
    fn gc_bif(bif: ImportRef, args: Vec<Operand>, dst: Operand) -> Instr {
        Instr::GcBif {
            arity: args.len() as u8,
            bif,
            fail: NO_FAIL,
            live: args.len() as u32,
            args,
            dst,
        }
    }

    #[test]
    fn fast_path_constants_match_the_frozen_small_encoding() {
        // The guard's nibble and the two range bounds are derived from
        // `beam_term`, so they must equal what `beam_term` itself encodes.
        assert_eq!(SMALL_MASK, 0xF);
        assert_eq!(SMALL_TAG, 0xF);
        assert_eq!(SMALL_SHIFT, 4);
        assert_eq!(MIN_SMALL_WORD, make_small(MIN_SMALL).raw() as i64);
        assert_eq!(MAX_SMALL_WORD, make_small(MAX_SMALL).raw() as i64);
        assert_eq!(MIN_SMALL_WORD, i64::MIN + 15);
        assert_eq!(MAX_SMALL_WORD, i64::MAX);
        // Every small integer's word is tagged and inside the window the range
        // check accepts (which is why the emitted code needs no re-tag check).
        for value in [MIN_SMALL, MIN_SMALL + 1, -1, 0, 1, MAX_SMALL - 1, MAX_SMALL] {
            let word = make_small(value).raw() as i64;
            assert_eq!(word & SMALL_MASK, SMALL_TAG, "word for {value}");
            assert!(
                (MIN_SMALL_WORD..=MAX_SMALL_WORD).contains(&word),
                "word for {value} outside the accepted window"
            );
        }
        // The scratch registers must not overlap the cached x-registers or the
        // block's reserved `rbx`/`rsp`.
        for reg in [FAST_A, FAST_B, FAST_T, FAST_C, FAST_R] {
            assert!(!CACHE_REGISTERS.contains(&reg), "{reg:?} is a cache register");
            assert!(!SAVED_REGISTERS.contains(&reg), "{reg:?} is callee-saved");
        }
    }

    #[test]
    fn tagged_word_arithmetic_reproduces_make_small() {
        // The emitted fast path is exact because `make_small(v) == v*16 + 15`,
        // so `a_word + b_word - 15 == make_small(va + vb)` whenever neither step
        // overflows an `i64`; otherwise the `jo`/range guards bail to the helper.
        for (a, b) in [
            (0, 0), (2, 3), (-4, 10), (7, -20), (-1, -1),
            (MAX_SMALL - 2, 1), (MIN_SMALL + 2, -1), (MAX_SMALL, 0), (MIN_SMALL, 0),
            (MAX_SMALL, MAX_SMALL), (MIN_SMALL, -1),
        ] {
            let a_word = make_small(a).raw() as i64;
            let b_word = make_small(b).raw() as i64;
            let sum = a_word.checked_add(b_word).and_then(|s| s.checked_sub(SMALL_TAG));
            let difference = a_word.checked_sub(b_word).and_then(|d| d.checked_add(SMALL_TAG));
            for (word, expected) in [
                (sum, a.checked_add(b)),
                (difference, a.checked_sub(b)),
            ] {
                match word {
                    Some(word) if (MIN_SMALL_WORD..=MAX_SMALL_WORD).contains(&word) => {
                        assert_eq!(
                            word,
                            make_small(expected.expect("in-range word")).raw() as i64,
                            "{a} / {b}"
                        );
                    }
                    // Out of the window: the range check bails, and the true
                    // value genuinely is not a small integer.
                    Some(_) => assert!(
                        matches!(expected, Some(v) if v < MIN_SMALL || v > MAX_SMALL),
                        "{a} / {b} produced an out-of-window word in range"
                    ),
                    // An `i64` overflow: the `jo` guard bails.
                    None => {}
                }
            }
        }
    }

    #[test]
    fn fast_arith_recognizes_only_the_two_small_int_bifs() {
        let (module, refs) = module_with_imports(&[
            ("erlang", "+", 2),
            ("erlang", "-", 2),
            ("erlang", "*", 2),
            ("erlang", "+", 3),
            ("lists", "+", 2),
        ]);
        let args = vec![Operand::X(0), Operand::Integer(1)];
        for (index, expected) in [
            (0usize, Some(SmallArith::Add)),
            (1, Some(SmallArith::Sub)),
            (2, None),
            (3, None),
            (4, None),
        ] {
            let instr = gc_bif(refs[index], args.clone(), Operand::X(0));
            assert_eq!(
                fast_arith(&module, &instr).map(|fast| fast.op),
                expected,
                "import {index}"
            );
        }
        // Shapes the fast path refuses: a Y destination, a Y operand, a non-small
        // constant operand, a wrong argument count, an out-of-bound register.
        let plus = refs[0];
        let atom = module.name;
        for instr in [
            gc_bif(plus, args.clone(), Operand::Y(0)),
            gc_bif(plus, vec![Operand::X(0), Operand::Y(1)], Operand::X(0)),
            gc_bif(plus, vec![Operand::X(0), Operand::Atom(atom)], Operand::X(0)),
            gc_bif(plus, vec![Operand::X(0), Operand::Nil], Operand::X(0)),
            gc_bif(plus, vec![Operand::X(0)], Operand::X(0)),
            gc_bif(
                plus,
                vec![Operand::X(MAX_REGISTER), Operand::Integer(1)],
                Operand::X(0),
            ),
            gc_bif(plus, args.clone(), Operand::X(MAX_REGISTER)),
        ] {
            assert!(fast_arith(&module, &instr).is_none(), "{instr:?}");
        }
    }

    #[test]
    fn allocation_caches_the_fast_path_operands_and_destination() {
        let (module, refs) = module_with_imports(&[("erlang", "+", 2)]);
        let code = vec![
            Instr::Label(Label(1)),
            gc_bif(refs[0], vec![Operand::X(4), Operand::Integer(1)], Operand::X(2)),
            Instr::Return,
        ];
        let allocation = allocate_registers(&module, &code, 0, code.len());
        // x4 (the operand) then x2 (the destination), in that order.
        assert_eq!(allocation.slots, vec![(4, Reg::R12), (2, Reg::R13)]);
        assert_eq!(allocation.slot(2), Some(Reg::R13));

        // A `gc_bif` outside the fast path caches nothing.
        let (module, refs) = module_with_imports(&[("erlang", "*", 2)]);
        let code = vec![gc_bif(
            refs[0],
            vec![Operand::X(4), Operand::Integer(1)],
            Operand::X(2),
        )];
        assert!(allocate_registers(&module, &code, 0, code.len()).slots.is_empty());
    }
}
