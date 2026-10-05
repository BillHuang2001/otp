//! The direct-threaded dispatch loop (`impl Machine`).
//!
//! One `match` arm per Milestone-1 [`beam_loader::Instr`] variant implements the
//! interpreter model of Part II §F without `unsafe`/computed-goto (the IR is
//! pre-decoded, so typed dispatch is equivalent to a jump table). The
//! instruction bodies reproduce the C interpreter exactly: the frame/continuation
//! conventions of `../../erts/emulator/beam/emu/macros.tab` and the
//! `gc_bif`/common-BIF semantics of `../../erts/emulator/beam/emu/beam_emu.c`.
//!
//! Failure is return-based: a `GcBif` error with a live fail label branches
//! (guard failure), an error without one propagates out of the run as
//! `Err(BeamError)` (a raise). Every unsupported opcode/operand is a documented
//! typed error, never a panic.

use std::collections::BTreeMap;

use beam_common::Result;
use beam_loader::{ImportRef, Instr, Label, Module, Operand, TestKind, NO_FAIL};
use beam_term::{
    cmp, cmp_arith, is_atom, is_internal_pid, is_internal_port, is_nil, is_not_nil_list, Eterm,
    MAX_ARITYVAL,
};

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::{Continuation, Machine};
use crate::operand::{read_operand, write_operand};
use crate::predicates;

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
fn resolve(labels: &BTreeMap<Label, (u32, u32)>, label: Label) -> Result<(u32, u32)> {
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
        let labels = build_label_map(module);
        self.run_loop_with_map(module, &labels, func, pc, ctx)
    }

    /// The dispatch loop over a pre-built label map (shared by
    /// [`Machine::run_loop`] and [`Machine::run_label`]).
    pub(crate) fn run_loop_with_map(
        &mut self,
        module: &Module,
        labels: &BTreeMap<Label, (u32, u32)>,
        mut func: u32,
        mut pc: u32,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        loop {
            // The instruction borrows `module` (never `self`), so mutating the
            // machine in the arms below is sound and clone-free.
            let instr = match module.functions.get(func as usize) {
                Some(function) => match function.code.get(pc as usize) {
                    Some(instr) => instr,
                    None => return Err(error::unsupported("run: pc out of range")),
                },
                None => return Err(error::unsupported("run: function index out of range")),
            };

            match instr {
                // Metadata / no-ops.
                Instr::Label(_) | Instr::Line(_) | Instr::FuncInfo { .. } | Instr::IntCodeEnd => {
                    pc += 1;
                }
                Instr::Move { src, dst } => {
                    let value = read_operand(self, src)?;
                    write_operand(self, dst, value)?;
                    pc += 1;
                }
                Instr::Swap { a, b } => {
                    let va = read_operand(self, a)?;
                    let vb = read_operand(self, b)?;
                    write_operand(self, a, vb)?;
                    write_operand(self, b, va)?;
                    pc += 1;
                }
                Instr::Test { kind, fail, args } => {
                    if self.eval_test(module, *kind, args)? {
                        pc += 1;
                    } else if *fail == NO_FAIL {
                        return Err(error::unsupported("test/no_fail"));
                    } else {
                        let (next_func, next_pc) = resolve(labels, *fail)?;
                        func = next_func;
                        pc = next_pc;
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
                            let (next_func, next_pc) = resolve(labels, label)?;
                            func = next_func;
                            pc = next_pc;
                        }
                        None if *fail == NO_FAIL => {
                            return Err(error::unsupported("select_val/no_fail"));
                        }
                        None => {
                            let (next_func, next_pc) = resolve(labels, *fail)?;
                            func = next_func;
                            pc = next_pc;
                        }
                    }
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
                            pc += 1;
                        }
                        Err(e) if *fail == NO_FAIL => return Err(e),
                        Err(_) => {
                            let (next_func, next_pc) = resolve(labels, *fail)?;
                            func = next_func;
                            pc = next_pc;
                        }
                    }
                }
                Instr::Call { target, .. } => {
                    // Store the continuation in the *current* frame's CP slot.
                    self.set_continuation(Continuation::Address { func, pc: pc + 1 })?;
                    let (next_func, next_pc) = resolve(labels, *target)?;
                    func = next_func;
                    pc = next_pc;
                }
                Instr::CallLast {
                    target, deallocate, ..
                } => {
                    self.pop_frame_checked(*deallocate)?;
                    let (next_func, next_pc) = resolve(labels, *target)?;
                    func = next_func;
                    pc = next_pc;
                }
                Instr::CallOnly { target, .. } => {
                    let (next_func, next_pc) = resolve(labels, *target)?;
                    func = next_func;
                    pc = next_pc;
                }
                Instr::CallExt { target, arity } => {
                    let value = self.call_import(module, *target, *arity, ctx)?;
                    self.set_x(0, value)?;
                    pc += 1;
                }
                Instr::CallExtOnly { target, arity } => {
                    let value = self.call_import(module, *target, *arity, ctx)?;
                    self.set_x(0, value)?;
                    match self.continuation()? {
                        Continuation::Halt => return self.finish(),
                        Continuation::Address {
                            func: next_func,
                            pc: next_pc,
                        } => {
                            func = next_func;
                            pc = next_pc;
                        }
                    }
                }
                Instr::CallExtLast {
                    target,
                    arity,
                    deallocate,
                } => {
                    self.pop_frame_checked(*deallocate)?;
                    let value = self.call_import(module, *target, *arity, ctx)?;
                    self.set_x(0, value)?;
                    match self.continuation()? {
                        Continuation::Halt => return self.finish(),
                        Continuation::Address {
                            func: next_func,
                            pc: next_pc,
                        } => {
                            func = next_func;
                            pc = next_pc;
                        }
                    }
                }
                Instr::Allocate { stack_need, .. } => {
                    self.push_frame(*stack_need)?;
                    pc += 1;
                }
                Instr::AllocateHeap {
                    stack_need,
                    heap_need,
                    live,
                } => {
                    self.ensure_heap(*heap_need, *live, ctx)?;
                    self.push_frame(*stack_need)?;
                    pc += 1;
                }
                Instr::TestHeap { heap_need, live } => {
                    self.ensure_heap(*heap_need, *live, ctx)?;
                    pc += 1;
                }
                Instr::Deallocate(n) => {
                    self.pop_frame_checked(*n)?;
                    pc += 1;
                }
                Instr::Trim { n, .. } => {
                    self.trim(*n)?;
                    pc += 1;
                }
                Instr::Return => match self.continuation()? {
                    Continuation::Halt => return self.finish(),
                    Continuation::Address {
                        func: next_func,
                        pc: next_pc,
                    } => {
                        func = next_func;
                        pc = next_pc;
                    }
                },
                Instr::Generic { op, .. } => return Err(error::unsupported(op.name())),
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
            TestKind::TestArity => self.eval_test_arity(args),
        }
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
    use beam_loader::{Function, Import};
    use beam_term::Atom;

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
}
