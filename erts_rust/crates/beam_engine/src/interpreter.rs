//! The direct-threaded dispatch loop (`impl Machine`).
//!
//! One `match` arm per Milestone-1 [`beam_loader::Instr`] variant implements the
//! interpreter model of Part II §F without `unsafe`/computed-goto (the IR is
//! pre-decoded, so typed dispatch is equivalent to a jump table). The
//! instruction bodies reproduce the C interpreter exactly: the frame/continuation
//! conventions of `../../erts/emulator/beam/emu/macros.tab` and the
//! `gc_bif`/common-BIF semantics of `../../erts/emulator/beam/emu/beam_emu.c`.
//!
//! Failure is return-based: a `gc_bif`/`bif`/`call_ext` error with a live fail
//! label branches (guard failure); without one it **raises** — landing in a live
//! `try`/`catch` frame when there is one (C's `handle_error`/`next_catch`,
//! `../../erts/emulator/beam/beam_common.c:580-679`, reached from `gc_bif`/
//! `bif`/`call_light_bif` in `emu/macros.tab:266-278` and
//! `emu/bif_instrs.tab:308-314`) and otherwise propagating out of the run as
//! `Err(BeamError)`. Every unsupported opcode/operand is a documented typed error,
//! never a panic.

use std::collections::BTreeMap;

use beam_common::{BeamError, ExceptionClass, Result};
use beam_loader::{ImportRef, Instr, Label, Module, Operand, TestKind, NO_FAIL};
use beam_term::{
    cmp, cmp_arith, is_atom, is_internal_pid, is_internal_port, is_nil, is_not_nil_list, make_atom,
    Eterm, MAX_ARITYVAL, NIL,
};

use crate::context::ExecutionContext;
use crate::error;
use crate::families;
use crate::machine::{Continuation, Machine};
use crate::operand::{read_operand, write_operand};
use crate::predicates;
use crate::Step;

/// Build the module-wide `Label -> (function index, code index)` map.
///
/// Labels are module-unique code addresses, so a call/fail target may live in a
/// different function than the instruction that references it (the slice's
/// `fib/1` tail-calls into `fib/3`). Every function's [`beam_loader::Function::labels`]
/// contributes to the one map.
pub(crate) fn build_label_map(module: &Module) -> BTreeMap<Label, (u32, u32)> {
    let mut map = BTreeMap::new();
    for (func_index, function) in module.functions.iter().enumerate() {
        for (label, &code_index) in &function.labels {
            map.insert(*label, (func_index as u32, code_index as u32));
        }
    }
    map
}

/// Resolve a label against an already-built map; a missing label is a typed error.
pub(crate) fn resolve(labels: &BTreeMap<Label, (u32, u32)>, label: Label) -> Result<(u32, u32)> {
    labels
        .get(&label)
        .copied()
        .ok_or_else(|| error::unknown_label(label))
}

/// Resolve a module-local label to its `(function index, code offset)`.
pub(crate) fn resolve_label(module: &Module, label: Label) -> Result<(u32, u32)> {
    resolve(&build_label_map(module), label)
}

/// Resolve an import reference to `(module name, function name, arity)`.
fn import_target(module: &Module, target: ImportRef) -> Result<(&str, &str, u32)> {
    let import = module
        .imports
        .get(target.0 as usize)
        .ok_or_else(|| error::unsupported("call: import out of range"))?;
    let module_name = module
        .atom_name(import.module)
        .ok_or_else(|| error::unsupported("call: import module atom"))?;
    let function_name = module
        .atom_name(import.function)
        .ok_or_else(|| error::unsupported("call: import function atom"))?;
    Ok((module_name, function_name, import.arity))
}

impl Machine {
    /// The direct-threaded dispatch loop over the Milestone-1 opcode subset.
    ///
    /// Entry point: function index `func` (into [`Module::functions`]) and code
    /// offset `pc`; runs to the function's `return` and yields `x(0)`.
    pub(crate) fn run_loop(
        &mut self,
        module: &Module,
        func: u32,
        pc: u32,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        // Bind the running module's literal area so the operand seam can resolve
        // `Operand::Literal(idx)` (the module is fixed for the whole run; see the
        // `Machine::literals` field docs). `reset()` cleared any previous binding.
        self.set_literals(module.literals.clone());
        let labels = build_label_map(module);
        self.run_loop_with_map(module, &labels, func, pc, ctx)
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
    /// * [`Step::Done`] — it would finish the run (a `return`/`call_ext*` reaching
    ///   the bottom frame's [`Continuation::Halt`]); the caller turns this into
    ///   `x(0)` with [`Machine::finish`].
    ///
    /// A raise (or any other typed error — an out-of-range `pc`/function index, an
    /// unsupported opcode/operand) stays an `Err(beam_common::BeamError)`; it is
    /// never a panic and never a wrong result.
    pub(crate) fn step_instruction(
        &mut self,
        module: &Module,
        labels: &BTreeMap<Label, (u32, u32)>,
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
            | Instr::FuncInfo { .. }
            | Instr::IntCodeEnd
            | Instr::ExecutableLine { .. }
            | Instr::DebugLine { .. }
            | Instr::NifStart
            | Instr::OnLoad => Ok(Step::Next),
            Instr::Move { src, dst } => {
                let value = read_operand(self, src)?;
                write_operand(self, dst, value)?;
                Ok(Step::Next)
            }
            Instr::Swap { a, b } => {
                let va = read_operand(self, a)?;
                let vb = read_operand(self, b)?;
                write_operand(self, a, vb)?;
                write_operand(self, b, va)?;
                Ok(Step::Next)
            }
            Instr::Test { kind, fail, args } => {
                if self.eval_test(module, *kind, args)? {
                    Ok(Step::Next)
                } else if *fail == NO_FAIL {
                    Err(error::unsupported("test/no_fail"))
                } else {
                    let (func, pc) = resolve(labels, *fail)?;
                    Ok(Step::Goto { func, pc })
                }
            }
            Instr::SelectVal { value, fail, pairs } => {
                let scrutinee = read_operand(self, value)?;
                let mut target = None;
                for (candidate, label) in pairs {
                    let candidate_value = read_operand(self, candidate)?;
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
            } => {
                let (module_name, function_name, arity) = import_target(module, *bif)?;
                let mut arg_terms = Vec::with_capacity(args.len());
                for arg in args {
                    arg_terms.push(read_operand(self, arg)?);
                }
                match ctx.call_bif(module_name, function_name, arity, &arg_terms) {
                    Ok(value) => {
                        write_operand(self, dst, value)?;
                        Ok(Step::Next)
                    }
                    // No fail label ⇒ a *raise*: land in a live catch frame
                    // (C falls through to `find_func_info`/`handle_error`) or
                    // propagate the typed error.
                    Err(e) if *fail == NO_FAIL => self.raise_bif_error(module, e),
                    // Guard failure: jump to the fail label.
                    Err(_) => {
                        let (func, pc) = resolve(labels, *fail)?;
                        Ok(Step::Goto { func, pc })
                    }
                }
            }
            Instr::Bif0 { bif, dst } => {
                // `bif0 Bif Dst` — a non-garbage-collecting BIF with no
                // arguments (`emu/ops.tab:984-985`, e.g. `self`/`node`). There
                // is no fail label, so any error is a raise; C routes the call
                // through `call_light_bif`, whose error path is catch-aware.
                let (module_name, function_name, arity) = import_target(module, *bif)?;
                match ctx.call_bif(module_name, function_name, arity, &[]) {
                    Ok(value) => {
                        write_operand(self, dst, value)?;
                        Ok(Step::Next)
                    }
                    Err(e) => self.raise_bif_error(module, e),
                }
            }
            Instr::Bif {
                bif,
                fail,
                args,
                dst,
                ..
            } => {
                // `bif{1,2,3} Fail Bif Arg… Dst` — a non-garbage-collecting BIF.
                // Like the `GcBif` arm minus the heap/live check: on failure,
                // jump to `fail` if it is live (guard failure), else raise
                // (landing in a live catch frame when there is one).
                let (module_name, function_name, arity) = import_target(module, *bif)?;
                let mut arg_terms = Vec::with_capacity(args.len());
                for arg in args {
                    arg_terms.push(read_operand(self, arg)?);
                }
                match ctx.call_bif(module_name, function_name, arity, &arg_terms) {
                    Ok(value) => {
                        write_operand(self, dst, value)?;
                        Ok(Step::Next)
                    }
                    Err(e) if *fail == NO_FAIL => self.raise_bif_error(module, e),
                    // Guard failure: jump to the fail label.
                    Err(_) => {
                        let (func, pc) = resolve(labels, *fail)?;
                        Ok(Step::Goto { func, pc })
                    }
                }
            }
            Instr::Call { target, .. } => {
                // Store the continuation in the *current* frame's CP slot.
                self.set_continuation(Continuation::Address { func, pc: pc + 1 })?;
                let (func, pc) = resolve(labels, *target)?;
                Ok(Step::Goto { func, pc })
            }
            Instr::CallLast {
                target, deallocate, ..
            } => {
                self.pop_frame_checked(*deallocate)?;
                let (func, pc) = resolve(labels, *target)?;
                Ok(Step::Goto { func, pc })
            }
            Instr::CallOnly { target, .. } => {
                let (func, pc) = resolve(labels, *target)?;
                Ok(Step::Goto { func, pc })
            }
            Instr::CallExt { target, arity } => {
                // An ext call carries no fail label: a failure is a raise (C's
                // `i_call_ext`/`call_light_bif` end in `handle_error`, so a live
                // catch frame catches it).
                match self.call_import(module, *target, *arity, ctx) {
                    Ok(value) => {
                        self.set_x(0, value)?;
                        Ok(Step::Next)
                    }
                    Err(e) => self.raise_bif_error(module, e),
                }
            }
            Instr::CallExtOnly { target, arity } => {
                match self.call_import(module, *target, *arity, ctx) {
                    Ok(value) => {
                        self.set_x(0, value)?;
                        match self.continuation()? {
                            Continuation::Halt => Ok(Step::Done),
                            Continuation::Address { func, pc } => Ok(Step::Goto { func, pc }),
                        }
                    }
                    Err(e) => self.raise_bif_error(module, e),
                }
            }
            Instr::CallExtLast {
                target,
                arity,
                deallocate,
            } => {
                self.pop_frame_checked(*deallocate)?;
                match self.call_import(module, *target, *arity, ctx) {
                    Ok(value) => {
                        self.set_x(0, value)?;
                        match self.continuation()? {
                            Continuation::Halt => Ok(Step::Done),
                            Continuation::Address { func, pc } => Ok(Step::Goto { func, pc }),
                        }
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
                Continuation::Address { func, pc } => Ok(Step::Goto { func, pc }),
            },
            // An opcode the loader has no typed IR variant for: a small residual
            // set is executed by [`crate::generic`] (currently only
            // `is_any_native_record`); everything else stays the documented
            // `unsupported(op.name())` typed error.
            Instr::Generic { op, operands, .. } => crate::generic::exec(self, labels, *op, operands),
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

    /// The dispatch loop over a pre-built label map (shared by
    /// [`Machine::run_loop`] and [`Machine::run_label`]).
    ///
    /// A thin driver over [`Machine::step_instruction`]: it executes one
    /// instruction at a time and folds the returned [`Step`] into the running
    /// `(func, pc)`, finishing the run with `x(0)` on [`Step::Done`].
    pub(crate) fn run_loop_with_map(
        &mut self,
        module: &Module,
        labels: &BTreeMap<Label, (u32, u32)>,
        mut func: u32,
        mut pc: u32,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        loop {
            match self.step_instruction(module, labels, func, pc, ctx)? {
                Step::Next => pc += 1,
                Step::Goto { func: f, pc: p } => {
                    func = f;
                    pc = p;
                }
                Step::Done => return self.finish(),
            }
        }
    }

    /// Call an imported BIF with the `arity` register arguments `x(0..arity)`.
    fn call_import(
        &self,
        module: &Module,
        target: ImportRef,
        arity: u32,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        let (module_name, function_name, bif_arity) = import_target(module, target)?;
        let mut args = Vec::with_capacity(arity as usize);
        for i in 0..arity {
            args.push(self.x(i).ok_or_else(|| error::bad_register(i))?);
        }
        ctx.call_bif(module_name, function_name, bif_arity, &args)
    }

    /// Raise a BIF/`call_ext` error on the **no-fail-label** path, landing it in a
    /// live catch frame when there is one.
    ///
    /// Reproduces the C interpreter's error path for a BIF call without a fail
    /// label: `gc_bif`/`bif` fall through to `find_func_info`
    /// (`../../erts/emulator/beam/emu/macros.tab:266-278`) and `call_light_bif*`
    /// runs `handle_error` (`emu/bif_instrs.tab:308-314`, `:409-415`); both end in
    /// `next_catch` (`../../erts/emulator/beam/beam_common.c:580-679`), which finds
    /// the innermost live `try`/`catch` handler and continues there. The engine's
    /// single equivalent entry point is [`Machine::throw_error`].
    ///
    /// * **No live catch frame** ([`Machine::catch_frame_depth`] `== 0`) ⇒ the error
    ///   propagates unchanged.
    /// * **A live catch frame** ⇒ the error's BEAM atom name
    ///   ([`BeamError::as_atom`]: `badarg`/`badarith`/`badarity`/`badmatch`/
    ///   `system_limit`/`out_of_memory`) is resolved in the running module's atom
    ///   table, turned into a reason term and raised with class `error` — the class
    ///   every BIF failure carries (`BADARG`/`BADARITH`/… are `EXTAG_ERROR`
    ///   reasons), which is what the handler's `catch error:Reason` clause sees.
    /// * An error the engine cannot name as a term propagates unchanged instead: a
    ///   [`BeamError::Raise`] (its class/reason are carried term-independently and
    ///   the reason may be [`beam_common::ExitReason::Structured`]), a variant with
    ///   no BEAM atom ([`BeamError::TermNotFound`], [`BeamError::Io`]), or an atom
    ///   the running module's atom table does not hold. Substituting a *different*
    ///   atom would be a wrong result, so the caller's typed error is preferred —
    ///   a documented Milestone-1 limit (the [`Module`] reaches the interpreter
    ///   read-only, so a name the compiler never emitted cannot be interned here,
    ///   unlike the C runtime's global atom table).
    fn raise_bif_error(&mut self, module: &Module, error: BeamError) -> Result<Step> {
        if self.catch_frame_depth() == 0 || matches!(&error, BeamError::Raise(_)) {
            return Err(error);
        }
        let Some(name) = error.as_atom() else {
            return Err(error);
        };
        let Some(atom) = module.atom_table.lookup(name) else {
            return Err(error);
        };
        // The class atom a landing installs in `x(3)`; a module that cannot name
        // `error` leaves it unset and `throw_error` reports its own typed error
        // rather than substituting a wrong atom.
        if let Some(class_atom) = module.atom_table.lookup(ExceptionClass::Error.as_str()) {
            self.set_class_atom(ExceptionClass::Error, make_atom(class_atom));
        }
        self.throw_error(make_atom(atom))
    }

    /// Evaluate one `is_*`/`test_arity` test; `true` means the test *succeeds*.
    fn eval_test(&self, module: &Module, kind: TestKind, args: &[Operand]) -> Result<bool> {
        let atoms = &module.atom_table;
        match kind {
            TestKind::IsLt => {
                let (a, b) = self.test_pair(args)?;
                Ok(cmp_arith(a, b, atoms).is_lt())
            }
            TestKind::IsGe => {
                let (a, b) = self.test_pair(args)?;
                Ok(cmp_arith(a, b, atoms).is_ge())
            }
            TestKind::IsEq => {
                let (a, b) = self.test_pair(args)?;
                Ok(cmp_arith(a, b, atoms).is_eq())
            }
            TestKind::IsNe => {
                let (a, b) = self.test_pair(args)?;
                Ok(!cmp_arith(a, b, atoms).is_eq())
            }
            TestKind::IsEqExact => {
                let (a, b) = self.test_pair(args)?;
                Ok(cmp(a, b, atoms).is_eq())
            }
            TestKind::IsNeExact => {
                let (a, b) = self.test_pair(args)?;
                Ok(!cmp(a, b, atoms).is_eq())
            }
            TestKind::IsInteger => Ok(predicates::is_integer(self.test_arg(args, 0)?)),
            TestKind::IsFloat => Ok(predicates::is_float(self.test_arg(args, 0)?)),
            TestKind::IsNumber => Ok(predicates::is_number(self.test_arg(args, 0)?)),
            TestKind::IsAtom => Ok(is_atom(self.test_arg(args, 0)?)),
            TestKind::IsPid => {
                let v = self.test_arg(args, 0)?;
                Ok(is_internal_pid(v) || predicates::is_external_pid(v))
            }
            TestKind::IsReference => {
                let v = self.test_arg(args, 0)?;
                Ok(predicates::is_internal_ref(v) || predicates::is_external_ref(v))
            }
            TestKind::IsPort => {
                let v = self.test_arg(args, 0)?;
                Ok(is_internal_port(v) || predicates::is_external_port(v))
            }
            TestKind::IsNil => Ok(is_nil(self.test_arg(args, 0)?)),
            TestKind::IsBinary => Ok(predicates::is_binary(self.test_arg(args, 0)?)),
            TestKind::IsBitstr => Ok(predicates::is_bitstring(self.test_arg(args, 0)?)),
            TestKind::IsList => {
                // `is_list` is true for `[]` *or* a cons cell.
                let v = self.test_arg(args, 0)?;
                Ok(is_nil(v) || is_not_nil_list(v))
            }
            TestKind::IsNonemptyList => Ok(is_not_nil_list(self.test_arg(args, 0)?)),
            TestKind::IsTuple => Ok(predicates::is_tuple(self.test_arg(args, 0)?)),
            TestKind::IsMap => Ok(predicates::is_map(self.test_arg(args, 0)?)),
            TestKind::IsBoolean => {
                // `is_boolean Fail Src` (`emu/instrs.tab:933`): `Src` is the atom
                // `true` or `false`. Both are ordinary atoms, so compare against the
                // module's atom table; a name absent from the table can never be the
                // value, so a missing name is `false` (not an error).
                let v = self.test_arg(args, 0)?;
                if !is_atom(v) {
                    return Ok(false);
                }
                let is_true = atoms
                    .lookup("true")
                    .map(|a| v == make_atom(a))
                    .unwrap_or(false);
                let is_false = atoms
                    .lookup("false")
                    .map(|a| v == make_atom(a))
                    .unwrap_or(false);
                Ok(is_true || is_false)
            }
            // `is_function Fail Fun` and `is_function2 Fail Fun Arity` — reuse the
            // `terms` fun readers (`is_any_fun` / the fun-header arity field).
            TestKind::IsFunction => Ok(crate::terms::is_fun(self.test_arg(args, 0)?)),
            TestKind::IsFunction2 => self.eval_is_function2(args),
            TestKind::IsTaggedTuple => self.eval_is_tagged_tuple(args),
            TestKind::TestArity => self.eval_test_arity(args),
        }
    }

    /// `is_function2 Fail Fun Arity` — `Fun` is a fun of the given arity.
    ///
    /// Operand order is the file's (`emu/ops.tab:831`, verified against the oracle:
    /// `{test,is_function2,{f,N},[Fun,{integer,Arity}]}`), i.e. `args[0]` = `Fun`,
    /// `args[1]` = `Arity` (an integer operand).
    fn eval_is_function2(&self, args: &[Operand]) -> Result<bool> {
        let value = self.test_arg(args, 0)?;
        if !crate::terms::is_fun(value) {
            return Ok(false);
        }
        match args.get(1) {
            Some(Operand::Integer(arity)) if *arity >= 0 && *arity <= u32::MAX as i64 => {
                Ok(crate::terms::fun_arity(value)? == *arity as u32)
            }
            _ => Err(error::unsupported("is_function2: arity operand")),
        }
    }

    /// `is_tagged_tuple Fail Src Arity Tag` — `Src` is a tuple of `Arity` whose first
    /// element is `Tag` (the compiler's record test).
    ///
    /// Operand order is the file's (`emu/ops.tab:664`; verified against the oracle:
    /// `{test,is_tagged_tuple,{f,N},[{x,0},3,{atom,r}]}`), i.e. `args[0]` = `Src`,
    /// `args[1]` = `Arity` (an integer), `args[2]` = `Tag` (an atom) — the same layout
    /// `beam_loader::TestKind::IsTaggedTuple` documents.
    fn eval_is_tagged_tuple(&self, args: &[Operand]) -> Result<bool> {
        let value = self.test_arg(args, 0)?;
        let arity = match args.get(1) {
            Some(Operand::Integer(arity)) => *arity,
            _ => return Err(error::unsupported("is_tagged_tuple: arity operand")),
        };
        // A tagged tuple has at least the tag element and its arity always fits
        // `MAX_ARITYVAL`; any other operand can never match, so the test fails.
        if !(1..=MAX_ARITYVAL as i64).contains(&arity) {
            return Ok(false);
        }
        if !predicates::is_tuple_arity(value, arity as usize) {
            return Ok(false);
        }
        let tag = self.test_arg(args, 2)?;
        // The first element of a tagged tuple is the (immediate) tag atom.
        Ok(crate::terms::tuple_element(value, 0)? == tag)
    }

    /// `test_arity Value Integer` — the value is a tuple of that arity.
    fn eval_test_arity(&self, args: &[Operand]) -> Result<bool> {
        let value = self.test_arg(args, 0)?;
        match args.get(1) {
            Some(Operand::Integer(arity)) => {
                if *arity < 0 || *arity > MAX_ARITYVAL as i64 {
                    // No real tuple has a negative or out-of-range arity.
                    return Ok(false);
                }
                Ok(predicates::is_tuple_arity(value, *arity as usize))
            }
            _ => Err(error::unsupported("test_arity: arity operand")),
        }
    }

    /// Read test argument `i`, or a typed error if the instruction has too few.
    fn test_arg(&self, args: &[Operand], i: usize) -> Result<Eterm> {
        match args.get(i) {
            Some(operand) => read_operand(self, operand),
            None => Err(error::unsupported("test: missing argument")),
        }
    }

    /// Read the two arguments of a comparison test.
    fn test_pair(&self, args: &[Operand]) -> Result<(Eterm, Eterm)> {
        Ok((self.test_arg(args, 0)?, self.test_arg(args, 1)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MinimalContext;
    use beam_loader::{Function, Import};
    use beam_term::{make_small, Atom};

    /// A one-function module whose single entry label is `entry`.
    fn module_with_function(code: Vec<Instr>, imports: Vec<Import>) -> (Module, Atom) {
        let mut module = Module::default();
        module.imports = imports;
        let name = module.atom_table.intern("");
        let function = Function {
            func_label: Label(1),
            entry_label: Label(2),
            module: Atom::MAX,
            name,
            arity: 0,
            labels: code
                .iter()
                .enumerate()
                .filter_map(|(i, ins)| match ins {
                    Instr::Label(l) => Some((*l, i)),
                    _ => None,
                })
                .collect(),
            blocks: Vec::new(),
            code,
        };
        module.functions = vec![function];
        (module, name)
    }

    #[test]
    fn build_label_map_collects_every_function() {
        let (module, _) =
            module_with_function(vec![Instr::Label(Label(1)), Instr::Return], Vec::new());
        let map = build_label_map(&module);
        assert_eq!(map.get(&Label(1)), Some(&(0, 0)));
    }

    #[test]
    fn resolve_label_of_a_missing_label_is_a_typed_error() {
        let (module, _) = module_with_function(vec![Instr::Return], Vec::new());
        assert!(resolve_label(&module, Label(99)).is_err());
    }

    #[test]
    fn step_instruction_advances_exactly_one_instruction() {
        // `[Label, move x0 <- 5, return]`.
        let (module, _) = module_with_function(
            vec![
                Instr::Label(Label(1)),
                Instr::Move {
                    src: Operand::Integer(5),
                    dst: Operand::X(0),
                },
                Instr::Return,
            ],
            Vec::new(),
        );
        let labels = build_label_map(&module);
        let mut machine = Machine::new();
        // A bottom frame whose continuation is `Halt`, so the trailing `return`
        // finishes the run rather than looking for a caller's continuation.
        machine.push_frame(0).unwrap();
        let mut ctx = MinimalContext::new();

        // `pc = 1` is the `move`: exactly one instruction, advancing to `pc + 1`.
        let step = machine
            .step_instruction(&module, &labels, 0, 1, &mut ctx)
            .unwrap();
        assert_eq!(step, Step::Next);
        assert_eq!(machine.x(0), Some(make_small(5)));
        // The move did not run the `return`: the frame is still live.
        assert_eq!(machine.frame_depth(), 1);

        // `pc = 2` is the `return` reaching the `Halt` continuation: the run ends.
        let step = machine
            .step_instruction(&module, &labels, 0, 2, &mut ctx)
            .unwrap();
        assert_eq!(step, Step::Done);
    }

    #[test]
    fn step_instruction_reports_a_jump_target() {
        // `[Label(1), jump Label(2), Label(2), return]`.
        let (module, _) = module_with_function(
            vec![
                Instr::Label(Label(1)),
                Instr::Jump { target: Label(2) },
                Instr::Label(Label(2)),
                Instr::Return,
            ],
            Vec::new(),
        );
        let labels = build_label_map(&module);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();

        // The `jump` at `pc = 1` transfers control to `Label(2)`, which is code
        // offset 2 of the same (only) function.
        let step = machine
            .step_instruction(&module, &labels, 0, 1, &mut ctx)
            .unwrap();
        assert_eq!(step, Step::Goto { func: 0, pc: 2 });
    }

    #[test]
    fn step_instruction_out_of_range_is_a_typed_error() {
        let (module, _) = module_with_function(vec![Instr::Return], Vec::new());
        let labels = build_label_map(&module);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();

        // An out-of-range `pc` / function index stays an `Err`, never a panic.
        assert!(machine
            .step_instruction(&module, &labels, 0, 99, &mut ctx)
            .is_err());
        assert!(machine
            .step_instruction(&module, &labels, 99, 0, &mut ctx)
            .is_err());
    }
}
