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
//!
//! # One reservation for the whole graph
//!
//! Both entry points here build a **multi-node** graph: a `{M,F,A,Location}` item,
//! its `[{file,File},{line,N}]` location (which is itself four nodes) and, for the
//! cooked trace, a list of such items. A per-node allocation would be wrong under a
//! **moving** collector: the nodes already written are not roots, so the next
//! collecting allocation would strand them. The graph is therefore sized from
//! **metadata only** ([`LocationShape`]/[`ItemShape`]), reserved in ONE rooted
//! allocation and carved node-by-node from that single region — see
//! [`crate::terms::slack`] and [`crate::terms::sink`]. One [`write_location`]/
//! [`write_item`] pair writes every node through the [`Sink`] trait, so the rooted
//! and the unrooted path cannot diverge in layout; the carve order is exactly the
//! order the shape's word count is computed in, so a correctly sized region is
//! consumed exactly.

use beam_common::Result;
use beam_loader::{Atom, Instr, LineEntry, Module};
use beam_term::{make_atom, make_small, Eterm, NIL};

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::Machine;
use crate::terms;
use crate::terms::{ContextSink, Sink};

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
///
/// The whole `[Item]` graph is sized from metadata, reserved in **one** rooted
/// allocation ([`crate::terms::reserve_slack`]) and carved from it, so a moving
/// collector cannot interleave a collection between the nodes.
pub(crate) fn symbolic_stacktrace(
    machine: &mut Machine,
    module: &Module,
    ctx: &mut dyn ExecutionContext,
    func: u32,
    pc: u32,
) -> Result<Eterm> {
    // The function executing right now (the dispatch loop's current position),
    // then every frame that is suspended in a call, innermost first. Only the
    // *shapes* are computed here: no BEAM-heap word is touched yet.
    let mut shapes = Vec::new();
    for (func, pc) in std::iter::once((func, pc)).chain(machine.resumption_addresses()) {
        shapes.push(item_shape(module, func, pc)?);
    }
    // `2` words per list cell plus every item's own nodes — sized **before** the
    // reservation, because a collection under it relocates the machine's roots and
    // the shape metadata is the only thing that survives it.
    let total = 2 * shapes.len() + shapes.iter().map(ItemShape::words).sum::<usize>();
    let mut sink = terms::reserve_slack(machine, ctx, total)?;
    let mut items = Vec::with_capacity(shapes.len());
    for shape in &shapes {
        // Child nodes before the parent, in the order `ItemShape::words` counts.
        items.push(write_item(&mut sink, shape)?);
    }
    sink.list(&items)
}

/// One `{Module,Function,Arity,Location}` stacktrace item for the function at
/// index `func`, located at code offset `pc` (C's `erts_build_mfa_item`,
/// `../../erts/emulator/beam/utils.c:3430-3457`).
///
/// Shared with the engine's read-only execution-position introspection
/// ([`Machine::current_location`](crate::machine::Machine::current_location)),
/// whose `process_info(P, current_location)` value is exactly this
/// `{M,F,A,Location}` tuple.
///
/// This entry point builds through the **unrooted** [`ContextSink`] (one
/// `alloc_words` per node) because its only callers — the four `&self`
/// execution-position accessors in [`crate::machine::location`] — hold no machine
/// roots at all and cannot lend `&mut` root slices. That is moving-safe here by
/// construction: every operand is a module-metadata atom, a small int, or a node
/// this same sink already built, so no machine-heap term is held across an
/// allocation — and `alloc_words` never collects. The exception family's own
/// caller uses [`symbolic_stacktrace`], which reserves the whole graph rooted.
pub(crate) fn stacktrace_item(
    module: &Module,
    ctx: &mut dyn ExecutionContext,
    func: u32,
    pc: u32,
) -> Result<Eterm> {
    let shape = item_shape(module, func, pc)?;
    let mut sink = ContextSink::new(ctx);
    write_item(&mut sink, &shape)
}

/// The metadata **shape** of a stacktrace item: everything [`write_item`] needs to
/// carve the node, and nothing that points into the BEAM heap.
///
/// Holding the shape rather than a built term is what lets the caller size a whole
/// graph before allocating (see the module docs).
struct ItemShape<'a> {
    /// The item's module atom (`{M,F,A}`'s `M`).
    module: Atom,
    /// The item's function atom (`{M,F,A}`'s `F`).
    function: Atom,
    /// The item's arity (`{M,F,A}`'s `A`).
    arity: u32,
    /// The `Location` element's shape, or `None` for the empty list.
    location: Option<LocationShape<'a>>,
}

impl ItemShape<'_> {
    /// The exact number of words this item's nodes occupy:
    /// the 4-element tuple (`5`) plus, when there is a location, the char list
    /// (`2 * chars`), the `{file,File}` tuple (`3`), the `{line,N}` tuple (`3`) and
    /// the 2-element outer list (`4`) — i.e. `2 * chars + 10`. A `None` location is
    /// [`NIL`] and occupies nothing.
    fn words(&self) -> usize {
        5 + match &self.location {
            Some(location) => 2 * location.file.chars().count() + 10,
            None => 0,
        }
    }
}

/// The metadata shape of a `Location`: `[{file,File},{line,N}]`'s atoms plus the
/// file name and line to materialize.
struct LocationShape<'a> {
    /// The `file` atom, resolved from the module atom table.
    file_atom: Atom,
    /// The `line` atom, resolved from the module atom table.
    line_atom: Atom,
    /// The source file name (the `Line` chunk stores it as UTF-8; the term is a
    /// list of code points).
    file: &'a str,
    /// The 1-based source line.
    line: u32,
}

/// The [`ItemShape`] for the function at index `func`, located at `pc` — metadata
/// only, no allocation.
///
/// The shape borrows the module's `Line` chunk (the file name), so the caller must
/// reserve and carve while that module is alive — which it is, since the reservation
/// happens inside the same call.
///
/// A `func` index outside the module's function table is a typed `unsupported`,
/// never a panic; a location the module cannot name (no `file`/`line` atom, a file
/// index outside the `Line` chunk) degrades to the empty list exactly as before.
fn item_shape(module: &Module, func: u32, pc: u32) -> Result<ItemShape<'_>> {
    let function = module
        .functions
        .get(func as usize)
        .ok_or_else(|| error::unsupported("build_stacktrace: function index out of range"))?;
    Ok(ItemShape {
        module: function.module,
        function: function.name,
        arity: function.arity,
        location: location_shape(module, func, pc)?,
    })
}

/// The `Location` element of a stacktrace item: `[{file,Name},{line,N}]` when the
/// function has a source location in effect at `pc`, else `None` (materialized as
/// [`NIL`]).
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
/// `unsupported`, never a substituted atom. The atoms are resolved only **after**
/// a real location is found, so a frame without one never errors on them.
fn location_shape(module: &Module, func: u32, pc: u32) -> Result<Option<LocationShape<'_>>> {
    let entry = match nearest_line(module, func, pc) {
        Some(entry) => entry,
        None => return Ok(None),
    };
    let name = match module.line.files.get(entry.file as usize) {
        Some(name) => name,
        None => return Ok(None),
    };
    let file_atom = module.atom_table.lookup("file").ok_or_else(|| {
        error::unsupported("build_stacktrace: module atom table lacks the 'file' atom")
    })?;
    let line_atom = module.atom_table.lookup("line").ok_or_else(|| {
        error::unsupported("build_stacktrace: module atom table lacks the 'line' atom")
    })?;
    Ok(Some(LocationShape {
        file_atom,
        line_atom,
        file: name,
        line: entry.line,
    }))
}

/// Carve `shape`'s nodes out of `sink`, children before parents: the location
/// (char list, `{file,File}`, `{line,N}`, the outer list), then the 4-element
/// `{Module,Function,Arity,Location}` tuple.
///
/// The carve order and sizes are exactly [`ItemShape::words`]'s, so a region sized
/// by that method is consumed exactly; a region that is somehow too small is the
/// typed [`crate::error::unsupported`] from [`crate::terms::Slack::alloc_words`],
/// never a wrong term.
fn write_item(sink: &mut dyn Sink, shape: &ItemShape<'_>) -> Result<Eterm> {
    let location = match &shape.location {
        Some(location) => write_location(sink, location)?,
        None => NIL,
    };
    sink.tuple(&[
        make_atom(shape.module),
        make_atom(shape.function),
        make_small(shape.arity as i64),
        location,
    ])
}

/// Carve `location`'s four nodes out of `sink` (see [`write_item`]).
fn write_location(sink: &mut dyn Sink, location: &LocationShape<'_>) -> Result<Eterm> {
    // An Erlang file name term is a list of code points (the oracle prints
    // `{file,"m.erl"}`); the `Line` chunk carries the name as UTF-8.
    let chars: Vec<Eterm> = location
        .file
        .chars()
        .map(|ch| make_small(ch as i64))
        .collect();
    let file_name = sink.list(&chars)?;
    let file_pair = sink.tuple(&[make_atom(location.file_atom), file_name])?;
    let line_pair = sink.tuple(&[
        make_atom(location.line_atom),
        make_small(location.line as i64),
    ])?;
    sink.list(&[file_pair, line_pair])
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
///
/// Shared with the [`Machine::raise_stacktrace`](crate::machine::Machine::raise_stacktrace)
/// metadata the uncaught path carries, so the symbolic trace and the raise's
/// frames agree on a frame's location.
pub(crate) fn nearest_line(module: &Module, func: u32, pc: u32) -> Option<LineEntry> {
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
