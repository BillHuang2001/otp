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

//! `build_stacktrace` — cooking a raw stacktrace into the symbolic
//! `[{Module,Function,Arity,Location}]` form.
//!
//! Split out of [`super`] (the family's `exec` dispatch) by responsibility; the
//! Milestone-1 approximation and its precise residuals are documented on
//! [`symbolic_stacktrace`].

use beam_common::Result;
use beam_loader::{Instr, LineEntry, Module};
use beam_term::{make_atom, make_small, Eterm, NIL};

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::Machine;
use crate::terms;

// ---------------------------------------------------------------------------
// `build_stacktrace` — the symbolic (cooked) stacktrace
// ---------------------------------------------------------------------------

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
pub(crate) fn symbolic_stacktrace(
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
///
/// Shared with the engine's read-only execution-position introspection
/// ([`Machine::current_location`](crate::machine::Machine::current_location)),
/// whose `process_info(P, current_location)` value is exactly this
/// `{M,F,A,Location}` tuple.
pub(crate) fn stacktrace_item(
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
