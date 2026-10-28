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
//! ```
//!
//! The engine's raise snapshot is `(context, func, pc)` metadata, so its cooked
//! items can only ever carry an **arity** in the `A` slot (see [`super::stacktrace`])
//! — the BIF frame with its real argument list and its `error_info` location is
//! exactly the one item the snapshot cannot produce, so the BIF dispatch site
//! synthesizes it and publishes it as the raise's own **raw-trace word**, the same
//! protocol [`super::callee`] uses for an `undef`:
//! [`Machine::raise_bif_error_with_head`] arms the one-shot `pending_callee_head`
//! latch immediately before the `throw_with_trace` that lands it, and
//! `build_stacktrace` conses the item in front of the snapshot's frames.
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
//! * A raise that **publishes its own ext info** ([`beam_common::Raise::error_info`])
//!   is likewise rendered `#{module => FormatModule}` rather than verbatim: the
//!   payload is an unrooted `Eterm` inside the `BeamError`, and carrying it across
//!   the reservation is the dangling-pointer hazard the rooting law forbids. The
//!   verbatim form needs a machine latch for the payload (SLICE B1's
//!   `pending_error_info`/`raise_error_info`), and no BIF raise in the tree sets
//!   the payload today — the only producer is `erlang:error/3`, which the
//!   [`is_non_bif_raise`] gate excludes from this path entirely.

use beam_loader::{Module, Operand};
use beam_term::{make_atom, Eterm, NIL};

use beam_common::Result;
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
/// `maps:update_with/3`'s own Erlang body is a **source** frame, not a BIF frame,
/// and never comes through this module.
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
/// (SLICE B1's `saved_args` shape), so prepending a `{erlang,error,[…]}` item would
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

/// The synthesized innermost cooked-stacktrace item of a raise coming out of the BIF
/// `bif_module:bif_function/arity`:
///
/// ```text
/// {Module, Function, ArgList, [{error_info, #{module => FormatModule}}]}
/// ```
///
/// with the empty-list `Location` when [`format_module`] does not name a format
/// module for `bif_module`.
///
/// `args` are the **instruction's own** argument operands (the very list the
/// dispatch marshalled for the BIF), so the item names the callee with its real
/// arguments exactly as C's `make_arglist(c_p, reg, bif_mfa->arity)` does. They are
/// read **after** the one rooted reservation, honoring the frozen
/// reserve → read → write ordering (a moving collector may run under it).
///
/// Returns `None` — today's behaviour, an unmodified raise — when any of:
///
/// * the failure takes C's `non_bif_stacktrace` arm ([`is_non_bif_raise`]);
/// * **no catch frame is live**, so the raise cannot land and the cooked trace is
///   never reached (the *uncaught* path derives its frames from the raise snapshot,
///   which this module deliberately does not touch);
/// * the BIF's `(module, function)` atom cannot be named ([`namespace_atom`]);
/// * an argument operand is unreadable (a malformed instruction, not a runtime
///   value) — nothing is fabricated in its place.
pub(crate) fn bif_head_item(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    module: &Module,
    bif_module: &str,
    bif_function: &str,
    args: &[Operand],
) -> Result<Option<Eterm>> {
    if is_non_bif_raise(bif_module, bif_function, args.len() as u32)
        || machine.catch_frame_depth() == 0
    {
        return Ok(None);
    }
    let (Some(module_atom), Some(function_atom)) = (
        namespace_atom(ctx, module, bif_module),
        namespace_atom(ctx, module, bif_function),
    ) else {
        return Ok(None);
    };
    // Resolved before the reservation: the three atom words are `Copy` indices and
    // can never be relocated. An extension the module cannot name is simply absent.
    let extension = extension_atoms(ctx, module, bif_module);
    // The whole graph in ONE rooted reservation, sized from metadata only:
    // `2 * arity` words for the argument list (one cons cell per argument), `5` for
    // the item tuple, and `EXTENSION_WORDS` more when the location carries the
    // `{error_info, Map}` extension (the map, the pair and its cons cell).
    let total = 2 * args.len() + ITEM_WORDS + extension.map_or(0, |_| EXTENSION_WORDS);
    let mut sink = terms::reserve_slack(machine, ctx, total)?;
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
    // Post-collection read: the BIF's real arguments, in order. An unreadable
    // operand is not a runtime value (`args` comes from the instruction), so the
    // honest answer is to synthesize nothing and keep the raise as it was — never to
    // replace the BIF's own error with a marshalling one.
    let mut values = Vec::with_capacity(args.len());
    for operand in args {
        match read_operand(machine, ctx, operand) {
            Ok(value) => values.push(value),
            Err(_) => return Ok(None),
        }
    }
    let arg_list = sink.list(&values)?;
    Ok(Some(sink.tuple(&[
        make_atom(module_atom),
        make_atom(function_atom),
        arg_list,
        location,
    ])?))
}

#[cfg(test)]
mod tests;
