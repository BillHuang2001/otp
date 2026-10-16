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

//! The baseline JIT's register allocator: which x-registers a basic block caches.
//!
//! [`allocate_registers`] computes each x-register's **live interval** over the block's
//! IR — `[first touch, last touch]`, where a *touch* is exactly the set of x-register
//! operands a cacheable instruction's **native lowering** references — and the
//! [`CACHE_REGISTERS`]`.len()` registers with the **longest intervals** are held in
//! machine registers (the prologue loads them, every block exit spills them); every other
//! register a block names is addressed directly in `machine.x` — see [`super`].
//!
//! The interval is the same span [`super::liveness::fill_sets`] derives its live set
//! from (a register is in a helper's fill set exactly when it has a later touch), so the
//! selector and the emitter share **one** definition of liveness rather than agreeing by
//! coincidence.
//!
//! # The dirty set (`spill_sets`): a per-`pc` store set for the block's spills
//!
//! A cached register only ever needs to be written back to `machine.x` when it is
//! **dirty** — when an inline lowering has diverged its machine register from the
//! `machine.x` slot since the last event that synced the two. There are exactly two
//! syncing events:
//!
//! * the **prologue**, which loads the whole cache *from* `machine.x`, so every cached
//!   register starts the block clean; and
//! * **any helper call's spill** (a [`super::emit_helper_call`] begins with a
//!   [`super::regs::spill`]) followed by the helper running: the spill writes the dirty
//!   registers out, so afterwards every cached register equals its `machine.x` slot — and
//!   a helper that *wrote* `machine.x` is followed by a refilling
//!   [`super::emit_continue_check`] (or the equivalent `test` helper `fill`), which reads
//!   it back.
//!
//! So the true dirty set is empty at every helper boundary and grows only across the
//! inline lowerings between them. [`Allocation::spill_sets`] models exactly that: a
//! **forward walk** over the block's pcs that starts empty, unions each instruction's
//! inline **write** set ([`cacheable_writes`]) when the instruction inlines, and resets to
//! empty at an instruction whose emission is entirely a helper.
//!
//! ## Why the model can only be *conservative*, never wrong
//!
//! The walk's reset decision is derived by [`is_inlined`], which mirrors
//! [`super::emit_block`]'s **compile-time** inlining decision through the very same
//! `*_shape` predicates the `try_inline_*` entries consult. The soundness direction is
//! asymmetric and it is why every uncertainty resolves the same way:
//!
//! * a register that is truly dirty must be in the store set — otherwise its stale
//!   machine register would be lost (a **wrong result**);
//! * a register that is already clean may still be in the store set — the store writes
//!   back the value it already holds (a harmless redundant store).
//!
//! A **missed** reset therefore only ever costs a redundant store, while a **wrong** reset
//! would silently drop a live value. So `is_inlined` is biased toward "inlined": a variant
//! without an inline arm at all is a certain reset, an inline-capable variant keeps its
//! write union (never a reset) even when its run-time guard would reject, and any variant
//! the mirror does not recognise is treated as inlined. The only variants that may reset
//! are the ones whose emission unconditionally reaches a helper (the family-owned set, the
//! receive family, the cross-module calls, the frame-discipline trio, the heap family, and
//! the `_`-arm sentinel); for those the reset is *exact*, because the helper's spill
//! provably synced the whole cache.
//!
//! The model reproduces the classic "already-clean-aware" store elimination a handwritten
//! JIT's register cache does — see `ERTS_RUST_REWRITE_DEEPDIVE.md` §F (the baseline/hot
//! tier split and its correctness-first register-cache design) and the read-only legacy-C
//! reference `erts/emulator/beam/jit/beam_jit_register_cache.hpp` (its `reg_cache->x_regs[x]
//! .modified` dirty bit, checked before every `mov [x_regs+x], reg` store) plus the
//! `erts/emulator/beam/jit/x86/beam_asm*.cpp` emitters that consult it.

use std::collections::BTreeMap;

use beam_loader::{Instr, Label, Module, Operand, TestKind, NO_FAIL};

use crate::interpreter::{self, LabelMap};
use crate::machine::MAX_REGISTER;

use super::aggregate::{
    get_hd_shape, get_list_shape, get_tl_shape, get_tuple_element_shape, select_tuple_arity_plan,
    set_tuple_element_shape,
};
use super::arith::{fast_arith, uses_y};
use super::bif::bif_shape;
use super::compare::comparison_shape;
use super::r#move::move_shape;
use super::select::select_val_plan;
use super::test::{test_plan, TestSrc};
use super::{Allocation, CACHE_REGISTERS};

/// Pick the x-registers a block caches.
///
/// The block keeps the [`CACHE_REGISTERS`]`.len()` x-registers with the **longest live
/// intervals** over the block's IR. A register's live interval is
/// `[first touch, last touch]`, where a *touch* is exactly the x-register operands a
/// cacheable instruction's **native lowering** references ([`cacheable_touches`],
/// unioned with [`cacheable_writes`] so a register that an instruction only *writes* is
/// live at that point too). The cacheable instructions are the ones
/// [`super::emit_block`] lowers: a `move` whose shape
/// [`super::r#move::try_inline_move`] lowers, a `swap` over two addressable x-registers, a `test`
/// whose shape [`super::test::test_plan`] lowers, a `gc_bif` [`fast_arith`] lowers,
/// a `get_list`/`get_hd`/`get_tl` its `*_shape` lowers, a `get_tuple_element` its
/// [`get_tuple_element_shape`], a `set_tuple_element` its
/// [`set_tuple_element_shape`], and a `select_tuple_arity` its
/// [`select_tuple_arity_plan`].
///
/// The selection is a **linear scan** over those intervals:
///
/// * **primary key** — the interval length `last - first + 1`, **descending**: the
///   registers that stay live across the most instruction boundaries are held resident;
/// * **tie-break** — **ascending register index**, so the choice is fully
///   deterministic.
///
/// The span is the *same* one [`super::liveness::fill_sets`] derives its live set from
/// (a register is in a helper's fill set exactly when it has a later touch), so the
/// selector and the emitter share one definition of liveness. Every register the budget
/// cannot hold is read and written **directly in `machine.x`**, which is exactly the
/// fill/spill an in-block eviction scheme would perform; the membership itself stays
/// **fixed for the whole block** (the correctness contract — see the module docs).
///
/// A register index outside the engine's [`MAX_REGISTER`] bound never counts: the
/// inline paths refuse such an operand (reading it would be out of bounds, whereas
/// the interpreter step reports the faithful `bad_register`), so the allocator must
/// not cache it either.
///
/// The slots are returned in the registers' **first-appearance** order, so the
/// physical (`rbp`/`r12`–`r15`) assignment is deterministic and stays stable across an
/// allocator change; only the *selection* is interval-weighted.
pub(crate) fn allocate_registers(
    module: &Module,
    code: &[Instr],
    start: usize,
    end: usize,
) -> Allocation {
    // The `test` shape needs the module-wide label map to know whether a fail label
    // resolves; build it here (the driver builds the same map once per run, so this
    // costs no more than an existing per-run step).
    let labels = interpreter::build_label_map(module);
    allocate_registers_with_labels(module, &labels, code, start, end)
}

/// [`allocate_registers`] against an already-built module-wide label map.
fn allocate_registers_with_labels(
    module: &Module,
    labels: &LabelMap,
    code: &[Instr],
    start: usize,
    end: usize,
) -> Allocation {
    let end = end.min(code.len());
    let start = start.min(end);

    // The block's per-instruction native touches and writes (the shared shape mirrors of
    // `emit_block`'s inlining), restricted to registers the engine can hold. Built once
    // and reused by the interval scan, the slot order and the write set, so the three
    // views of the block can never disagree.
    let mut touches: Vec<Vec<u32>> = Vec::with_capacity(end - start);
    let mut writes: Vec<Vec<u32>> = Vec::with_capacity(end - start);
    for instr in &code[start..end] {
        touches.push(
            cacheable_touches(module, labels, instr)
                .into_iter()
                .filter(|x| *x < MAX_REGISTER)
                .collect(),
        );
        writes.push(cacheable_writes(module, labels, instr));
    }

    // The registers' first-appearance order; the slots are assigned in it, so the
    // physical mapping stays stable across an allocator change. Built over the union of
    // the touches and the writes, so a register that only ever appears as a write can
    // still be given a slot.
    let mut appearance: Vec<u32> = Vec::new();
    for (touched, written) in touches.iter().zip(&writes) {
        for x in touched.iter().chain(written) {
            if *x < MAX_REGISTER && !appearance.contains(x) {
                appearance.push(*x);
            }
        }
    }

    // Every candidate register's live interval `[first, last]`, over the instruction
    // indices `0..end - start` (scanning ascending, so the first occurrence fixes
    // `first` and every later occurrence advances `last`).
    let mut intervals: BTreeMap<u32, (usize, usize)> = BTreeMap::new();
    for (i, (touched, written)) in touches.iter().zip(&writes).enumerate() {
        for x in touched.iter().chain(written) {
            if *x >= MAX_REGISTER {
                continue;
            }
            intervals.entry(*x).or_insert((i, i)).1 = i;
        }
    }

    // Rank by descending interval length, ties by ascending register index, and keep the
    // top `CACHE_REGISTERS.len()`.
    let mut ranked: Vec<(u32, usize)> = intervals
        .iter()
        .map(|(x, (first, last))| (*x, last - first + 1))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let selected: Vec<u32> = ranked
        .iter()
        .take(CACHE_REGISTERS.len())
        .map(|(x, _)| *x)
        .collect();

    let mut allocation = Allocation::default();
    for x in appearance {
        if !selected.contains(&x) {
            continue;
        }
        allocation
            .slots
            .push((x, CACHE_REGISTERS[allocation.slots.len()]));
    }

    // Live-range statistics for the test-only structural counters: the number of
    // candidate registers the scan ranked, and the selected intervals' total and maximum
    // length (the residency quality a longer selected interval set stands for).
    #[cfg(test)]
    {
        allocation.live_candidates = intervals.len() as u32;
        allocation.live_total = ranked
            .iter()
            .take(CACHE_REGISTERS.len())
            .map(|(_, len)| *len as u32)
            .sum();
        allocation.live_max = ranked
            .iter()
            .take(CACHE_REGISTERS.len())
            .map(|(_, len)| *len as u32)
            .max()
            .unwrap_or(0);
    }

    // The block's **write set**: the cached x-registers some inline lowering of the
    // block may write (its destination operands). This is exactly the set the exit /
    // helper [`spill`](super::regs::spill) stores — a cached register outside it holds
    // exactly its `machine.x` value at every program point (see [`Allocation`]), so
    // skipping its store is a no-op. The set is collected over the whole block (a
    // register diverged anywhere is spilled at every later spill site) and restricted
    // to the cached slots, so `written ⊆ slots`.
    let mut written_set: Vec<u32> = Vec::new();
    for instr in &code[start..end] {
        for x in cacheable_writes(module, labels, instr) {
            if x < MAX_REGISTER && !written_set.contains(&x) {
                written_set.push(x);
            }
        }
    }
    allocation.written = allocation
        .slots
        .iter()
        .map(|(x, _)| *x)
        .filter(|x| written_set.contains(x))
        .collect();

    // The last pc in the block whose native lowering reads or writes a `Y` slot. The
    // engine publishes the frame's Y window (see [`super::super::runtime`]) only in the
    // prologue of a block that touches Y, and refreshes it after an in-block deopt whose
    // helper may have moved `machine.stack` — this is the compile-time gate for both.
    // A `Y` slot is **not** an x-register, so it is never charged to the live intervals
    // above (it must not enter `highest_native_register`).
    allocation.y_last = code[start..end]
        .iter()
        .enumerate()
        .filter(|(_, instr)| uses_y(module, instr))
        .map(|(offset, _)| (start + offset) as u32)
        .max();

    // The **fill sets**: for every instruction of the block, the cached x-registers a
    // helper call at that pc must reload out of `machine.x` (the block-local liveness of
    // the continuation, unioned with the write set above). See `super::liveness`.
    allocation.block_start = start as u32;
    allocation.fill_sets = super::liveness::fill_sets(module, labels, code, start, end, &allocation);

    // The **dirty set at every pc** (`spill_sets`): the cached x-registers a spill at
    // that pc must store (see the module docs). A forward walk from an empty set: an
    // instruction that inlines ([`is_inlined`]) unions the cached x-registers it writes
    // (its `machine` register diverged from `machine.x`), while an instruction whose
    // emission is entirely a helper **resets** — the helper's own spill synced the whole
    // cache, so nothing is dirty afterwards. The walk covers `[start, end)`; the final
    // entry is the block exit pc `end` (the fall-through exit's store set).
    allocation.spill_sets = spill_sets(module, labels, code, start, end, &allocation);
    allocation
}

/// The dirty set at every pc of the block `[start, end]`, indexed by `pc - start`.
///
/// The block's per-`pc` store set ([`Allocation::spill_sets`]): the cached x-registers
/// that an inline lowering has diverged from `machine.x` by that pc. Computed by the
/// forward walk documented on the module — union [`cacheable_writes`] for an inlined
/// instruction, reset for a purely-helper one, biased toward "inlined" so a missed reset
/// only costs a redundant store (never a dropped dirty value). The walk emits one entry
/// per pc in `[start, end)` plus the block exit pc `end`, so every spill site (whose pcs
/// lie in that range) resolves.
fn spill_sets(
    module: &Module,
    labels: &LabelMap,
    code: &[Instr],
    start: usize,
    end: usize,
    allocation: &Allocation,
) -> Vec<Vec<u32>> {
    let cached: Vec<u32> = allocation.slots.iter().map(|(x, _)| *x).collect();
    let mut dirty: Vec<u32> = Vec::new();
    let mut sets: Vec<Vec<u32>> = Vec::with_capacity(end - start + 1);
    for instr in &code[start..end] {
        // The store set at this pc is the dirty set *entering* the instruction.
        sets.push(dirty.clone());
        if is_inlined(module, labels, instr) {
            // An inline lowering may diverge its destination registers from `machine.x`;
            // union them in (the register index is kept only when it is cached — a
            // non-cached register is written straight to `machine.x`, so it is never
            // dirty).
            for x in cacheable_writes(module, labels, instr) {
                if cached.contains(&x) && !dirty.contains(&x) {
                    dirty.push(x);
                }
            }
        } else {
            // A purely-helper instruction: its own spill synced the whole cache, so
            // nothing is dirty afterwards.
            dirty.clear();
        }
    }
    // The block exit pc `end` (the fall-through exit).
    sets.push(dirty);
    sets
}

/// Whether [`super::emit_block`] emits an **inline lowering** for `instr` (as opposed to
/// routing the whole instruction through a helper, which is a *certain* dirty-set reset).
///
/// Mirrors the `emit_block` dispatch through exactly the `*_shape` predicates its
/// `try_inline_*` entries consult — plus the label resolution its terminating arms need —
/// so [`spill_sets`] agrees with the emitter's own compile-time decision. It is biased
/// toward `true` wherever the emitter's arm consults more than the shape (the emitter
/// treats a slightly-wider set as inlined, which only ever costs a redundant store), and
/// returns `false` only for the variants whose emission unconditionally reaches a helper:
/// the family-owned set, the receive family, the cross-module calls, the
/// frame-discipline trio, the heap family, and any variant the emitter's `_` deopt arm
/// claims.
///
/// The bias is the soundness rule (see the module docs): a **wrong** `true` costs a
/// redundant store, a **wrong** `false` would drop a dirty register's value. So a variant
/// this mirror does not model — including any future variant added to `emit_block` — must
/// fall to `false` **only** when the emitter is known to fully helper it; an unmodelled
/// variant that *might* inline is not distinguishable here, so the match keeps the
/// certain-reset list explicit and treats everything else as inlined.
fn is_inlined(module: &Module, labels: &LabelMap, instr: &Instr) -> bool {
    match instr {
        // Metadata no-ops emit no bytes and write nothing.
        Instr::Label(_)
        | Instr::Line(_)
        | Instr::IntCodeEnd
        | Instr::ExecutableLine { .. }
        | Instr::DebugLine { .. } => true,
        Instr::Move { src, dst } => move_shape(src, dst).is_some(),
        Instr::Swap { a, b } => super::r#move::swap_shape(a, b).is_some(),
        Instr::InitYregs { registers } => super::operands::locate_all(registers).is_some(),
        Instr::GetList { source, head, tail } => get_list_shape(source, head, tail).is_some(),
        Instr::GetHd { source, head } => get_hd_shape(source, head).is_some(),
        Instr::GetTl { source, tail } => get_tl_shape(source, tail).is_some(),
        Instr::GetTupleElement {
            source,
            element,
            dst,
        } => get_tuple_element_shape(source, *element, dst).is_some(),
        Instr::SetTupleElement {
            element,
            tuple,
            offset,
        } => set_tuple_element_shape(element, tuple, *offset).is_some(),
        Instr::SelectTupleArity {
            tuple,
            fail,
            destinations,
        } => select_tuple_arity_plan(labels, tuple, *fail, destinations).is_some(),
        // A `gc_bif`/`bif` inlines when any of its fast-path shapes claims it. Each
        // predicate is a **superset** of the emitter's actual inline decision (the
        // emitter's arm may still require a module-wide `true`/`false` or a resolvable
        // operand), which is the safe direction — an instruction this reports as inlined
        // but the emitter deopts spills-and-fills through `emit_deopt`, so the retained
        // dirty bits only cost redundant stores.
        Instr::GcBif { .. } => {
            fast_arith(module, instr).is_some()
                || comparison_shape(module, instr).is_some()
                || bif_shape(module, instr).is_some()
        }
        Instr::Bif { .. } => {
            comparison_shape(module, instr).is_some() || bif_shape(module, instr).is_some()
        }
        // A `test` inlines as a native predicate ([`test_plan`]) or through the shared
        // `jit_test` comparison helper (the six comparison kinds at two operands with a
        // resolvable non-`NO_FAIL` fail label) — exactly the emitter's two routes.
        Instr::Test { kind, fail, args } => {
            test_plan(module, *kind, *fail, args, labels).is_some()
                || test_helper_inlined(*kind, *fail, args, labels)
        }
        Instr::SelectVal { value, fail, pairs } => {
            select_val_plan(labels, value, *fail, pairs).is_some()
        }
        // A terminating instruction inlines iff its label resolves; an unresolved label
        // falls to the emitter's `emit_deopt`.
        Instr::Jump { target } => labels.resolve(*target).is_ok(),
        Instr::CallLast { target, .. }
        | Instr::CallOnly { target, .. }
        | Instr::Call { target, .. } => labels.resolve(*target).is_ok(),
        // Everything else: a pure helper (`emit_deopt` for the `_` arm, or one of the
        // dedicated family/receive/frame/heap helpers) — a certain reset.
        _ => false,
    }
}

/// Whether the shared `jit_test` comparison-helper route ([`super::test::emit_test_helper`])
/// claims this `test` — the six comparison kinds at two operands with a non-`NO_FAIL`,
/// resolvable fail label (the mirror of that function's own refusals).
fn test_helper_inlined(kind: TestKind, fail: Label, args: &[Operand], labels: &LabelMap) -> bool {
    matches!(
        kind,
        TestKind::IsEqExact
            | TestKind::IsNeExact
            | TestKind::IsEq
            | TestKind::IsNe
            | TestKind::IsLt
            | TestKind::IsGe
    ) && args.len() == 2
        && fail != NO_FAIL
        && labels.resolve(fail).is_ok()
}

/// The x-registers one instruction's **native** lowering touches — the operands
/// [`allocate_registers`] charges to the block's per-register live intervals.
///
/// Each arm mirrors the inlining decision of the instruction's lowering, so a
/// register only counts when the emitted code would actually reference it: a `move`
/// is counted only when [`super::r#move::try_inline_move`] lowers it, a `swap` only when both
/// operands are addressable x-registers, a `test` only when [`super::test::test_plan`] claims it,
/// a `select_val` only when [`super::select::select_val_plan`] claims it (its sole register operand
/// is the scrutinee), a `gc_bif` only when [`fast_arith`] does, a
/// `get_list`/`get_hd`/`get_tl` only when its `*_shape` claims it (its source and
/// every destination count), a `get_tuple_element` only when
/// [`get_tuple_element_shape`] claims it (its source and destination count), and a
/// `select_tuple_arity` only when [`select_tuple_arity_plan`] claims it (its source
/// tuple counts). Every other
/// instruction contributes nothing because it stays on the helper.
pub(crate) fn cacheable_touches(
    module: &Module,
    labels: &LabelMap,
    instr: &Instr,
) -> Vec<u32> {
    let mut touches = Vec::new();
    match instr {
        Instr::Move { src, dst } => {
            // Charged exactly the x-register operands of the shape
            // [`super::r#move::try_inline_move`] lowers. A `Y` source or `Y` destination is
            // a stack slot, **not** an x-register, so it is never charged: a spurious
            // entry would both evict a real x-register from the cache and raise the
            // x-register bound the driver's native-entry guard reads.
            if let Some(shape) = move_shape(src, dst) {
                touches.extend(shape.registers());
            }
        }
        Instr::Swap { a, b } => {
            // Charged exactly the x-register sides of the shape the swap inliner lowers;
            // a `Y` side is a stack slot, **not** an x-register, so it is never charged.
            if let Some(shape) = super::r#move::swap_shape(a, b) {
                touches.extend(shape.registers());
            }
        }
        // `init_yregs Registers` writes every listed register (all `NIL`) through its own
        // representation: a `Y` register is a stack slot and is never charged, while each
        // in-bound x-register is charged (it is written through its cache register when
        // the block caches it, else through its `machine.x` slot).
        Instr::InitYregs { registers } => {
            if let Some(locations) = super::operands::locate_all(registers) {
                touches.extend(locations.iter().filter_map(|location| location.register()));
            }
        }
        // A cons-cell read is charged its source and every destination, and only when
        // its `*_shape` claims it (a `Y` operand or an out-of-bound register keeps the
        // whole instruction on the helper, which contributes no native touch).
        Instr::GetList { source, head, tail } => {
            if let Some(shape) = get_list_shape(source, head, tail) {
                touches.extend(shape.registers());
            }
        }
        Instr::GetHd { source, head } => {
            if let Some(shape) = get_hd_shape(source, head) {
                touches.extend(shape.registers());
            }
        }
        Instr::GetTl { source, tail } => {
            if let Some(shape) = get_tl_shape(source, tail) {
                touches.extend(shape.registers());
            }
        }
        // A tuple read is charged its source and its destination, and only when its
        // shape is claimed (an un-addressable operand keeps the whole instruction on
        // the helper, which contributes no native touch).
        Instr::GetTupleElement {
            source,
            element,
            dst,
        } => {
            if let Some(shape) = get_tuple_element_shape(source, *element, dst) {
                touches.extend(shape.registers());
            }
        }
        // A tuple write is charged its target tuple and, for a register element, the
        // element register, and only when its shape is claimed (a `Y`/literal/out-of-bound
        // operand, or an element needing heap materialisation, keeps the whole instruction
        // on the helper, which contributes no native touch).
        Instr::SetTupleElement {
            element,
            tuple,
            offset,
        } => {
            if let Some(shape) = set_tuple_element_shape(element, tuple, *offset) {
                touches.extend(shape.registers());
            }
        }
        // An arity switch's only runtime operand is the scrutinee tuple — every
        // alternative is an immediate — and it is referenced exactly when the switch
        // is inlined.
        Instr::SelectTupleArity {
            tuple,
            fail,
            destinations,
        } => {
            if let Some(plan) = select_tuple_arity_plan(labels, tuple, *fail, destinations) {
                touches.extend(plan.registers());
            }
        }
        Instr::GcBif { .. } => {
            if let Some(fast) = fast_arith(module, instr) {
                touches.extend(fast.registers());
            } else if let Some(plan) = comparison_shape(module, instr) {
                touches.extend(plan.registers());
            } else if let Some((shape_touches, _)) = bif_shape(module, instr) {
                touches.extend(shape_touches);
            }
        }
        // A comparison BIF (`Instr::Bif` is the settled corpus's form) is charged its
        // operand registers and its destination, and only when
        // [`comparison_shape`] claims it (another BIF, an un-addressable operand, or a
        // module without both `true`/`false` interned keeps the whole instruction on
        // the helper, which contributes no native touch). The other three `Bif`s the
        // tier lowers ([`bif_shape`]: `erlang:hd/1`/`element/2`/`tuple_size/1`) are
        // charged the same way, from their own shape's operand set.
        Instr::Bif { .. } => {
            if let Some(plan) = comparison_shape(module, instr) {
                touches.extend(plan.registers());
            } else if let Some((shape_touches, _)) = bif_shape(module, instr) {
                touches.extend(shape_touches);
            }
        }
        Instr::Test { kind, fail, args } => {
            if let Some(plan) = test_plan(module, *kind, *fail, args, labels) {
                if let TestSrc::Reg(x) = plan.lhs {
                    touches.push(x);
                }
                if let Some(TestSrc::Reg(x)) = plan.rhs {
                    touches.push(x);
                }
            }
        }
        // A `select_val`'s only runtime operand is the scrutinee (every case value is
        // an immediate), and it is referenced exactly when the switch is inlined.
        Instr::SelectVal { value, fail, pairs } => {
            if let Some(plan) = select_val_plan(labels, value, *fail, pairs) {
                if let TestSrc::Reg(x) = plan.scrutinee {
                    touches.push(x);
                }
            }
        }
        _ => {}
    }
    touches
}

/// The x-registers one instruction's **native** lowering **writes** — the block's
/// write set ([`Allocation::written`]), a statically-derived superset of the cached
/// registers an inline lowering of the block may diverge from `machine.x`.
///
/// It mirrors [`cacheable_touches`]'s per-instruction shape tests exactly (so the two
/// agree on which instructions the block lowers), but keeps only each shape's
/// **destination** registers: a `move` writes its (x-register) destination, a `swap`
/// writes **both** operands, a list read writes its destinations, a tuple read writes
/// its destination, an arith `gc_bif` writes its x-register destination, and a
/// comparison BIF writes its destination. A `set_tuple_element` (a destructive heap
/// store), a `select_tuple_arity`, a `test` and a `select_val` write no x-register at
/// all, and every instruction the block does not lower contributes nothing.
///
/// The set is a **superset**: a register claimed here but never actually written (a
/// shape whose run-time guard rejects and deopts) only costs a redundant spill store,
/// never a missing one. A register the block writes but this set omitted would let a
/// stale cache register survive an exit — the one failure mode the derivation may not
/// have, which is why it mirrors the lowering shapes rather than the operands alone.
///
/// Besides the spill set, the write set is a **kill** in the block's register liveness
/// (`super::liveness::fill_sets`): an instruction that writes a register makes everything
/// before it stop needing that register's cached value. That is sound because a cached
/// register is written through its **machine register** whenever the shape writes it
/// (`super::r#move`'s destination arms), never through `machine.x` — a shape that writes a
/// register it does not cache addresses `machine.x`, and a non-cached register is never in
/// a fill set.
pub(crate) fn cacheable_writes(module: &Module, labels: &LabelMap, instr: &Instr) -> Vec<u32> {
    match instr {
        Instr::Move { src, dst } => move_shape(src, dst)
            .map(|shape| shape.writes())
            .unwrap_or_default(),
        // `swap A B` writes both operands (a `Y` side is a stack slot, never a cached
        // register).
        Instr::Swap { a, b } => super::r#move::swap_shape(a, b)
            .map(|shape| shape.writes())
            .unwrap_or_default(),
        // `init_yregs Registers` writes every listed register (all `NIL`), so each cached
        // x-register operand is a destination the exit spill must store.
        Instr::InitYregs { registers } => super::operands::locate_all(registers)
            .map(|locations| locations.iter().filter_map(|location| location.register()).collect())
            .unwrap_or_default(),
        Instr::GetList { source, head, tail } => get_list_shape(source, head, tail)
            .map(|shape| shape.writes())
            .unwrap_or_default(),
        Instr::GetHd { source, head } => get_hd_shape(source, head)
            .map(|shape| shape.writes())
            .unwrap_or_default(),
        Instr::GetTl { source, tail } => get_tl_shape(source, tail)
            .map(|shape| shape.writes())
            .unwrap_or_default(),
        Instr::GetTupleElement {
            source,
            element,
            dst,
        } => get_tuple_element_shape(source, *element, dst)
            .map(|shape| shape.writes())
            .unwrap_or_default(),
        // A tuple write stores into the heap, not into an x-register (its shape's
        // write set is empty).
        Instr::SetTupleElement {
            element,
            tuple,
            offset,
        } => set_tuple_element_shape(element, tuple, *offset)
            .map(|shape| shape.writes())
            .unwrap_or_default(),
        // An arity switch's only runtime operand is a read (its plan's write set is
        // empty).
        Instr::SelectTupleArity {
            tuple,
            fail,
            destinations,
        } => select_tuple_arity_plan(labels, tuple, *fail, destinations)
            .map(|plan| plan.writes())
            .unwrap_or_default(),
        Instr::GcBif { .. } => {
            if let Some(fast) = fast_arith(module, instr) {
                fast.writes()
            } else if let Some(plan) = comparison_shape(module, instr) {
                plan.writes()
            } else if let Some((_, writes)) = bif_shape(module, instr) {
                writes
            } else {
                Vec::new()
            }
        }
        Instr::Bif { .. } => {
            if let Some(plan) = comparison_shape(module, instr) {
                plan.writes()
            } else if let Some((_, writes)) = bif_shape(module, instr) {
                writes
            } else {
                Vec::new()
            }
        }
        // `Test`/`SelectVal` branch but write no x-register; everything else deopts.
        _ => Vec::new(),
    }
}
