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

//! Unit tests for the block register cache's **dirty-set store model**: the per-`pc`
//! [`Allocation::spill_sets`] table, [`Allocation::spill_slots_at`], and the
//! [`spill`] store they drive.
//!
//! The model (documented in [`super::regalloc`] and [`super::regs`]): a cached x-register
//! only needs writing back to `machine.x` when an inline lowering has diverged its machine
//! register from the `machine.x` slot since the last syncing event (the prologue, or a
//! post-helper `fill`). So a spill stores exactly the registers **dirty at that `pc`** —
//! never the whole `written` set, except as a conservative fallback when no dirty table was
//! computed at all (a synthetic `Allocation` a unit test built directly).
//!
//! These tests pin the load-bearing transitions directly on
//! [`super::allocate_registers`]' output — no codegen, no driver: an inline write makes a
//! register dirty, an inline instruction that writes nothing leaves it dirty, and a
//! purely-helper instruction resets the set. The last test is a **consistency pin**: over
//! the committed fixture corpus, the mirror [`super::is_inlined`] (which decides the reset)
//! must agree with the emitter's actual static inline-vs-deopt decision, so a future edit to
//! `is_inlined` that mislabels a shape-based variant — the dangerous direction, a *wrong*
//! reset that would drop a dirty register — fails here.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use beam_loader::{Instr, Label, Operand};

use super::super::counters;
use super::super::tests::body_module;
use super::{allocate_registers, compile_function, is_inlined, spill, Allocation, SpillSite};

use crate::jit::emitter::CodeBuffer;

/// A `move 5 -> x0` into the register the tests then watch go dirty.
fn write_x0() -> Instr {
    Instr::Move {
        src: Operand::Integer(5),
        dst: Operand::X(0),
    }
}

/// The cached x-registers a spill at `pc` would store.
fn stored(allocation: &Allocation, pc: u32) -> Vec<u32> {
    allocation
        .spill_slots_at(pc)
        .into_iter()
        .map(|(x, _)| x)
        .collect()
}

/// An inline write stores once at the very next spill site; a second (helper) site and the
/// block exit store nothing.
#[test]
fn a_write_is_stored_once_and_the_helper_and_the_exit_store_nothing() {
    // Label, Label, `move 5 -> x0`, `on_load`, `on_load`, `return`.
    let (module, _) = body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        write_x0(),
        Instr::OnLoad,
        Instr::OnLoad,
        Instr::Return,
    ]);
    let program = &module.functions[0].code;
    let allocation = allocate_registers(&module, program, 0, 6);

    // x0 is the only register the block touches, so it is cached; the inline `move` makes
    // it dirty entering the first `on_load` (pc 3).
    let x0 = allocation.slot(0).expect("x0 is the block's only touched register");
    assert_eq!(allocation.slots, vec![(0, x0)]);
    assert_eq!(stored(&allocation, 3), vec![0], "the move makes x0 dirty at pc 3");

    // The first `on_load` is a purely-helper instruction: its own spill synced the cache, so
    // nothing is dirty afterwards — neither at the second helper (pc 4) nor at the exit (pc 6).
    assert!(allocation.spill_slots_at(4).is_empty());
    assert!(allocation.spill_slots_at(6).is_empty());

    // The three sites together emit exactly one store: the dirty pc 3.
    counters::set_enabled(true);
    counters::reset();
    let mut buffer = CodeBuffer::new().unwrap();
    spill(&mut buffer, &allocation, SpillSite::Helper, 3).unwrap();
    spill(&mut buffer, &allocation, SpillSite::Helper, 4).unwrap();
    spill(&mut buffer, &allocation, SpillSite::Exit, 6).unwrap();
    let snapshot = counters::snapshot();
    counters::set_enabled(false);

    assert_eq!(snapshot.spill_stores, 1, "only the dirty pc stores a register");
    assert_eq!(snapshot.spill_stores_by_site.get(&SpillSite::Helper), Some(&1));
    assert_eq!(snapshot.spill_stores_by_site.get(&SpillSite::Exit), Some(&0));
}

/// An inline instruction that writes no register does **not** reset the dirty set: the
/// exit-pc store set still contains the register an earlier write made dirty.
#[test]
fn an_inlined_no_write_instruction_does_not_reset_the_dirty_set() {
    // `int_code_end` is an inline metadata no-op (it writes nothing), unlike the helper
    // `on_load`; dropping the trailing `return` leaves a block whose exit pc is the last
    // instruction, so the surviving dirty set is directly observable there.
    let (module, _) = body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        write_x0(),
        Instr::IntCodeEnd,
    ]);
    let program = &module.functions[0].code;
    let allocation = allocate_registers(&module, program, 0, 4);

    // Dirty entering the no-op (pc 3) and still dirty at the pc after it (the exit pc 4).
    assert_eq!(stored(&allocation, 3), vec![0]);
    assert_eq!(
        stored(&allocation, 4),
        vec![0],
        "an inline no-write instruction must not clear the dirty set"
    );
}

/// A fully-deopted instruction **does** reset the dirty set: the store set after it is
/// empty, and the emitter recorded it as a static deopt.
#[test]
fn a_fully_deopted_instruction_resets_the_dirty_set() {
    // The same shape as the previous test with `on_load` (a pure helper) in place of the
    // inline no-op.
    let (module, _) = body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        write_x0(),
        Instr::OnLoad,
    ]);
    let program = &module.functions[0].code;
    let allocation = allocate_registers(&module, program, 0, 4);

    assert_eq!(
        stored(&allocation, 3),
        vec![0],
        "the write is still dirty entering the helper"
    );
    assert!(
        allocation.spill_slots_at(4).is_empty(),
        "the helper's own spill synced the cache, so the exit pc stores nothing"
    );

    // …and the mirror agrees: the emitter deopted this instruction (the reset is exact).
    let labels = crate::interpreter::build_label_map(&module);
    counters::set_enabled(true);
    counters::reset();
    crate::jit::cache::clear_baseline_cache();
    compile_function(&module, &labels, 0).expect("a helper-only body lowers");
    let snapshot = counters::snapshot();
    counters::set_enabled(false);

    assert!(
        snapshot.instr_deopts.get("OnLoad").copied().unwrap_or(0) > 0,
        "an on_load is a purely-helper instruction and must be a static deopt"
    );
}

/// An `Allocation` with no dirty table (a synthetic one, or one whose table was dropped)
/// conservatively falls back to the whole `written` set at every pc.
#[test]
fn an_allocation_without_a_dirty_table_falls_back_to_the_whole_written_set() {
    let (module, _) = body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        write_x0(),
        Instr::Return,
    ]);
    let program = &module.functions[0].code;
    let mut allocation = allocate_registers(&module, program, 0, program.len());

    let x0 = allocation.slot(0).expect("x0 is cached");
    let written_cache = vec![(0, x0)];
    assert_eq!(allocation.written, vec![0], "the move writes x0");

    // Drop the dirty analysis: every store then falls back to the whole `written` set.
    allocation.spill_sets.clear();
    for pc in 0..=program.len() as u32 {
        assert_eq!(
            allocation.spill_slots_at(pc),
            written_cache,
            "pc {pc} with no dirty table must store the whole written cache"
        );
    }
}

/// The **consistency pin**: `super::is_inlined` (the dirty-set walk's reset decision) must
/// agree with `emit_block`'s actual static inline-vs-deopt choice over the committed
/// fixture corpus. A variant the mirror models (it predicts at least one inline) may not be
/// predicted as a reset more often than the emitter actually deopted it — a *wrong* reset
/// (`is_inlined == false` for a shape the emitter INLINES) would drop a dirty register and
/// fail here.
#[test]
fn the_dirty_set_mirror_agrees_with_the_emitters_static_inline_decision() {
    // The corpus is exactly the committed fixtures — never the installed OTP tree (the OTP
    // sweep is the slow census's job, not this pin's). Discovered generically, no names.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("the fixture directory exists")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("beam"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "the fixture corpus must not be empty");

    crate::jit::cache::clear_baseline_cache();

    // Phase A — the prediction. With counters off, compile each function only to learn
    // whether the emitter can lower it at all (a function the emitter refuses is not a
    // source of instances this walk can account for), then walk the *blocks* it visits and
    // score the mirror.
    counters::set_enabled(false);
    counters::reset();

    let mut predicted_inlined: BTreeMap<String, u64> = BTreeMap::new();
    let mut predicted_reset: BTreeMap<String, u64> = BTreeMap::new();
    let mut modules: Vec<beam_loader::Module> = Vec::new();
    for path in &paths {
        let Ok(module) = beam_loader::load_file(path) else {
            continue;
        };
        let labels = crate::interpreter::build_label_map(&module);
        for function in &module.functions {
            let Some(func) = module
                .functions
                .iter()
                .position(|candidate| std::ptr::eq(candidate, function))
            else {
                continue;
            };
            // A function the emitter refuses cannot be walked against a full instance set.
            if compile_function(&module, &labels, func as u32).is_err() {
                continue;
            }
            let code = &function.code;
            for block in &function.blocks {
                let end = block.end.min(code.len());
                let mut pc = block.start;
                while pc < end {
                    let instr = &code[pc];
                    let name = counters::variant_name(instr);
                    if is_inlined(&module, &labels, instr) {
                        *predicted_inlined.entry(name).or_default() += 1;
                    } else {
                        *predicted_reset.entry(name).or_default() += 1;
                    }
                    // `emit_block` `break`s after a block-terminating instruction, so a
                    // walked prefix must stop there too (an instruction the emitter never
                    // reaches would inflate the prediction).
                    if ends_the_block(code, pc) {
                        break;
                    }
                    pc += 1;
                }
            }
        }
        modules.push(module);
    }
    assert!(!modules.is_empty(), "at least one fixture must load and lower");

    // Phase B — the emitter's own decision, recorded by the static census counters.
    counters::reset();
    counters::set_enabled(true);
    for module in &modules {
        let labels = crate::interpreter::build_label_map(module);
        for func in 0..module.functions.len() as u32 {
            // The counter is what we read; a refusal only leaves a partial prefix behind,
            // which can only make the counters larger (never smaller) than the walk.
            let _ = compile_function(module, &labels, func);
        }
    }
    let snapshot = counters::snapshot();
    counters::set_enabled(false);

    // (iv) The load-bearing direction, over exactly the variants `is_inlined` models with an
    // explicit **shape predicate** ([`is_shape_modelled`]). Each such variant's refusal must
    // coincide with a genuine static deopt — `is_inlined` returning `false` for a shape
    // `emit_block` actually INLINES is the dangerous drift, and it would silently drop a
    // dirty register. A variant the mirror reports as a reset more often than the emitter
    // statically deopted it fails here. The always-reset dedicated-helper arms (the receive
    // family, the frame trio, the heap family, the cross-module calls, the three local call
    // forms and the family-owned surface) are deliberately absent: they never reach
    // `emit_deopt`, so `is_inlined`'s `false` for them is not a deopt claim.
    //
    // Candidates come from the emitter's own accounting (`instr_instances`) as well as the
    // mirror's walks, so a variant the mirror mislabels to *always* reset (its
    // `predicted_inlined` drops to 0 and the fixture corpus happens to deopt none of it) is
    // still checked against `emit_block`'s real decision.
    let mut candidates: BTreeSet<&String> = BTreeSet::new();
    for variant in predicted_inlined.keys() {
        candidates.insert(variant);
    }
    for variant in predicted_reset.keys() {
        candidates.insert(variant);
    }
    for variant in snapshot.instr_instances.keys() {
        candidates.insert(variant);
    }

    let mut checked = 0usize;
    let mut non_vacuous: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for variant in &candidates {
        if !is_shape_modelled(variant) {
            continue;
        }
        let reset = predicted_reset.get(*variant).copied().unwrap_or(0);
        let deopts = snapshot.instr_deopts.get(*variant).copied().unwrap_or(0);
        assert!(
            deopts >= reset,
            "variant {variant}: the dirty-set mirror predicts {reset} resets but the emitter \
             statically deopted only {deopts} — `is_inlined` reports `false` for a shape \
             `emit_block` actually INLINES, which would silently drop a dirty register"
        );
        checked += 1;
        if reset > 0 && deopts > 0 {
            non_vacuous.insert((*variant).clone(), (reset, deopts));
        }
    }
    assert!(checked > 0, "the corpus must exercise at least one modelled variant");
    assert!(
        !non_vacuous.is_empty(),
        "the pin is vacuous unless some checked variant has BOTH a predicted reset and an \
         actual static deopt"
    );

    // (v) The once-walk is a lower bound on the emitter's visits: a block's instructions are
    // visited once by `emit_function` and again by any continuation sub-block, so the
    // predicted total can never exceed the recorded instance count.
    let variants: BTreeSet<&String> = predicted_inlined
        .keys()
        .chain(predicted_reset.keys())
        .collect();
    for variant in &variants {
        let predicted = predicted_inlined.get(*variant).copied().unwrap_or(0)
            + predicted_reset.get(*variant).copied().unwrap_or(0);
        let instances = snapshot.instr_instances.get(*variant).copied().unwrap_or(0);
        assert!(
            predicted <= instances,
            "variant {variant}: the once-walk counts {predicted} instructions but the emitter \
             visited only {instances} — the mirror walked an instruction the emitter never emits"
        );
    }
}

/// Whether [`super::emit_block`]'s dispatch `break`s **after** the instruction at `pc`.
///
/// The mirror of the emitter's block-terminating arms — the points after which it stops
/// iterating, so a walked prefix must stop there too. A `deallocate` only ends a block when
/// the emitter fuses an adjacent `return` into it (the loader's own superinstruction), which
/// skips that `return` — so the mirror must not visit it either.
fn ends_the_block(code: &[Instr], pc: usize) -> bool {
    match &code[pc] {
        Instr::Deallocate(_) => matches!(code.get(pc + 1), Some(Instr::Return)),
        Instr::DeallocateReturn(_)
        | Instr::Return
        | Instr::Jump { .. }
        | Instr::CallLast { .. }
        | Instr::CallOnly { .. }
        | Instr::Call { .. }
        | Instr::SelectVal { .. }
        | Instr::SelectTupleArity { .. } => true,
        _ => false,
    }
}

/// The variant names [`super::is_inlined`] models with an **explicit arm** — its
/// shape-predicate set plus the metadata no-ops. For every one of these, `false` is a claim
/// that [`super::emit_block`] statically deopted the instruction, so the emitter's
/// `instr_deopts` must cover the mirror's `predicted_reset` count. The always-reset
/// dedicated-helper arms (receive/frame/heap/cross-module/local-call/family-owned) are
/// deliberately **absent**: for them `is_inlined`'s `false` is a certain reset, not a deopt
/// claim.
///
/// Kept in lockstep with `is_inlined`'s match: a variant whose arm is removed there while
/// still named here (or whose *predicate* is made stricter than the emitter's) trips the
/// assertion. `Test(*)` names are matched by prefix because [`counters::variant_name`] keys a
/// `test` by its kind (`Test(IsTuple)`, …).
const SHAPE_MODELS: &[&str] = &[
    "Move",
    "Swap",
    "InitYregs",
    "GetList",
    "GetHd",
    "GetTl",
    "GetTupleElement",
    "SetTupleElement",
    "SelectTupleArity",
    "GcBif",
    "Bif",
    "SelectVal",
    "Jump",
    "Label",
    "Line",
    "IntCodeEnd",
    "ExecutableLine",
    "DebugLine",
];

/// Whether `variant` (a [`counters::variant_name`] key) is one `is_inlined` models with an
/// explicit shape-predicate arm — see [`SHAPE_MODELS`].
fn is_shape_modelled(variant: &str) -> bool {
    variant.starts_with("Test(") || SHAPE_MODELS.contains(&variant)
}
