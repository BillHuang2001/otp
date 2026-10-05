//! Exception / raise instruction family.
//!
//! Owns the in-VM exception instructions (per `../beam_loader/CONTEXT.md`, "The
//! instruction vocabulary"):
//! - [`Instr::Badmatch`] — raise `{badmatch,Value}`
//! - [`Instr::CaseEnd`] — raise `{case_clause,Value}`
//! - [`Instr::IfEnd`] — raise `if_clause`
//! - [`Instr::TryCaseEnd`] — raise `{try_clause,Value}`
//! - [`Instr::Badrecord`] — raise `{badrecord,Value}`
//! - [`Instr::Raise`] — re-raise with the class of the exception being handled
//! - [`Instr::Try`] / [`Instr::Catch`] — push a catch frame
//! - [`Instr::TryCase`] / [`Instr::TryEnd`] / [`Instr::CatchEnd`] — the handler
//!   opcodes (each consumes exactly one catch frame)
//! - [`Instr::BuildStacktrace`] / [`Instr::RawRaise`] — documented `unsupported`
//!
//! ## Semantics (C ground truth, `emu/instrs.tab:1137-1263`, `beam_common.c:735-778`)
//!
//! The raising opcodes set a pending exception and **expand the reason at raise
//! time** (`expand_error_value`): `badmatch` ⇒ `{badmatch,V}`, `case_end` ⇒
//! `{case_clause,V}`, `try_case_end` ⇒ `{try_clause,V}`, `badrecord` ⇒
//! `{badrecord,V}`, `if_end` ⇒ the bare atom `if_clause`. All are class `error`.
//! The raise then goes through the one uniform machine entry point
//! [`Machine::throw`]: it consults the innermost catch frame, unwinds the call
//! frames to the frame owning the handler and lands the exception in the handler
//! registers (`x(0) = THE_NON_VALUE`, `x(1) = reason`, `x(2) = trace`,
//! `x(3) = class atom`), yielding a `Step::Goto` to the handler. With no live
//! catch frame it propagates as `Err(BeamError::Raise(..))` — never a panic.
//!
//! The catch frame a `try`/`catch` pushed is **not** popped by the raise; the
//! handler opcode that runs next pops exactly one entry (the normal or the
//! exceptional path — never both). `try_case` blanks its frame slot, moves the
//! class atom from `x(3)` into `x(0)` and requires the `THE_NON_VALUE` marker in
//! `x(0)`; `try_end` just blonks its slot; `catch_end` leaves a normal result
//! untouched but folds a landed exception into the caught term — the thrown value
//! for class `throw`, `{'EXIT',Reason}` otherwise.
//!
//! The **class atom** (`error`/`exit`/`throw`) is installed from the running
//! module's atom table before a raise ([`install_class_atom`]) so a landing can
//! place it in `x(3)`. An atom absent from the module atom table is never
//! substituted by a *different* atom: an uncaught raise still propagates as a
//! typed error, and a caught raise fails with a typed `unsupported`.
//!
//! **Milestone-1 limits (all documented, never a wrong result):**
//! - No stacktrace is captured, so `catch_end` builds `{'EXIT',Reason}` without
//!   the stacktrace element the real runtime appends to the reason (C's
//!   `add_stacktrace`); a `catch` whose handler would need a stacktrace only ever
//!   observes a shorter (but otherwise identical) reason term.
//! - `catch_end` cannot build `{'EXIT',V}` in a module whose atom table lacks
//!   `'EXIT'` (the loader exposes only the module's atoms); it returns a typed
//!   `unsupported` rather than substituting a wrong atom.
//! - [`Instr::BuildStacktrace`] / [`Instr::RawRaise`] (the NIF error-return path)
//!   are `unsupported`: the engine has no raw-stacktrace/class protocol yet.
//! - The re-raise opcode [`Instr::Raise`] requires a class to be currently
//!   handled ([`Machine::pending_class`]); the compiler emits it only as the
//!   "no catch clause matched" tail of a handler, where that always holds
//!   (`lib/compiler/src/beam_ssa_*.erl` `resume` ⇒ `{bif,raise,..}`).

use std::collections::BTreeMap;

use beam_common::{ExceptionClass, Result};
use beam_loader::{Instr, Label, Module, Operand};
use beam_term::{is_nil, make_atom, Eterm, NIL};

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::{CatchFrame, Machine};
use crate::operand::{read_operand, write_operand};
use crate::terms;
use crate::Step;

/// Execute an exception-family instruction.
///
/// Returns `Ok(Some(step))` when `instr` is an exception opcode this family
/// executes, `Ok(None)` when `instr` is not this family's variant, and `Err` on a
/// raise.
pub(crate) fn exec(
    machine: &mut Machine,
    module: &Module,
    labels: &BTreeMap<Label, (u32, u32)>,
    instr: &Instr,
    _func: u32,
    _pc: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    match instr {
        // `try Frame Fail` / `catch Frame Fail` (`catch` is a macro for `try`):
        // push the handler's catch frame, blank the frame slot (`catches++`, `Y =
        // Fail` in C) and fall through.
        Instr::Try { frame, fail } | Instr::Catch { frame, fail } => {
            let (handler_func, handler_pc) = labels
                .get(fail)
                .copied()
                .ok_or_else(|| error::unknown_label(*fail))?;
            // The handler lives in the frame that is current *now* (an unwind in
            // `Machine::throw` drops every deeper frame). `saturating_sub` keeps a
            // frame-less machine a typed error rather than a panic.
            let frame_index = machine.frame_depth().saturating_sub(1);
            machine.push_catch_frame(CatchFrame::new(handler_func, handler_pc, frame_index));
            write_operand(machine, frame, NIL)?;
            Ok(Some(Step::Next))
        }

        // `badmatch Src` ⇒ `{badmatch,Src}` (class error).
        Instr::Badmatch { value } => {
            raise_error_tuple(machine, module, ctx, "badmatch", value)
        }
        // `case_end Src` ⇒ `{case_clause,Src}` (class error).
        Instr::CaseEnd { value } => raise_error_tuple(machine, module, ctx, "case_clause", value),
        // `try_case_end Src` ⇒ `{try_clause,Src}` (class error).
        Instr::TryCaseEnd { value } => raise_error_tuple(machine, module, ctx, "try_clause", value),
        // `badrecord Src` ⇒ `{badrecord,Src}` (class error).
        Instr::Badrecord { value } => raise_error_tuple(machine, module, ctx, "badrecord", value),
        // `if_end` ⇒ the bare atom `if_clause` (class error).
        Instr::IfEnd => raise_error_atom(machine, module, "if_clause"),

        // `raise Trace Value` — the re-raise tail of a handler (`{bif,raise,...}`).
        // C's `i_raise` copies the reason + raw stacktrace but never touches
        // `freason`, so the class of the exception currently being handled is kept.
        Instr::Raise { trace, value } => {
            let value = read_operand(machine, value)?;
            let trace = read_operand(machine, trace)?;
            let class = machine.pending_class().ok_or_else(|| {
                error::unsupported("exceptions: raise with no exception being handled")
            })?;
            let _ = install_class_atom(machine, module, class);
            machine.throw_with_trace(class, value, trace).map(Some)
        }

        // `try_case Frame` — the handler entry of a `try`. C:
        // `catch--; make_blank(Y); assert(is_non_value(x(0))); x(0) = x(3)`.
        Instr::TryCase { frame } => {
            machine.pop_catch_frame_checked("try_case: unbalanced catch frames")?;
            write_operand(machine, frame, NIL)?;
            if machine.x(0) != Some(Eterm::non_value()) {
                return Err(error::unsupported(
                    "try_case: x(0) is not THE_NON_VALUE (no exception landed)",
                ));
            }
            let class = machine
                .x(3)
                .filter(|term| !is_nil(*term))
                .ok_or_else(|| error::unsupported("try_case: x(3) holds no exception class"))?;
            machine.set_x(0, class)?;
            Ok(Some(Step::Next))
        }

        // `try_end Frame` — the normal-exit of a `try`: `catch--; make_blank(Y)`.
        Instr::TryEnd { frame } => {
            machine.pop_catch_frame_checked("try_end: unbalanced catch frames")?;
            write_operand(machine, frame, NIL)?;
            Ok(Some(Step::Next))
        }

        // `catch_end Frame` — fold the old-style `catch`: a normal result passes
        // through unchanged; a landed exception becomes the thrown value (class
        // `throw`) or `{'EXIT',Reason}` (classes `error`/`exit`).
        Instr::CatchEnd { frame } => {
            machine.pop_catch_frame_checked("catch_end: unbalanced catch frames")?;
            write_operand(machine, frame, NIL)?;
            if machine.x(0) == Some(Eterm::non_value()) {
                let class = machine
                    .x(3)
                    .ok_or_else(|| error::unsupported("catch_end: x(3) out of range"))?;
                let reason = machine
                    .x(1)
                    .ok_or_else(|| error::unsupported("catch_end: x(1) out of range"))?;
                let throw_atom = module.atom_table.lookup("throw").map(make_atom);
                if throw_atom == Some(class) {
                    // Class `throw`: the caught value is the thrown term itself.
                    machine.set_x(0, reason)?;
                } else {
                    // Classes `error`/`exit` ⇒ `{'EXIT',Reason}`. M1 appends no
                    // stacktrace (C's `add_stacktrace`).
                    let exit_atom = module.atom_table.lookup("EXIT").ok_or_else(|| {
                        error::unsupported(
                            "catch_end: module atom table lacks the 'EXIT' atom",
                        )
                    })?;
                    let tuple = terms::make_tuple(ctx, &[make_atom(exit_atom), reason])?;
                    machine.set_x(0, tuple)?;
                }
            }
            Ok(Some(Step::Next))
        }

        // The NIF error-return protocol (cook a raw stacktrace / raise from
        // `x(0..2)`), out of Milestone-1 scope.
        Instr::BuildStacktrace => Err(error::unsupported("build_stacktrace")),
        Instr::RawRaise => Err(error::unsupported("raw_raise")),

        // Not this family's variant.
        _ => Ok(None),
    }
}

/// Raise a `{Tag,Value}` `error` exception (the `badmatch`/`case_end`/
/// `try_case_end`/`badrecord` expansion).
///
/// The tag atom is resolved from the running module's atom table; a module that
/// never mentions it cannot name the reason, so a missing atom is a typed
/// `unsupported` (never a wrong atom). The `error` class atom is installed when
/// the module has it (see [`install_class_atom`]); an uncaught raise therefore
/// still propagates as a proper `Err(BeamError::Raise(..))`.
fn raise_error_tuple(
    machine: &mut Machine,
    module: &Module,
    ctx: &mut dyn ExecutionContext,
    tag: &str,
    value: &Operand,
) -> Result<Option<Step>> {
    let value = read_operand(machine, value)?;
    let tag_atom = module
        .atom_table
        .lookup(tag)
        .ok_or_else(|| error::unsupported(&format!("exceptions: no '{tag}' atom in the module")))?;
    let reason = terms::make_tuple(ctx, &[make_atom(tag_atom), value])?;
    let _ = install_class_atom(machine, module, ExceptionClass::Error);
    machine.throw_error(reason).map(Some)
}

/// Raise a bare-atom `error` exception (`if_end` ⇒ the atom `if_clause`).
fn raise_error_atom(machine: &mut Machine, module: &Module, tag: &str) -> Result<Option<Step>> {
    let atom = module
        .atom_table
        .lookup(tag)
        .ok_or_else(|| error::unsupported(&format!("exceptions: no '{tag}' atom in the module")))?;
    let reason = make_atom(atom);
    let _ = install_class_atom(machine, module, ExceptionClass::Error);
    machine.throw_error(reason).map(Some)
}

/// Install `class`'s atom from the module atom table into the machine.
///
/// Returns the atom term on success; when the atom is absent from the module
/// atom table the machine keeps its previous (or unset) value and a typed
/// `unsupported` is returned. Callers that must not spend the error (a raise
/// landing decides lazily) ignore it; a `throw` with no installed atom then fails
/// with the same typed error at landing time, rather than substituting a
/// different, wrong atom.
fn install_class_atom(
    machine: &mut Machine,
    module: &Module,
    class: ExceptionClass,
) -> Result<Eterm> {
    match module.atom_table.lookup(class.as_str()) {
        Some(atom) => {
            let term = make_atom(atom);
            machine.set_class_atom(class, term);
            Ok(term)
        }
        None => Err(error::unsupported(&format!(
            "exceptions: no '{}' atom for the exception class in the module",
            class.as_str()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MinimalContext;
    use beam_common::BeamError;
    use beam_loader::{Function, HeapNeed};
    use beam_term::{cmp, make_small};

    /// A tiny module builder with an owned atom table (mirrors the `Fixture`
    /// scaffolding in `tests/opcodes.rs`, kept local so the unit tests here do not
    /// depend on integration-test helpers).
    struct Fixture {
        module: Module,
    }

    impl Fixture {
        fn new(extra_atoms: &[&str]) -> Fixture {
            let mut module = Module::default();
            module.name = module.atom_table.intern("m");
            for name in extra_atoms {
                module.atom_table.intern(name);
            }
            Fixture { module }
        }

        /// Append a function whose code starts with `entry`; returns its label.
        fn add_function(&mut self, name: &str, arity: u32, code: Vec<Instr>) -> Label {
            let name = self.module.atom_table.intern(name);
            let module = self.module.name;
            let mut labels = BTreeMap::new();
            for (index, instr) in code.iter().enumerate() {
                if let Instr::Label(label) = instr {
                    labels.insert(*label, index);
                }
            }
            let entry = labels.keys().next().copied().unwrap_or(Label(0));
            self.module.functions.push(Function {
                func_label: entry,
                entry_label: entry,
                module,
                name,
                arity,
                code,
                blocks: Vec::new(),
                labels,
            });
            entry
        }

        fn finish(self) -> Module {
            self.module
        }
    }

    /// Empty label map; fine for instructions that raise (they do not jump).
    fn no_labels() -> BTreeMap<Label, (u32, u32)> {
        BTreeMap::new()
    }

    /// Run one instruction body with a machine that has a bottom frame.
    fn exec_one(module: &Module, instr: &Instr, machine: &mut Machine, ctx: &mut MinimalContext) -> Result<Option<Step>> {
        exec(machine, module, &no_labels(), instr, 0, 0, ctx)
    }

    /// Assert that `reason` (an `Eterm`) equals the tuple `[tag, elem]`.
    fn assert_reason_tuple(cmp_ctx: &mut MinimalContext, module: &Module, reason: Eterm, tag: &str, elem: Eterm) {
        let expected = terms::make_tuple(cmp_ctx, &[make_atom(module.atom_table.lookup(tag).unwrap()), elem]).unwrap();
        assert!(
            cmp(reason, expected, &module.atom_table).is_eq(),
            "reason does not equal {{{tag}, ..}}"
        );
    }

    // ------------------------------------------------------------------
    // try / catch push a frame
    // ------------------------------------------------------------------

    #[test]
    fn try_pushes_a_catch_frame_and_blanks_the_slot() {
        let module = Fixture::new(&["case_clause", "error"]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        let mut ctx = MinimalContext::new();
        let mut labels = BTreeMap::new();
        labels.insert(Label(3), (0u32, 7u32));

        let step = exec(
            &mut machine,
            &module,
            &labels,
            &Instr::Try {
                frame: Operand::X(1),
                fail: Label(3),
            },
            0,
            0,
            &mut ctx,
        )
        .unwrap();
        assert_eq!(step, Some(Step::Next));
        assert_eq!(machine.catch_frame_depth(), 1);
        assert_eq!(machine.x(1), Some(NIL));
    }

    #[test]
    fn catch_is_the_same_as_try() {
        let module = Fixture::new(&[]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        let mut ctx = MinimalContext::new();
        let mut labels = BTreeMap::new();
        labels.insert(Label(9), (0u32, 4u32));
        let step = exec(
            &mut machine,
            &module,
            &labels,
            &Instr::Catch {
                frame: Operand::X(0),
                fail: Label(9),
            },
            0,
            0,
            &mut ctx,
        )
        .unwrap();
        assert_eq!(step, Some(Step::Next));
        assert_eq!(machine.catch_frame_depth(), 1);
    }

    #[test]
    fn try_with_an_unknown_fail_label_is_a_typed_error() {
        let module = Fixture::new(&[]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        let mut ctx = MinimalContext::new();
        let err = exec(
            &mut machine,
            &module,
            &no_labels(),
            &Instr::Try {
                frame: Operand::X(1),
                fail: Label(42),
            },
            0,
            0,
            &mut ctx,
        )
        .unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
        assert_eq!(machine.catch_frame_depth(), 0);
    }

    // ------------------------------------------------------------------
    // Raising opcodes (uncaught ⇒ Err, reason expanded + recorded)
    // ------------------------------------------------------------------

    #[test]
    fn badmatch_raises_the_expanded_tuple_uncaught() {
        let module = Fixture::new(&["badmatch", "error"]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        let mut ctx = MinimalContext::new();
        let err = exec_one(
            &module,
            &Instr::Badmatch {
                value: Operand::Integer(7),
            },
            &mut machine,
            &mut ctx,
        )
        .unwrap_err();
        assert!(matches!(err, BeamError::Raise(_)));
        let (class, reason) = machine.pending_exception().expect("a reason is recorded");
        assert_eq!(class, ExceptionClass::Error);
        assert_reason_tuple(&mut ctx, &module, reason, "badmatch", make_small(7));
    }

    #[test]
    fn case_end_if_end_badrecord_and_try_case_end_expand_their_reasons() {
        // case_end ⇒ {case_clause,V}
        let module = Fixture::new(&["case_clause"]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        let mut ctx = MinimalContext::new();
        let _ = exec_one(
            &module,
            &Instr::CaseEnd {
                value: Operand::Integer(2),
            },
            &mut machine,
            &mut ctx,
        );
        let (_, reason) = machine.pending_exception().unwrap();
        assert_reason_tuple(&mut ctx, &module, reason, "case_clause", make_small(2));

        // if_end ⇒ the bare atom if_clause
        let module = Fixture::new(&["if_clause"]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        let _ = exec_one(&module, &Instr::IfEnd, &mut machine, &mut ctx);
        let (_, reason) = machine.pending_exception().unwrap();
        assert_eq!(reason, make_atom(module.atom_table.lookup("if_clause").unwrap()));

        // badrecord ⇒ {badrecord,V}
        let module = Fixture::new(&["badrecord"]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        let _ = exec_one(
            &module,
            &Instr::Badrecord {
                value: Operand::Integer(3),
            },
            &mut machine,
            &mut ctx,
        );
        let (_, reason) = machine.pending_exception().unwrap();
        assert_reason_tuple(&mut ctx, &module, reason, "badrecord", make_small(3));

        // try_case_end ⇒ {try_clause,V}
        let module = Fixture::new(&["try_clause"]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        let _ = exec_one(
            &module,
            &Instr::TryCaseEnd {
                value: Operand::Integer(4),
            },
            &mut machine,
            &mut ctx,
        );
        let (_, reason) = machine.pending_exception().unwrap();
        assert_reason_tuple(&mut ctx, &module, reason, "try_clause", make_small(4));
    }

    #[test]
    fn a_raising_opcode_without_the_tag_atom_is_a_typed_error() {
        // No `badmatch` atom interned: the module cannot name the reason.
        let module = Fixture::new(&[]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        let mut ctx = MinimalContext::new();
        let err = exec_one(
            &module,
            &Instr::Badmatch {
                value: Operand::Integer(1),
            },
            &mut machine,
            &mut ctx,
        )
        .unwrap_err();
        assert!(matches!(err, BeamError::Raise(_)));
        assert_eq!(machine.pending_exception(), None);
    }

    // ------------------------------------------------------------------
    // The handler opcodes
    // ------------------------------------------------------------------

    #[test]
    fn try_case_moves_the_class_atom_into_x0() {
        let module = Fixture::new(&["error"]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        machine.push_catch_frame(CatchFrame::new(0, 0, 0));
        let error_atom = make_atom(module.atom_table.lookup("error").unwrap());
        machine.set_x(0, Eterm::non_value()).unwrap();
        machine.set_x(1, make_small(99)).unwrap();
        machine.set_x(3, error_atom).unwrap();

        let mut ctx = MinimalContext::new();
        let step = exec_one(
            &module,
            &Instr::TryCase {
                frame: Operand::X(1),
            },
            &mut machine,
            &mut ctx,
        )
        .unwrap();
        assert_eq!(step, Some(Step::Next));
        assert_eq!(machine.x(0), Some(error_atom));
        assert_eq!(machine.x(1), Some(NIL), "the frame slot is blanked");
        assert_eq!(machine.catch_frame_depth(), 0);
    }

    #[test]
    fn try_case_requires_the_non_value_marker() {
        let module = Fixture::new(&["error"]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        machine.push_catch_frame(CatchFrame::new(0, 0, 0));
        // x(0) is a plain NIL (no exception landed).
        let mut ctx = MinimalContext::new();
        let err = exec_one(
            &module,
            &Instr::TryCase {
                frame: Operand::X(1),
            },
            &mut machine,
            &mut ctx,
        )
        .unwrap_err();
        assert!(matches!(err, BeamError::Raise(_)));
        // The frame was consumed before the check failed.
        assert_eq!(machine.catch_frame_depth(), 0);
    }

    #[test]
    fn try_end_pops_the_frame_and_blanks_the_slot() {
        let module = Fixture::new(&[]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        machine.push_catch_frame(CatchFrame::new(5, 5, 0));
        machine.set_x(1, make_small(3)).unwrap();
        let mut ctx = MinimalContext::new();
        let step = exec_one(
            &module,
            &Instr::TryEnd {
                frame: Operand::X(1),
            },
            &mut machine,
            &mut ctx,
        )
        .unwrap();
        assert_eq!(step, Some(Step::Next));
        assert_eq!(machine.x(1), Some(NIL));
        assert_eq!(machine.catch_frame_depth(), 0);
    }

    // ------------------------------------------------------------------
    // catch_end — both branches, plus the preconditions
    // ------------------------------------------------------------------

    #[test]
    fn catch_end_passes_a_normal_result_through() {
        let module = Fixture::new(&[]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        machine.push_catch_frame(CatchFrame::new(0, 0, 0));
        machine.set_x(0, make_small(123)).unwrap();
        let mut ctx = MinimalContext::new();
        let step = exec_one(
            &module,
            &Instr::CatchEnd {
                frame: Operand::X(5),
            },
            &mut machine,
            &mut ctx,
        )
        .unwrap();
        assert_eq!(step, Some(Step::Next));
        assert_eq!(machine.x(0), Some(make_small(123)));
        assert_eq!(machine.catch_frame_depth(), 0);
    }

    #[test]
    fn catch_end_throw_class_yields_the_thrown_value() {
        let module = Fixture::new(&["throw"]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        machine.push_catch_frame(CatchFrame::new(0, 0, 0));
        let throw_atom = make_atom(module.atom_table.lookup("throw").unwrap());
        machine.set_class_atom(ExceptionClass::Throw, throw_atom);
        // Land a thrown exception (this sets x0=non_value, x1=reason, x3=class).
        let reason = make_small(77);
        machine
            .throw(ExceptionClass::Throw, reason)
            .expect("a live frame catches");
        let mut ctx = MinimalContext::new();
        let step = exec_one(
            &module,
            &Instr::CatchEnd {
                frame: Operand::X(5),
            },
            &mut machine,
            &mut ctx,
        )
        .unwrap();
        assert_eq!(step, Some(Step::Next));
        assert_eq!(machine.x(0), Some(reason));
    }

    #[test]
    fn catch_end_error_class_builds_an_exit_tuple() {
        let module = Fixture::new(&["EXIT", "error"]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        machine.push_catch_frame(CatchFrame::new(0, 0, 0));
        machine.set_class_atom(
            ExceptionClass::Error,
            make_atom(module.atom_table.lookup("error").unwrap()),
        );
        let reason = make_small(88);
        machine
            .throw(ExceptionClass::Error, reason)
            .expect("a live frame catches");
        let mut ctx = MinimalContext::new();
        let step = exec_one(
            &module,
            &Instr::CatchEnd {
                frame: Operand::X(5),
            },
            &mut machine,
            &mut ctx,
        )
        .unwrap();
        assert_eq!(step, Some(Step::Next));
        let exit_atom = make_atom(module.atom_table.lookup("EXIT").unwrap());
        let expected = terms::make_tuple(&mut ctx, &[exit_atom, reason]).unwrap();
        let actual = machine.x(0).unwrap();
        assert!(cmp(actual, expected, &module.atom_table).is_eq());
    }

    #[test]
    fn catch_end_without_the_exit_atom_is_a_typed_error() {
        // Class error with no 'EXIT' atom: the tuple is not representable.
        let module = Fixture::new(&["error"]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        machine.push_catch_frame(CatchFrame::new(0, 0, 0));
        machine.set_class_atom(
            ExceptionClass::Error,
            make_atom(module.atom_table.lookup("error").unwrap()),
        );
        machine
            .throw(ExceptionClass::Error, make_small(1))
            .expect("a live frame catches");
        let mut ctx = MinimalContext::new();
        let err = exec_one(
            &module,
            &Instr::CatchEnd {
                frame: Operand::X(5),
            },
            &mut machine,
            &mut ctx,
        )
        .unwrap_err();
        assert!(matches!(err, BeamError::Raise(_)));
    }

    #[test]
    fn handler_opcode_without_a_catch_frame_is_a_typed_error() {
        let module = Fixture::new(&[]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        let mut ctx = MinimalContext::new();
        for instr in [
            Instr::TryCase {
                frame: Operand::X(1),
            },
            Instr::TryEnd {
                frame: Operand::X(1),
            },
            Instr::CatchEnd {
                frame: Operand::X(1),
            },
        ] {
            let err = exec_one(&module, &instr, &mut machine, &mut ctx).unwrap_err();
            assert!(matches!(err, BeamError::Raise(_)));
        }
    }

    // ------------------------------------------------------------------
    // raise (the re-raise opcode)
    // ------------------------------------------------------------------

    #[test]
    fn raise_re_raises_with_the_pending_class() {
        let module = Fixture::new(&["throw"]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        machine.push_catch_frame(CatchFrame::new(0, 0, 0));
        machine.set_class_atom(
            ExceptionClass::Throw,
            make_atom(module.atom_table.lookup("throw").unwrap()),
        );
        // Establish a pending `throw` class by landing one, then re-raise.
        machine
            .throw(ExceptionClass::Throw, make_small(1))
            .expect("a live frame catches");
        machine.set_x(1, make_small(42)).unwrap(); // raise value
        machine.set_x(2, NIL).unwrap(); // raise trace

        // Consume the frame the earlier landing left in place, then re-arm it so
        // the re-raise lands again.
        machine.pop_catch_frame();
        machine.push_catch_frame(CatchFrame::new(0, 0, 0));

        let mut ctx = MinimalContext::new();
        let step = exec_one(
            &module,
            &Instr::Raise {
                trace: Operand::X(2),
                value: Operand::X(1),
            },
            &mut machine,
            &mut ctx,
        )
        .unwrap();
        assert_eq!(step, Some(Step::Goto { func: 0, pc: 0 }));
        assert_eq!(
            machine.pending_exception(),
            Some((ExceptionClass::Throw, make_small(42)))
        );
    }

    #[test]
    fn raise_without_a_pending_class_is_a_typed_error() {
        let module = Fixture::new(&[]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        let mut ctx = MinimalContext::new();
        let err = exec_one(
            &module,
            &Instr::Raise {
                trace: Operand::X(2),
                value: Operand::X(1),
            },
            &mut machine,
            &mut ctx,
        )
        .unwrap_err();
        assert!(matches!(err, BeamError::Raise(_)));
    }

    // ------------------------------------------------------------------
    // documented unsupported opcodes + the not-mine fallthrough
    // ------------------------------------------------------------------

    #[test]
    fn build_stacktrace_and_raw_raise_are_unsupported() {
        let module = Fixture::new(&[]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        let mut ctx = MinimalContext::new();
        assert!(exec_one(&module, &Instr::BuildStacktrace, &mut machine, &mut ctx).is_err());
        assert!(exec_one(&module, &Instr::RawRaise, &mut machine, &mut ctx).is_err());
    }

    #[test]
    fn a_foreign_instruction_returns_none() {
        let module = Fixture::new(&[]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        let mut ctx = MinimalContext::new();
        let step = exec_one(
            &module,
            &Instr::Move {
                src: Operand::Integer(1),
                dst: Operand::X(0),
            },
            &mut machine,
            &mut ctx,
        )
        .unwrap();
        assert_eq!(step, None);
    }

    // ------------------------------------------------------------------
    // End-to-end synthetic sequence: try → case_end → try_case → return
    // ------------------------------------------------------------------

    #[test]
    fn full_try_raise_handler_sequence_runs() {
        // f() -> try (case 1 of 2 -> ok end) catch error:{case_clause,_} -> caught end
        // The handler moves x(1) (the expanded reason) into x(0) and returns it;
        // x(0) == x(3) (the class atom) is asserted through `try_case`.
        let mut fixture = Fixture::new(&["case_clause", "error"]);
        let code = vec![
            Instr::Label(Label(1)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(1),
                live: 0,
            },
            Instr::Try {
                frame: Operand::Y(0),
                fail: Label(3),
            },
            Instr::CaseEnd {
                value: Operand::Integer(1),
            },
            Instr::Label(Label(3)),
            Instr::TryCase {
                frame: Operand::Y(0),
            },
            Instr::Move {
                src: Operand::X(1),
                dst: Operand::X(0),
            },
            Instr::Deallocate(1),
            Instr::Return,
        ];
        let entry = fixture.add_function("f", 0, code);
        let module = fixture.finish();

        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let result = machine.run_label(&module, entry, 0, &[], &mut ctx).unwrap();
        let expected = terms::make_tuple(
            &mut ctx,
            &[
                make_atom(module.atom_table.lookup("case_clause").unwrap()),
                make_small(1),
            ],
        )
        .unwrap();
        assert!(cmp(result, expected, &module.atom_table).is_eq());
        assert_eq!(machine.catch_frame_depth(), 0);
    }

    #[test]
    fn nested_try_handler_runs_and_does_not_disturb_the_outer_frame() {
        // outer() -> try inner() catch error:{case_clause,_} -> caught end
        //   inner() -> try (case 2 of 1 -> ok end) catch throw:_ -> nope end
        // `inner/0` raises `{case_clause,2}`; its handler does not match `throw`,
        // so `raise` re-raises with class `error`, landing in `outer`'s handler
        // after unwinding `inner`'s frame.
        let mut fixture = Fixture::new(&["case_clause", "error", "throw"]);
        let inner = vec![
            Instr::Label(Label(10)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(1),
                live: 0,
            },
            Instr::Try {
                frame: Operand::Y(0),
                fail: Label(12),
            },
            Instr::CaseEnd {
                value: Operand::Integer(2),
            },
            Instr::Label(Label(12)),
            Instr::TryCase {
                frame: Operand::Y(0),
            },
            // Re-raise: the not-matching tail (value = x(1), trace = x(2)).
            Instr::Raise {
                trace: Operand::X(2),
                value: Operand::X(1),
            },
        ];
        let outer = vec![
            Instr::Label(Label(1)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(1),
                live: 0,
            },
            Instr::Try {
                frame: Operand::Y(0),
                fail: Label(3),
            },
            Instr::Call {
                arity: 0,
                target: Label(10),
            },
            Instr::TryEnd {
                frame: Operand::Y(0),
            },
            Instr::Deallocate(1),
            Instr::Return,
            Instr::Label(Label(3)),
            Instr::TryCase {
                frame: Operand::Y(0),
            },
            Instr::Move {
                src: Operand::X(1),
                dst: Operand::X(0),
            },
            Instr::Deallocate(1),
            Instr::Return,
        ];
        let entry = fixture.add_function("outer", 0, outer);
        fixture.add_function("inner", 0, inner);
        let module = fixture.finish();

        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let result = machine
            .run_label(&module, entry, 0, &[], &mut ctx)
            .unwrap();
        let expected = terms::make_tuple(
            &mut ctx,
            &[
                make_atom(module.atom_table.lookup("case_clause").unwrap()),
                make_small(2),
            ],
        )
        .unwrap();
        assert!(cmp(result, expected, &module.atom_table).is_eq());
        assert_eq!(machine.frame_depth(), 1, "the run is back at the bottom frame");
    }
}
