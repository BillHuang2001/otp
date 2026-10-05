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
use beam_term::{make_atom, make_small, Eterm, NIL};

use crate::error;
use crate::machine::Machine;

/// Evaluate a `.beam` operand to a term in the current machine state.
///
/// Supported: `X(n)`/`Y(n)`/`Typed{reg,..}` (the register's term, bounds-checked
/// to [`crate::error::bad_register`]), `Integer(i)`/`Character(c)` (`make_small`),
/// `Atom(a)` (`make_atom`) and `Nil` (`NIL`). `FloatReg`/`Literal`/`Label`/
/// `List`/`Allocation` are the documented Milestone-1 gaps and return a typed
/// error.
pub(crate) fn read_operand(machine: &Machine, operand: &Operand) -> Result<Eterm> {
    match operand {
        Operand::X(n) => machine.x(*n).ok_or_else(|| error::bad_register(*n)),
        Operand::Y(n) => machine.y(*n).ok_or_else(|| error::bad_register(*n)),
        Operand::Typed { reg, .. } => read_register(machine, *reg),
        Operand::Integer(i) => Ok(make_small(*i)),
        Operand::Atom(a) => Ok(make_atom(*a)),
        Operand::Nil => Ok(NIL),
        Operand::Character(c) => Ok(make_small(i64::from(*c))),
        other => Err(error::unsupported(operand_kind(other))),
    }
}

/// Write a term into the operand's destination register / Y slot.
///
/// Supported for `X(n)`/`Y(n)`/`Typed{reg,..}`; anything else is a typed error.
pub(crate) fn write_operand(machine: &mut Machine, operand: &Operand, value: Eterm) -> Result<()> {
    match operand {
        Operand::X(n) => machine.set_x(*n, value),
        Operand::Y(n) => machine.set_y(*n, value),
        Operand::Typed { reg, .. } => write_register(machine, *reg, value),
        _ => Err(error::unsupported("dst operand")),
    }
}

/// Read the term in register `reg` (the shared `X`/`Y`/`Typed` body).
fn read_register(machine: &Machine, reg: Reg) -> Result<Eterm> {
    match reg {
        Reg::X(n) => machine.x(n).ok_or_else(|| error::bad_register(n)),
        Reg::Y(n) => machine.y(n).ok_or_else(|| error::bad_register(n)),
    }
}

/// Write `value` into register `reg` (the shared `X`/`Y`/`Typed` body).
fn write_register(machine: &mut Machine, reg: Reg, value: Eterm) -> Result<()> {
    match reg {
        Reg::X(n) => machine.set_x(n, value),
        Reg::Y(n) => machine.set_y(n, value),
    }
}

/// A stable label for an unsupported operand kind (used in the error reason).
fn operand_kind(operand: &Operand) -> &'static str {
    match operand {
        Operand::FloatReg(_) => "operand: float register",
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
    use crate::machine::Machine;
    use beam_loader::{Label, TypeIndex};
    use beam_term::Atom;

    #[test]
    fn read_register_and_immediates() {
        let mut machine = Machine::with_max_register(4);
        machine.set_x(1, make_small(42)).unwrap();

        assert_eq!(
            read_operand(&machine, &Operand::X(1)).unwrap(),
            make_small(42)
        );
        assert_eq!(read_operand(&machine, &Operand::X(3)).unwrap(), NIL);
        assert_eq!(
            read_operand(&machine, &Operand::Integer(7)).unwrap(),
            make_small(7)
        );
        assert_eq!(
            read_operand(&machine, &Operand::Character(u32::from(b'a'))).unwrap(),
            make_small(97)
        );
        let a = Atom::from_raw(5);
        assert_eq!(
            read_operand(&machine, &Operand::Atom(a)).unwrap(),
            make_atom(a)
        );
        assert_eq!(read_operand(&machine, &Operand::Nil).unwrap(), NIL);
        assert_eq!(
            read_operand(
                &machine,
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
        assert!(read_operand(&machine, &Operand::X(4)).is_err());
        // No frame: a Y operand is out of range too.
        assert!(read_operand(&machine, &Operand::Y(0)).is_err());
    }

    #[test]
    fn read_unsupported_operands_are_typed_errors() {
        let machine = Machine::new();
        assert!(read_operand(&machine, &Operand::FloatReg(0)).is_err());
        assert!(read_operand(&machine, &Operand::Literal(0)).is_err());
        assert!(read_operand(&machine, &Operand::Label(Label(1))).is_err());
        assert!(read_operand(&machine, &Operand::List(vec![])).is_err());
        assert!(read_operand(&machine, &Operand::Allocation(vec![])).is_err());
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
