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

//! The baseline JIT's **per-instruction lowering dispatch**: one native
//! `extern "C"` x86-64 function per IR basic block ([`emit_block`]) plus the
//! helper-sequence emitters every arm of that dispatch shares.
//!
//! [`emit_block`] is the single `match` that decides, at **compile time**, whether an
//! instruction is lowered natively or deopts to exactly one interpreter step: each
//! instruction group's own `try_inline_*` (in the sibling modules of this directory —
//! `r#move`, `test`, `select`, `arith`, `compare`, `bif`, `aggregate`, `frame`, `heap`,
//! `link`, `family`) is asked first, and a refusal falls through to [`emit_deopt`] —
//! which is [`emit_step_and_continue`] plus the test-only static-census attribution, the
//! one place the "did not lower this instruction" decision is taken. The module also
//! owns the prologue helpers the compiled-block shape needs ([`emit_position_store`] /
//! [`emit_publish_y`] / [`emit_prologue_helper`]), the **cross-module** arm
//! ([`emit_ext_call`]), the **receive/send** arm ([`emit_receive`]) and the
//! **local-call seam probe** ([`emit_local_call_probe`]).
//!
//! This file holds the dispatch itself; the compiled-block shape it emits — the
//! register-file constants, the [`Allocation`] register cache with its spill/fill
//! traffic, the helper-call sequence and the shared continuation check
//! [`emit_continue_check`] — lives in `super` (`emit/mod.rs`), which also documents the
//! block ABI (in `runtime`) and the driver that runs the emitted code.

use std::collections::BTreeMap;

use beam_common::Result;
use beam_loader::{ImportRef, Instr, Module};

use crate::interpreter::LabelMap;
use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::aggregate::{
    try_inline_get_hd, try_inline_get_list, try_inline_get_tl, try_inline_get_tuple_element,
    try_inline_select_tuple_arity, try_inline_set_tuple_element,
};
use super::arith::try_inline_gc_bif;
use super::bif::try_inline_bif;
use super::compare::try_inline_comparison;
use super::family;
use super::frame::{emit_deallocate, emit_deallocate_return, emit_return};
use super::heap::{emit_allocate, emit_allocate_heap, emit_test_heap, emit_trim};
use super::link::{emit_frame_teardown, Linker};
use super::operands;
use super::operands::x_offset;
use super::r#move::{try_inline_move, try_inline_swap};
use super::select::try_inline_select_val;
use super::super::call::{jit_call_store, CALL_STORED};
use super::super::receive::jit_receive;
use super::super::runtime::{
    jit_publish_y, ExtCallHelper, PositionHelper, StepHelper, EXT_CALL, EXT_CALL_LAST,
    EXT_CALL_ONLY, LOCAL_DECLINE, RUNTIME_XREGS_DISP,
};
use super::super::{jit_ext_call, jit_local_call, jit_set_position};
use super::test::try_inline_test;
use super::{
    emit_continue_check, emit_helper_call, emit_step_and_continue, touch, Allocation, MOVE_SCRATCH,
    RUNTIME_SLOT_DISP, SAVED_REGISTERS,
};
#[cfg(test)]
use super::FillSite;

/// Emit `init_yregs Registers` natively, or report that the block must helper it.
///
/// Returns `true` when every listed register was written; returns `false` **without
/// having emitted a byte** when any operand is neither an in-bound x-register nor an
/// addressable `Y` slot ([`operands::locate_all`]) — the caller then emits exactly today's
/// `jit_step_one` step, whose interpreter run reports the faithful `bad_register`.
///
/// The interpreter's arm writes `NIL` to each listed register in order (`write_operand`),
/// and so does this: `NIL` is materialised once and stored into each register through its
/// own representation — a cache register, its `machine.x` slot, or an addressable `Y` slot
/// behind the shared frame-window guard. The instruction is emitted right after
/// `allocate`, so the frame is live and the window was already published by the block
/// prologue ([`Allocation::y_last`]).
///
/// A rejected `Y` guard branches to the single `fallback`, whose `jit_step_one` re-runs
/// the whole instruction in the interpreter. A partially applied prefix is *idempotent*
/// here: every store is `NIL` into the very slot the interpreter stores `NIL` into, so the
/// re-run reaches exactly the interpreter's end state (a rejection only ever means "the
/// slot lies outside the published frame window", which the interpreter's own `y(k)`
/// resolves the same way).
#[allow(clippy::too_many_arguments)]
fn try_inline_init_yregs(
    buffer: &mut CodeBuffer,
    registers: &[beam_loader::Operand],
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some(locations) = operands::locate_all(registers) else {
        return Ok(false);
    };

    let fallback: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();

    buffer.mov_ri(MOVE_SCRATCH, beam_term::NIL.raw() as i64)?;
    for location in &locations {
        if location.is_y() {
            location.guard_store(
                buffer,
                operands::Y_ADDR_SCRATCH,
                operands::Y_ADDR_SCRATCH_2,
                fallback,
            )?;
        }
        location.store_at(
            buffer,
            operands::Y_ADDR_SCRATCH,
            MOVE_SCRATCH,
            allocation,
            highest_touched,
        )?;
    }
    buffer.jmp(fast_done)?;

    // Rejected (a `Y` register outside the current frame): one interpreter step.
    buffer.bind(fallback)?;
    emit_step_and_continue(buffer, done, allocation, func, pc)?;
    buffer.bind(fast_done)?;
    Ok(true)
}

/// Emit one native function for `code[start..end]` (a basic block of `func`).
///
/// `highest_touched` accumulates the highest x-register index the emitted code
/// touches natively (a cached load/store, or a direct `machine.x` access); the
/// driver refuses native entry to a machine whose register file does not cover it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_block(
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
    #[cfg(test)]
    let block_start_bytes = buffer.len();
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
    // Structural counter: the prologue loaded the whole cache.
    #[cfg(test)]
    super::super::counters::record_fill(FillSite::Prologue, allocation.slots.len() as u64);

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
        // Static-census instrument: every instruction this dispatch visits is an
        // "instance"; the ones it does not lower (below, through `emit_deopt`) are the
        // "deopts". Test-only and zero-cost in a production build (`#[cfg(test)]`).
        #[cfg(test)]
        super::super::counters::record_instr_instance(&super::super::counters::variant_name(instr));
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
                    emit_deopt(buffer, done, allocation, func, pc, instr)?;
                }
            }
            // `swap A B` exchanges two registers; a block normally caches fewer
            // registers than the instruction touches, so this is a common deopt it
            // is cheap to remove (a three-`mov` shuffle, register or memory). A `Y`
            // side is exchanged through the shared frame-window guard (see
            // [`r#move::emit_swap_y`]).
            Instr::Swap { a, b } => {
                if !try_inline_swap(
                    buffer,
                    a,
                    b,
                    allocation,
                    done,
                    func,
                    pc,
                    highest_touched,
                )? {
                    emit_deopt(buffer, done, allocation, func, pc, instr)?;
                }
            }
            // `init_yregs Registers` writes `NIL` into each listed register — the
            // instruction the compiler emits right after `allocate` to clear a frame's
            // `Y` slots. Every operand is written natively: an in-bound x-register
            // (through its cache register when the block caches it, else its `machine.x`
            // slot) or an addressable `Y` slot behind the frame-window guard. Any other
            // operand kind (a float register, a literal, an out-of-bound index, an
            // un-addressable `Y` index) refuses the whole instruction, which then deopts
            // to the interpreter's own step.
            Instr::InitYregs { registers } => {
                if !try_inline_init_yregs(
                    buffer,
                    registers,
                    allocation,
                    done,
                    func,
                    pc,
                    highest_touched,
                )? {
                    emit_deopt(buffer, done, allocation, func, pc, instr)?;
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
                    emit_deopt(buffer, done, allocation, func, pc, instr)?;
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
                    emit_deopt(buffer, done, allocation, func, pc, instr)?;
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
                    emit_deopt(buffer, done, allocation, func, pc, instr)?;
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
                    emit_deopt(buffer, done, allocation, func, pc, instr)?;
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
                    emit_deopt(buffer, done, allocation, func, pc, instr)?;
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
                emit_deopt(buffer, done, allocation, func, pc, instr)?;
            }
            // The optimised `{gc_bif,…}` family: the small-int
            // `erlang:'+'/2`/`'-'/2`/`'*'/2` (and `band`/`bor`/`bsr`), the comparison
            // BIFs, and the pure value accessors `erlang:byte_size/1`/`length/1` (the
            // corpus emits both as a collecting `gc_bif`). When one of the guarded fast
            // paths is emitted the block keeps going; otherwise (any other BIF, an
            // un-addressable operand, or a register the budget could not cache) it is
            // exactly the `_` arm below.
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
                )? || try_inline_bif(
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
                    emit_deopt(buffer, done, allocation, func, pc, instr)?;
                }
            }
            // The comparison BIFs (`erlang`'s `=:=`/`=/=`/`==`/`/=`/`<`/`=<`/`>`/`>=`),
            // which the compiler emits as a **non-collecting** `bif` (see [`compare`]),
            // plus the three pure term accessors [`bif`] lowers — `erlang:hd/1`,
            // `erlang:element/2` (an immediate index) and `erlang:tuple_size/1`.
            // Any other BIF, an un-addressable operand, or a module without both
            // `true`/`false` interned is the `_` arm below.
            Instr::Bif { .. } => {
                if !(try_inline_comparison(
                    buffer,
                    module,
                    instr,
                    allocation,
                    done,
                    func,
                    pc,
                    highest_touched,
                )? || try_inline_bif(
                    buffer,
                    module,
                    instr,
                    allocation,
                    done,
                    func,
                    pc,
                    highest_touched,
                )?) {
                    emit_deopt(buffer, done, allocation, func, pc, instr)?;
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
            //
            // The one call replaces **two** instruction positions, so it has **one**
            // pre-call store, whose set must be the dirty set at the moment it happens:
            // the dirty set entering the **`deallocate`** — this arm's own `pc`. The
            // `return`'s entry in the dirty table is already empty (the table clears it at
            // the `deallocate`, whose store *is* this call), so passing the recorded
            // `pc + 1` as the store pc would store nothing for a register this block's
            // inline prefix made dirty and leave `machine.x` stale for the helper.
            Instr::Deallocate(n) => {
                if matches!(code.get(start + i + 1), Some(Instr::Return)) {
                    emit_deallocate_return(buffer, allocation, done, func, pc + 1, pc, *n)?;
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
                emit_deallocate_return(buffer, allocation, done, func, pc, pc, *n)?;
                terminated = true;
                break;
            }
            // The **heap / frame-admission family** (`allocate`, `allocate_heap`,
            // `test_heap`, `trim`) — the frame-management and heap-admission
            // instructions every allocating function body carries. Like the
            // frame-discipline trio above, what a compiled block cannot do itself is
            // touch the interpreter's Rust-side `Machine` state: `push_frame`/`trim`
            // resize and renumber the `Vec`-backed frame stack, and `ensure_heap`/
            // `ensure_heap_full` runs the host's heap-admission door (the GC root-set
            // hand-off). So each goes through its own dedicated helper (the [`heap`]
            // submodule), which makes exactly the interpreter's own call (`push_frame`,
            // `trim`, or `ensure_heap`, in the interpreter's order) and answers "the
            // next instruction" on success or `ERROR` (the call's typed diagnostic)
            // otherwise. None of them transfers control, so each continues in-block on
            // success — with **no** x-register refill for the two frame-only ones (they
            // touch no x-register) and the shared partial refill for the two
            // heap-admitting ones (a collecting host may have relocated the cached
            // heap terms during `ensure_heap`).
            //
            // `allocate Ns` pushes a frame of `Ns` Y slots. It deliberately performs
            // **no** heap admission: C's `allocate Ns Live` is `$AH(Ns,0,Live)` (a heap
            // check) but this engine's `Instr::Allocate` is a bare `push_frame` (see the
            // crate `CONTEXT.md`), and the interpreter's own arm is `push_frame` then
            // `Step::Next`.
            Instr::Allocate { stack_need, .. } => {
                emit_allocate(buffer, allocation, done, func, pc, stack_need.words())?
            }
            // `allocate_heap Ns Nh Live` admits heap FIRST, then pushes the frame (the
            // interpreter's order — a failed admission must leave no frame behind).
            Instr::AllocateHeap {
                stack_need,
                heap_need,
                live,
            } => emit_allocate_heap(
                buffer,
                allocation,
                done,
                func,
                pc,
                stack_need.words(),
                heap_need.words(),
                *live,
            )?,
            // `test_heap Nh Live` — the bare heap admission, which reaches the host's
            // `ExecutionContext::ensure_heap_full` door exactly once (with the full root
            // set: the block spilled its cached x-registers first).
            Instr::TestHeap { heap_need, live } => {
                emit_test_heap(buffer, allocation, done, func, pc, heap_need.words(), *live)?
            }
            // `trim N Remaining` shrinks the current frame to `N` Y slots; a frame-size
            // mismatch (no live frame, or `N` past the frame) stays the interpreter's own
            // typed error.
            Instr::Trim { n, .. } => emit_trim(buffer, allocation, done, func, pc, *n)?,            Instr::Jump { target } => match labels.resolve(*target) {
                Ok((target_func, target_pc)) => {
                    // A compiled successor is entered **directly** (the linker tears
                    // this block's frame down and jumps to its entry); anything else
                    // returns the `(func, pc)` word to the driver exactly as before.
                    linker.emit_exit(buffer, allocation, done, target_func, target_pc, pc)?;
                    terminated = true;
                    break;
                }
                // An undefined label keeps the interpreter's exact `unknown_label`
                // error: hand the jump to the helper rather than inventing one.
                Err(_) => emit_deopt(buffer, done, allocation, func, pc, instr)?,
            },
            // A **local tail call** (`call_last Arity Label Deallocate`) transfers
            // control to another block — usually of another function — after popping
            // the current frame. The label is module-local, so it is resolved here, at
            // compile time. Three outcomes, all native:
            //
            // * the successor is a block **this compilation emitted** — the frame is
            //   popped by the interpreter's own checked pop (`jit_tail_call`, in the
            //   interpreter's resolve-then-pop order) and control jumps straight to the
            //   successor's native entry ([`link::Linker::emit_tail_exit`]);
            // * the successor is a resolved label this compilation emitted **no** block
            //   for — the common case for a tail call into another function of the
            //   **lazy** per-function artifact — the same checked pop runs and the
            //   resolved `(func, pc)` word is left in `rax` for the driver
            //   ([`link::Linker::emit_tail_exit_to_driver`]), which is exactly the
            //   interpreter's `resolve` → `pop_frame_checked` → `Step::Goto`;
            // * a label the module does **not** define keeps the documented deopt, so
            //   the interpreter's own `call_last` reports its exact `unknown_label`.
            //
            // Like `jump`, the instruction always transfers control, so the block ends
            // here on every path.
            //
            // The seam probe ([`emit_local_call_probe`]) runs first: the host's
            // **local-call** seam must be consulted even when the call is statically
            // linkable (or resolved but unlinked), because a host that serves the callee
            // as a BIF means the callee never runs at all. A **decline** falls through to
            // the unchanged exit.
            Instr::CallLast {
                target, deallocate, ..
            } => match labels.resolve(*target) {
                Ok((target_func, target_pc)) => {
                    emit_local_call_probe(buffer, allocation, done, func, pc)?;
                    if linker.is_compiled(target_func, target_pc) {
                        linker.emit_tail_exit(
                            buffer,
                            allocation,
                            done,
                            target_func,
                            target_pc,
                            *deallocate,
                        pc)?;
                    } else {
                        linker.emit_tail_exit_to_driver(
                            buffer,
                            allocation,
                            done,
                            target_func,
                            target_pc,
                            *deallocate,
                        pc)?;
                    }
                    terminated = true;
                    break;
                }
                // An undefined label keeps the interpreter's exact `unknown_label`
                // error: hand the call to the interpreter's own step rather than
                // inventing one.
                Err(_) => emit_deopt(buffer, done, allocation, func, pc, instr)?,
            },
            // A local tail call **without** a frame to pop (`call_only Arity Label`):
            // `i_call_only(CallDest)` is `$DISPATCH_REL(Dest)`, so it lowers exactly
            // like the `jump` arm above — resolve at compile time, then link or deopt.
            // The seam probe precedes the link here too (a `call_only` is the shape a
            // NIF-stub tail call takes), and a decline falls through to the unchanged
            // exit — which for a successor this compilation did not emit is
            // `emit_exit`'s return word, exactly as before.
            Instr::CallOnly { target, .. } => match labels.resolve(*target) {
                Ok((target_func, target_pc)) => {
                    emit_local_call_probe(buffer, allocation, done, func, pc)?;
                    linker.emit_exit(buffer, allocation, done, target_func, target_pc, pc)?;
                    terminated = true;
                    break;
                }
                Err(_) => emit_deopt(buffer, done, allocation, func, pc, instr)?,
            },
            // A **frame-preserving local call** (`call Arity Label`) — the one local
            // call form that needs a continuation. Unlike the two tail forms above it
            // writes `Continuation::Address { func, pc: pc + 1, context }` into the
            // **live** frame, Rust-side `Machine` state (`Machine::frames`/`stack`) a
            // compiled block cannot address, so the store goes through the
            // `jit_call_store` helper (`super::call`) while the transfer to the callee
            // reuses the linker's exit exactly as `call_only` does.
            //
            // The one helper call covers the store *and* the host's **local-call seam**
            // (`ExecutionContext::resolve_local_call`) — the interpreter's arm asks the
            // seam *before* it stores anything, because a served callee's own code never
            // runs and must not be left a return address. Its answer is the block ABI's
            // own vocabulary: [`CALL_STORED`] means "the continuation is in place,
            // transfer now", and every other word (a **served** callee's folded `Step`,
            // or the interpreter's own typed diagnostic) leaves the block for the driver
            // — the same probe-then-exit shape the two tail forms use through
            // [`emit_local_call_probe`].
            //
            // The continuation check below is the **frame-only** body of `emit/heap.rs`'s
            // private `emit_frame_change_continue`, re-implemented here because the word
            // it compares against differs: a served non-tail local call folds its
            // `Step::Next` to `encode_target(func, pc + 1)`, so the store needs a
            // sentinel of its own (see [`CALL_STORED`]). It deliberately does **not**
            // refill — the helper wrote no x-register on the transfer path, and a `call`
            // has no x-register operand — and, like the two tail forms, it must not
            // refill either: the exit below spills first, which is the tier's whole
            // liveness/root-set model.
            Instr::Call { target, .. } => match labels.resolve(*target) {
                Ok((target_func, target_pc)) => {
                    emit_helper_call(
                        buffer,
                        allocation,
                        jit_call_store as *const (),
                        &[i64::from(func), i64::from(pc)],
                    pc)?;
                    buffer.mov_ri(Reg::R11, CALL_STORED)?;
                    buffer.cmp_rr(Reg::Rax, Reg::R11)?;
                    buffer.jcc(Cond::Ne, done)?;
                    linker.emit_exit(buffer, allocation, done, target_func, target_pc, pc)?;
                    terminated = true;
                    break;
                }
                // An undefined label keeps the interpreter's exact `unknown_label`
                // error: hand the call to the interpreter's own step rather than
                // inventing a target.
                Err(_) => emit_deopt(buffer, done, allocation, func, pc, instr)?,
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
                    emit_deopt(buffer, done, allocation, func, pc, instr)?;
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
                    func,
                    pc,
                    highest_touched,
                )? {
                    terminated = true;
                    break;
                }
                emit_deopt(buffer, done, allocation, func, pc, instr)?;
            }
            // The `send` / receive-control family — the copy-bound message-passing
            // path (`send`, `loop_rec`, `loop_rec_end`, `wait`, `wait_timeout`,
            // `timeout`, `remove_message`). Left to the `_` arm each of these paid a
            // whole interpreter step per mailbox operation, which is the dominant
            // deopt class of a `send`/`receive` loop. They run through the
            // `jit_receive` helper instead, which is the receive family's own body
            // plus the interpreter step's position store and raise funnel (see
            // [`super::receive`]): a `send`/`loop_rec`/`timeout`/`remove_message`
            // continues in-block, a `loop_rec` fail-label branch and a suspending
            // `wait`/`wait_timeout` leave with that word for the driver.
            Instr::Send
            | Instr::LoopRec { .. }
            | Instr::LoopRecEnd { .. }
            | Instr::Wait { .. }
            | Instr::WaitTimeout { .. }
            | Instr::Timeout
            | Instr::RemoveMessage => emit_receive(buffer, allocation, done, func, pc)?,
            // The **whole family-owned surface** the interpreter's `other` arm hands to
            // [`crate::families::dispatch`] — every variant [`crate::families::dispatch`]
            // claims and the interpreter's core arms above do **not** (the tuples/list
            // *readers* `get_tuple_element`/`set_tuple_element`/`select_tuple_arity`/
            // `get_list`/`get_hd`/`get_tl` and the receive family keep their own arms
            // above). Each is routed to its owning family's own body through the shared
            // family helper, so the lowering is interpreter-identical **by construction**
            // (one block-local continue, else the family's own word for the driver) — the
            // same shape as the receive family's arm above.
            //
            // `dispatch` routes by the instruction's own variant to the single owning
            // family, and an unclaimed variant is the same documented `unsupported` the
            // interpreter emits (both reach the identical `families::dispatch` call), so
            // this arm can never produce a different result than the interpreter's
            // per-instruction step it replaces.
            Instr::PutList { .. } | Instr::PutTuple2 { .. }
            | Instr::UpdateRecord(_) | Instr::IsAnyNativeRecord { .. } | Instr::IsNativeRecord { .. }
            | Instr::GetRecordElements { .. } | Instr::GetRecordElementsId { .. } | Instr::PutRecord(_)
            | Instr::IsRecordAccessible { .. } | Instr::GetRecordField(_) | Instr::UpdateRecordId(_)
            | Instr::MakeFun3 { .. } | Instr::CallFun { .. } | Instr::CallFun2 { .. }
            | Instr::Apply { .. } | Instr::ApplyLast { .. }
            | Instr::Try { .. } | Instr::Catch { .. } | Instr::Badmatch { .. } | Instr::CaseEnd { .. }
            | Instr::TryCaseEnd { .. } | Instr::Badrecord { .. } | Instr::IfEnd | Instr::Raise { .. }
            | Instr::TryCase { .. } | Instr::TryEnd { .. } | Instr::CatchEnd { .. }
            | Instr::BuildStacktrace | Instr::RawRaise
            | Instr::Fmove { .. } | Instr::Fconv { .. } | Instr::Fadd { .. } | Instr::Fsub { .. }
            | Instr::Fmul { .. } | Instr::Fdiv { .. } | Instr::Fnegate { .. }
            | Instr::GetMapElements { .. } | Instr::HasMapFields { .. } | Instr::PutMapAssoc(_) | Instr::PutMapExact(_)
            | Instr::BsStartMatch3 { .. } | Instr::BsStartMatch4 { .. } | Instr::BsGetPosition { .. }
            | Instr::BsSetPosition { .. } | Instr::BsGetTail { .. } | Instr::BsTestTail2 { .. }
            | Instr::BsGetInteger2(_) | Instr::BsGetBinary2(_) | Instr::BsSkipBits2 { .. } | Instr::BsMatch { .. }
            | Instr::BsMatchString { .. } | Instr::BsGetFloat2(_) | Instr::BsGetUtf8 { .. }
            | Instr::BsGetUtf16 { .. } | Instr::BsGetUtf32 { .. } | Instr::BsSkipUtf8 { .. }
            | Instr::BsSkipUtf16 { .. } | Instr::BsSkipUtf32 { .. } | Instr::BsInitWritable | Instr::BsCreateBin { .. }
            | Instr::RecvMarkerReserve { .. } | Instr::RecvMarkerBind { .. }
            | Instr::RecvMarkerUse { .. } | Instr::RecvMarkerClear { .. }
            => { family::emit_family(buffer, allocation, done, func, pc)? }
            // Everything else: one interpreter step, then continue at `pc + 1`.
            _ => emit_deopt(buffer, done, allocation, func, pc, instr)?,
        }
    }

    if !terminated {
        // The block ran off its end: hand the next address back to the driver — or,
        // when the following block starts exactly there, jump straight into it.
        linker.emit_exit(buffer, allocation, done, func, end as u32, end as u32)?;
    }

    // --- epilogue -----------------------------------------------------------
    //
    // The shared exit every deopt path jumps to (a helper that produced a
    // non-`Next` outcome, a returned `fail`/case target word). Those paths have
    // already spilled the cached x-registers, so the epilogue only leaves the
    // frame — the exact inverse of the prologue.
    buffer.bind(done)?;
    emit_frame_teardown(buffer)?;
    buffer.ret()?;
    // Structural counter: this block's IR size, cache size, write set (the spill's
    // store count on every exit) and emitted native byte length.
    #[cfg(test)]
    super::super::counters::record_block(
        (end - start) as u64,
        allocation,
        (buffer.len() - block_start_bytes) as u64,
    );
    Ok(())
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

/// [`emit_step_and_continue`] plus the **static-census** deopt attribution: the whole
/// point of the wrapper is that it is the *one* place the dispatch's "did not lower
/// this instruction" decision is taken, so a test build can count it per variant.
///
/// Every refusal in [`emit_block`] — each `try_inline_*` returning `Ok(false)` and the
/// `_` arm — goes through here, while the *run-time* guard fallbacks **inside** the
/// `try_inline_*` functions keep calling [`emit_step_and_continue`] directly: those are
/// a rejection of an operand shape for an instruction the dispatch **did** lower, so
/// they are run-time deopts ([`super::super::counters::record_step_one`]), not static ones.
///
/// The production build is byte-for-byte the old one: the whole wrapper body is the one
/// call, and it is `#[inline]`.
#[inline]
fn emit_deopt(
    buffer: &mut CodeBuffer,
    done: CodeLabel,
    allocation: &Allocation,
    func: u32,
    pc: u32,
    #[cfg_attr(not(test), allow(unused_variables))] instr: &Instr,
) -> Result<()> {
    #[cfg(test)]
    super::super::counters::record_instr_deopt(&super::super::counters::variant_name(instr));
    emit_step_and_continue(buffer, done, allocation, func, pc)
}

/// Emit the **local-call seam probe** in front of a local tail call's exit: call
/// [`jit_local_call`], and either leave for the driver with the helper's word or fall
/// through to the block's own — unchanged — static exit.
///
/// The probe is the run-time half of the seam a compiled block needs: the cross-run
/// code cache shares blocks between runs and hosts, so whether a same-module callee is
/// served as a BIF can only be decided while the block runs, never at compile time.
///
/// The helper's word is left in `rax` (the block ABI's return register); the decline
/// comparison is against a constant loaded into `r11` — never a `movabs` into `rax`,
/// which must keep the word — and on a non-decline word the emitted code jumps to the
/// block epilogue **without a refill**, exactly as a deopt's exit path does. A decline
/// is the only path that runs on into the caller's static exit; it is safe there
/// because a decline mutates nothing (see [`jit_local_call`]), so the cached
/// x-registers that exit spills are still the values `machine.x` must receive.
fn emit_local_call_probe(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
) -> Result<()> {
    let helper: StepHelper = jit_local_call;
    emit_helper_call(
        buffer,
        allocation,
        helper as *const (),
        &[i64::from(func), i64::from(pc)],
    pc)?;
    buffer.mov_ri(Reg::R11, LOCAL_DECLINE)?;
    buffer.cmp_rr(Reg::Rax, Reg::R11)?;
    buffer.jcc(Cond::Ne, done)
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
    pc)?;
    emit_continue_check(buffer, done, allocation, func, pc)
}

/// Emit the native arm for one `send`/receive-control instruction: the `jit_receive`
/// helper followed by the same continuation check a deopt uses.
///
/// The helper has [`StepHelper`]'s exact signature (`rt`, the instruction's
/// `(func, pc)`), so it reuses [`emit_helper_call`] + [`emit_continue_check`] — the
/// spill-then-call contract and the refill after the call included:
///
/// * `encode_target(func, pc + 1)` — the instruction advanced by exactly one (`send`,
///   a `loop_rec` that produced a candidate, `timeout`, `remove_message`, and a
///   `wait_timeout` whose time operand is the small integer `0`), so the block
///   continues in-block;
/// * anything else — a `loop_rec` fail-label `Goto`, the [`encode_wait`] word of a
///   suspending `wait`/`wait_timeout`, an `ERROR` word — leaves the block with the
///   word in `rax`, which the driver decodes exactly as it decodes a deopt's.
///
/// [`encode_wait`]: crate::jit::baseline::encode_wait
fn emit_receive(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
) -> Result<()> {
    let helper: StepHelper = jit_receive;
    emit_helper_call(
        buffer,
        allocation,
        helper as *const (),
        &[i64::from(func), i64::from(pc)],
    pc)?;
    emit_continue_check(buffer, done, allocation, func, pc)
}

