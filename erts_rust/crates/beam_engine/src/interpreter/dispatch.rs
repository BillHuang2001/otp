//! The `impl Machine` dispatch loop.
//!
//! `step_instruction` executes exactly one IR instruction (the body of one
//! dispatch-loop iteration); `run_loop`/`run_loop_with_map` are the thin run
//! entries that hand the module-context-aware driver
//! ([`Machine::drive`](crate::machine::Machine)) its starting position, and
//! `enter_run` is the shared per-run prologue.

use beam_common::{ExceptionClass, Result};
use beam_loader::{ImportRef, Instr, Module, NO_FAIL};
use beam_term::{cmp, make_atom, NIL};

use crate::context::ExecutionContext;
use crate::error;
use crate::families;
use crate::machine::{Continuation, Machine, RunOutcome};
use crate::operand::{read_operand, write_operand};
use crate::Step;

use super::{resolve, LabelMap};

impl Machine {
    /// The run entry over the current module context: bind the entry module's
    /// literal area and record the run's entry call ([`Machine::enter_run`]), then
    /// drive the dispatch loop ([`Machine::drive`]).
    ///
    /// `module` is the run's **entry** module (module context `0`); when the machine
    /// is being resumed it is already positioned in the context the process blocked
    /// in, and that context's module — not this one — is what `func`/`pc` index (see
    /// [`Machine::drive`]).
    ///
    /// It does **not** reset the machine: it only (re)binds the entry module's
    /// literal area and records the run's entry call, so
    /// [`Machine::resume_outcome`] can re-enter a suspended process through it
    /// unchanged.
    pub(crate) fn run_loop(
        &mut self,
        module: &Module,
        func: u32,
        pc: u32,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<RunOutcome> {
        self.enter_run(module, func);
        self.drive(module, None, func, pc, ctx)
    }

    /// The per-run prologue shared by [`Machine::run_loop`] and
    /// [`Machine::run_label_outcome`]: bind the running module's literal area so
    /// the operand seam can resolve `Operand::Literal(idx)`, and record the run's
    /// entry `(func, arity)` — **once** per run.
    ///
    /// Both effects are idempotent in the way a resume needs: the module named here is
    /// always the run's entry module (context `0`, see [`Machine::drive`]), and an
    /// entry call is recorded only when this run has none yet (a `resume_outcome`
    /// re-enters the same run without resetting, so the process's initial call stays
    /// the one it entered at). A `func` outside the module records nothing rather
    /// than a bogus entry.
    pub(crate) fn enter_run(&mut self, module: &Module, func: u32) {
        // `reset()` cleared any previous literal binding.
        self.set_literals(module.literals.clone());
        // Context `0` is the entry module; the machine never owns it (the drive loop
        // borrows it), so its area is recorded here for `switch_context` to restore
        // when a cross-module callee returns into it.
        self.entry_literals = Some(module.literals.clone());
        if self.entry_call.is_none() {
            if let Some(function) = module.functions.get(func as usize) {
                self.entry_call = Some((func, function.arity));
            }
        }
    }

    /// Execute **exactly one** instruction at `(func, pc)` and report where control
    /// goes next.
    ///
    /// This is the body of one dispatch-loop iteration factored out of
    /// [`Machine::run_loop_with_map`], so a JIT driver can interpret a *single*
    /// instruction and step the interpreter's state by one instruction (used for
    /// per-instruction deopt). It performs precisely the effect the corresponding
    /// dispatch arm performed in place, then returns:
    ///
    /// * [`Step::Next`] — the instruction advances `pc` by 1 in the same function.
    /// * [`Step::Goto { func, pc }`] — it transfers control (jump/call/select/
    ///   test-fail/raise-landing/…): the returned `(func, pc)` is the next address.
    /// * [`Step::Wait { func, pc }`] — it suspends the process in a `receive`: the
    ///   returned `(func, pc)` is where the module resumes once a message arrives
    ///   (see [`crate::RunOutcome::Wait`]).
    /// * [`Step::Done`] — it would finish the run (a `return`/`call_ext*` reaching
    ///   the bottom frame's [`Continuation::Halt`]); the caller turns this into
    ///   `x(0)` with [`Machine::finish`].
    ///
    /// A raise (or any other typed error — an out-of-range `pc`/function index, an
    /// unsupported opcode/operand) stays an `Err(beam_common::BeamError)`; it is
    /// never a panic and never a wrong result.
    #[inline]
    pub(crate) fn step_instruction(
        &mut self,
        module: &Module,
        labels: &LabelMap,
        func: u32,
        pc: u32,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Step> {
        // Record the machine's execution position: the read-only "saved instruction
        // pointer" behind `Machine::current_code` and the derived
        // `current_function`/`current_location`/`current_stacktrace`
        // (`process_info/2`). Every *interpreted* instruction passes through here —
        // the dispatch loop below and the native tiers' per-instruction deopt helper
        // both call it — so the address is exact at every suspension point (a
        // blocking `wait` is interpreted, never inlined); a natively-executed JIT
        // block does not update it (see the field docs).
        self.current_code = Some((func, pc));
        // The instruction borrows `module` (never `self`), so mutating the machine
        // in the arms below is sound and clone-free.
        let instr = match module.functions.get(func as usize) {
            Some(function) => match function.code.get(pc as usize) {
                Some(instr) => instr,
                None => return Err(error::unsupported("run: pc out of range")),
            },
            None => return Err(error::unsupported("run: function index out of range")),
        };

        match instr {
            // Metadata / no-ops. `executable_line`/`debug_line` exist for
            // instruction counting and breakpoints (they are `_` in
            // `emu/ops.tab:103-108`), so they do not change control flow.
            //
            // `nif_start` (`emu/ops.tab:43`) and `on_load` (`emu/ops.tab:1572`)
            // are bare markers with no body — the C emulator falls straight
            // through them — so they are no-ops too: `nif_start` marks the start
            // of a `-nifs()` function (`beam_ssa_codegen.erl:2451` emits it) and
            // `on_load` marks the `-on_load/1` entry point
            // (`beam_disasm.erl:1259` resolves `{on_load,[]}` to `on_load`).
            // Neither carries operands nor touches machine state.
            Instr::Label(_)
            | Instr::Line(_)
            | Instr::IntCodeEnd
            | Instr::ExecutableLine { .. }
            | Instr::DebugLine { .. }
            | Instr::NifStart
            | Instr::OnLoad => Ok(Step::Next),
            // `func_info Module Function Arity` — the failed-clause raise.
            //
            // A function's **callable** entry label is the label *after* `func_info`
            // (`Export.label == Function::entry_label`, `../beam_loader/CONTEXT.md`),
            // so a normal call never executes this instruction: the only way to reach
            // it is a JUMP from a failed head-pattern/guard `fail` label. C
            // (`i_func_info_IaaI`, `../../erts/emulator/beam/emu/beam_emu.c:578-583`)
            // then unconditionally sets `c_p->freason = EXC_FUNCTION_CLAUSE` and
            // `goto handle_error`: class `error`, reason the **bare atom**
            // `function_clause` (the call arguments ride in the stacktrace, not in the
            // reason — no `{function_clause, Args}` tuple). There is no first-entry or
            // re-entry test in C. Treating it as a no-op instead would fall through to
            // the entry label and re-run the clause tests forever — the
            // `proplists:get_value(a, undefined)` hang.
            Instr::FuncInfo { .. } => {
                // Resolve the reason atom in the running module's atom table; a module
                // that never mentions it cannot name the reason, so a missing atom is a
                // typed `unsupported` (never a substituted, wrong atom).
                let atom = module.atom_table.lookup("function_clause").ok_or_else(|| {
                    error::unsupported("func_info: no 'function_clause' atom in the module")
                })?;
                let reason = make_atom(atom);
                // Install the `error` class atom when the module has it; the throw
                // consults it only when the raise actually lands. A module without it
                // still propagates a typed `BeamError::Raise(..)` uncaught, and a
                // landing fails with a typed `unsupported` — the `install_class_atom`
                // contract (mirrors `families/exceptions.rs::raise_error_atom`).
                if let Some(class_atom) = module.atom_table.lookup(ExceptionClass::Error.as_str())
                {
                    self.set_class_atom(ExceptionClass::Error, make_atom(class_atom));
                }
                self.throw_error(reason)
            }
            Instr::Move { src, dst } => {
                let value = read_operand(self, ctx, src)?;
                write_operand(self, dst, value)?;
                Ok(Step::Next)
            }
            Instr::Swap { a, b } => {
                let va = read_operand(self, ctx, a)?;
                let vb = read_operand(self, ctx, b)?;
                write_operand(self, a, vb)?;
                write_operand(self, b, va)?;
                Ok(Step::Next)
            }
            Instr::Test { kind, fail, args } => {
                if self.eval_test(module, *kind, args, ctx)? {
                    Ok(Step::Next)
                } else if *fail == NO_FAIL {
                    Err(error::unsupported("test/no_fail"))
                } else {
                    let (func, pc) = resolve(labels, *fail)?;
                    Ok(Step::Goto { func, pc })
                }
            }
            Instr::SelectVal { value, fail, pairs } => {
                let scrutinee = read_operand(self, ctx, value)?;
                let mut target = None;
                for (candidate, label) in pairs {
                    let candidate_value = read_operand(self, ctx, candidate)?;
                    if cmp(scrutinee, candidate_value, &module.atom_table).is_eq() {
                        target = Some(*label);
                        break;
                    }
                }
                match target {
                    Some(label) => {
                        let (func, pc) = resolve(labels, label)?;
                        Ok(Step::Goto { func, pc })
                    }
                    None if *fail == NO_FAIL => Err(error::unsupported("select_val/no_fail")),
                    None => {
                        let (func, pc) = resolve(labels, *fail)?;
                        Ok(Step::Goto { func, pc })
                    }
                }
            }
            Instr::Jump { target } => {
                let (func, pc) = resolve(labels, *target)?;
                Ok(Step::Goto { func, pc })
            }
            Instr::GcBif {
                bif,
                fail,
                args,
                dst,
                ..
            } => self.exec_bif(module, labels, *bif, args, *fail, dst, ctx),
            Instr::Bif0 { bif, dst } => {
                // `bif0 Bif Dst` — a non-garbage-collecting BIF with no
                // arguments (`emu/ops.tab:984-985`, e.g. `self`/`node`). There
                // is no fail label, so any error is a raise; C routes the call
                // through `call_light_bif`, whose error path is catch-aware.
                self.exec_bif(module, labels, *bif, &[], NO_FAIL, dst, ctx)
            }
            // `bif{1,2,3} Fail Bif Arg… Dst` — a non-garbage-collecting BIF.
            // Like the `GcBif` arm minus the heap/live check: on failure, jump
            // to `fail` if it is live (guard failure), else raise (landing in a
            // live catch frame when there is one).
            Instr::Bif {
                bif,
                fail,
                args,
                dst,
                ..
            } => self.exec_bif(module, labels, *bif, args, *fail, dst, ctx),
            Instr::Call { target, .. } => {
                // Store the continuation in the *current* frame's CP slot, tagged
                // with the module context it points into (the one executing now).
                self.set_continuation(Continuation::Address {
                    func,
                    pc: pc + 1,
                    context: self.current_context(),
                })?;
                let (callee, callee_pc) = resolve(labels, *target)?;
                // Contract A.8: report the callee's `call` event (the oracle's
                // breakpoint fires on entry to the callee). Gated, so an untraced
                // run pays one predictable branch and builds no event.
                if self.trace_on {
                    self.trace_call_local(module, callee, ctx);
                }
                Ok(Step::Goto {
                    func: callee,
                    pc: callee_pc,
                })
            }
            Instr::CallLast {
                target, deallocate, ..
            } => {
                // `i_call_last(CallDest, Deallocate)` is `$deallocate(D);
                // $DISPATCH_REL(Dest)` (`emu/instrs.tab:163`): the frame is popped
                // as part of *entering* the tail-callee. Resolve the target
                // **first** so a bad label cannot leave a popped frame behind —
                // the jump itself is the only step that depends on the popped
                // frame, and by then nothing may fail.
                let (callee, callee_pc) = resolve(labels, *target)?;
                self.pop_frame_checked(*deallocate)?;
                // The `call` event names the tail-callee; the frame that remains is
                // the original caller's, so the callee's eventual `return` reports
                // *that* caller (`return_to`), never the tail-called-into function.
                if self.trace_on {
                    self.trace_call_local(module, callee, ctx);
                }
                Ok(Step::Goto {
                    func: callee,
                    pc: callee_pc,
                })
            }
            Instr::CallOnly { target, .. } => {
                let (callee, callee_pc) = resolve(labels, *target)?;
                if self.trace_on {
                    self.trace_call_local(module, callee, ctx);
                }
                Ok(Step::Goto {
                    func: callee,
                    pc: callee_pc,
                })
            }
            Instr::CallExt { target, arity } => {
                // An ext call carries no fail label: a failure is a raise (C's
                // `i_call_ext`/`call_light_bif` end in `handle_error`, so a live
                // catch frame catches it).
                //
                // Only the callee's *entry* is traced: the oracle emits no
                // `return_to` for a BIF/cross-module completion, so reporting a
                // return here would invent an event (see `super::trace`).
                if self.trace_on {
                    self.trace_call_import(module, labels, *target, ctx);
                }
                // A host that can run the callee **in this machine** suspends and
                // resumes *this* process through it: enter the callee's code with the
                // caller's return address stored in the live frame, exactly like the
                // local `Instr::Call` (the callee's `return` reads it back).
                if let Some((context, callee, callee_pc)) =
                    self.resolve_external_entry(module, labels, *target, ctx)?
                {
                    self.set_continuation(Continuation::Address {
                        func,
                        pc: pc + 1,
                        context: self.current_context(),
                    })?;
                    return Ok(Step::Enter {
                        context,
                        func: callee,
                        pc: callee_pc,
                    });
                }
                match self.call_import(module, labels, *target, *arity, ctx) {
                    Ok(value) => {
                        self.set_x(0, value)?;
                        Ok(Step::Next)
                    }
                    Err(e) => self.raise_bif_error(module, e),
                }
            }
            Instr::CallExtOnly { target, arity } => {
                if self.trace_on {
                    self.trace_call_import(module, labels, *target, ctx);
                }
                // A tail call: the caller's frame is reused, so nothing is stored —
                // the callee's `return` follows the frame's *existing* continuation
                // (exactly like the local `Instr::CallOnly`).
                if let Some((context, callee, callee_pc)) =
                    self.resolve_external_entry(module, labels, *target, ctx)?
                {
                    return Ok(Step::Enter {
                        context,
                        func: callee,
                        pc: callee_pc,
                    });
                }
                match self.call_import(module, labels, *target, *arity, ctx) {
                    Ok(value) => {
                        self.set_x(0, value)?;
                        self.continuation_step()
                    }
                    Err(e) => self.raise_bif_error(module, e),
                }
            }
            Instr::CallExtLast {
                target,
                arity,
                deallocate,
            } => {
                // C expands it to `call_light_bif Bif Bif | deallocate_return D`
                // (`emu/ops.tab:964`), and `call_light_bif`
                // (`emu/bif_instrs.tab:262`) asserts `E == c_p->stop` across the
                // call — the frame pointer is untouched while the BIF runs — so a
                // raising callee unwinds from the *live* frame and `handle_error` →
                // `next_catch` reports the callee's own reason (`:312`). The pop
                // belongs to the callee's success path only:
                //
                // * **success** — pop the frame and follow the (now current)
                //   caller frame's continuation, exactly like `deallocate_return D`
                //   followed by `$RETURN()`.
                // * **raise** — nothing is popped; the frame discipline at the
                //   raise point is C's, so the unwinder (and a live `try`/`catch`)
                //   sees the right frames. Popping first would both corrupt that
                //   unwind and reject a `Deallocate` operand the compiler
                //   legitimately left **stale**: `trim N` (`emu/instrs.tab:593`,
                //   `E += N`) shrinks a frame, and the operand of a following
                //   never-returning `call_ext_last` (e.g. `erlang:error/1`) is then
                //   dead code C never applies — the shape that produced the bogus
                //   `deallocate: frame size mismatch` reason instead of the BIF's.
                if self.trace_on {
                    self.trace_call_import(module, labels, *target, ctx);
                }
                // The seam target is resolved **before** the pop, so a label/context
                // the callee's module does not define cannot leave a popped frame
                // behind (the local `Instr::CallLast` discipline); once it is
                // resolved, entering the callee is the only step that depends on the
                // popped frame and nothing may fail after it.
                if let Some((context, callee, callee_pc)) =
                    self.resolve_external_entry(module, labels, *target, ctx)?
                {
                    self.pop_frame_checked(*deallocate)?;
                    return Ok(Step::Enter {
                        context,
                        func: callee,
                        pc: callee_pc,
                    });
                }
                match self.call_import(module, labels, *target, *arity, ctx) {
                    Ok(value) => {
                        self.set_x(0, value)?;
                        self.pop_frame_checked(*deallocate)?;
                        self.continuation_step()
                    }
                    Err(e) => self.raise_bif_error(module, e),
                }
            }
            Instr::Allocate { stack_need, .. } => {
                // `stack_need` is a `HeapNeed` (either a bare word count or a
                // `z,3` allocation list); `.words()` recovers the total the frame
                // reserves (see `beam_loader::HeapNeed`).
                self.push_frame(stack_need.words())?;
                Ok(Step::Next)
            }
            Instr::AllocateHeap {
                stack_need,
                heap_need,
                live,
            } => {
                self.ensure_heap(heap_need.words(), *live, ctx)?;
                self.push_frame(stack_need.words())?;
                Ok(Step::Next)
            }
            Instr::TestHeap { heap_need, live } => {
                self.ensure_heap(heap_need.words(), *live, ctx)?;
                Ok(Step::Next)
            }
            Instr::Deallocate(n) => {
                self.pop_frame_checked(*n)?;
                Ok(Step::Next)
            }
            Instr::Trim { n, .. } => {
                self.trim(*n)?;
                Ok(Step::Next)
            }
            Instr::InitYregs { registers } => {
                // `init_yregs {Yregs}` — initialize the listed (Y) registers to
                // `[]` (`emu/instrs.tab:506-508` `i_init`: `make_blank($Y)`, and
                // `make_blank` is `NIL`). Emitted right after `allocate`, so the
                // frame is live.
                for register in registers {
                    write_operand(self, register, NIL)?;
                }
                Ok(Step::Next)
            }
            Instr::Return => match self.continuation()? {
                Continuation::Halt => Ok(Step::Done),
                Continuation::Address {
                    func: caller,
                    pc: caller_pc,
                    context,
                } => {
                    // Contract A.8: the returning function and the function control
                    // returns *to*. The frame is already the caller's (the compiler's
                    // `deallocate` has popped the returning function's own frame), so
                    // a tail-called function's `return` names the ORIGINAL caller —
                    // and the continuation names the *caller's* module context, which
                    // is what brings a cross-module `call_ext` back home.
                    //
                    // The event is only reported for an intra-module return: the
                    // oracle's `return_to` names a function in the frame the trace
                    // would have to resolve, and `module` here is the *callee's*
                    // module, so a cross-context caller's index is not a nameable
                    // function of it (reporting one would invent an event).
                    if self.trace_on && context == self.current_context() {
                        self.trace_return(module, func, caller, ctx);
                    }
                    if context == self.current_context() {
                        Ok(Step::Goto {
                            func: caller,
                            pc: caller_pc,
                        })
                    } else {
                        Ok(Step::Enter {
                            context,
                            func: caller,
                            pc: caller_pc,
                        })
                    }
                }
            },
            // An opcode the loader has no typed IR variant for: a residual
            // [`crate::generic`] opcode is reported as the documented
            // `unsupported(op.name())` typed error carrying its own `ops.tab` name.
            Instr::Generic { op, .. } => crate::generic::exec(*op),
            // Every remaining IR variant is routed to the per-instruction-family
            // dispatch seam ([`crate::families`]). Each family is currently a stub
            // returning `Ok(None)`, so an instruction no family claims yields the
            // same documented `unsupported` error the interpreter emitted before
            // this seam existed; a family that later implements an opcode returns
            // a [`Step`] telling the driver where to continue. Never a panic and
            // never a wrong result.
            other => families::dispatch(self, module, labels, other, func, pc, ctx),
        }
    }

    /// The run entry over an **already-built** label map (shared by
    /// [`Machine::run_label_outcome`], which resolved its entry label against this
    /// very map, and [`Machine::resume_outcome`]).
    ///
    /// A thin wrapper over the module-context-aware driver
    /// ([`Machine::drive`](crate::machine::Machine)): the loop itself — including
    /// the re-binding of the module and its literal area when a `call_ext*` the
    /// host resolved runs a callee in this machine — lives there.
    pub(crate) fn run_loop_with_map(
        &mut self,
        module: &Module,
        labels: &LabelMap,
        func: u32,
        pc: u32,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<RunOutcome> {
        self.drive(module, Some(labels), func, pc, ctx)
    }

    /// Resolve a `call_ext*`'s target through the host's cross-module-call seam
    /// ([`ExecutionContext::resolve_external_call`]) into an address **this machine**
    /// can enter.
    ///
    /// `Ok(None)` means the host cannot run the callee here (the seam's `Ok(None)`
    /// default): the caller keeps the synchronous `call_bif` path unchanged.
    /// `Ok(Some((context, func, pc)))` means the callee's module is registered as
    /// module context `context` and `(func, pc)` is its export entry — the caller's
    /// `CallExt*` arm applies its own frame discipline and returns
    /// [`Step::Enter`].
    ///
    /// The `(module name, function name, arity)` triple handed to the host is the
    /// **same** one [`Machine::call_import`] would pass to `call_bif` (the import
    /// table's, resolved through the per-run cache), so a host can answer the seam
    /// however it answers the BIF dispatch. The returned entry `Label` is resolved
    /// against the **callee's own** label index; an unknown label is the typed
    /// [`crate::error::unknown_label`], never a jump into the wrong module.
    fn resolve_external_entry(
        &mut self,
        module: &Module,
        labels: &LabelMap,
        target: ImportRef,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Option<(u32, u32, u32)>> {
        let (module_name, function_name, arity) = labels.import_target(module, target)?;
        let Some((callee, label)) = ctx.resolve_external_call(module_name, function_name, arity)?
        else {
            return Ok(None);
        };
        // Registering is idempotent by the handle's allocation identity, so a
        // recursive or repeated cross-module call reuses one context (and its
        // already-built label index).
        let context = self.register_context(&callee);
        let handle = self
            .module_context(context)
            .ok_or_else(|| error::unsupported("run: unknown module context"))?;
        let (func, pc) = handle.labels.resolve(label)?;
        Ok(Some((context, func, pc)))
    }
}
