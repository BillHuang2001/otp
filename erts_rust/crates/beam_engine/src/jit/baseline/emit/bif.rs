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

//! The baseline JIT's **BIF** fast paths: five `erlang:` accessors lowered in block
//! instead of deopting to one interpreter step, through **two routes**.
//!
//! **(a) The pure aggregate accessors, reused in block.** `hd/1`, `element/2` (with a
//! compile-time small-int index `>= 1`) and `tuple_size/1` are lowered through **the very
//! code the equivalent aggregate instruction already uses**, so no new guard or store shape
//! is invented. The compiler emits these three as a non-collecting `{bif,…}` (`Instr::Bif`)
//! — pure term accessors with no heap effect — so leaving them to `jit_step_one` costs a
//! helper round-trip per call for an operation that is two machine instructions plus a
//! guard. Each is claimed **only** from `Instr::Bif`:
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
//! **(b) `byte_size/1` and `length/1`, evaluated natively by the Rust helper.** Neither
//! value is a plain word read — `byte_size/1` rounds the bit size up to whole bytes and
//! `length/1` walks a *proper* list — so both are computed by [`jit_bif_value`], whose Rust
//! body reproduces the `beam_bifs` ground truth word for word. This route is claimed from
//! **either** `Instr::Bif` or `Instr::GcBif`, because the installed corpus emits both as the
//! collecting `{gc_bif,…}` (`Instr::GcBif`); the instruction's own `fail` label and raise
//! semantics stay the interpreter's, and the helper re-reads its operand from the running
//! module.
//!
//! Every rejection — an operand outside the addressable set, an index that is not a
//! compile-time small int, or any *run-time* guard failure (a non-cons source, a non-tuple
//! source, an out-of-range index, a non-bitstring `byte_size/1`, a non-list/improper
//! `length/1`, or a shape the helper's pure read cannot address) — branches to the same
//! [`emit_step_and_continue`] the instruction would have taken before, i.e. exactly one
//! interpreter step. The interpreter's `Bif`/`GcBif` arm then calls the real BIF through the
//! host, so every error (`badarg` for `hd(5)`/`hd([])`/`element(0, T)`/`element(9, {a})`/
//! `tuple_size([1])`/`byte_size(0)`/`length({a})`) is the interpreter's own: the fast path
//! can only ever *produce the same value* or *decline*, never a different result. Route (a)
//! allocates nothing and touches no heap word; route (b) reads its operand through the
//! engine's own `read_operand` and writes its result through `write_operand`, and the emitted
//! code refills the cached x-registers from the helper's [`emit_continue_check`] word exactly
//! as it does after a deopt.
//!
//! The shape functions ([`bif_shape`]/[`bif_uses_y`]) are **purely syntactic** — they
//! classify by the import's names and the operands' kinds and never consult the register
//! allocation — so [`super::regalloc`] and [`super::emit_block`] agree on which BIFs are
//! lowered, exactly like every other shape predicate in this directory.

use beam_common::Result;
use beam_loader::{Instr, Module, Operand};
use beam_term::{ARITYVAL_SUBTAG, HEADER_ARITY_OFFS, TAG_IMMED1_SIZE, TAG_IMMED1_SMALL};

use crate::jit::emitter::{CodeBuffer, Label as CodeLabel};

use super::aggregate::{self, ListShape, TupleReadShape};
use super::arith::import_names;
use super::operands::{constant_word, Loc, Y_ADDR_SCRATCH};
use super::super::runtime::StepHelper;
use super::{
    emit_continue_check, emit_helper_call, emit_step_and_continue, Allocation, MOVE_SCRATCH,
    MOVE_SCRATCH_2,
};
use crate::jit::baseline::bif_helper::jit_bif_value;

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

/// One of the named `Bif`s this module lowers, reduced to the access it performs.
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
    /// `byte_size/1` or `length/1` — evaluated by the native helper
    /// [`jit_bif_value`] (`super::super::bif_helper`), whose Rust body is the `beam_bifs`
    /// ground truth itself (`bit_size` rounded up for `byte_size/1`; the proper-list walk
    /// for `length/1`). The operand is re-read from the module by the helper, so the
    /// target carries no operand of its own.
    Value,
}

/// The accessor shape a lowered `Bif` uses — the aggregate module's own shape types, so a
/// lowered `Bif` charges (and emits) exactly what the equivalent instruction does.
enum Inlined {
    /// A cons read ([`ListShape`], the `get_hd` shape).
    Cons(ListShape),
    /// A tuple read ([`TupleReadShape`], the `get_tuple_element` shape).
    Tuple(TupleReadShape),
    /// A helper call ([`jit_bif_value`]): the block spills, calls the helper and refills
    /// on its `encode_target(func, pc + 1)` word, so its only x-register traffic is the
    /// destination it writes (a `Y` slot destination contributes nothing).
    Value {
        /// The x-register indices the sequence touches (the destination).
        touches: Vec<u32>,
        /// The x-register indices the sequence writes (the destination) — the block
        /// cache's write set; a missed destination here is a silent wrong result.
        writes: Vec<u32>,
    },
}

impl Inlined {
    /// The x-register indices the lowered sequence touches, in emission order.
    fn registers(&self) -> Vec<u32> {
        match self {
            Inlined::Cons(shape) => shape.registers(),
            Inlined::Tuple(shape) => shape.registers(),
            Inlined::Value { touches, .. } => touches.clone(),
        }
    }

    /// The x-register indices the lowered sequence **writes** (the destination, when it is
    /// an x-register) — the block cache's write set ([`Allocation::written`]).
    fn writes(&self) -> Vec<u32> {
        match self {
            Inlined::Cons(shape) => shape.writes(),
            Inlined::Tuple(shape) => shape.writes(),
            Inlined::Value { writes, .. } => writes.clone(),
        }
    }
    /// The x-register indices the lowered sequence **reads through the block register
    /// cache** — the operand registers.
    ///
    /// The helper-evaluated accessors ([`Inlined::Value`]) read their source out of
    /// `machine.x` *inside the helper* (the block spills first), so they read **no** cache
    /// register: only their destination is charged. This is what the block prologue's
    /// entry-load decision needs (see [`super::super::liveness`]).
    fn reads(&self) -> Vec<u32> {
        match self {
            Inlined::Cons(shape) => shape.reads(),
            Inlined::Tuple(shape) => shape.reads(),
            Inlined::Value { .. } => Vec::new(),
        }
    }

    /// Whether the lowered sequence reads the current frame's `Y` window (a `Y` source or
    /// destination), i.e. whether the block must publish/refresh it
    /// ([`super::arith::uses_y`]).
    ///
    /// The helper-evaluated accessors are always `false`: [`jit_bif_value`] reads and
    /// writes `machine.x`/`machine.stack` through the engine's own `read_operand`/
    /// `write_operand`, which address a `Y` slot in `machine.stack` directly and never
    /// through the emitted window.
    fn uses_y(&self) -> bool {
        match self {
            Inlined::Cons(shape) => shape.uses_y(),
            Inlined::Tuple(shape) => shape.uses_y(),
            Inlined::Value { .. } => false,
        }
    }
}

/// Whether `source`/`dst` are a shape [`jit_bif_value`] can serve for `byte_size/1` or
/// `length/1`.
///
/// Purely syntactic: the source must be a **pure machine read** the helper can address
/// (an in-bound `X` register or an addressable `Y` slot) or a **constant** operand
/// (`small_word`: a small-int `Integer`/`Character`, an `Atom`, or `[]`), and the
/// destination must be a [`Loc`] the helper can write (an in-bound `X` register or an
/// addressable `Y` slot). Anything else — a float register, a literal-area pointer, an
/// out-of-bound register, or a non-small `Integer` constant whose `read_operand` would
/// materialise a bignum — is not a fast-path shape and keeps today's interpreter step.
fn value_shape(source: &Operand, dst: &Operand) -> bool {
    (Loc::of(source).is_some() || constant_word(source).is_some()) && Loc::of(dst).is_some()
}

/// The `(touches, writes)` x-register sets a [`Target::Value`] lowering charges.
///
/// The helper call's own lived registers are never held in a machine register (the spill
/// writes `machine.x` first and the helper reads it back), so the shape charges exactly
/// the destination — **including it in the write set is load-bearing**: the block's exit
/// spill must store the (refilled) cached destination back to `machine.x`, and the
/// post-helper [`emit_continue_check`] fill set is `(live ∪ written) ∩ slots`.
fn value_registers(dst: &Operand) -> (Vec<u32>, Vec<u32>) {
    match Loc::of(dst).and_then(Loc::register) {
        Some(x) => (vec![x], vec![x]),
        None => (Vec::new(), Vec::new()),
    }
}

/// Classify `instr` as one of the five lowered BIFs — the three pure accessors reused from
/// the aggregate read guards, or the two helper-evaluated value accessors — with the
/// destination operand every one of them writes.
///
/// The test is exactly the interpreter's own dispatch key: the import's **module** name is
/// `erlang`, its arity matches `Instr::Bif`'s, and the argument count matches; anything
/// else (another BIF, an unresolved import, a mismatched arity) is `None`, so the
/// instruction keeps today's `jit_step_one` step.
fn classify<'a>(module: &Module, instr: &'a Instr) -> Option<(Target<'a>, &'a Operand)> {
    // The two BIF-carrying variants share their `arity`/`bif`/`args`/`dst` fields (a
    // `GcBif` adds only the `fail` label and `live` count, which the interpreter owns —
    // the helper never reproduces a `GcBif`'s fail label or raise itself). A shape is
    // still classified **per variant**: the pure accessors below are the non-collecting
    // `bif` the compiler emits them as, while the helper-evaluated value accessors are
    // eligible from **either** variant (the installed corpus emits both `byte_size/1`
    // and `length/1` as `{gc_bif,…}`).
    let (arity, bif, args, dst, collecting) = match instr {
        Instr::Bif {
            arity, bif, args, dst, ..
        } => (arity, bif, args, dst, false),
        Instr::GcBif {
            arity, bif, args, dst, ..
        } => (arity, bif, args, dst, true),
        _ => return None,
    };
    let (module_name, function_name, import_arity) = import_names(module, *bif)?;
    if module_name != "erlang" || import_arity != u32::from(*arity) {
        return None;
    }
    match (function_name, args.as_slice()) {
        // The three pure accessors are `Bif`-only (`collecting` = a `{gc_bif,…}` of the
        // same name is not a shape this lowering owns).
        ("hd", [source]) if !collecting => Some((Target::Hd(source), dst)),
        ("element", [index, tuple]) if !collecting => Some((
            Target::Element {
                tuple,
                element: element_index(index)?,
            },
            dst,
        )),
        ("tuple_size", [source]) if !collecting => Some((Target::TupleSize(source), dst)),
        // The helper-evaluated value accessors: `byte_size/1` and `length/1`, accepted
        // only for a shape their Rust body can serve end to end (`value_shape`), from
        // either variant — neither has a heap effect, so the helper's value is exact and
        // any shape it cannot prove still declines to the interpreter's own step.
        ("byte_size" | "length", [source]) if value_shape(source, dst) => {
            Some((Target::Value, dst))
        }
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
        Target::Value => {            let (touches, writes) = value_registers(dst);
            Inlined::Value { touches, writes }
        }
    })
}

/// The `(touches, writes)` x-register sets of the `Bif`'s native lowering, or `None` when
/// the instruction is not one of the five lowered BIFs in an inlinable shape.
///
/// Purely syntactic, so [`super::regalloc::allocate_registers`] caches exactly the
/// registers the emitter reads and — load-bearing — **writes** ([`Inlined::writes`]: a
/// missed destination in the write set would leave a stale cached register behind a block
/// exit, a silent wrong result).
pub(super) fn bif_shape(module: &Module, instr: &Instr) -> Option<(Vec<u32>, Vec<u32>)> {
    let shape = shape(module, instr)?;
    Some((shape.registers(), shape.writes()))
}
/// The x-registers the `Bif`'s native lowering **reads through the block register cache**,
/// or `None` when the instruction is not one of the five lowered BIFs in an inlinable
/// shape. The read-set twin of [`bif_shape`] (see [`Inlined::reads`]).
pub(super) fn bif_reads(module: &Module, instr: &Instr) -> Option<Vec<u32>> {
    shape(module, instr).map(|shape| shape.reads())
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
/// **without having emitted a byte** when `instr` is not one of the five lowered BIFs or
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
        // `byte_size/1`/`length/1` are served by the native helper `jit_bif_value`, whose
        // Rust body reproduces `beam_bifs`'s own value (or declines to the interpreter's
        // step, which raises its exact `badarg`/`system_limit`).
        Target::Value => try_inline_bif_value(buffer, allocation, done, func, pc),
    }
}

/// Emit the native helper call for `erlang:byte_size/1` / `erlang:length/1`, always
/// continuing in block on the helper's "advanced by one" word.
///
/// [`jit_bif_value`] has [`StepHelper`]'s exact signature, so the sequence is the shared
/// [`emit_helper_call`] (which **spills the cached x-registers first**, so the helper's
/// `read_operand` sees the interpreter's exact live `machine.x`) followed by the shared
/// [`emit_continue_check`]:
///
/// * `encode_target(func, pc + 1)` — the value is in the destination (written through
///   `write_operand`) **or** the interpreter's own step ran the instruction and advanced
///   by one (a declined shape) — the emitted code refills the cached registers the
///   continuation can still observe (the destination among them, being in the block's
///   write set) and continues in block;
/// * any other word — the folded `Step` of the interpreter's own step (the `ERROR` of a
///   propagating `badarg`/`system_limit`, a `Goto`, an `ENTER`, the `Wait` bit, `HALT`) —
///   leaves the block with that word in `rax`, exactly as a deopt's.
///
/// Nothing here allocates: the accepted shapes are only ones whose `read_operand` is a
/// pure machine read (`value_shape`), and the helper declines a non-small `Integer`
/// constant to the interpreter.
fn try_inline_bif_value(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
) -> Result<bool> {
    let helper: StepHelper = jit_bif_value;
    emit_helper_call(
        buffer,
        allocation,
        helper as *const (),
        &[i64::from(func), i64::from(pc)],
    pc)?;
    emit_continue_check(buffer, done, allocation, func, pc)?;
    Ok(true)
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
