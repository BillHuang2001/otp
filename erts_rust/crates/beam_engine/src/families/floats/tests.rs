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

//! Per-variant unit tests for the floating-point instruction family.
//!
//! Every float instruction is driven straight through [`exec`](super::exec) over a
//! [`Machine`](crate::machine::Machine) and the mock context — the loader is not
//! involved — so the suite pins the family's own documented decisions:
//!
//! * the **success** path (the value each `f*` instruction leaves in its
//!   destination, and the bignum → double rounding of `fconv`),
//! * the **fail-branch** path (a documented arithmetic failure with a live `fail`
//!   label is `Step::Goto { func, pc }`),
//! * the **`NO_FAIL`** path (the `badarith` raise, its catchability by a live
//!   `try`/`catch` frame and the landing convention in `x(0..=3)`), and
//! * the seam contract (`Ok(None)` for an instruction no float variant claims).
//!
//! Two end-to-end programs additionally run through the dispatch loop
//! ([`Machine::run_export`](crate::machine::Machine::run_export)).
//!
//! Everything asserted here mirrors `../../erts/emulator/beam/emu/float_instrs.tab`
//! and the oracle-checked values noted inline; see the parent module docs for the
//! semantics and the M1 FR-file substitution.

use super::*;

use beam_loader::Function;
use beam_term::{is_small, make_atom, make_small, Atom, NIL};

use crate::context::MinimalContext;
use crate::machine::CatchFrame;
use crate::terms::{float_value, make_float};

/// An empty label map (only the tests that exercise a `fail` target build one).
fn no_labels() -> LabelMap {
    LabelMap::new()
}

/// A module whose atom table holds the two atoms a `badarith` landing needs
/// (`badarith` and the `error` class), so `raise_badarith` can name the raise.
fn module_with_raise_atoms() -> Module {
    let mut module = Module::default();
    module.name = module.atom_table.intern("m");
    module.atom_table.intern("error");
    module.atom_table.intern("badarith");
    module
}

/// Dispatch one instruction through the family with an empty module.
fn step(
    machine: &mut Machine,
    labels: &LabelMap,
    instr: &Instr,
    ctx: &mut MinimalContext,
) -> Result<Option<Step>> {
    let module = Module::default();
    exec(machine, &module, labels, instr, 0, 0, ctx)
}

/// A float term `v` that lives in `ctx`'s arena.
fn float(ctx: &mut MinimalContext, v: f64) -> Eterm {
    make_float(ctx, v).expect("the arena allocates a float")
}

/// The four binary opcodes built from one operand layout.
fn fadd(fail: Label, lhs: Operand, rhs: Operand, dst: Operand) -> Instr {
    Instr::Fadd {
        fail,
        lhs,
        rhs,
        dst,
    }
}

fn fsub(fail: Label, lhs: Operand, rhs: Operand, dst: Operand) -> Instr {
    Instr::Fsub {
        fail,
        lhs,
        rhs,
        dst,
    }
}

fn fmul(fail: Label, lhs: Operand, rhs: Operand, dst: Operand) -> Instr {
    Instr::Fmul {
        fail,
        lhs,
        rhs,
        dst,
    }
}

fn fdiv(fail: Label, lhs: Operand, rhs: Operand, dst: Operand) -> Instr {
    Instr::Fdiv {
        fail,
        lhs,
        rhs,
        dst,
    }
}

// -- fmove ----------------------------------------------------------------

#[test]
fn fmove_moves_between_the_value_and_float_register_files() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let labels = no_labels();

    // X → FR.
    let a = float(&mut ctx, 1.5);
    machine.set_x(0, a).unwrap();
    let instr = Instr::Fmove {
        src: Operand::X(0),
        dst: Operand::FloatReg(0),
    };
    assert_eq!(
        step(&mut machine, &labels, &instr, &mut ctx).unwrap(),
        Some(Step::Next)
    );
    assert_eq!(machine.get_fr(0), Some(a));

    // FR → X (the `fstore` direction).
    let b = float(&mut ctx, -2.25);
    machine.set_fr(1, b).unwrap();
    let instr = Instr::Fmove {
        src: Operand::FloatReg(1),
        dst: Operand::X(1),
    };
    assert_eq!(
        step(&mut machine, &labels, &instr, &mut ctx).unwrap(),
        Some(Step::Next)
    );
    assert_eq!(machine.x(1), Some(b));

    // FR → FR.
    let instr = Instr::Fmove {
        src: Operand::FloatReg(0),
        dst: Operand::FloatReg(2),
    };
    assert_eq!(
        step(&mut machine, &labels, &instr, &mut ctx).unwrap(),
        Some(Step::Next)
    );
    assert_eq!(machine.get_fr(2), Some(a));
}

#[test]
fn fmove_rejects_an_out_of_range_float_register() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let instr = Instr::Fmove {
        src: Operand::X(0),
        dst: Operand::FloatReg(crate::machine::MAX_FLOAT_REGISTER),
    };
    assert!(step(&mut machine, &no_labels(), &instr, &mut ctx).is_err());
}

#[test]
fn fmove_of_a_missing_literal_is_a_typed_error() {
    // A `Module::default()` has no literals, so the index is out of range: the
    // documented typed error, never a panic.
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let instr = Instr::Fmove {
        src: Operand::Literal(0),
        dst: Operand::FloatReg(0),
    };
    assert!(step(&mut machine, &no_labels(), &instr, &mut ctx).is_err());
}

// -- fconv ----------------------------------------------------------------

#[test]
fn fconv_converts_small_integers_to_floats() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    machine.set_x(0, make_small(-7)).unwrap();
    let instr = Instr::Fconv {
        src: Operand::X(0),
        dst: Operand::FloatReg(0),
    };
    assert_eq!(
        step(&mut machine, &no_labels(), &instr, &mut ctx).unwrap(),
        Some(Step::Next)
    );
    let stored = machine.get_fr(0).expect("the FR slot is written");
    assert_eq!(float_value(stored).unwrap(), -7.0);
}

#[test]
fn fconv_passes_a_float_through_unchanged() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let f = float(&mut ctx, 3.5);
    machine.set_x(0, f).unwrap();
    let instr = Instr::Fconv {
        src: Operand::X(0),
        dst: Operand::FloatReg(3),
    };
    assert_eq!(
        step(&mut machine, &no_labels(), &instr, &mut ctx).unwrap(),
        Some(Step::Next)
    );
    assert_eq!(machine.get_fr(3), Some(f));
}

#[test]
fn fconv_of_a_non_number_raises_badarith() {
    // `fconv` has no fail label: a non-numeric source is a `badarith` raise.
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let instr = Instr::Fconv {
        src: Operand::X(0),
        dst: Operand::FloatReg(0),
    };
    machine.set_x(0, make_atom(Atom::from_raw(1))).unwrap();
    assert!(matches!(
        step(&mut machine, &no_labels(), &instr, &mut ctx),
        Err(BeamError::BadArith)
    ));

    machine.set_x(0, NIL).unwrap();
    assert!(matches!(
        step(&mut machine, &no_labels(), &instr, &mut ctx),
        Err(BeamError::BadArith)
    ));
}

#[test]
fn fconv_of_a_non_register_operand_is_not_masked_as_badarith() {
    // A `Literal` source that the module cannot resolve is a *structural* error,
    // not the arithmetic failure.
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let instr = Instr::Fconv {
        src: Operand::Literal(0),
        dst: Operand::FloatReg(0),
    };
    match step(&mut machine, &no_labels(), &instr, &mut ctx) {
        Err(BeamError::BadArith) => panic!("a structural error must not be badarith"),
        Err(_) => {}
        Ok(_) => panic!("expected an error"),
    }
}

#[test]
fn fconv_converts_a_representable_bignum_to_the_nearest_double() {
    // The sources below are *bignums* (outside the 60-bit immediate range), so
    // `fconv` takes the faithful `big_to_double` path rather than the small-int
    // cast. Every expected value is the oracle's:
    // `erl -noshell -eval 'io:format("~.17e~n",[float(1 bsl 60)])'` etc.
    let cases: [(i64, f64); 4] = [
        // 2^60 — exact.
        (1i64 << 60, 2.0f64.powi(60)),
        // 10^18 = 2^18 * 5^18 — exact.
        (1_000_000_000_000_000_000, 1.0e18),
        // 2^60 + 1 rounds down to the nearest double, 2^60.
        ((1i64 << 60) + 1, 2.0f64.powi(60)),
        // -(2^60) keeps the sign.
        (-(1i64 << 60), -(2.0f64.powi(60))),
    ];
    for (source_value, expected) in cases {
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let source =
            crate::terms::make_integer(&mut ctx, source_value).expect("the arena builds a bignum");
        assert!(!is_small(source), "{source_value} must be a bignum");
        machine.set_x(0, source).unwrap();
        let instr = Instr::Fconv {
            src: Operand::X(0),
            dst: Operand::FloatReg(0),
        };
        assert_eq!(
            step(&mut machine, &no_labels(), &instr, &mut ctx).unwrap(),
            Some(Step::Next)
        );
        let stored = machine.get_fr(0).expect("the FR slot is written");
        assert_eq!(
            float_value(stored).unwrap(),
            expected,
            "fconv({source_value})"
        );
    }
}

#[test]
fn fconv_rounds_a_small_int_beyond_2_pow_53_to_the_nearest_double() {
    // `2^53 + 1` is still an immediate (`MAX_SMALL = 2^59 - 1`), so `fconv` takes
    // the small-int cast; that cast must round to nearest, ties to even
    // (`float(9007199254740993)` = `9.0071992547409920e+15`).
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let source = make_small((1i64 << 53) + 1);
    assert!(is_small(source));
    machine.set_x(0, source).unwrap();
    let instr = Instr::Fconv {
        src: Operand::X(0),
        dst: Operand::FloatReg(0),
    };
    assert_eq!(
        step(&mut machine, &no_labels(), &instr, &mut ctx).unwrap(),
        Some(Step::Next)
    );
    let stored = machine.get_fr(0).expect("the FR slot is written");
    assert_eq!(float_value(stored).unwrap(), 2.0f64.powi(53));
}

// -- fadd / fsub / fmul / fdiv --------------------------------------------

#[test]
fn binary_opcodes_compute_into_the_destination_float_register() {
    let cases: [(fn(Label, Operand, Operand, Operand) -> Instr, f64, f64, f64); 4] = [
        (fadd, 1.5, 2.25, 3.75),
        (fsub, 1.5, 2.25, -0.75),
        (fmul, 1.5, 2.0, 3.0),
        (fdiv, 3.0, 2.0, 1.5),
    ];
    for (make, a, b, expected) in cases {
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        machine.set_fr(0, float(&mut ctx, a)).unwrap();
        machine.set_fr(1, float(&mut ctx, b)).unwrap();
        let instr = make(
            NO_FAIL,
            Operand::FloatReg(0),
            Operand::FloatReg(1),
            Operand::FloatReg(2),
        );
        assert_eq!(
            step(&mut machine, &no_labels(), &instr, &mut ctx).unwrap(),
            Some(Step::Next)
        );
        let stored = machine.get_fr(2).expect("the destination FR is written");
        assert_eq!(float_value(stored).unwrap(), expected);
    }
}

#[test]
fn division_by_zero_is_badarith_without_a_fail_label() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let instr = Instr::Fdiv {
        fail: NO_FAIL,
        lhs: Operand::FloatReg(0),
        rhs: Operand::FloatReg(1),
        dst: Operand::FloatReg(0),
    };

    // `1.0 / 0.0` is `+inf`.
    machine.set_fr(0, float(&mut ctx, 1.0)).unwrap();
    machine.set_fr(1, float(&mut ctx, 0.0)).unwrap();
    assert!(matches!(
        step(&mut machine, &no_labels(), &instr, &mut ctx),
        Err(BeamError::BadArith)
    ));

    // `0.0 / 0.0` is `NaN`.
    machine.set_fr(0, float(&mut ctx, 0.0)).unwrap();
    assert!(matches!(
        step(&mut machine, &no_labels(), &instr, &mut ctx),
        Err(BeamError::BadArith)
    ));
}

#[test]
fn a_non_finite_result_is_badarith() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    machine.set_fr(0, float(&mut ctx, f64::MAX)).unwrap();
    machine.set_fr(1, float(&mut ctx, f64::MAX)).unwrap();
    let instr = fmul(
        NO_FAIL,
        Operand::FloatReg(0),
        Operand::FloatReg(1),
        Operand::FloatReg(0),
    );
    assert!(matches!(
        step(&mut machine, &no_labels(), &instr, &mut ctx),
        Err(BeamError::BadArith)
    ));
}

#[test]
fn a_live_fail_label_turns_the_failure_into_a_branch() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    machine.set_fr(0, float(&mut ctx, 1.0)).unwrap();
    machine.set_fr(1, float(&mut ctx, 0.0)).unwrap();

    let mut labels = LabelMap::new();
    labels.insert(Label(7), (2u32, 9u32));
    let instr = Instr::Fdiv {
        fail: Label(7),
        lhs: Operand::FloatReg(0),
        rhs: Operand::FloatReg(1),
        dst: Operand::FloatReg(0),
    };
    assert_eq!(
        step(&mut machine, &labels, &instr, &mut ctx).unwrap(),
        Some(Step::Goto { func: 2, pc: 9 })
    );

    // A label the module does not carry is a typed error, not a panic.
    let missing = Instr::Fdiv {
        fail: Label(8),
        lhs: Operand::FloatReg(0),
        rhs: Operand::FloatReg(1),
        dst: Operand::FloatReg(0),
    };
    assert!(step(&mut machine, &labels, &missing, &mut ctx).is_err());
}

#[test]
fn a_non_float_operand_takes_the_failure_path() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    machine.set_fr(0, float(&mut ctx, 1.0)).unwrap();
    // FR1 still defaults to `NIL` — not a float.

    let raise = fadd(
        NO_FAIL,
        Operand::FloatReg(0),
        Operand::FloatReg(1),
        Operand::FloatReg(0),
    );
    assert!(matches!(
        step(&mut machine, &no_labels(), &raise, &mut ctx),
        Err(BeamError::BadArith)
    ));

    // With a live label it branches instead.
    let branch = fadd(
        Label(3),
        Operand::FloatReg(0),
        Operand::FloatReg(1),
        Operand::FloatReg(0),
    );
    let mut labels = LabelMap::new();
    labels.insert(Label(3), (0u32, 4u32));
    assert_eq!(
        step(&mut machine, &labels, &branch, &mut ctx).unwrap(),
        Some(Step::Goto { func: 0, pc: 4 })
    );
}

// -- catchability of the `badarith` raise ---------------------------------

#[test]
fn a_fconv_badarith_with_no_fail_label_lands_in_a_live_catch_frame() {
    // `fconv` has no fail label; a non-numeric source is the `$BADARITH0` raise
    // (`emu/float_instrs.tab:38-52`), which C takes through `next_catch`. With a
    // live `try` frame the engine must land it in the handler, not propagate.
    let module = module_with_raise_atoms();
    let badarith = module.atom_table.lookup("badarith").expect("interned");
    let error = module.atom_table.lookup("error").expect("interned");

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    // Frame 0 (the bottom frame) "owns" the handler at (func 4, pc 6).
    machine.push_frame(0).unwrap();
    machine.push_catch_frame(CatchFrame::new(4, 6, 0));

    // `fconv` on an atom (a non-number) raises `badarith`.
    machine.set_x(0, make_atom(Atom::from_raw(1))).unwrap();
    let instr = Instr::Fconv {
        src: Operand::X(0),
        dst: Operand::FloatReg(0),
    };
    let step = exec(&mut machine, &module, &no_labels(), &instr, 0, 0, &mut ctx).unwrap();
    assert_eq!(step, Some(Step::Goto { func: 4, pc: 6 }));

    // The C landing convention (beam_common.c:535-544): x0 = THE_NON_VALUE,
    // x1 = the reason (`badarith`), x3 = the exception class (`error`).
    assert_eq!(machine.x(0), Some(Eterm::non_value()));
    assert_eq!(machine.x(1), Some(make_atom(badarith)));
    assert_eq!(machine.x(3), Some(make_atom(error)));
    // The raise consults the innermost entry but does not pop it.
    assert_eq!(machine.catch_frame_depth(), 1);
}

#[test]
fn a_non_finite_binary_result_lands_in_a_live_catch_frame() {
    // The other half of the family: a *non-finite* `f*` result
    // (`$FLOAT_OP`'s finiteness check, no fail label) is the same catchable
    // `badarith` raise.
    let module = module_with_raise_atoms();
    let badarith = module.atom_table.lookup("badarith").expect("interned");

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    machine.push_frame(0).unwrap();
    machine.push_catch_frame(CatchFrame::new(2, 1, 0));

    // `f64::MAX * f64::MAX` overflows to `+inf`.
    machine.set_fr(0, float(&mut ctx, f64::MAX)).unwrap();
    machine.set_fr(1, float(&mut ctx, f64::MAX)).unwrap();
    let instr = fmul(
        NO_FAIL,
        Operand::FloatReg(0),
        Operand::FloatReg(1),
        Operand::FloatReg(2),
    );
    let step = exec(&mut machine, &module, &no_labels(), &instr, 0, 0, &mut ctx).unwrap();
    assert_eq!(step, Some(Step::Goto { func: 2, pc: 1 }));
    assert_eq!(machine.x(1), Some(make_atom(badarith)));
}

#[test]
fn a_badarith_without_a_catch_frame_is_the_typed_error() {
    // No live frame: the raise cannot land, so it propagates unchanged as the
    // typed `BeamError::BadArith` (the uncaught-error contract).
    let module = module_with_raise_atoms();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    machine.set_x(0, make_atom(Atom::from_raw(1))).unwrap();
    let instr = Instr::Fconv {
        src: Operand::X(0),
        dst: Operand::FloatReg(0),
    };
    assert!(matches!(
        exec(&mut machine, &module, &no_labels(), &instr, 0, 0, &mut ctx),
        Err(BeamError::BadArith)
    ));
    assert_eq!(machine.catch_frame_depth(), 0);
}

#[test]
fn a_live_fail_label_still_branches_instead_of_raising() {
    // The guard/`FAIL` path is unchanged: with a live label the arithmetic
    // failure is a branch (`Step::Goto`), even where a catch frame is live and
    // the atoms for a landing exist — no exception is raised or made pending.
    let module = module_with_raise_atoms();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    machine.push_frame(0).unwrap();
    machine.push_catch_frame(CatchFrame::new(4, 6, 0));
    machine.set_fr(0, float(&mut ctx, 1.0)).unwrap();
    machine.set_fr(1, float(&mut ctx, 0.0)).unwrap();

    let mut labels = LabelMap::new();
    labels.insert(Label(7), (2u32, 9u32));
    let instr = fdiv(
        Label(7),
        Operand::FloatReg(0),
        Operand::FloatReg(1),
        Operand::FloatReg(0),
    );
    assert_eq!(
        exec(&mut machine, &module, &labels, &instr, 0, 0, &mut ctx).unwrap(),
        Some(Step::Goto { func: 2, pc: 9 })
    );
    assert_eq!(machine.catch_frame_depth(), 1);
    assert_eq!(machine.pending_exception(), None);
}
// -- fnegate --------------------------------------------------------------

#[test]
fn fnegate_negates_a_float() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    machine.set_fr(0, float(&mut ctx, 2.5)).unwrap();
    let instr = Instr::Fnegate {
        fail: NO_FAIL,
        src: Operand::FloatReg(0),
        dst: Operand::FloatReg(1),
    };
    assert_eq!(
        step(&mut machine, &no_labels(), &instr, &mut ctx).unwrap(),
        Some(Step::Next)
    );
    assert_eq!(float_value(machine.get_fr(1).unwrap()).unwrap(), -2.5);

    // Negating `-0.0` yields `+0.0` (the sign bit flips).
    machine.set_fr(0, float(&mut ctx, -0.0)).unwrap();
    assert_eq!(
        step(&mut machine, &no_labels(), &instr, &mut ctx).unwrap(),
        Some(Step::Next)
    );
    let zero = float_value(machine.get_fr(1).unwrap()).unwrap();
    assert_eq!(zero, 0.0);
    assert!(!zero.is_sign_negative());
}

#[test]
fn fnegate_of_a_non_float_raises_or_branches() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let raise = Instr::Fnegate {
        fail: NO_FAIL,
        src: Operand::FloatReg(0),
        dst: Operand::FloatReg(1),
    };
    assert!(matches!(
        step(&mut machine, &no_labels(), &raise, &mut ctx),
        Err(BeamError::BadArith)
    ));

    let branch = Instr::Fnegate {
        fail: Label(5),
        src: Operand::FloatReg(0),
        dst: Operand::FloatReg(1),
    };
    let mut labels = LabelMap::new();
    labels.insert(Label(5), (1u32, 2u32));
    assert_eq!(
        step(&mut machine, &labels, &branch, &mut ctx).unwrap(),
        Some(Step::Goto { func: 1, pc: 2 })
    );
}

// -- the seam contract ----------------------------------------------------

#[test]
fn non_float_instructions_are_not_claimed() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    for instr in [
        Instr::Return,
        Instr::Move {
            src: Operand::X(0),
            dst: Operand::X(1),
        },
        Instr::Generic {
            op: beam_loader::Opcode::put_tuple2,
            offset: 0,
            operands: Vec::new(),
        },
    ] {
        assert_eq!(
            step(&mut machine, &no_labels(), &instr, &mut ctx).unwrap(),
            None
        );
    }
}

// -- end to end through the dispatch loop ---------------------------------

/// A one-function module whose `code` is `body`, exported as `name/arity`.
fn module_with(name: &str, arity: u32, body: Vec<Instr>) -> (Module, Atom) {
    let mut module = Module::default();
    module.name = module.atom_table.intern("floats");
    let function_name = module.atom_table.intern(name);
    let labels = body
        .iter()
        .enumerate()
        .filter_map(|(i, ins)| match ins {
            Instr::Label(l) => Some((*l, i)),
            _ => None,
        })
        .collect();
    let entry = Label(1);
    module.exports.push(beam_loader::Export {
        function: function_name,
        arity,
        label: entry,
    });
    module.functions.push(Function {
        func_label: entry,
        entry_label: entry,
        module: module.name,
        name: function_name,
        arity,
        labels,
        blocks: Vec::new(),
        code: body,
    });
    (module, function_name)
}

#[test]
fn a_full_float_program_runs_through_the_dispatch_loop() {
    // The leaf code shape the compiler emits for a guarded `A + B`: move both
    // args into FR slots, add into FR0, box the result back into X0.
    let (module, name) = module_with(
        "fadd",
        2,
        vec![
            Instr::Label(Label(1)),
            Instr::Fmove {
                src: Operand::X(0),
                dst: Operand::FloatReg(0),
            },
            Instr::Fmove {
                src: Operand::X(1),
                dst: Operand::FloatReg(1),
            },
            Instr::Fadd {
                fail: NO_FAIL,
                lhs: Operand::FloatReg(0),
                rhs: Operand::FloatReg(1),
                dst: Operand::FloatReg(0),
            },
            Instr::Fmove {
                src: Operand::FloatReg(0),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let a = float(&mut ctx, 0.5);
    let b = float(&mut ctx, 0.25);
    let result = machine
        .run_export(&module, name, 2, &[a, b], &mut ctx)
        .expect("0.5 + 0.25");
    assert_eq!(float_value(result).unwrap(), 0.75);
}

#[test]
fn a_fail_branch_inside_a_run_lands_on_the_target_label() {
    // `fdiv NO_FAIL` would raise; with a live label the run continues at the
    // handler, which yields `1` in x(0).
    let (module, name) = module_with(
        "div_or_one",
        2,
        vec![
            Instr::Label(Label(1)),
            Instr::Fmove {
                src: Operand::X(0),
                dst: Operand::FloatReg(0),
            },
            Instr::Fmove {
                src: Operand::X(1),
                dst: Operand::FloatReg(1),
            },
            Instr::Fdiv {
                fail: Label(2),
                lhs: Operand::FloatReg(0),
                rhs: Operand::FloatReg(1),
                dst: Operand::FloatReg(0),
            },
            Instr::Fmove {
                src: Operand::FloatReg(0),
                dst: Operand::X(0),
            },
            Instr::Return,
            Instr::Label(Label(2)),
            Instr::Move {
                src: Operand::Integer(1),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let one = float(&mut ctx, 1.0);
    let two = float(&mut ctx, 2.0);
    let result = machine
        .run_export(&module, name, 2, &[one, two], &mut ctx)
        .expect("1.0 / 2.0");
    assert_eq!(float_value(result).unwrap(), 0.5);

    // `1.0 / 0.0` takes the branch and yields the small int `1`.
    let zero = float(&mut ctx, 0.0);
    let mut machine = Machine::new();
    let result = machine
        .run_export(&module, name, 2, &[one, zero], &mut ctx)
        .expect("the fail label is taken");
    assert_eq!(result, make_small(1));
}
