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

//! The operand read/write seam.
//!
//! [`read_operand`] evaluates a decoded [`beam_loader::Operand`] to a term in the
//! current machine state; [`write_operand`] stores a term into the operand's
//! destination register or Y slot. Both are consumed by the interpreter
//! (`crate::interpreter`). Only the Milestone-1 operand subset is supported;
//! everything else is a documented typed error ([`crate::error::unsupported`]),
//! never a panic (crate `CONTEXT.md`, "Operand subset").

use beam_common::Result;
use beam_loader::{Operand, Reg};
use beam_term::{make_atom, Eterm, NIL};

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::Machine;
use crate::terms;

/// Evaluate a `.beam` operand to a term in the current machine state.
///
/// Supported: `X(n)`/`Y(n)`/`Typed{reg,..}` (the register's term, bounds-checked
/// to [`crate::error::bad_register`]), `FloatReg(n)` (the FR-file term, likewise
/// bounds-checked), `Integer(i)`/`Character(c)` (a small int when in range, an
/// exact bignum otherwise — [`crate::terms::make_integer`]), `Atom(a)`
/// (`make_atom`), `Nil` (`NIL`) and `Literal(idx)` (the running module's decoded
/// static literal via [`Machine::literal`]). `Label`/`List`/`Allocation` are the
/// documented Milestone-1 gaps and return a typed error.
///
/// The arms are ordered **hottest first** — the register/immediate kinds the
/// compiler emits for every real instruction — and the kinds it never emits as a
/// source fall through one `other` arm to a single typed error, so the common
/// operand is a bounds check and a load with no branch into the rare ones.
///
/// `#[inline(always)]`-marked: it is called several times per interpreted
/// instruction and the crate is built in many codegen units, so the hint is what
/// lets the register arms collapse into the dispatch loop's own unit — and, just
/// as importantly, what lets the optimizer see through the 72-byte
/// `Result<Eterm, BeamError>` return and keep the `Ok` value in a register on the
/// hot path (an out-of-line call pays that return through memory).
#[inline(always)]
pub(crate) fn read_operand(
    machine: &Machine,
    ctx: &mut dyn ExecutionContext,
    operand: &Operand,
) -> Result<Eterm> {
    match operand {
        Operand::X(n) => machine.x(*n).ok_or_else(|| error::bad_register(*n)),
        Operand::Integer(i) => terms::make_integer(ctx, *i),
        Operand::Nil => Ok(NIL),
        Operand::Y(n) => machine.y(*n).ok_or_else(|| error::bad_register(*n)),
        Operand::Atom(a) => Ok(make_atom(*a)),
        Operand::FloatReg(n) => machine.get_fr(*n).ok_or_else(|| error::bad_register(*n)),
        Operand::Typed { reg, .. } => read_register(machine, *reg),
        Operand::Character(c) => terms::make_integer(ctx, i64::from(*c)),
        // The module's decoded `LitT` literal at `idx`, bound to the machine for
        // the duration of the run. A missing/out-of-range index is a structural
        // error (the code references a literal the module does not define), never
        // a wrong term.
        Operand::Literal(idx) => machine
            .literal(*idx)
            .ok_or_else(|| error::unsupported("operand: literal")),
        other => Err(error::unsupported(operand_kind(other))),
    }
}

/// Write a term into the operand's destination register / Y slot.
///
/// Supported for `X(n)`/`Y(n)`/`Typed{reg,..}`/`FloatReg(n)`; anything else is a
/// typed error. A `Literal` is never a valid destination: it names a term in the
/// module's immutable literal area, which has no addressable slot. Hottest arm
/// first, like [`read_operand`].
///
/// `#[inline(always)]`-marked for the same reason as [`read_operand`]: it is
/// called several times per interpreted instruction (every `move`/`get_*`/`gc_bif`
/// destination) and, with only `#[inline]`, the much-larger-than-the-caller-type
/// `Result<(), BeamError>` return kept it out of line in the many-codegen-unit
/// build (measured: it was the single largest *out-of-line* self-time entry in the
/// interpreter profile). Inlining it into the dispatch loop lets the optimizer fold
/// the register store into the instruction arm.
#[inline(always)]
pub(crate) fn write_operand(machine: &mut Machine, operand: &Operand, value: Eterm) -> Result<()> {
    match operand {
        Operand::X(n) => machine.set_x(*n, value),
        Operand::Y(n) => machine.set_y(*n, value),
        Operand::FloatReg(n) => machine.set_fr(*n, value),
        Operand::Typed { reg, .. } => write_register(machine, *reg, value),
        Operand::Literal(_) => Err(error::unsupported("dst operand: literal")),
        _ => Err(error::unsupported("dst operand")),
    }
}

/// Read the term in register `reg` (the shared `X`/`Y`/`Typed` body).
#[inline]
fn read_register(machine: &Machine, reg: Reg) -> Result<Eterm> {
    match reg {
        Reg::X(n) => machine.x(n).ok_or_else(|| error::bad_register(n)),
        Reg::Y(n) => machine.y(n).ok_or_else(|| error::bad_register(n)),
    }
}

/// Write `value` into register `reg` (the shared `X`/`Y`/`Typed` body).
#[inline]
fn write_register(machine: &mut Machine, reg: Reg, value: Eterm) -> Result<()> {
    match reg {
        Reg::X(n) => machine.set_x(n, value),
        Reg::Y(n) => machine.set_y(n, value),
    }
}
/// A stable label for an unsupported operand kind (used in the error reason).
fn operand_kind(operand: &Operand) -> &'static str {
    match operand {
        Operand::Literal(_) => "operand: literal",
        Operand::Label(_) => "operand: label",
        Operand::List(_) => "operand: list",
        Operand::Allocation(_) => "operand: allocation",
        _ => "operand: unsupported",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MinimalContext;
    use crate::machine::Machine;
    use beam_loader::{Label, LiteralArea, Module, TypeIndex};
    use beam_term::{make_small, Atom, MAX_SMALL, MIN_SMALL};

    /// A module whose decoded `LitT` holds the small integer `42` (index 0) and the
    /// tuple `{ok,17}` (index 1) — hand-built ETF payloads, no loader involved.
    fn module_with_literals() -> Module {
        let mut module = Module::default();
        let raw = vec![
            vec![131, 97, 42],
            vec![131, 104, 2, 119, 2, 111, 107, 97, 17],
        ];
        let area = LiteralArea::decode(&raw, &mut module.atom_table).expect("decode literals");
        module.literals = std::sync::Arc::new(area);
        module
    }
    #[test]
    fn read_register_and_immediates() {
        let mut machine = Machine::with_max_register(4);
        machine.set_x(1, make_small(42)).unwrap();
        let mut ctx = MinimalContext::new();

        assert_eq!(
            read_operand(&machine, &mut ctx, &Operand::X(1)).unwrap(),
            make_small(42)
        );
        assert_eq!(read_operand(&machine, &mut ctx, &Operand::X(3)).unwrap(), NIL);
        assert_eq!(
            read_operand(&machine, &mut ctx, &Operand::Integer(7)).unwrap(),
            make_small(7)
        );
        assert_eq!(
            read_operand(&machine, &mut ctx, &Operand::Character(u32::from(b'a'))).unwrap(),
            make_small(97)
        );
        let a = Atom::from_raw(5);
        assert_eq!(
            read_operand(&machine, &mut ctx, &Operand::Atom(a)).unwrap(),
            make_atom(a)
        );
        assert_eq!(read_operand(&machine, &mut ctx, &Operand::Nil).unwrap(), NIL);
        assert_eq!(
            read_operand(
                &machine,
                &mut ctx,
                &Operand::Typed {
                    reg: beam_loader::Reg::X(1),
                    ty: TypeIndex(0)
                }
            )
            .unwrap(),
            make_small(42)
        );
    }

    #[test]
    fn read_out_of_range_register_is_a_typed_error() {
        let machine = Machine::with_max_register(4);
        let mut ctx = MinimalContext::new();
        assert!(read_operand(&machine, &mut ctx, &Operand::X(4)).is_err());
        // No frame: a Y operand is out of range too.
        assert!(read_operand(&machine, &mut ctx, &Operand::Y(0)).is_err());
    }

    #[test]
    fn read_operands_missing_a_module_are_typed_errors() {
        // No module is bound (no `run_*` in progress), so even a `Literal` cannot
        // be resolved; the other kinds have no reader at all.
        let machine = Machine::new();
        let mut ctx = MinimalContext::new();
        assert!(read_operand(&machine, &mut ctx, &Operand::Literal(0)).is_err());
        assert!(read_operand(&machine, &mut ctx, &Operand::Label(Label(1))).is_err());
        assert!(read_operand(&machine, &mut ctx, &Operand::list(vec![])).is_err());
        assert!(read_operand(&machine, &mut ctx, &Operand::alloc(vec![])).is_err());
    }

    #[test]
    fn literal_operands_resolve_through_the_bound_literal_area() {
        // The machine binds the running module's decoded `LitT` area; the operand
        // seam reads the term at the referenced index (never a wrong term).
        let module = module_with_literals();
        let mut machine = Machine::with_max_register(4);
        machine.set_literals(module.literals.clone());
        let mut ctx = MinimalContext::new();

        assert_eq!(
            read_operand(&machine, &mut ctx, &Operand::Literal(0)).unwrap(),
            make_small(42)
        );
        let tuple = read_operand(&machine, &mut ctx, &Operand::Literal(1)).unwrap();
        assert_eq!(crate::terms::tuple_arity(tuple).unwrap(), 2);
        assert_eq!(
            crate::terms::tuple_element(tuple, 0).unwrap(),
            make_atom(module.atom_table.lookup("ok").expect("ok is interned")),
        );
        assert_eq!(crate::terms::tuple_element(tuple, 1).unwrap(), make_small(17));

        // An index the module's `LitT` does not define is a structural error, not a
        // substituted term.
        let err = read_operand(&machine, &mut ctx, &Operand::Literal(2)).unwrap_err();
        match err {
            beam_common::BeamError::Raise(raise) => assert_eq!(
                raise.reason.as_atom(),
                Some("unsupported_opcode: operand: literal")
            ),
            other => panic!("expected the typed literal error, got {other:?}"),
        }
    }

    #[test]
    fn a_literal_is_never_a_valid_destination() {
        // A literal names a term in the module's immutable area — there is no
        // addressable destination slot, so a `Literal` dst stays a typed error.
        let module = module_with_literals();
        let mut machine = Machine::with_max_register(4);
        machine.set_literals(module.literals.clone());
        assert!(write_operand(&mut machine, &Operand::Literal(0), make_small(1)).is_err());
    }
    #[test]
    fn float_register_round_trips_through_the_fr_file() {
        let mut machine = Machine::with_max_register(4);
        let mut ctx = MinimalContext::new();
        // The FR file defaults to NIL.
        assert_eq!(
            read_operand(&machine, &mut ctx, &Operand::FloatReg(0)).unwrap(),
            Eterm::default()
        );

        // set_fr then read_operand(FloatReg(..)) returns the set term.
        let v = make_small(1234);
        machine.set_fr(0, v).unwrap();
        assert_eq!(read_operand(&machine, &mut ctx, &Operand::FloatReg(0)).unwrap(), v);

        // write_operand(FloatReg(..), v) stores into the FR file.
        let w = make_small(5678);
        write_operand(&mut machine, &Operand::FloatReg(1), w).unwrap();
        assert_eq!(machine.get_fr(1), Some(w));
    }

    #[test]
    fn out_of_range_float_register_is_a_typed_error() {
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let err = read_operand(
            &machine,
            &mut ctx,
            &Operand::FloatReg(crate::machine::MAX_FLOAT_REGISTER),
        );
        assert!(matches!(err, Err(beam_common::BeamError::BadArg)));
        let err = write_operand(
            &mut machine,
            &Operand::FloatReg(crate::machine::MAX_FLOAT_REGISTER),
            NIL,
        );
        assert!(matches!(err, Err(beam_common::BeamError::BadArg)));
    }

    #[test]
    #[allow(unsafe_code)]
    fn integer_operands_materialize_smalls_and_exact_bignums() {
        let machine = Machine::with_max_register(4);
        let mut ctx = MinimalContext::new();

        // In range: a small int, exactly as before.
        for v in [0i64, 1, -1, MAX_SMALL, MIN_SMALL] {
            let t = read_operand(&machine, &mut ctx, &Operand::Integer(v)).unwrap();
            assert!(beam_term::is_small(t), "{v} must stay a small int");
            assert_eq!(beam_term::signed_value(t), v);
        }

        // Out of range (reachable from real `erlc` output): an exact bignum, never a
        // panic and never a truncated small int.
        let cases = [
            (MAX_SMALL + 1, false),
            (999_999_999_999_999_999, false),
            (MIN_SMALL - 1, true),
        ];
        for (v, negative) in cases {
            let t = read_operand(&machine, &mut ctx, &Operand::Integer(v)).unwrap();
            assert!(!beam_term::is_small(t), "{v} must not be a small int");
            // SAFETY: `t` is the live bignum `read_operand` just wrote through `ctx`.
            let (neg, digits) = unsafe { beam_term::big_parts(t) };
            assert_eq!(neg, negative, "{v}");
            assert_eq!(digits, [v.unsigned_abs()], "{v}");
        }
    }

    #[test]
    fn write_registers_and_reject_non_destinations() {
        let mut machine = Machine::with_max_register(4);
        write_operand(&mut machine, &Operand::X(2), make_small(9)).unwrap();
        assert_eq!(machine.x(2), Some(make_small(9)));

        write_operand(
            &mut machine,
            &Operand::Typed {
                reg: beam_loader::Reg::X(3),
                ty: TypeIndex(1),
            },
            make_small(11),
        )
        .unwrap();
        assert_eq!(machine.x(3), Some(make_small(11)));

        // A Y operand with no frame is an out-of-range register.
        assert!(write_operand(&mut machine, &Operand::Y(0), NIL).is_err());
        // Non-register destinations are unsupported.
        assert!(write_operand(&mut machine, &Operand::Integer(1), NIL).is_err());
        assert!(write_operand(&mut machine, &Operand::Nil, NIL).is_err());
    }
}
