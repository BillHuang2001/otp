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

//! The **synthesized BIF head item** of a raise coming out of a BIF.
//!
//! C's `save_stacktrace` (`../../erts/emulator/beam/beam_common.c:877-1063`) makes
//! the failing **BIF itself** the innermost frame of the exception's saved raw
//! trace whenever the failure happened in a BIF *other than* `erlang:error/1,2,3`,
//! `erlang:exit/1` and `erlang:throw/1` (:906-921):
//!
//! ```text
//! s->current = bif_mfa;      /* the BIF MFA, not the caller's function */
//! s->pc      = NULL;         /* no code address, so NO {file,_}/{line,_}      */
//! args       = make_arglist(c_p, reg, bif_mfa->arity);   /* the REAL arguments */
//! ```
//!
//! and it attaches a location extension derived from a **closed** table of BIF
//! modules (:937-1006, [`format_module`]): `#{module => FormatModule}`, or —
//! when the exception carries `EXF_HAS_EXT_INFO` — `#{cause => Fvalue, module =>
//! FormatModule}`. A module outside that table gets **no `error_info` at all**, so
//! its item's `Location` is the empty list.
//!
//! # The shape rule: only a *declared* BIF gets the extension
//!
//! Rule 1 above is C's `bif_mfa` path: it applies to a BIF the `bif.tab` surface
//! **declares**. The engine's BIF door is wider than that — a host serves every
//! `beam_bifs` registry key through
//! [`call_bif`](crate::context::ExecutionContext::call_bif), including the
//! **stdlib fast paths** that stand in for pure-Erlang source functions
//! (`lists:max/1`, `lists:nth/2`, …). The oracle's innermost frame for such a call
//! is the **source function**
//! (`{lists, max, [[]], [{file,"lists.erl"},{line,638}]}`), never a BIF frame, so an
//! `error_info` extension there would be a fabricated classification — and a
//! harmful one, because `erl_error:format_exception/4` would then look up
//! `erl_stdlib_errors:format_error/1` for a call that never was a BIF.
//!
//! The rule is therefore closed-form, and both halves land on the same shape (the
//! real argument list with an **empty** `Location`):
//!
//! * [`BifKind::StdlibFastPath`] — the host classifies the name as a fast path
//!   ([`ExecutionContext::bif_kind`](crate::context::ExecutionContext::bif_kind));
//!   the extension is dropped whatever the reason.
//! * the reason is the bare atom **`function_clause`** — C's only producer is
//!   `func_info` (`emu/instrs.tab`'s `i_func_info_IaaI`), which names **its own**
//!   frame with a source location, and **no declared BIF raises it**; so an
//!   extension on such a raise is always a misclassification. This arm applies
//!   **today**, before any host overrides [`BifKind`], which is what makes
//!   `lists:max([])` cook `{lists,max,[[]],[]}` on a pre-seam host.
//!
//! The oracle's true `{file,_}`/`{line,_}` location for a fast path is an honest
//! residual (see the module-level "Residuals" below): the engine has no
//! "source location of a loaded function" door, so the C-producible, non-fabricated
//! shape is the empty location.
//!
//! `build_stacktrace` (`:1197-1297`) then renders that innermost frame with
//! `erts_build_mfa_item` (`../../erts/emulator/beam/utils.c:3430-3457`), whose
//! `Location` argument is *appended to* the `[{error_info, Map}]` list it was
//! given — every **outer** frame gets the arity in its `A` slot and no extension.
//!
//! Measured on the system oracle (`erl` 29.1.2 / erts-17.1.1), compiled module:
//!
//! ```text
//! catch hd(1)                     -> {erlang,hd,[1],[{error_info,#{module => erl_erts_errors}}]}
//! catch maps:get(k,#{})           -> {erlang,map_get,[k,#{}],[{error_info,#{module => erl_stdlib_errors}}]}
//! catch records:get(1,2)          -> {records,get,[1,2],[]}
//! catch lists:max([])             -> {lists,max,[[]],[{file,"lists.erl"},{line,638}]}
//! ```
//!
//! The last row is the shape rule below: `lists:max/1` is a **stdlib fast path**
//! standing in for the `lists.erl` body, so the oracle's item is the **source**
//! function with a source location and **no** extension. This runtime reproduces
//! `{lists,max,[[]],[]}` for it (same module, function and real argument list; the
//! location is the honest "no extension" form — see the residuals).
//!
//! The engine's raise snapshot is `(context, func, pc)` metadata, so its cooked
//! items can only ever carry an **arity** in the `A` slot (see [`super::stacktrace`])
//! — the BIF frame with its real argument list and its `error_info` location is
//! exactly the one item the snapshot cannot produce, so the failing site
//! synthesizes it and publishes it as the raise's own **raw-trace word**, the same
//! protocol [`super::callee`] uses for an `undef`:
//! [`Machine::raise_bif_error_with_head`] arms the one-shot `pending_callee_head`
//! latch immediately before the `throw_with_trace` that lands it, and
//! `build_stacktrace` conses the item in front of the snapshot's frames.
//!
//! # The two entry points (one carve)
//!
//! A BIF failure reaches this module through **two register shapes**, which is why
//! there are two entry points over one shared [`carve`]:
//!
//! * [`bif_head_item`] — the `bif`/`gc_bif` instruction, whose arguments are its own
//!   [`Operand`]s;
//! * [`bif_head_item_regs`] — every `call_ext`-shaped site (`call_ext*`, the
//!   `apply`/`apply_target` route, `send` and the local-callee seam), whose
//!   arguments are the registers `x(0..arity)` and whose **arity is the import
//!   table's declared one**, not necessarily the instruction's own (the
//!   `call_ext 3 erlang:apply/3` split): C's
//!   `make_arglist(c_p, reg, bif_mfa->arity)`
//!   (`../../erts/emulator/beam/beam_common.c:1062`), where `bif_mfa` is the
//!   export's `info.mfa`, never the call site's operand.
//!
//! Both go through the **same** [`carve`], so the layout, the sizing and the
//! reserve → read → write ordering are defined once and the two shapes cannot
//! drift.
//!
//! **Never a fabricated frame.** The item is built only at a real BIF failure,
//! from that instruction's own `(module, function)` import and its own **real**
//! argument operands, and only for a raise that can actually land (a live catch
//! frame). Anything else keeps today's behaviour — an unmodified raise.
//!
//! # Rooting
//!
//! The whole item is **one** rooted reservation ([`crate::terms::reserve_slack`]),
//! carved children-before-parents, so nothing the item names can be relocated out
//! from under it. The argument operands are read **after** the reservation (from
//! the machine's post-collection registers), so no `Eterm` is ever held across the
//! allocating door.
//!
//! # Residuals (each a documented omission, never a wrong value)
//!
//! * The **`cause` variant** of the extension (`#{cause => Value, module =>
//!   FormatModule}`, C's `EXF_HAS_EXT_INFO` arm, :996-1004) is not rendered: the
//!   engine emits `#{module => FormatModule}` only. `ets:lookup/2`,
//!   `erlang:register/2`, `erlang:process_flag/2,3`, `erlang:start_timer/3,4` and
//!   the bit-syntax BIFs carry a `cause` in the oracle.
//! * A BIF that **published its own ext info** would still render
//!   `#{module => FormatModule}` here: this carve builds the item from the BIF's
//!   own dispatch site and never reads a `Raise` payload, so it has no C
//!   `#{cause => Fvalue, module => …}` arm. No BIF in the tree publishes one —
//!   the only producer is `erlang:error/3`, which the [`is_non_bif_raise`] gate
//!   excludes from this path entirely; the *caller-frame* path renders a
//!   `Raise::error_info` payload verbatim instead (`stacktrace.rs`'s
//!   `raise_error_info` latch).
//! * A **stdlib fast path** carries an **empty** `Location` where the oracle names
//!   the Erlang source function's `{file,_}`/`{line,_}` (e.g. `lists:max/1` at
//!   `lists.erl:638`). Nothing engine-side can supply it: a source location is a
//!   property of a loaded Erlang function (a `beam_loader` line-table entry keyed by
//!   `(func, pc)`), and a fast path has no code — the host that recognized the name
//!   is the only party that could. Module, function and the real argument list are
//!   right; the absent extension is deliberate (see the shape rule above), never a
//!   fabricated classification.

use beam_loader::{Atom, Module, Operand};
use beam_term::{make_atom, Eterm, NIL};

use beam_common::{BeamError, Result};
use crate::context::{BifKind, ExecutionContext};
use crate::machine::{namespace_atom, Machine};
use crate::operand::read_operand;
use crate::terms::{self, Sink};

/// The `{Module, Function, ArgList, Location}` item's own 4-element tuple:
/// `1` `arityval` header word + `4` elements.
const ITEM_WORDS: usize = 5;

/// What the `{error_info, Map}` location extension costs **on top of** a `NIL`
/// location: the map (`map_words_capacity(1)`, one pair), the 2-tuple
/// `{error_info, Map}` (`1` header + `2` elements) and the one cons cell of the
/// `[{error_info,Map}]` list (`2`). The base location is `NIL`, which has no block,
/// so it costs nothing.
const EXTENSION_WORDS: usize = beam_term::map::map_words_capacity(1) + 3 + 2;

/// The `format_error/2` module a failing BIF's `error_info` names — C's **closed**
/// table in `save_stacktrace` (`../../erts/emulator/beam/beam_common.c:937-1006`).
///
/// Every `format_error/2` implementation for a BIF lives in one of three modules,
/// deliberately, "to allow them to be removed from application systems with tight
/// storage constraints" (:930-935): the `erlang`/`erts_internal`/`persistent_term`/
/// `atomics`/`counters` BIF family formats through `erl_erts_errors`, the `code`/`os`
/// kernel BIFs through `erl_kernel_errors`, and the stdlib BIFs (`binary`, `ets`,
/// `lists`, `maps`, `math`, `re`, `unicode`) through `erl_stdlib_errors`.
///
/// `None` — **no `error_info` in the item's location at all** — for every module
/// outside the table (C's `default:` arm, :1000-1004). That arm is real and
/// measured: `records:get/2` cooks `{records,get,[1,2],[]}`, and so do
/// `erts_debug:*`, `erl_ddll:*`, `prim_file:*` and
/// `net_kernel:dflag_unicode_io/1`.
///
/// Note that `lists`/`maps` appear here because a *BIF* may be named by them
/// (`erlang:map_get/2` is reached as `maps:get/2` from source), not because their
/// Erlang **functions** are BIFs: a raise out of `lists:max/1`'s or
/// `maps:update_with/3`'s own Erlang body is a **source** frame, not a BIF frame.
/// Such a call still reaches this module when a host serves it as a **stdlib fast
/// path** — but then [`extension_allowed`] drops the extension, so the item this
/// table would select for `lists`/`maps` is never attached to a fast-path raise.
pub(crate) fn format_module(bif_module: &str) -> Option<&'static str> {
    match bif_module {
        "atomics" | "counters" | "erlang" | "erts_internal" | "persistent_term" => {
            Some("erl_erts_errors")
        }
        "code" | "os" => Some("erl_kernel_errors"),
        "binary" | "ets" | "lists" | "maps" | "math" | "re" | "unicode" => {
            Some("erl_stdlib_errors")
        }
        _ => None,
    }
}

/// Whether this BIF failure takes C's `non_bif_stacktrace` arm — i.e. the raise's
/// innermost frame is the **caller's** function, so **no** BIF head item may be
/// synthesized (`../../erts/emulator/beam/beam_common.c:911-922`).
///
/// The three exceptions are the BIFs that *are* the raise: `erlang:error/1,2,3`
/// (the reason the program chose), `erlang:exit/1` and `erlang:throw/1`. Their
/// frame identity and argument list are the ones the caller's own frame renders
/// (the `saved_args` shape), so prepending a `{erlang,error,[…]}` item would
/// be a frame the oracle never shows.
///
/// Every other `erlang` BIF — `hd/1`, `binary_to_atom/1`, `map_get/2`, … — **does**
/// get the head item, whatever its arity.
pub(crate) fn is_non_bif_raise(bif_module: &str, bif_function: &str, arity: u32) -> bool {
    bif_module == "erlang"
        && match bif_function {
            "error" => (1..=3).contains(&arity),
            "exit" | "throw" => arity == 1,
            _ => false,
        }
}

/// Whether this failure may attach the `[{error_info, #{module => FormatModule}}]`
/// location extension at all — the closed-form **shape rule** at the top of this
/// module.
///
/// `kind` is the host's answer for the failing name
/// ([`ExecutionContext::bif_kind`](crate::context::ExecutionContext::bif_kind)) and
/// `reason_is_function_clause` says whether the raise's reason is BARE
/// `function_clause`, read at the arming site from the `BeamError` the BIF door just
/// produced.
///
/// * A **stdlib fast path** gets no extension, whatever the reason: it stands in for
///   an Erlang source function, whose oracle frame is
///   `{M, F, Args, [{file, F}, {line, N}]}` — source location, no extension.
/// * A bare **`function_clause`** reason gets no extension even for a **declared**
///   BIF: C's only producer of that reason is `func_info`
///   (`emu/instrs.tab:1236`'s `i_func_info_IaaI`), which names its own frame with a
///   source location, and no declared BIF raises it — an extension there is always a
///   misclassification (typically a fast path on a host that has not overridden
///   [`ExecutionContext::bif_kind`](crate::context::ExecutionContext::bif_kind) yet).
/// * [`BifKind::Unknown`] is treated exactly as [`BifKind::Declared`], so a pre-seam
///   host differs from the old behaviour **only** on the `function_clause` arm.
fn extension_allowed(kind: BifKind, reason_is_function_clause: bool) -> bool {
    match kind {
        BifKind::StdlibFastPath => false,
        BifKind::Declared | BifKind::Unknown => !reason_is_function_clause,
    }
}

/// Whether `error`'s reason is the bare atom `function_clause` — C's
/// `func_info`-only reason (see [`extension_allowed`]).
///
/// Only the reason's **name** is read; a [`beam_common::Raise`]'s term payloads are
/// never touched (the caller classifies **before** the item's reservation, so no
/// `Eterm` is read across an allocating door). Both the `&'static str` form and the
/// owned-name form are covered, exactly as [`super::callee::is_unresolved_undef`]
/// does for `undef`.
fn is_function_clause_reason(error: &BeamError) -> bool {
    if error.as_atom() == Some("function_clause") {
        return true;
    }
    matches!(error, BeamError::Raise(raise) if raise.reason.as_atom() == Some("function_clause"))
}

/// The three atoms the `[{error_info, #{module => FormatModule}}]` extension needs,
/// or `None` when the BIF module has no format module or a name cannot be
/// resolved.
///
/// Every name goes through [`namespace_atom`] — the host's **live** namespace
/// first, the running module's load-time snapshot second — because none of
/// `error_info`, `module` or a `erl_*_errors` name is likely to be in a compiled
/// module's own atom chunk. An unnameable atom leaves the item **without** the
/// extension (never a substituted index, never a fabricated key), which is a
/// documented residual rather than a wrong term.
fn extension_atoms(
    ctx: &dyn ExecutionContext,
    module: &Module,
    bif_module: &str,
) -> Option<(beam_loader::Atom, beam_loader::Atom, beam_loader::Atom)> {
    let format = format_module(bif_module)?;
    Some((
        namespace_atom(ctx, module, "error_info")?,
        namespace_atom(ctx, module, "module")?,
        namespace_atom(ctx, module, format)?,
    ))
}

/// Where an entry point's **argument list** comes from.
///
/// The two shapes C itself distinguishes: a `bif`/`gc_bif` instruction's own
/// operands, and the registers of every `call_ext`-shaped site. [`carve`] is the
/// only consumer, so both shapes provably produce the same item layout.
#[derive(Clone, Copy)]
enum Args<'a> {
    /// The `bif`/`gc_bif` instruction's own argument operands.
    Operands(&'a [Operand]),
    /// The registers `x(0..arity)`, where `arity` is the **BIF's declared** arity
    /// (C's `bif_mfa->arity`, `../../erts/emulator/beam/beam_common.c:1062`).
    Registers(u32),
}

impl Args<'_> {
    /// How many arguments the item's `ArgList` holds — the count [`carve`] sizes the
    /// reservation from, and the arity [`is_non_bif_raise`] is asked about.
    fn len(self) -> usize {
        match self {
            Args::Operands(operands) => operands.len(),
            Args::Registers(arity) => arity as usize,
        }
    }

    /// Read argument `index` **after** the reservation. Both arms go through
    /// [`crate::operand::read_operand`], so an unreadable argument answers the same
    /// typed error either way and nothing is read twice.
    fn read(
        self,
        machine: &Machine,
        ctx: &mut dyn ExecutionContext,
        index: usize,
    ) -> Result<Eterm> {
        match self {
            Args::Operands(operands) => read_operand(machine, ctx, &operands[index]),
            Args::Registers(_) => read_operand(machine, ctx, &Operand::X(index as u32)),
        }
    }
}

/// The **one** carve of a `{Module, Function, ArgList, Location}` item, shared by
/// both entry points.
///
/// `item_atoms` is the `(module, function)` pair, already resolved to atom indices
/// (an atom is an immediate, so holding the two words across the reservation is
/// safe); `extension` names the `{error_info, #{module => FormatModule}}` location
/// (see [`extension_atoms`]) or `None` for the empty-list location.
///
/// The whole graph is **one** rooted reservation, sized from metadata only —
/// `2 * argc` words for the argument list (one cons cell per argument), `5` for the
/// item tuple, and [`EXTENSION_WORDS`] more when the location carries the
/// extension. Children are written before parents, in that order, and every
/// argument is read **after** the reservation (the frozen reserve → read → write
/// discipline: a moving collector may run under it).
///
/// An argument that cannot be read is **not** a runtime value (it comes from the
/// instruction or from the BIF's declared arity), so the honest answer is
/// `Ok(None)` — synthesize nothing and keep the raise as it was — never a
/// fabricated `[]` in that slot.
fn carve(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    module: &Module,
    item_atoms: (Atom, Atom),
    extension: Option<(Atom, Atom, Atom)>,
    args: Args<'_>,
) -> Result<Option<Eterm>> {
    let argc = args.len();
    let total = 2 * argc + ITEM_WORDS + extension.map_or(0, |_| EXTENSION_WORDS);
    // A host that cannot serve the reservation — a minimal embedding host with no
    // `alloc_words` at all, or a genuinely exhausted heap — must **not** have the
    // BIF's own raise replaced by this marshalling failure (the one outcome the
    // `Err(_) => None` decline below already guards for an unreadable operand). The
    // head item is a best-effort *extension* of the cooked trace, so failing to
    // build one DECLINES: the raise then lands with the pre-mechanism
    // (`raise_snapshot`-derived) trace, exactly the shape a raise naming no
    // nameable BIF produces. Propagating here would substitute
    // `unsupported_opcode: alloc_words` for the real reason, turning a correct
    // raise into a wrong one.
    let Ok(mut sink) = terms::reserve_slack(machine, ctx, total) else {
        return Ok(None);
    };
    // Children before parents, in the order `total` counts.
    let location = match extension {
        Some((error_info, key, value)) => {
            let map = sink.map(
                &[(make_atom(key), make_atom(value))],
                &module.atom_table,
            )?;
            let pair = sink.tuple(&[make_atom(error_info), map])?;
            sink.list(&[pair])?
        }
        None => NIL,
    };
    // Post-collection read: the BIF's real arguments, in order.
    let mut values = Vec::with_capacity(argc);
    for index in 0..argc {
        match args.read(machine, ctx, index) {
            Ok(value) => values.push(value),
            Err(_) => return Ok(None),
        }
    }
    let arg_list = sink.list(&values)?;
    Ok(Some(sink.tuple(&[
        make_atom(item_atoms.0),
        make_atom(item_atoms.1),
        arg_list,
        location,
    ])?))
}

/// The synthesized innermost cooked-stacktrace item of a raise coming out of the BIF
/// `bif_module:bif_function/arity`:
///
/// ```text
/// {Module, Function, ArgList, [{error_info, #{module => FormatModule}}]}
/// ```
///
/// with the empty-list `Location` when [`format_module`] does not name a format
/// module for `bif_module` **or** the shape rule ([`extension_allowed`]) drops the
/// extension — a stdlib fast path, or a bare `function_clause` reason.
///
/// This is the **`bif`/`gc_bif` instruction** shape: `args` are the instruction's own
/// argument operands (the very list the dispatch marshalled for the BIF), so the
/// item names the callee with its real arguments exactly as C's
/// `make_arglist(c_p, reg, bif_mfa->arity)` does. For the `call_ext`-shaped sites see
/// [`bif_head_item_regs`].
///
/// `error` is the failure the BIF door produced. Only its **reason name** is read
/// ([`is_function_clause_reason`]), and only to decide the extension; the raise
/// itself is landed by the caller. It is passed in rather than derived here because
/// the reason exists only at the arming site.
///
/// Returns `None` — today's behaviour, an unmodified raise — when any of:
///
/// * the failure takes C's `non_bif_stacktrace` arm ([`is_non_bif_raise`]);
/// * **no catch frame is live**, so the raise cannot land and the cooked trace is
///   never reached (the *uncaught* path derives its frames from the raise snapshot,
///   which this module deliberately does not touch);
/// * the BIF's `(module, function)` atom cannot be named ([`namespace_atom`]);
/// * an argument operand is unreadable (a malformed instruction, not a runtime
///   value) — nothing is fabricated in its place;
/// * the host cannot serve the item's reservation (no `alloc_words`, or an
///   exhausted heap) — a marshalling failure never replaces the BIF's own error.
pub(crate) fn bif_head_item(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    module: &Module,
    bif_module: &str,
    bif_function: &str,
    args: &[Operand],
    error: &BeamError,
) -> Result<Option<Eterm>> {
    build(
        machine,
        ctx,
        module,
        bif_module,
        bif_function,
        Args::Operands(args),
        error,
    )
}

/// [`bif_head_item`] for the **register-shaped** sites — every `call_ext`-shaped
/// route (`call_ext*`, the `apply`/`apply_target` seam, `send`, the local-callee
/// seam).
///
/// `arity` is the **BIF's declared arity** (the import table's, or the local callee's
/// own), never the instruction's operand: those two differ for the
/// `call_ext 3 erlang:apply/3` split, and C's `make_arglist(c_p, reg, bif_mfa->arity)`
/// uses the export's (`../../erts/emulator/beam/beam_common.c:1062`). The arguments
/// are the registers `x(0..arity)`, read after the reservation, and an arity beyond
/// the register file is a decline (`Ok(None)`), exactly like an unreadable operand.
pub(crate) fn bif_head_item_regs(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    module: &Module,
    bif_module: &str,
    bif_function: &str,
    arity: u32,
    error: &BeamError,
) -> Result<Option<Eterm>> {
    build(
        machine,
        ctx,
        module,
        bif_module,
        bif_function,
        Args::Registers(arity),
        error,
    )
}

/// The shared gate + atom resolution of the two entry points (see
/// [`bif_head_item`] for the declines, which are identical for both shapes).
fn build(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    module: &Module,
    bif_module: &str,
    bif_function: &str,
    args: Args<'_>,
    error: &BeamError,
) -> Result<Option<Eterm>> {
    let arity = args.len() as u32;
    if is_non_bif_raise(bif_module, bif_function, arity)
        || machine.catch_frame_depth() == 0
    {
        return Ok(None);
    }
    // The shape rule, decided **before** the reservation: it reads the host's
    // classification and the raise's reason *name* only, so no `Eterm` is held
    // across the allocating door.
    let attach = extension_allowed(ctx.bif_kind(bif_module, bif_function, arity), is_function_clause_reason(error));
    let (Some(module_atom), Some(function_atom)) = (
        namespace_atom(ctx, module, bif_module),
        namespace_atom(ctx, module, bif_function),
    ) else {
        return Ok(None);
    };
    // Resolved before the reservation: the three atom words are `Copy` indices and
    // can never be relocated. An extension the module cannot name — or one the shape
    // rule dropped — is simply absent, and then costs no words at all.
    let extension = if attach {
        extension_atoms(ctx, module, bif_module)
    } else {
        None
    };
    carve(
        machine,
        ctx,
        module,
        (module_atom, function_atom),
        extension,
        args,
    )
}

// `pub(crate)` so the sibling route suites (`tests/error_info.rs`) reuse the
// oracle's own item builder rather than growing a second copy that could drift.
#[cfg(test)]
pub(crate) mod tests;
