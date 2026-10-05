//! The operand read/write seam.
//!
//! [`read_operand`] evaluates a decoded [`beam_loader::Operand`] to a term in the
//! current machine state; [`write_operand`] stores a term into the operand's
//! destination register or Y slot. Both are consumed by the interpreter
//! (`crate::interpreter`); the manager fills in the operand bodies (only the
//! Milestone-1 operand subset is supported, everything else is a typed error).

use beam_common::Result;
use beam_loader::Operand;
use beam_term::Eterm;

use crate::machine::Machine;

/// Evaluate a `.beam` operand to a term in the current machine state.
///
/// Skeleton: the manager implements the operand cases. Until then this returns the
/// documented coverage-limit error and never panics. Consumed by the interpreter
/// dispatch loop once it lands.
#[allow(dead_code)]
pub(crate) fn read_operand(machine: &Machine, operand: &Operand) -> Result<Eterm> {
    let _ = (machine, operand);
    Err(crate::error::unsupported("read_operand"))
}

/// Write a term into the operand's destination register / Y slot.
///
/// Skeleton: the manager implements the operand cases. Until then this returns the
/// documented coverage-limit error and never panics. Consumed by the interpreter
/// dispatch loop once it lands.
#[allow(dead_code)]
pub(crate) fn write_operand(machine: &mut Machine, operand: &Operand, value: Eterm) -> Result<()> {
    let _ = (machine, operand, value);
    Err(crate::error::unsupported("write_operand"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_operand_is_a_typed_error_for_now() {
        let machine = Machine::new();
        assert!(read_operand(&machine, &Operand::X(0)).is_err());
        assert!(read_operand(&machine, &Operand::Nil).is_err());
    }

    #[test]
    fn write_operand_is_a_typed_error_for_now() {
        let mut machine = Machine::new();
        assert!(write_operand(&mut machine, &Operand::X(0), Eterm::nil()).is_err());
    }
}
