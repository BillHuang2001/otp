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

    /// Resume a suspended machine at a [`RunOutcome::Wait`] address on a selected
    /// execution tier, reporting whether the resumed slice returned or blocked again.
    ///
    /// This is [`Machine::resume_outcome`] with the tier selection of
    /// [`Machine::run_export_outcome_with_tier`]: a slice that suspended on a native
    /// tier (or on the interpreter) resumes on the **same** tier the caller names, so
    /// the tier seam survives a suspension. As with the first-entry tier entries, the
    /// engine never silently substitutes a different tier — the requested tier's
    /// result or a typed error is what the caller gets.
    ///
    /// The machine must be the one the preceding [`RunOutcome::Wait`] reported its
    /// `(func, pc)` for; none of these entries [`reset`](Machine::reset) it (the
    /// registers, the Erlang stack and every call frame of the suspended `receive`
    /// must survive). `module` is the run's **entry** module (context `0`): a
    /// suspended `(func, pc)` may live in a context the process entered through
    /// [`ExecutionContext::resolve_external_call`], which the host's module context
    /// registry recovers from [`Machine::current_context`].
    ///
    /// Each tier is **interpreter-identical** — the same value, the same
    /// [`RunOutcome`] and the same typed errors the interpreter would produce:
    ///
    /// * [`Tier::Interpreter`] — delegates to [`Machine::resume_outcome`], which
    ///   remains the **default resume entry** (unchanged; this method is a
    ///   dispatcher, not a replacement).
    /// * [`Tier::BaselineJit`] — the native baseline backend over the per-thread
    ///   cross-run cache ([`crate::jit::cache`]), resuming through its no-reset
    ///   driver rather than recompiling or resetting.
    /// * [`Tier::Cranelift`] — the optional Cranelift hot tier. A resume is handed
    ///   **no** compiled artifact and the tier has no path that would compile and
    ///   bind one at a parked address on demand, so resuming a mid-function `wait`
    ///   address is unserviceable natively: this is a typed
    ///   [`unsupported`](crate::error::unsupported) error (or, with the
    ///   `jit-cranelift` feature off, the same typed error the first-entry Cranelift
    ///   path reports) — never a panic and never a silent fallback to another tier.
    pub fn resume_outcome_with_tier(
        &mut self,
        module: &Module,
        func: u32,
        pc: u32,
        ctx: &mut dyn ExecutionContext,
        tier: Tier,
    ) -> Result<RunOutcome> {
        match tier {
            Tier::Interpreter => self.resume_outcome(module, func, pc, ctx),
            Tier::BaselineJit => {
                crate::jit::cache::run_baseline_cached_resume_outcome(self, module, func, pc, ctx)
            }
            Tier::Cranelift => {
                crate::jit::run_cranelift_resume_outcome(self, module, func, pc, ctx)
            }
        }
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
    ///   to one interpreter step per un-lowered instruction. The compilation is
    ///   **cached** across calls ([`crate::jit::cache`]): a module compiled once is
    ///   reused by every later run of this tier on the same thread, with no recompile.
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
                // Reuse the per-thread compiled-block cache: a module compiled once is
                // not recompiled on a later run of the same tier (see `jit::cache`).
                crate::jit::cache::run_baseline_cached(self, module, function, arity, args, ctx)
            }
            Tier::Cranelift => crate::jit::run_cranelift(self, module, function, arity, args, ctx),
        }
    }

    /// Run the exported `function/arity` on a selected execution tier and report
    /// whether it **returned or blocked** in a `receive`.
    ///
    /// The tier-selecting, blocking-aware entry point: [`Machine::run_with_tier`]
    /// with a [`RunOutcome`] result, so a `receive` that finds no message available
    /// arrives as [`RunOutcome::Wait`] with the address to resume at instead of the
    /// folded typed [`crate::error::blocked`] error. That is what lets a host run a
    /// real module on a native tier and still drive its `receive` through
    /// [`Machine::resume_outcome`].
    ///
    /// Every tier reports the **same** suspension: the address is the one the
    /// interpreter would report, whether the block came from the interpreter's own
    /// dispatch loop or from a compiled block that deopted to the shared
    /// interpreter-backed helper.
    ///
    /// * [`Tier::Interpreter`] — delegates to [`Machine::run_export_outcome`]
    ///   (unchanged; this method is a dispatcher, not a replacement).
    /// * [`Tier::BaselineJit`] — the native baseline backend over the per-thread
    ///   cross-run cache ([`crate::jit::cache`]).
    /// * [`Tier::Cranelift`] — the optional Cranelift hot tier; with the
    ///   `jit-cranelift` feature off this is a typed
    ///   [`unsupported`](crate::error::unsupported) error, never a panic and never a
    ///   silent fallback to another tier.
    pub fn run_export_outcome_with_tier(
        &mut self,
        module: &Module,
        function: Atom,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
        tier: Tier,
    ) -> Result<RunOutcome> {
        match tier {
            Tier::Interpreter => self.run_export_outcome(module, function, arity, args, ctx),
            Tier::BaselineJit => crate::jit::cache::run_baseline_cached_outcome(
                self, module, function, arity, args, ctx,
            ),
            Tier::Cranelift => {
                crate::jit::run_cranelift_outcome(self, module, function, arity, args, ctx)
            }
        }
    }

    /// Run a raw entry label on a selected execution tier and report whether it
    /// **returned or blocked** in a `receive`.
    ///
    /// The label-addressed counterpart of [`Machine::run_export_outcome_with_tier`]:
    /// the same tier selection and the same suspension reporting, entering at a raw
    /// [`Label`] (which may live in any function of the module) rather than at an
    /// export. A label the module does not define is a typed error, never a panic.
    pub fn run_label_outcome_with_tier(
        &mut self,
        module: &Module,
        label: Label,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
        tier: Tier,
    ) -> Result<RunOutcome> {
        match tier {
            Tier::Interpreter => self.run_label_outcome(module, label, arity, args, ctx),
            Tier::BaselineJit => crate::jit::cache::run_baseline_cached_label_outcome(
                self, module, label, arity, args, ctx,
            ),
            Tier::Cranelift => {
                crate::jit::run_cranelift_label_outcome(self, module, label, arity, args, ctx)
            }
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
    ///
    /// The module-wide label index is built **exactly once per run**: the entry
    /// label is resolved against one [`interpreter::build_label_map`] and the very
    /// same map is handed to the dispatch loop, instead of resolving the label with
    /// one build and then rebuilding the identical map inside [`Machine::run_loop`].
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
        let labels = interpreter::build_label_map(module);
        let (func, pc) = interpreter::resolve(&labels, label)?;
        self.enter_run(module, func);
        self.run_loop_with_map(module, &labels, func, pc, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::{Machine, RunOutcome};
    use beam_common::{BeamError, Result};
    use beam_loader::{Atom, Block, Export, Function, Instr, Label, Module, Operand};
    use beam_term::{make_small, Eterm, NIL};
    use std::collections::VecDeque;

    use crate::context::{Mailbox, MinimalContext};
    use crate::error;
    use crate::jit::Tier;

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

    /// Change 2: a run must build the module-wide label index **exactly once**.
    ///
    /// `run_label_outcome` used to resolve the entry label with one
    /// `build_label_map` and then rebuild the identical map inside `run_loop` — two
    /// `O(labels)` passes per run. The `cfg(test)` counter ([`crate::interpreter`])
    /// makes the count observable here.
    #[test]
    fn run_export_builds_the_label_map_exactly_once() {
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
        crate::interpreter::reset_label_map_builds();
        assert_eq!(
            machine.run_export(&module, name, 0, &[], &mut ctx).unwrap(),
            make_small(5)
        );
        assert_eq!(
            crate::interpreter::label_map_builds(),
            1,
            "run_export must build the module label index exactly once per run"
        );
    }

    /// The `resume_outcome` path builds its own (single) label index: a suspension
    /// is re-entered by a fresh run, so it builds the map once when resumed.
    #[test]
    fn resume_outcome_builds_the_label_map_exactly_once() {
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

        let mut ctx = MinimalContext::with_mailbox(Box::new(ScriptedMailbox {
            messages: VecDeque::from([make_small(42)]),
        }));
        crate::interpreter::reset_label_map_builds();
        assert_eq!(
            machine.resume_outcome(&module, 0, 7, &mut ctx).unwrap(),
            RunOutcome::Returned(make_small(42))
        );
        assert_eq!(
            crate::interpreter::label_map_builds(),
            1,
            "resume_outcome must build the module label index exactly once"
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

    // ---- the outcome-aware tier entries -----------------------------------

    /// Give every function of `module` the loader's label-delimited block
    /// decomposition, which the synthetic fixture above omits.
    ///
    /// The interpreter does not need it (`step_instruction` walks the code through
    /// the label map), but the native baseline emitter lowers exactly
    /// `Function::blocks`; a fixture without blocks compiles no code at all, so it
    /// would never exercise the native tier's `Step::Wait` path.
    fn with_blocks((mut module, name): (Module, Atom)) -> (Module, Atom) {
        for function in &mut module.functions {
            let code_len = function.code.len();
            let mut starts: Vec<(usize, Label)> = function
                .labels
                .iter()
                .map(|(label, at)| (*at, *label))
                .collect();
            starts.sort_by_key(|(at, _)| *at);
            starts.dedup_by_key(|(at, _)| *at);
            let mut blocks = Vec::with_capacity(starts.len());
            for (index, (start, label)) in starts.iter().enumerate() {
                let end = starts.get(index + 1).map_or(code_len, |(at, _)| *at);
                if *start < end {
                    blocks.push(Block {
                        label: *label,
                        start: *start,
                        end,
                    });
                }
            }
            function.blocks = blocks;
        }
        (module, name)
    }

    /// A mailbox with nothing queued: every `receive` suspends.
    fn empty_mailbox() -> MinimalContext {
        MinimalContext::with_mailbox(Box::new(ScriptedMailbox {
            messages: VecDeque::new(),
        }))
    }

    #[test]
    fn the_receive_family_reports_the_same_wait_on_the_interpreter_and_baseline_tiers() {
        // The receive fixture with a real block decomposition, so the baseline tier
        // actually compiles and enters native code — the suspension then travels
        // through a compiled block's `jit_step_one` deopt (the `wait` instruction is
        // not one the emitter inlines) and back to the driver as a `Wait`.
        let (module, name) = with_blocks(receive_module());

        let mut interpreter = Machine::new();
        let expected = interpreter
            .run_export_outcome_with_tier(&module, name, 0, &[], &mut empty_mailbox(), Tier::Interpreter)
            .expect("interpreter run");
        assert_eq!(expected, RunOutcome::Wait { func: 0, pc: 7 });

        let mut baseline = Machine::new();
        let got = baseline
            .run_export_outcome_with_tier(&module, name, 0, &[], &mut empty_mailbox(), Tier::BaselineJit)
            .expect("baseline run");
        assert_eq!(
            got, expected,
            "the native tier must report the interpreter's exact suspension"
        );
    }

    #[test]
    fn a_baseline_tier_wait_resumes_to_returned_once_a_message_arrives() {
        let (module, name) = with_blocks(receive_module());
        let mut machine = Machine::new();
        assert_eq!(
            machine
                .run_export_outcome_with_tier(
                    &module,
                    name,
                    0,
                    &[],
                    &mut empty_mailbox(),
                    Tier::BaselineJit,
                )
                .expect("baseline run"),
            RunOutcome::Wait { func: 0, pc: 7 }
        );
        // The compiled block spills its cached registers on the deopt exit, so the
        // machine is left exactly as an interpreter suspension leaves it and the
        // bottom frame is still there to resume against.
        assert_eq!(machine.frame_depth(), 1, "the JIT suspension kept the frame");

        let mut ctx = MinimalContext::with_mailbox(Box::new(ScriptedMailbox {
            messages: VecDeque::from([make_small(42)]),
        }));
        assert_eq!(
            machine.resume_outcome(&module, 0, 7, &mut ctx).unwrap(),
            RunOutcome::Returned(make_small(42))
        );
    }

    /// The tier seam must survive a **resume**: a slice that suspended on the native
    /// baseline tier resumes on it (through `resume_outcome_with_tier`) and produces
    /// exactly the interpreter's result.
    #[test]
    fn a_baseline_tier_wait_resumes_natively_and_matches_the_interpreter() {
        let (module, name) = with_blocks(receive_module());

        // Suspend two fresh machines the *same* way — on an empty mailbox — one via
        // the interpreter tier and one via the baseline tier. Both must report the
        // interpreter's exact suspension address.
        let mut interpreter = Machine::new();
        assert_eq!(
            interpreter
                .run_export_outcome_with_tier(
                    &module,
                    name,
                    0,
                    &[],
                    &mut empty_mailbox(),
                    Tier::Interpreter,
                )
                .expect("interpreter run"),
            RunOutcome::Wait { func: 0, pc: 7 }
        );

        let mut baseline = Machine::new();
        assert_eq!(
            baseline
                .run_export_outcome_with_tier(
                    &module,
                    name,
                    0,
                    &[],
                    &mut empty_mailbox(),
                    Tier::BaselineJit,
                )
                .expect("baseline run"),
            RunOutcome::Wait { func: 0, pc: 7 }
        );

        // Resume each through the tier-selecting resume entry with its matching tier.
        // The message has arrived, so both must return the interpreter-identical value.
        let mut interpreter_ctx = MinimalContext::with_mailbox(Box::new(ScriptedMailbox {
            messages: VecDeque::from([make_small(42)]),
        }));
        let expected = interpreter
            .resume_outcome_with_tier(&module, 0, 7, &mut interpreter_ctx, Tier::Interpreter)
            .expect("interpreter resume");
        assert_eq!(expected, RunOutcome::Returned(make_small(42)));

        // Empty the module's cached artifact right before the baseline resume, so the
        // resume must compile (and therefore really drive native code) rather than
        // reuse what the first suspension compiled. A resume that fell back to the
        // interpreter would touch neither the cache nor the compile counter.
        crate::jit::cache::clear_baseline_cache();
        assert_eq!(crate::jit::cache::baseline_compile_count(), 0);
        assert_eq!(crate::jit::cache::baseline_cache_len(), 0);

        let mut baseline_ctx = MinimalContext::with_mailbox(Box::new(ScriptedMailbox {
            messages: VecDeque::from([make_small(42)]),
        }));
        let got = baseline
            .resume_outcome_with_tier(&module, 0, 7, &mut baseline_ctx, Tier::BaselineJit)
            .expect("baseline resume");
        assert_eq!(
            got, expected,
            "a resumed baseline slice must agree with the interpreter exactly"
        );
        assert_eq!(
            crate::jit::cache::baseline_compile_count(),
            1,
            "the baseline resume must have compiled and driven a native artifact, not the interpreter"
        );
        assert_eq!(
            crate::jit::cache::baseline_cache_len(),
            1,
            "the baseline resume must have cached its artifact"
        );
        assert!(
            crate::jit::baseline::compile_module(&module)
                .expect("compile the fixture")
                .block_count()
                > 0,
            "the fixture must actually lower to compiled blocks for the resume to run natively"
        );
        assert!(
            crate::jit::baseline::compile_module(&module)
                .expect("compile the fixture")
                .entry(0, 7)
                .is_some(),
            "the resume address must be a compiled block entry, so the resume runs native code"
        );
    }

    /// A `Tier::Cranelift` resume is the honest typed `unsupported` error: the
    /// resume path is handed **no** compiled artifact and the tier has no path that
    /// would compile and bind one at a parked address on demand, so a mid-function
    /// suspension address is unserviceable — never a silent fallback to the
    /// interpreter.
    #[cfg(feature = "jit-cranelift")]
    #[test]
    fn a_cranelift_tier_resume_is_the_honest_typed_unsupported_error() {
        let (module, name) = with_blocks(receive_module());
        // Park the machine on the empty mailbox through the interpreter; the
        // Cranelift resume is what we are exercising.
        let mut machine = Machine::new();
        assert_eq!(
            machine
                .run_export_outcome_with_tier(
                    &module,
                    name,
                    0,
                    &[],
                    &mut empty_mailbox(),
                    Tier::Interpreter,
                )
                .expect("interpreter run"),
            RunOutcome::Wait { func: 0, pc: 7 }
        );

        let mut ctx = MinimalContext::with_mailbox(Box::new(ScriptedMailbox {
            messages: VecDeque::from([make_small(42)]),
        }));
        let err = machine
            .resume_outcome_with_tier(&module, 0, 7, &mut ctx, Tier::Cranelift)
            .expect_err("the cranelift tier cannot resume at an arbitrary pc");
        match err {
            BeamError::Raise(raise) => {
                let reason = raise.reason.as_atom().unwrap_or_default();
                assert!(
                    reason.starts_with("unsupported_opcode: ") && reason.contains("cranelift"),
                    "expected the honest cranelift resume error, got {reason:?}"
                );
            }
            other => panic!("expected the typed cranelift unsupported error, got {other:?}"),
        }
    }

    #[test]
    fn the_label_entry_reports_the_same_wait_on_the_interpreter_and_baseline_tiers() {
        let (module, name) = with_blocks(receive_module());
        let label = module
            .export(name, 0)
            .expect("the fixture exports its function at arity 0");

        let mut interpreter = Machine::new();
        let expected = interpreter
            .run_label_outcome_with_tier(
                &module,
                label,
                0,
                &[],
                &mut empty_mailbox(),
                Tier::Interpreter,
            )
            .expect("interpreter run");
        assert_eq!(expected, RunOutcome::Wait { func: 0, pc: 7 });

        let mut baseline = Machine::new();
        let got = baseline
            .run_label_outcome_with_tier(
                &module,
                label,
                0,
                &[],
                &mut empty_mailbox(),
                Tier::BaselineJit,
            )
            .expect("baseline run");
        assert_eq!(got, expected);
    }

    #[test]
    fn a_non_blocking_tier_entry_still_folds_a_wait_into_the_typed_blocked_error() {
        // The plain tier entries stay non-blocking: the same fixture through
        // `run_with_tier` must report the typed `blocked` error, not a suspension and
        // certainly not a wrong result.
        let (module, name) = with_blocks(receive_module());
        let mut machine = Machine::new();
        let err = machine
            .run_with_tier(&module, name, 0, &[], &mut empty_mailbox(), Tier::BaselineJit)
            .expect_err("a non-blocking entry cannot suspend");
        match err {
            BeamError::Raise(raise) => assert_eq!(
                raise.reason.as_atom(),
                Some("unsupported_opcode: run: process blocked (no message available)")
            ),
            other => panic!("expected the typed blocked error, got {other:?}"),
        }
    }
}
