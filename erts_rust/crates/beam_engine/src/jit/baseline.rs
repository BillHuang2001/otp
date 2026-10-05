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
//!   `(func, pc)` pair is known at compile time, so it is a constant return word.
//!
//! **Helper-executed** (`jit_step_one`, literally the interpreter's
//! [`Machine::step_instruction`](crate::Machine)): **every other instruction**, plus
//! a `move` with an operand outside the cached set and a `jump` to a label the
//! module does not define (so the interpreter's exact `unknown_label` error
//! surfaces rather than an invented one).
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
use beam_loader::{Atom, Instr, Label, Module, Operand, Reg as RegRef};
use beam_term::{make_atom, make_small, Eterm, NIL};

use crate::context::ExecutionContext;
use crate::error;
use crate::interpreter;
use crate::jit::emitter::{CodeBuffer, Label as CodeLabel, Reg, DEFAULT_CAPACITY};
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
struct JitRuntime<'a> {
    /// The process state a block reads and writes through (`stack`, frames, …).
    machine: *mut Machine,
    /// The base of the x-register array (`machine.x`) a block loads from / spills to.
    x_regs: *mut Eterm,
    /// The module being executed (its `code`, labels, imports, literals).
    module: *const Module,
    /// The module-wide `Label -> (func, pc)` map the helper resolves fail/target
    /// labels through (built once per driver call by
    /// [`interpreter::build_label_map`](crate::interpreter)).
    labels: *const BTreeMap<Label, (u32, u32)>,
    /// The [`ExecutionContext`] seam, handed back as `&mut dyn` to the helper.
    ctx: *mut (dyn ExecutionContext + 'a),
    /// Set by a helper that hit a propagating `Err`; the `ERROR` word tells the
    /// driver to take it.
    error: Option<BeamError>,
}

/// A compiled block's native signature.
type NativeBlock = for<'a> extern "C" fn(*mut JitRuntime<'a>) -> i64;

/// The `jit_step_one` helper's native signature (the block ABI plus the
/// instruction address).
type StepHelper = for<'a> extern "C" fn(*mut JitRuntime<'a>, u32, u32) -> i64;

// ---------------------------------------------------------------------------
// The backend
// ---------------------------------------------------------------------------

/// The native baseline tier.
///
/// A zero-sized selector today: compilation is per [`JitBackend::run_export`] call
/// (`run_with_tier` creates a fresh backend per run), and caching compiled modules
/// across runs — like the inlining and the block register allocator's quality — is
/// a *performance* concern deferred to a later task. This task is the mechanism and
/// its correctness.
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

    /// Compile `module` and run exported `function/arity` through the driver loop.
    fn run_export(
        &mut self,
        machine: &mut Machine,
        module: &Module,
        function: Atom,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        let compiled = compile_module(module)?;
        run_compiled(&compiled, machine, module, function, arity, args, ctx)
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
extern "C" fn jit_step_one(rt: *mut JitRuntime<'_>, func: u32, pc: u32) -> i64 {
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
            let allocation = allocate_registers(code, start, end);
            for (x, _) in &allocation.slots {
                highest_cached_register = Some(match highest_cached_register {
                    Some(prev) => prev.max(*x),
                    None => *x,
                });
            }

            let offset = buffer.len();
            emit_block(
                &mut buffer,
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
/// (~7–10 bytes each); under-estimating would turn a large module into a
/// `SystemLimit` compile error, so the estimate is deliberately loose.
/// [`CodeBuffer::with_capacity`] rounds it up to whole pages.
fn code_capacity_estimate(module: &Module) -> usize {
    let mut instructions = 0usize;
    let mut blocks = 0usize;
    for function in &module.functions {
        instructions += function.code.len();
        blocks += function.blocks.len();
    }
    64 * blocks + 16 * instructions + DEFAULT_CAPACITY
}

/// Pick the x-registers a block caches, in first-appearance order.
///
/// Only `move`s whose *shape* could be inlined contribute (`dst` an x-register and
/// `src` an x-register or a constant), so the registers a block's actual inline
/// prefix needs are collected before any register from a later, never-inlined
/// instruction. A register index outside the engine's [`MAX_REGISTER`] bound is
/// never cached: reading it would be out of bounds, whereas the interpreter step
/// reports the faithful `bad_register` error.
fn allocate_registers(code: &[Instr], start: usize, end: usize) -> Allocation {
    let mut allocation = Allocation::default();
    for instr in &code[start..end] {
        let Instr::Move { src, dst } = instr else {
            continue;
        };
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
fn emit_block(
    buffer: &mut CodeBuffer,
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
// The driver
// ---------------------------------------------------------------------------

/// Run exported `function/arity` through the compiled blocks.
///
/// Mirrors [`Machine::run_label`](crate::Machine)'s setup (reset, seed `x[0..arity)`
/// from `args`, push the bottom frame, bind the module's literal area, resolve the
/// export label and build the module-wide label map), then loops:
///
/// * if a block was compiled for `(func, pc)`, call it and decode its return word —
///   [`HALT`] finishes the run, [`ERROR`] propagates `rt.error`, a `(func, pc)`
///   pair continues there;
/// * otherwise take exactly one interpreter step
///   ([`Machine::step_instruction`](crate::Machine)), identically to the
///   interpreter's own loop.
///
/// The second path is not an emergency fallback: it is the tier's documented
/// per-instruction deopt, and it is also what happens when the running machine's
/// register file is smaller than the highest x-register a block caches (native
/// execution would read outside `machine.x`), and for every address no block starts
/// at.
fn run_compiled(
    compiled: &CompiledModule,
    machine: &mut Machine,
    module: &Module,
    function: Atom,
    arity: u32,
    args: &[Eterm],
    ctx: &mut dyn ExecutionContext,
) -> Result<Eterm> {
    let entry = module
        .export(function, arity)
        .ok_or_else(|| error::unsupported("baseline jit: export not found"))?;

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
    // no `&mut` of the driver's aliases them while a block call is in flight.
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

    // A block loads its cached x-registers out of `machine.x` without a bounds
    // check, so a machine whose register file is shorter than the highest cached
    // index cannot enter native code; such a run stays on the interpreter step.
    let native_ok = match compiled.highest_cached_register {
        Some(highest) => machine.x.len() > highest as usize,
        None => true,
    };

    loop {
        if native_ok {
            if let Some(block_entry) = compiled.entry(func, pc) {
                // SAFETY: `block_entry` points just past the start of a complete
                // native function emitted by `emit_block` with exactly the
                // `NativeBlock` signature (`extern "C" fn(*mut JitRuntime) -> i64`),
                // inside a buffer sealed read+execute by `compile_module`; `rt` is a
                // live local the driver does not otherwise borrow during the call.
                let block: NativeBlock = unsafe { mem::transmute(block_entry) };
                let word = block(&mut rt);
                if word == HALT {
                    // SAFETY: `machine_ptr` points at the live driver-owned `Machine`;
                    // no block is running here.
                    return unsafe { (*machine_ptr).finish() };
                }
                if word == ERROR {
                    return Err(rt
                        .error
                        .take()
                        .unwrap_or_else(|| error::unsupported("baseline jit: helper error")));
                }
                match decode_target(word) {
                    Some((next_func, next_pc)) => {
                        func = next_func;
                        pc = next_pc;
                        continue;
                    }
                    None => return Err(error::unsupported("baseline jit: bad block target")),
                }
            }
        }

        // SAFETY: the block call returned, so no `&mut` derived from `rt` is alive;
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
        let allocation = allocate_registers(&code, 0, code.len());
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
        let allocation = allocate_registers(&code, 0, code.len());
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
}
