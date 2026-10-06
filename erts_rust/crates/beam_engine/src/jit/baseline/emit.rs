//! The baseline JIT's emitter: typed IR → x86-64 basic blocks.
//!
//! The register allocator ([`allocate_registers`]) and the per-instruction
//! lowering ([`emit_block`]) — the inline `move`/`swap`/`jump`/metadata set, the
//! spill-across-blocks discipline ([`CACHE_REGISTERS`]) and the tier's one
//! optimisation, the guarded small-int `erlang:'+'/2`/`'-'/2` fast path
//! ([`try_inline_gc_bif`]). [`compile_module`] lowers every basic block of a
//! module into the [`CompiledModule`] artifact. See `super` for the block ABI (in
//! `runtime`) and the driver that runs the emitted code.
//!
//! **Only [`CACHE_REGISTERS`]`.len()` x-registers are held in machine registers**;
//! every other x-register a block names is addressed **directly in `machine.x`**
//! (`[rbx + x*8]`, `rbx` = the array base the prologue loads from
//! [`RUNTIME_XREGS_DISP`]) — the same slot the exit spill writes — so a block that
//! touches more registers than the cache budget still runs natively instead of
//! deopting. That is safe precisely because the cache membership is fixed for the
//! whole block ([`Allocation`]): an x-register is either *always* held in its
//! machine register (and written back at every block exit) or *never* cached, so
//! the two representations can never disagree. The highest x-register index a block
//! touches natively — cached or direct — is recorded on the [`CompiledModule`] so
//! [`run_compiled`](super::run_compiled) can refuse native entry to a machine whose
//! register file does not cover it.
#![allow(unsafe_code)]

use std::collections::{BTreeMap, HashMap};
use std::mem;

use beam_common::Result;
use beam_loader::{ImportRef, Instr, Label, Module, Operand, Reg as RegRef};
use beam_term::{
    make_atom, make_small, Eterm, MAX_SMALL, MIN_SMALL, NIL, TAG_IMMED1_MASK, TAG_IMMED1_SIZE,
    TAG_IMMED1_SMALL,
};

use crate::interpreter;
use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg, DEFAULT_CAPACITY};
use crate::machine::MAX_REGISTER;

use super::runtime::{encode_target, StepHelper, RUNTIME_XREGS_DISP};
use super::{jit_step_one, CompiledModule};

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
pub(crate) const SAVED_REGISTERS: [Reg; 5] = [Reg::Rbx, Reg::R12, Reg::R13, Reg::R14, Reg::R15];

/// The machine registers a block caches x-registers in — `rbx` is reserved for the
/// machine base pointer, so four x-registers fit.
pub(crate) const CACHE_REGISTERS: [Reg; 4] = [Reg::R12, Reg::R13, Reg::R14, Reg::R15];

/// The byte offset of the prologue-reserved `rt` slot from `rsp`.
pub(crate) const RUNTIME_SLOT_DISP: i32 = 0;

/// The caller-saved scratch registers a block uses to shuffle values between
/// `machine.x` slots (a `move`/`swap` whose source, destination, or both are not
/// cached), and for the raw `Eterm` word of a constant stored into `machine.x`.
///
/// Both are caller-saved (System V AMD64), so clobbering them across an instruction
/// boundary is free; neither is a cache register ([`CACHE_REGISTERS`]) nor the
/// block's reserved `rbx` (the `machine.x` base) / `rsp` (the `rt` slot). They are
/// used strictly *within* one instruction's lowering — no value survives into the
/// next instruction — so they may also be reused by the small-int fast path's
/// temporaries.
pub(crate) const MOVE_SCRATCH: Reg = Reg::R11;
/// The second `machine.x`-shuffle scratch register (`swap` needs two values in
/// flight at once).
pub(crate) const MOVE_SCRATCH_2: Reg = Reg::R10;

/// Record that x-register `x` is touched natively by the block (loaded into a
/// cache register, or addressed directly in `machine.x`).
fn touch(highest: &mut Option<u32>, x: u32) {
    *highest = Some(match *highest {
        Some(previous) => previous.max(x),
        None => x,
    });
}

/// A block's register allocation: the `(x-register index, machine register)` pairs
/// the prologue loads and the epilogue spills.
#[derive(Default)]
pub(crate) struct Allocation {
    /// The slots, in assignment order (`src` before `dst` within a `move`).
    pub(crate) slots: Vec<(u32, Reg)>,
}

impl Allocation {
    /// The machine register holding x-register `x`, if it is cached.
    pub(crate) fn slot(&self, x: u32) -> Option<Reg> {
        self.slots
            .iter()
            .find(|(n, _)| *n == x)
            .map(|(_, reg)| *reg)
    }
}

/// Lower every `Function`'s every `Block` of `module` into a native function.
pub(crate) fn compile_module(module: &Module) -> Result<CompiledModule> {
    let labels = interpreter::build_label_map(module);
    let mut buffer = CodeBuffer::with_capacity(code_capacity_estimate(module))?;
    let mut entries: HashMap<(u32, u32), *const u8> = HashMap::new();
    let mut highest_cached_register: Option<u32> = None;
    let mut highest_native_register: Option<u32> = None;

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
                touch(&mut highest_cached_register, *x);
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
                &mut highest_native_register,
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
        highest_native_register,
    })
}

/// A generous byte-count estimate for the code `module` lowers to.
///
/// A block costs a prologue/epilogue pair (~64 bytes) plus its inlined
/// instructions; a `move`/`swap` whose operands live in `machine.x` is at most two
/// loads plus two stores (~28 bytes), so [`INSTRUCTION_BYTES`] is budgeted per
/// instruction. A `gc_bif` costs either the helper call sequence (~50 bytes) or,
/// when the small-int fast path lowers it, the whole guarded sequence plus its
/// helper fallback ([`FAST_PATH_BYTES`]).
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
    64 * blocks + INSTRUCTION_BYTES * instructions + FAST_PATH_BYTES * gc_bifs + DEFAULT_CAPACITY
}

/// The code-size budget reserved for one instruction (see
/// [`code_capacity_estimate`]) — comfortably above the largest lowering (a
/// two-`machine.x`-operand `swap`: two loads, two stores and a constant).
const INSTRUCTION_BYTES: usize = 32;

/// The code-size budget reserved for one `gc_bif` (see
/// [`code_capacity_estimate`]): the guarded small-int fast path (~140 bytes with
/// `machine.x` operand loads and a `machine.x` result store) plus the
/// spill-and-`jit_step_one` fallback it branches to (~70 bytes).
const FAST_PATH_BYTES: usize = 240;

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
pub(crate) fn allocate_registers(
    module: &Module,
    code: &[Instr],
    start: usize,
    end: usize,
) -> Allocation {
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
///
/// `highest_touched` accumulates the highest x-register index the emitted code
/// touches natively (a cached load/store, or a direct `machine.x` access); the
/// driver refuses native entry to a machine whose register file does not cover it.
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
    highest_touched: &mut Option<u32>,
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
        touch(highest_touched, *x);
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
                if !try_inline_move(buffer, src, dst, allocation, highest_touched)? {
                    emit_helper(buffer, done, allocation, func, pc)?;
                    fell_through = false;
                    break;
                }
            }
            // `swap A B` exchanges two x-registers; a block normally caches fewer
            // registers than the instruction touches, so this is a common deopt it
            // is cheap to remove (a three-`mov` shuffle, register or memory).
            Instr::Swap { a, b } => {
                if !try_inline_swap(buffer, a, b, allocation, highest_touched)? {
                    emit_helper(buffer, done, allocation, func, pc)?;
                    fell_through = false;
                    break;
                }
            }
            // The one optimised BIF: the small-int `erlang:'+'/2`/`'-'/2`. When
            // the guarded fast path is emitted the block keeps going; otherwise
            // (any other BIF, an un-addressable operand, or a register the budget
            // could not cache) it is exactly the `_` arm below.
            Instr::GcBif { .. } => {
                if !try_inline_gc_bif(buffer, module, instr, allocation, done, func, pc, highest_touched)? {
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

/// Where a `move`'s source value comes from.
enum MoveSource {
    /// An x-register the block holds in a machine register.
    Cached(Reg),
    /// An x-register the block does **not** cache — read straight from its
    /// `machine.x` slot (`[rbx + x*8]`), which is where the value is at a block
    /// boundary.
    Memory(u32),
    /// A constant operand, materialised as its raw `Eterm` word.
    Constant(i64),
}

/// Emit `move Src Dst` natively, or report that the block must helper it.
///
/// Returns `true` when the move was emitted. **Any** destination that names an
/// x-register inside the engine's [`MAX_REGISTER`] bound is inlined, whether or not
/// the register allocator could cache it:
///
/// * a **cached** destination is written in its machine register;
/// * a destination **beyond the cache budget** is written straight into `machine.x`
///   (`mov [rbx + dst*8], …`) — the very slot [`spill`] would have written, so the
///   tier's liveness model is unchanged (cached values live in registers *within* a
///   block and are always back in `machine.x` at a block boundary).
///
/// The source is read from its cached register, from its `machine.x` slot, or from
/// a constant word. A `move` with a Y destination (or any other operand shape), or
/// one naming an x-register no machine can hold, still deopts to the helper — which
/// reports the interpreter's exact result (including its faithful `bad_register`
/// error).
fn try_inline_move(
    buffer: &mut CodeBuffer,
    src: &Operand,
    dst: &Operand,
    allocation: &Allocation,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some(dst_x) = x_register(dst) else {
        return Ok(false);
    };
    if dst_x >= MAX_REGISTER {
        // `machine.x` is at most `MAX_REGISTER` long, so addressing `dst_x` directly
        // would be out of bounds; the helper reports the faithful `bad_register`.
        return Ok(false);
    }
    let source = match x_register(src) {
        Some(src_x) if src_x < MAX_REGISTER => match allocation.slot(src_x) {
            Some(reg) => MoveSource::Cached(reg),
            None => MoveSource::Memory(src_x),
        },
        // A register index outside the engine's bound (same rule as the
        // destination): the interpreter step reports the exact error.
        Some(_) => return Ok(false),
        None => match constant_word(src) {
            Some(word) => MoveSource::Constant(word),
            None => return Ok(false),
        },
    };

    match (allocation.slot(dst_x), source) {
        // Cached destination: write its machine register.
        (Some(dst_reg), MoveSource::Cached(src_reg)) => buffer.mov_rr(dst_reg, src_reg)?,
        (Some(dst_reg), MoveSource::Memory(src_x)) => {
            buffer.mov_rm(dst_reg, Reg::Rbx, x_offset(src_x))?;
            touch(highest_touched, src_x);
        }
        (Some(dst_reg), MoveSource::Constant(word)) => buffer.mov_ri(dst_reg, word)?,
        // Uncached destination: write its `machine.x` slot directly.
        (None, MoveSource::Cached(src_reg)) => {
            buffer.mov_mr(Reg::Rbx, x_offset(dst_x), src_reg)?;
        }
        (None, MoveSource::Memory(src_x)) => {
            buffer.mov_rm(MOVE_SCRATCH, Reg::Rbx, x_offset(src_x))?;
            buffer.mov_mr(Reg::Rbx, x_offset(dst_x), MOVE_SCRATCH)?;
            touch(highest_touched, src_x);
        }
        (None, MoveSource::Constant(word)) => {
            buffer.mov_ri(MOVE_SCRATCH, word)?;
            buffer.mov_mr(Reg::Rbx, x_offset(dst_x), MOVE_SCRATCH)?;
        }
    }
    touch(highest_touched, dst_x);
    Ok(true)
}

/// Emit `swap A B` natively, or report that the block must helper it.
///
/// Returns `true` when the exchange was emitted. Both operands must name
/// x-registers inside the engine's [`MAX_REGISTER`] bound; each is then read and
/// written through its own representation — a cached operand through its machine
/// register, a non-cached one through its `machine.x` slot — so the swap is exact
/// for every combination of the four. A `Y` (or other) operand deopts, and the
/// helper performs the interpreter's exact `write(b, read(a))` / `write(a, read(b))`
/// exchange.
fn try_inline_swap(
    buffer: &mut CodeBuffer,
    a: &Operand,
    b: &Operand,
    allocation: &Allocation,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let (Some(a_x), Some(b_x)) = (x_register(a), x_register(b)) else {
        return Ok(false);
    };
    if a_x >= MAX_REGISTER || b_x >= MAX_REGISTER {
        return Ok(false);
    }
    match (allocation.slot(a_x), allocation.slot(b_x)) {
        (Some(a_reg), Some(b_reg)) => {
            // `xchg` spelled as three moves through the scratch register (the
            // encoder has no `xchg`, and a register pair that overlaps the
            // destination is not expressible with `mov` alone otherwise).
            buffer.mov_rr(MOVE_SCRATCH, a_reg)?;
            buffer.mov_rr(a_reg, b_reg)?;
            buffer.mov_rr(b_reg, MOVE_SCRATCH)?;
        }
        (Some(a_reg), None) => {
            // a is cached, b lives in `machine.x`.
            buffer.mov_rm(MOVE_SCRATCH, Reg::Rbx, x_offset(b_x))?;
            buffer.mov_mr(Reg::Rbx, x_offset(b_x), a_reg)?;
            buffer.mov_rr(a_reg, MOVE_SCRATCH)?;
            touch(highest_touched, b_x);
        }
        (None, Some(b_reg)) => {
            buffer.mov_rm(MOVE_SCRATCH, Reg::Rbx, x_offset(a_x))?;
            buffer.mov_mr(Reg::Rbx, x_offset(a_x), b_reg)?;
            buffer.mov_rr(b_reg, MOVE_SCRATCH)?;
            touch(highest_touched, a_x);
        }
        (None, None) => {
            buffer.mov_rm(MOVE_SCRATCH, Reg::Rbx, x_offset(a_x))?;
            buffer.mov_rm(MOVE_SCRATCH_2, Reg::Rbx, x_offset(b_x))?;
            buffer.mov_mr(Reg::Rbx, x_offset(a_x), MOVE_SCRATCH_2)?;
            buffer.mov_mr(Reg::Rbx, x_offset(b_x), MOVE_SCRATCH)?;
            touch(highest_touched, a_x);
            touch(highest_touched, b_x);
        }
    }
    Ok(true)
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
pub(crate) fn x_register(operand: &Operand) -> Option<u32> {
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
///
/// An `Integer`/`Character` outside the small-int range is `None`: the native tier
/// only inlines immediates, so such a constant makes the block **deopt** to
/// `jit_step_one`, whose interpreter step materialises the exact bignum through
/// [`crate::terms::make_integer`]. `make_small` must never see an out-of-range value
/// (it `debug_assert`s).
pub(crate) fn constant_word(operand: &Operand) -> Option<i64> {
    match operand {
        Operand::Integer(value) => small_word(*value),
        Operand::Character(c) => small_word(i64::from(*c)),
        Operand::Atom(atom) => Some(make_atom(*atom).raw() as i64),
        Operand::Nil => Some(NIL.raw() as i64),
        _ => None,
    }
}

/// The raw `Eterm` word of a small-int constant, or `None` when `value` is outside
/// the 60-bit immediate range (`beam_term::{MIN_SMALL, MAX_SMALL}`).
fn small_word(value: i64) -> Option<i64> {
    if (MIN_SMALL..=MAX_SMALL).contains(&value) {
        Some(make_small(value).raw() as i64)
    } else {
        None
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
pub(crate) enum SmallArith {
    /// `erlang:'+'/2`.
    Add,
    /// `erlang:'-'/2`.
    Sub,
}

/// `TAG_IMMED1_MASK` as an `i64` — the low-nibble mask `is_small` tests with.
pub(crate) const SMALL_MASK: i64 = TAG_IMMED1_MASK as i64;

/// `TAG_IMMED1_SMALL` as an `i64` — the immediate-1 small tag, which is also
/// `make_small`'s addend (`((v << TAG_IMMED1_SIZE) + TAG_IMMED1_SMALL)`).
pub(crate) const SMALL_TAG: i64 = TAG_IMMED1_SMALL as i64;

/// The emitted guard compares the masked word against [`SMALL_MASK`] (one
/// register holds both, per the objective's shape), so the mask and the tag must
/// be the same nibble. `beam_term` defines both as `0b1111`.
const _: () = assert!(SMALL_MASK == SMALL_TAG);

/// The shift `make_small` applies (`TAG_IMMED1_SIZE`).
pub(crate) const SMALL_SHIFT: u32 = TAG_IMMED1_SIZE;

/// The raw word of the smallest small integer (`make_small(MIN_SMALL)`).
///
/// `MIN_SMALL * 16` is exactly `i64::MIN`, so the (saturating, hence exact) word
/// is `i64::MIN + 15`.
pub(crate) const MIN_SMALL_WORD: i64 = MIN_SMALL
    .saturating_mul(1i64 << SMALL_SHIFT)
    .saturating_add(SMALL_TAG);

/// The raw word of the largest small integer (`make_small(MAX_SMALL)`) — which
/// saturates to exactly `i64::MAX`.
pub(crate) const MAX_SMALL_WORD: i64 = MAX_SMALL
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
pub(crate) const FAST_A: Reg = Reg::Rax;
/// The second operand's scratch register.
pub(crate) const FAST_B: Reg = Reg::R8;
/// The tag/range temporary.
pub(crate) const FAST_T: Reg = Reg::R10;
/// The tag mask / arithmetic constant.
pub(crate) const FAST_C: Reg = Reg::Rcx;
/// The computed result word.
pub(crate) const FAST_R: Reg = Reg::R11;

/// A `gc_bif` shape the fast path can lower.
pub(crate) struct FastArith<'a> {
    /// Which of the two BIFs is called.
    pub(crate) op: SmallArith,
    /// The first operand (a cacheable `x(i)` or a small-int constant).
    pub(crate) a: &'a Operand,
    /// The second operand.
    pub(crate) b: &'a Operand,
    /// The x-register index the result is written to.
    pub(crate) dst_x: u32,
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
pub(crate) fn fast_arith<'a>(module: &Module, instr: &'a Instr) -> Option<FastArith<'a>> {
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
    /// A non-cached x-register, read straight from its `machine.x` slot.
    Memory(u32),
    /// A small-int constant, loaded as a raw `Eterm` word.
    Constant(i64),
}

/// The [`FastSource`] for `operand`, or `None` when it is neither an addressable
/// x-register nor a small constant.
///
/// A non-cached x-register is a `machine.x` read, not a deopt: the register
/// allocator's four-register budget decides only *where* the value is read from,
/// never whether the instruction can be lowered.
fn fast_source(operand: &Operand, allocation: &Allocation) -> Option<FastSource> {
    match x_register(operand) {
        Some(x) if x < MAX_REGISTER => match allocation.slot(x) {
            Some(reg) => Some(FastSource::Cached(reg)),
            None => Some(FastSource::Memory(x)),
        },
        Some(_) => None,
        None => small_constant(operand).map(FastSource::Constant),
    }
}

/// Load `source` into scratch register `reg`, recording a `machine.x` read.
fn load_into(
    buffer: &mut CodeBuffer,
    source: FastSource,
    reg: Reg,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    match source {
        FastSource::Cached(src) => buffer.mov_rr(reg, src),
        FastSource::Memory(x) => {
            touch(highest_touched, x);
            buffer.mov_rm(reg, Reg::Rbx, x_offset(x))
        }
        FastSource::Constant(word) => buffer.mov_ri(reg, word),
    }
}

/// Lower a fast-path-eligible `gc_bif` natively, or report that the block must
/// helper it.
///
/// Returns `true` when the whole guarded sequence was emitted, leaving the
/// result in the destination (its cached machine register, or its `machine.x`
/// slot when it is beyond the cache budget) and letting the block continue with
/// the next instruction. Returns `false` **without having emitted a byte** when
/// the shape matched ([`fast_arith`]) but an operand names a register outside the
/// engine's bound or the destination is not an addressable x-register — the caller
/// then emits exactly today's `jit_step_one` fallback.
///
/// The emitted shape (see the section comment above for why it is exact):
///
/// ```text
///   mov  rax, a          ; the operand: a cached register, `[rbx + x*8]`, or a
///                        ; movabs of a small constant
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
///   mov  <dst>, r11         ; cached destination: its machine register;
///                           ; otherwise `mov [rbx + dst*8], r11`
///   jmp  fast_done
/// fallback:
///   <spill cached x-registers; call jit_step_one; jmp the block epilogue>
/// fast_done:
///   <emit the rest of the block>
/// ```
///
/// The fast path never spills: the destination's cached register is written
/// back by the next helper call or by the block's own exit spill, and a
/// non-cached destination is written to `machine.x` — the exact slot the spill
/// would write — so the spill-across-blocks discipline is unchanged.
#[allow(clippy::too_many_arguments)]
fn try_inline_gc_bif(
    buffer: &mut CodeBuffer,
    module: &Module,
    instr: &Instr,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
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

    let fallback: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();

    // Load both operands into scratch registers.
    load_into(buffer, a_source, FAST_A, highest_touched)?;
    load_into(buffer, b_source, FAST_B, highest_touched)?;

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

    // Success: store the result — in the destination's cached register, or
    // straight into its `machine.x` slot when the budget could not cache it —
    // and skip the fallback.
    match allocation.slot(fast.dst_x) {
        Some(dst_reg) => buffer.mov_rr(dst_reg, FAST_R)?,
        None => {
            buffer.mov_mr(Reg::Rbx, x_offset(fast.dst_x), FAST_R)?;
            touch(highest_touched, fast.dst_x);
        }
    }
    buffer.jmp(fast_done)?;

    // Rejected (or not lowered): one interpreter step, then leave the block.
    buffer.bind(fallback)?;
    emit_helper(buffer, done, allocation, func, pc)?;
    buffer.bind(fast_done)?;
    Ok(true)
}
