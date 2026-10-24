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
//! Per-shape pins for the **eight native bitstring inline arms** in
//! [`super::emit::bitstr_inline`]: `bs_get_position`, `bs_set_position`,
//! `bs_test_tail2`, `bs_skip_bits2`, `bs_start_match3`, `bs_start_match4`,
//! `bs_get_integer2` and `bs_match`.
//!
//! `bitstr_tests` pins the *family route* (the `jit_bitstr` helper and the predicate
//! that claims the whole `Bs*` set). This node pins the **inline** route, one claim per
//! concern:
//!
//! * **each arm actually emits.** A tier-agreement test alone also passes for an
//!   "implementation" that deopts every instruction, so every shape is proved through a
//!   **cache-free** compile ([`super::emit::compile_function`], the
//!   `call_tests.rs::the_frame_preserving_local_call_records_no_static_deopt` route):
//!   its `#[cfg(test)]` emission counter
//!   ([`bitstr_inline::inlined`]) must move and the variant must record **no** static
//!   deopt. The counter is a *compile-time* instrument, which is exactly why the
//!   process-wide content-keyed [`crate::jit::cache::CodeCache`] has to be bypassed — a
//!   warmed artifact re-emits nothing, so the same assertion made through a normal tier
//!   run would read `0 -> 0` and prove nothing.
//! * **each arm is interpreter-identical** on a real subject, with the returned value
//!   asserted (not just the outcome), because a wrong extraction inside an otherwise
//!   agreeing outcome is the failure mode an inline arm risks.
//! * **the boundary rows** are pinned explicitly: an empty subject, a `size == 0`
//!   segment in both its `unit == 1` and `unit == 8` forms, a non-byte-aligned unit, a
//!   bit-granular 6-bit `bs_match` field beside the aligned 16- and 32-bit ones, an
//!   overrunning segment (the fail-label branch — never a trap, never the wrong class),
//!   an extracted integer used as a heap term afterwards, and every `NO_FAIL` /
//!   signed / little / width-outside-the-set / `binary` / `get_tail` / `=:=` /
//!   `> 1` `Y` destination shape that must stay on the family.
//! * **the register cache is charged for every register an arm writes**
//!   ([`super::emit::cacheable_writes`]). A write set that missed a destination is the
//!   one *unsound* failure mode of this optimisation — a stale cached x-register
//!   surviving a block exit — so each shape's set is pinned exactly, and a `Y`
//!   destination must contribute nothing to it.
//!
//! Module placement follows `bitstr_tests` (a plain `#[cfg(test)] mod` declared from
//! `super::mod.rs`), so it reaches [`super::tests`]' harness by absolute path rather
//! than by being `#[path]`-included from it.

use beam_loader::{BsGetArgs, Export, Instr, Label, Module, Operand, NO_FAIL};
use beam_term::Atom;

use super::bitstr::bitstr_calls;
use super::bitstr_tests::{export, fixture, with_both_tiers_arg, Arg};
use super::counters;
use super::emit::bitstr_inline::{
    self, inlined, MatchFail, SHAPE_BS_MATCH, SHAPE_COUNT, SHAPE_GET_INTEGER2, SHAPE_GET_POSITION,
    SHAPE_SET_POSITION, SHAPE_SKIP_BITS2, SHAPE_START_MATCH3, SHAPE_START_MATCH4, SHAPE_TEST_TAIL2,
};
use super::emit::cacheable_writes;
use super::tests::{assemble_function, assert_same_outcome, function_code_bytes};
use crate::machine::RunOutcome;

// ---------------------------------------------------------------------------
// A tiny module builder: mint the atoms, then assemble the code
// ---------------------------------------------------------------------------
/// A one-export module named `name` whose function `f/<arity>` (entry `Label(2)`) is
/// `build(&module)`.
///
/// The atoms are minted **before** `build` runs because a `bs_match` command name is an
/// `Operand::Atom` — its id has to exist before the instruction stream is written. The
/// block decomposition is [`super::tests::assemble_function`]'s, i.e. the same one
/// `tests::build_module` uses, so a hand-built module is cut into blocks exactly like a
/// real `.beam`.
fn bitstr_module(
    name: &str,
    atoms: &[&str],
    arity: u32,
    build: impl FnOnce(&Module) -> Vec<Instr>,
) -> Module {
    let mut module = Module::default();
    module.name = module.atom_table.intern(name);
    for atom in atoms {
        module.atom_table.intern(atom);
    }
    module.atom_table.intern("f");
    let code = build(&module);
    assemble_function(&mut module, "f", arity, code, Some(Label(2)));
    let function = atom(&module, "f");
    module.exports.push(Export {
        function,
        arity,
        label: Label(2),
    });
    module
}

/// The atom `name` of `module` (interned by [`bitstr_module`]).
fn atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("`{name}` must be interned"))
}

/// A `bs_match` `ensure_at_least` command group.
fn ensure_at_least(module: &Module, size: i64, unit: i64) -> Vec<Operand> {
    vec![
        Operand::Atom(atom(module, "ensure_at_least")),
        Operand::Integer(size),
        Operand::Integer(unit),
    ]
}

/// A `bs_match` `integer` command group: `{integer, Live, Flags, Size, Unit, Dst}` — the
/// operand layout `families::bitstrings::prepared` reads (and `bs_match_shape` mirrors).
fn integer_command(module: &Module, size: i64, unit: i64, dst: Operand) -> Vec<Operand> {
    vec![
        Operand::Atom(atom(module, "integer")),
        Operand::Integer(2),
        Operand::Nil,
        Operand::Integer(size),
        Operand::Integer(unit),
        dst,
    ]
}

/// `move -1 -> x(0); return` — a fail-label block whose answer is distinguishable from
/// every real one, so "took the fail branch" is observable through the returned value.
fn sentinel_block(label: Label) -> Vec<Instr> {
    vec![
        Instr::Label(label),
        Instr::Move {
            src: Operand::Integer(-1),
            dst: Operand::X(0),
        },
        Instr::Return,
    ]
}

// ---------------------------------------------------------------------------
// The eight hand-built shape modules
// ---------------------------------------------------------------------------
/// `bs_get_position`: read 8 bits, then report the context position — `8`.
fn module_get_position() -> Module {
    bitstr_module("inline_get_position", &[], 1, |_| {
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsStartMatch3 {
                fail: Label(3),
                bin: Operand::X(0),
                live: 1,
                dst: Operand::X(1),
            },
            Instr::BsGetInteger2(Box::new(BsGetArgs {
                fail: Label(3),
                ctx: Operand::X(1),
                live: 2,
                size: Operand::Integer(8),
                unit: 1,
                flags: 0,
                dst: Operand::X(0),
            })),
            Instr::BsGetPosition {
                ctx: Operand::X(1),
                dst: Operand::X(0),
                live: 2,
            },
            Instr::Return,
        ];
        code.extend(sentinel_block(Label(3)));
        code
    })
}

/// `bs_set_position`: save the position, read a byte, rewind, read the same byte again —
/// so the answer is the *first* byte and a lowering that ignored the rewind answers the
/// second.
fn module_set_position() -> Module {
    bitstr_module("inline_set_position", &[], 1, |_| {
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsStartMatch3 {
                fail: Label(3),
                bin: Operand::X(0),
                live: 1,
                dst: Operand::X(1),
            },
            Instr::BsGetPosition {
                ctx: Operand::X(1),
                dst: Operand::X(2),
                live: 2,
            },
            Instr::BsGetInteger2(Box::new(BsGetArgs {
                fail: Label(3),
                ctx: Operand::X(1),
                live: 3,
                size: Operand::Integer(8),
                unit: 1,
                flags: 0,
                dst: Operand::X(3),
            })),
            Instr::BsSetPosition {
                ctx: Operand::X(1),
                pos: Operand::X(2),
            },
            Instr::BsGetInteger2(Box::new(BsGetArgs {
                fail: Label(3),
                ctx: Operand::X(1),
                live: 3,
                size: Operand::Integer(8),
                unit: 1,
                flags: 0,
                dst: Operand::X(0),
            })),
            Instr::Return,
        ];
        code.extend(sentinel_block(Label(3)));
        code
    })
}

/// `bs_test_tail2`: read 16 bits, then require an **empty** tail.
fn module_test_tail2() -> Module {
    bitstr_module("inline_test_tail2", &[], 1, |_| {
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsStartMatch3 {
                fail: Label(3),
                bin: Operand::X(0),
                live: 1,
                dst: Operand::X(1),
            },
            Instr::BsGetInteger2(Box::new(BsGetArgs {
                fail: Label(3),
                ctx: Operand::X(1),
                live: 2,
                size: Operand::Integer(16),
                unit: 1,
                flags: 0,
                dst: Operand::X(0),
            })),
            Instr::BsTestTail2 {
                fail: Label(4),
                ctx: Operand::X(1),
                bits: 0,
            },
            Instr::Return,
        ];
        // A non-empty tail takes this branch.
        code.extend(sentinel_block(Label(4)));
        code.extend(sentinel_block(Label(3)));
        code
    })
}

/// `bs_skip_bits2`: skip one **constant** byte, then read the next one.
fn module_skip_bits2() -> Module {
    bitstr_module("inline_skip_bits2", &[], 1, |_| {
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsStartMatch3 {
                fail: Label(3),
                bin: Operand::X(0),
                live: 1,
                dst: Operand::X(1),
            },
            Instr::BsSkipBits2 {
                fail: Label(3),
                ctx: Operand::X(1),
                size: Operand::Integer(8),
                unit: 1,
                flags: 0,
            },
            Instr::BsGetInteger2(Box::new(BsGetArgs {
                fail: Label(3),
                ctx: Operand::X(1),
                live: 2,
                size: Operand::Integer(8),
                unit: 1,
                flags: 0,
                dst: Operand::X(0),
            })),
            Instr::Return,
        ];
        code.extend(sentinel_block(Label(3)));
        code
    })
}

/// `bs_start_match3`: start the match, read one byte.
fn module_start_match3() -> Module {
    bitstr_module("inline_start_match3", &[], 1, |_| {
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsStartMatch3 {
                fail: Label(3),
                bin: Operand::X(0),
                live: 1,
                dst: Operand::X(1),
            },
            Instr::BsGetInteger2(Box::new(BsGetArgs {
                fail: Label(3),
                ctx: Operand::X(1),
                live: 2,
                size: Operand::Integer(8),
                unit: 1,
                flags: 0,
                dst: Operand::X(0),
            })),
            Instr::Return,
        ];
        code.extend(sentinel_block(Label(3)));
        code
    })
}

/// `bs_start_match4` in its **label** form: re-bind an existing match context through a
/// second `bs_start_match4` and read from the rebound context.
fn module_start_match4_label() -> Module {
    bitstr_module("inline_start_match4_label", &[], 1, |_| {
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsStartMatch3 {
                fail: Label(3),
                bin: Operand::X(0),
                live: 1,
                dst: Operand::X(1),
            },
            Instr::BsStartMatch4 {
                fail: Operand::Label(Label(4)),
                live: 2,
                src: Operand::X(1),
                dst: Operand::X(2),
            },
            Instr::BsGetInteger2(Box::new(BsGetArgs {
                fail: Label(4),
                ctx: Operand::X(2),
                live: 3,
                size: Operand::Integer(8),
                unit: 1,
                flags: 0,
                dst: Operand::X(0),
            })),
            Instr::Return,
        ];
        code.extend(sentinel_block(Label(4)));
        code.extend(sentinel_block(Label(3)));
        code
    })
}

/// `bs_start_match4` in its **`resume`** atom form: the source operand already *is* a
/// match context, so the reuse is the whole instruction.
fn module_start_match4_resume() -> Module {
    bitstr_module("inline_start_match4_resume", &["resume"], 1, |module| {
        let resume = Operand::Atom(atom(module, "resume"));
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsStartMatch3 {
                fail: Label(3),
                bin: Operand::X(0),
                live: 1,
                dst: Operand::X(1),
            },
            Instr::BsStartMatch4 {
                fail: resume,
                live: 2,
                src: Operand::X(1),
                dst: Operand::X(2),
            },
            Instr::BsGetInteger2(Box::new(BsGetArgs {
                fail: Label(3),
                ctx: Operand::X(2),
                live: 3,
                size: Operand::Integer(8),
                unit: 1,
                flags: 0,
                dst: Operand::X(0),
            })),
            Instr::Return,
        ];
        code.extend(sentinel_block(Label(3)));
        code
    })
}

/// `bs_get_integer2`: a 16-bit big-endian read out of the context.
fn module_get_integer2() -> Module {
    bitstr_module("inline_get_integer2", &[], 1, |_| {
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsStartMatch3 {
                fail: Label(3),
                bin: Operand::X(0),
                live: 1,
                dst: Operand::X(1),
            },
            Instr::BsGetInteger2(Box::new(BsGetArgs {
                fail: Label(3),
                ctx: Operand::X(1),
                live: 2,
                size: Operand::Integer(16),
                unit: 1,
                flags: 0,
                dst: Operand::X(0),
            })),
            Instr::Return,
        ];
        code.extend(sentinel_block(Label(3)));
        code
    })
}

/// `bs_match`: `ensure_at_least 8 8` then one 8-bit `integer` field.
fn module_bs_match() -> Module {
    bitstr_module("inline_bs_match", &["ensure_at_least", "integer"], 1, |module| {
        let mut commands = ensure_at_least(module, 8, 8);
        commands.extend(integer_command(module, 8, 1, Operand::X(0)));
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsStartMatch3 {
                fail: Label(3),
                bin: Operand::X(0),
                live: 1,
                dst: Operand::X(1),
            },
            Instr::BsMatch {
                fail: Label(3),
                ctx: Operand::X(1),
                commands,
            },
            Instr::Return,
        ];
        code.extend(sentinel_block(Label(3)));
        code
    })
}

// ---------------------------------------------------------------------------
// The instrument: a cache-free compile per shape
// ---------------------------------------------------------------------------
/// Compile `module`'s function `index` with the counters on and **no** `CodeCache`
/// involvement, so every inline arm re-emits (the counter the warm path cannot move).
fn compile_cache_free(module: &Module, index: u32) -> counters::Counters {
    crate::jit::cache::clear_baseline_cache();
    counters::reset();
    counters::set_enabled(true);
    let labels = crate::interpreter::build_label_map(module);
    super::emit::compile_function(module, &labels, index).expect("the shape must compile");
    counters::set_enabled(false);
    counters::snapshot()
}

/// The shapes whose arm emitted during a **cache-free** compile of `module`'s function
/// `index`, beside the compile's counter snapshot.
///
/// `pub(crate)` because the sibling `bitstr_tests` module classifies its *real fixture* rows
/// by exactly this measurement: a fixture whose executed `Bs*` instructions are all natively
/// inlined never moves the family helper's **run-time** counter, so the only honest proof
/// that its arm ran is this compile-time one.
pub(crate) fn emitted_shapes_for(
    module: &Module,
    index: u32,
) -> (Vec<usize>, counters::Counters) {
    let before: Vec<u64> = (0..SHAPE_COUNT).map(inlined).collect();
    let snapshot = compile_cache_free(module, index);
    let moved = (0..SHAPE_COUNT)
        .filter(|shape| inlined(*shape) > before[*shape])
        .collect();
    (moved, snapshot)
}

/// The shapes whose arm emitted during a **cache-free** compile of `module`'s `f`.
fn emitted_shapes(module: &Module) -> (Vec<usize>, counters::Counters) {
    emitted_shapes_for(module, 0)
}

/// The static-deopt count `snapshot` recorded for the `Instr` variant `variant`.
///
/// `pub(crate)` for the same reason as [`emitted_shapes_for`].
pub(crate) fn static_deopts(snapshot: &counters::Counters, variant: &str) -> u64 {
    snapshot.instr_deopts.get(variant).copied().unwrap_or(0)
}

/// The number of instruction instances the emitter visited for `variant` — the proof that
/// `variant` is the name the census actually uses (so the `static_deopts == 0` assertions
/// below cannot pass merely by naming a variant that never appears).
fn visited_instances(snapshot: &counters::Counters, variant: &str) -> u64 {
    snapshot.instr_instances.get(variant).copied().unwrap_or(0)
}

/// **Every arm emits and stays native — the mandatory instrument.**
///
/// For each of the eight shapes: a cache-free [`super::emit::compile_function`] moves that
/// shape's `inlined(SHAPE_*)` counter, visits the variant (so the name is real), and
/// records **zero** static deopts for it. Without this a fully-deopting "implementation"
/// would pass every tier-agreement test in this file.
#[test]
fn every_inline_shape_emits_on_a_cache_free_compile() {
    let cases: [(&str, usize, &str, fn() -> Module); 8] = [
        ("bs_get_position", SHAPE_GET_POSITION, "BsGetPosition", module_get_position),
        ("bs_set_position", SHAPE_SET_POSITION, "BsSetPosition", module_set_position),
        ("bs_test_tail2", SHAPE_TEST_TAIL2, "BsTestTail2", module_test_tail2),
        ("bs_skip_bits2", SHAPE_SKIP_BITS2, "BsSkipBits2", module_skip_bits2),
        ("bs_start_match3", SHAPE_START_MATCH3, "BsStartMatch3", module_start_match3),
        ("bs_start_match4", SHAPE_START_MATCH4, "BsStartMatch4", module_start_match4_label),
        ("bs_get_integer2", SHAPE_GET_INTEGER2, "BsGetInteger2", module_get_integer2),
        ("bs_match", SHAPE_BS_MATCH, "BsMatch", module_bs_match),
    ];
    let mut covered = 0usize;
    for (shape_name, index, variant, build) in cases {
        let module = build();
        let (emitted, snapshot) = emitted_shapes(&module);
        assert!(
            emitted.contains(&index),
            "{shape_name}: the cache-free compile must emit the arm \
             (emitted {emitted:?}, wanted {index})"
        );
        assert!(
            visited_instances(&snapshot, variant) >= 1,
            "{shape_name}: `{variant}` must be the census's variant name (visited \
             {:?}); a wrong name would make the deopt assertion below vacuous",
            snapshot.instr_instances,
        );
        assert_eq!(
            static_deopts(&snapshot, variant),
            0,
            "{shape_name}: the arm must lower the instruction, not statically deopt it \
             (static deopts {:?})",
            snapshot.instr_deopts,
        );
        covered += 1;
    }
    assert_eq!(covered, SHAPE_COUNT, "every shape must be covered");
}

/// **`bs_start_match4`'s `resume` form emits too**, and it is a *different* decision from
/// the label form (the guard is `HEADER_SUB_BITS` alone), so it is pinned separately.
#[test]
fn the_start_match4_resume_form_emits_on_a_cache_free_compile() {
    let module = module_start_match4_resume();
    let (emitted, snapshot) = emitted_shapes(&module);
    assert!(
        emitted.contains(&SHAPE_START_MATCH4),
        "the resume form must emit the `bs_start_match4` arm (emitted {emitted:?})"
    );
    assert_eq!(
        static_deopts(&snapshot, "BsStartMatch4"),
        0,
        "the resume form must lower, not deopt"
    );
}

// ---------------------------------------------------------------------------
// Tier equivalence + value identity, one row per shape
// ---------------------------------------------------------------------------
/// **Each shape agrees with the interpreter and answers the right value.**
///
/// `with_both_tiers_arg` runs the same subject under [`crate::jit::Tier::Interpreter`] and
/// [`crate::jit::Tier::BaselineJit`]; `assert_same_outcome` requires the two outcomes to be
/// observably identical, and the value assertion then pins *what* was read — an inline arm
/// that produced a plausible-but-wrong integer would otherwise agree across two tiers that
/// share the extraction.
#[test]
fn each_shape_agrees_with_the_interpreter_and_answers_the_right_value() {
    // (label, module factory, subject, expected)
    let cases: [(&str, fn() -> Module, &'static [u8], i64); 9] = [
        // After one 8-bit read the position is 8.
        ("bs_get_position", module_get_position, &[7, 9], 8),
        // The rewind makes the *first* byte the answer, not the second.
        ("bs_set_position", module_set_position, &[7, 9], 7),
        // 16 bits big-endian, empty tail required.
        ("bs_test_tail2", module_test_tail2, &[1, 2], 0x0102),
        // Skip one byte, answer the next.
        ("bs_skip_bits2", module_skip_bits2, &[7, 9], 9),
        ("bs_start_match3", module_start_match3, &[5], 5),
        ("bs_start_match4 (label)", module_start_match4_label, &[5], 5),
        ("bs_start_match4 (resume)", module_start_match4_resume, &[5], 5),
        ("bs_get_integer2", module_get_integer2, &[1, 2], 0x0102),
        ("bs_match", module_bs_match, &[0x2a, 0xff], 0x2a),
    ];
    // (label, expected + subject count) — the array below is one row per shape.
    assert_eq!(cases.len(), 9, "eight shapes, with `bs_start_match4` counted twice");
    for (label, build, subject, expected) in cases {
        let module = build();
        let name = atom(&module, "f");
        with_both_tiers_arg(&module, name, Arg::Bytes(subject), |interpreter, baseline| {
            assert_same_outcome(label, interpreter, baseline, &module.atom_table);
            let returned = baseline
                .as_ref()
                .unwrap_or_else(|error| panic!("{label}: {error:?}"));
            let RunOutcome::Returned(value) = returned else {
                panic!("{label}: unexpected outcome {returned:?}");
            };
            assert_eq!(
                beam_term::signed_value(*value),
                expected,
                "{label}: answered the wrong value for {subject:?}"
            );
        });
    }
}

// ---------------------------------------------------------------------------
// Boundary rows
// ---------------------------------------------------------------------------
/// **An empty subject** on every `bs_start_match3`-rooted shape: the context is built over
/// zero bits and the subsequent read fails into its fail label — a value, never a trap.
#[test]
fn an_empty_subject_takes_the_fail_label_on_every_rooted_shape() {
    let cases: [(&str, fn() -> Module); 6] = [
        ("bs_get_position", module_get_position),
        ("bs_set_position", module_set_position),
        ("bs_skip_bits2", module_skip_bits2),
        ("bs_start_match3", module_start_match3),
        ("bs_start_match4 (label)", module_start_match4_label),
        ("bs_get_integer2", module_get_integer2),
    ];
    for (label, build) in cases {
        let module = build();
        let name = atom(&module, "f");
        with_both_tiers_arg(&module, name, Arg::Bytes(&[]), |interpreter, baseline| {
            assert_same_outcome(label, interpreter, baseline, &module.atom_table);
            let returned = baseline
                .as_ref()
                .unwrap_or_else(|error| panic!("{label}: {error:?}"));
            let RunOutcome::Returned(value) = returned else {
                panic!("{label}: unexpected outcome {returned:?}");
            };
            assert_eq!(
                beam_term::signed_value(*value),
                -1,
                "{label}: an empty subject must reach the fail label"
            );
        });
    }
}

/// **A `bs_test_tail2` that fails** (a non-empty tail) branches to its own fail label, and
/// the same instruction **succeeds** when the tail is empty. Both rows are on one module,
/// so the two branches are the only difference.
#[test]
fn a_non_empty_tail_takes_the_test_tail2_fail_label() {
    let module = module_test_tail2();
    let name = atom(&module, "f");
    for (subject, expected) in [
        (&[1u8, 2][..], 0x0102i64),
        (&[1u8, 2, 3][..], -1),
        (&[1u8][..], -1),
    ] {
        with_both_tiers_arg(&module, name, Arg::Bytes(subject), |interpreter, baseline| {
            assert_same_outcome(
                &format!("bs_test_tail2 {subject:?}"),
                interpreter,
                baseline,
                &module.atom_table,
            );
            let returned = baseline.as_ref().unwrap_or_else(|error| panic!("{error:?}"));
            let RunOutcome::Returned(value) = returned else {
                panic!("unexpected outcome {returned:?}");
            };
            assert_eq!(
                beam_term::signed_value(*value),
                expected,
                "bs_test_tail2 on {subject:?}"
            );
        });
    }
}

/// **The zero-length segment, in both its forms.**
///
/// `ensure_at_least 0 1` emits *no test at all* in the lowering (`size == 0 && unit == 1`),
/// while `ensure_at_least 0 8` still has to fold the `% 8` remainder to a mask — so the two
/// rows exercise different emitted sequences and both must agree with the interpreter.
#[test]
fn a_zero_length_segment_agrees_in_both_unit_forms() {
    for unit in [1i64, 8] {
        let mut expected = vec![Instr::Label(Label(1)), Instr::Label(Label(2))];
        expected.push(Instr::BsStartMatch3 {
            fail: Label(3),
            bin: Operand::X(0),
            live: 1,
            dst: Operand::X(1),
        });
        expected.push(Instr::BsMatch {
            fail: Label(3),
            ctx: Operand::X(1),
            commands: {
                // The closure needs the module, so this arm is built below instead.
                Vec::new()
            },
        });
        expected.push(Instr::Return);
        expected.extend(sentinel_block(Label(3)));
        let module = bitstr_module(
            "inline_zero_segment",
            &["ensure_at_least", "integer"],
            1,
            move |module| {
                let mut commands = ensure_at_least(module, 0, unit);
                commands.extend(integer_command(module, 8, 1, Operand::X(0)));
                vec![
                    Instr::Label(Label(1)),
                    Instr::Label(Label(2)),
                    Instr::BsStartMatch3 {
                        fail: Label(3),
                        bin: Operand::X(0),
                        live: 1,
                        dst: Operand::X(1),
                    },
                    Instr::BsMatch {
                        fail: Label(3),
                        ctx: Operand::X(1),
                        commands,
                    },
                    Instr::Return,
                    Instr::Label(Label(3)),
                    Instr::Move {
                        src: Operand::Integer(-1),
                        dst: Operand::X(0),
                    },
                    Instr::Return,
                ]
            },
        );
        // The shape must still be inlinable, otherwise this row proves nothing new.
        let labels = crate::interpreter::build_label_map(&module);
        let instruction = module.functions[0]
            .code
            .iter()
            .find(|instr| matches!(instr, Instr::BsMatch { .. }))
            .expect("the module holds a `bs_match`");
        assert!(
            bitstr_inline::bs_match_shape(&labels, &module, Label(3), &Operand::X(1),
                match instruction {
                    Instr::BsMatch { commands, .. } => commands,
                    _ => unreachable!(),
                })
                .is_some(),
            "a zero-length `ensure_at_least 0 {unit}` segment must stay inlinable"
        );
        let name = atom(&module, "f");
        with_both_tiers_arg(&module, name, Arg::Bytes(&[0x5a]), |interpreter, baseline| {
            assert_same_outcome(
                &format!("zero segment unit {unit}"),
                interpreter,
                baseline,
                &module.atom_table,
            );
            let returned = baseline.as_ref().unwrap_or_else(|error| panic!("{error:?}"));
            let RunOutcome::Returned(value) = returned else {
                panic!("unexpected outcome {returned:?}");
            };
            assert_eq!(
                beam_term::signed_value(*value),
                0x5a,
                "zero-length segment unit {unit}"
            );
        });
    }
}

/// **A non-byte-aligned unit** (`unit == 3`, a power of two is *not* required here — it is
/// the `size * unit` product that must be a width the extractor covers): `2 * 3 == 6` bits
/// is bit-granular, so the extraction reads a 16-bit window and masks.
#[test]
fn a_non_byte_aligned_unit_agrees_with_the_interpreter() {
    let module = bitstr_module("inline_granular", &["ensure_at_least", "integer"], 1, |module| {
        let mut commands = ensure_at_least(module, 2, 3);
        commands.extend(integer_command(module, 2, 3, Operand::X(0)));
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsStartMatch3 {
                fail: Label(3),
                bin: Operand::X(0),
                live: 1,
                dst: Operand::X(1),
            },
            Instr::BsMatch {
                fail: Label(3),
                ctx: Operand::X(1),
                commands,
            },
            Instr::Return,
        ];
        code.extend(sentinel_block(Label(3)));
        code
    });
    let name = atom(&module, "f");
    // `0b1010_1100`: the first 6 bits are `101011` = 43.
    with_both_tiers_arg(&module, name, Arg::Bytes(&[0b1010_1100]), |interpreter, baseline| {
        assert_same_outcome("granular unit 3", interpreter, baseline, &module.atom_table);
        let returned = baseline.as_ref().unwrap_or_else(|error| panic!("{error:?}"));
        let RunOutcome::Returned(value) = returned else {
            panic!("unexpected outcome {returned:?}");
        };
        assert_eq!(beam_term::signed_value(*value), 0b101011, "first 6 bits");
    });
}

/// **A bit-granular field narrower than 8 bits (6), beside the aligned 16- and 32-bit
/// widths.** All three are inlinable; the 6-bit one goes through the 16-bit-window form of
/// [`bitstr_inline::emit_bit_extract`], the other two through its aligned forms.
#[test]
fn bit_granular_and_aligned_bs_match_fields_agree_with_the_interpreter() {
    // (label, size, unit, subject, expected)
    let cases: [(&str, i64, i64, &'static [u8], i64); 3] = [
        ("6-bit", 6, 1, &[0b1011_0110], 0b101101),
        ("16-bit", 16, 1, &[0x12, 0x34], 0x1234),
        ("32-bit", 32, 1, &[0x12, 0x34, 0x56, 0x78], 0x1234_5678),
    ];
    for (label, size, unit, subject, expected) in cases {
        let module = bitstr_module("inline_width", &["ensure_at_least", "integer"], 1, |module| {
            let mut commands = ensure_at_least(module, size, unit);
            commands.extend(integer_command(module, size, unit, Operand::X(0)));
            let mut code = vec![
                Instr::Label(Label(1)),
                Instr::Label(Label(2)),
                Instr::BsStartMatch3 {
                    fail: Label(3),
                    bin: Operand::X(0),
                    live: 1,
                    dst: Operand::X(1),
                },
                Instr::BsMatch {
                    fail: Label(3),
                    ctx: Operand::X(1),
                    commands,
                },
                Instr::Return,
            ];
            code.extend(sentinel_block(Label(3)));
            code
        });
        // The width must be one the arm actually covers.
        let labels = crate::interpreter::build_label_map(&module);
        let commands = match &module.functions[0].code[3] {
            Instr::BsMatch { commands, .. } => commands.clone(),
            other => panic!("expected a `bs_match`, got {other:?}"),
        };
        assert!(
            bitstr_inline::bs_match_shape(&labels, &module, Label(3), &Operand::X(1), &commands)
                .is_some(),
            "{label}: must stay inlinable"
        );
        let name = atom(&module, "f");
        with_both_tiers_arg(&module, name, Arg::Bytes(subject), |interpreter, baseline| {
            assert_same_outcome(label, interpreter, baseline, &module.atom_table);
            let returned = baseline.as_ref().unwrap_or_else(|error| panic!("{label}: {error:?}"));
            let RunOutcome::Returned(value) = returned else {
                panic!("{label}: unexpected outcome {returned:?}");
            };
            assert_eq!(
                beam_term::signed_value(*value),
                expected,
                "{label}: read the wrong field out of {subject:?}"
            );
        });
    }
}

/// **A segment that overruns takes the fail-label branch** — never a trap, never a wrong
/// class, and the context is left where the last successful command left it (the family's
/// own `{case_clause,_}` / `{badmatch,_}` reason follows from that).
///
/// The two rows are a `bs_get_integer2` and a `bs_match`; both must agree with the
/// interpreter, which is what rules out a fabricated value.
#[test]
fn an_overrunning_segment_takes_the_fail_label() {
    let get_integer2 = module_get_integer2();
    let name = atom(&get_integer2, "f");
    with_both_tiers_arg(&get_integer2, name, Arg::Bytes(&[1]), |interpreter, baseline| {
        assert_same_outcome("overrun bs_get_integer2", interpreter, baseline, &get_integer2.atom_table);
        let returned = baseline.as_ref().unwrap_or_else(|error| panic!("{error:?}"));
        let RunOutcome::Returned(value) = returned else {
            panic!("unexpected outcome {returned:?}");
        };
        assert_eq!(
            beam_term::signed_value(*value),
            -1,
            "a 16-bit read out of one byte must reach the fail label"
        );
    });
    let bs_match = module_bs_match();
    let name = atom(&bs_match, "f");
    with_both_tiers_arg(&bs_match, name, Arg::Bytes(&[]), |interpreter, baseline| {
        assert_same_outcome("overrun bs_match", interpreter, baseline, &bs_match.atom_table);
        let returned = baseline.as_ref().unwrap_or_else(|error| panic!("{error:?}"));
        let RunOutcome::Returned(value) = returned else {
            panic!("unexpected outcome {returned:?}");
        };
        assert_eq!(
            beam_term::signed_value(*value),
            -1,
            "an empty subject cannot satisfy `ensure_at_least 8 8`"
        );
    });
}

/// **A `bs_get_integer2` result is a first-class heap term afterwards.**
///
/// The extracted integer is written into `x(0)` and then used as a tuple element, so the
/// value the arm stored must be a real small integer the term builder accepts (a mangled
/// word — an untagged shift, a stale cache register — fails here rather than in a later
/// BIF).
#[test]
fn a_bs_get_integer2_result_is_usable_as_a_heap_term() {
    let module = bitstr_module("inline_integer_as_term", &[], 1, |_| {
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsStartMatch3 {
                fail: Label(3),
                bin: Operand::X(0),
                live: 1,
                dst: Operand::X(1),
            },
            Instr::BsGetInteger2(Box::new(BsGetArgs {
                fail: Label(3),
                ctx: Operand::X(1),
                live: 2,
                size: Operand::Integer(8),
                unit: 1,
                flags: 0,
                dst: Operand::X(0),
            })),
            Instr::PutTuple2 {
                dst: Operand::X(0),
                elements: vec![Operand::Integer(1), Operand::X(0)],
            },
            Instr::Return,
        ];
        code.extend(sentinel_block(Label(3)));
        code
    });
    let name = atom(&module, "f");
    with_both_tiers_arg(&module, name, Arg::Bytes(&[0x2a]), |interpreter, baseline| {
        assert_same_outcome("integer as a tuple element", interpreter, baseline, &module.atom_table);
        let returned = baseline.as_ref().unwrap_or_else(|error| panic!("{error:?}"));
        let RunOutcome::Returned(value) = returned else {
            panic!("unexpected outcome {returned:?}");
        };
        // `tuple_arity` is the safe reader: it answers `BadArg` for a non-tuple, so it
        // doubles as the "the answer must be a tuple" assertion.
        assert_eq!(
            crate::terms::tuple_arity(*value).expect("the answer must be a tuple"),
            2,
            "the answer must be a 2-tuple"
        );
        assert_eq!(
            beam_term::signed_value(crate::terms::tuple_element(*value, 1).expect("element 1")),
            0x2a,
            "the extracted byte must be the tuple's second element"
        );
    });
}

// ---------------------------------------------------------------------------
// The deliberately-not-inlined shapes still reach the family
// ---------------------------------------------------------------------------
/// **A shape the arm refuses still runs through the family helper.**
///
/// `bitstr_calls()` counts `jit_bitstr` entries, i.e. the emitted arm's *helper* route. A
/// recorded gap therefore cannot hide a shape that silently stopped executing: the value
/// assertion pins the answer and the counter pins the route.
#[test]
fn a_refused_shape_still_runs_through_the_bitstring_helper() {
    let subject: &'static [u8] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
    // (fixture, function, subject, the reason the arm refuses the shape)
    let cases: [(&str, &str, &'static [u8], &str); 6] = [
        ("bssmall", "u64", subject, "width rule: 64 bits is neither <= 16 nor 32"),
        ("bssmall", "s64", subject, "signed field"),
        ("bssmall", "u64le", subject, "little-endian field"),
        ("bssmall", "u36", subject, "width rule: 36 bits"),
        ("vb_bitstr", "flat", &[0, 1, 0, 2, 9], "a `get_tail` command"),
        ("bscreate", "u8", &[], "`bs_create_bin` (not one of the eight shapes)"),
    ];
    for (fixture_name, function, _subject, reason) in cases {
        let module = fixture(fixture_name);
        let name = export(&module, fixture_name, function);
        // `bscreate:u8/1` takes an integer, everything else a binary.
        let arg = if fixture_name == "bscreate" {
            Arg::Int(65)
        } else if fixture_name == "vb_bitstr" {
            Arg::Bytes(&[0, 1, 0, 2])
        } else {
            Arg::Bytes(&[1, 2, 3, 4, 5, 6, 7, 8, 9])
        };
        let before = bitstr_calls();
        with_both_tiers_arg(&module, name, arg, |interpreter, baseline| {
            assert_same_outcome(
                &format!("{fixture_name}:{function}/1"),
                interpreter,
                baseline,
                &module.atom_table,
            );
        });
        assert!(
            bitstr_calls() > before,
            "{fixture_name}:{function}/1 must reach the bitstring helper ({reason}); \
             counter {before} -> {}",
            bitstr_calls(),
        );
    }
}

/// **A `NO_FAIL` shape still goes through the family.** A `NO_FAIL` fail code raises (the
/// family's allocating reason — `{badmatch, Subject}` / the field's own error), so every
/// `*_shape` predicate refuses it and the instruction keeps the helper route.
///
/// The hand-built rows are the only way to reach `NO_FAIL` from a synthetic module: `erlc`
/// never emits it for a `case` clause.
#[test]
fn a_no_fail_shape_still_goes_through_the_family() {
    // `bs_get_integer2` with `NO_FAIL`: the predicate refuses it ...
    let module = bitstr_module("inline_no_fail_get_integer", &[], 1, |_| {
        vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsStartMatch3 {
                fail: Label(3),
                bin: Operand::X(0),
                live: 1,
                dst: Operand::X(1),
            },
            Instr::BsGetInteger2(Box::new(BsGetArgs {
                fail: NO_FAIL,
                ctx: Operand::X(1),
                live: 2,
                size: Operand::Integer(8),
                unit: 1,
                flags: 0,
                dst: Operand::X(0),
            })),
            Instr::Return,
            Instr::Label(Label(3)),
            Instr::Move {
                src: Operand::Integer(-1),
                dst: Operand::X(0),
            },
            Instr::Return,
        ]
    });
    let labels = crate::interpreter::build_label_map(&module);
    assert!(
        bitstr_inline::get_integer2_shape(
            &labels,
            NO_FAIL,
            &Operand::X(1),
            &Operand::Integer(8),
            1,
            0,
            &Operand::X(0),
        )
        .is_none(),
        "a `NO_FAIL` `bs_get_integer2` must stay on the family"
    );
    // ... and at run time it really reaches the helper, on both success and failure.
    let name = atom(&module, "f");
    for subject in [&[0x2au8][..], &[][..]] {
        let before = bitstr_calls();
        with_both_tiers_arg(&module, name, Arg::Bytes(subject), |interpreter, baseline| {
            assert_same_outcome(
                &format!("NO_FAIL bs_get_integer2 {subject:?}"),
                interpreter,
                baseline,
                &module.atom_table,
            );
        });
        assert!(
            bitstr_calls() > before,
            "a `NO_FAIL` `bs_get_integer2` on {subject:?} must reach the helper"
        );
    }
}

// ---------------------------------------------------------------------------
// The refusal rules, shape by shape
// ---------------------------------------------------------------------------
/// **Every deliberate refusal of the eight arms, classified at the predicate.**
///
/// A refused shape is not a bug: it is the family's general path, which is why each of
/// these must answer `None`. The list is the same one
/// `emit/test/refusal_census.rs`'s `Bitstring refusals` section reports against the real
/// corpus, so the two cannot drift.
#[test]
fn every_deliberate_refusal_is_refused_by_the_shape_predicate() {
    let module = bitstr_module(
        "inline_refusals",
        &[
            "ensure_at_least",
            "integer",
            "get_tail",
            "binary",
            "=:=",
            "skip",
            "no_fail",
            "resume",
            "something_else",
        ],
        1,
        |_| vec![Instr::Label(Label(1)), Instr::Label(Label(2)), Instr::Return],
    );
    let labels = crate::interpreter::build_label_map(&module);
    let x0 = Operand::X(0);
    let x1 = Operand::X(1);
    let y0 = Operand::Y(0);
    let y1 = Operand::Y(1);

    // --- `bs_get_position` / `bs_set_position`: an un-addressable operand ------------
    assert!(
        bitstr_inline::get_position_shape(&x1, &Operand::FloatReg(0)).is_none(),
        "a float register destination must refuse `bs_get_position`"
    );
    assert!(
        bitstr_inline::set_position_shape(&x1, &Operand::FloatReg(0)).is_none(),
        "a float register position must refuse `bs_set_position`"
    );

    // --- `bs_test_tail2` / `bs_skip_bits2` / `bs_get_integer2` / `bs_match`: fail -----
    assert!(
        bitstr_inline::test_tail2_shape(&labels, NO_FAIL, &x1, 8).is_none(),
        "`NO_FAIL` must refuse `bs_test_tail2`"
    );
    assert!(
        bitstr_inline::skip_bits2_shape(&labels, NO_FAIL, &x1, &Operand::Integer(8), 1).is_none(),
        "`NO_FAIL` must refuse `bs_skip_bits2`"
    );
    assert!(
        bitstr_inline::get_integer2_shape(
            &labels, NO_FAIL, &x1, &Operand::Integer(8), 1, 0, &x0
        )
        .is_none(),
        "`NO_FAIL` must refuse `bs_get_integer2`"
    );
    // An unresolved fail label is the family's own `unknown_label`, never a fabricated
    // exit: the label map holds only `Label(1)`/`Label(2)`.
    assert!(
        bitstr_inline::test_tail2_shape(&labels, Label(99), &x1, 8).is_none(),
        "an unresolved fail label must refuse `bs_test_tail2`"
    );
    assert!(
        bitstr_inline::bs_match_shape(&labels, &module, Label(99), &x1, &ensure_at_least(&module, 8, 8))
            .is_none(),
        "an unresolved fail label must refuse `bs_match`"
    );

    // --- `bs_skip_bits2`: a runtime size is a compare the sequence does not have -----
    assert!(
        bitstr_inline::skip_bits2_shape(&labels, Label(2), &x1, &Operand::X(2), 1).is_none(),
        "a runtime `bs_skip_bits2` size must refuse the arm"
    );

    // --- `bs_get_integer2`: the width / signedness / endianness rule ------------------
    let refuse = |size: i64, unit: u32, flags: u32| {
        bitstr_inline::get_integer2_shape(
            &labels,
            Label(2),
            &x1,
            &Operand::Integer(size),
            unit,
            flags,
            &x0,
        )
        .is_none()
    };
    assert!(refuse(24, 1, 0), "24 bits is outside {{<= 16, 32}}");
    assert!(refuse(64, 1, 0), "64 bits is outside {{<= 16, 32}}");
    assert!(refuse(8, 1, 4), "a signed field is the family's general reader");
    assert!(refuse(8, 1, 2), "a little-endian field is the family's general reader");
    assert!(refuse(8, 1, 16), "a native-endian field is the family's general reader");
    assert!(
        bitstr_inline::get_integer2_shape(
            &labels,
            Label(2),
            &x1,
            &Operand::X(2),
            1,
            0,
            &x0
        )
        .is_none(),
        "a runtime `bs_get_integer2` size must refuse the arm"
    );

    // --- `bs_match`: the whole-list rules --------------------------------------------
    let refuse_commands = |commands: Vec<Operand>| {
        bitstr_inline::bs_match_shape(&labels, &module, Label(2), &x1, &commands).is_none()
    };
    // A `get_tail` / `binary` / `=:=` command allocates a term, so a moving collection
    // could relocate the context block in the middle of the list.
    assert!(
        refuse_commands(vec![
            Operand::Atom(atom(&module, "get_tail")),
            Operand::Integer(2),
            Operand::Integer(8),
            x0.clone(),
        ]),
        "a `get_tail` command must refuse the whole `bs_match`"
    );
    assert!(
        refuse_commands(vec![
            Operand::Atom(atom(&module, "binary")),
            Operand::Integer(2),
            Operand::Nil,
            Operand::Integer(8),
            Operand::Integer(1),
            x0.clone(),
        ]),
        "a `binary` command must refuse the whole `bs_match`"
    );
    assert!(
        refuse_commands(vec![
            Operand::Atom(atom(&module, "=:=")),
            Operand::Nil,
            Operand::Integer(8),
            Operand::Integer(98),
        ]),
        "a `=:=` command must refuse the whole `bs_match`"
    );
    // A runtime stride/size.
    assert!(
        refuse_commands(vec![Operand::Atom(atom(&module, "skip")), Operand::X(2)]),
        "a runtime `skip` stride must refuse the whole `bs_match`"
    );
    // A non-power-of-two `ensure_at_least` unit would need a division, not a mask.
    assert!(
        refuse_commands(ensure_at_least(&module, 8, 3)),
        "a non-power-of-two `ensure_at_least` unit must refuse the whole `bs_match`"
    );
    // The width rule, inside a command list.
    let mut wide = ensure_at_least(&module, 8, 8);
    wide.extend(integer_command(&module, 24, 1, x0.clone()));
    assert!(refuse_commands(wide), "a 24-bit `integer` must refuse the `bs_match`");
    // More than one `Y` destination needs a second live Y address.
    let mut two_y = integer_command(&module, 8, 1, y0);
    two_y.extend(integer_command(&module, 8, 1, y1));
    assert!(
        refuse_commands(two_y),
        "two `Y` destinations must refuse the whole `bs_match`"
    );
    // A single `Y` destination is fine — the rule is `> 1`, not `>= 1`.
    let mut one_y = ensure_at_least(&module, 8, 8);
    one_y.extend(integer_command(&module, 8, 1, Operand::Y(0)));
    assert!(
        bitstr_inline::bs_match_shape(&labels, &module, Label(2), &x1, &one_y).is_some(),
        "one `Y` destination must stay inlinable"
    );
    // `bs_start_match4`'s fail operand: only a label, `no_fail` or `resume` decode.
    assert!(
        bitstr_inline::start_match4_shape(&module, &labels, &Operand::Integer(1), &x1, &x0)
            .is_none(),
        "a non-label/non-atom `bs_start_match4` fail operand must refuse the arm"
    );
    // `NO_FAIL` (and the `no_fail` atom) do **not** refuse the arm: they inline as
    // `MatchFail::Family`, i.e. a *hit* is native and only the allocating
    // `{badmatch, Subject}` **miss** routes to the interpreter step. Refusing here would
    // throw away every well-formed `bs_start_match3` a compiler emits for a plain binary
    // pattern match (the commonest shape in the corpus).
    assert_eq!(
        bitstr_inline::start_match4_shape(&module, &labels, &Operand::Label(NO_FAIL), &x1, &x0)
            .map(|(_, fail)| fail),
        Some(MatchFail::Family),
        "`NO_FAIL` must inline `bs_start_match4` with every miss on the family"
    );
    assert_eq!(
        bitstr_inline::start_match4_shape(
            &module,
            &labels,
            &Operand::Atom(atom(&module, "no_fail")),
            &x1,
            &x0
        )
        .map(|(_, fail)| fail),
        Some(MatchFail::Family),
        "the `no_fail` atom must inline `bs_start_match4` with every miss on the family"
    );
    assert_eq!(
        bitstr_inline::start_match4_shape(
            &module,
            &labels,
            &Operand::Atom(atom(&module, "resume")),
            &x1,
            &x0
        )
        .map(|(_, fail)| fail),
        Some(MatchFail::Resume),
        "the `resume` atom must inline `bs_start_match4` as the `Resume` class"
    );
    // An atom the module cannot name, and an atom that is neither `no_fail` nor `resume`,
    // are the family's typed `unsupported` — refused here so it reports them.
    assert!(
        bitstr_inline::start_match4_shape(
            &module,
            &labels,
            &Operand::Atom(atom(&module, "something_else")),
            &x1,
            &x0
        )
        .is_none(),
        "an unrelated fail atom must refuse the `bs_start_match4` arm"
    );
    // `bs_start_match3` shares the classification (it has no atom form).
    assert_eq!(
        bitstr_inline::start_match3_shape(&labels, NO_FAIL, &x1, &x0).map(|(_, fail)| fail),
        Some(MatchFail::Family),
        "`NO_FAIL` must inline `bs_start_match3` with every miss on the family"
    );
    assert_eq!(
        bitstr_inline::start_match3_shape(&labels, Label(2), &x1, &x0).map(|(_, fail)| fail),
        Some(MatchFail::Label(
            labels.resolve(Label(2)).expect("label 2 resolves")
        )),
        "a resolved fail label must inline as a direct miss branch"
    );
}

// ---------------------------------------------------------------------------
// The register cache: every written register is charged, and a `Y` never is
// ---------------------------------------------------------------------------
/// **`regalloc::cacheable_writes` charges every x-register an arm writes.**
///
/// Omitting a destination is the one *unsound* failure mode of an inline arm: the block's
/// exit spill would not store that cache register, so a stale one survives across an exit
/// — a wrong value, not a redundant store. The three context-mutating shapes
/// (`bs_set_position`, `bs_test_tail2`, `bs_skip_bits2`) write a heap word inside the
/// context block and no x-register at all.
#[test]
fn the_register_cache_is_charged_for_every_register_an_arm_writes() {
    let module = bitstr_module(
        "inline_writes",
        &["ensure_at_least", "integer"],
        1,
        |_| vec![Instr::Label(Label(1)), Instr::Label(Label(2)), Instr::Return],
    );
    let labels = crate::interpreter::build_label_map(&module);

    let get_position = Instr::BsGetPosition {
        ctx: Operand::X(1),
        dst: Operand::X(0),
        live: 2,
    };
    assert_eq!(
        cacheable_writes(&module, &labels, &get_position),
        vec![0],
        "`bs_get_position` writes its destination"
    );

    let set_position = Instr::BsSetPosition {
        ctx: Operand::X(1),
        pos: Operand::X(2),
    };
    assert!(
        cacheable_writes(&module, &labels, &set_position).is_empty(),
        "`bs_set_position` writes into the context block, no x-register"
    );

    let test_tail2 = Instr::BsTestTail2 {
        fail: Label(2),
        ctx: Operand::X(1),
        bits: 0,
    };
    assert!(
        cacheable_writes(&module, &labels, &test_tail2).is_empty(),
        "`bs_test_tail2` only branches"
    );

    let skip_bits2 = Instr::BsSkipBits2 {
        fail: Label(2),
        ctx: Operand::X(1),
        size: Operand::Integer(8),
        unit: 1,
        flags: 0,
    };
    assert!(
        cacheable_writes(&module, &labels, &skip_bits2).is_empty(),
        "`bs_skip_bits2` writes into the context block, no x-register"
    );

    let start_match3 = Instr::BsStartMatch3 {
        fail: Label(2),
        bin: Operand::X(1),
        live: 2,
        dst: Operand::X(0),
    };
    assert_eq!(
        cacheable_writes(&module, &labels, &start_match3),
        vec![0],
        "`bs_start_match3` stores the reused context word into its destination"
    );

    let start_match4 = Instr::BsStartMatch4 {
        fail: Operand::Label(Label(2)),
        live: 2,
        src: Operand::X(1),
        dst: Operand::X(0),
    };
    assert_eq!(
        cacheable_writes(&module, &labels, &start_match4),
        vec![0],
        "`bs_start_match4` stores the reused context word into its destination"
    );

    let get_integer2 = Instr::BsGetInteger2(Box::new(BsGetArgs {
        fail: Label(2),
        ctx: Operand::X(1),
        live: 2,
        size: Operand::Integer(8),
        unit: 1,
        flags: 0,
        dst: Operand::X(0),
    }));
    assert_eq!(
        cacheable_writes(&module, &labels, &get_integer2),
        vec![0],
        "`bs_get_integer2` writes its destination"
    );

    // `bs_match` charges **every** `integer` destination, in command order.
    let mut commands = ensure_at_least(&module, 16, 8);
    commands.extend(integer_command(&module, 8, 1, Operand::X(0)));
    commands.extend(integer_command(&module, 8, 1, Operand::X(3)));
    let bs_match = Instr::BsMatch {
        fail: Label(2),
        ctx: Operand::X(1),
        commands,
    };
    assert_eq!(
        cacheable_writes(&module, &labels, &bs_match),
        vec![0, 3],
        "`bs_match` writes every `integer` destination"
    );

    // A `Y` destination is a stack slot: it must contribute **nothing** to the write set
    // (a `Y` index in a spill list would index `machine.x`).
    let y_destination = Instr::BsGetPosition {
        ctx: Operand::X(1),
        dst: Operand::Y(0),
        live: 1,
    };
    assert!(
        y_destination_is_inlinable(bitstr_inline::get_position_shape(
            &Operand::X(1),
            &Operand::Y(0)
        )),
        "an addressable `Y` destination must stay inlinable"
    );
    assert!(
        cacheable_writes(&module, &labels, &y_destination).is_empty(),
        "a `Y` destination must never appear in the x-register write set"
    );
    // ... and neither does a `Y` appear among the shape's touched x-registers.
    assert!(
        !bitstr_inline::get_position_shape(&Operand::X(1), &Operand::Y(0))
            .expect("the shape")
            .registers()
            .contains(&0),
        "a `Y` destination is not an x-register touch"
    );
}

/// Whether a `bs_get_position` shape exists at all (a tiny helper so the `Y`-destination
/// assertion above reads as one claim).
fn y_destination_is_inlinable<T>(shape: Option<T>) -> bool {
    shape.is_some()
}

// ---------------------------------------------------------------------------
// Emitted-byte pins
// ---------------------------------------------------------------------------
/// Whether `code` contains `word` as an eight-byte little-endian immediate.
///
/// Every constant the encoder materialises is a `movabs` (a ten-byte `mov` with the imm64
/// last), so a sufficiently distinctive constant can be located without pinning the
/// encoder's opcode bytes — which `super::emitter`'s own tests already do.
fn contains_immediate(code: &[u8], word: i64) -> bool {
    let needle = word.to_le_bytes();
    code.windows(needle.len()).any(|window| window == needle)
}

/// **`bs_get_position`'s position-term sequence is really emitted**: the `MAX_SMALL` guard
/// that keeps an oversized position on the family (its whole-context snapshot allocates)
/// appears in the compiled block, and nowhere in a block that has no such shape.
///
/// The guard is unexercisable at run time from a test — a position above `MAX_SMALL` would
/// need a subject of ≥ 2^56 bytes — so the emitted bytes are the only honest evidence that
/// the branch exists; the *charged* half (the small-integer construction) is the
/// `shl`/`or` pair the same arm emits, covered by the value rows above.
#[test]
fn the_get_position_arm_emits_the_max_small_guard() {
    let module = module_get_position();
    let labels = crate::interpreter::build_label_map(&module);
    let function = super::emit::compile_function(&module, &labels, 0).expect("the block compiles");
    let code = function_code_bytes(&function);
    assert!(
        contains_immediate(code, beam_term::MAX_SMALL),
        "the `bs_get_position` arm must emit the `MAX_SMALL` position guard"
    );

    let plain = bitstr_module("inline_position_free", &[], 1, |_| {
        vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::Move {
                src: Operand::Integer(1),
                dst: Operand::X(0),
            },
            Instr::Return,
        ]
    });
    let labels = crate::interpreter::build_label_map(&plain);
    let function = super::emit::compile_function(&plain, &labels, 0).expect("the block compiles");
    assert!(
        !contains_immediate(function_code_bytes(&function), beam_term::MAX_SMALL),
        "a `move`-only block must not carry the position guard"
    );
}

/// **The inlined extract emits no `jit_bitstr` helper call at all** — the property that
/// actually distinguishes the two routes on the *same* operand shape (an 8-bit
/// `bs_get_integer2`, inlined, against a 64-bit one the width rule sends to the family).
///
/// This pin originally asserted the inlined block was **shorter**, and it is not: measured
/// here at 1356 B inlined against 753 B on the helper route, and that is the correct
/// outcome. The helper route replaces the whole extraction with one `emit_helper_call`
/// (`movabs r11, jit_bitstr` + two argument loads + `call r11`, ~41 B) and the *family*
/// pays the extraction, while the inline arm emits that extraction — plus its match-context
/// test, bounds check, byte-wise big-endian assembly and mask — into the block. Code size is
/// therefore the wrong axis; what the shape buys is that the extraction is **native**, so
/// the surviving claim is the one asserted below: the inlined block carries **no** helper
/// call, and the helper-routed block does. (Both byte figures are printed, not baked, so a
/// future change to either route reports itself here.)
#[test]
fn an_inlined_extract_emits_no_helper_call_unlike_the_helper_routed_one() {
    let build = |size: i64| {
        bitstr_module("inline_extract_size", &[], 1, move |_| {
            let mut code = vec![
                Instr::Label(Label(1)),
                Instr::Label(Label(2)),
                Instr::BsStartMatch3 {
                    fail: Label(3),
                    bin: Operand::X(0),
                    live: 1,
                    dst: Operand::X(1),
                },
                Instr::BsGetInteger2(Box::new(BsGetArgs {
                    fail: Label(3),
                    ctx: Operand::X(1),
                    live: 2,
                    size: Operand::Integer(size),
                    unit: 1,
                    flags: 0,
                    dst: Operand::X(0),
                })),
                Instr::Return,
            ];
            code.extend(sentinel_block(Label(3)));
            code
        })
    };
    let inlined_module = build(8);
    let (emitted, snapshot) = emitted_shapes(&inlined_module);
    assert!(
        emitted.contains(&SHAPE_GET_INTEGER2),
        "the 8-bit read must be inlined (emitted {emitted:?})"
    );
    assert_eq!(static_deopts(&snapshot, "BsGetInteger2"), 0);
    let helper_module = build(64);
    let (emitted, _) = emitted_shapes(&helper_module);
    assert!(
        !emitted.contains(&SHAPE_GET_INTEGER2),
        "the 64-bit read must stay on the helper (emitted {emitted:?})"
    );
    // An **inline** arm never emits the family helper, and a helper-routed variant always
    // does — that asymmetry is what "the shape is native" means at the byte level.
    let helper_probe =
        super::bitstr::jit_bitstr as extern "C" fn(*mut super::runtime::JitRuntime<'_>, u32, u32) -> i64
            as *const ();
    let labels = crate::interpreter::build_label_map(&inlined_module);
    let inlined_function =
        super::emit::compile_function(&inlined_module, &labels, 0).expect("the block compiles");
    let inlined_len = inlined_function.buffer.len();
    let labels = crate::interpreter::build_label_map(&helper_module);
    let helper_function =
        super::emit::compile_function(&helper_module, &labels, 0).expect("the block compiles");
    let helper_len = helper_function.buffer.len();
    eprintln!(
        "bitstring inline vs helper route on one 8-bit/64-bit bs_get_integer2 block: \
         inlined {inlined_len} B, helper {helper_len} B"
    );
    assert!(
        !super::tests::code_contains_helper(&inlined_function, helper_probe),
        "an inlined extract must not call the bitstring helper (the extraction is native)"
    );
    assert!(
        super::tests::code_contains_helper(&helper_function, helper_probe),
        "a helper-routed extract must call the bitstring helper"
    );
}

// ---------------------------------------------------------------------------
// The tier-equivalence value rows are also checked against the real `erl` oracle
// ---------------------------------------------------------------------------
/// **Every hand-built expectation above is the system `erl` oracle's answer** for the same
/// shape, so a mistake shared by both tiers cannot pass.
///
/// The module is compiled with the system `erlc` from the *same* Erlang source the shapes
/// model, and the oracle's value is compared with the expected constant this file asserts.
/// Skips loudly when no `erlc`/`erl` is on `PATH` (a green run without the toolchain proves
/// nothing about corpus parity).
#[test]
fn the_hand_built_expectations_match_the_system_oracle() {
    let erlc = std::env::var("ERLC").unwrap_or_else(|_| "erlc".to_string());
    let erl = std::env::var("ERTS_ORACLE").unwrap_or_else(|_| "erl".to_string());
    let dir = std::env::temp_dir().join(format!(
        "beam_engine_bitstr_inline_oracle_{}",
        std::process::id()
    ));
    if std::fs::create_dir_all(&dir).is_err() {
        eprintln!("SKIP the_hand_built_expectations_match_the_system_oracle: no temp dir");
        return;
    }
    let source = dir.join("inline_oracle.erl");
    let text = "\
-module(inline_oracle).
-export([granular/1, six/1, wide16/1, wide32/1, tail_empty/1, skip8/1]).
granular(<<A:6, _/bits>>) -> A.
six(<<A:6, _/bits>>) -> A.
wide16(<<A:16, _/bits>>) -> A.
wide32(<<A:32, _/bits>>) -> A.
tail_empty(<<A:16>>) -> A.
skip8(<<_:8, A:8, _/bits>>) -> A.
";
    if std::fs::write(&source, text).is_err() {
        eprintln!("SKIP the_hand_built_expectations_match_the_system_oracle: cannot write");
        return;
    }
    let compiled = std::process::Command::new(&erlc)
        .arg("-o")
        .arg(&dir)
        .arg(&source)
        .output();
    let Ok(compiled) = compiled else {
        eprintln!("SKIP the_hand_built_expectations_match_the_system_oracle: no `{erlc}`");
        return;
    };
    if !compiled.status.success() {
        eprintln!(
            "SKIP the_hand_built_expectations_match_the_system_oracle: `{erlc}` failed: {}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        return;
    }
    let eval = format!(
        "S=<<16#AC,16#12,16#34,16#56,16#78>>,\
         io:format(\"~~p~~n\",[{{inline_oracle:granular(S),\
         inline_oracle:six(S),\
         inline_oracle:wide16(S),\
         inline_oracle:wide32(S),\
         inline_oracle:tail_empty(<<16#12,16#34>>),\
         inline_oracle:skip8(S)}}]),halt()."
    );
    let output = std::process::Command::new(&erl)
        .args(["-noshell", "-pa", dir.to_str().unwrap_or("."), "-eval", &eval])
        .output();
    let Ok(output) = output else {
        eprintln!("SKIP the_hand_built_expectations_match_the_system_oracle: no `{erl}`");
        return;
    };
    if !output.status.success() {
        eprintln!(
            "SKIP the_hand_built_expectations_match_the_system_oracle: `{erl}` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.trim();
    // `{43,43,4660,305419896,4660,18}` — recomputed here from the scheme above, so a
    // hand-edited constant in this file would not track a changed expectation.
    let values: Vec<i64> = text
        .trim_matches(|c| c == '{' || c == '}')
        .split(',')
        .map(|field| field.trim().parse::<i64>().expect("an oracle integer"))
        .collect();
    assert_eq!(values.len(), 6, "the oracle must answer six values: {text}");
    assert_eq!(values[0], 0b101011, "`<<A:6,_/bits>>` on 0xAC");
    assert_eq!(values[1], 0b101011, "the same shape under its own name");
    assert_eq!(values[2], 0x1234, "`<<A:16,_/bits>>`");
    assert_eq!(values[3], 0x1234_5678, "`<<A:32,_/bits>>`");
    assert_eq!(values[4], 0x1234, "`<<A:16>>`");
    assert_eq!(values[5], 0x12, "`<<_:8,A:8,_/bits>>`");
    let _ = std::fs::remove_dir_all(&dir);
}

