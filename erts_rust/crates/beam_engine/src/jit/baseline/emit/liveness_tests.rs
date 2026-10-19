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

//! Pins for the block register cache's **fill model**: the `pc`-local join rule
//! [`super::liveness::fill_sets`] applies on top of block-local liveness.
//!
//! A post-helper `fill` reloads `(live ∪ writes(pc)) ∩ slots`
//! ([`Allocation::fill_slots`](super::Allocation::fill_slots)). Two independent claims:
//!
//! * **which registers enter the cache at all** — only registers an *inlined* shape
//!   touches ([`super::regalloc::cacheable_touches`]). A block whose x-register references
//!   are *all* family/bitstring helper arms caches **none** of them: the helper reads and
//!   writes `machine.x` directly, so a slot for it could only ever cost a store and a load.
//!   A register an inline shape also touches keeps its slot.
//! * **which cached registers a helper refills** — liveness (a later inline read) plus the
//!   write set of the instruction *at* that `pc` (the join rule that keeps a guarded
//!   lowering's `jit_step_one` fallback in sync). A register the block writes *elsewhere* is
//!   neither, so it is no longer reloaded at every helper site — which is the whole point:
//!   a bitstring decoder issues ~93 helper calls per `to_lower/1` call, and the block-global
//!   write set reloaded every register the block ever wrote at each of them.
//!
//! The last test is the **real-`.beam`** pin: on committed `erlc` output the refill traffic
//! is strictly below the full-cache model *and* the two tiers still agree.

use beam_common::Result;
use beam_loader::{load_file, Instr, Label, Module, Operand};
use beam_term::Atom;

use super::super::tests::{assert_same_outcome, body_module};
use super::{allocate_registers, Allocation};

use crate::context::MinimalContext;
use crate::jit::Tier;
use crate::machine::{Machine, RunOutcome};
use crate::terms::make_heap_binary;

/// The cached x-register indices a `fill` at `pc` reloads, in slot order.
fn refilled(allocation: &Allocation, pc: u32) -> Vec<u32> {
    allocation.fill_slots(pc).into_iter().map(|(x, _)| x).collect()
}

/// The cached x-register indices the block holds.
fn cached(allocation: &Allocation) -> Vec<u32> {
    allocation.slots.iter().map(|(x, _)| *x).collect()
}

/// A family-owned instruction: `put_list` is executed by the family seam, so it reads and
/// writes `machine.x` directly and touches no cached register.
fn put_list(head: Operand, dst: Operand) -> Instr {
    Instr::PutList {
        head,
        tail: Operand::Nil,
        dst,
    }
}

/// `move s -> d`, an **inlined** shape (the cacheable counterpart of [`put_list`]).
fn move_x(src: Operand, dst: Operand) -> Instr {
    Instr::Move { src, dst }
}

/// **(a) A block whose only x-register references are helper-arm instructions caches none
/// of them.**
///
/// `put_list` is family-owned and `on_load` is fully deopted, so neither contributes a
/// cacheable touch: the block's cache is empty, every fill set is empty and the prologue
/// loads nothing. A slot here could only cost a store before the helper and a reload after.
#[test]
fn a_helper_only_block_caches_no_register() {
    let (module, _) = body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        put_list(Operand::X(0), Operand::X(1)),
        Instr::OnLoad,
        Instr::Return,
    ]);
    let code = &module.functions[0].code;
    let allocation = allocate_registers(&module, code, 0, code.len() as usize);

    assert!(
        allocation.slots.is_empty(),
        "a block whose x-register references are all helper arms must cache nothing; \
         slots: {:?}",
        allocation.slots,
    );
    for pc in 0..code.len() as u32 {
        assert!(
            refilled(&allocation, pc).is_empty(),
            "a slot-less block refills nothing, at any pc (pc {pc})",
        );
    }
    assert!(
        allocation.entry_len() == 0,
        "and the prologue loads nothing either",
    );
}

/// **(b) A register an inlined shape also touches keeps its slot.**
///
/// The same helper-arm block plus one inline `move x(1) -> x(2)`: `x(1)` and `x(2)` are
/// native touches of the block, so both are cached — the helper arm does not disqualify
/// them, it simply does not contribute any itself.
#[test]
fn a_register_an_inline_shape_touches_keeps_its_slot() {
    let (module, _) = body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        put_list(Operand::X(0), Operand::X(1)),
        move_x(Operand::X(1), Operand::X(2)),
        Instr::Return,
    ]);
    let code = &module.functions[0].code;
    let allocation = allocate_registers(&module, code, 0, code.len() as usize);

    let slots = cached(&allocation);
    assert!(
        slots.contains(&1) && slots.contains(&2),
        "the inline `move`'s source and destination must be cached; slots: {slots:?}",
    );
    // The helper arm's *other* x-register (`x(0)`, read by `put_list` only) is not.
    assert!(
        !slots.contains(&0),
        "a register only a helper arm reads stays uncached; slots: {slots:?}",
    );
}

/// **(c) The join rule is `pc`-local: a register the block writes elsewhere is not
/// refilled at a helper site — but liveness still refills it.**
///
/// Both bodies write `x(0)` with an inline `move` before a `put_list` helper. In the first
/// nothing reads `x(0)` afterwards, so it is dead at the helper and (though it is in the
/// block's write set, and cached) it is not refilled there. In the second a later inline
/// `move` reads it, which makes it live at the helper, so it *is* refilled — the case whose
/// omission would read a stale machine register.
#[test]
fn the_join_rule_is_pc_local_not_block_global() {
    // pc: 0 Label(1) · 1 Label(2) · 2 `move x(3) -> x(0)` · 3 `put_list` · 4 Return / move.
    const HELPER_PC: u32 = 3;
    for (label, tail, expect_refill) in [
        (
            "dead after the helper",
            Instr::Return,
            false,
        ),
        (
            "read after the helper",
            move_x(Operand::X(0), Operand::X(2)),
            true,
        ),
    ] {
        let (module, _) = body_module(vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            move_x(Operand::X(3), Operand::X(0)),
            put_list(Operand::X(3), Operand::X(1)),
            tail,
            Instr::Return,
        ]);
        let code = &module.functions[0].code;
        let allocation = allocate_registers(&module, code, 0, code.len() as usize);

        assert!(
            matches!(code[HELPER_PC as usize], Instr::PutList { .. }),
            "{label}: the pin's pc arithmetic must name the helper; code: {code:?}",
        );
        assert!(
            allocation.is_written(0) && allocation.slot(0).is_some(),
            "{label}: x(0) must be a cached register of the block's *write set*, or the \
             pin proves nothing; slots: {:?} written: {:?}",
            allocation.slots,
            allocation.written,
        );
        let fill = refilled(&allocation, HELPER_PC);
        if expect_refill {
            assert!(
                fill.contains(&0),
                "{label}: x(0) is read after the helper, so it must be live there and \
                 refilled; fill: {fill:?}",
            );
        } else {
            assert!(
                !fill.contains(&0),
                "{label}: x(0) is dead at the helper and not written by it, so refilling \
                 it is pure traffic; fill: {fill:?}",
            );
        }
    }
}

/// **(c′) The join rule's `writes(pc)` term is retained.**
///
/// `swap {y,0} {x,1}` is an inlined read-modify-write with a `Y` side, so it can reject to
/// `jit_step_one` at its own `pc` — and x(1)'s machine register is authoritative on the
/// fallback while the cache register is authoritative on the fast path. The fill set of
/// that `pc` therefore has to list x(1) from the **`pc`-local write set**, even though no
/// later instruction reads it (nothing makes it live). This is the case the narrowing must
/// not remove.
#[test]
fn a_guarded_inline_shapes_own_pc_refills_its_write_set() {
    // pc: 0 Label(1) · 1 Label(2) · 2 `swap {y,0} {x,1}` · 3 Return.
    const SWAP_PC: u32 = 2;
    let (module, _) = body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Swap {
            a: Operand::Y(0),
            b: Operand::X(1),
        },
        Instr::Return,
    ]);
    let code = &module.functions[0].code;
    let allocation = allocate_registers(&module, code, 0, code.len() as usize);

    assert!(
        matches!(code[SWAP_PC as usize], Instr::Swap { .. }),
        "the pin's pc arithmetic must name the swap; code: {code:?}",
    );
    assert!(
        allocation.slot(1).is_some(),
        "x(1) must be cached, or the join rule has nothing to keep in sync; slots: {:?}",
        allocation.slots,
    );
    let fill = refilled(&allocation, SWAP_PC);
    assert!(
        fill.contains(&1),
        "the swap's own write set must be refilled at its `pc` (the guarded fallback's \
         join); fill: {fill:?}",
    );
    // `x(1)` is not live after the swap — the term that puts it in the set is the local
    // write set, not liveness. The block-global write set would instead list it at every
    // later pc, which is the traffic this lever removes: `SWAP_PC + 1` is the `return`,
    // which neither writes nor is followed by a read of x(1).
    assert!(
        refilled(&allocation, SWAP_PC + 1).is_empty(),
        "x(1) is neither live nor written at pc {}, so the *block-global* write set must \
         not list it there; fill: {:?}",
        SWAP_PC + 1,
        refilled(&allocation, SWAP_PC + 1),
    );
}

/// Load one of the committed `erlc` fixtures (`tests/fixtures/*.beam`).
fn fixture(name: &str) -> Module {
    let path = format!("{}/tests/fixtures/{name}.beam", env!("CARGO_MANIFEST_DIR"));
    load_file(&path).unwrap_or_else(|error| panic!("load {path}: {error:?}"))
}

/// Run exported `name/1` with `bytes` on both tiers and hand the two outcomes to `check`.
///
/// Each side builds its own heap (a returned boxed term points into it), so the comparison
/// happens *inside* this call.
fn with_both_tiers_bytes<R>(
    module: &Module,
    name: Atom,
    bytes: &[u8],
    check: impl FnOnce(&Result<RunOutcome>, &Result<RunOutcome>) -> R,
) -> R {
    let mut interpreter = Machine::new();
    let mut interpreter_ctx = MinimalContext::new();
    let value = make_heap_binary(&mut interpreter_ctx, bytes).expect("the argument builds");
    let interpreter_outcome = interpreter.run_export_outcome_with_tier(
        module,
        name,
        1,
        &[value],
        &mut interpreter_ctx,
        Tier::Interpreter,
    );
    let mut baseline = Machine::new();
    let mut baseline_ctx = MinimalContext::new();
    let value = make_heap_binary(&mut baseline_ctx, bytes).expect("the argument builds");
    let baseline_outcome = baseline.run_export_outcome_with_tier(
        module,
        name,
        1,
        &[value],
        &mut baseline_ctx,
        Tier::BaselineJit,
    );
    check(&interpreter_outcome, &baseline_outcome)
}

/// **(d) On real `.beam` decoder code the refill traffic is strictly below the full-cache
/// model, and the two tiers still agree.**
///
/// The corpus half is the deterministic half: over every block of the committed bitstring
/// fixtures, `Σ_pc fill_slots(pc).len()` must be strictly smaller than the full-cache
/// alternative `Σ_pc slots.len()` those blocks used to reload — and the blocks must carry
/// real cache traffic (a fixture set with no cached register would satisfy the inequality
/// vacuously). The run half pins that the narrowed refill is still **interpreter-identical**
/// on a real decoder (`bitstrings:first8/1`, whose block is the `bs_start_match3` +
/// `bs_get_integer2` shape).
#[test]
fn real_decoder_refills_are_narrower_than_the_full_cache_model_and_agree_across_tiers() {
    let mut narrowed = 0usize;
    let mut full = 0usize;
    let mut blocks_with_slots = 0usize;
    for name in ["bitstrings", "bsmatch", "bscreate", "bssmall", "bsstring"] {
        let module = fixture(name);
        for function in &module.functions {
            let code = &function.code;
            for block in &function.blocks {
                let (start, end) = (block.start, block.end.min(code.len()));
                if start >= end {
                    continue;
                }
                let allocation = allocate_registers(&module, code, start, end);
                if allocation.slots.is_empty() {
                    continue;
                }
                blocks_with_slots += 1;
                for pc in start..end {
                    narrowed += allocation.fill_slots(pc as u32).len();
                    full += allocation.slots.len();
                }
            }
        }
    }
    assert!(
        blocks_with_slots > 20,
        "the fixture set must carry real cache traffic (only {blocks_with_slots} blocks \
         hold a cached register)",
    );
    assert!(
        narrowed < full,
        "the narrowed fill model must refill strictly fewer registers than the full cache \
         over real decoder code: {narrowed} vs {full}",
    );

    // `<<1,2,3,4,5,6,7,8,...>>` — `first8/1` matches one byte and returns it.
    let module = fixture("bitstrings");
    let name = module
        .atom_table
        .lookup("first8")
        .expect("bitstrings:first8/1 must be exported");
    with_both_tiers_bytes(&module, name, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11], |a, b| {
        assert_same_outcome("bitstrings:first8/1", a, b, &module.atom_table);
        let RunOutcome::Returned(value) = b.as_ref().expect("the baseline run completes")
        else {
            panic!("bitstrings:first8/1: unexpected outcome");
        };
        assert_eq!(
            beam_term::signed_value(*value),
            1,
            "bitstrings:first8/1 answered the wrong value",
        );
    });
}
