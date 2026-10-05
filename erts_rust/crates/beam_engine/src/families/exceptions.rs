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
//! - [`Instr::BuildStacktrace`] — cook the raw stacktrace in `x(0)` into the
//!   symbolic `[{M,F,A,Location}]` form
//! - [`Instr::RawRaise`] — raise from `x(0)`=class, `x(1)`=value, `x(2)`=trace
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
//! - [`Instr::BuildStacktrace`] cooks a **NIL** raw stacktrace to `NIL` exactly as
//!   C does (a caught exception lands with `x(2) = NIL` because the machine
//!   captures no stacktrace), and approximates a non-NIL one from the machine's
//!   **live call frames** — the engine keeps no raw `StackTrace` object. See the
//!   `symbolic_stacktrace` body for the precise residuals.
//! - [`Instr::RawRaise`] reproduces C exactly for the three real classes and for
//!   the "unknown class" fall-through; the fall-through needs the atom `badarg` in
//!   the module's atom table (a module whose atom table lacks it is a typed
//!   error, never a substituted atom).
//! - The re-raise opcode [`Instr::Raise`] requires a class to be currently
//!   handled ([`Machine::pending_class`]); the compiler emits it only as the
//!   "no catch clause matched" tail of a handler, where that always holds
//!   (`lib/compiler/src/beam_ssa_*.erl` `resume` ⇒ `{bif,raise,..}`).

use std::collections::BTreeMap;

use beam_common::{ExceptionClass, Result};
use beam_loader::{Instr, Label, LineEntry, Module, Operand};
use beam_term::{is_nil, make_atom, make_small, Eterm, NIL};

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
    func: u32,
    pc: u32,
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
        Instr::Badmatch { value } => raise_error_tuple(machine, module, ctx, "badmatch", value),
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
                        error::unsupported("catch_end: module atom table lacks the 'EXIT' atom")
                    })?;
                    let tuple = terms::make_tuple(ctx, &[make_atom(exit_atom), reason])?;
                    machine.set_x(0, tuple)?;
                }
            }
            Ok(Some(Step::Next))
        }

        // `build_stacktrace` — cook the raw stacktrace in `x(0)` into the
        // symbolic `[{M,F,A,Location}]` list (C:
        // `x(0) = build_stacktrace(c_p, x(0))`, `emu/instrs.tab:1247-1251`).
        Instr::BuildStacktrace => {
            let raw = read_operand(machine, &Operand::X(0))?;
            // C's `get_trace_from_exc/1` returns NULL for a NIL exception, and
            // `build_stacktrace` then yields NIL (`beam_common.c:1219-1223`). The
            // M1 machine captures no stacktrace, so every caught exception lands
            // with `x(2) = NIL` (`Machine::land_exception`) and the compiler's
            // handler moves that into `x(0)` — the engine therefore reproduces the
            // oracle's `[]` exactly on the compiled path. A non-NIL raw stacktrace
            // (which no compiler-emitted path produces at M1) is approximated from
            // the machine's live call frames.
            let cooked = if is_nil(raw) {
                NIL
            } else {
                symbolic_stacktrace(machine, module, ctx, func, pc)?
            };
            machine.set_x(0, cooked)?;
            Ok(Some(Step::Next))
        }

        // `raw_raise` — raise from the register protocol (`emu/instrs.tab:1253-1263`,
        // `beam_common.c:1148-1179`): x(0) = the class atom, x(1) = the value,
        // x(2) = the raw stacktrace. `raw_raise` is what `sys_core_fold` folds the
        // `catch C:R:S -> erlang:raise(C, R, S)` shape into
        // (`lib/compiler/src/sys_core_fold.erl:2441-2465`).
        Instr::RawRaise => {
            let class_term = read_operand(machine, &Operand::X(0))?;
            let value = read_operand(machine, &Operand::X(1))?;
            let trace = read_operand(machine, &Operand::X(2))?;
            match classify_class(module, class_term) {
                Some(class) => {
                    // The class atom is exactly the one we matched, so it is
                    // present in the module atom table and the landing can install
                    // it in `x(3)` (C stores the class in the `StackTrace` struct
                    // *and* in `c_p->freason`, `beam_common.c:1151-1175`).
                    install_class_atom(machine, module, class)?;
                    machine.throw_with_trace(class, value, trace).map(Some)
                }
                None => {
                    // C: an unknown class is not an error — `raw_raise` returns 1,
                    // the instruction stores `am_badarg` in `x(0)` and execution
                    // continues with the next instruction (no raise,
                    // `beam_common.c:1176-1178`).
                    let badarg = module.atom_table.lookup("badarg").ok_or_else(|| {
                        error::unsupported("raw_raise: module atom table lacks the 'badarg' atom")
                    })?;
                    machine.set_x(0, make_atom(badarg))?;
                    Ok(Some(Step::Next))
                }
            }
        }

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

// ---------------------------------------------------------------------------
// `build_stacktrace` — the symbolic (cooked) stacktrace
// ---------------------------------------------------------------------------

/// The BEAM exception `class` the atom `class_term` names, if any.
///
/// C compares against its global `am_error`/`am_exit`/`am_throw`
/// (`../../erts/emulator/beam/beam_common.c:1161-1176`); the engine's atoms are
/// module-local, so the names are resolved through the running module's atom
/// table. Any other term — including a non-atom — is *not* a class, which is
/// exactly what makes `raw_raise` take its `badarg` fall-through.
fn classify_class(module: &Module, class_term: Eterm) -> Option<ExceptionClass> {
    [
        ExceptionClass::Error,
        ExceptionClass::Exit,
        ExceptionClass::Throw,
    ]
    .into_iter()
    .find(|class| module.atom_table.lookup(class.as_str()).map(make_atom) == Some(class_term))
}

/// The symbolic (cooked) stacktrace `[{Module,Function,Arity,Location}]` for a
/// non-`NIL` raw stacktrace, innermost frame first.
///
/// **Milestone-1 approximation — the precise residuals.** C's `build_stacktrace`
/// (`../../erts/emulator/beam/beam_common.c:1204-1292`) walks the *saved* raw
/// `StackTrace` object carried by the exception (`s->trace[]`, a sequence of saved
/// continuation pointers) and resolves each through `erts_lookup_function_info`,
/// building `{M,F,A,Location}` items with `erts_build_mfa_item`
/// (`../../erts/emulator/beam/utils.c:3430-3457`). The M1 machine keeps no such
/// object — it captures no stacktrace at all — so the engine substitutes the
/// equivalent information it *does* have: the **live call frames**. Every frame's
/// continuation-pointer slot is a `(func, pc)` ([`Machine::resumption_addresses`]);
/// `func` indexes the running [`Module`]'s functions, giving
/// `{Module,Function,Arity}`, and the location is the `line` marker in effect at
/// `pc` (C's line lookup, `../../erts/emulator/beam/beam_file.c`).
///
/// The residuals, all deliberate and all reported:
/// 1. **Only interpreted frames appear.** The frame stack starts at the
///    `run_export`/`run_label` entry, so a caller *outside* the run — the host's
///    own chain, the oracle's trailing `erl_eval`/`init` entries — has no frame
///    here and is absent from the result. A call that leaves the loop into the
///    host context (a `call_ext` into another module, an `apply_fun`) likewise
///    materialises no interpreted frame.
/// 2. **The frames are the live ones, not the saved ones.** C reports the chain
///    captured when the exception was raised; the engine reports the chain at
///    `build_stacktrace` time. The compiler emits `build_stacktrace` immediately
///    after `try_case`, which has already unwound to the frame owning the handler,
///    so C's saved chain additionally holds the frames the raise unwound out of.
/// 3. **The innermost item carries the arity, never an argument list.** C uses the
///    exception's saved arguments where the raw trace holds them (`args` in
///    `erts_build_mfa_item`); M1 keeps no such list, so the arity — C's own
///    fallback for a frame without saved args — is always emitted.
/// 4. **No `error_info` entries.** C's innermost item extends its `Location` with
///    the raw trace's extra info (`get_error_info_from_exc`); M1 has none.
///
/// The *order* is C's: `build_stacktrace` prepends the current function last
/// (`beam_common.c:1278-1288`), so the list runs innermost frame first.
fn symbolic_stacktrace(
    machine: &Machine,
    module: &Module,
    ctx: &mut dyn ExecutionContext,
    func: u32,
    pc: u32,
) -> Result<Eterm> {
    // The function executing right now (the dispatch loop's current position),
    // then every frame that is suspended in a call, innermost first.
    let mut frames = Vec::new();
    for (func, pc) in std::iter::once((func, pc)).chain(machine.resumption_addresses()) {
        frames.push(stacktrace_item(module, ctx, func, pc)?);
    }
    terms::make_list(ctx, &frames)
}

/// One `{Module,Function,Arity,Location}` stacktrace item for the function at
/// index `func`, located at code offset `pc` (C's `erts_build_mfa_item`,
/// `../../erts/emulator/beam/utils.c:3430-3457`).
fn stacktrace_item(
    module: &Module,
    ctx: &mut dyn ExecutionContext,
    func: u32,
    pc: u32,
) -> Result<Eterm> {
    let function = module
        .functions
        .get(func as usize)
        .ok_or_else(|| error::unsupported("build_stacktrace: function index out of range"))?;
    let location = location_term(module, ctx, func, pc)?;
    terms::make_tuple(
        ctx,
        &[
            make_atom(function.module),
            make_atom(function.name),
            make_small(function.arity as i64),
            location,
        ],
    )
}

/// The `Location` element of a stacktrace item: `[{file,Name},{line,N}]` when the
/// function has a source location in effect at `pc`, else [`NIL`].
///
/// C appends the pair only when the resolved location is not
/// `LINE_INVALID_LOCATION`, in that `{file,..}`-then-`{line,..}` order
/// (`../../erts/emulator/beam/utils.c:3432-3446`). The engine maps "invalid" to
/// the loader's undefined location (item 0, whose line is 0 — real Erlang source
/// lines are 1-based) and to a file-name index outside the module's `Line` chunk;
/// both yield [`NIL`] rather than an invented name.
///
/// The `file`/`line` atoms are resolved from the **module** atom table (C reads
/// its globals `am_file`/`am_line`); a module that cannot name them is a typed
/// `unsupported`, never a substituted atom.
fn location_term(
    module: &Module,
    ctx: &mut dyn ExecutionContext,
    func: u32,
    pc: u32,
) -> Result<Eterm> {
    let entry = match nearest_line(module, func, pc) {
        Some(entry) => entry,
        None => return Ok(NIL),
    };
    let name = match module.line.files.get(entry.file as usize) {
        Some(name) => name,
        None => return Ok(NIL),
    };
    let file_atom = module.atom_table.lookup("file").ok_or_else(|| {
        error::unsupported("build_stacktrace: module atom table lacks the 'file' atom")
    })?;
    let line_atom = module.atom_table.lookup("line").ok_or_else(|| {
        error::unsupported("build_stacktrace: module atom table lacks the 'line' atom")
    })?;
    // An Erlang file name term is a list of code points (the oracle prints
    // `{file,"m.erl"}`); the `Line` chunk carries the name as UTF-8.
    let chars: Vec<Eterm> = name.chars().map(|ch| make_small(ch as i64)).collect();
    let file_name = terms::make_list(ctx, &chars)?;
    let file_pair = terms::make_tuple(ctx, &[make_atom(file_atom), file_name])?;
    let line_pair = terms::make_tuple(
        ctx,
        &[make_atom(line_atom), make_small(entry.line as i64)],
    )?;
    terms::make_list(ctx, &[file_pair, line_pair])
}

/// The source location in effect at code offset `pc` of the function at index
/// `func`: the loader's entry for the **nearest preceding** `line` marker, or
/// `None` when there is none.
///
/// The loader lowers every `line` opcode to [`Instr::Line`] carrying the index of
/// its [`LineEntry`], and the entry a marker installs stays in effect until the
/// next one — the mapping the C interpreter's `line` handling establishes
/// (`../../erts/emulator/beam/beam_file.c`) and that `erts_lookup_function_info`
/// reads back for a saved code pointer. An instruction before any marker has no
/// location (C's `LINE_INVALID_LOCATION`).
fn nearest_line(module: &Module, func: u32, pc: u32) -> Option<LineEntry> {
    let function = module.functions.get(func as usize)?;
    if function.code.is_empty() {
        return None;
    }
    let end = (pc as usize).min(function.code.len() - 1);
    for instr in function.code[..=end].iter().rev() {
        if let Instr::Line(reference) = instr {
            let entry = *module.line.items.get(*reference as usize)?;
            return (entry.line != 0).then_some(entry);
        }
    }
    None
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
    fn exec_one(
        module: &Module,
        instr: &Instr,
        machine: &mut Machine,
        ctx: &mut MinimalContext,
    ) -> Result<Option<Step>> {
        exec(machine, module, &no_labels(), instr, 0, 0, ctx)
    }

    /// Assert that `reason` (an `Eterm`) equals the tuple `[tag, elem]`.
    fn assert_reason_tuple(
        cmp_ctx: &mut MinimalContext,
        module: &Module,
        reason: Eterm,
        tag: &str,
        elem: Eterm,
    ) {
        let expected = terms::make_tuple(
            cmp_ctx,
            &[make_atom(module.atom_table.lookup(tag).unwrap()), elem],
        )
        .unwrap();
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
        assert_eq!(
            reason,
            make_atom(module.atom_table.lookup("if_clause").unwrap())
        );

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

    // ------------------------------------------------------------------
    // raw_raise (the x(0)/x(1)/x(2) class protocol)
    // ------------------------------------------------------------------

    #[test]
    fn raw_raise_with_a_valid_class_raises_through_the_machine() {
        // C: `raw_raise` sets the class in `c_p->freason` and the value/trace in
        // `fvalue`/`ftrace`, then `goto find_func_info` — i.e. an ordinary raise.
        for class in [
            ExceptionClass::Error,
            ExceptionClass::Exit,
            ExceptionClass::Throw,
        ] {
            let module = Fixture::new(&[class.as_str()]).finish();
            let mut machine = Machine::new();
            machine.push_frame(0).unwrap();
            machine.push_catch_frame(CatchFrame::new(4, 9, 0));
            let class_atom = make_atom(module.atom_table.lookup(class.as_str()).unwrap());
            machine.set_x(0, class_atom).unwrap();
            machine.set_x(1, make_small(42)).unwrap();
            machine.set_x(2, NIL).unwrap();

            let mut ctx = MinimalContext::new();
            let step = exec_one(&module, &Instr::RawRaise, &mut machine, &mut ctx).unwrap();
            assert_eq!(
                step,
                Some(Step::Goto { func: 4, pc: 9 }),
                "raw_raise with class {} lands in the live catch frame",
                class.as_str()
            );
            assert_eq!(machine.pending_exception(), Some((class, make_small(42))));
            // The landing convention: reason in x(1), the trace in x(2), the class
            // atom in x(3).
            assert_eq!(machine.x(0), Some(Eterm::non_value()));
            assert_eq!(machine.x(1), Some(make_small(42)));
            assert_eq!(machine.x(2), Some(NIL));
            assert_eq!(machine.x(3), Some(class_atom));
        }
    }

    #[test]
    fn raw_raise_uncaught_propagates_a_typed_error() {
        let module = Fixture::new(&["throw"]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        machine
            .set_x(0, make_atom(module.atom_table.lookup("throw").unwrap()))
            .unwrap();
        machine.set_x(1, make_small(7)).unwrap();
        machine.set_x(2, NIL).unwrap();

        let mut ctx = MinimalContext::new();
        let err = exec_one(&module, &Instr::RawRaise, &mut machine, &mut ctx).unwrap_err();
        assert!(matches!(err, BeamError::Raise(_)));
        assert_eq!(
            machine.pending_exception(),
            Some((ExceptionClass::Throw, make_small(7)))
        );
    }

    #[test]
    fn raw_raise_with_an_unknown_class_sets_x0_to_badarg_and_continues() {
        // C: an unknown class makes `raw_raise` return 1 and the instruction stores
        // `am_badarg` in x(0) **without raising** (`beam_common.c:1176-1178`,
        // `emu/instrs.tab:1258-1260`) — the oracle-witnessed `badclass/1` value.
        // A non-class *atom*, then a term that is not an atom at all.
        for unknown in [Some("nonsense"), None] {
            let module = Fixture::new(&["badarg", "nonsense"]).finish();
            let mut machine = Machine::new();
            machine.push_frame(0).unwrap();
            machine.push_catch_frame(CatchFrame::new(4, 9, 0));
            machine.set_x(1, make_small(1)).unwrap();
            machine.set_x(2, NIL).unwrap();
            let unknown_term = match unknown {
                Some(name) => make_atom(module.atom_table.lookup(name).unwrap()),
                None => make_small(3),
            };
            machine.set_x(0, unknown_term).unwrap();

            let mut ctx = MinimalContext::new();
            let step = exec_one(&module, &Instr::RawRaise, &mut machine, &mut ctx).unwrap();
            assert_eq!(step, Some(Step::Next), "an unknown class does not raise");
            assert_eq!(
                machine.x(0),
                Some(make_atom(module.atom_table.lookup("badarg").unwrap()))
            );
            // Nothing was raised: no pending exception, and the catch frame is
            // still live for the next instruction.
            assert_eq!(machine.pending_exception(), None);
            assert_eq!(machine.catch_frame_depth(), 1);
        }
    }

    #[test]
    fn raw_raise_without_the_badarg_atom_is_a_typed_error() {
        // The engine may not substitute a different atom for `badarg`.
        let module = Fixture::new(&["nonsense"]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        machine
            .set_x(0, make_atom(module.atom_table.lookup("nonsense").unwrap()))
            .unwrap();
        let mut ctx = MinimalContext::new();
        let err = exec_one(&module, &Instr::RawRaise, &mut machine, &mut ctx).unwrap_err();
        assert!(matches!(err, BeamError::Raise(_)));
        assert_eq!(machine.pending_exception(), None);
    }

    // ------------------------------------------------------------------
    // build_stacktrace (the symbolic stacktrace)
    // ------------------------------------------------------------------

    #[test]
    fn build_stacktrace_of_a_nil_raw_trace_is_nil() {
        // C: `get_trace_from_exc(NIL)` is NULL, so `build_stacktrace` yields NIL
        // (`beam_common.c:1219-1223`). This is the path every compiler-emitted
        // handler takes at M1 (a caught exception lands with x(2) = NIL).
        let module = Fixture::new(&[]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        machine.set_x(0, NIL).unwrap();
        let mut ctx = MinimalContext::new();
        let step = exec_one(&module, &Instr::BuildStacktrace, &mut machine, &mut ctx).unwrap();
        assert_eq!(step, Some(Step::Next));
        assert_eq!(machine.x(0), Some(NIL));
    }

    #[test]
    fn build_stacktrace_derives_the_live_callers_innermost_first() {
        // A non-NIL raw trace (only a synthetic module produces one at M1) is
        // approximated from the live frames: the current function first, then every
        // frame suspended in a call, innermost first. No line marker is in effect,
        // so each Location is NIL (C's LINE_INVALID_LOCATION).
        let mut fixture = Fixture::new(&[]);
        let entry = fixture.add_function(
            "f",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::BuildStacktrace,
                Instr::Return,
            ],
        );
        let g_entry = fixture.add_function(
            "g",
            2,
            vec![Instr::Label(Label(2)), Instr::Return],
        );
        let module = fixture.finish();
        assert_eq!(entry, Label(1));
        assert_eq!(g_entry, Label(2));

        let mut machine = Machine::new();
        // The bottom frame, then a frame that called into `f` — its continuation is
        // `f` at code offset 2 (`g/2`'s own frame-less `return`).
        machine.push_frame(0).unwrap();
        machine.push_frame(0).unwrap();
        machine
            .set_continuation(crate::machine::Continuation::Address { func: 1, pc: 1 })
            .unwrap();
        // x(0) must hold a non-NIL raw stacktrace for the instruction to cook one.
        machine.set_x(0, make_small(1)).unwrap();

        let mut ctx = MinimalContext::new();
        // Run the `build_stacktrace` at code offset 1 of function 0.
        let step = exec(
            &mut machine,
            &module,
            &no_labels(),
            &Instr::BuildStacktrace,
            0,
            1,
            &mut ctx,
        )
        .unwrap();
        assert_eq!(step, Some(Step::Next));

        let m = module.atom_table.lookup("m").unwrap();
        let f = module.atom_table.lookup("f").unwrap();
        let g = module.atom_table.lookup("g").unwrap();
        let item_f = terms::make_tuple(&mut ctx, &[make_atom(m), make_atom(f), make_small(0), NIL])
            .unwrap();
        let item_g = terms::make_tuple(&mut ctx, &[make_atom(m), make_atom(g), make_small(2), NIL])
            .unwrap();
        let expected = terms::make_list(&mut ctx, &[item_f, item_g]).unwrap();
        assert!(
            cmp(machine.x(0).unwrap(), expected, &module.atom_table).is_eq(),
            "the cooked trace must be [{{m,f,0,[]}}, {{m,g,2,[]}}]"
        );
    }

    #[test]
    fn build_stacktrace_item_carries_the_source_location() {
        // The Location element is `[{file,Name},{line,N}]`, taken from the `line`
        // marker in effect at the frame's code offset (C's `erts_build_mfa_item`).
        let mut fixture = Fixture::new(&["file", "line"]);
        fixture.module.line = beam_loader::LineTable {
            flags: 0,
            instruction_count: 0,
            items: vec![
                LineEntry::default(),
                LineEntry { file: 0, line: 7 },
            ],
            files: vec!["m.erl".to_string()],
        };
        fixture.add_function(
            "f",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::Line(1),
                Instr::BuildStacktrace,
                Instr::Return,
            ],
        );
        let module = fixture.finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        machine.set_x(0, make_small(1)).unwrap(); // a non-NIL raw trace
        let mut ctx = MinimalContext::new();
        let step = exec(&mut machine, &module, &no_labels(), &Instr::BuildStacktrace, 0, 2, &mut ctx)
            .unwrap();
        assert_eq!(step, Some(Step::Next));

        let m = module.atom_table.lookup("m").unwrap();
        let f = module.atom_table.lookup("f").unwrap();
        let file = module.atom_table.lookup("file").unwrap();
        let line = module.atom_table.lookup("line").unwrap();
        let chars: Vec<Eterm> = "m.erl".chars().map(|c| make_small(c as i64)).collect();
        let name = terms::make_list(&mut ctx, &chars).unwrap();
        let file_pair = terms::make_tuple(&mut ctx, &[make_atom(file), name]).unwrap();
        let line_pair = terms::make_tuple(&mut ctx, &[make_atom(line), make_small(7)]).unwrap();
        let location = terms::make_list(&mut ctx, &[file_pair, line_pair]).unwrap();
        let item =
            terms::make_tuple(&mut ctx, &[make_atom(m), make_atom(f), make_small(0), location])
                .unwrap();
        let expected = terms::make_list(&mut ctx, &[item]).unwrap();
        assert!(
            cmp(machine.x(0).unwrap(), expected, &module.atom_table).is_eq(),
            "the item must carry [{{file,\"m.erl\"}},{{line,7}}]"
        );
    }

    #[test]
    fn build_stacktrace_without_the_file_or_line_atom_is_a_typed_error() {
        // The location can only be named if the module's atom table holds
        // `file`/`line` (C reads its globals `am_file`/`am_line`).
        for extra in [&["line"][..], &["file"][..]] {
            let mut fixture = Fixture::new(extra);
            fixture.module.line = beam_loader::LineTable {
                flags: 0,
                instruction_count: 0,
                items: vec![LineEntry::default(), LineEntry { file: 0, line: 7 }],
                files: vec!["m.erl".to_string()],
            };
            fixture.add_function(
                "f",
                0,
                vec![Instr::Label(Label(1)), Instr::Line(1), Instr::BuildStacktrace],
            );
            let module = fixture.finish();
            let mut machine = Machine::new();
            machine.push_frame(0).unwrap();
            machine.set_x(0, make_small(1)).unwrap();
            let mut ctx = MinimalContext::new();
            let err =
                exec(&mut machine, &module, &no_labels(), &Instr::BuildStacktrace, 0, 2, &mut ctx)
                    .unwrap_err();
            assert!(
                matches!(err, BeamError::Raise(_)),
                "a module that cannot name file/line must fail typed"
            );
        }
    }

    #[test]
    fn build_stacktrace_of_an_unknown_function_index_is_a_typed_error() {
        let module = Fixture::new(&[]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        machine.set_x(0, make_small(1)).unwrap();
        let mut ctx = MinimalContext::new();
        // Function index 3 does not exist in the empty module.
        let err = exec(
            &mut machine,
            &module,
            &no_labels(),
            &Instr::BuildStacktrace,
            3,
            0,
            &mut ctx,
        )
        .unwrap_err();
        assert!(matches!(err, BeamError::Raise(_)));
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
        let result = machine.run_label(&module, entry, 0, &[], &mut ctx).unwrap();
        let expected = terms::make_tuple(
            &mut ctx,
            &[
                make_atom(module.atom_table.lookup("case_clause").unwrap()),
                make_small(2),
            ],
        )
        .unwrap();
        assert!(cmp(result, expected, &module.atom_table).is_eq());
        assert_eq!(
            machine.frame_depth(),
            1,
            "the run is back at the bottom frame"
        );
    }
}
