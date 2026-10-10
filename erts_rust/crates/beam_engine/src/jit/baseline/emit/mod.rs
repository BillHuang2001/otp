// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! The baseline JIT's emitter: typed IR → x86-64 basic blocks.
//!
//! The register allocator ([`allocate_registers`]) and the per-instruction
//! lowering ([`emit_block`]) — the inline `move`/`swap`/`jump`/metadata set, the
//! word-level `test` and exact-value `select_val` subsets, the
//! deopt-and-continue sequence ([`emit_step_and_continue`]) that covers every
//! instruction the block does not lower natively, the spill/fill discipline
//! ([`CACHE_REGISTERS`]), the tier's one arithmetic optimisation — the guarded
//! small-int `erlang:'+'/2`/`'-'/2`/`'*'/2` fast path ([`try_inline_gc_bif`]) — and the
//! guarded aggregate readers ([`aggregate`]: the cons-cell readers
//! `get_list`/`get_hd`/`get_tl` and the tuple readers `get_tuple_element` /
//! `select_tuple_arity`).
//! [`compile_module`] lowers every basic block of a module into the
//! [`CompiledModule`] artifact. See `super` for the block ABI (in `runtime`) and the
//! driver that runs the emitted code.
//!
//! **Only [`CACHE_REGISTERS`]`.len()` x-registers are held in machine registers**;
//! every other x-register a block names is addressed **directly in `machine.x`**
//! (`[rbx + x*8]`, `rbx` = the array base the prologue loads from
//! [`RUNTIME_XREGS_DISP`]) — the same slot the exit spill writes — so a block that
//! touches more registers than the cache budget still runs natively instead of
//! deopting. The residents are the block's *most-used* registers, chosen by
//! [`allocate_registers`] from the native touch counts of the block's inlinable
//! instructions, so the registers with the longest live ranges are the ones kept in
//! machine registers; the direct `machine.x` accesses are the fill/spill for the
//! rest. That is safe precisely because the cache membership is fixed for the
//! whole block ([`Allocation`]): an x-register is either *always* held in its
//! machine register (and written back at every block exit) or *never* cached, so
//! the two representations can never disagree. The highest x-register index a block
//! touches natively — cached or direct — is recorded on the [`CompiledModule`] so
//! [`run_compiled`](super::run_compiled) can refuse native entry to a machine whose
//! register file does not cover it.
//!
//! **Direct block-to-block linking.** A taken branch to a successor basic block
//! that this compilation emitted does not return its `(func, pc)` word to the
//! driver: [`Linker::emit_exit`] tears down the current block's frame (spilling the
//! cached x-registers first — the tier's whole liveness/root-set model) and jumps
//! **straight to the successor block's native entry**, whose own prologue rebuilds
//! its frame. `compile_module` reserves one entry label per block *before* emitting
//! any code, so a forward branch is an ordinary `rel32` fixup resolved at `bind`
//! time — never a post-`seal` patch, so the W^X policy is unchanged. Every
//! continuation that is **not** a compiled block's entry (an address inside a
//! block, an empty block, an unresolved label, a function's end) keeps the
//! documented deopt: spill, materialise the `encode_target` word in `rax`, and jump
//! to the block epilogue for the driver to decode.
//!
//! The **local tail calls** are linked the same way. `call_only` is a plain jump
//! (`$DISPATCH_REL(Dest)`), so it lowers exactly like `jump`; `call_last` additionally
//! pops the current frame *before* the transfer, which the tier does through the
//! `jit_tail_call` helper — the interpreter's own `Machine::pop_frame_checked` — with
//! the module-local target already resolved at compile time, i.e. in the interpreter's
//! `resolve`-then-`pop`-then-transfer order ([`Linker::emit_tail_exit`]). A tail-call
//! target that is not a compiled block's entry, and every `call_ext_*` (which enters
//! code outside this module's blocks), keep the documented deopt.
//!
//! The emitter is split by responsibility across this directory: this file owns the
//! compiled-block shape (the register-file constants, the allocation artifact,
//! [`emit_block`], the deopt-and-continue sequence and [`compile_module`]);
//! [`regalloc`] the register allocator, [`operands`] the operand/constant readers,
//! [`r#move`] the inline `move`/`swap`, [`test`] the word-level `test` subset,
//! [`select`] the exact-value switch, [`arith`] the guarded small-int fast path,
//! [`link`] the direct block-to-block linker and [`aggregate`] the guarded cons-cell
//! and tuple readers.
#![allow(unsafe_code)]

use std::collections::BTreeMap;

use beam_common::Result;
use beam_loader::{ImportRef, Instr, Module};

use crate::interpreter::{self, LabelMap};
use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg, DEFAULT_CAPACITY};

use super::runtime::{
    encode_target, ExtCallHelper, PositionHelper, StepHelper, EXT_CALL, EXT_CALL_LAST,
    EXT_CALL_ONLY, RUNTIME_XREGS_DISP,
};
use super::{jit_ext_call, jit_set_position, jit_step_one, CompiledModule};

mod aggregate;
mod arith;
mod compare;
mod link;
mod r#move;
mod operands;
mod regalloc;
mod select;
mod test;

use aggregate::{
    try_inline_get_hd, try_inline_get_list, try_inline_get_tl, try_inline_get_tuple_element,
    try_inline_select_tuple_arity,
};
use arith::try_inline_gc_bif;
use compare::try_inline_comparison;
use link::{emit_frame_teardown, Linker};
use operands::x_offset;
use r#move::{try_inline_move, try_inline_swap};
use select::try_inline_select_val;
use test::try_inline_test;

// The names this module's own unit tests (`baseline/tests.rs` glob-imports `emit`)
// reach as `emit::NAME`, re-exported here so that splitting the emitter into
// submodules changed no item path. Only the tests use them, hence `cfg(test)`.
#[cfg(test)]
pub(crate) use arith::{
    fast_arith, SmallArith, FAST_A, FAST_B, FAST_C, FAST_R, FAST_T, MAX_SMALL_WORD, MIN_SMALL_WORD,
    SMALL_MASK, SMALL_SHIFT, SMALL_TAG,
};
#[cfg(test)]
pub(crate) use operands::{constant_word, x_register};
pub(crate) use regalloc::allocate_registers;

// ---------------------------------------------------------------------------
// Compilation
// ---------------------------------------------------------------------------

/// The callee-saved machine registers a block preserves across its prologue and
/// epilogue (the Rust ABI requires them to survive the call).
///
/// Six registers keep `rsp` 16-byte aligned at the helper call site: the entry
/// `rsp ≡ 8 (mod 16)` (`call` pushed the return address), six 8-byte pushes (48
/// bytes, a multiple of 16) leave it at `8 (mod 16)`, and the 24-byte `rt` slot the
/// prologue reserves by `lea` — a further `24 ≡ 8 (mod 16)` bytes — brings it to
/// `0 (mod 16)`. (`rbp` is released from its conventional frame-pointer role to
/// enlarge the cache below; the emitted addressing is uniformly `mod=10` `disp32`,
/// so `rbp` is usable as a memory base with no RIP-relative special case — see
/// [`crate::jit::emitter`].)
pub(crate) const SAVED_REGISTERS: [Reg; 6] =
    [Reg::Rbx, Reg::Rbp, Reg::R12, Reg::R13, Reg::R14, Reg::R15];

/// The machine registers a block caches x-registers in — `rbx` is reserved for the
/// machine base pointer, so five x-registers fit (`rbp` included).
///
/// The assignment order is historical: the four established slots keep their
/// `r12`–`r15` registers and the released `rbp` is appended as the fifth, so a
/// register that was cacheable before the pool was widened stays in the same
/// physical register and the slot↔register mapping is stable across the change.
pub(crate) const CACHE_REGISTERS: [Reg; 5] =
    [Reg::R12, Reg::R13, Reg::R14, Reg::R15, Reg::Rbp];

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
pub(super) fn touch(highest: &mut Option<u32>, x: u32) {
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
    // A flat `[func][pc]` table, not a `HashMap`: the driver's per-block lookup sits on
    // the hot path of every cross-block jump — a `fib`-style loop re-enters the driver
    // once per iteration — so SipHash on a `(u32, u32)` key is a measurable share of the
    // run. Two indexed `Vec` loads replace it.
    let mut entries: Vec<Vec<Option<*const u8>>> = module
        .functions
        .iter()
        .map(|function| vec![None; function.code.len()])
        .collect();
    let mut block_count = 0usize;
    let mut cached_registers: Vec<u32> = Vec::new();
    let mut highest_cached_register: Option<u32> = None;
    let mut highest_native_register: Option<u32> = None;

    // Reserve one entry label per non-empty compiled block **before** emitting any
    // code, so a branch to a successor block — usually a forward reference, e.g. a
    // `jump` to a block emitted later or a `fail` label — is resolved by the
    // buffer's ordinary `rel32` fixup machinery at `bind` time, before `seal` flips
    // the mapping to read+execute. Each label is bound at its block's entry offset
    // below, exactly where the driver's `entries` slot points.
    let mut block_labels: BTreeMap<(u32, u32), CodeLabel> = BTreeMap::new();
    for (func_index, function) in module.functions.iter().enumerate() {
        let code = &function.code;
        for block in &function.blocks {
            let start = block.start;
            let end = block.end.min(code.len());
            if start >= end {
                continue;
            }
            block_labels.insert((func_index as u32, start as u32), buffer.label());
        }
    }

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
                if !cached_registers.contains(x) {
                    cached_registers.push(*x);
                }
            }

            // Bind this block's reserved entry label to the offset it is about to be
            // emitted at (the slot `entries[func_index][start]` will point at). The
            // key always exists: it was reserved for exactly these non-empty blocks.
            buffer.bind(block_labels[&(func, start as u32)])?;
            let offset = buffer.len();
            emit_block(
                &mut buffer,
                module,
                &labels,
                &block_labels,
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
            // `start < end <= code.len()`, so the slot always exists.
            entries[func_index][start] = Some(entry);
            block_count += 1;
        }
    }

    buffer.seal()?;
    cached_registers.sort_unstable();
    Ok(CompiledModule {
        buffer,
        entries,
        block_count,
        cached_registers,
        highest_cached_register,
        highest_native_register,
    })
}

/// A generous byte-count estimate for the code `module` lowers to.
///
/// A block costs a prologue/epilogue pair plus its inlined
/// instructions; the prologue carries the six callee-saved pushes, the `rt`-slot
/// setup, the position-store helper call ([`emit_position_store`] — the same
/// three-argument indirect-call sequence as one deopt, ~40 bytes) and the cached
/// register loads, so [`BLOCK_BYTES`] covers it. A `move`/`swap` whose operands live
/// in `machine.x` is at most two
/// loads plus two stores (~28 bytes), so [`INSTRUCTION_BYTES`] is budgeted per
/// instruction. A `gc_bif` costs either the helper call sequence (~50 bytes) or,
/// when the small-int fast path lowers it, the whole guarded sequence plus its
/// helper fallback ([`FAST_PATH_BYTES`]); a `get_list`/`get_hd`/`get_tl` likewise
/// costs its guarded sequence plus the helper fallback it may still branch to
/// ([`LIST_PATH_BYTES`]), and a `get_tuple_element`/`select_tuple_arity` its own
/// guarded sequence plus one spill-and-return stub per arity alternative
/// ([`TUPLE_PATH_BYTES`]).
/// Under-estimating would turn a large module into a `SystemLimit` compile error,
/// so the estimate is deliberately loose — [`FAST_PATH_BYTES`] is budgeted for
/// *every* `gc_bif` and [`LIST_PATH_BYTES`] for every cons-cell read, whether or
/// not the fast path claims it, and [`CodeBuffer::with_capacity`] rounds the total
/// up to whole pages.
fn code_capacity_estimate(module: &Module) -> usize {
    let mut instructions = 0usize;
    let mut blocks = 0usize;
    let mut gc_bifs = 0usize;
    let mut cmp_bifs = 0usize;
    let mut list_reads = 0usize;
    let mut tuple_paths = 0usize;
    for function in &module.functions {
        instructions += function.code.len();
        blocks += function.blocks.len();
        for instr in &function.code {
            match instr {
                Instr::Bif { .. } => cmp_bifs += 1,
                Instr::GcBif { .. } => {
                    gc_bifs += 1;
                    cmp_bifs += 1;
                }
                Instr::GetList { .. } | Instr::GetHd { .. } | Instr::GetTl { .. } => {
                    list_reads += 1
                }
                Instr::GetTupleElement { .. } => tuple_paths += 1,
                Instr::SelectTupleArity { destinations, .. } => {
                    tuple_paths += 1 + destinations.len()
                }
                _ => {}
            }
        }
    }
    BLOCK_BYTES * blocks
        + INSTRUCTION_BYTES * instructions
        + FAST_PATH_BYTES * gc_bifs
        + COMPARE_PATH_BYTES * cmp_bifs
        + LIST_PATH_BYTES * list_reads
        + TUPLE_PATH_BYTES * tuple_paths
        + DEFAULT_CAPACITY
}

/// The code-size budget reserved for one compiled block (see
/// [`code_capacity_estimate`]): the prologue (six callee-saved pushes, the 24-byte
/// `rt`-slot reservation, the `rbx` load, the [`emit_position_store`] helper call)
/// and the epilogue (the frame teardown and `ret`) plus their padding.
const BLOCK_BYTES: usize = 192;

/// The code-size budget reserved for one instruction (see
/// [`code_capacity_estimate`]).
///
/// Every instruction the block does not lower natively is emitted as the full
/// [`emit_step_and_continue`] sequence — the exit spill, the three helper arguments,
/// the indirect call, the continuation check and the reload ([`Fill`]) — which is
/// the largest per-instruction lowering the emitter produces (~150 bytes with five
/// cached registers). Budgeting for the deopt form for *every* instruction also
/// covers the inlined forms, which are smaller.
const INSTRUCTION_BYTES: usize = 192;

/// The code-size budget reserved for one `gc_bif` (see
/// [`code_capacity_estimate`]): the guarded small-int fast path (~140 bytes with
/// `machine.x` operand loads and a `machine.x` result store, ~150 for the
/// `erlang:'*'/2` sequence, which adds the untag/re-tag shifts and a second
/// constant compare) plus the [`emit_step_and_continue`] fallback it branches to
/// (~150 bytes).
const FAST_PATH_BYTES: usize = 384;

/// The code-size budget reserved for one comparison BIF (see
/// [`code_capacity_estimate`]): the two operand loads (~20 bytes), the two small-int
/// guards with their mask materialisation (~40), the compare/branch (~10), both
/// `true`/`false` materialisations with their skip jumps (~45) and the
/// [`emit_step_and_continue`] fallback it branches to (~150, also covered by the
/// per-instruction [`INSTRUCTION_BYTES`]). Deliberately loose: under-estimating
/// would turn a large module into a `SystemLimit` compile error.
const COMPARE_PATH_BYTES: usize = 256;

/// The code-size budget reserved for one `get_list`/`get_hd`/`get_tl` (see
/// [`code_capacity_estimate`]): the guarded cons-cell read — a mask/test/branch, the
/// pointer computation and the CAR/CDR loads with their stores to cached or
/// `machine.x` destinations (~90 bytes) — emitted *in front of* the
/// [`emit_step_and_continue`] fallback it may still branch to, which the
/// per-instruction [`INSTRUCTION_BYTES`] already budgets.
const LIST_PATH_BYTES: usize = 128;

/// The code-size budget reserved for one `get_tuple_element` — and one
/// `select_tuple_arity` *alternative* (see [`code_capacity_estimate`]): the guarded
/// header read (the boxed test, the pointer computation, the arityval test and the
/// range compare, ~70 bytes) plus — for the arity switch — one `mov_ri`/`cmp`/`jcc`
/// and its spill-and-return stub (~45 bytes each). It is emitted *in front of* the
/// [`emit_step_and_continue`] fallback the guard may still branch to, which the
/// per-instruction [`INSTRUCTION_BYTES`] already budgets.
const TUPLE_PATH_BYTES: usize = 160;

/// Emit one native function for `code[start..end]` (a basic block of `func`).
///
/// `highest_touched` accumulates the highest x-register index the emitted code
/// touches natively (a cached load/store, or a direct `machine.x` access); the
/// driver refuses native entry to a machine whose register file does not cover it.
#[allow(clippy::too_many_arguments)]
fn emit_block(
    buffer: &mut CodeBuffer,
    module: &Module,
    labels: &LabelMap,
    block_labels: &BTreeMap<(u32, u32), CodeLabel>,
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
    // Reserve the `rt` slot. `lea rsp, [rsp - 24]` is `sub rsp, 24` and keeps `rsp`
    // 16-byte aligned at the helper call site (see `SAVED_REGISTERS`).
    buffer.lea(Reg::Rsp, Reg::Rsp, -24)?;
    buffer.mov_mr(Reg::Rsp, RUNTIME_SLOT_DISP, Reg::Rdi)?;
    // rbx = rt->x_regs, the base of `machine.x` the cached slots are indexed from.
    // Loaded **before** the position store below: `rbx` is callee-saved, so the helper
    // call preserves it, and the cached x-registers are loaded only *after* the call,
    // so nothing a block keeps live across the call sits in caller-saved scratch.
    buffer.mov_rm(Reg::Rbx, Reg::Rdi, RUNTIME_XREGS_DISP)?;
    // Record this block's own `(func, pc)` as the machine's execution position, so
    // `Machine::current_code` (and the `process_info/2` position items built from it)
    // advances at block granularity even across a **linked** branch, which never
    // returns to the driver. A parked `receive` and a raise stay exact because both go
    // through the interpreted deopt (`jit_step_one`), which records the *instruction*
    // address itself. See `jit_set_position`.
    emit_position_store(buffer, func, start as u32)?;
    for (x, reg) in &allocation.slots {
        touch(highest_touched, *x);
        buffer.mov_rm(*reg, Reg::Rbx, x_offset(*x))?;
    }

    let done: CodeLabel = buffer.label();
    // The direct block-to-block linker: a taken branch to a compiled successor's
    // entry leaves this block's frame and jumps straight there (see [`link`]).
    let linker = Linker::new(block_labels);

    // --- body ---------------------------------------------------------------
    //
    // Every instruction of the block is covered: either it is lowered natively, or
    // it is executed by one `jit_step_one` call after which the block **continues**
    // natively at `pc + 1` whenever the helper advanced by exactly one instruction
    // (`Step::Next`) — see [`emit_step_and_continue`]. Only a resolved `Jump`
    // terminates the block early; a helper that produced any other outcome
    // (`Goto`/`Halt`/`Wait`/`Err`) returns to the driver with that word.
    let mut terminated = false;
    for (i, instr) in code[start..end].iter().enumerate() {
        let pc = (start + i) as u32;
        match instr {
            // Metadata / no-ops (`ops.tab:103-108`): they never change control flow.
            // `func_info` is **not** here: it is the failed-clause raise (see the
            // interpreter's `Instr::FuncInfo` arm), so it must deopt through
            // `jit_step_one` rather than be inlined as a no-op (which would fall
            // through to the entry label and re-run the clause tests forever).
            Instr::Label(_)
            | Instr::Line(_)
            | Instr::IntCodeEnd
            | Instr::ExecutableLine { .. }
            | Instr::DebugLine { .. } => {}
            Instr::Move { src, dst } => {
                if !try_inline_move(buffer, src, dst, allocation, highest_touched)? {
                    emit_step_and_continue(buffer, done, allocation, func, pc)?;
                }
            }
            // `swap A B` exchanges two x-registers; a block normally caches fewer
            // registers than the instruction touches, so this is a common deopt it
            // is cheap to remove (a three-`mov` shuffle, register or memory).
            Instr::Swap { a, b } => {
                if !try_inline_swap(buffer, a, b, allocation, highest_touched)? {
                    emit_step_and_continue(buffer, done, allocation, func, pc)?;
                }
            }
            // The guarded cons-cell readers: `get_list`/`get_hd`/`get_tl` split a
            // real cons cell natively (see [`aggregate`]). Any other shape — a `Y`
            // operand, a register past `MAX_REGISTER` — is the `_` arm, and so is a
            // **non-cons source**: the guard branches to the helper, whose
            // interpreter step raises the exact `{badmatch, Source}`.
            Instr::GetList { source, head, tail } => {
                if !try_inline_get_list(
                    buffer,
                    source,
                    head,
                    tail,
                    allocation,
                    done,
                    func,
                    pc,
                    highest_touched,
                )? {
                    emit_step_and_continue(buffer, done, allocation, func, pc)?;
                }
            }
            Instr::GetHd { source, head } => {
                if !try_inline_get_hd(
                    buffer,
                    source,
                    head,
                    allocation,
                    done,
                    func,
                    pc,
                    highest_touched,
                )? {
                    emit_step_and_continue(buffer, done, allocation, func, pc)?;
                }
            }
            Instr::GetTl { source, tail } => {
                if !try_inline_get_tl(
                    buffer,
                    source,
                    tail,
                    allocation,
                    done,
                    func,
                    pc,
                    highest_touched,
                )? {
                    emit_step_and_continue(buffer, done, allocation, func, pc)?;
                }
            }
            // The guarded tuple readers: `get_tuple_element` reads one element of a
            // real tuple natively and `select_tuple_arity` switches on its header
            // word (see [`aggregate`]). Any shape that is not two (resp. one)
            // in-bound x-registers is the `_` arm, and so is a **non-tuple source**:
            // the guard branches to the helper, whose interpreter step raises the
            // exact `{badmatch, Source}` — or reports the typed `unsupported` the
            // arity switch uses for a non-tuple.
            Instr::GetTupleElement {
                source,
                element,
                dst,
            } => {
                if !try_inline_get_tuple_element(
                    buffer,
                    source,
                    *element,
                    dst,
                    allocation,
                    done,
                    func,
                    pc,
                    highest_touched,
                )? {
                    emit_step_and_continue(buffer, done, allocation, func, pc)?;
                }
            }
            // Like the `select_val` arm below, this instruction **always** transfers
            // control (the interpreter returns a `Step::Goto` for every path), so the
            // block ends here when the switch is inlined.
            Instr::SelectTupleArity {
                tuple,
                fail,
                destinations,
            } => {
                if try_inline_select_tuple_arity(
                    buffer,
                    labels,
                    &linker,
                    tuple,
                    *fail,
                    destinations,
                    allocation,
                    done,
                    func,
                    pc,
                    highest_touched,
                )? {
                    terminated = true;
                    break;
                }
                emit_step_and_continue(buffer, done, allocation, func, pc)?;
            }
            // The one optimised BIF family: the small-int `erlang:'+'/2`/`'-'/2`/`'*'/2`.
            // When the guarded fast path is emitted the block keeps going; otherwise
            // (any other BIF, an un-addressable operand, or a register the budget
            // could not cache) it is exactly the `_` arm below.
            Instr::GcBif { .. } => {
                let inlined = try_inline_gc_bif(
                    buffer,
                    module,
                    instr,
                    allocation,
                    done,
                    func,
                    pc,
                    highest_touched,
                )? || try_inline_comparison(
                    buffer,
                    module,
                    instr,
                    allocation,
                    done,
                    func,
                    pc,
                    highest_touched,
                )?;
                if !inlined {
                    emit_step_and_continue(buffer, done, allocation, func, pc)?;
                }
            }
            // The comparison BIFs (`erlang`'s `=:=`/`=/=`/`==`/`/=`/`<`/`=<`/`>`/`>=`),
            // which the compiler emits as a **non-collecting** `bif` (see [`compare`]).
            // Any other BIF, an un-addressable operand, or a module without both
            // `true`/`false` interned is the `_` arm below.
            Instr::Bif { .. } => {
                if !try_inline_comparison(
                    buffer,
                    module,
                    instr,
                    allocation,
                    done,
                    func,
                    pc,
                    highest_touched,
                )? {
                    emit_step_and_continue(buffer, done, allocation, func, pc)?;
                }
            }
            Instr::Jump { target } => match labels.resolve(*target) {
                Ok((target_func, target_pc)) => {
                    // A compiled successor is entered **directly** (the linker tears
                    // this block's frame down and jumps to its entry); anything else
                    // returns the `(func, pc)` word to the driver exactly as before.
                    linker.emit_exit(buffer, allocation, done, target_func, target_pc)?;
                    terminated = true;
                    break;
                }
                // An undefined label keeps the interpreter's exact `unknown_label`
                // error: hand the jump to the helper rather than inventing one.
                Err(_) => emit_step_and_continue(buffer, done, allocation, func, pc)?,
            },
            // A **local tail call** (`call_last Arity Label Deallocate`) transfers
            // control to another block — usually of another function — after popping
            // the current frame. The label is module-local, so it is resolved here, at
            // compile time; when the successor is a compiled block's entry the frame is
            // popped by the interpreter's own checked pop (`jit_tail_call`, in the
            // interpreter's resolve-then-pop order) and control jumps straight to the
            // successor's native entry (see [`link`]). A label the module does not
            // define, or a successor this compilation emitted no block for, keeps the
            // documented deopt: the helper runs the interpreter's own `call_last` and so
            // reports its exact `unknown_label`/frame error. Like `jump`, the
            // instruction always transfers control, so the block ends here either way.
            Instr::CallLast {
                target, deallocate, ..
            } => match labels.resolve(*target) {
                Ok((target_func, target_pc)) if linker.is_compiled(target_func, target_pc) => {
                    linker.emit_tail_exit(
                        buffer,
                        allocation,
                        done,
                        target_func,
                        target_pc,
                        *deallocate,
                    )?;
                    terminated = true;
                    break;
                }
                _ => emit_step_and_continue(buffer, done, allocation, func, pc)?,
            },
            // A local tail call **without** a frame to pop (`call_only Arity Label`):
            // `i_call_only(CallDest)` is `$DISPATCH_REL(Dest)`, so it lowers exactly
            // like the `jump` arm above — resolve at compile time, then link or deopt.
            Instr::CallOnly { target, .. } => match labels.resolve(*target) {
                Ok((target_func, target_pc)) => {
                    linker.emit_exit(buffer, allocation, done, target_func, target_pc)?;
                    terminated = true;
                    break;
                }
                Err(_) => emit_step_and_continue(buffer, done, allocation, func, pc)?,
            },
            // The **cross-module** calls. Unlike a local `call_*` these enter code
            // outside this module's compiled blocks, so the transfer is always a
            // driver round-trip — but the *call itself* no longer needs an
            // interpreter step: [`emit_ext_call`] performs the import resolution and
            // the variant's frame discipline in the `jit_ext_call` helper, which
            // hands the resolved callee back as the `ENTER` word. Everything the
            // host cannot run in this machine (the synchronous `call_bif` path and
            // every one of its errors) is exactly the interpreter's own step, taken
            // by the helper's fallback.
            Instr::CallExt { target, .. } => {
                emit_ext_call(
                    buffer, allocation, done, func, pc, *target, EXT_CALL, 0,
                )?;
            }
            Instr::CallExtOnly { target, .. } => {
                emit_ext_call(
                    buffer,
                    allocation,
                    done,
                    func,
                    pc,
                    *target,
                    EXT_CALL_ONLY,
                    0,
                )?;
            }
            Instr::CallExtLast {
                target,
                deallocate,
                ..
            } => {
                emit_ext_call(
                    buffer,
                    allocation,
                    done,
                    func,
                    pc,
                    *target,
                    EXT_CALL_LAST,
                    *deallocate,
                )?;
            }
            // The word-level `test` subset (see the section below): a native
            // predicate plus a branch to the resolved fail label. Any other shape —
            // a `NO_FAIL` test, an unresolved fail label, a boxed-term type test, an
            // inexact comparison — is the `_` arm.
            Instr::Test { .. } => {
                if !try_inline_test(
                    buffer,
                    module,
                    labels,
                    &linker,
                    instr,
                    allocation,
                    done,
                    func,
                    pc,
                    highest_touched,
                )? {
                    emit_step_and_continue(buffer, done, allocation, func, pc)?;
                }
            }
            // The exact-value switch subset (see the section below): a chain of
            // word compares against the immediate alternatives, each branching to its
            // resolved target. Any other shape — a `NO_FAIL` switch, a non-immediate
            // case value (a float/bignum/literal), an unresolved label, a `Y`/float/
            // literal scrutinee — is the `_` arm.
            //
            // Unlike the `test` arm this instruction **always** transfers control
            // (the interpreter returns a `Step::Goto` for every path), so the block
            // ends here.
            Instr::SelectVal { .. } => {
                if try_inline_select_val(
                    buffer,
                    labels,
                    &linker,
                    instr,
                    allocation,
                    done,
                    highest_touched,
                )? {
                    terminated = true;
                    break;
                }
                emit_step_and_continue(buffer, done, allocation, func, pc)?;
            }
            // Everything else: one interpreter step, then continue at `pc + 1`.
            _ => emit_step_and_continue(buffer, done, allocation, func, pc)?,
        }
    }

    if !terminated {
        // The block ran off its end: hand the next address back to the driver — or,
        // when the following block starts exactly there, jump straight into it.
        linker.emit_exit(buffer, allocation, done, func, end as u32)?;
    }

    // --- epilogue -----------------------------------------------------------
    //
    // The shared exit every deopt path jumps to (a helper that produced a
    // non-`Next` outcome, a returned `fail`/case target word). Those paths have
    // already spilled the cached x-registers, so the epilogue only leaves the
    // frame — the exact inverse of the prologue.
    buffer.bind(done)?;
    emit_frame_teardown(buffer)?;
    buffer.ret()
}

/// Emit the prologue's position store: call `jit_set_position(rt, func, pc)` so the
/// machine records this block's entry `(func, pc)` as its current execution position.
///
/// `rt` is read out of the prologue-reserved stack slot ([`RUNTIME_SLOT_DISP`]), so
/// this may be emitted anywhere after that slot is written. The helper clobbers only
/// caller-saved registers, so a block must have loaded nothing it needs into them at
/// the call; every cached x-register and `rbx` live in callee-saved registers, which
/// the helper preserves ([`emit_block`] emits this before loading the cached
/// x-registers for exactly that reason). `rsp` is already 16-byte aligned at the call
/// (see [`SAVED_REGISTERS`]). The helper returns nothing — a position store cannot
/// fail — so `rax` is not consulted.
fn emit_position_store(buffer: &mut CodeBuffer, func: u32, pc: u32) -> Result<()> {
    buffer.mov_rm(Reg::Rdi, Reg::Rsp, RUNTIME_SLOT_DISP)?;
    buffer.mov_ri(Reg::Rsi, i64::from(func))?;
    buffer.mov_ri(Reg::Rdx, i64::from(pc))?;
    let helper: PositionHelper = jit_set_position;
    buffer.mov_ri(Reg::R11, helper as *const () as i64)?;
    buffer.call_r(Reg::R11)
}

/// Emit the spill-then-helper sequence for `helper` — **without** the block exit, so
/// the caller decides what to do with the helper's word.
///
/// The spill comes **first**: every helper reads `machine.x` (the interpreter itself
/// for [`jit_step_one`], the frame discipline of `call_ext_last` for
/// [`jit_ext_call`]), which must already hold the values the inline prefix produced.
/// The `rt` slot is reloaded into `rdi` (every helper's first argument) and `args`
/// fills the remaining integer argument registers in ABI order (`rsi`, `rdx`, `rcx`,
/// `r8`, `r9`); a helper preserves the callee-saved registers, so `rbx` (the
/// `machine.x` base) survives the call — but the block must **not** spill after it:
/// the helper legitimately wrote `machine.x`.
///
/// The helper's result word is left in `rax` (the block ABI's return register) for
/// the caller's continuation check. Passing the helper as a raw pointer keeps the
/// call sites' own function-pointer types (`StepHelper`, `ExtCallHelper`) as the
/// type check on its signature.
fn emit_helper_call(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    helper: *const (),
    args: &[i64],
) -> Result<()> {
    spill(buffer, allocation)?;
    buffer.mov_rm(Reg::Rdi, Reg::Rsp, RUNTIME_SLOT_DISP)?;
    // The `rsi`/`rdx`/`rcx`/`r8`/`r9` argument registers, in ABI order.
    const ARG_REGISTERS: [Reg; 5] = [Reg::Rsi, Reg::Rdx, Reg::Rcx, Reg::R8, Reg::R9];
    for (reg, value) in ARG_REGISTERS.iter().zip(args) {
        buffer.mov_ri(*reg, *value)?;
    }
    buffer.mov_ri(Reg::R11, helper as i64)?;
    buffer.call_r(Reg::R11)
}

/// Emit "execute instruction `(func, pc)` with one interpreter step, then **continue
/// natively** when it advanced by exactly one instruction".
///
/// This is the tier's per-instruction deopt. `jit_step_one` maps its outcome onto
/// the block ABI:
///
/// * `Step::Next` → `encode_target(func, pc + 1)` — the helper executed exactly the
///   instruction at `pc` and the block can carry on at `pc + 1`, so the emitted code
///   refills the cached x-registers (mandatory: the helper *is* the interpreter and
///   wrote `machine.x`) and **falls through** to the next instruction's lowering;
/// * any other word (`Goto`/`HALT`/`ERROR`/`ENTER`, and the `Wait` → `blocked`
///   error) → jump to the block epilogue with that word in `rax`, exactly as before.
///
/// Continuing in-block rather than leaving the block is what makes native coverage
/// independent of where a block's first non-inlinable instruction sits: the
/// surrounding instructions still run natively, and the driver's per-instruction
/// entry lookup is paid once per block instead of once per instruction.
pub(super) fn emit_step_and_continue(
    buffer: &mut CodeBuffer,
    done: CodeLabel,
    allocation: &Allocation,
    func: u32,
    pc: u32,
) -> Result<()> {
    let helper: StepHelper = jit_step_one;
    emit_helper_call(
        buffer,
        allocation,
        helper as *const (),
        &[i64::from(func), i64::from(pc)],
    )?;
    emit_continue_check(buffer, done, allocation, func, pc)
}

/// Emit the native arm for one `call_ext*` instruction: the `jit_ext_call` helper
/// (import resolution + frame discipline) followed by the same continuation check a
/// deopt uses.
///
/// The two outcomes the emitted code distinguishes are exactly the tier's:
///
/// * `encode_target(func, pc + 1)` — the helper fell back to the interpreter, which
///   ran the instruction and advanced by one (the synchronous `call_bif` path), so
///   the cached x-registers are refilled and the block continues at `pc + 1`;
/// * anything else — the `ENTER` word of a resolved cross-module transfer (this arm's
///   whole point), and `HALT`/`ERROR`/`WAIT` — jumps to the block epilogue with the
///   word in `rax`, which the driver decodes exactly as it decodes a deopt's.
///
/// `kind`/`deallocate` select the variant ([`EXT_CALL`], [`EXT_CALL_ONLY`],
/// [`EXT_CALL_LAST`] + the frame size to pop).
#[allow(clippy::too_many_arguments)]
fn emit_ext_call(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    target: ImportRef,
    kind: u32,
    deallocate: u32,
) -> Result<()> {
    let helper: ExtCallHelper = jit_ext_call;
    emit_helper_call(
        buffer,
        allocation,
        helper as *const (),
        &[
            i64::from(func),
            i64::from(pc),
            i64::from(target.0),
            i64::from(kind),
            i64::from(deallocate),
        ],
    )?;
    emit_continue_check(buffer, done, allocation, func, pc)
}

/// Emit the shared tail of a helper sequence: continue in-block when the helper's word
/// in `rax` is exactly "the next instruction" (`Step::Next`), otherwise leave the block
/// with that word in `rax` for the driver to decode.
///
/// The refill is what makes the continuation legal: every helper that can produce
/// `Next` is the interpreter and wrote `machine.x`, so the block's cached x-registers
/// are stale.
fn emit_continue_check(
    buffer: &mut CodeBuffer,
    done: CodeLabel,
    allocation: &Allocation,
    func: u32,
    pc: u32,
) -> Result<()> {
    // `rax` holds the helper's word; only "the next instruction" continues here.
    buffer.mov_ri(Reg::R11, encode_target(func, pc + 1))?;
    buffer.cmp_rr(Reg::Rax, Reg::R11)?;
    buffer.jcc(Cond::Ne, done)?;
    fill(buffer, allocation)
}

/// Write every cached x-register back into `machine.x` (`rbx` holds the base).
pub(super) fn spill(buffer: &mut CodeBuffer, allocation: &Allocation) -> Result<()> {
    for (x, reg) in &allocation.slots {
        buffer.mov_mr(Reg::Rbx, x_offset(*x), *reg)?;
    }
    Ok(())
}

/// Load every cached x-register out of `machine.x` (`rbx` holds the base).
///
/// The mandatory counterpart of [`spill`] after a helper call: the helper ran the
/// interpreter on `machine.x`, so the block's cached values are stale — every one of
/// them may have been changed (an instruction can write any x-register). Reloading
/// **all** of them is the liveness model's conservative form: no per-register
/// liveness is needed, and a register that was not actually touched is reloaded with
/// the value it already holds.
fn fill(buffer: &mut CodeBuffer, allocation: &Allocation) -> Result<()> {
    for (x, reg) in &allocation.slots {
        buffer.mov_rm(*reg, Reg::Rbx, x_offset(*x))?;
    }
    Ok(())
}
