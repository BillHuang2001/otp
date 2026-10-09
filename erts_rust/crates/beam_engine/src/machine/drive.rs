//! The module-context-aware dispatch driver.
//!
//! [`Machine::drive`] is the loop that actually runs instructions: it owns the
//! `(func, pc)` position of the run and the **module context** those indices
//! belong to, and folds every [`Step`] the interpreter or an instruction family
//! returns back into that position.
//!
//! # Why the driver, not the loops, owns the context
//!
//! A `call_ext*` the host resolved through
//! [`ExecutionContext::resolve_external_call`](crate::ExecutionContext::resolve_external_call)
//! is executed **in this machine** (see [`crate::machine::context`]): the callee's
//! instructions live in another module, so the loop's `(module, labels)` pair is no
//! longer fixed for the run. The `call_ext*` arms report the transfer as
//! [`Step::Enter`], the frame model records a continuation's module context on the
//! frame itself ([`Continuation::Address`]), and this loop re-binds the active
//! module (and the machine's literal area) at those points only — an intra-module
//! [`Step::Goto`] still costs nothing.
//!
//! The loop keeps the entry module **borrowed** (`&Module`, context `0`) and every
//! entered module **owned** through its [`ModuleContext`] handle, so the module
//! identity of a suspended frame survives a
//! [`RunOutcome::Wait`](crate::RunOutcome::Wait) — which is exactly what
//! [`Machine::resume_outcome`] needs, because it is deliberately not a
//! [`reset`](Machine::reset).

use std::sync::Arc;

use beam_common::Result;
use beam_loader::Module;

use crate::context::ExecutionContext;
use crate::error;
use crate::interpreter::{build_label_map, LabelMap};
use crate::Step;

use super::{Continuation, Machine, ModuleContext, RunOutcome};

impl Machine {
    /// The dispatch loop over a pre-built label map, with the module context the
    /// machine currently carries.
    ///
    /// * `module` / `labels` describe the **entry** module — context `0`. The map may
    ///   be handed in (a run entry that already resolved its own entry label) or
    ///   built here, exactly once.
    /// * The starting context is [`Machine::current_context`]: `0` for a fresh run,
    ///   and the context a suspended process blocked in for a resume. A non-zero
    ///   context must still be registered in [`Machine::contexts`] (a resume does not
    ///   reset, so it is), and its module is used for `(func, pc)` — the passed
    ///   `module` is only context `0`'s module.
    ///
    /// Reports a `receive` that finds no message as [`RunOutcome::Wait`] (leaving the
    /// machine untouched at the reported address, and the context in
    /// [`Machine::current_context`] for the resume), and a `return`/halt as
    /// [`RunOutcome::Returned`].
    ///
    /// Also latches the A.8 tracing gate **once per run**: the per-instruction
    /// call/return arms read the plain `Machine::trace_on` `bool` instead of making a
    /// `trace_enabled()` vtable call per instruction, so an untraced run pays a
    /// single `bool` load and no event is ever built. See the `Machine::trace_on`
    /// field docs for the mid-run semantics.
    ///
    /// And charges **one reduction per interpreted instruction**: the slice's count
    /// starts at zero here and is read back through [`Machine::reductions`], while
    /// each charge is also offered to the host through
    /// [`ExecutionContext::reduce`] — the only way a **non-allocating** loop can be
    /// pre-empted, since `ensure_heap` is never reached by one. An `Err` from the
    /// hook ends the run exactly like any other engine failure (the host has
    /// exhausted its budget); it is never swallowed.
    pub(crate) fn drive(
        &mut self,
        module: &Module,
        labels: Option<&LabelMap>,
        mut func: u32,
        mut pc: u32,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<RunOutcome> {
        self.trace_on = ctx.trace_enabled();
        // One `drive` call is one scheduling slice, so the reduction counter starts
        // here (the ERTS process counter, `c_p->fcalls`, is likewise reset when a
        // process is scheduled in; the host reads the count when the slice returns).
        self.reductions = 0;
        // The entry module's label index. A run entry that already resolved its
        // entry label hands its map in; otherwise it is built here (still exactly
        // one build per run).
        let built: LabelMap;
        let entry_labels: &LabelMap = match labels {
            Some(map) => map,
            None => {
                built = build_label_map(module);
                &built
            }
        };
        // Bind the starting context. `reset` left a fresh run in context `0`; a
        // resume arrives in the context the process blocked in.
        let mut entered: Option<Arc<ModuleContext>> = if self.current_context() == 0 {
            self.entry_literals = Some(module.literals.clone());
            self.set_literals(module.literals.clone());
            None
        } else {
            let handle = self
                .module_context(self.current_context())
                .ok_or_else(|| error::unsupported("run: unknown module context"))?;
            self.set_literals(handle.module.literals.clone());
            Some(handle)
        };
        loop {
            // The module/labels the next instruction is resolved against. Re-borrowed
            // per iteration: a context switch replaces `entered` below.
            let (active_module, active_labels): (&Module, &LabelMap) = match &entered {
                Some(handle) => (&handle.module, &handle.labels),
                None => (module, entry_labels),
            };
            let step = self.step_instruction(active_module, active_labels, func, pc, ctx)?;
            // Price the instruction: the engine's own slice counter (read back by the
            // host through `Machine::reductions`) and the host hook that lets a
            // **non-allocating** loop be pre-empted. `ensure_heap` — the only other
            // charge — is unreachable from such a loop, so this is the only point a
            // host is offered one reduction per instruction.
            self.reductions = self.reductions.saturating_add(1);
            ctx.reduce(1)?;
            match step {
                Step::Next => pc += 1,
                Step::Goto { func: f, pc: p } => {
                    func = f;
                    pc = p;
                }
                Step::Enter {
                    context,
                    func: f,
                    pc: p,
                } => {
                    if context != self.current_context() {
                        self.switch_context(context)?;
                        entered = match context {
                            0 => None,
                            id => Some(
                                self.module_context(id).ok_or_else(|| {
                                    error::unsupported("run: unknown module context")
                                })?,
                            ),
                        };
                    }
                    func = f;
                    pc = p;
                }
                Step::Wait { func: f, pc: p } => {
                    return Ok(RunOutcome::Wait { func: f, pc: p })
                }
                Step::Done => return self.finish().map(RunOutcome::Returned),
            }
        }
    }

    /// Follow the current frame's continuation as a control-flow [`Step`].
    ///
    /// A [`Continuation::Halt`] finishes the run ([`Step::Done`]); an address in the
    /// **current** module context is an intra-module [`Step::Goto`], and an address in
    /// another context is a [`Step::Enter`] the driver must switch for. The two are
    /// distinguished here (one integer compare) rather than at every call site.
    pub(crate) fn continuation_step(&self) -> Result<Step> {
        match self.continuation()? {
            Continuation::Halt => Ok(Step::Done),
            Continuation::Address {
                func,
                pc,
                context,
            } if context == self.current_context() => Ok(Step::Goto { func, pc }),
            Continuation::Address {
                func,
                pc,
                context,
            } => Ok(Step::Enter {
                context,
                func,
                pc,
            }),
        }
    }

    /// The number of interpreted instructions charged in the current (or most
    /// recently finished) [`Machine::drive`] slice.
    ///
    /// One `drive` call — a run, or a resume of a suspended process — is one
    /// scheduling slice, and the counter is zeroed at its start, so this is that
    /// slice's work in the unit the runtime prices pre-emption in. It is the
    /// **engine-side twin** of [`ExecutionContext::reduce`]: the hook lets a host
    /// act *during* the slice, this accessor lets it observe *after* the slice
    /// returns (`RunOutcome::Wait` included) without keeping its own tally.
    ///
    /// Honest by construction: only the interpreter's dispatch loop charges, so
    /// under the baseline JIT tier — where a natively executed block bypasses
    /// `step_instruction` — the count is a **lower bound** (each deopt still
    /// charges). Never fabricated, never negative.
    pub fn reductions(&self) -> u64 {
        self.reductions
    }

    /// Zero the reduction counter, returning the value it held.
    ///
    /// [`Machine::drive`] already zeroes it at every slice entry, so a host only
    /// needs this to *re-base* a running tally (e.g. when it wants a window that
    /// spans several slices). The returned value is the count that was discarded, so
    /// a host that re-bases can still account for the work it saw.
    pub fn reset_reductions(&mut self) -> u64 {
        std::mem::replace(&mut self.reductions, 0)
    }
}

#[cfg(test)]
mod tests {
    //! The driver's `Step::Enter` contract: **every** way a `Step::Enter` reaches this
    //! loop must leave the machine's current module (its context and its literal
    //! binding) on the module the reported `(func, pc)` actually index. The three
    //! producers exercised here are a resolved cross-module `call_ext`, a
    //! cross-module `return` onto a caller-context continuation, and a raise that
    //! lands in a catch frame belonging to another module context. A wrong current
    //! module would resolve the next instruction against the wrong module's labels and
    //! literals — a silent correctness bug — so each test pins it explicitly.
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use beam_common::Result;
    use beam_loader::{
        Atom, Export, Function, HeapNeed, Import, ImportRef, Instr, Label, Module, Operand,
    };
    use beam_term::{make_small, Eterm, NIL};

    use crate::context::{Mailbox, MinimalContext};
    use crate::machine::{Machine, RunOutcome};

    /// Map every `Instr::Label` in `code` to its index.
    fn labels_of(code: &[Instr]) -> BTreeMap<Label, usize> {
        let mut labels = BTreeMap::new();
        for (index, instr) in code.iter().enumerate() {
            if let Instr::Label(label) = instr {
                labels.insert(*label, index);
            }
        }
        labels
    }

    /// One synthetic module with its own atom table.
    struct Fixture {
        module: Module,
    }

    impl Fixture {
        fn new(name: &str) -> Fixture {
            let mut module = Module::default();
            module.name = module.atom_table.intern(name);
            Fixture { module }
        }

        fn atom(&mut self, name: &str) -> Atom {
            self.module.atom_table.intern(name)
        }

        fn import(&mut self, module: &str, function: &str, arity: u32) -> ImportRef {
            let module = self.atom(module);
            let function = self.atom(function);
            self.module.imports.push(Import {
                module,
                function,
                arity,
            });
            ImportRef((self.module.imports.len() - 1) as u32)
        }

        fn add_function(&mut self, name: &str, arity: u32, code: Vec<Instr>) -> Label {
            let name = self.atom(name);
            let module_name = self.module.name;
            let labels = labels_of(&code);
            let entry = labels.keys().next().copied().unwrap_or(Label(0));
            self.module.functions.push(Function {
                func_label: entry,
                entry_label: entry,
                module: module_name,
                name,
                arity,
                code,
                blocks: Vec::new(),
                labels,
            });
            entry
        }

        fn add_export(&mut self, name: &str, arity: u32, label: Label) {
            let function = self.atom(name);
            self.module.exports.push(Export {
                function,
                arity,
                label,
            });
        }

        fn finish(self) -> Module {
            self.module
        }
    }

    /// A mailbox that is always empty: `loop_rec` takes its fail label immediately.
    struct EmptyMailbox;

    impl Mailbox for EmptyMailbox {
        fn send_message(&mut self, _to: Eterm, _message: Eterm) -> Result<()> {
            Ok(())
        }
        fn receive_next(&mut self) -> Result<Option<Eterm>> {
            Ok(None)
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

    /// A `MinimalContext` whose cross-module seam resolves the named callees and
    /// whose mailbox is empty.
    fn seam(callees: Vec<(String, Arc<Module>)>) -> MinimalContext {
        let mut ctx =
            MinimalContext::with_external_resolver(Box::new(move |module, function, arity| {
                for (name, callee) in &callees {
                    if name != module {
                        continue;
                    }
                    let Some(function) = callee.atom_table.lookup(function) else {
                        continue;
                    };
                    if let Some(label) = callee.export(function, arity) {
                        return Ok(Some((Arc::clone(callee), label)));
                    }
                }
                Ok(None)
            }));
        ctx.set_mailbox(Box::new(EmptyMailbox));
        ctx
    }

    /// The caller `m`: `run/0` allocates, `call_ext`s `c:<target>/0`, then receives
    /// (suspending on the empty mailbox).
    fn caller_then_receive(target: &str) -> (Module, Atom) {
        let mut f = Fixture::new("m");
        let call = f.import("c", target, 0);
        let run = f.add_function(
            "run",
            0,
            vec![
                Instr::Label(Label(100)),
                Instr::Allocate {
                    stack_need: HeapNeed::Words(0),
                    live: 0,
                },
                Instr::CallExt {
                    target: call,
                    arity: 0,
                },
                Instr::Deallocate(0),
                // The value the callee returned is ignored; the caller now blocks on a
                // receive, which is what lets the test observe which context it is in.
                Instr::Label(Label(101)),
                Instr::LoopRec {
                    fail: Label(102),
                    dst: Operand::X(0),
                },
                Instr::RemoveMessage,
                Instr::Return,
                Instr::Label(Label(102)),
                Instr::Wait { target: Label(101) },
            ],
        );
        f.add_export("run", 0, run);
        let name = f.atom("run");
        (f.finish(), name)
    }

    /// The callee `c`: `recv/0` (a bare receive that suspends on an empty mailbox) and
    /// `noop/0` (returns immediately).
    fn recv_callee() -> Module {
        let mut f = Fixture::new("c");
        let recv = f.add_function(
            "recv",
            0,
            vec![
                Instr::Label(Label(10)),
                Instr::LoopRec {
                    fail: Label(11),
                    dst: Operand::X(0),
                },
                Instr::RemoveMessage,
                Instr::Return,
                Instr::Label(Label(11)),
                Instr::Wait { target: Label(10) },
            ],
        );
        f.add_export("recv", 0, recv);
        let noop = f.add_function("noop", 0, vec![Instr::Label(Label(1)), Instr::Return]);
        f.add_export("noop", 0, noop);
        f.finish()
    }

    /// The caller `m`: a `try`/`catch` guarding a cross-module `call_ext c:boom/0`,
    /// whose handler returns the landed reason from `x(1)`.
    fn caller_guarding(target: &str) -> (Module, Atom) {
        let mut f = Fixture::new("m");
        let call = f.import("c", target, 0);
        let run = f.add_function(
            "run",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::Allocate {
                    stack_need: HeapNeed::Words(1),
                    live: 0,
                },
                Instr::Try {
                    frame: Operand::Y(0),
                    fail: Label(20),
                },
                Instr::CallExt {
                    target: call,
                    arity: 0,
                },
                Instr::TryEnd {
                    frame: Operand::Y(0),
                },
                Instr::Deallocate(1),
                Instr::Return,
                Instr::Label(Label(20)),
                Instr::TryCase {
                    frame: Operand::Y(0),
                },
                Instr::Move {
                    src: Operand::X(1),
                    dst: Operand::X(0),
                },
                Instr::Deallocate(1),
                Instr::Return,
            ],
        );
        f.add_export("run", 0, run);
        let name = f.atom("run");
        (f.finish(), name)
    }

    /// The callee `c`: `boom/0` raises `42` with class `throw`.
    fn raising_callee() -> Module {
        let mut f = Fixture::new("c");
        let throw = f.atom("throw");
        let boom = f.add_function(
            "boom",
            0,
            vec![
                Instr::Label(Label(50)),
                Instr::Move {
                    src: Operand::Atom(throw),
                    dst: Operand::X(0),
                },
                Instr::Move {
                    src: Operand::Integer(42),
                    dst: Operand::X(1),
                },
                Instr::Move {
                    src: Operand::Nil,
                    dst: Operand::X(2),
                },
                Instr::RawRaise,
            ],
        );
        f.add_export("boom", 0, boom);
        f.finish()
    }

    /// Producer 1: a `call_ext` the host resolved through the seam runs the callee
    /// **in this machine**, so the machine's current module must be the callee's.
    #[test]
    fn a_resolved_call_ext_puts_the_machine_in_the_callee_module() {
        let (caller, run) = caller_then_receive("recv");
        let callee = Arc::new(recv_callee());
        let mut ctx = seam(vec![("c".to_string(), Arc::clone(&callee))]);
        let mut machine = Machine::new();

        let outcome = machine
            .run_export_outcome(&caller, run, 0, &[], &mut ctx)
            .expect("the callee suspends on its empty mailbox");
        assert!(
            matches!(outcome, RunOutcome::Wait { .. }),
            "the suspension is reported, got {outcome:?}"
        );
        assert_ne!(
            machine.current_context(),
            0,
            "the machine is inside the registered callee"
        );
        let current = machine
            .current_module()
            .expect("a registered callee has a module handle");
        assert!(
            Arc::ptr_eq(&current, &callee),
            "current_module must be the very handle the seam resolved with"
        );
    }

    /// Producer 2: a cross-module `return` onto the caller's continuation is an
    /// `Step::Enter { context: 0, .. }`, so the machine must switch **back** into the
    /// entry module. The caller then suspends in its own `receive`, which pins both
    /// the context and (via the resume address) the module the position belongs to.
    #[test]
    fn a_cross_module_return_switches_the_machine_back_into_the_caller_context() {
        let (caller, run) = caller_then_receive("noop");
        let callee = Arc::new(recv_callee());
        let mut ctx = seam(vec![("c".to_string(), Arc::clone(&callee))]);
        let mut machine = Machine::new();

        let outcome = machine
            .run_export_outcome(&caller, run, 0, &[], &mut ctx)
            .expect("the caller suspends after the callee returned");
        // The `wait` resumes at `Label(101)`, which is code index 4 of the caller's own
        // `run/0` — so the position is a caller address, not a callee one.
        assert_eq!(outcome, RunOutcome::Wait { func: 0, pc: 4 });
        assert_eq!(
            machine.current_context(),
            0,
            "the return switched the machine back to the entry context"
        );
        assert!(
            machine.current_module().is_none(),
            "context 0 is the borrowed entry module, reported as None"
        );
    }

    /// Producer 3: a raise inside the callee that lands in a catch frame belonging to
    /// the caller is `Step::Enter { context: 0, .. }` — the handler's `(func, pc)` are
    /// the caller's, so the machine must switch back before executing it.
    #[test]
    fn a_raise_landing_in_a_callers_catch_frame_switches_into_the_callers_context() {
        let (caller, run) = caller_guarding("boom");
        let callee = Arc::new(raising_callee());
        let mut ctx = seam(vec![("c".to_string(), Arc::clone(&callee))]);
        let mut machine = Machine::new();

        let outcome = machine
            .run_export_outcome(&caller, run, 0, &[], &mut ctx)
            .expect("the caller's try contains the callee's raise");
        assert_eq!(
            outcome,
            RunOutcome::Returned(make_small(42)),
            "the handler returned the landed reason"
        );
        assert_eq!(
            machine.current_context(),
            0,
            "the handler ran in the caller's context, not the callee's"
        );
        assert!(
            machine.current_module().is_none(),
            "context 0 is the borrowed entry module, reported as None"
        );
        assert_eq!(machine.catch_frame_depth(), 0, "the handler consumed its frame");
    }
}
