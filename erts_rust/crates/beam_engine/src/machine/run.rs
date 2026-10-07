//! The run entries: starting, resuming and blocking a machine.
//!
//! [`Machine::run_export`]/[`Machine::run_label`] are the standard (non-blocking)
//! entries: they reset the machine, seed the registers, push the bottom frame and
//! run the dispatch loop ([`crate::interpreter`]) to the function's `return`.
//! Their `*_outcome` counterparts report a `receive` that finds no message as
//! [`RunOutcome::Wait`] instead of folding it into the typed
//! [`crate::error::blocked`] error, and [`Machine::resume_outcome`] re-enters a
//! suspended machine without resetting it. [`Machine::run_with_tier`] selects the
//! execution tier ([`crate::jit::Tier`]).

use beam_common::Result;
use beam_loader::{Atom, Label, Module};
use beam_term::Eterm;

use crate::context::ExecutionContext;
use crate::error;
use crate::interpreter;
use crate::jit::Tier;

use super::Machine;

/// The outcome of one engine run: either it returned a value, or the process blocked
/// in a `receive` with no message available (and must be resumed at `func`/`pc`).
///
/// Produced by the blocking-aware entries [`Machine::run_export_outcome`]/
/// [`Machine::run_label_outcome`]/[`Machine::resume_outcome`]. The plain
/// [`Machine::run_export`]/[`Machine::run_label`] (and the JIT backends'
/// `run_export`) are **non-blocking**: they fold a [`RunOutcome::Wait`] into the
/// typed `Err(`[`crate::error::blocked`]`)` rather than inventing a result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    /// The run finished; this is `x(0)`.
    Returned(Eterm),
    /// The process blocked on an empty `receive`; resume at code offset `pc` of the
    /// function at index `func` (into [`Module::functions`]) once a message arrives.
    Wait {
        /// The function index (into [`Module::functions`]) to resume in.
        func: u32,
        /// The code offset within that function to resume at.
        pc: u32,
    },
}

/// Fold a run's [`RunOutcome`] into the `Result<Eterm>` the non-blocking entries
/// return.
///
/// A [`RunOutcome::Returned`] is the value; a [`RunOutcome::Wait`] becomes the
/// typed [`crate::error::blocked`] error, because a non-blocking entry cannot
/// suspend the process and must never substitute a wrong result for the block.
fn fold_outcome(outcome: RunOutcome) -> Result<Eterm> {
    match outcome {
        RunOutcome::Returned(value) => Ok(value),
        RunOutcome::Wait { .. } => Err(error::blocked()),
    }
}

impl Machine {
    /// Run the exported `function/arity` to its `return`; yields `x(0)`.
    ///
    /// Resolves the export entry label and delegates to [`Machine::run_label`].
    ///
    /// This is the **non-blocking** entry: a `receive` that finds no message
    /// available suspends the process, and because this entry cannot suspend it
    /// folds that outcome into the typed `Err(`[`crate::error::blocked`]`)` (never
    /// a panic and never an invented result). Use
    /// [`Machine::run_export_outcome`] + [`Machine::resume_outcome`] to drive a
    /// blocking receive.
    pub fn run_export(
        &mut self,
        module: &Module,
        function: Atom,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        fold_outcome(self.run_export_outcome(module, function, arity, args, ctx)?)
    }

    /// Run the exported `function/arity` and report whether it returned or blocked.
    ///
    /// The export is resolved exactly as [`Machine::run_export`] does (a missing
    /// export stays the typed `unsupported("run_export: export not found")`); the
    /// difference is only the *outcome* type, so a `receive` that finds no message
    /// is reported as [`RunOutcome::Wait`] instead of an error.
    pub fn run_export_outcome(
        &mut self,
        module: &Module,
        function: Atom,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<RunOutcome> {
        match module.export(function, arity) {
            Some(label) => self.run_label_outcome(module, label, arity, args, ctx),
            None => Err(error::unsupported("run_export: export not found")),
        }
    }

    /// Resume the **current** machine state at a [`RunOutcome::Wait`] address.
    ///
    /// The machine must be the one a previous outcome reported its `(func, pc)` for
    /// (typically from the same `ExecutionContext`'s mailbox): this entry
    /// deliberately does **not** [`reset`](Machine::reset) — the registers, the
    /// Erlang stack and every call frame of the suspended `receive` must survive so
    /// the process continues exactly where it blocked. It does (re)bind the
    /// module's literal area for the run, which is what
    /// [`Machine::run_label_outcome`]'s reset used to do; that is precisely
    /// [`Machine::run_loop`]'s body (literal binding + label map + dispatch loop,
    /// no reset), so this delegates to it.
    ///
    /// An out-of-range `(func, pc)` is the loop's own typed error, never a panic.
    pub fn resume_outcome(
        &mut self,
        module: &Module,
        func: u32,
        pc: u32,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<RunOutcome> {
        self.run_loop(module, func, pc, ctx)
    }

    /// Run the exported `function/arity` on a selected execution tier; yields `x(0)`.
    ///
    /// The tier-selecting entry point of the engine ([`crate::jit::Tier`]): the
    /// caller states which tier it wants and gets that tier's result or a typed
    /// error — the engine never silently substitutes a different tier.
    ///
    /// * [`Tier::Interpreter`] — delegates to [`Machine::run_export`], which
    ///   remains the **default interpreter entry** (unchanged; this method is a
    ///   dispatcher, not a replacement).
    /// * [`Tier::BaselineJit`] — the native baseline backend
    ///   ([`crate::jit::baseline::BaselineJit`]): it compiles the module's basic
    ///   blocks to x86-64 and runs the export through a driver loop that falls back
    ///   to one interpreter step per un-lowered instruction.
    /// * [`Tier::Cranelift`] — the optional Cranelift hot tier
    ///   (`crate::jit::cranelift::CraneliftJit`), compiled in only with the
    ///   `jit-cranelift` Cargo feature: it lowers each IR function into a native
    ///   Cranelift function that loops over the function's control flow and delegates
    ///   per-instruction semantics to the same interpreter-backed helper. With the
    ///   feature off this is a typed [`unsupported`](crate::error::unsupported)
    ///   error, never a panic and never a fallback to the interpreter.
    pub fn run_with_tier(
        &mut self,
        module: &Module,
        function: Atom,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
        tier: Tier,
    ) -> Result<Eterm> {
        match tier {
            Tier::Interpreter => self.run_export(module, function, arity, args, ctx),
            Tier::BaselineJit => {
                use crate::jit::JitBackend;
                let mut backend = crate::jit::baseline::BaselineJit::new();
                backend.run_export(self, module, function, arity, args, ctx)
            }
            Tier::Cranelift => crate::jit::run_cranelift(self, module, function, arity, args, ctx),
        }
    }

    /// Run a raw entry label (used by [`Machine::run_export`]).
    ///
    /// Resets the machine, seeds `x[0..arity)` from `args` (the remaining
    /// registers stay `NIL`), pushes the **bottom** frame (whose continuation is
    /// [`Continuation::Halt`](super::Continuation)), resolves `label` to its
    /// `(function, code)` pair and enters the dispatch loop.
    ///
    /// Like [`Machine::run_export`] this is a **non-blocking** entry: a
    /// [`RunOutcome::Wait`] becomes the typed `Err(`[`crate::error::blocked`]`)`.
    /// Use [`Machine::run_label_outcome`] when the caller wants to see (and later
    /// resume) the suspension.
    pub fn run_label(
        &mut self,
        module: &Module,
        label: Label,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        fold_outcome(self.run_label_outcome(module, label, arity, args, ctx)?)
    }

    /// Run a raw entry label and report whether it returned or blocked.
    ///
    /// The exact body of [`Machine::run_label`] with a [`RunOutcome`] result
    /// instead of the folded `x(0)`: the machine is reset, `x[0..arity)` is seeded
    /// from `args`, the bottom frame is pushed, and the dispatch loop runs from the
    /// label's `(func, pc)`.
    pub fn run_label_outcome(
        &mut self,
        module: &Module,
        label: Label,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<RunOutcome> {
        self.reset();
        let n = arity.min(args.len() as u32);
        for (i, arg) in args.iter().take(n as usize).enumerate() {
            self.set_x(i as u32, *arg)?;
        }
        self.push_frame(0)?;
        let (func, pc) = interpreter::resolve_label(module, label)?;
        self.run_loop(module, func, pc, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::{Machine, RunOutcome};
    use beam_common::{BeamError, Result};
    use beam_loader::{Atom, Export, Function, Instr, Label, Module, Operand};
    use beam_term::{make_small, Eterm, NIL};
    use std::collections::VecDeque;

    use crate::context::{Mailbox, MinimalContext};
    use crate::error;

    #[test]
    fn run_export_of_missing_export_is_a_typed_error() {
        let module = Module::default();
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        assert!(machine
            .run_export(&module, Atom::MAX, 0, &[], &mut ctx)
            .is_err());
    }

    #[test]
    fn run_label_seeds_registers_then_reports_unknown_label() {
        let module = Module::default();
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let args = [Eterm::from_raw(1), Eterm::from_raw(2)];
        // An empty module has no label 1: a typed error, but the register seeding
        // already happened and a bottom frame was pushed for the duration.
        let err = machine.run_label(&module, Label(1), 2, &args, &mut ctx);
        assert!(err.is_err());
        assert_eq!(machine.x(0), Some(Eterm::from_raw(1)));
        assert_eq!(machine.x(1), Some(Eterm::from_raw(2)));
    }

    // ---- the blocking run outcome -----------------------------------------

    /// A one-function module: entry label `Label(2)` (immediately after
    /// `Label(1)`), body `code`, exported as `name/0`.
    fn module_with_code(name: &str, code: Vec<Instr>) -> (Module, Atom) {
        let mut module = Module::default();
        let function_name = module.atom_table.intern(name);
        let labels = code
            .iter()
            .enumerate()
            .filter_map(|(i, ins)| match ins {
                Instr::Label(l) => Some((*l, i)),
                _ => None,
            })
            .collect();
        module.functions = vec![Function {
            func_label: Label(1),
            entry_label: Label(2),
            module: Atom::MAX,
            name: function_name,
            arity: 0,
            code,
            blocks: Vec::new(),
            labels,
        }];
        module.exports = vec![Export {
            function: function_name,
            arity: 0,
            label: Label(2),
        }];
        (module, function_name)
    }

    /// A mailbox that hands out its queue one message per `receive_next`, and
    /// ignores `to` in `send_message` (enough to script a receive).
    struct ScriptedMailbox {
        messages: VecDeque<Eterm>,
    }

    impl Mailbox for ScriptedMailbox {
        fn send_message(&mut self, _to: Eterm, message: Eterm) -> Result<()> {
            self.messages.push_back(message);
            Ok(())
        }

        fn receive_next(&mut self) -> Result<Option<Eterm>> {
            Ok(self.messages.pop_front())
        }

        fn receive_skip(&mut self) -> Result<()> {
            Ok(())
        }

        fn receive_remove(&mut self) -> Result<()> {
            Ok(())
        }

        fn receive_reset(&mut self) -> Result<()> {
            Ok(())
        }

        fn receive_marker_reserve(&mut self) -> Result<Eterm> {
            Ok(NIL)
        }

        fn receive_marker_bind(&mut self, _marker: Eterm, _reference: Eterm) -> Result<()> {
            Ok(())
        }

        fn receive_marker_use(&mut self, _reference: Eterm) -> Result<()> {
            Ok(())
        }

        fn receive_marker_clear(&mut self, _reference: Eterm) -> Result<()> {
            Ok(())
        }
    }

    /// The receive sequence the compiler emits, hand-written:
    ///
    /// ```text
    ///  1: label 2                 ; entry
    ///  2: loop_rec {f,4} x(0)     ; take the next candidate, else jump to the wait
    ///  3: remove_message          ; the (unconditional) match succeeded
    ///  4: jump {f,5}              ; leave the receive
    ///  5: label 4
    ///  6: wait {f,3}              ; mailbox empty — suspend, resuming at label 3
    ///  7: label 3
    ///  8: jump {f,2}              ; re-examine the mailbox
    ///  9: label 5
    /// 10: return
    /// ```
    fn receive_module() -> (Module, Atom) {
        module_with_code(
            "recv",
            vec![
                Instr::Label(Label(1)),
                Instr::Label(Label(2)),
                Instr::LoopRec {
                    fail: Label(4),
                    dst: Operand::X(0),
                },
                Instr::RemoveMessage,
                Instr::Jump { target: Label(5) },
                Instr::Label(Label(4)),
                Instr::Wait { target: Label(3) },
                Instr::Label(Label(3)),
                Instr::Jump { target: Label(2) },
                Instr::Label(Label(5)),
                Instr::Return,
            ],
        )
    }

    #[test]
    fn run_export_outcome_reports_a_returned_value() {
        let (module, name) = module_with_code(
            "f",
            vec![
                Instr::Label(Label(1)),
                Instr::Label(Label(2)),
                Instr::Move {
                    src: Operand::Integer(5),
                    dst: Operand::X(0),
                },
                Instr::Return,
            ],
        );
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        assert_eq!(
            machine
                .run_export_outcome(&module, name, 0, &[], &mut ctx)
                .unwrap(),
            RunOutcome::Returned(make_small(5))
        );
    }

    #[test]
    fn a_receive_on_an_empty_mailbox_blocks_then_resumes_after_a_message_arrives() {
        let (module, name) = receive_module();
        let mut machine = Machine::new();
        // An installed but empty mailbox: exhaustion is an `Ok(None)` answer from
        // the host, not an error.
        let mut ctx = MinimalContext::with_mailbox(Box::new(ScriptedMailbox {
            messages: VecDeque::new(),
        }));

        // Empty mailbox: `loop_rec` jumps to the `wait`, which suspends. The
        // reported address is the `wait`'s target (`label 3`, code offset 7) — the
        // `loop_rec` that re-examines the mailbox once a message arrives.
        assert_eq!(
            machine
                .run_export_outcome(&module, name, 0, &[], &mut ctx)
                .unwrap(),
            RunOutcome::Wait { func: 0, pc: 7 }
        );

        // The suspension left the call frames in place, so the process can be
        // resumed in the *same* machine; a non-blocking entry would instead report
        // the typed block.
        assert_eq!(machine.frame_depth(), 1, "the bottom frame survived");

        // A message arrives, then the host resumes at the reported address.
        let mut ctx = MinimalContext::with_mailbox(Box::new(ScriptedMailbox {
            messages: VecDeque::from([make_small(42)]),
        }));
        assert_eq!(
            machine.resume_outcome(&module, 0, 7, &mut ctx).unwrap(),
            RunOutcome::Returned(make_small(42))
        );
    }

    #[test]
    fn run_export_folds_a_blocked_receive_into_the_typed_blocked_error() {
        let (module, name) = receive_module();
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::with_mailbox(Box::new(ScriptedMailbox {
            messages: VecDeque::new(),
        }));

        let err = machine
            .run_export(&module, name, 0, &[], &mut ctx)
            .unwrap_err();

        match err {
            BeamError::Raise(raise) => assert_eq!(
                raise.reason.as_atom(),
                Some("unsupported_opcode: run: process blocked (no message available)")
            ),
            other => panic!("expected the typed blocked error, got {other:?}"),
        }
        // `error::blocked()` is the single definition of that reason string.
        match error::blocked() {
            BeamError::Raise(raise) => assert_eq!(
                raise.reason.as_atom(),
                Some("unsupported_opcode: run: process blocked (no message available)")
            ),
            other => panic!("expected the typed blocked error, got {other:?}"),
        }
    }

    #[test]
    fn resume_outcome_does_not_reset_the_suspended_machine() {
        // A resumed machine keeps the x-registers written before the suspension:
        // `resume_outcome` must not reset (a reset would also drop the bottom frame
        // the trailing `return` needs).
        let (module, name) = receive_module();
        let mut machine = Machine::new();
        let mut empty = MinimalContext::with_mailbox(Box::new(ScriptedMailbox {
            messages: VecDeque::new(),
        }));
        assert_eq!(
            machine
                .run_export_outcome(&module, name, 0, &[], &mut empty)
                .unwrap(),
            RunOutcome::Wait { func: 0, pc: 7 }
        );
        // Sentinel in a register the resumed code never writes.
        machine.set_x(9, make_small(0x5A)).unwrap();

        let mut ctx = MinimalContext::with_mailbox(Box::new(ScriptedMailbox {
            messages: VecDeque::from([make_small(7)]),
        }));
        assert_eq!(
            machine.resume_outcome(&module, 0, 7, &mut ctx).unwrap(),
            RunOutcome::Returned(make_small(7))
        );
        assert_eq!(machine.x(9), Some(make_small(0x5A)));
    }
}
