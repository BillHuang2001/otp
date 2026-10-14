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
//! [`allocate_registers`] ranks a block's x-registers by how often the block's
//! **inlinable** instructions would touch them, and the most-used
//! [`CACHE_REGISTERS`]`.len()` of them are held in machine registers (the prologue
//! loads them, every block exit spills them); every other register a block names is
//! addressed directly in `machine.x` — see [`super`].

use std::collections::BTreeMap;

use beam_loader::{Instr, Module};

use crate::interpreter::{self, LabelMap};
use crate::machine::MAX_REGISTER;

use super::aggregate::{
    get_hd_shape, get_list_shape, get_tl_shape, get_tuple_element_shape, select_tuple_arity_plan,
    set_tuple_element_shape,
};
use super::arith::{fast_arith, uses_y};
use super::compare::comparison_shape;
use super::r#move::move_shape;
use super::operands::x_register;
use super::select::select_val_plan;
use super::test::{test_plan, TestSrc};
use super::{Allocation, CACHE_REGISTERS};

/// Pick the x-registers a block caches.
///
/// The block keeps the [`CACHE_REGISTERS`]`.len()` x-registers its native lowering
/// **touches the most**: every cacheable instruction (a `move` whose shape
/// [`super::r#move::try_inline_move`] lowers, a `swap` over two addressable x-registers, a `test`
/// whose shape [`super::test::test_plan`] lowers, a `gc_bif` [`fast_arith`] lowers,
/// a `get_list`/`get_hd`/`get_tl` its `*_shape` lowers, a `get_tuple_element` its
/// [`get_tuple_element_shape`], a `set_tuple_element` its
/// [`set_tuple_element_shape`], and a `select_tuple_arity` its
/// [`select_tuple_arity_plan`])
/// contributes one *native touch* per x-register operand, and the registers with the
/// highest touch counts are held resident — a usage-weighted approximation of
/// "keep the longest live ranges in machine registers". Ties are broken by ascending
/// register index, so the choice is deterministic. Every register the budget cannot
/// hold is read and written **directly in `machine.x`**, which is exactly the
/// fill/spill an in-block eviction scheme would perform; the membership itself stays
/// **fixed for the whole block** (the correctness contract — see the module docs).
///
/// A register index outside the engine's [`MAX_REGISTER`] bound never counts: the
/// inline paths refuse such an operand (reading it would be out of bounds, whereas
/// the interpreter step reports the faithful `bad_register`), so the allocator must
/// not cache it either.
///
/// The slots are returned in the registers' **first-appearance** order, so the
/// physical (`rbp`/`r12`–`r15`) assignment is deterministic; only the *selection* is
/// usage-weighted.
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

    // Native touch counts and the registers' first-appearance order.
    let mut counts: BTreeMap<u32, u32> = BTreeMap::new();
    let mut appearance: Vec<u32> = Vec::new();
    for instr in &code[start..end] {
        for x in cacheable_touches(module, labels, instr) {
            if x >= MAX_REGISTER {
                continue;
            }
            *counts.entry(x).or_insert(0) += 1;
            if !appearance.contains(&x) {
                appearance.push(x);
            }
        }
    }

    // Rank by descending touch count, ties by ascending register index, and keep the
    // top `CACHE_REGISTERS.len()`.
    let mut ranked: Vec<(u32, u32)> = counts.iter().map(|(x, count)| (*x, *count)).collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let selected: Vec<u32> = ranked
        .into_iter()
        .take(CACHE_REGISTERS.len())
        .map(|(x, _)| x)
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
    // A `Y` slot is **not** an x-register, so it is never charged to the touch counts
    // above (it must not enter `highest_native_register`).
    allocation.y_last = code[start..end]
        .iter()
        .enumerate()
        .filter(|(_, instr)| uses_y(module, instr))
        .map(|(offset, _)| (start + offset) as u32)
        .max();
    allocation
}

/// The x-registers one instruction's **native** lowering touches — the operands
/// [`allocate_registers`] charges to the block's usage counts.
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
fn cacheable_touches(
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
            let (Some(a_x), Some(b_x)) = (x_register(a), x_register(b)) else {
                return touches;
            };
            if a_x >= MAX_REGISTER || b_x >= MAX_REGISTER {
                return touches;
            }
            touches.push(a_x);
            touches.push(b_x);
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
            }
        }
        // A comparison BIF (`Instr::Bif` is the settled corpus's form) is charged its
        // operand registers and its destination, and only when
        // [`comparison_shape`] claims it (another BIF, an un-addressable operand, or a
        // module without both `true`/`false` interned keeps the whole instruction on
        // the helper, which contributes no native touch).
        Instr::Bif { .. } => {
            if let Some(plan) = comparison_shape(module, instr) {
                touches.extend(plan.registers());
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
fn cacheable_writes(module: &Module, labels: &LabelMap, instr: &Instr) -> Vec<u32> {
    match instr {
        Instr::Move { src, dst } => move_shape(src, dst)
            .map(|shape| shape.writes())
            .unwrap_or_default(),
        // `swap A B` writes both operands.
        Instr::Swap { a, b } => {
            let (Some(a_x), Some(b_x)) = (x_register(a), x_register(b)) else {
                return Vec::new();
            };
            if a_x >= MAX_REGISTER || b_x >= MAX_REGISTER {
                return Vec::new();
            }
            vec![a_x, b_x]
        }
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
            } else {
                Vec::new()
            }
        }
        Instr::Bif { .. } => comparison_shape(module, instr)
            .map(|plan| plan.writes())
            .unwrap_or_default(),
        // `Test`/`SelectVal` branch but write no x-register; everything else deopts.
        _ => Vec::new(),
    }
}
