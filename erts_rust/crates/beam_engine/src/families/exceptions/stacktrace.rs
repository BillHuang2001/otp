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

use beam_common::{Result, StackFrame, StackTrace};
use beam_loader::{Atom, Instr, LineEntry, Module, Operand};
use beam_term::{is_list, is_nil, make_atom, make_small, AtomTable, Eterm, NIL};

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::{namespace_atom, Machine};
use crate::operand::read_operand;
use crate::terms;
use crate::terms::{ContextSink, Sink, Slack};

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
///   owns — resolves against the `entry_module` argument, whatever module context
///   the handler cooking the trace executes in: context `0` *always* names the run's
///   entry module, so a handler inside a cross-module callee still names the entry
///   module's frames correctly.
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
/// 2. **The snapshot's items carry the arity, except the innermost one, which carries
///    the argument list the raise published.** C takes `args` out of the exception
///    (`get_args_from_exc` → `erts_build_mfa_item`); the engine has no saved raw
///    trace, so the two raise sites that *know* the argument list arm a latch —
///    the `function_clause` site and an `erlang:error/2,3` raise — and that list
///    (as a term, verbatim: a proper list, an improper one or `[]`) fills the first
///    item's `A` slot. Every other item keeps the declared arity, C's own fallback.
///    The synthesized **callee head item** an unresolvable `call_ext*`/`apply`
///    target publishes (`head` below, [`super::callee`]) is consed in front of the
///    frame list and carries the callee's real argument list — exactly C's innermost
///    item for an `undef`.
/// 3. **The `error_info` extension rides the INNERMOST item, and never an outer
///    one.** C's `save_stacktrace` packs `{error_info, Map}` into the exception's
///    `ftrace` and `erts_build_mfa_item` starts **one** item's `Location` from it
///    (`get_error_info_from_exc`), so the engine resolves it for the first item of
///    the cooked list alone — by one of two routes:
///    - the synthesized **head item** (`head` below, [`super::bif_frame`]) carries
///      its own extension, derived from C's closed format-module table
///      (`erl_erts_errors`/`erl_kernel_errors`/`erl_stdlib_errors`) or omitted
///      entirely for a module outside it (C's `default:` arm leaves `Location`
///      `[]`);
///    - otherwise, a raise that published `beam_common::Raise::error_info` (an
///      `erlang:error/3`) attaches it to the caller frame, including a
///      `{error_info,Map}`-only location for a frame with no source position.
///    The raise's map is consumed by that first item: every outer item keeps the
///    plain two-element location.
///
/// The *order* is C's: `build_stacktrace` prepends the current function last
/// (`beam_common.c:1278-1288`), so the list runs innermost frame first.
///
/// **When this is called.** C's raw trace is an opaque object of **saved code
/// pointers** (`c_p->ftrace`), with no engine equivalent, so a landed exception carries
/// `x(2) = NIL` unless a raise **supplied** a trace — and a supplied one lands as the
/// already-cooked list, which returns through the `is_list` passthrough at the head of
/// this function (C's `is_raised_exc` arm). Everything else cooks **this** function
/// from the machine's raise-time snapshot whenever one is live (the compiler reaches
/// it only from a `try`/`catch` handler clause that just landed an exception — see
/// `super::Instr::BuildStacktrace`); a `NIL` raw trace with no recorded raise is the
/// only shape that cooks to `NIL`. The oracle's own cooked `S` for
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
///
/// **Supplied traces come first.** A raise that **arrived carrying** a trace renders
/// THAT trace, never the derived one, exactly as the oracle's `build_stacktrace`
/// returns a supplied trace unchanged (`../../erts/emulator/beam/beam_common.c:1219-1223`).
/// Two shapes reach here, both consumed **once**:
/// * an already-cooked list in the operand the caller read (`is_list` — the term a
///   landing published in `x(2)`, i.e. C's `c_p->ftrace` after `raw_raise`), returned
///   verbatim;
/// * explicit symbolic frames ([`Machine::take_raise_supplied_trace`]) —
///   `erlang:raise/3` with a non-empty trace list — carved into the same
///   `[{M,F,A,Location}]` shape, which REPLACES the derived trace.
/// An absent/empty supplied trace leaves every path byte-identical to the
/// `raise_snapshot`-derived behavior. A supplied frame carries an **arity** (never an
/// argument list) in its `A` slot and its `Location` is the
/// `[{file,File},{line,N}]` pair the [`SourceLocation`](beam_common::SourceLocation)
/// describes when present, else [`NIL`]. Every name it needs is resolved through the
/// **live** atom namespace ([`ExecutionContext::atoms`]) with the running module's
/// snapshot as the fallback — never substituted, so a name **neither** source can name
/// is the typed [`crate::error::unsupported`].
pub(crate) fn symbolic_stacktrace(
    machine: &mut Machine,
    entry_module: &Module,
    ctx: &mut dyn ExecutionContext,
    head_operand: &Operand,
    head: bool,
) -> Result<Eterm> {
    // An **already cooked** trace passes through verbatim: the landing publishes the
    // cooked `[{M,F,A,Location}]` list of the exception being handled in `x(2)` (see
    // [`Machine::land_exception`](crate::machine::Machine)), and C's
    // `build_stacktrace` likewise returns a supplied trace unchanged
    // (`get_args_from_exc` verbatim when `is_raised_exc`,
    // `../../erts/emulator/beam/beam_common.c:1219-1223`). A raw-trace word is never
    // a cons cell — it is `NIL` or one synthesized item (the callee head,
    // [`super::callee`]) — so `is_list` is exactly the "already cooked" test.
    //
    // The **verbatim echo** arm comes first, because the shape it carries may be
    // untestable by tag: `erlang:raise/3`'s third argument is echoed by the oracle
    // with no decoding, no name resolution, no re-encoding and no frame counting
    // (C's `build_stacktrace` `is_raised_exc` arm returns `c_p->ftrace` unchanged,
    // `../../erts/emulator/beam/beam_common.c:1219-1223`; for a trace whose frames
    // all already carry a location `raise_3` stored that very term,
    // `c_p->ftrace = stacktrace`, `bif.c:1641-1643`), and an **explicitly empty**
    // trace is the term `[]` — word-identical to "nothing supplied", so only the
    // engine's one-shot marker can tell them apart. Consumed once here, so it
    // describes exactly this cooking.
    let verbatim = machine.take_raise_supplied_term_verbatim();
    let raw = read_operand(machine, ctx, head_operand)?;
    if verbatim {
        return Ok(raw);
    }
    if is_list(raw) {
        return Ok(raw);
    }
    // A raise that ARRIVED carrying explicit symbolic frames renders THOSE frames,
    // replacing the `raise_snapshot` heuristic. Consumed once, so a later raise that
    // carries none falls back to the snapshot below; `None` leaves this function
    // byte-identical to the pre-supplied behavior.
    if let Some(supplied) = machine.take_raise_supplied_trace() {
        return carve_supplied_frames(machine, entry_module, ctx, &supplied);
    }
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
    //
    // The **failed clause's argument list** — a machine field the `function_clause`
    // raise site armed (`Machine::pending_function_clause_args`) — belongs to the
    // snapshot's FIRST frame (the raising frame, which is the one whose function
    // contains the failed clause and is the only frame C gives an `args` slot to).
    // It is taken here, once, so it can never leak into a later cooked trace; a frame
    // the module/context cannot name consumes it all the same (there is no later
    // frame it could belong to).
    let live_clause_args = machine.live_raise_function_clause_args();
    // The `{error_info, Map}` extension an `erlang:error/3` raise arrived carrying: it is
    // the **innermost cooked item's** location tail only, and a synthesized head item IS
    // that item — it carries its own location and its own arguments — so the extension is
    // attached to a snapshot frame **only when this cooking has no head item** (C's
    // `build_stacktrace` gives `args`/`error_info` to `fi` and builds every outer frame
    // with `am_true`/`NIL`: `../../erts/emulator/beam/beam_common.c:1283-1291`).
    //
    // Its `error_info` atom is resolved through the **live** namespace first and the
    // running module's load-time snapshot second ([`namespace_atom`]): the atom comes
    // from the map the *raising* module built, which a module loaded earlier need never
    // have interned (the same reason a supplied trace's names are resolved that way),
    // while `location_shape`'s `file`/`line` keys always come from the frame module's own
    // `Line` chunk. Only resolved when it is actually used, so a frame with no extension
    // never errors on it.
    let live_error_info = if head {
        None
    } else {
        machine.live_raise_error_info()
    };
    let error_info_atom = match live_error_info {
        Some(_) => Some(namespace_atom(ctx, entry_module, "error_info").ok_or_else(|| {
            error::unsupported(
                "build_stacktrace: cannot name the 'error_info' atom of the raise's error-info extension",
            )
        })?),
        None => None,
    };
    let mut args_item: Option<usize> = None;
    let mut error_info_item: Option<usize> = None;
    let mut error_info_map_words: Option<usize> = None;
    let mut shapes: Vec<ItemShape<'_>> = Vec::with_capacity(frames.len());
    for (index, ((_context, func, pc), owner)) in frames.iter().zip(plan.iter()).enumerate() {
        // Both slots belong to the innermost item, so a head-bearing cooking leaves them
        // to the head item (`super::callee`, or the BIF head builder) and never gives
        // them to the caller's frame.
        let args = if index == 0 && !head { live_clause_args } else { None };
        let owner_module: &Module = match owner {
            FrameOwner::EntryModule => entry_module,
            FrameOwner::Owned(index) => &owned[*index],
            FrameOwner::Skip => continue,
        };
        let mut shape = item_shape(owner_module, *func, *pc, args)?;
        if shape.args.is_some() {
            args_item = Some(shapes.len());
        }
        // The map is a **sizing** witness here: its word count is charged now (before
        // the reservation) and the carve re-reads the term from the re-seated latch
        // below, where the count is invariant under the move.
        if index == 0 {
            if let (Some(atom), Some(map)) = (error_info_atom, live_error_info) {
                let map_words = terms::bounded_copy_budget(map)?;
                shape.error_info = Some(ErrorInfoShape {
                    atom,
                    map,
                    map_words,
                });
                error_info_map_words = Some(map_words);
                error_info_item = Some(shapes.len());
            }
        }
        shapes.push(shape);
    }
    // `2` words per list cell plus every item's own nodes — sized **before** the
    // reservation, because a collection under it relocates the machine's roots and
    // the shape metadata is the only thing that survives it. A synthesized callee
    // head item adds exactly one cons cell (`2` words) in front of the frame list.
    let total =
        2 * shapes.len() + shapes.iter().map(ItemShape::words).sum::<usize>() + if head { 2 } else { 0 };
    let mut sink = terms::reserve_slack(machine, ctx, total)?;
    // The reservation above may have MOVED the failed clause's argument list: the
    // list is a machine-held term behind the raise latch, and `Machine::with_roots`
    // parks that latch (and re-seats it from the slot a conforming host wrote the
    // relocated word back through) for exactly this reason. The word captured before
    // the reservation is therefore finished with — the list is re-read HERE, from the
    // re-seated latch, and `write_item` deep-copies its elements out of it into the
    // same reservation, so no element word is held across a collection either. The
    // element count is invariant under a move, so the region sized above is still
    // exactly right.
    if live_clause_args.is_some() {
        let list = machine.take_raise_function_clause_args();
        if let Some(index) = args_item {
            match list {
                Some(list) if is_list(list) || is_nil(list) => shapes[index].args = Some(list),
                _ => {
                    return Err(error::unsupported(
                        "build_stacktrace: the live function-clause argument latch vanished across its reservation",
                    ))
                }
            }
        }
    }
    // The `{error_info, Map}` **map** rides the same split — read half before the
    // reservation (presence + word count, for sizing), consumed after it, so the word
    // the carve deep-copies into the location tail is the one the collector left in the
    // re-seated latch and never a pre-move address. The map's own nodes are **copied
    // now** (the raise's latch backing store may be rewound), so the extension costs the
    // region the `5`-word tuple shell plus the map words `ItemShape::words` charges.
    if live_error_info.is_some() {
        let map = machine.take_raise_error_info();
        if let Some(index) = error_info_item {
            match (map, error_info_atom, error_info_map_words) {
                (Some(map), Some(atom), Some(map_words)) => {
                    shapes[index].error_info = Some(ErrorInfoShape {
                        atom,
                        map,
                        map_words,
                    });
                }
                _ => {
                    return Err(error::unsupported(
                        "build_stacktrace: the live error-info latch vanished across its reservation",
                    ))
                }
            }
        }
    }
    // The live atom namespace the map's flat-map sort compares keys against; `sink` is
    // owned, so this immutable borrow of `ctx` cannot conflict with anything below.
    let atoms = ctx.atoms(entry_module);
    let mut items = Vec::with_capacity(shapes.len());
    for shape in &shapes {
        // Child nodes before the parent, in the order `ItemShape::words` counts.
        items.push(write_item(&mut sink, shape, atoms)?);
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

/// Cook a raise's **explicitly supplied** frames (a non-empty
/// [`Raise::stacktrace`](beam_common::Raise::stacktrace) the raise arrived with)
/// into the symbolic `[{M,F,A,Location}]` list, verbatim.
///
/// Each [`StackFrame`] becomes the item its `module`/`function`/`arity` name, with
/// its `location` (when present) materialized as the `[{file,File},{line,N}]` shape
/// [`write_location`] builds for a derived frame — so a supplied frame's `Location`
/// is byte-identical to the oracle's. A supplied frame carries an **arity**, never
/// an argument list, in its `A` slot.
///
/// Every atom a frame names (`Module`, `Function` and, for a location, `File` and
/// `line`) is resolved **before** the one reservation through the **live atom
/// namespace** ([`ExecutionContext::atoms`]) with the running module's load-time
/// snapshot as the fallback (see [`lookup_supplied`]); a name **neither** source can
/// name is the typed [`crate::error::unsupported`], never a substituted atom. The
/// whole list is one rooted reservation carved node-by-node, exactly like the
/// snapshot path.
///
/// This is the engine's **single** carve of a supplied trace: the public encoder
/// ([`crate::encode_supplied_stacktrace`] and its `Machine` twin) reuses it rather
/// than growing a second layout that could drift.
pub(crate) fn carve_supplied_frames(
    machine: &mut Machine,
    module: &Module,
    ctx: &mut dyn ExecutionContext,
    frames: &StackTrace,
) -> Result<Eterm> {
    // Only the *shapes* are computed here: no BEAM-heap word is touched yet, and no
    // shape is allocated. Every name is resolved before the reservation below. A
    // shape holds only `Copy` [`Atom`]s and `&str`s borrowed from the frames — never
    // a borrow of `ctx` — so the immutable namespace borrow ends before the
    // `&mut ctx` the reservation takes.
    let mut shapes: Vec<ItemShape<'_>> = Vec::with_capacity(frames.frames.len());
    for frame in &frames.frames {
        shapes.push(supplied_frame_shape(ctx, module, frame)?);
    }
    // `2` words per list cell plus every item's own nodes — sized before the
    // reservation, exactly as the snapshot path does.
    let total = 2 * shapes.len() + shapes.iter().map(ItemShape::words).sum::<usize>();
    let mut sink = terms::reserve_slack(machine, ctx, total)?;
    // Supplied frames carry neither an argument list nor an error-info map, so this
    // table is only threaded through `write_item` for signature uniformity; it is never
    // consulted by the carve below.
    let atoms = ctx.atoms(module);
    let mut items = Vec::with_capacity(shapes.len());
    for shape in &shapes {
        // Child nodes before the parent, in the order `ItemShape::words` counts.
        items.push(write_item(&mut sink, shape, atoms)?);
    }
    sink.list(&items)
}

/// The [`ItemShape`] of one **supplied** frame: the
/// `{Module,Function,Arity,Location}` metadata a [`StackFrame`] carries, with every
/// atom resolved (by name) through [`lookup_supplied`] — the host's live namespace
/// first, the running module's load-time snapshot second.
///
/// A name **neither** source can name is the typed [`crate::error::unsupported`] — the
/// engine never substitutes an atom the caller did not name, because a substituted
/// atom would be a *wrong* term. The shape borrows the supplied stacktrace (its file
/// names), so the caller must reserve and carve while it is alive — which it is,
/// since both happen inside [`carve_supplied_frames`].
fn supplied_frame_shape<'a>(
    ctx: &dyn ExecutionContext,
    module: &Module,
    frame: &'a StackFrame,
) -> Result<ItemShape<'a>> {
    let module_atom = lookup_supplied(ctx, module, frame.module.as_str(), "module")?;
    let function_atom = lookup_supplied(ctx, module, frame.function.as_str(), "function")?;
    let location = match &frame.location {
        Some(location) => {
            // The location's `{file,Name}` KEY is the atom `file`; `Name` is the file
            // name, rendered as a char list exactly as a derived frame's is.
            let file_atom = lookup_supplied(ctx, module, "file", "file")?;
            let line_atom = lookup_supplied(ctx, module, "line", "line")?;
            Some(LocationShape {
                file_atom,
                line_atom,
                file: location.file.as_str(),
                line: location.line,
            })
        }
        None => None,
    };
    Ok(ItemShape {
        module: module_atom,
        function: function_atom,
        arity: u32::from(frame.arity),
        args: None,
        args_words: 0,
        error_info: None,
        location,
    })
}

/// The **live namespace**'s atom for `name` (or the running module's snapshot
/// fallback), or the typed [`crate::error::unsupported`] naming the `what` a supplied
/// frame asked for.
///
/// The live namespace is the one the names came from: a supplied trace's
/// `{Module,Function,Arity,Location}` items are terms built by the *raising* program
/// from its runtime's atom namespace, and the module re-raising them (`proc_lib`, a
/// supervisor, `error_logger`) need never have mentioned those names — so a snapshot-
/// only lookup refused a trace the oracle echoes **verbatim**
/// (`build_stacktrace`'s `is_raised_exc` branch does no atom resolution at all,
/// `../../erts/emulator/beam/beam_common.c:1219-1223`).
///
/// A name **neither** source can name keeps the typed error: an atom index is valid
/// only against the table it was read with, so substituting one would be a *wrong*
/// term. The lookup result is a `Copy` [`Atom`], so the immutable borrow of `ctx`
/// ends here.
fn lookup_supplied(
    ctx: &dyn ExecutionContext,
    module: &Module,
    name: &str,
    what: &str,
) -> Result<Atom> {
    namespace_atom(ctx, module, name).ok_or_else(|| {
        error::unsupported(&format!(
            "build_stacktrace: supplied frame names a {what} atom the running module cannot resolve: {name}"
        ))
    })
}

/// Which [`Module`] a snapshot frame's `(func, pc)` pair belongs to, resolved
/// **before** the shapes are computed and before the one reservation.
///
/// The distinction exists so that a resolved frame's `Arc<Module>` handle lives in
/// one local vector for the whole build: an [`ItemShape`] borrows the module's
/// `Line` chunk (the file name), so the handle must outlive every `reserve`/`write`
/// step — never be dropped after the shape was computed.
enum FrameOwner {
    /// The run's **entry** module — the `entry_module` argument. Context `0` always
    /// names it, whatever module context the handler executes in.
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
    let shape = item_shape(module, func, pc, None)?;
    let mut sink = ContextSink::new(ctx);
    // `stacktrace_item` never carries an argument list or an error-info map (`item_shape`
    // is handed `None` for both), so the rooted `write_item` (which deep-copies those
    // payloads) is not needed here; this inline carve reproduces the exact
    // `{M,F,A,Location}` layout on the unrooted sink.
    let location = match &shape.location {
        Some(location) => write_location_unrooted(&mut sink, location)?,
        None => NIL,
    };
    sink.tuple(&[
        make_atom(shape.module),
        make_atom(shape.function),
        make_small(shape.arity as i64),
        location,
    ])
}

/// The metadata **shape** of a stacktrace item: everything [`write_item`] needs to
/// carve the node, and nothing that points into the BEAM heap — with **one**
/// deliberate exception, the failed clause's argument list ([`ItemShape::args`]),
/// which C's cooked item carries in its `A` slot in place of the arity.
///
/// Holding the shape rather than a built term is what lets the caller size a whole
/// graph before allocating (see the module docs).
struct ItemShape<'a> {
    /// The item's module atom (`{M,F,A}`'s `M`).
    module: Atom,
    /// The item's function atom (`{M,F,A}`'s `F`).
    function: Atom,
    /// The item's arity (`{M,F,A}`'s `A`) — what the `A` slot holds for every frame
    /// except the failed clause's own, which [`ItemShape::args`] replaces.
    arity: u32,
    /// The **innermost item's argument list**: the one frame whose function either
    /// contains the failed clause or is the caller of an `erlang:error/2,3` raise;
    /// `None` for every other frame.
    ///
    /// C's `erts_build_mfa_item` (`../../erts/emulator/beam/utils.c:3430-3457`) puts the
    /// saved `args` in the `A` slot when it is a list/nil, and only the
    /// `function_clause` raise site and an `error/2,3` raise publish one (see
    /// `Machine::take_raise_function_clause_args`) — so the oracle reports
    /// `{Mod, Fun, [Arg, …], Location}` for that function while every outer frame keeps
    /// its declared arity.
    ///
    /// The **list term itself** is deep-copied into the reservation (self-contained) and
    /// written into the tuple: a proper list, an **improper** list
    /// (`erlang:error(foo, [1|2])` keeps its tail) and `[]` all copy through unchanged —
    /// and [`ItemShape::words`] charges their real words via [`ItemShape::args_words`].
    ///
    /// The word is a machine-held term, so it is read by [`write_item`] **after** the one
    /// reservation, from the re-seated latch (`Machine::with_roots` parks and re-seats
    /// it); nothing allocates between that read and the deep copy into the reservation.
    args: Option<Eterm>,
    /// The words [`write_item`] must carve for the deep-copied `args` list, sized
    /// **before** the reservation by [`terms::bounded_copy_budget`]. The list is now
    /// copied into the reserved region (self-contained), so this charges its real words;
    /// `0` when there is no argument list.
    args_words: usize,
    /// The `{error_info, Map}` extension of this item's `Location`: the **caller
    /// frame** of an `erlang:error/3` raise, and `None` for every other item —
    /// including a synthesized head item, which carries its own extension and is
    /// consed in front of these shapes verbatim ([`super::bif_frame`]).
    ///
    /// C's `save_stacktrace` keeps the `EXC_ERROR_3` value as `c_p->fvalue`
    /// (`../../erts/emulator/beam/beam_common.c:1024-1026`) and packages it as
    /// `{error_info, Map}` in the exception's `ftrace`; `erts_build_mfa_item` starts the
    /// item's location from that cons and prepends the `{line,_}`/`{file,_}` pairs, so
    /// the oracle's innermost location is `[{file,F},{line,N},{error_info,Map}]` and
    /// every outer item's stays two elements.
    error_info: Option<ErrorInfoShape>,
    /// The `Location` element's shape, or `None` for the empty list.
    location: Option<LocationShape<'a>>,
}

/// The `{error_info, Map}` tail of a cooked item's location: the `error_info` **key
/// atom** (resolved before the reservation) and the **map term**, which is deep-copied
/// into the reservation rather than shared — the map's own nodes now ride the item.
#[derive(Clone, Copy)]
struct ErrorInfoShape {
    /// The `error_info` key atom, resolved through the live namespace first and the
    /// running module's snapshot second (`machine::namespace_atom`).
    atom: Atom,
    /// The map term, re-read from the re-seated raise latch **after** the one
    /// reservation, so a collecting host's relocation is what the carve writes.
    map: Eterm,
    /// The words [`write_error_info_pair`] carves for the deep-copied map, sized
    /// **before** the reservation by [`terms::bounded_copy_budget`].
    map_words: usize,
}

impl ItemShape<'_> {
    /// The exact number of words this item's nodes occupy:
    /// the 4-element tuple (`5`) plus, when there is a location, the char list
    /// (`2 * chars`), the `{file,File}` tuple (`3`), the `{line,N}` tuple (`3`) and the
    /// outer list (`4` for two elements) — i.e. `2 * chars + 10`; plus the
    /// `{error_info, Map}` extension (`5`) when one is attached, making the extended
    /// form `2 * chars + 15`.
    ///
    /// The extension's derivation: the `{error_info,Map}` 2-tuple is `3` words
    /// (`Sink::tuple` charges `elems.len() + 1`) and the outer list grows from 2 to 3
    /// cells, `+2` — and the **map is deep-copied now**, so [`terms::bounded_copy_budget`]
    /// charges its real words on top of that shell. With **no** location the extension is
    /// the whole location: a 1-element list (`2`) around the tuple (`3`), i.e. the same
    /// `5` shell plus the copied map's words.
    ///
    /// The argument list ([`ItemShape::args`]) is also deep-copied into the reservation,
    /// so [`ItemShape::args_words`] charges its real words.
    fn words(&self) -> usize {
        5 + self.args_words
            + match &self.location {
                Some(location) => 2 * location.file.chars().count() + 10,
                None => 0,
            }
            + match &self.error_info {
                Some(info) => 5 + info.map_words,
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
/// `failed_clause_args` is the argument list C puts in this item's `A` slot **instead
/// of** the arity, for the one frame whose function contains the failed clause (see
/// [`ItemShape::args`]); it is `None` for every other frame. A term that is neither a
/// proper list nor [`NIL`] is not an argument list at all and degrades to the arity,
/// exactly C's `is_list(args) || is_nil(args)` test.
///
/// The shape borrows the module's `Line` chunk (the file name), so the caller must
/// reserve and carve while that module is alive — which it is, since the reservation
/// happens inside the same call.
///
/// A `func` index outside the module's function table is a typed `unsupported`,
/// never a panic; a location the module cannot name (no `file`/`line` atom, a file
/// index outside the `Line` chunk) degrades to the empty list exactly as before.
fn item_shape(
    module: &Module,
    func: u32,
    pc: u32,
    failed_clause_args: Option<Eterm>,
) -> Result<ItemShape<'_>> {
    let function = module
        .functions
        .get(func as usize)
        .ok_or_else(|| error::unsupported("build_stacktrace: function index out of range"))?;
    // C's `is_list(args) || is_nil(args)` test, and no length read: the list term is
    // deep-copied into the `A` slot, so an improper list is walked (and never
    // rejected — `erlang:error(foo, [1|2])` keeps its tail verbatim in the oracle too).
    let args = match failed_clause_args {
        Some(list) if is_list(list) || is_nil(list) => Some(list),
        _ => None,
    };
    // Size the argument-list copy **before** the reservation: the structure (and thus the
    // word count) is invariant under the reservation's move, and `ItemShape::words`
    // charges exactly what `write_item` will carve.
    let args_words = match args {
        Some(list) => terms::bounded_copy_budget(list)?,
        None => 0,
    };
    Ok(ItemShape {
        module: function.module,
        function: function.name,
        arity: function.arity,
        args,
        args_words,
        error_info: None,
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
fn write_item(sink: &mut Slack, shape: &ItemShape<'_>, atoms: &AtomTable) -> Result<Eterm> {
    let location = match (&shape.location, shape.error_info) {
        (Some(location), error_info) => write_location(sink, location, error_info, atoms)?,
        // A frame with no source location (C's `LINE_INVALID_LOCATION`) still reports
        // the extension: `erts_build_mfa_item` starts `loc` from the `{error_info,Map}`
        // cons and only *prepends* the file/line pairs, so the location is the
        // one-element list `[{error_info,Map}]`.
        (None, Some(error_info)) => write_error_info_only(sink, error_info, atoms)?,
        (None, None) => NIL,
    };
    // The `A` slot: the innermost item's argument list when the raise published one,
    // else the declared arity. The list is **deep-copied** into the same reservation,
    // so its graph is self-contained and survives a later collection or the host's
    // rewind of the latch backing store (the word was re-seated from the raise latch
    // after the rooted allocation, but the cells behind it are copied here).
    let arity = match shape.args {
        Some(list) => terms::bounded_copy(sink, list, atoms)?,
        None => make_small(shape.arity as i64),
    };
    sink.tuple(&[make_atom(shape.module), make_atom(shape.function), arity, location])
}

/// Carve `location`'s nodes out of `sink` (see [`write_item`]), plus the
/// `{error_info, Map}` tail when the raise attached one.
///
/// The carve order (char list, `{file,File}`, `{line,N}`, the optional
/// `{error_info,Map}`, then the outer list) is exactly [`ItemShape::words`]'s, so a
/// region sized by that method is consumed exactly.
fn write_location(
    sink: &mut Slack,
    location: &LocationShape<'_>,
    error_info: Option<ErrorInfoShape>,
    atoms: &AtomTable,
) -> Result<Eterm> {
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
    match error_info {
        // C's `erts_build_mfa_item` prepends the pairs to the extension cons, so the
        // extension is the **third** element.
        Some(error_info) => {
            let pair = write_error_info_pair(sink, error_info, atoms)?;
            sink.list(&[file_pair, line_pair, pair])
        }
        None => sink.list(&[file_pair, line_pair]),
    }
}

/// The **unrooted** location carve for [`stacktrace_item`]: the
/// `[{file,File},{line,N}]` list on a [`ContextSink`], which never carries an
/// `{error_info, Map}` tail (that item has no raise payloads).
fn write_location_unrooted(
    sink: &mut dyn Sink,
    location: &LocationShape<'_>,
) -> Result<Eterm> {
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

/// The location of an item whose frame has **no** source location but whose raise did
/// carry the extension: `[{error_info, Map}]` (C starts `loc` from the extension and
/// prepends nothing when `fi->loc == LINE_INVALID_LOCATION`).
fn write_error_info_only(
    sink: &mut Slack,
    error_info: ErrorInfoShape,
    atoms: &AtomTable,
) -> Result<Eterm> {
    let pair = write_error_info_pair(sink, error_info, atoms)?;
    sink.list(&[pair])
}

/// Carve the `{error_info, Map}` 2-tuple; the **map is deep-copied** into the same
/// reservation so it is self-contained (never a pointer into the raise's latch backing
/// store, which the host may rewind after this carve).
fn write_error_info_pair(
    sink: &mut Slack,
    error_info: ErrorInfoShape,
    atoms: &AtomTable,
) -> Result<Eterm> {
    let map = terms::bounded_copy(sink, error_info.map, atoms)?;
    sink.tuple(&[make_atom(error_info.atom), map])
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
