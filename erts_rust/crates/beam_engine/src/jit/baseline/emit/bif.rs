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

//! The baseline JIT's **BIF** fast paths: `erlang:hd/1`, `erlang:element/2` and
//! `erlang:tuple_size/1` lowered in block instead of deopting to one interpreter step.
//!
//! The compiler emits these three as a non-collecting `{bif,…}` (`Instr::Bif`) — pure
//! term accessors with no heap effect — so leaving them to `jit_step_one` costs a helper
//! round-trip per call for an operation that is two machine instructions plus a guard.
//! Each is lowered through **the very code the equivalent aggregate instruction already
//! uses**, so no new guard or store shape is invented:
//!
//! * `hd/1` is [`emit::aggregate::try_inline_get_hd`] — the `get_hd Source Head` lowering
//!   (guard `is_list`, then read the CAR word into the destination);
//! * `element/2` with an immediate `1 =< Index` is
//!   [`emit::aggregate::try_inline_get_tuple_element`] with `element = Index - 1` — the
//!   `get_tuple_element` lowering, whose guard (`is_tuple` **and** `element < arity`) is
//!   exactly `1 =< Index =< tuple_size(Tuple)`;
//! * `tuple_size/1` reuses the same `is_tuple` header guard and derives the arity from the
//!   header word the guard leaves behind (see [`try_inline_tuple_size`]).
//!
//! Every rejection — an operand outside the addressable set, an index that is not a
//! compile-time small int, or any *run-time* guard failure (a non-cons source, a non-tuple
//! source, an out-of-range index) — branches to the same [`emit_step_and_continue`] the
//! instruction would have taken before, i.e. exactly one interpreter step. The
//! interpreter's `Bif` arm then calls the real BIF through the host, so every error
//! (`badarg` for `hd(5)`/`hd([])`/`element(0, T)`/`element(9, {a})`/`tuple_size([1])`) is
//! the interpreter's own: the fast path can only ever *produce the same value* or
//! *decline*, never a different result. It allocates nothing and touches no heap word.
//!
//! The two shape functions ([`bif_shape`]/[`bif_uses_y`]) are **purely syntactic** — they
//! classify by the import's names and the operands' kinds and never consult the register
//! allocation — so [`super::regalloc`] and [`super::emit_block`] agree on which `Bif`s are
//! lowered, exactly like every other shape predicate in this directory.

use beam_common::Result;
use beam_loader::{Instr, Module, Operand};
use beam_term::{ARITYVAL_SUBTAG, HEADER_ARITY_OFFS, TAG_IMMED1_SIZE, TAG_IMMED1_SMALL};

use crate::jit::emitter::{CodeBuffer, Label as CodeLabel};

use super::aggregate::{self, ListShape, TupleReadShape};
use super::arith::import_names;
use super::operands::Y_ADDR_SCRATCH;
use super::{emit_step_and_continue, Allocation, MOVE_SCRATCH, MOVE_SCRATCH_2};

/// The right shift turning a tuple's header word into its arity's **untagged** value: the
/// header of an `N`-tuple is `(N << HEADER_ARITY_OFFS) + ARITYVAL_SUBTAG` and a small int
/// is `(V << TAG_IMMED1_SIZE) + TAG_IMMED1_SMALL`, so `hdr >> ARITY_SHIFT` is
/// `arity << TAG_IMMED1_SIZE` and one `or` with the small tag completes `make_small(arity)`.
///
/// Subtracting the two `u32` term-layout constants in a `const` is deliberately
/// compile-checked, and the shift is sound only because the low `HEADER_ARITY_OFFS` bits of
/// a tuple header are the arityval subtag — asserted below, so a term-layout change can
/// never silently turn this into a different value.
const ARITY_SHIFT: u32 = HEADER_ARITY_OFFS - TAG_IMMED1_SIZE;
const _: () = assert!(ARITYVAL_SUBTAG == 0);

/// One of the three `Bif`s this module lowers, reduced to the access it performs.
#[derive(Clone, Copy)]
enum Target<'a> {
    /// `hd/1` over the cons cell in this operand.
    Hd(&'a Operand),
    /// `element/2`: the tuple operand and the resolved 0-based element index.
    Element {
        /// The tuple operand (`element/2`'s second argument).
        tuple: &'a Operand,
        /// The 1-based index operand resolved to a 0-based element offset.
        element: u32,
    },
    /// `tuple_size/1` over the tuple in this operand.
    TupleSize(&'a Operand),
}

/// The accessor shape a lowered `Bif` uses — the aggregate module's own shape types, so a
/// lowered `Bif` charges (and emits) exactly what the equivalent instruction does.
enum Inlined {
    /// A cons read ([`ListShape`], the `get_hd` shape).
    Cons(ListShape),
    /// A tuple read ([`TupleReadShape`], the `get_tuple_element` shape).
    Tuple(TupleReadShape),
}

impl Inlined {
    /// The x-register indices the lowered sequence touches, in emission order.
    fn registers(&self) -> Vec<u32> {
        match self {
            Inlined::Cons(shape) => shape.registers(),
            Inlined::Tuple(shape) => shape.registers(),
        }
    }

    /// The x-register indices the lowered sequence **writes** (the destination, when it is
    /// an x-register) — the block cache's write set ([`Allocation::written`]).
    fn writes(&self) -> Vec<u32> {
        match self {
            Inlined::Cons(shape) => shape.writes(),
            Inlined::Tuple(shape) => shape.writes(),
        }
    }

    /// Whether the lowered sequence reads the current frame's `Y` window (a `Y` source or
    /// destination), i.e. whether the block must publish/refresh it
    /// ([`super::arith::uses_y`]).
    fn uses_y(&self) -> bool {
        match self {
            Inlined::Cons(shape) => shape.uses_y(),
            Inlined::Tuple(shape) => shape.uses_y(),
        }
    }
}

/// Classify `instr` as one of the three lowered BIFs, with the destination operand every
/// one of them writes.
///
/// The test is exactly the interpreter's own dispatch key: the import's **module** name is
/// `erlang`, its arity matches `Instr::Bif`'s, and the argument count matches; anything
/// else (another BIF, an unresolved import, a mismatched arity) is `None`, so the
/// instruction keeps today's `jit_step_one` step.
fn classify<'a>(module: &Module, instr: &'a Instr) -> Option<(Target<'a>, &'a Operand)> {
    let Instr::Bif {
        arity, bif, args, dst, ..
    } = instr
    else {
        return None;
    };
    let (module_name, function_name, import_arity) = import_names(module, *bif)?;
    if module_name != "erlang" || import_arity != u32::from(*arity) {
        return None;
    }
    match (function_name, args.as_slice()) {
        ("hd", [source]) => Some((Target::Hd(source), dst)),
        ("element", [index, tuple]) => Some((
            Target::Element {
                tuple,
                element: element_index(index)?,
            },
            dst,
        )),
        ("tuple_size", [source]) => Some((Target::TupleSize(source), dst)),
        _ => None,
    }
}

/// The 0-based element offset an `element/2` index operand addresses, or `None` when the
/// operand is not a plain small-int immediate `>= 1`.
///
/// The interpreter raises `badarg` for a non-integer or out-of-range index, so such a call
/// must stay on the helper; a `>= 1` immediate is the shape the emitted `element < arity`
/// guard reproduces exactly.
fn element_index(operand: &Operand) -> Option<u32> {
    let Operand::Integer(index) = operand else {
        return None;
    };
    u32::try_from(index.checked_sub(1)?).ok()
}

/// The aggregate shape a lowered `Bif` emits, or `None` when the operands are outside the
/// inlinable subset.
fn shape(module: &Module, instr: &Instr) -> Option<Inlined> {
    let (target, dst) = classify(module, instr)?;
    Some(match target {
        Target::Hd(source) => Inlined::Cons(aggregate::get_hd_shape(source, dst)?),
        Target::Element { tuple, element } => {
            Inlined::Tuple(aggregate::get_tuple_element_shape(tuple, element, dst)?)
        }
        Target::TupleSize(source) => {
            Inlined::Tuple(aggregate::get_tuple_element_shape(source, 0, dst)?)
        }
    })
}

/// The `(touches, writes)` x-register sets of the `Bif`'s native lowering, or `None` when
/// the instruction is not one of the three lowered BIFs in an inlinable shape.
///
/// Purely syntactic, so [`super::regalloc::allocate_registers`] caches exactly the
/// registers the emitter reads and — load-bearing — **writes** ([`Inlined::writes`]: a
/// missed destination in the write set would leave a stale cached register behind a block
/// exit, a silent wrong result).
pub(super) fn bif_shape(module: &Module, instr: &Instr) -> Option<(Vec<u32>, Vec<u32>)> {
    let shape = shape(module, instr)?;
    Some((shape.registers(), shape.writes()))
}

/// Whether the `Bif`'s native lowering reads the current frame's `Y` window.
///
/// The per-shape half of [`super::arith::uses_y`]: a `Y` source or destination is read
/// through the guarded window, so a block containing one must publish (and, after an
/// in-block helper call, refresh) it. A `Bif` the tier does not lower reads nothing.
pub(super) fn bif_uses_y(module: &Module, instr: &Instr) -> bool {
    shape(module, instr).is_some_and(|shape| shape.uses_y())
}

/// Emit the fast path for `instr`, or report that the block must helper it.
///
/// Returns `true` when the whole guarded sequence was emitted, leaving the BIF's value in
/// `dst` and letting the block continue with the next instruction. Returns `false`
/// **without having emitted a byte** when `instr` is not one of the three lowered BIFs or
/// its operands are not an inlinable shape ([`shape`]), so the caller emits exactly today's
/// `jit_step_one` step.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_inline_bif(
    buffer: &mut CodeBuffer,
    module: &Module,
    instr: &Instr,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some((target, dst)) = classify(module, instr) else {
        return Ok(false);
    };
    match target {
        // `hd/1` is the cons read the `get_hd Source Head` instruction already lowers
        // (`is_list` guard, then the CAR word); its rejection runs the interpreter's step,
        // which calls the real `hd/1` and raises its exact `badarg`.
        Target::Hd(source) => aggregate::try_inline_get_hd(
            buffer,
            source,
            dst,
            allocation,
            done,
            func,
            pc,
            highest_touched,
        ),
        // `element(Index, Tuple)` with an immediate `Index >= 1` is the tuple read of
        // element `Index - 1`, whose guard (`is_tuple && element < arity`) is exactly
        // `1 =< Index =< tuple_size(Tuple)`; a rejection runs the interpreter's step, which
        // calls the real `element/2` (`badarg` for an out-of-range index or a non-tuple).
        Target::Element { tuple, element } => aggregate::try_inline_get_tuple_element(
            buffer,
            tuple,
            element,
            dst,
            allocation,
            done,
            func,
            pc,
            highest_touched,
        ),
        // `tuple_size/1` has no matching instruction (no IR op reads a tuple's arity as a
        // term), so it reuses the tuple header guard and derives the arity from the header
        // word the guard leaves in `GUARD_SCRATCH`.
        Target::TupleSize(source) => try_inline_tuple_size(
            buffer,
            source,
            dst,
            allocation,
            done,
            func,
            pc,
            highest_touched,
        ),
    }
}

/// Emit `erlang:tuple_size/1` natively, or report that the block must helper it
/// ([`try_inline_tuple_size`] is called only after [`classify`] accepted the BIF, so the
/// only refusal here is a non-addressable source/destination).
///
/// The emitted shape:
///
/// ```text
///   <emit_tuple_header_guard>   ; is_boxed + arityval subtag -> header word in r8,
///                               ;   else `jmp fallback`
///   <a Y destination's guard>   ; its slot address in r9, else nothing
///   mov  r11, r8                ; the arityval header word
///   sar  r11, ARITY_SHIFT       ; `arity << TAG_IMMED1_SIZE` (the header is non-negative)
///   mov  r10, TAG_IMMED1_SMALL
///   or   r11, r10               ; `make_small(arity)`
///   mov  <dst>, r11             ; the cached register, `machine.x` slot, or the Y slot
///   jmp  fast_done
/// fallback:
///   <the `emit_step_and_continue` sequence: one `jit_step_one` on the instruction, which
///    calls the real `tuple_size/1` and raises its exact `badarg` for a non-tuple>
/// fast_done:
///   <emit the rest of the block>
/// ```
///
/// The destination is written the same way every other aggregate read writes one — a cached
/// register, its own `machine.x` slot, or a `Y` slot behind its frame-window guard emitted
/// **before** the value is computed — so the tier's liveness/root-set model is unchanged.
/// The fast path never spills, never allocates and never touches the heap.
#[allow(clippy::too_many_arguments)]
fn try_inline_tuple_size(
    buffer: &mut CodeBuffer,
    source: &Operand,
    dst: &Operand,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    // The arity `0` shape: only the source and the destination *locations* are needed
    // (the element offset is never read), and it enforces the same addressability the
    // emitted stores require.
    let Some(shape) = aggregate::get_tuple_element_shape(source, 0, dst) else {
        return Ok(false);
    };

    let fallback: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();

    // `is_tuple(source)`, leaving the arityval header word in `GUARD_SCRATCH`.
    aggregate::emit_tuple_header_guard(
        buffer,
        shape.source,
        allocation,
        fallback,
        highest_touched,
    )?;

    // A `Y` destination's frame-window guard goes **before** the value is computed, so a
    // rejection leaves no destination written and cannot clobber the header/address
    // scratch the value comes from.
    if shape.dst.is_y() {
        shape
            .dst
            .guard_store(buffer, Y_ADDR_SCRATCH, MOVE_SCRATCH_2, fallback)?;
    }

    // `make_small(arity)`: the header word is `(arity << HEADER_ARITY_OFFS) + 0`, so an
    // **arithmetic** right shift (the header word is non-negative) by
    // `HEADER_ARITY_OFFS - TAG_IMMED1_SIZE` yields `arity << TAG_IMMED1_SIZE`, whose low
    // bits an `or` with the small tag completes.
    buffer.mov_rr(MOVE_SCRATCH, aggregate::GUARD_SCRATCH)?;
    buffer.sar_ri(MOVE_SCRATCH, ARITY_SHIFT)?;
    buffer.mov_ri(MOVE_SCRATCH_2, TAG_IMMED1_SMALL as i64)?;
    buffer.or_rr(MOVE_SCRATCH, MOVE_SCRATCH_2)?;
    shape.dst.store_at(
        buffer,
        Y_ADDR_SCRATCH,
        MOVE_SCRATCH,
        allocation,
        highest_touched,
    )?;
    buffer.jmp(fast_done)?;

    // Rejected (or not lowered): one interpreter step, then continue at `pc + 1`.
    buffer.bind(fallback)?;
    emit_step_and_continue(buffer, done, allocation, func, pc)?;
    buffer.bind(fast_done)?;
    Ok(true)
}
