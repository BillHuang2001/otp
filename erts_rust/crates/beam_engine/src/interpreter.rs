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

use beam_common::{BeamError, ExceptionClass, Result};
use beam_loader::{ImportRef, Instr, Label, Module, Operand, TestKind, NO_FAIL};
use beam_term::{
    cmp, cmp_arith, is_atom, is_internal_pid, is_internal_port, is_nil, is_not_nil_list, make_atom,
    Eterm, MAX_ARITYVAL, NIL,
};

use crate::context::ExecutionContext;
use crate::error;
use crate::families;
use crate::machine::{Continuation, Machine, RunOutcome};
use crate::operand::{read_operand, write_operand};
use crate::predicates;
use crate::Step;

/// The module-wide `Label -> (function index, code index)` index, plus the
/// per-run import cache.
///
/// A `.beam` module numbers its labels contiguously from `1` (`beam_asm`
/// renumbers them; `0` is the `NO_FAIL` sentinel), so a label id is a dense
/// index and the primary form is a flat `Vec<Option<(u32, u32)>>` addressed by
/// `label.0` — one bounds-checked load, no tree/pointer walk. `resolve` is on the
/// hottest path (every branch/call/test-fail/select), so it is `#[inline]`.
///
/// A module whose ids are *not* dense (a hand-crafted or corrupt file, where one
/// `label 4000000000` would otherwise reserve tens of gigabytes) uses the sorted
/// fallback instead; both forms answer identically.
///
/// The same index also carries the **pre-resolved import table** (`imports`), so
/// the per-run table is threaded to the dispatch loop as the map the loop already
/// takes, with no extra parameter and no lifetime on the map (see
/// [`ImportTarget`]). A map that was *not* built by [`build_label_map`]
/// ([`LabelMap::new`], `From`, a hand-built test map) carries an empty table and a
/// zero `imports_owner`, so every import lookup falls back to [`import_target`] —
/// the same answer, exactly the same typed errors.
pub(crate) struct LabelMap {
    slots: Slots,
    /// `imports[ImportRef.0]`: the resolved `(module, function, arity)` of each
    /// `Module::imports` entry (see [`ImportTarget`] and [`resolve_imports`]).
    imports: ImportTable,
    /// The address of the [`Module`] `imports` was resolved from; `0` (an address
    /// no live `Module` can occupy) marks the table as unusable, so the lookup
    /// falls back to the per-call [`import_target`] and can never feed a name from
    /// a different module.
    imports_owner: usize,
}

/// One `Module::imports` entry, resolved **once per run** into the
/// `(module name, function name, arity)` triple
/// [`ExecutionContext::call_bif`](crate::ExecutionContext::call_bif) takes.
///
/// Resolving one import costs two atom-table lookups, and the interpreter used to
/// repeat them for *every* `gc_bif`/`bif`/`call_ext*` — the hottest instruction
/// class (measured: ~5 ns per BIF call, against ~0.5 ns for a cache hit). The two
/// names are stored **owned** rather than borrowed from the running [`Module`]:
/// the map is also reached through a raw pointer inside the baseline JIT's
/// [`JitRuntime`](crate::jit::baseline), which is a non-generic `#[repr(C)]` type,
/// so the map cannot carry the module's lifetime. Copying the two names once per
/// run is the whole price of the cache.
struct ImportTarget {
    /// The imported module's name (the string `call_bif` dispatches on).
    module: Box<str>,
    /// The imported function's name.
    function: Box<str>,
    /// The import's declared arity (what `call_bif` is called with).
    arity: u32,
}

/// The per-run import table: `table[ImportRef.0]` is the resolved target, or the
/// reason string of the typed error that resolving that import produced (an atom
/// the module's atom table does not hold — see [`import_target`]). An index past
/// the end is the typed `call: import out of range`, exactly as before.
type ImportTable = Vec<std::result::Result<ImportTarget, &'static str>>;

/// The two backing representations of a [`LabelMap`].
enum Slots {
    /// `slots[label.0]` over ids `0..=max`; `slots[0]` is the unused `NO_FAIL`
    /// sentinel and stays `None`.
    Dense(Vec<Option<(u32, u32)>>),
    /// `(label id, target)` sorted by id, for a sparse id space.
    Sparse(Vec<(u32, (u32, u32))>),
}

impl LabelMap {
    /// An empty index: every lookup is the typed [`crate::error::unknown_label`],
    /// and every import falls back to the uncached [`import_target`].
    pub(crate) fn new() -> LabelMap {
        LabelMap {
            slots: Slots::Dense(Vec::new()),
            imports: Vec::new(),
            imports_owner: 0,
        }
    }
    /// Record `label -> (function index, code index)`, an existing entry for the
    /// same label being replaced (labels are module-unique, so this only matters
    /// for a malformed/duplicate table — and it matches the previous last-wins
    /// `BTreeMap::insert`).
    pub(crate) fn insert(&mut self, label: Label, target: (u32, u32)) {
        match &mut self.slots {
            Slots::Dense(slots) => {
                let index = label.0 as usize;
                if index >= slots.len() {
                    slots.resize(index + 1, None);
                }
                slots[index] = Some(target);
            }
            Slots::Sparse(entries) => match entries.binary_search_by_key(&label.0, |(id, _)| *id) {
                Ok(found) => match entries.get_mut(found) {
                    Some(entry) => entry.1 = target,
                    None => {}
                },
                Err(at) => entries.insert(at, (label.0, target)),
            },
        }
    }

    /// Resolve `label` to its `(function index, code index)`; a label the module
    /// does not define is the typed [`crate::error::unknown_label`].
    #[inline]
    pub(crate) fn resolve(&self, label: Label) -> Result<(u32, u32)> {
        match &self.slots {
            Slots::Dense(slots) => match slots.get(label.0 as usize) {
                Some(Some(target)) => Ok(*target),
                _ => Err(error::unknown_label(label)),
            },
            Slots::Sparse(entries) => {
                match entries.binary_search_by_key(&label.0, |(id, _)| *id) {
                    Ok(found) => entries
                        .get(found)
                        .map(|(_, target)| *target)
                        .ok_or_else(|| error::unknown_label(label)),
                    Err(_) => Err(error::unknown_label(label)),
                }
            }
        }
    }

    /// Resolve an import reference to `(module name, function name, arity)`.
    ///
    /// The hot path of every `gc_bif`/`bif`/`call_ext*`: a **cache hit** is one
    /// address check, one bounds check and three loads from the per-run import
    /// table ([`ImportTarget`]) instead of the two atom-table lookups
    /// [`import_target`] performs per call.
    ///
    /// Nothing about the *semantics* changes: the table only answers when it was
    /// built from **this** module (`imports_owner`), and a table miss — an index
    /// past the end (`call: import out of range`), or an import whose module /
    /// function atom is not in the module's table (`call: import module atom` /
    /// `call: import function atom`) — is the exact typed error
    /// [`import_target`] produces for the same import. A map this cache does not
    /// cover (not built from this module: [`LabelMap::new`], `From`, ...) falls
    /// straight through to [`import_target`], so the two can never disagree.
    #[inline]
    fn import_target<'a>(
        &'a self,
        module: &'a Module,
        target: ImportRef,
    ) -> Result<(&'a str, &'a str, u32)> {
        if self.imports_owner == module as *const Module as usize {
            match self.imports.get(target.0 as usize) {
                Some(Ok(entry)) => {
                    return Ok((&entry.module, &entry.function, entry.arity));
                }
                Some(Err(reason)) => return Err(error::unsupported(reason)),
                None => return Err(error::unsupported("call: import out of range")),
            }
        }
        import_target(module, target)
    }
}
impl Default for LabelMap {
    fn default() -> LabelMap {
        LabelMap::new()
    }
}

impl<const N: usize> From<[(Label, (u32, u32)); N]> for LabelMap {
    fn from(pairs: [(Label, (u32, u32)); N]) -> LabelMap {
        let mut map = LabelMap::new();
        for (label, target) in pairs {
            map.insert(label, target);
        }
        map
    }
}

// A test-only counter of `build_label_map` calls on the current thread.
//
// A run must build the module-wide label index **exactly once** (it used to build
// it twice — once to resolve the entry label and once inside the dispatch loop),
// and a focused unit test asserts that. Compiled only under `cfg(test)`, so it is
// absent from production builds and from the `tests/` integration suites (which
// link the non-test library) and cannot perturb their timing.
#[cfg(test)]
thread_local! {
    static LABEL_MAP_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Test-only: reset the current thread's [`build_label_map`] call counter.
#[cfg(test)]
pub(crate) fn reset_label_map_builds() {
    LABEL_MAP_BUILDS.with(|c| c.set(0));
}

/// Test-only: the current thread's [`build_label_map`] call count.
#[cfg(test)]
pub(crate) fn label_map_builds() -> usize {
    LABEL_MAP_BUILDS.with(|c| c.get())
}

/// Build the module-wide [`LabelMap`].
///
/// Labels are module-unique code addresses, so a call/fail target may live in a
/// different function than the instruction that references it (the slice's
/// `fib/1` tail-calls into `fib/3`). Every function's [`beam_loader::Function::labels`]
/// contributes to the one index.
///
/// The same call also resolves the module's **import table** once
/// ([`resolve_imports`]) and binds the map to `module`, so the map is the run's
/// complete name-resolution side table: `resolve` answers labels and
/// [`LabelMap::import_target`] answers BIF/`call_ext` targets without touching the
/// atom table per call. One build per run, exactly as before.
pub(crate) fn build_label_map(module: &Module) -> LabelMap {
    #[cfg(test)]
    LABEL_MAP_BUILDS.with(|c| c.set(c.get() + 1));
    // One pass to size the index: a real `.beam` numbers its labels `1..=count`,
    // so `max + 1 == count + 1` and the dense table is exactly as large as the
    // label set (verified across the installed OTP: density is exactly 1.0).
    let mut count = 0usize;
    let mut max = 0u32;
    for function in &module.functions {
        count += function.labels.len();
        for label in function.labels.keys() {
            if label.0 > max {
                max = label.0;
            }
        }
    }
    if max as u64 <= (count as u64).saturating_mul(4) + 64 {
        let mut slots: Vec<Option<(u32, u32)>> = vec![None; max as usize + 1];
        for (func_index, function) in module.functions.iter().enumerate() {
            for (label, &code_index) in &function.labels {
                slots[label.0 as usize] = Some((func_index as u32, code_index as u32));
            }
        }
        LabelMap {
            slots: Slots::Dense(slots),
            imports: resolve_imports(module),
            imports_owner: module as *const Module as usize,
        }
    } else {
        let mut entries: Vec<(u32, (u32, u32))> = Vec::with_capacity(count);
        for (func_index, function) in module.functions.iter().enumerate() {
            for (label, &code_index) in &function.labels {
                entries.push((label.0, (func_index as u32, code_index as u32)));
            }
        }
        entries.sort_unstable_by_key(|(id, _)| *id);
        LabelMap {
            slots: Slots::Sparse(entries),
            imports: resolve_imports(module),
            imports_owner: module as *const Module as usize,
        }
    }
}

/// Resolve **every** import of `module` once, producing the per-run import table
/// [`LabelMap::import_target`] looks up.
///
/// Each entry is either the resolved [`ImportTarget`] or the reason string of the
/// typed error resolution produced, so the cached path reproduces
/// [`import_target`]'s errors exactly (a missing module/function atom is
/// remembered per import, never substituted by a different name).
fn resolve_imports(module: &Module) -> ImportTable {
    module
        .imports
        .iter()
        .map(|import| {
            let module_name = module
                .atom_name(import.module)
                .ok_or("call: import module atom")?;
            let function_name = module
                .atom_name(import.function)
                .ok_or("call: import function atom")?;
            Ok(ImportTarget {
                module: module_name.into(),
                function: function_name.into(),
                arity: import.arity,
            })
        })
        .collect()
}

/// Resolve a label against an already-built index; a missing label is a typed error.
pub(crate) fn resolve(labels: &LabelMap, label: Label) -> Result<(u32, u32)> {
    labels.resolve(label)
}

/// Resolve a module-local label to its `(function index, code offset)`.
pub(crate) fn resolve_label(module: &Module, label: Label) -> Result<(u32, u32)> {
    build_label_map(module).resolve(label)
}

/// Resolve an import reference to `(module name, function name, arity)` — **the
/// uncached fallback**.
///
/// This is the per-call resolution the interpreter used to run for every
/// `gc_bif`/`bif`/`call_ext*` (two atom-table lookups and the `Vec` bounds check).
/// [`LabelMap::import_target`] answers from the per-run cache instead; this
/// function stays the definition of the import semantics and the path taken when
/// the cache cannot answer (a map not built from this module), so both agree
/// exactly — including on the three typed errors below.
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
/// The argument count up to which a BIF/`call_ext` marshals its arguments through
/// a **stack array** instead of a heap `Vec`.
///
/// `gc_bif`/`bif`/`call_ext` used to build a fresh `Vec` (one heap allocation and
/// free) for **every** BIF call, which a micro-benchmark showed to be the single
/// largest per-instruction cost of the interpreter. arities in `0..=STACK_ARGS`
/// (every arity the compiler actually emits for arithmetic/guard BIFs) now fill a
/// fixed stack array; a larger arity still works through the `Vec` fallback, so
/// the observable behaviour and the `ctx.call_bif(.., &[Eterm])` signature are
/// unchanged.
const STACK_ARGS: usize = 8;

/// A stack array sized for the common BIF/`call_ext` arities ([`STACK_ARGS`]).
const EMPTY_ARGS: [Eterm; STACK_ARGS] = [NIL; STACK_ARGS];

impl Machine {
    /// The direct-threaded dispatch loop over the Milestone-1 opcode subset.
    ///
    /// Entry point: function index `func` (into [`Module::functions`]) and code
    /// offset `pc`; runs until the function's `return` (yielding `x(0)` as
    /// [`RunOutcome::Returned`]) or until a `receive` suspends the process on an
    /// empty mailbox (yielding [`RunOutcome::Wait`], with the registers, the
    /// Erlang stack and every call frame left exactly as the `wait` instruction
    /// found them).
    ///
    /// It does **not** reset the machine: it only (re)binds the running module's
    /// literal area, records the run's entry call and builds the per-run name
    /// index ([`Machine::enter_run`], [`build_label_map`] — labels *and* the
    /// resolved import table), so [`Machine::resume_outcome`] can re-enter a
    /// suspended process through it unchanged.
    pub(crate) fn run_loop(
        &mut self,
        module: &Module,
        func: u32,
        pc: u32,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<RunOutcome> {
        self.enter_run(module, func);
        let labels = build_label_map(module);
        self.run_loop_with_map(module, &labels, func, pc, ctx)
    }

    /// The per-run prologue shared by [`Machine::run_loop`] and
    /// [`Machine::run_label_outcome`]: bind the running module's literal area so
    /// the operand seam can resolve `Operand::Literal(idx)`, and record the run's
    /// entry `(func, arity)` — **once** per run.
    ///
    /// Both effects are idempotent in the way a resume needs: the module is fixed
    /// for the whole run (see the `Machine::literals` field docs), and an entry
    /// call is recorded only when this run has none yet (a `resume_outcome`
    /// re-enters the same run without resetting, so the process's initial call
    /// stays the one it entered at). A `func` outside the module records nothing
    /// rather than a bogus entry.
    pub(crate) fn enter_run(&mut self, module: &Module, func: u32) {
        // `reset()` cleared any previous literal binding.
        self.set_literals(module.literals.clone());
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
                // Store the continuation in the *current* frame's CP slot.
                self.set_continuation(Continuation::Address { func, pc: pc + 1 })?;
                let (func, pc) = resolve(labels, *target)?;
                Ok(Step::Goto { func, pc })
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
                let (func, pc) = resolve(labels, *target)?;
                self.pop_frame_checked(*deallocate)?;
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
                match self.call_import(module, labels, *target, *arity, ctx) {
                    Ok(value) => {
                        self.set_x(0, value)?;
                        Ok(Step::Next)
                    }
                    Err(e) => self.raise_bif_error(module, e),
                }
            }
            Instr::CallExtOnly { target, arity } => {
                match self.call_import(module, labels, *target, *arity, ctx) {
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
                // `call_ext_last` deallocates **after** the callee, never before.
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
                match self.call_import(module, labels, *target, *arity, ctx) {
                    Ok(value) => {
                        self.set_x(0, value)?;
                        self.pop_frame_checked(*deallocate)?;
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

    /// The dispatch loop over a pre-built label map (shared by
    /// [`Machine::run_loop`] and [`Machine::resume_outcome`]).
    ///
    /// A thin driver over [`Machine::step_instruction`]: it executes one
    /// instruction at a time and folds the returned [`Step`] into the running
    /// `(func, pc)`, finishing the run with `x(0)` on [`Step::Done`] and reporting a
    /// suspension as [`RunOutcome::Wait`] on [`Step::Wait`] (leaving the machine
    /// untouched so the host can re-enter at the reported address).
    pub(crate) fn run_loop_with_map(
        &mut self,
        module: &Module,
        labels: &LabelMap,
        mut func: u32,
        mut pc: u32,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<RunOutcome> {
        loop {
            match self.step_instruction(module, labels, func, pc, ctx)? {
                Step::Next => pc += 1,
                Step::Goto { func: f, pc: p } => {
                    func = f;
                    pc = p;
                }
                Step::Wait { func: f, pc: p } => return Ok(RunOutcome::Wait { func: f, pc: p }),
                Step::Done => return self.finish().map(RunOutcome::Returned),
            }
        }
    }

    /// Read a `gc_bif`/`bif`/`bif0`'s **operand** arguments, call the BIF through
    /// [`ExecutionContext::call_bif`] and fold the outcome into the control-flow
    /// [`Step`] the dispatch loop expects.
    ///
    /// The arguments are marshalled into a **stack array** for the common arities
    /// ([`STACK_ARGS`]) and into a heap `Vec` only beyond it, so the heap
    /// allocation the old code paid on **every** BIF call is gone; `call_bif` keeps
    /// its `&[Eterm]` shape either way. The BIF's `(module, function, arity)`
    /// target comes from the per-run import cache ([`LabelMap::import_target`]),
    /// which replaced the two per-call atom-table lookups the interpreter used to
    /// pay here (measured: ~5 ns of the ~20 ns a small-int `gc_bif '+'/2` costs).
    ///
    /// The control flow is unchanged, and the distinction matters: an error while
    /// *marshalling* the call (a bad import reference or a bad operand) propagates
    /// as the typed error the instruction already returned (the original `?`),
    /// **never** as a guard failure — only a **BIF** error raises
    /// ([`Machine::raise_bif_error`], which is the whole story when `fail == NO_FAIL`,
    /// i.e. for every `bif0`) or branches to `fail`.
    fn exec_bif(
        &mut self,
        module: &Module,
        labels: &LabelMap,
        bif: ImportRef,
        args: &[Operand],
        fail: Label,
        dst: &Operand,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Step> {
        let (module_name, function_name, arity) = labels.import_target(module, bif)?;
        let mut buf = EMPTY_ARGS;
        let mut heap: Vec<Eterm> = Vec::new();
        let arg_terms: &[Eterm] = if args.len() <= STACK_ARGS {
            for (slot, arg) in buf.iter_mut().zip(args) {
                *slot = read_operand(self, ctx, arg)?;
            }
            &buf[..args.len()]
        } else {
            heap.reserve_exact(args.len());
            for arg in args {
                heap.push(read_operand(self, ctx, arg)?);
            }
            &heap
        };
        match ctx.call_bif(module_name, function_name, arity, arg_terms) {
            Ok(value) => {
                write_operand(self, dst, value)?;
                Ok(Step::Next)
            }
            // No fail label ⇒ a *raise*: land in a live catch frame (C falls
            // through to `find_func_info`/`handle_error`) or propagate the typed
            // error.
            Err(e) if fail == NO_FAIL => self.raise_bif_error(module, e),
            // Guard failure: jump to the fail label.
            Err(_) => {
                let (func, pc) = resolve(labels, fail)?;
                Ok(Step::Goto { func, pc })
            }
        }
    }

    /// Call an imported BIF with the `arity` register arguments `x(0..arity)`.
    ///
    /// Like [`Machine::exec_bif`] the arguments are marshalled through a stack
    /// array for the common arities and a `Vec` only beyond it, so a `call_ext`
    /// pays no heap allocation in the common case, and the import target comes from
    /// the same per-run cache ([`LabelMap::import_target`], `labels`). A
    /// marshalling error (a bad import or an out-of-range register) propagates as
    /// the typed error the instruction already returned; the BIF's own result is
    /// handed back for the caller's raise/fail decision.
    fn call_import(
        &self,
        module: &Module,
        labels: &LabelMap,
        target: ImportRef,
        arity: u32,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        let (module_name, function_name, bif_arity) = labels.import_target(module, target)?;
        if arity as usize <= STACK_ARGS {
            let mut buf = EMPTY_ARGS;
            for (i, slot) in buf.iter_mut().enumerate().take(arity as usize) {
                *slot = self
                    .x(i as u32)
                    .ok_or_else(|| error::bad_register(i as u32))?;
            }
            ctx.call_bif(
                module_name,
                function_name,
                bif_arity,
                &buf[..arity as usize],
            )
        } else {
            let mut buf = Vec::with_capacity(arity as usize);
            for i in 0..arity {
                buf.push(self.x(i).ok_or_else(|| error::bad_register(i))?);
            }
            ctx.call_bif(module_name, function_name, bif_arity, &buf)
        }
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
    ///   propagates unchanged (unless it carries a live reason term — see below).
    /// * **A live catch frame** ⇒ the error's BEAM atom name
    ///   ([`BeamError::as_atom`]: `badarg`/`badarith`/`badarity`/`badmatch`/
    ///   `system_limit`/`out_of_memory`) is resolved in the running module's atom
    ///   table, turned into a reason term and raised with class `error` — the class
    ///   every BIF failure carries (`BADARG`/`BADARITH`/… are `EXTAG_ERROR`
    ///   reasons), which is what the handler's `catch error:Reason` clause sees.
    /// * An error the engine cannot name as a term propagates unchanged instead: a
    ///   [`BeamError::Raise`] **without** a live reason slot (its class/reason are
    ///   carried term-independently and the reason may be
    ///   [`beam_common::ExitReason::Structured`]), a variant with no BEAM atom
    ///   ([`BeamError::TermNotFound`], [`BeamError::Io`]), or an atom the running
    ///   module's atom table does not hold. Substituting a *different* atom would be
    ///   a wrong result, so the caller's typed error is preferred — a documented
    ///   Milestone-1 limit (the [`Module`] reaches the interpreter read-only, so a
    ///   name the compiler never emitted cannot be interned here, unlike the C
    ///   runtime's global atom table).
    ///
    /// A [`BeamError::Raise`] that **does** carry a live reason term
    /// ([`beam_common::Raise::live_reason`], payload [`Eterm`]) is an exception
    /// whose reason is a compound term — the shape [`beam_bifs`]-backed BIFs raise
    /// (e.g. `{badmap,M}`/`{badkey,K}`/`{bad_generator,G}`). It is raised through
    /// the one uniform entry point [`Machine::throw_with_trace`], exactly like an
    /// instruction-level raise: with a live catch frame it lands in the handler
    /// (`x(1) = reason`), and with none it propagates the term-carrying
    /// `Err(BeamError::Raise(..))`. This reproduces C, where `handle_error` always
    /// runs `next_catch` regardless of the reason's shape.
    ///
    /// Shared with the instruction families (e.g. `families::receive`'s `send`,
    /// which carries no fail label either), so the landing convention stays defined
    /// in exactly one place.
    pub(crate) fn raise_bif_error(&mut self, module: &Module, error: BeamError) -> Result<Step> {
        // A raise whose reason is a compound term lands (or propagates) with the
        // term, mirroring C's `handle_error` → `next_catch` for every reason shape.
        if let BeamError::Raise(raise) = &error {
            if let Some(reason) = raise
                .live_reason()
                .and_then(|live| live.downcast_ref::<Eterm>())
            {
                let class = raise.class;
                let reason = *reason;
                if let Some(class_atom) = module.atom_table.lookup(class.as_str()) {
                    self.set_class_atom(class, make_atom(class_atom));
                }
                return self.throw_with_trace(class, reason, NIL);
            }
        }
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
    fn eval_test(
        &self,
        module: &Module,
        kind: TestKind,
        args: &[Operand],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<bool> {
        let atoms = &module.atom_table;
        match kind {
            TestKind::IsLt => {
                let (a, b) = self.test_pair(args, ctx)?;
                Ok(cmp_arith(a, b, atoms).is_lt())
            }
            TestKind::IsGe => {
                let (a, b) = self.test_pair(args, ctx)?;
                Ok(cmp_arith(a, b, atoms).is_ge())
            }
            TestKind::IsEq => {
                let (a, b) = self.test_pair(args, ctx)?;
                Ok(cmp_arith(a, b, atoms).is_eq())
            }
            TestKind::IsNe => {
                let (a, b) = self.test_pair(args, ctx)?;
                Ok(!cmp_arith(a, b, atoms).is_eq())
            }
            TestKind::IsEqExact => {
                let (a, b) = self.test_pair(args, ctx)?;
                Ok(cmp(a, b, atoms).is_eq())
            }
            TestKind::IsNeExact => {
                let (a, b) = self.test_pair(args, ctx)?;
                Ok(!cmp(a, b, atoms).is_eq())
            }
            TestKind::IsInteger => Ok(predicates::is_integer(self.test_arg(args, 0, ctx)?)),
            TestKind::IsFloat => Ok(predicates::is_float(self.test_arg(args, 0, ctx)?)),
            TestKind::IsNumber => Ok(predicates::is_number(self.test_arg(args, 0, ctx)?)),
            TestKind::IsAtom => Ok(is_atom(self.test_arg(args, 0, ctx)?)),
            TestKind::IsPid => {
                let v = self.test_arg(args, 0, ctx)?;
                Ok(is_internal_pid(v) || predicates::is_external_pid(v))
            }
            TestKind::IsReference => {
                let v = self.test_arg(args, 0, ctx)?;
                Ok(predicates::is_internal_ref(v) || predicates::is_external_ref(v))
            }
            TestKind::IsPort => {
                let v = self.test_arg(args, 0, ctx)?;
                Ok(is_internal_port(v) || predicates::is_external_port(v))
            }
            TestKind::IsNil => Ok(is_nil(self.test_arg(args, 0, ctx)?)),
            TestKind::IsBinary => Ok(predicates::is_binary(self.test_arg(args, 0, ctx)?)),
            TestKind::IsBitstr => Ok(predicates::is_bitstring(self.test_arg(args, 0, ctx)?)),
            TestKind::IsList => {
                // `is_list` is true for `[]` *or* a cons cell.
                let v = self.test_arg(args, 0, ctx)?;
                Ok(is_nil(v) || is_not_nil_list(v))
            }
            TestKind::IsNonemptyList => Ok(is_not_nil_list(self.test_arg(args, 0, ctx)?)),
            TestKind::IsTuple => Ok(predicates::is_tuple(self.test_arg(args, 0, ctx)?)),
            TestKind::IsMap => Ok(predicates::is_map(self.test_arg(args, 0, ctx)?)),
            TestKind::IsBoolean => {
                // `is_boolean Fail Src` (`emu/instrs.tab:933`): `Src` is the atom
                // `true` or `false`. Both are ordinary atoms, so compare against the
                // module's atom table; a name absent from the table can never be the
                // value, so a missing name is `false` (not an error).
                let v = self.test_arg(args, 0, ctx)?;
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
            TestKind::IsFunction => Ok(crate::terms::is_fun(self.test_arg(args, 0, ctx)?)),
            TestKind::IsFunction2 => self.eval_is_function2(args, ctx),
            TestKind::IsTaggedTuple => self.eval_is_tagged_tuple(args, ctx),
            TestKind::TestArity => self.eval_test_arity(args, ctx),
        }
    }

    /// `is_function2 Fail Fun Arity` — `Fun` is a fun of the given arity.
    ///
    /// Operand order is the file's (`emu/ops.tab:831`, verified against the oracle:
    /// `{test,is_function2,{f,N},[Fun,{integer,Arity}]}`), i.e. `args[0]` = `Fun`,
    /// `args[1]` = `Arity` (an integer operand).
    fn eval_is_function2(&self, args: &[Operand], ctx: &mut dyn ExecutionContext) -> Result<bool> {
        let value = self.test_arg(args, 0, ctx)?;
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
    fn eval_is_tagged_tuple(
        &self,
        args: &[Operand],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<bool> {
        let value = self.test_arg(args, 0, ctx)?;
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
        let tag = self.test_arg(args, 2, ctx)?;
        // The first element of a tagged tuple is the (immediate) tag atom.
        Ok(crate::terms::tuple_element(value, 0)? == tag)
    }

    /// `test_arity Value Integer` — the value is a tuple of that arity.
    fn eval_test_arity(&self, args: &[Operand], ctx: &mut dyn ExecutionContext) -> Result<bool> {
        let value = self.test_arg(args, 0, ctx)?;
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
    fn test_arg(
        &self,
        args: &[Operand],
        i: usize,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        match args.get(i) {
            Some(operand) => read_operand(self, ctx, operand),
            None => Err(error::unsupported("test: missing argument")),
        }
    }

    /// Read the two arguments of a comparison test.
    fn test_pair(
        &self,
        args: &[Operand],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<(Eterm, Eterm)> {
        Ok((self.test_arg(args, 0, ctx)?, self.test_arg(args, 1, ctx)?))
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
        assert_eq!(map.resolve(Label(1)).unwrap(), (0, 0));
    }

    /// The index is module-wide: a target living in *another* function resolves to
    /// that function's `(index, code offset)`.
    #[test]
    fn label_map_resolves_cross_function_targets() {
        let (mut module, _) =
            module_with_function(vec![Instr::Label(Label(1)), Instr::Return], Vec::new());
        // A second function with labels `2` (offset 0) and `3` (offset 2).
        let (second, _) = module_with_function(
            vec![
                Instr::Label(Label(2)),
                Instr::Jump { target: Label(3) },
                Instr::Label(Label(3)),
                Instr::Return,
            ],
            Vec::new(),
        );
        module.functions.extend(second.functions);

        let map = build_label_map(&module);
        assert_eq!(map.resolve(Label(1)).unwrap(), (0, 0));
        assert_eq!(map.resolve(Label(2)).unwrap(), (1, 0));
        assert_eq!(map.resolve(Label(3)).unwrap(), (1, 2));
    }

    /// A missing label is the exact same typed error the previous `BTreeMap` map
    /// produced, through every entry point (`LabelMap::resolve`, `resolve`,
    /// `resolve_label`).
    #[test]
    fn label_map_missing_label_is_the_same_typed_error() {
        let (module, _) =
            module_with_function(vec![Instr::Label(Label(1)), Instr::Return], Vec::new());
        let map = build_label_map(&module);

        assert!(matches!(map.resolve(Label(99)), Err(BeamError::BadArg)));
        assert!(matches!(resolve(&map, Label(99)), Err(BeamError::BadArg)));
        assert!(matches!(
            resolve_label(&module, Label(99)),
            Err(BeamError::BadArg)
        ));
        // An out-of-range label beyond the dense table is a miss, not a panic.
        assert!(matches!(
            map.resolve(Label(u32::MAX)),
            Err(BeamError::BadArg)
        ));
        // `Label(0)` is the `NO_FAIL` sentinel, never a code label.
        assert!(matches!(map.resolve(NO_FAIL), Err(BeamError::BadArg)));
    }

    /// A sparse id space takes the sorted fallback (so one far-away id can never
    /// reserve a huge dense table) and still resolves every defined label.
    #[test]
    fn label_map_tolerates_a_sparse_id_space() {
        // `count = 2` but `max = 4_000_000_000`: a dense table would be ~48 GB.
        let (module, _) = module_with_function(
            vec![
                Instr::Label(Label(1)),
                Instr::Jump {
                    target: Label(4_000_000_000),
                },
                Instr::Label(Label(4_000_000_000)),
                Instr::Return,
            ],
            Vec::new(),
        );
        let map = build_label_map(&module);
        assert_eq!(map.resolve(Label(1)).unwrap(), (0, 0));
        assert_eq!(map.resolve(Label(4_000_000_000)).unwrap(), (0, 2));
        assert!(matches!(map.resolve(Label(2)), Err(BeamError::BadArg)));
    }

    /// The builder used by tests (`new`/`insert`/`from`) replaces a repeated id
    /// (last wins), exactly like the `BTreeMap` it replaced.
    #[test]
    fn label_map_builder_replaces_a_repeated_label() {
        let mut map = LabelMap::new();
        map.insert(Label(7), (0, 1));
        map.insert(Label(7), (0, 2));
        map.insert(Label(3), (1, 5));
        assert_eq!(map.resolve(Label(7)).unwrap(), (0, 2));
        assert_eq!(map.resolve(Label(3)).unwrap(), (1, 5));

        let from = LabelMap::from([(Label(4), (0, 4u32))]);
        assert_eq!(from.resolve(Label(4)).unwrap(), (0, 4));
        assert!(matches!(
            LabelMap::default().resolve(Label(4)),
            Err(BeamError::BadArg)
        ));
    }

    // ---- the argument-marshalling / fail-branch contract -------------------

    /// A one-function module whose body is `[Label(2), gc_bif, Label(3), return]`,
    /// the `gc_bif` calling `erlang:f/<args.len()>` with the given `fail` label.
    fn gc_bif_module(fail: Label, args: Vec<Operand>) -> Module {
        let arity = args.len() as u32;
        let (mut module, _) = module_with_function(
            vec![
                Instr::Label(Label(2)),
                Instr::GcBif {
                    arity: arity as u8,
                    bif: ImportRef(0),
                    fail,
                    live: arity,
                    args,
                    dst: Operand::X(arity),
                },
                Instr::Label(Label(3)),
                Instr::Return,
            ],
            Vec::new(),
        );
        let erlang = module.atom_table.intern("erlang");
        let name = module.atom_table.intern("f");
        module.imports.push(Import {
            module: erlang,
            function: name,
            arity,
        });
        module
    }

    /// A *marshalling* failure (a bad import reference, or a bad operand) must
    /// propagate the typed error **even with a live fail label**: it is not a guard
    /// failure, and branching to `fail` would silently continue down a wrong path.
    #[test]
    fn gc_bif_marshalling_errors_propagate_instead_of_branching() {
        // (a) The import reference is out of range (no imports declared) while the
        // instruction has a *live* fail label, which must not be used.
        let (module, _) = module_with_function(
            vec![
                Instr::Label(Label(2)),
                Instr::GcBif {
                    arity: 1,
                    bif: ImportRef(0),
                    fail: Label(3),
                    live: 1,
                    args: vec![Operand::X(0)],
                    dst: Operand::X(0),
                },
                Instr::Label(Label(3)),
                Instr::Return,
            ],
            Vec::new(),
        );
        let labels = build_label_map(&module);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let err = machine
            .step_instruction(&module, &labels, 0, 1, &mut ctx)
            .unwrap_err();
        match err {
            BeamError::Raise(raise) => assert_eq!(
                raise.reason.as_atom(),
                Some("unsupported_opcode: call: import out of range")
            ),
            other => panic!("expected the typed import error, got {other:?}"),
        }

        // (b) The operand is beyond the register file: a typed `BadArg`.
        let module = gc_bif_module(Label(3), vec![Operand::X(crate::machine::MAX_REGISTER)]);
        let labels = build_label_map(&module);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        assert!(matches!(
            machine.step_instruction(&module, &labels, 0, 1, &mut ctx),
            Err(BeamError::BadArg)
        ));
    }

    /// A genuine **BIF** error *is* a guard failure: it branches to the live fail
    /// label, or raises when there is none. (`MinimalContext::new()` rejects every
    /// BIF; `Label(3)` is code offset 2 of the only function.)
    #[test]
    fn gc_bif_bif_error_follows_the_fail_label_or_raises() {
        let module = gc_bif_module(Label(3), vec![Operand::X(0)]);
        let labels = build_label_map(&module);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        assert_eq!(
            machine
                .step_instruction(&module, &labels, 0, 1, &mut ctx)
                .unwrap(),
            Step::Goto { func: 0, pc: 2 }
        );

        let module = gc_bif_module(NO_FAIL, vec![Operand::X(0)]);
        let labels = build_label_map(&module);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        assert!(machine
            .step_instruction(&module, &labels, 0, 1, &mut ctx)
            .is_err());
    }

    /// An arity above [`STACK_ARGS`] takes the heap fallback; every operand must
    /// still arrive, in order.
    #[test]
    fn gc_bif_beyond_stack_args_uses_the_vec_fallback() {
        const N: usize = STACK_ARGS + 1;
        let module = gc_bif_module(NO_FAIL, (0..N as u32).map(Operand::X).collect());
        let labels = build_label_map(&module);
        let mut machine = Machine::new();
        for i in 0..N as u32 {
            machine.set_x(i, make_small(100 + i as i64)).unwrap();
        }
        let mut ctx = MinimalContext::with_bif_hook(Box::new(|m, f, a, args| {
            assert_eq!((m, f, a), ("erlang", "f", N as u32));
            assert_eq!(args.len(), N);
            for (i, arg) in args.iter().enumerate() {
                assert_eq!(beam_term::signed_value(*arg), 100 + i as i64);
            }
            Ok(args[N - 1])
        }));

        assert_eq!(
            machine
                .step_instruction(&module, &labels, 0, 1, &mut ctx)
                .unwrap(),
            Step::Next
        );
        assert_eq!(machine.x(N as u32), Some(make_small(100 + N as i64 - 1)));
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

    /// A module whose `n` imports are `erlang:f0/2` .. `erlang:f<n-1>/2`,
    /// interned into its own atom table.
    fn module_with_imports(n: u32) -> Module {
        let (mut module, _) =
            module_with_function(vec![Instr::Label(Label(1)), Instr::Return], Vec::new());
        let m = module.atom_table.intern("erlang");
        for i in 0..n {
            let f = module.atom_table.intern(&format!("f{i}"));
            module.imports.push(Import {
                module: m,
                function: f,
                arity: 2,
            });
        }
        module
    }

    /// The per-run cache answers exactly what the uncached resolution would, for
    /// every in-range import.
    #[test]
    fn import_cache_matches_the_uncached_resolution() {
        let module = module_with_imports(64);
        let labels = build_label_map(&module);
        for i in 0..64u32 {
            let cached = labels.import_target(&module, ImportRef(i)).unwrap();
            let direct = import_target(&module, ImportRef(i)).unwrap();
            assert_eq!(cached, direct);
            assert_eq!(cached.0, "erlang");
            assert_eq!(cached.1, format!("f{i}").as_str());
            assert_eq!(cached.2, 2);
        }
    }

    /// An out-of-range `ImportRef` is the same typed error as before, cached or
    /// not.
    #[test]
    fn import_cache_out_of_range_is_the_same_typed_error() {
        let module = module_with_imports(3);
        let labels = build_label_map(&module);
        for i in [3u32, 4, u32::MAX] {
            let cached = labels.import_target(&module, ImportRef(i)).unwrap_err();
            let direct = import_target(&module, ImportRef(i)).unwrap_err();
            assert_eq!(format!("{cached:?}"), format!("{direct:?}"));
            assert!(format!("{cached:?}").contains("call: import out of range"));
        }
    }

    /// An import whose module/function atom is absent from the table yields the
    /// same typed error from the cache and the fallback (never a substituted
    /// name).
    #[test]
    fn import_cache_missing_atoms_are_the_same_typed_error() {
        let (mut module, _) =
            module_with_function(vec![Instr::Label(Label(1)), Instr::Return], Vec::new());
        let good = module.atom_table.intern("erlang");
        let missing = Atom::from_raw(1_000_000);
        module.imports.push(Import {
            module: missing,
            function: good,
            arity: 1,
        });
        module.imports.push(Import {
            module: good,
            function: missing,
            arity: 1,
        });
        let labels = build_label_map(&module);
        for i in 0..2u32 {
            let cached = labels.import_target(&module, ImportRef(i)).unwrap_err();
            let direct = import_target(&module, ImportRef(i)).unwrap_err();
            assert_eq!(format!("{cached:?}"), format!("{direct:?}"));
            assert!(format!("{cached:?}").contains("call: import"));
        }
    }

    /// A `LabelMap` built from one module never answers for another: the owner
    /// check falls back to resolving against the module actually running, so a
    /// foreign name can never be substituted and a longer import table is never
    /// truncated to the cached length.
    #[test]
    fn import_cache_never_answers_for_a_foreign_module() {
        let a = module_with_imports(2);
        let b = module_with_imports(5);
        let labels_a = build_label_map(&a);

        let cached = labels_a.import_target(&b, ImportRef(1)).unwrap();
        assert_eq!(cached, import_target(&b, ImportRef(1)).unwrap());
        assert_eq!(cached.1, "f1");
        assert_eq!(cached.2, 2);
        // `b` has five imports, so index 4 resolves through the fallback even
        // though `a`'s cache only holds two.
        assert_eq!(labels_a.import_target(&b, ImportRef(4)).unwrap().1, "f4");
        // And `a`'s own cache still answers for `a`.
        assert_eq!(labels_a.import_target(&a, ImportRef(0)).unwrap().1, "f0");
    }

    /// A `LabelMap` that was not built from any module (`new`/`from`) carries no
    /// cache and still resolves through the fallback.
    #[test]
    fn import_cache_is_absent_on_a_hand_built_label_map() {
        let module = module_with_imports(2);
        let map = LabelMap::new();
        assert_eq!(
            map.import_target(&module, ImportRef(1)).unwrap(),
            import_target(&module, ImportRef(1)).unwrap()
        );
    }
}