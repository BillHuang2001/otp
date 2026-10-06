//! The baseline JIT's register allocator: which x-registers a basic block caches.
//!
//! [`allocate_registers`] ranks a block's x-registers by how often the block's
//! **inlinable** instructions would touch them, and the most-used
//! [`CACHE_REGISTERS`]`.len()` of them are held in machine registers (the prologue
//! loads them, every block exit spills them); every other register a block names is
//! addressed directly in `machine.x` — see [`super`].

use std::collections::BTreeMap;

use beam_loader::{Instr, Label, Module};

use crate::interpreter;
use crate::machine::MAX_REGISTER;

use super::arith::fast_arith;
use super::operands::{constant_word, x_register};
use super::select::select_val_plan;
use super::test::{test_plan, TestSrc};
use super::{Allocation, CACHE_REGISTERS};

/// Pick the x-registers a block caches.
///
/// The block keeps the [`CACHE_REGISTERS`]`.len()` x-registers its native lowering
/// **touches the most**: every cacheable instruction (a `move` whose shape
/// [`super::r#move::try_inline_move`] lowers, a `swap` over two addressable x-registers, a `test`
/// whose shape [`super::test::test_plan`] lowers, and a `gc_bif` [`fast_arith`] lowers)
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
    labels: &BTreeMap<Label, (u32, u32)>,
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
/// is the scrutinee), and a `gc_bif` only when [`fast_arith`] does. Every other
/// instruction contributes nothing because it stays on the helper.
fn cacheable_touches(
    module: &Module,
    labels: &BTreeMap<Label, (u32, u32)>,
    instr: &Instr,
) -> Vec<u32> {
    let mut touches = Vec::new();
    match instr {
        Instr::Move { src, dst } => {
            let Some(dst_x) = x_register(dst) else {
                return touches;
            };
            if dst_x >= MAX_REGISTER {
                return touches;
            }
            match x_register(src) {
                Some(src_x) => {
                    if src_x >= MAX_REGISTER {
                        return touches;
                    }
                    touches.push(src_x);
                }
                // A non-register source must be a constant word, else the move deopts.
                None => {
                    if constant_word(src).is_none() {
                        return touches;
                    }
                }
            }
            touches.push(dst_x);
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
        Instr::GcBif { .. } => {
            if let Some(fast) = fast_arith(module, instr) {
                touches.extend(fast.registers());
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
