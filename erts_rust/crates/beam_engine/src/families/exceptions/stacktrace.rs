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
//! **raise-time snapshot** the cooked trace is derived from — each frame resolved
//! in its **own** module context — and the precise residuals are documented on
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

use std::sync::Arc;

use beam_common::Result;
use beam_loader::{Atom, Instr, LineEntry, Module, Operand};
use beam_term::{make_atom, make_small, Eterm, NIL};

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::Machine;
use crate::operand::read_operand;
use crate::terms;
use crate::terms::{ContextSink, Sink};

// ---------------------------------------------------------------------------
// `build_stacktrace` — the symbolic (cooked) stacktrace
// ---------------------------------------------------------------------------

/// The symbolic (cooked) stacktrace `[{Module,Function,Arity,Location}]` for a
/// non-`NIL` raw stacktrace, innermost frame first.
///
/// **Where the frames come from.** C's `build_stacktrace`
/// (`../../erts/emulator/beam/beam_common.c:1204-1292`) walks the *saved* raw
/// `StackTrace` object carried by the exception (`s->trace[]`, a sequence of saved
/// continuation pointers) and resolves each through `erts_lookup_function_info`,
/// building `{M,F,A,Location}` items with `erts_build_mfa_item`
/// (`../../erts/emulator/beam/utils.c:3430-3457`). The engine keeps no such object,
/// but it does capture the equivalent information at **raise time**: every
/// [`Machine::throw_with_trace`] rebuilds [`Machine::raise_snapshot`] — the
/// pre-unwind `(context, func, pc)` frame chain, the **raising** frame first, then
/// every live caller innermost-first — *before* it unwinds the call frames to the
/// handler. This function therefore cooks from that snapshot, **not** from the
/// machine's live call frames: by the time the handler's `build_stacktrace` runs,
/// the frames the raise unwound out of are gone from the call-frame stack, yet the
/// snapshot still names them. `func` indexes the frame's own [`Module`]'s functions,
/// giving `{Module,Function,Arity}`, and the location is the `line` marker in effect
/// at `pc` (C's line lookup, `../../erts/emulator/beam/beam_file.c`).
///
/// **Which module a frame belongs to.** A snapshot entry's context names the module
/// its `(func, pc)` pair belongs to (a frame's continuation carries the module
/// context it returns into — see [`crate::machine::ModuleContext`]):
/// * context `0` — the run's entry module, which the machine **borrows** rather than
///   owns — resolves against the `module` argument, but **only** when the machine's
///   [`Machine::current_context`] is also `0` (then that argument *is* that entry
///   module). When the handler — where `build_stacktrace` runs — is in a registered
///   (non-zero) context, the caller's `module` argument is the *callee's* module, so
///   a context-`0` snapshot frame cannot be resolved and is **skipped** rather than
///   named against the wrong module.
/// * a non-zero context `id` resolves through [`Machine::module_context`]; an id that
///   was never registered is **skipped**.
///
/// A skipped frame contributes **no** item — this is best-effort metadata, exactly
/// like [`Machine::raise_stacktrace`] on the uncaught path, and a frame is never
/// named with a module its address does not belong to. An **empty** snapshot, or one
/// whose every frame is skipped, cooks to the **empty list** ([`NIL`]) — the honest
/// answer for a trace the engine cannot name. (A frame whose module *is* resolvable
/// but whose function index is outside that module's function table is a typed
/// [`crate::error::unsupported`], never a panic: the module is the only thing that
/// can name the address, so silently dropping it would hide a real mismatch.)
///
/// The residuals, all deliberate and all reported:
/// 1. **Only interpreted frames appear.** The frame stack starts at the
///    `run_export`/`run_label` entry, so a caller *outside* the run — the host's
///    own chain, the oracle's trailing `erl_eval`/`init` entries — has no frame
///    here and is absent from the result.
/// 2. **The snapshot's items carry the arity, never an argument list.** C uses the
///    exception's saved arguments where the raw trace holds them (`args` in
///    `erts_build_mfa_item`); the snapshot keeps no such list, so the arity — C's
///    own fallback for a frame without saved args — is always emitted. The one
///    exception is the synthesized **callee head item** an unresolvable
///    `call_ext*`/`apply` target publishes (`head` below, [`super::callee`]), which
///    is consed in front of the frame list and *does* carry the callee's real
///    argument list — exactly C's innermost item for an `undef`.
/// 3. **No `error_info` entries.** C's innermost item extends its `Location` with
///    the raw trace's extra info (`get_error_info_from_exc`); the engine has none.
///
/// The *order* is C's: `build_stacktrace` prepends the current function last
/// (`beam_common.c:1278-1288`), so the list runs innermost frame first.
///
/// **When this is called.** C's raw trace is an opaque object of **saved code
/// pointers** (`c_p->ftrace`), with no engine equivalent, so a landed exception still
/// carries `x(2) = NIL`. The compiled path's `build_stacktrace` therefore cooks **this**
/// function from the machine's raise-time snapshot whenever one is live (the compiler
/// reaches it only from a `try`/`catch` handler clause that just landed an exception —
/// see `super::Instr::BuildStacktrace`); a `NIL` raw trace with no recorded raise is
/// the only shape that cooks to `NIL`. The oracle's own cooked `S` for
/// `try ... catch C:R:S -> S end` additionally holds host `init:*`/`erl_eval` frames
/// that live *outside* this engine's run, so a snapshot-cooked trace is a
/// strictly-**shorter prefix** of the oracle's — the engine's frames first, never a
/// different value at the head.
///
/// The whole `[Item]` graph is sized from metadata, reserved in **one** rooted
/// allocation ([`crate::terms::reserve_slack`]) and carved from it, so a moving
/// collector cannot interleave a collection between the nodes.
///
/// **A synthesized head item.** When `head` is set, the raise being cooked was
/// raised by a call site that knew its unresolvable callee and published that
/// callee's `{Module,Function,ArgList,[]}` item as the raise's raw trace (see
/// [`super::callee`]). That item is one **more** node than the snapshot's frames —
/// and it is not a heap term held in a Rust local: it is the raise's own raw-trace
/// word, published in the register `head_operand` names, so it is re-read **after**
/// the one reservation (through the very operand `build_stacktrace` read it from)
/// and consed in front of the carved frames. The list is then never empty: an empty
/// frame chain with a live head cooks to the **one-element** list `[Item]`.
pub(crate) fn symbolic_stacktrace(
    machine: &mut Machine,
    entry_module: &Module,
    ctx: &mut dyn ExecutionContext,
    head_operand: &Operand,
    head: bool,
) -> Result<Eterm> {
    // The **raise-time** frame chain: the raising frame first, then every caller
    // innermost-first, exactly as it was captured *before* the raise unwound the
    // call frames. Copy it out of the machine so the `shapes` below borrow only
    // local module handles (never `machine`), leaving the later
    // `reserve_slack(machine, ..)` free to take `&mut machine`.
    let frames: Vec<(u32, u32, u32)> = machine.raise_snapshot().to_vec();
    // Resolve each frame's owning module *before* sizing: a non-zero context's
    // `Arc<Module>` must stay alive across the one reservation below, because an
    // [`ItemShape`] borrows its `Line` chunk (the file name).
    let mut owned: Vec<Arc<Module>> = Vec::new();
    let mut plan: Vec<FrameOwner> = Vec::with_capacity(frames.len());
    for (context, _func, _pc) in &frames {
        match *context {
            // Context `0` is ALWAYS the run's borrowed **entry module**, whatever
            // module context the handler cooking the trace is executing in.
            0 => plan.push(FrameOwner::EntryModule),
            id => match machine.module_context(id) {
                Some(handle) => {
                    plan.push(FrameOwner::Owned(owned.len()));
                    owned.push(handle.module.clone());
                }
                None => plan.push(FrameOwner::Skip),
            },
        }
    }
    // Only the *shapes* are computed here: no BEAM-heap word is touched yet. A
    // skipped frame contributes no shape.
    let mut shapes: Vec<ItemShape<'_>> = Vec::with_capacity(frames.len());
    for ((_context, func, pc), owner) in frames.iter().zip(plan.iter()) {
        let owner_module: &Module = match owner {
            FrameOwner::EntryModule => entry_module,
            FrameOwner::Owned(index) => &owned[*index],
            FrameOwner::Skip => continue,
        };
        shapes.push(item_shape(owner_module, *func, *pc)?);
    }
    // `2` words per list cell plus every item's own nodes — sized **before** the
    // reservation, because a collection under it relocates the machine's roots and
    // the shape metadata is the only thing that survives it. A synthesized callee
    // head item adds exactly one cons cell (`2` words) in front of the frame list.
    let total =
        2 * shapes.len() + shapes.iter().map(ItemShape::words).sum::<usize>() + if head { 2 } else { 0 };
    let mut sink = terms::reserve_slack(machine, ctx, total)?;
    let mut items = Vec::with_capacity(shapes.len());
    for shape in &shapes {
        // Child nodes before the parent, in the order `ItemShape::words` counts.
        items.push(write_item(&mut sink, shape)?);
    }
    // An empty (or wholly skipped) snapshot is the honest empty list — unless the
    // raise published a callee head item, which is always the FIRST item.
    let cooked = sink.list(&items)?;
    if head {
        // Post-collection read of the head word (the operand is the register the
        // calling `build_stacktrace` read the raw trace from — a pure register load,
        // so no allocation can intervene between the carve of `cooked` and this
        // cons), then the one cons cell that prepends it.
        let item = read_operand(machine, ctx, head_operand)?;
        return sink.cons(item, cooked);
    }
    Ok(cooked)
}

/// Which [`Module`] a snapshot frame's `(func, pc)` pair belongs to, resolved
/// **before** the shapes are computed and before the one reservation.
///
/// The distinction exists so that a resolved frame's `Arc<Module>` handle lives in
/// one local vector for the whole build: an [`ItemShape`] borrows the module's
/// `Line` chunk (the file name), so the handle must outlive every `reserve`/`write`
/// step — never be dropped after the shape was computed.
enum FrameOwner {
    /// The run's **entry** module — the `module` argument. Valid only for a
    /// context-`0` frame observed from a context-`0` handler.
    EntryModule,
    /// Index into the local handle vector: a registered context's module.
    Owned(usize),
    /// The frame's context cannot be resolved — the frame contributes no item.
    Skip,
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
