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
//! # The shape rule: the closed table decides, plus one reason gate
//!
//! The extension is a property of the **format-module table** ([`format_module`])
//! and of the raise's *reason*, never of how the name reached the BIF door. In
//! particular a `beam_bifs` **stdlib fast path** — a registry key `bif.tab` does
//! not declare, standing in for an Erlang source function (`lists:seq/3`,
//! `maps:to_list/1`, …) — keeps its module's extension exactly like a declared
//! BIF, because the OTP sources raise through `error_with_info/2`:
//!
//! ```text
//! error_with_info(Reason, Args) ->
//!     erlang:error(Reason, Args, [{error_info, #{module => erl_stdlib_errors}}]).
//! ```
//!
//! That is an **Erlang-level `erlang:error/3`**, not a BIF classification, so the
//! oracle's item for `lists:seq(1, 2, 0)` carries the extension (measured below) and
//! ours must too. Nothing distinguishes a fast path here: it is a raise out of the
//! module whose name the closed table already maps to a `format_error/2` owner.
//!
//! The rule is therefore closed-form:
//!
//! * [`format_module`] answers `None` — a module outside C's closed table gets
//!   **no `error_info` at all**, so its `Location` is the empty list
//!   (`records:get/2`, `prim_file:*`, `erts_debug:*`, …).
//! * the reason is the bare atom **`function_clause`** — no extension, whatever the
//!   module: C's only producer of that reason is `func_info`
//!   (`emu/instrs.tab`'s `i_func_info_IaaI`), which names **its own** frame with a
//!   source location and never attaches info. This is the arm that makes
//!   `lists:max([])` cook `{lists,max,[[]],[]}` — that fast path raises
//!   `function_clause`, so no extension is attached even though `lists` is in the
//!   table.
//! * otherwise — the extension.
//!
//! The oracle's `{file,_}`/`{line,_}` location is an honest residual (see the
//! module-level "Residuals" below): the engine has no "source location of a loaded
//! function" door, so a frame cooked here carries the real argument list and the
//! module's extension, but never a position.
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
//! catch lists:seq(1,2,0)          -> {lists,seq,[1,2,0],[{file,"lists.erl"},{line,541},
//!                                                  {error_info,#{module => erl_stdlib_errors}}]}
//! catch records:get(1,2)          -> {records,get,[1,2],[]}
//! catch lists:max([])             -> {lists,max,[[]],[{file,"lists.erl"},{line,638}]}
//! ```
//!
//! The `lists:seq/3` row is the one that fixes the rule. `lists:seq/3` is a
//! **stdlib fast path** (no `bif.tab` row) — the `lists.erl` body is what ran on the
//! oracle — yet its item carries the `erl_stdlib_errors` extension, because that
//! body asks for it through `error_with_info/2`. This runtime reproduces
//! `{lists,seq,[1,2,0],[{error_info,#{module => erl_stdlib_errors}}]}`: same module,
//! function, real argument list and extension; the source position is the honest
//! residual below. The `lists:max/1` row is the opposite shape, and its **reason**
//! (not its module) is what drops the extension: `function_clause` is only ever
//! produced by `func_info`, so this runtime cooks `{lists,max,[[]],[]}`.
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
//! The **cause** is the one value resolved before it: the host's
//! [`ExecutionContext::bif_error_cause`] may allocate on its own heap, so it is asked
//! first (with the arguments read at that moment) and must answer an **immediate** —
//! the only kind of word the carve can embed verbatim without re-rooting it. A boxed
//! answer is dropped (the one-pair form), never stored.
//!
//! # Residuals (each a documented omission, never a wrong value)
//!
//! * The **`cause` variant** of the extension (`#{cause => Value, module =>
//!   FormatModule}`, C's `EXF_HAS_EXT_INFO` arm, :992-1000) is rendered only when
//!   the **host** answers one through [`ExecutionContext::bif_error_cause`]; the
//!   trait default is `Ok(None)`, which keeps the one-pair
//!   `#{module => FormatModule}` form. Deciding a cause is a property of the layer
//!   that owns the BIF's own state (its table registry, its name registry), never of
//!   this crate: no engine-side door can tell `ets:lookup/2`'s `id` from
//!   `erlang:register/2`'s `none`. `ets:lookup/2`, `erlang:register/2`,
//!   `erlang:process_flag/2,3`, `erlang:start_timer/3,4` and the bit-syntax BIFs
//!   carry a `cause` in the oracle.
//! * A BIF that **published its own ext info** would still render
//!   `#{module => FormatModule}` here: this carve builds the item from the BIF's
//!   own dispatch site and never reads a `Raise` payload, so its `cause` can only
//!   come from the host seam above. No BIF in the tree publishes one through a
//!   raise — the only producer is `erlang:error/3`, which the [`is_non_bif_raise`]
//!   gate excludes from this path entirely; the *caller-frame* path renders a
//!   `Raise::error_info` payload verbatim instead (`stacktrace.rs`'s
//!   `raise_error_info` latch).
//! * Every item cooked here has an **empty** `{file,_}`/`{line,_}` position, where the
//!   oracle names the Erlang source function's (e.g. `lists:seq/3` at
//!   `lists.erl:541`). Nothing engine-side can supply it: a source location is a
//!   property of a loaded Erlang function (a `beam_loader` line-table entry keyed by
//!   `(func, pc)`), and a `bif.tab` BIF has no Erlang source at all while a stdlib
//!   fast path has no code — the host that recognized the name is the only party that
//!   could. Module, function, the real argument list and the module's `error_info`
//!   extension are right.

use beam_loader::{Atom, Module, Operand};
use beam_term::{make_atom, Eterm, NIL};

use beam_common::{BeamError, Result};
use crate::context::ExecutionContext;
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

/// [`EXTENSION_WORDS`] for C's **two-pair** form
/// (`#{cause => Fvalue, module => FormatModule}`, `beam_common.c:992-1000`): the map
/// is one pair larger, the tuple and the cons cell are unchanged.
const EXTENSION_WORDS_WITH_CAUSE: usize = beam_term::map::map_words_capacity(2) + 3 + 2;

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
/// (`erlang:map_get/2` is reached as `maps:get/2` from source), and also because a
/// host may serve a pure-Erlang function of theirs as a **stdlib fast path** whose
/// source body raises through `error_with_info/2` — an Erlang-level
/// `erlang:error/3` carrying `#{module => erl_stdlib_errors}`, exactly the extension
/// this table names. Both shapes keep the extension; only the bare
/// `function_clause` reason and a module outside the table drop it (the crate's
/// shape rule, `extension_allowed`).
///
/// `pub` (re-exported as `beam_engine::format_module`) because the `-eval`
/// tree-walking route in `beam_rt` cooks the same `{error_info, …}` extension
/// without a compiled module, and the closed table must have exactly **one**
/// definition in the workspace.
pub fn format_module(bif_module: &str) -> Option<&'static str> {
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
///
/// `pub` (re-exported as `beam_engine::is_non_bif_raise`) for the same reason as
/// [`format_module`]: `beam_rt`'s `-eval` route renders the same item without a
/// compiled module and must apply the same exclusion.
pub fn is_non_bif_raise(bif_module: &str, bif_function: &str, arity: u32) -> bool {
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
/// Two gates, and deliberately **no classification of the callable**: nothing here
/// asks whether the failing name is a declared `bif.tab` BIF or a host **stdlib fast
/// path** (see the module header for the measurement that settled it).
///
/// * [`format_module`] must name a `format_error/2` owner — a module outside C's
///   closed table gets no `error_info` at all (C's `default:` arm,
///   `../../erts/emulator/beam/beam_common.c:1000-1004`).
/// * the reason must not be the BARE atom `function_clause`, C's `func_info`-only
///   reason (see [`is_function_clause_reason`]): `func_info` names its own frame with
///   a source location and attaches no info, so an extension there is always a
///   misclassification.
///
/// A **fast path** therefore keeps the extension exactly like a declared BIF. OTP's
/// sources re-raise through `error_with_info/2` — an Erlang-level
/// `erlang:error(Reason, Args, [{error_info, #{module => erl_stdlib_errors}}])` — so
/// the oracle's item for `lists:seq(1, 2, 0)` carries
/// `#{module => erl_stdlib_errors}` and this runtime must reproduce it (measured row
/// at the top of this module).
fn extension_allowed(bif_module: &str, reason_is_function_clause: bool) -> bool {
    format_module(bif_module).is_some() && !reason_is_function_clause
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

/// The resolved atoms of the `[{error_info, #{cause => …, module => FormatModule}}]`
/// extension, or of its one-pair form when no cause rides along (see
/// [`extension_atoms`]).
#[derive(Clone, Copy)]
struct Extension {
    /// The `error_info` atom — the location entry's own key.
    error_info: Atom,
    /// The `module` atom — the pair naming the `format_error/2` owner.
    module_key: Atom,
    /// The `format_error/2` owner itself.
    format: Atom,
    /// The `cause` atom, `Some` **iff** a cause value is attached: C's
    /// `EXF_HAS_EXT_INFO` arm additionally emits `cause => c_p->fvalue`
    /// (`../../erts/emulator/beam/beam_common.c:992-1000`). `None` keeps the one-pair
    /// `#{module => FormatModule}` map (`:1001-1004`).
    cause_key: Option<Atom>,
}

impl Extension {
    /// The words the whole extension costs: [`EXTENSION_WORDS`], or
    /// [`EXTENSION_WORDS_WITH_CAUSE`] for the two-pair map. `cause_key` is `Some`
    /// **only** when a cause value exists, so sizing and writing cannot disagree.
    fn words(&self) -> usize {
        match self.cause_key {
            Some(_) => EXTENSION_WORDS_WITH_CAUSE,
            None => EXTENSION_WORDS,
        }
    }
}

/// The atoms the `[{error_info, …}]` extension needs, or `None` when the BIF module
/// has no format module or a name cannot be resolved.
///
/// Every name goes through [`namespace_atom`] — the host's **live** namespace
/// first, the running module's load-time snapshot second — because none of
/// `error_info`, `module` or a `erl_*_errors` name is likely to be in a compiled
/// module's own atom chunk. An unnameable atom leaves the item **without** the
/// extension (never a substituted index, never a fabricated key), which is a
/// documented residual rather than a wrong term.
///
/// `cause` is the host's cause value, if any: it makes the `cause` key part of the
/// answer, and an unnameable `cause` atom **drops the cause** (the one-pair form)
/// rather than the whole extension — a missing key must never fabricate one and must
/// never cost the module name either.
fn extension_atoms(
    ctx: &dyn ExecutionContext,
    module: &Module,
    bif_module: &str,
    cause: Option<Eterm>,
) -> Option<Extension> {
    let format = format_module(bif_module)?;
    let error_info = namespace_atom(ctx, module, "error_info")?;
    let module_key = namespace_atom(ctx, module, "module")?;
    let format = namespace_atom(ctx, module, format)?;
    let cause_key = match cause {
        Some(_) => namespace_atom(ctx, module, "cause"),
        None => None,
    };
    Some(Extension {
        error_info,
        module_key,
        format,
        cause_key,
    })
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
/// safe); `extension` names the `{error_info, …}` location (see [`extension_atoms`])
/// or `None` for the empty-list location; `cause` is the host's `cause` value when
/// [`extension`]'s [`cause_key`](Extension::cause_key) is `Some`.
///
/// The whole graph is **one** rooted reservation, sized before allocating —
/// `2 * argc` words for the argument list (one cons cell per argument), `5` for the
/// item tuple, [`Extension::words`] more when the location carries the extension, and
/// the [`terms::deep_copy_budget`] of every argument (so each argument's boxed
/// subgraph is **self-contained** inside the reservation and moves with the item).
/// Children are written before parents, in that order, and every argument is read
/// **after** the reservation (the frozen reserve → read → write discipline: a moving
/// collector may run under it).
///
/// An argument that cannot be read is **not** a runtime value (it comes from the
/// instruction or from the BIF's declared arity), so the honest answer is
/// `Ok(None)` — synthesize nothing and keep the raise as it was — never a
/// fabricated `[]` in that slot. The same decline covers an argument whose subgraph
/// cannot be deep-copied into the reservation (a match context, or a sizing/copy
/// failure): the raise stays unmodified.
fn carve(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    module: &Module,
    item_atoms: (Atom, Atom),
    extension: Option<Extension>,
    cause: Option<Eterm>,
    args: Args<'_>,
) -> Result<Option<Eterm>> {
    let argc = args.len();
    // Sizing pass, before the reservation: the deep-copy budget must be part of the
    // single rooted reservation. The argument words read here are used only for their
    // size (an `Operand::Integer` read may materialize a bignum through the unrooted
    // seam, which never collects); they are discarded and re-read after the reserve.
    let mut copy_budget = 0usize;
    for index in 0..argc {
        let Ok(value) = args.read(machine, ctx, index) else {
            return Ok(None);
        };
        let Some(words) = terms::deep_copy_budget(value) else {
            return Ok(None);
        };
        copy_budget += words;
    }
    let total = 2 * argc + ITEM_WORDS + extension.map_or(0, |ext| ext.words()) + copy_budget;
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
        Some(ext) => {
            // C's `MAP2(hp, am_cause, fvalue, am_module, format_module)` writes the
            // two pairs (or `MAP1` the one), and `beam_term::map::write_map` is the
            // only map writer in the tree. It **sorts a flat map's atom keys by atom
            // index** — C's `erts_cmp_flatmap_keys` order — so the rendered order is
            // the table's index order, which on a table in `atom.names` order puts
            // `cause` (line 178) before `module` (line 471): the oracle's
            // `#{cause => …, module => …}`.
            let module_pair = (make_atom(ext.module_key), make_atom(ext.format));
            let (pairs, count) = match (ext.cause_key, cause) {
                (Some(key), Some(value)) => ([(make_atom(key), value), module_pair], 2),
                _ => ([module_pair, module_pair], 1),
            };
            let map = sink.map(&pairs[..count], &module.atom_table)?;
            let pair = sink.tuple(&[make_atom(ext.error_info), map])?;
            sink.list(&[pair])?
        }
        None => NIL,
    };
    // Post-collection read: the BIF's real arguments, in order. Each one is then
    // deep-copied into the reserved region so the `ArgList` it names is never left
    // pointing at a boxed operand the next collection relocates out from under it.
    let mut values = Vec::with_capacity(argc);
    for index in 0..argc {
        match args.read(machine, ctx, index) {
            Ok(value) => match terms::deep_copy(&mut sink, value) {
                Ok(copied) => values.push(copied),
                Err(_) => return Ok(None),
            },
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
/// extension — a bare `function_clause` reason.
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
    // The shape rule, decided **before** the reservation: it reads the module NAME and
    // the raise's reason NAME only, so no `Eterm` is held across the allocating door.
    let attach = extension_allowed(bif_module, is_function_clause_reason(error));
    let (Some(module_atom), Some(function_atom)) = (
        namespace_atom(ctx, module, bif_module),
        namespace_atom(ctx, module, bif_function),
    ) else {
        return Ok(None);
    };
    // The host's `cause` (C's `c_p->fvalue`) is resolved **before** the reservation:
    // the seam may allocate on the host's own heap, and the carve must be one
    // uninterrupted reserve → read → write. The arguments it is asked about are read
    // here, from the machine's pre-collection registers; the carve re-reads them
    // afterwards for the item's `A` slot (`Args::read` is total and cheap).
    let cause = if attach {
        error_cause(machine, ctx, bif_module, bif_function, args)
    } else {
        None
    };
    // Resolved before the reservation: the atom words are `Copy` indices and can never
    // be relocated — an immediate `cause` value neither. An extension the module
    // cannot name — or one the shape rule dropped — is simply absent, and then costs
    // no words at all.
    let extension = if attach {
        extension_atoms(ctx, module, bif_module, cause)
    } else {
        None
    };
    carve(
        machine,
        ctx,
        module,
        (module_atom, function_atom),
        extension,
        cause,
        args,
    )
}

/// Resolve the host's `cause` value for `bif_module:bif_function(args…)` — C's
/// `c_p->fvalue` when the BIF fails with `BADARG | EXF_HAS_EXT_INFO`
/// (`../../erts/emulator/beam/beam_common.c:992-1000`) — through
/// [`ExecutionContext::bif_error_cause`].
///
/// Called **before** the item's reservation, because the seam may allocate on the
/// host's heap and the carve must stay one reserve → read → write sequence.
///
/// Every way of not getting a usable cause **declines** to the one-pair
/// `#{module => FormatModule}` form instead of failing the item, because this value is
/// decoration on a trace that is already being raised:
///
/// * the host answers `Ok(None)` (the trait default: "no cause to attach");
/// * the host answers `Err` — marshalling the decoration must never replace the BIF's
///   own reason;
/// * an argument operand is unreadable (a malformed instruction, not a runtime value);
/// * the answer is not an **immediate**. The word is embedded verbatim into the map
///   and is never re-read or re-rooted, so a boxed word would point into a heap the
///   carve's own reservation may relocate under it (see the seam's stability
///   contract). An unrootable answer is dropped, never written.
fn error_cause(
    machine: &Machine,
    ctx: &mut dyn ExecutionContext,
    bif_module: &str,
    bif_function: &str,
    args: Args<'_>,
) -> Option<Eterm> {
    let argc = args.len();
    let mut values = Vec::with_capacity(argc);
    for index in 0..argc {
        values.push(args.read(machine, ctx, index).ok()?);
    }
    let cause = ctx.bif_error_cause(bif_module, bif_function, &values).ok().flatten()?;
    beam_term::is_immediate(cause).then_some(cause)
}

// `pub(crate)` so the sibling route suites (`tests/error_info.rs`) reuse the
// oracle's own item builder rather than growing a second copy that could drift.
#[cfg(test)]
pub(crate) mod tests;
