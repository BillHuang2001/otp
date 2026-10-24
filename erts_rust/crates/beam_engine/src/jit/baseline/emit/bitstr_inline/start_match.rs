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

//! The match-entry shapes: `bs_start_match3` and `bs_start_match4`.
//!
//! Both answer the subject word itself on a hit (the family's reuse arm, no reservation),
//! and both classify a miss into [`MatchFail`] so the miss arm is either the caller's
//! resolved fail label or the one interpreter step the family's allocating raise needs.
//! The `bs_start_match4` `resume` form is the same guard with a single
//! `HEADER_SUB_BITS` compare.

use beam_common::Result;
use beam_loader::{Label, Module, Operand, NO_FAIL};

use crate::interpreter::LabelMap;
use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::super::link::{ExitSpill, Linker};
use super::super::operands::{Loc, Y_ADDR_SCRATCH};
use super::{
    emit_step_and_continue, Allocation, BIN_REF_HEADER, BITSTRING_TAG_MASK_WORD, HEADER_DISP,
    HEAP_BITS_SUBTAG_WORD, MOVE_SCRATCH, MOVE_SCRATCH_2, NOT_BOXED_MASK, POINTER_MASK,
    SUB_BITS_HEADER, SUB_BITS_ORIG_DISP,
};

#[cfg(test)]
use super::{record_inlined, SHAPE_START_MATCH3, SHAPE_START_MATCH4};

// ---------------------------------------------------------------------------
// Shapes 5/6: `bs_start_match3 { fail, bin, live, dst }` and
//              `bs_start_match4 { fail, live, src, dst }`
// ---------------------------------------------------------------------------

/// How a `bs_start_match3`/`bs_start_match4` fails at run time — the failure half of
/// the family's [`enter_match`](crate::families::bitstrings::matching::enter_match)
/// plus its `fail_step`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MatchFail {
    /// `bs_start_match4 resume`: the source must already be a match context, and any
    /// other value is the family's `NO_FAIL` raise (building the reason allocates), so
    /// a miss is the interpreter step.
    Resume,
    /// A resolvable module-local fail label, already resolved to its `(func, pc)` jump
    /// target — the family's `Step::Goto`.
    Label((u32, u32)),
    /// Everything the family has to decide itself: a `NO_FAIL` fail code (the
    /// allocating `{badmatch, Subject}` raise) or the `no_fail` atom.
    Family,
}

/// The operands of a `bs_start_match3`/`bs_start_match4` the tier lowers natively.
#[derive(Clone, Copy)]
pub(crate) struct StartMatchShape {
    /// The subject operand's location.
    src: Loc,
    /// The destination the match context is written to.
    dst: Loc,
}

impl StartMatchShape {
    /// The [`StartMatchShape`] two subject/destination operands name, or `None` when
    /// either is neither an in-bound x-register nor an addressable `Y` slot.
    ///
    /// Purely syntactic and **label-free** — the fail classification is the caller's — so
    /// the block's Y gate ([`super::super::arith::uses_y`]) derives the same answer as the
    /// emitter without needing the label map.
    pub(crate) fn of(src: &Operand, dst: &Operand) -> Option<StartMatchShape> {
        Some(StartMatchShape {
            src: Loc::of(src)?,
            dst: Loc::of(dst)?,
        })
    }

    /// The x-register indices the shape touches — the subject and the destination.
    pub(crate) fn registers(&self) -> Vec<u32> {
        let mut registers = Vec::with_capacity(2);
        registers.extend(self.src.register());
        registers.extend(self.dst.register());
        registers
    }

    /// The x-register indices the shape **writes** — the destination only. This is the
    /// one x-register a caller of [`super::super::regalloc::cacheable_writes`] must never miss:
    /// the reused context word is stored into `Dst`'s cache register, so a block that
    /// did not charge it would let a stale cache register survive an exit.
    pub(crate) fn writes(&self) -> Vec<u32> {
        self.dst.register().into_iter().collect()
    }

    /// The x-register indices the shape **reads through the block register cache** — the
    /// subject only.
    pub(crate) fn reads(&self) -> Vec<u32> {
        self.src.register().into_iter().collect()
    }

    /// Whether the lowering reads the current frame's Y window.
    pub(crate) fn uses_y(&self) -> bool {
        self.src.is_y() || self.dst.is_y()
    }
}

/// The inlinable shape of a `bs_start_match3`, or `None` when the whole instruction must
/// stay on the family.
///
/// A `NO_FAIL` fail code raises `{badmatch, Subject}` on a miss, and building that reason
/// allocates — so it is classified [`MatchFail::Family`], which routes every miss to the
/// interpreter step. A real fail label must resolve in the module's map, because the miss
/// transfer needs its `(function, code)` target; an unresolved one refuses here so the
/// family reports its exact `unknown_label`.
pub(crate) fn start_match3_shape(
    labels: &LabelMap,
    fail: Label,
    bin: &Operand,
    dst: &Operand,
) -> Option<(StartMatchShape, MatchFail)> {
    let shape = StartMatchShape::of(bin, dst)?;
    let fail = if fail == NO_FAIL {
        MatchFail::Family
    } else {
        MatchFail::Label(labels.resolve(fail).ok()?)
    };
    Some((shape, fail))
}

/// The inlinable shape of a `bs_start_match4`, or `None` when the whole instruction must
/// stay on the family.
///
/// The `fail` operand is decoded exactly as
/// [`resolve_match4_fail`](crate::families::bitstrings::matching::resolve_match4_fail)
/// decodes it (`emu/ops.tab:1243-1262`): a label (`NO_FAIL` = "raise on failure"), or the
/// atom `no_fail` / `resume`. Any other atom, an unnameable atom id or a non-label/non-atom
/// operand is refused, so the family keeps its typed `unsupported`.
pub(crate) fn start_match4_shape(
    module: &Module,
    labels: &LabelMap,
    fail: &Operand,
    src: &Operand,
    dst: &Operand,
) -> Option<(StartMatchShape, MatchFail)> {
    let shape = StartMatchShape::of(src, dst)?;
    let fail = match fail {
        Operand::Label(label) => {
            if *label == NO_FAIL {
                MatchFail::Family
            } else {
                MatchFail::Label(labels.resolve(*label).ok()?)
            }
        }
        Operand::Atom(atom) => match module.atom_name(*atom)? {
            "no_fail" => MatchFail::Family,
            "resume" => MatchFail::Resume,
            // `resolve_match4_fail`'s own `unsupported` — refuse so the family reports it.
            _ => return None,
        },
        _ => return None,
    };
    Some((shape, fail))
}

/// Emit the shared `bs_start_match3`/`bs_start_match4` lowering (see
/// [`try_inline_bs_start_match3`] for the return convention and the sequence).
///
/// No path here allocates, so a moving collection cannot run part-way through one: the
/// subject is read once and the reused context word is a **plain move** of the very word
/// the family would re-bind.
#[allow(clippy::too_many_arguments)]
fn emit_start_match(
    buffer: &mut CodeBuffer,
    shape: StartMatchShape,
    fail: MatchFail,
    linker: &Linker<'_>,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    let fallback: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();
    // The match-failure arm. Only a resolved label needs its own block: every other
    // failure class is the family's own work, i.e. the interpreter step.
    let (miss, miss_target) = match fail {
        MatchFail::Label(target) => (buffer.label(), Some(target)),
        MatchFail::Resume | MatchFail::Family => (fallback, None),
    };

    // The destination's frame-window guard **first**: a rejection must write nothing.
    shape
        .dst
        .guard_store(buffer, Y_ADDR_SCRATCH, MOVE_SCRATCH_2, fallback)?;
    shape.src.load(
        buffer,
        allocation,
        MOVE_SCRATCH,
        MOVE_SCRATCH_2,
        fallback,
        highest_touched,
    )?;
    // The answer a *hit* stores: the subject word itself (the family's
    // `write_operand(dst, context)` with `context == value`).
    buffer.mov_rr(Reg::Rsi, MOVE_SCRATCH)?;

    // `is_boxed(value)`: a non-boxed subject is neither a bitstring nor a context.
    buffer.mov_ri(MOVE_SCRATCH_2, NOT_BOXED_MASK)?;
    buffer.test_rr(MOVE_SCRATCH, MOVE_SCRATCH_2)?;
    buffer.jcc(Cond::Ne, miss)?;
    // `boxed_val(value)` — the header word's address.
    buffer.mov_ri(MOVE_SCRATCH_2, POINTER_MASK)?;
    buffer.and_rr(MOVE_SCRATCH, MOVE_SCRATCH_2)?;
    buffer.mov_rm(Reg::Rdx, MOVE_SCRATCH, HEADER_DISP)?;
    buffer.mov_ri(MOVE_SCRATCH_2, SUB_BITS_HEADER)?;
    buffer.cmp_rr(Reg::Rdx, MOVE_SCRATCH_2)?;
    // `HEADER_SUB_BITS` is the whole test for the `resume` form
    // (`raw::ctx_parts`): the operand already **is** the mutable context.
    if fail == MatchFail::Resume {
        buffer.jcc(Cond::Ne, fallback)?;
        shape
            .dst
            .store_at(buffer, Y_ADDR_SCRATCH, Reg::Rsi, allocation, highest_touched)?;
        buffer.jmp(fast_done)?;
        buffer.bind(fallback)?;
        emit_step_and_continue(buffer, done, allocation, func, pc)?;
        buffer.bind(fast_done)?;
        return Ok(());
    }

    // Not an `ErlSubBits`: a heap-bits bitstring (or a refc window, handled below) is a
    // **fresh subject** the family re-wraps at bit 0 over its whole size — that
    // reservation is the family's `make_ctx`, so it is the interpreter step. Anything
    // else (a bare `BinRef`, a tuple, an atom, …) is the match failure.
    let check_orig: CodeLabel = buffer.label();
    buffer.jcc(Cond::E, check_orig)?;
    buffer.mov_rr(Reg::Rax, Reg::Rdx)?;
    buffer.mov_ri(MOVE_SCRATCH_2, BITSTRING_TAG_MASK_WORD)?;
    buffer.and_rr(Reg::Rax, MOVE_SCRATCH_2)?;
    buffer.mov_ri(MOVE_SCRATCH_2, HEAP_BITS_SUBTAG_WORD)?;
    buffer.cmp_rr(Reg::Rax, MOVE_SCRATCH_2)?;
    buffer.jcc(Cond::E, fallback)?;
    buffer.jmp(miss)?;

    // An `ErlSubBits`: a match context **unless** its `orig` is a `BinRef`, i.e. unless it
    // is a refc-binary window — which `raw::heap_bits` reads as a subject (so
    // `enter_match` re-wraps it) before `raw::ctx_parts` ever runs.
    buffer.bind(check_orig)?;
    buffer.mov_rm(Reg::Rax, MOVE_SCRATCH, SUB_BITS_ORIG_DISP)?;
    let hit: CodeLabel = buffer.label();
    buffer.mov_ri(MOVE_SCRATCH_2, NOT_BOXED_MASK)?;
    buffer.test_rr(Reg::Rax, MOVE_SCRATCH_2)?;
    buffer.jcc(Cond::Ne, hit)?;
    buffer.mov_ri(MOVE_SCRATCH_2, POINTER_MASK)?;
    buffer.and_rr(Reg::Rax, MOVE_SCRATCH_2)?;
    buffer.mov_rm(Reg::Rax, Reg::Rax, HEADER_DISP)?;
    buffer.mov_ri(MOVE_SCRATCH_2, BIN_REF_HEADER)?;
    buffer.cmp_rr(Reg::Rax, MOVE_SCRATCH_2)?;
    buffer.jcc(Cond::E, fallback)?;
    buffer.bind(hit)?;

    shape
        .dst
        .store_at(buffer, Y_ADDR_SCRATCH, Reg::Rsi, allocation, highest_touched)?;
    buffer.jmp(fast_done)?;

    if let Some((target_func, target_pc)) = miss_target {
        buffer.bind(miss)?;
        linker.emit_exit(
            buffer,
            allocation,
            done,
            target_func,
            target_pc,
            pc,
            ExitSpill::Required,
        )?;
    }

    buffer.bind(fallback)?;
    emit_step_and_continue(buffer, done, allocation, func, pc)?;
    buffer.bind(fast_done)?;
    Ok(())
}

/// Emit `bs_start_match3 Fail Bin Live Dst` natively, or report that the block must helper
/// it (see [`try_inline_bs_get_position`] for the return convention).
///
/// The hit path is the family's **reuse** arm — `write_operand(dst, value)`, a plain move
/// of the subject word with **no** reservation — so the guarded sequence is:
///
/// ```text
///   <Dst's Y-window guard>            ; before any store, so a rejection writes nothing
///   mov  r11, <Bin>                   ; the subject word (also the reused answer)
///   mov  rsi, r11
///   test r11, 1 ; jne miss            ; not boxed -> neither a bitstring nor a context
///   and  r11, !TAG_PTR_MASK           ; boxed_val
///   mov  rdx, [r11 + 0]               ; the header word
///   cmp  rdx, HEADER_SUB_BITS
///   jne  not_a_ctx                    ; a fresh subject, or a match failure
///   mov  rax, [r11 + 32]              ; an ErlSubBits' orig
///   test rax, 1 ; jne hit             ; a non-boxed orig: a real match context
///   and  rax, !TAG_PTR_MASK ; mov rax, [rax + 0]
///   cmp  rax, HEADER_BIN_REF
///   je   fallback                     ; a refc window: the family re-wraps it
/// hit:
///   mov  <Dst>, rsi                   ; the very same context word
///   jmp  fast_done
/// not_a_ctx:
///   (header & _BITSTRING_TAG_MASK) == HEAP_BITS_SUBTAG ? jmp fallback : jmp miss
/// miss:                               ; only when the fail label resolved
///   <the linker's exit to the fail label>
/// fallback:
///   <one interpreter step>            ; the family's `make_ctx` / `badmatch` raise
/// ```
#[allow(clippy::too_many_arguments)]
pub(in crate::jit::baseline::emit) fn try_inline_bs_start_match3(
    buffer: &mut CodeBuffer,
    labels: &LabelMap,
    linker: &Linker<'_>,
    fail: Label,
    bin: &Operand,
    dst: &Operand,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some((shape, fail)) = start_match3_shape(labels, fail, bin, dst) else {
        return Ok(false);
    };
    #[cfg(test)]
    record_inlined(SHAPE_START_MATCH3);
    emit_start_match(
        buffer,
        shape,
        fail,
        linker,
        allocation,
        done,
        func,
        pc,
        highest_touched,
    )?;
    Ok(true)
}

/// Emit `bs_start_match4 Fail Live Src Dst` natively, or report that the block must
/// helper it (see [`try_inline_bs_get_position`] for the return convention).
///
/// The `resume` form reduces to `move Src Dst` (the family's `ctx_parts` test is the only
/// condition), so its sequence is the same guard with a single `HEADER_SUB_BITS` compare
/// — every other fail class reuses [`emit_start_match`]'s full classification.
#[allow(clippy::too_many_arguments)]
pub(in crate::jit::baseline::emit) fn try_inline_bs_start_match4(
    buffer: &mut CodeBuffer,
    module: &Module,
    labels: &LabelMap,
    linker: &Linker<'_>,
    fail: &Operand,
    src: &Operand,
    dst: &Operand,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some((shape, fail)) = start_match4_shape(module, labels, fail, src, dst) else {
        return Ok(false);
    };
    #[cfg(test)]
    record_inlined(SHAPE_START_MATCH4);
    emit_start_match(
        buffer,
        shape,
        fail,
        linker,
        allocation,
        done,
        func,
        pc,
        highest_touched,
    )?;
    Ok(true)
}

