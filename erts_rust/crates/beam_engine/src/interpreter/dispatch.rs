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

//! The `impl Machine` dispatch loop.
//!
//! `step_instruction` executes exactly one IR instruction (the body of one
//! dispatch-loop iteration); `run_loop`/`run_loop_with_map` are the thin run
//! entries that hand the module-context-aware driver
//! ([`Machine::drive`](crate::machine::Machine)) its starting position, and
//! `enter_run` is the shared per-run prologue.

use beam_common::{ExceptionClass, Result};
use beam_loader::{ImportRef, Instr, Module, NO_FAIL};
use beam_term::{cmp, is_atom, is_list, is_nil, make_atom, atom_val, Eterm, NIL};

use crate::context::ExecutionContext;
use crate::error;
use crate::families;
use crate::families::funs::{apply_target, raise_badarg, ApplyTarget};
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
    ///
    /// **The uniform raise funnel.** A raise that leaves the machine **uncaught**
    /// gets its symbolic frames attached here, at the one point the interpreter's
    /// dispatch loop and the native tiers' per-instruction deopt helper
    /// ([`crate::jit::baseline`]'s `jit_step_one`) both pass through: in the run's
    /// **entry** context (`0`) the active `module` IS the entry module, so
    /// [`Machine::with_uncaught_frames`] can name every frame. A raise inside a
    /// cross-module callee (a non-zero context) is left untouched here, because the
    /// driver ([`Machine::drive`], which knows the true entry module) attaches the
    /// fuller frame set — naming the callee's module for a context-`0` caller would
    /// be a wrong result.
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
        let result = self.step_instruction_inner(module, labels, func, pc, ctx);
        // The uniform raise funnel (see this method's docs). In context `0` the
        // active module is the entry module, so `Some(module)` is exactly the
        // `entry_module` `Machine::raise_stacktrace` needs; in a callee context the
        // result is left for the driver, which knows the true entry module.
        if self.current_context() == 0 {
            result.map_err(|err| self.with_uncaught_frames(err, Some(module), func, pc))
        } else {
            result
        }
    }

    /// The body of one dispatch-loop iteration — the effect of the corresponding
    /// dispatch arm, without the raise funnel [`Machine::step_instruction`] wraps
    /// around it (so the driver and the native tiers share one step).
    fn step_instruction_inner(
        &mut self,
        module: &Module,
        labels: &LabelMap,
        func: u32,
        pc: u32,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Step> {
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
                // The frames an *uncaught* `function_clause` carries (the boot path's
                // failure mode) are attached uniformly as the raise leaves the machine
                // ([`Machine::with_uncaught_frames`], via `step_instruction`), so this
                // arm stays the plain raise and every raise kind shares one frame
                // derivation. A caught raise is unaffected (the frames are metadata on
                // the uncaught `Raise` only).
                raise_function_clause_no_fail(self, module, "func_info")
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
                    raise_function_clause_no_fail(self, module, "test")
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
                    None if *fail == NO_FAIL => {
                        raise_function_clause_no_fail(self, module, "select_val")
                    }
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
                if let Some(target) =
                    self.resolve_call_ext_apply(module, labels, *target, ctx)?
                {
                    return self.apply_import_target(target, ImportFrame::Call, func, pc);
                }
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
                if let Some(target) =
                    self.resolve_call_ext_apply(module, labels, *target, ctx)?
                {
                    return self.apply_import_target(target, ImportFrame::Only, func, pc);
                }
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
                if let Some(target) =
                    self.resolve_call_ext_apply(module, labels, *target, ctx)?
                {
                    return self.apply_import_target(target, ImportFrame::Last(*deallocate), func, pc);
                }
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
            // `deallocate N; return` — the loader's fused superinstruction
            // (`beam_loader::lower::fuse`). Exactly the two constituents in order: pop
            // the frame (validated like `Instr::Deallocate`), then take the return
            // path the popped frame's continuation now addresses.
            Instr::DeallocateReturn(n) => {
                self.pop_frame_checked(*n)?;
                self.return_step(module, func, ctx)
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
            Instr::Return => self.return_step(module, func, ctx),
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

    /// The `return` path: follow the current frame's continuation
    /// ([`Machine::continuation`]) to its caller, or halt the run when it is the
    /// bottom frame's [`Continuation::Halt`].
    ///
    /// Factored out of the `Instr::Return` arm so the loader's fused
    /// `Instr::DeallocateReturn` (which has already popped the returning function's
    /// frame) shares it verbatim. `module`/`func` name the **returning** function for
    /// the contract-A.8 [`Machine::trace_return`] event.
    fn return_step(
        &mut self,
        module: &Module,
        func: u32,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Step> {
        match self.continuation()? {
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

    /// The `call_ext*`-encoded `erlang:apply/2,3` special case — C's
    /// `fixed_apply`'s `/* Handle apply of apply/3 ... */` path
    /// (`../../erts/emulator/beam/beam_common.c:1539-1557`, which falls through to
    /// `apply()` lines 1433-1524).
    ///
    /// `erlang:apply/2,3` is a **floor** module in the runtime, so
    /// [`ExecutionContext::resolve_external_call`] declines it and a `call_ext`
    /// would otherwise fold into the host's synchronous `apply` BIF, which runs the
    /// callee on a *fresh* machine — a `receive` inside the callee then dies in the
    /// uncatchable typed `blocked()` gap instead of suspending *this* process. C
    /// instead resolves the export and **jumps into its code in the same process**,
    /// so the callee's `receive` suspends the caller normally.
    ///
    /// This method returns `Ok(None)` for any import that is not `erlang:apply/2,3`
    /// (the caller keeps its normal seam/BIF route). Otherwise it decodes the call
    /// exactly as C's `apply()` does and hands the outcome to
    /// [`funs::apply_target`](crate::families::funs::apply_target):
    ///
    /// * unwrap C's nested `apply of apply/3` shape (a 3-element `[M, F, Args]`
    ///   argument list where `M == erlang`, `F == apply`) in a loop;
    /// * walk the argument list into `x(0..N)` (`error:system_limit` past C's
    ///   `MAX_ARG`; a non-proper list is `badarg`);
    /// * place the resolved `Module` at `x(N)` and `Function` at `x(N+1)` (the
    ///   `fixed_apply` register shape) and dispatch through
    ///   [`funs::apply_target`](crate::families::funs::apply_target).
    ///
    /// Every shape error (a non-atom module/function, a non-list/non-proper `Args`)
    /// is the **catchable** `badarg` the oracle produces — raised through
    /// [`funs::raise_badarg`](crate::families::funs::raise_badarg), which resolves
    /// the `badarg` atom from the **running** module's table (never a fabricated
    /// atom). The returned [`ApplyTarget`] carries the dispatch, and the caller
    /// applies its own `CallExt*` frame discipline.
    fn resolve_call_ext_apply(
        &mut self,
        module: &Module,
        labels: &LabelMap,
        target: ImportRef,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Option<ApplyTarget>> {
        let (module_name, function_name, import_arity) = labels.import_target(module, target)?;
        if module_name != "erlang" || function_name != "apply" {
            return Ok(None);
        }
        if import_arity != 2 && import_arity != 3 {
            return Ok(None);
        }
        // C's `apply()` argument decode (`beam_common.c:1439-1441`): `reg[0] = Module`,
        // `reg[1] = Function`, `reg[2] = Args`. `apply/2` has no argument list, so it
        // is the `Args = []` case — exactly what the oracle's `erlang:apply/2` BIF
        // computes (`Module:Function()` with no arguments).
        let mut module_term = self.x(0).ok_or_else(|| error::bad_register(0))?;
        let mut function_term = self.x(1).ok_or_else(|| error::bad_register(1))?;
        let mut args_term = if import_arity == 3 {
            self.x(2).ok_or_else(|| error::bad_register(2))?
        } else {
            NIL
        };
        // `erlang`/`apply` are resolved against the running module's atom table: an
        // atom index is meaningful only against the table it is read with, so a
        // module that cannot name `erlang`/`apply` simply never matches.
        let erlang_atom = module.atom_table.lookup("erlang");
        let apply_atom = module.atom_table.lookup("apply");
        // C's `apply()` checks `is_not_atom(function)` up front, before the loop.
        if !is_atom(function_term) {
            return Ok(Some(ApplyTarget::Raised(raise_badarg(self, module)?)));
        }
        // C's nested `apply of apply/3` unwrap (`beam_common.c:1462-1497`): while the
        // target is `erlang:apply` and `Args` is exactly `[M, F, Inner]`, replace
        // `(Module, Function, Args)` with `(M, F, Inner)` and repeat.
        loop {
            if !is_atom(module_term) {
                return Ok(Some(ApplyTarget::Raised(raise_badarg(self, module)?)));
            }
            let is_erlang = erlang_atom.is_some_and(|a| atom_val(module_term) == a);
            let is_apply = apply_atom.is_some_and(|a| atom_val(function_term) == a);
            if !(is_erlang && is_apply) {
                break;
            }
            let Some((m, f, inner)) = unwrap_apply3(args_term) else {
                break;
            };
            module_term = m;
            function_term = f;
            args_term = inner;
            if !is_atom(function_term) {
                return Ok(Some(ApplyTarget::Raised(raise_badarg(self, module)?)));
            }
        }
        // Walk the argument list into `x(0..N)` (C's `beam_common.c:1503-1516`). A
        // list longer than C's `MAX_ARG` (`erl_vm.h:48`, 255) is `error:system_limit`;
        // a non-proper tail is `badarg`.
        let mut count: u32 = 0;
        let mut cursor = args_term;
        while is_list(cursor) {
            if count as usize >= MAX_ARG {
                return Ok(Some(ApplyTarget::Raised(
                    self.raise_bif_error(module, beam_common::BeamError::SystemLimit)?,
                )));
            }
            let element = crate::terms::cons_head(cursor)?;
            self.set_x(count, element)?;
            count += 1;
            cursor = crate::terms::cons_tail(cursor)?;
        }
        if !is_nil(cursor) {
            return Ok(Some(ApplyTarget::Raised(raise_badarg(self, module)?)));
        }
        // `fixed_apply`'s register shape for the resolved `M`/`F`: the module is
        // `x(N)` and the function is `x(N+1)`, with the arguments in `x(0..N)`.
        self.set_x(count, module_term)?;
        self.set_x(count + 1, function_term)?;
        Ok(Some(apply_target(self, module, count, ctx)?))
    }

    /// Apply an arm's own frame discipline to an [`ApplyTarget`] produced by
    /// [`Machine::resolve_call_ext_apply`].
    ///
    /// The `call_ext*` forms differ only in their frame handling
    /// ([`ImportFrame`]): a non-tail `call_ext` stores its continuation on the
    /// **enter** path and yields `Step::Next` on a synchronous value; the tail forms
    /// store nothing (the frame's existing continuation is reused) and the
    /// `call_ext_last` form pops the frame **before** entering (or, on a synchronous
    /// value, before following the now-current caller's continuation). A
    /// [`ApplyTarget::Raised`] leaves the caller's frame untouched and its landed
    /// `x(0..=3)` registers are returned as-is.
    fn apply_import_target(
        &mut self,
        target: ApplyTarget,
        frame: ImportFrame,
        func: u32,
        pc: u32,
    ) -> Result<Step> {
        match target {
            ApplyTarget::Enter {
                context,
                func: target_func,
                pc: target_pc,
            } => {
                match frame {
                    ImportFrame::Call => {
                        self.set_continuation(Continuation::Address {
                            func,
                            pc: pc + 1,
                            context: self.current_context(),
                        })?;
                    }
                    ImportFrame::Only => {}
                    ImportFrame::Last(deallocate) => self.pop_frame_checked(deallocate)?,
                }
                Ok(Step::Enter {
                    context,
                    func: target_func,
                    pc: target_pc,
                })
            }
            ApplyTarget::Called(value) => {
                self.set_x(0, value)?;
                match frame {
                    ImportFrame::Call => Ok(Step::Next),
                    ImportFrame::Only => self.continuation_step(),
                    ImportFrame::Last(deallocate) => {
                        self.pop_frame_checked(deallocate)?;
                        self.continuation_step()
                    }
                }
            }
            ApplyTarget::Raised(step) => Ok(step),
        }
    }
}

/// The frame discipline a `call_ext*` form applies to its resolved target.
///
/// See [`Machine::apply_import_target`].
enum ImportFrame {
    /// `call_ext`: a non-tail call — the caller's next instruction is stored as the
    /// live frame's continuation on the enter path, and a synchronous value is
    /// followed by `Step::Next`.
    Call,
    /// `call_ext_only`: a tail call — nothing is stored; the frame's *existing*
    /// continuation is followed, and a synchronous value is followed through
    /// [`Machine::continuation_step`].
    Only,
    /// `call_ext_last`: a tail call that pops the `deallocate` frame before entering
    /// the callee (or before following the successor frame's continuation).
    Last(u32),
}

/// The argument count past which C's `apply()` raises `error:system_limit`
/// (`../../erts/emulator/beam/erl_vm.h:48`, `MAX_ARG = 255`).
const MAX_ARG: usize = 255;

/// C's `apply of apply/3` argument-list shape (`beam_common.c:1472-1495`): a
/// **3-element** list `[M, F, Inner]`, returning the three elements. Anything else
/// yields `None` (C `break`s out of the unwrap loop).
fn unwrap_apply3(list: Eterm) -> Option<(Eterm, Eterm, Eterm)> {
    if !is_list(list) {
        return None;
    }
    let m = crate::terms::cons_head(list).ok()?;
    let tail = crate::terms::cons_tail(list).ok()?;
    if !is_list(tail) {
        return None;
    }
    let f = crate::terms::cons_head(tail).ok()?;
    let tail2 = crate::terms::cons_tail(tail).ok()?;
    if !is_list(tail2) {
        return None;
    }
    let inner = crate::terms::cons_head(tail2).ok()?;
    let tail3 = crate::terms::cons_tail(tail2).ok()?;
    if !is_nil(tail3) {
        return None;
    }
    Some((m, f, inner))
}

/// Raise the failed-clause reason: class `error` with the **bare atom**
/// `function_clause` — the engine's ONE definition of the `NO_FAIL` reason.
///
/// C's `func_info` is the instruction a failed head-pattern/guard jumps to, and
/// `i_func_info_IaaI` (`../../erts/emulator/beam/emu/beam_emu.c:578-583`)
/// unconditionally sets `c_p->freason = EXC_FUNCTION_CLAUSE` and `goto handle_error`,
/// which (`../../erts/emulator/beam/beam_common.c:448-544`, `expand_error_value`)
/// materializes class `error` with the **bare atom** `function_clause` — the call
/// arguments ride in the stacktrace, never in a `{function_clause, Args}` tuple.
///
/// C's plain `FAIL(Fail)` (`emu/macros.tab:56-60`) sets **no** reason, so a
/// branchless `$FAIL` sentinel (`{f,0}`) failure has nothing to raise on its own and
/// is routed to that same `func_info` behaviour — which is why `test`/`select_val`
/// and the maps read path share this reason.
///
/// It is engine-defined: the installed `erlc` never emits `{f,0}` for
/// `test`/`select_val`/`get_map_elements`/`has_map_fields` (it always supplies a real
/// fail label), so no corpus case can reach it.
///
/// The reason atom is resolved **only** from the running module's own atom table, so
/// a module that never mentions `function_clause` yields the honest typed
/// `error::unsupported("<what>: no 'function_clause' atom in the module")` and NEVER
/// a substituted atom index (an atom index is valid only against the table it is read
/// with). The `error` class atom is installed only when the module has it; the throw
/// consults it only when the raise actually lands, so a module without it still
/// propagates a typed `BeamError::Raise(..)` uncaught and a landing fails with a typed
/// `unsupported` — the `install_class_atom` contract (mirrors
/// `families/exceptions.rs::raise_error_atom`).
///
/// `what` names the raising instruction/site (`"func_info"`, `"test"`,
/// `"select_val"`, `"maps"`) for the typed error above.
pub(crate) fn raise_function_clause_no_fail(
    machine: &mut Machine,
    module: &Module,
    what: &str,
) -> Result<Step> {
    let atom = module.atom_table.lookup("function_clause").ok_or_else(|| {
        error::unsupported(&format!(
            "{what}: no 'function_clause' atom in the module"
        ))
    })?;
    let reason = make_atom(atom);
    // Install the `error` class atom when the module has it (see the doc above).
    if let Some(class_atom) = module.atom_table.lookup(ExceptionClass::Error.as_str()) {
        machine.set_class_atom(ExceptionClass::Error, make_atom(class_atom));
    }
    machine.throw_error(reason)
}
