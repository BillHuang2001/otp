//! Floating-point instruction family.
//!
//! Owns the float-register (FR) instructions (per `../beam_loader/CONTEXT.md`,
//! "The instruction vocabulary"):
//! [`Instr::Fmove`], [`Instr::Fconv`], [`Instr::Fadd`], [`Instr::Fsub`],
//! [`Instr::Fmul`], [`Instr::Fdiv`] and [`Instr::Fnegate`].
//!
//! # Ground truth (`../../erts/emulator/beam/emu/float_instrs.tab`)
//!
//! | IR variant | C body | semantics |
//! |---|---|---|
//! | `Fmove`   | `fload`/`fstore` (`emu/ops.tab:1337-1338`) | copy a float between an FR slot and a value register / literal |
//! | `Fconv`   | `i_fconv` (`:38-52`) | small int / bignum / float → FR; otherwise `badarith` (`$BADARITH0`) |
//! | `Fadd`/`Fsub`/`Fmul`/`Fdiv` | `i_f{add,sub,mul,div}` (`:62-76`) | `Dst = Src1 op Src2`; a **non-finite** result is `badarith` |
//! | `Fnegate` | `i_fnegate` (`:78-84`) | `Dst = -Src`; flipping a finite float's sign stays finite, so no finiteness check |
//!
//! # Representation (the M1 FR-file substitution)
//!
//! The C float-register file is raw doubles (`FloatDef f_reg_array[MAX_REG]`). At
//! Milestone 1 the engine's FR file stores **`Eterm` term slots** (see
//! [`Machine::get_fr`]/[`Machine::set_fr`] and `crate::machine`), so an FR slot
//! holds a *boxed float term*. Every read therefore converts the slot to `f64`
//! with [`crate::terms::float_value`] and every write stores a boxed float built by
//! [`crate::terms::make_float`]. For a genuine float this is observably identical to
//! the raw-double model: `fload`ing then `fstore`ing a value yields a float term the
//! C `fstore` would `HAlloc` the same way.
//!
//! # Errors (never a panic, never a wrong result)
//!
//! * A **non-float operand** or a **non-finite result** (including division by
//!   zero — `x/0.0` is `inf` or, for `0.0/0.0`, `NaN`) is the instruction's
//!   *arithmetic failure*: with a live `fail` label it becomes a guard branch
//!   ([`Step::Goto`]), otherwise it propagates [`BeamError::BadArith`] — the typed
//!   `badarith` the C `$BADARITH0` raise produces. The compiler only ever emits
//!   `NO_FAIL` for these opcodes (verified with `beam_disasm` against the oracle),
//!   so the live-label branch is engine-side robustness, exactly like `GcBif`.
//! * `Fconv` carries **no** fail label: a non-numeric source always raises
//!   [`BeamError::BadArith`]. A *bignum* source is a documented M1 coverage gap (no
//!   safe wide-integer → `f64` conversion exists yet) and yields a typed
//!   [`crate::error::unsupported`].
//! * An operand the shared seam cannot evaluate (out-of-range register, unsupported
//!   operand kind) propagates unchanged — that is a malformed/unsupported program,
//!   not a value error, and must not be masked as `badarith`.

use std::collections::BTreeMap;

use beam_common::{BeamError, Result};
use beam_loader::{Instr, Label, Module, Operand, NO_FAIL};
use beam_term::{is_small, signed_value, Eterm};

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::Machine;
use crate::operand::{read_operand, write_operand};
use crate::predicates;
use crate::terms::{float_value, make_float};
use crate::Step;

/// Execute a float-family instruction.
///
/// Returns `Ok(Some(step))` when `instr` is a float opcode this family executes,
/// `Ok(None)` when `instr` is not this family's variant, and `Err` on a raise.
pub(crate) fn exec(
    machine: &mut Machine,
    module: &Module,
    labels: &BTreeMap<Label, (u32, u32)>,
    instr: &Instr,
    func: u32,
    pc: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    // No instruction here stores a new (func, pc) of its own: control flow moves
    // only through the returned `Step` (the `fail` jump), so the loop's current
    // position is unused.
    let _ = (func, pc);

    match instr {
        // `fmove Src Dst` — FR ⇄ value register (and a float *literal* source).
        // Both directions are a single copy in the term-slot FR model.
        Instr::Fmove { src, dst } => {
            let value = read_value(machine, module, src)?;
            write_operand(machine, dst, value)?;
            Ok(Some(Step::Next))
        }
        // `fconv Src Dst` — integer/float → float register. No fail label.
        Instr::Fconv { src, dst } => {
            let source = read_value(machine, module, src)?;
            let value = conv_to_float(ctx, source)?;
            write_operand(machine, dst, value)?;
            Ok(Some(Step::Next))
        }
        Instr::Fadd {
            fail,
            lhs,
            rhs,
            dst,
        } => binary(
            machine,
            module,
            labels,
            *fail,
            lhs,
            rhs,
            dst,
            ctx,
            |a, b| a + b,
        ),
        Instr::Fsub {
            fail,
            lhs,
            rhs,
            dst,
        } => binary(
            machine,
            module,
            labels,
            *fail,
            lhs,
            rhs,
            dst,
            ctx,
            |a, b| a - b,
        ),
        Instr::Fmul {
            fail,
            lhs,
            rhs,
            dst,
        } => binary(
            machine,
            module,
            labels,
            *fail,
            lhs,
            rhs,
            dst,
            ctx,
            |a, b| a * b,
        ),
        Instr::Fdiv {
            fail,
            lhs,
            rhs,
            dst,
        } => binary(
            machine,
            module,
            labels,
            *fail,
            lhs,
            rhs,
            dst,
            ctx,
            |a, b| a / b,
        ),
        // `fnegate Fail Src Dst` — `Dst = -Src`. `i_fnegate` deliberately performs
        // no finiteness check: negating a finite float is always finite (and every
        // producer of a float value here rejects a non-finite result).
        Instr::Fnegate { fail, src, dst } => {
            let source = read_value(machine, module, src)?;
            let value = match float_value(source) {
                Ok(x) => x,
                Err(_) => return arithmetic_failure(labels, *fail),
            };
            let negated = make_float(ctx, -value)?;
            write_operand(machine, dst, negated)?;
            Ok(Some(Step::Next))
        }
        _ => Ok(None),
    }
}

/// The shared body of `fadd`/`fsub`/`fmul`/`fdiv`: `dst = op(lhs, rhs)`.
///
/// A non-float operand or a non-finite result is the arithmetic failure handled by
/// [`arithmetic_failure`] (`i_f*` bodies, `emu/float_instrs.tab:54-76`).
#[allow(clippy::too_many_arguments)] // the family seam's fixed parameter list
fn binary(
    machine: &mut Machine,
    module: &Module,
    labels: &BTreeMap<Label, (u32, u32)>,
    fail: Label,
    lhs: &Operand,
    rhs: &Operand,
    dst: &Operand,
    ctx: &mut dyn ExecutionContext,
    op: fn(f64, f64) -> f64,
) -> Result<Option<Step>> {
    let a = read_value(machine, module, lhs)?;
    let b = read_value(machine, module, rhs)?;
    let (x, y) = match (float_value(a), float_value(b)) {
        (Ok(x), Ok(y)) => (x, y),
        // A non-float FR slot is the documented arithmetic failure. A structurally
        // malformed program is a different error and was already propagated by the
        // `?` above.
        _ => return arithmetic_failure(labels, fail),
    };
    let result = op(x, y);
    if !result.is_finite() {
        return arithmetic_failure(labels, fail);
    }
    let value = make_float(ctx, result)?;
    write_operand(machine, dst, value)?;
    Ok(Some(Step::Next))
}

/// The arithmetic-failure outcome: the `fail`-label branch, or the `badarith` raise.
///
/// Mirrors `FAIL_HEAD_OR_BODY`/`$BADARITH0` (`emu/macros.tab:266-288`): a live label
/// makes the failure a guard failure (`Step::Goto`); `NO_FAIL` makes it the
/// `badarith` raise.
fn arithmetic_failure(labels: &BTreeMap<Label, (u32, u32)>, fail: Label) -> Result<Option<Step>> {
    if fail == NO_FAIL {
        return Err(BeamError::BadArith);
    }
    let (func, pc) = labels
        .get(&fail)
        .copied()
        .ok_or_else(|| error::unknown_label(fail))?;
    Ok(Some(Step::Goto { func, pc }))
}

/// `fconv`: a small integer or a float → the boxed float for `Dst`.
///
/// `fconv` carries no fail label, so every failure is a raise
/// (`emu/float_instrs.tab:38-52` calls `$BADARITH0`).
fn conv_to_float(ctx: &mut dyn ExecutionContext, source: Eterm) -> Result<Eterm> {
    if is_small(source) {
        return make_float(ctx, signed_value(source) as f64);
    }
    if predicates::is_float(source) {
        // Float → float is the C `LOAD_DOUBLE` path: the value is unchanged.
        return Ok(source);
    }
    if predicates::is_number(source) {
        // A bignum: the C path is `big_to_double`. M1 has no *safe* wide-integer →
        // `f64` conversion (`beam_term`'s is `unsafe`, and this module is safe
        // Rust), so this documented coverage gap is reported, never guessed.
        return Err(error::unsupported("fconv: bignum"));
    }
    Err(BeamError::BadArith)
}

/// Evaluate a float instruction's operand.
///
/// Everything but a static literal goes through the shared operand seam
/// ([`read_operand`]). A bare `Operand::Literal(index)` is the float family's own
/// `fmove {float,K} {fr,N}` source — the loader's IR documents `Fmove`'s source as
/// "a float register, value register **or float literal**", and the value lives in
/// the module's decoded literal area, which the shared seam does not resolve.
fn read_value(machine: &Machine, module: &Module, operand: &Operand) -> Result<Eterm> {
    match operand {
        Operand::Literal(index) => module
            .literal(*index)
            .ok_or_else(|| error::unsupported("float: literal index out of range")),
        other => read_operand(machine, other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use beam_loader::Function;
    use beam_term::{make_atom, make_small, Atom, NIL};

    use crate::context::MinimalContext;

    /// An empty label map (only the tests that exercise a `fail` target build one).
    fn no_labels() -> BTreeMap<Label, (u32, u32)> {
        BTreeMap::new()
    }

    /// Dispatch one instruction through the family with an empty module.
    fn step(
        machine: &mut Machine,
        labels: &BTreeMap<Label, (u32, u32)>,
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

        let mut labels = BTreeMap::new();
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
        let mut labels = BTreeMap::new();
        labels.insert(Label(3), (0u32, 4u32));
        assert_eq!(
            step(&mut machine, &labels, &branch, &mut ctx).unwrap(),
            Some(Step::Goto { func: 0, pc: 4 })
        );
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
        let mut labels = BTreeMap::new();
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
}
