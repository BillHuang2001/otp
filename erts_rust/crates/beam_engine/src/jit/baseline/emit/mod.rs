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
//! guarded aggregate readers plus the tuple writer ([`aggregate`]: the cons-cell
//! readers `get_list`/`get_hd`/`get_tl`, the tuple readers `get_tuple_element` /
//! `select_tuple_arity`, and the destructive `set_tuple_element`).
//! Two compilation entry points produce the artifact — in [`compile`]: [`compile_module`]
//! lowers **every** function of a module up front into one shared, eagerly-compiled
//! [`CompiledModule`](super::code::CompiledModule), while [`compile_function`] lowers
//! **one** function into its own sealed artifact (the
//! [`CompiledFunction`](super::code::CompiledFunction) the lazy layout fills on first
//! entry). Both share the per-function [`emit_function`](compile::emit_function). See
//! `super` for the block ABI (in `runtime`) and the driver that runs the emitted code.
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
//! touches natively — cached or direct — is recorded per function (in the function's
//! [`Summary`](super::code::Summary)); [`compile_module`] merges those into the
//! whole-module [`Summary`](super::code::Summary), and that whole-module bound is the
//! register-file guard the driver applies when entering an **eager** artifact — the one
//! shared mapping whose seal-time links may tail-call across functions — while the
//! **lazy** layout guards on the single function it is about to enter (see
//! [`enter_compiled`](super::enter_compiled)).
//!
//! **Direct block-to-block linking.** A taken branch to a successor basic block
//! that this compilation emitted does not return its `(func, pc)` word to the
//! driver: [`Linker::emit_exit`] tears down the current block's frame (spilling the
//! cached x-registers first — the tier's whole liveness/root-set model) and jumps
//! **straight to the successor block's native entry**, whose own prologue rebuilds
//! its frame. Both entry points reserve one entry label per block *before* emitting
//! any code, so a forward branch is an ordinary `rel32` fixup resolved at `bind`
//! time — never a post-`seal` patch, so the W^X policy is unchanged. A branch is
//! linked only when its target is in the **same compilation's** block map:
//! [`compile_module`]'s map spans the whole module (so a cross-function branch links
//! directly), while [`compile_function`]'s covers just that function, so a branch to
//! another function returns its word and the driver compiles that function on entry.
//! Every
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
//! [`emit_block`] and the deopt-and-continue sequence); [`compile`] the compilation
//! entry points ([`compile_module`]/[`compile_function`] via
//! [`emit_function`](compile::emit_function)) and the continuation sub-blocks;
//! [`regalloc`] the register allocator, [`operands`] the operand/constant readers,
//! [`r#move`] the inline `move`/`swap`, [`test`] the word-level `test` subset,
//! [`select`] the exact-value switch, [`arith`] the guarded small-int fast path,
//! [`link`] the direct block-to-block linker and [`aggregate`] the guarded cons-cell
//! and tuple readers.
#![allow(unsafe_code)]

use std::collections::BTreeMap;

use beam_common::Result;
use beam_loader::{ImportRef, Instr, Module};

use crate::interpreter::LabelMap;
use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::runtime::{
    encode_target, ExtCallHelper, PositionHelper, RefreshHelper, StepHelper, EXT_CALL,
    EXT_CALL_LAST, EXT_CALL_ONLY, RUNTIME_XREGS_DISP,
};
use super::{jit_ext_call, jit_set_position, jit_step_one};
use super::runtime::{jit_publish_y, jit_refresh_y};

mod aggregate;
mod arith;
mod compare;
mod compile;
mod frame;
mod link;
mod r#move;
mod operands;
mod regalloc;
mod select;
mod test;

use aggregate::{
    try_inline_get_hd, try_inline_get_list, try_inline_get_tl, try_inline_get_tuple_element,
    try_inline_select_tuple_arity, try_inline_set_tuple_element,
};
use arith::try_inline_gc_bif;
use compare::try_inline_comparison;
use frame::{emit_deallocate, emit_deallocate_return, emit_return};
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
    fast_arith, uses_y, FastDest, SmallArith, FAST_A, FAST_B, FAST_C, FAST_R, FAST_T,
    MAX_SMALL_WORD, MIN_SMALL_WORD, SMALL_MASK, SMALL_SHIFT, SMALL_TAG,
};
#[cfg(test)]
pub(crate) use operands::{constant_word, x_register, y_addressable, y_register};
#[cfg(test)]
pub(crate) use r#move::{move_shape, MoveShape};
pub(crate) use regalloc::allocate_registers;

// The compilation entry points live in the `compile` submodule; re-export them here
// so every item path outside this module (`emit::compile_module`,
// `emit::compile_function` — named by `baseline/mod.rs` and `baseline/code.rs`) stays
// exactly what it was.
pub(crate) use compile::{compile_function, compile_module};

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
    /// The **highest pc of the block whose instruction accesses a Y slot**, or `None`
    /// when no instruction of the block does.
    ///
    /// The emitter's two Y-window decisions both read it, and both are compile-time
    /// (so a Y-free block pays nothing at run time):
    ///
    /// * the **prologue** publishes the current frame's Y window iff `y_last.is_some()`
    ///   (a block that may read `JitRuntime::y_base`/`y_end`);
    /// * the **post-deopt refresh** ([`Allocation::refresh_y_after`]) is emitted only
    ///   when a still-later instruction accesses a Y slot.
    ///
    /// Only the arithmetic fast path reads the window (see the Y-operand contract in
    /// [`arith`]): an instruction "accesses a Y slot" exactly when [`arith::uses_y`]
    /// says so. A `move`/`get_list`/… with a Y operand is *not* a window access — it
    /// deopts to the interpreter, which reads `machine.stack` itself.
    pub(crate) y_last: Option<u32>,
}

impl Allocation {
    /// The machine register holding x-register `x`, if it is cached.
    pub(crate) fn slot(&self, x: u32) -> Option<Reg> {
        self.slots
            .iter()
            .find(|(n, _)| *n == x)
            .map(|(_, reg)| *reg)
    }

    /// Whether a helper call made at `pc` — whose `Step::Next` continues the block
    /// in-block at `pc + 1` — must be followed by a Y-window refresh.
    ///
    /// True only when a later instruction of the block still accesses a Y slot: the
    /// helper ran one instruction in the interpreter, which may have pushed a frame,
    /// `trim`med one, or **reallocated** `machine.stack`, so the window the block
    /// published in its prologue may be stale. When no later instruction accesses a Y
    /// slot the refresh is not emitted at all — and when the helper leaves the block to
    /// the driver, the next block's prologue publishes instead, so no driver
    /// round-trip ever refreshes.
    pub(crate) fn refresh_y_after(&self, pc: u32) -> bool {
        matches!(self.y_last, Some(last) if last > pc)
    }
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
    //
    // A block that accesses a Y slot ([`Allocation::y_last`]) publishes the current
    // frame's Y window through `jit_publish_y` **instead**, which does the position
    // store *and* refreshes the window in the one call — so publishing costs such a
    // block nothing extra, and a Y-free block keeps emitting the plain position store
    // (it pays literally nothing). See `jit_publish_y` for why the prologue is the
    // sound, cheapest publish point.
    if allocation.y_last.is_some() {
        emit_publish_y(buffer, func, start as u32)?;
    } else {
        emit_position_store(buffer, func, start as u32)?;
    }
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
                if !try_inline_move(
                    buffer,
                    src,
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
            // The guarded tuple writer: `set_tuple_element` overwrites one element word
            // of a real tuple natively and destructively (see [`aggregate`]). Any shape
            // that is not an in-bound x-register tuple plus an immediate-or-register
            // value is the `_` arm, and so is a **rejected target**: the guard branches to
            // the helper, whose interpreter step raises the exact `{badmatch, Source}` — a
            // non-tuple or an out-of-range `offset` — or reports the typed `unsupported`
            // for a term in a shared literal area.
            Instr::SetTupleElement {
                element,
                tuple,
                offset,
            } => {
                if !try_inline_set_tuple_element(
                    buffer,
                    element,
                    tuple,
                    *offset,
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
            // `deallocate N` — the **checked frame pop**. What a compiled block cannot
            // do itself is address the call-frame stack: `Machine::frames`/`stack` are
            // Rust-side `Vec`s with no layout a block could compute an offset into, and
            // the pop is *checked* (a `Deallocate` operand that no longer matches the
            // live frame must raise the interpreter's own error). So the pop goes
            // through the dedicated `jit_deallocate` helper (the [`frame`] submodule), which
            // makes the interpreter's own `Machine::pop_frame_checked` call and answers
            // "the next instruction" on success or `ERROR` (the pop's typed diagnostic)
            // otherwise — the interpreter's arm is `pop_frame_checked` then `Step::Next`.
            // Unlike a `jit_step_one` deopt this pays no interpreter dispatch, no `Step`
            // decode and **no register refill** (the pop touches no x-register), so the
            // block continues in-block on success.
            //
            // The adjacent `deallocate N; return` pair — the loader fuses it into
            // `Instr::DeallocateReturn` (`beam_loader::lower::fuse`), but a hand-built
            // module (the crate's own test modules, the bench fixtures) still holds the
            // separate pair — is lowered as **one** helper round-trip, the same shape the
            // loaded IR pays, with the `return`'s own `pc` recorded (the position the
            // interpreter would have recorded last in that code shape).
            Instr::Deallocate(n) => {
                if matches!(code.get(start + i + 1), Some(Instr::Return)) {
                    emit_deallocate_return(buffer, allocation, done, func, pc + 1, *n)?;
                    terminated = true;
                    break;
                }
                emit_deallocate(buffer, allocation, done, func, pc, *n)?;
            }
            // `return` — follow the current frame's continuation (the frame is **not**
            // popped; that is `deallocate`'s job). The dedicated `jit_return` helper makes
            // the interpreter's own `Machine::continuation()` read and encodes it as a
            // block-ABI word, so the block leaves with it in `rax`: a `(func, pc)` target
            // the driver enters (including a continuation **sub-block**), `HALT` on the
            // bottom frame, or `ENTER` for a caller in another module context. A `return`
            // always transfers control (the interpreter's `return_step` never yields
            // `Step::Next`), so the block ends here.
            Instr::Return => {
                emit_return(buffer, allocation, done, func, pc)?;
                terminated = true;
                break;
            }
            // The loader's fused superinstruction — the shape **every** compiler-produced
            // function body ends with. One `jit_deallocate_return` helper round-trip: the
            // interpreter's checked pop, then the continuation read, in the interpreter's
            // own order.
            Instr::DeallocateReturn(n) => {
                emit_deallocate_return(buffer, allocation, done, func, pc, *n)?;
                terminated = true;
                break;
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
    let helper: PositionHelper = jit_set_position;
    emit_prologue_helper(buffer, func, pc, helper as *const ())
}

/// Emit the prologue's **Y-window publish**: call `jit_publish_y(rt, func, pc)`, which
/// records this block's entry `(func, pc)` as the machine's position *and* re-publishes
/// the current frame's Y window.
///
/// A block whose body accesses a Y slot ([`Allocation::y_last`]) emits this in place of
/// [`emit_position_store`] — same three-argument call shape, so the block pays no extra
/// call for publishing. The window must be live before the block's first Y access
/// because the Erlang stack is a `Vec` a frame push may reallocate; see `jit_publish_y`.
fn emit_publish_y(buffer: &mut CodeBuffer, func: u32, pc: u32) -> Result<()> {
    let helper: PositionHelper = jit_publish_y;
    emit_prologue_helper(buffer, func, pc, helper as *const ())
}

/// Emit the prologue's three-argument helper call — `helper(rt, func, pc)` — shared by
/// [`emit_position_store`] and [`emit_publish_y`] (the two differ only in the helper's
/// body).
///
/// `rt` is read out of the prologue-reserved stack slot ([`RUNTIME_SLOT_DISP`]), so this
/// may be emitted anywhere after that slot is written. The helper clobbers only
/// caller-saved registers, so a block must have loaded nothing it needs into them at
/// the call; every cached x-register and `rbx` live in callee-saved registers, which
/// the helper preserves ([`emit_block`] emits this before loading the cached
/// x-registers for exactly that reason). `rsp` is already 16-byte aligned at the call
/// (see [`SAVED_REGISTERS`]). Neither helper returns a value, so `rax` is not consulted.
fn emit_prologue_helper(buffer: &mut CodeBuffer, func: u32, pc: u32, helper: *const ()) -> Result<()> {
    buffer.mov_rm(Reg::Rdi, Reg::Rsp, RUNTIME_SLOT_DISP)?;
    buffer.mov_ri(Reg::Rsi, i64::from(func))?;
    buffer.mov_ri(Reg::Rdx, i64::from(pc))?;
    buffer.mov_ri(Reg::R11, helper as i64)?;
    buffer.call_r(Reg::R11)
}

/// Emit the **gated Y-window refresh**: call `jit_refresh_y(rt)` so the block's next Y
/// access compares against the live frame.
///
/// Emitted on the *continue* arm of an in-block helper call ([`emit_continue_check`])
/// only when a later instruction of the block still accesses a Y slot
/// ([`Allocation::refresh_y_after`]): the helper ran one instruction in the interpreter,
/// which may have pushed a frame, `trim`med one, or reallocated `machine.stack`. The
/// register arguments are the same as a deopt's (`rt` from the block's own stack slot);
/// the helper clobbers only caller-saved registers and returns nothing.
fn emit_refresh_y(buffer: &mut CodeBuffer) -> Result<()> {
    buffer.mov_rm(Reg::Rdi, Reg::Rsp, RUNTIME_SLOT_DISP)?;
    let helper: RefreshHelper = jit_refresh_y;
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
    fill(buffer, allocation)?;
    // Continuing in-block past the helper: the instruction it ran may have pushed or
    // popped a frame (moving the Y window) or reallocated `machine.stack` (freeing the
    // published one), so re-publish the window before the next instruction when a later
    // instruction of this block still accesses a Y slot. The gate is compile-time, so a
    // block that never touches Y emits nothing here, and the refresh sits on the
    // *continue* arm only — a helper that leaves the block to the driver pays nothing
    // (the next block's prologue publishes).
    if allocation.refresh_y_after(pc) {
        emit_refresh_y(buffer)?;
    }
    Ok(())
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
