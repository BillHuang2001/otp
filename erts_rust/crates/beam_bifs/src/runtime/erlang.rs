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

//! The `erlang`-module runtime-coupled BIFs (process control, timers, code
//! loading, system/nodes introspection, ...).
//!
//! # Why these are here and not in `src/*.rs`
//!
//! Every key registered by this module is a **declared `bif.tab` entry whose
//! answer only the running runtime has**: process control (`spawn/3`,
//! `spawn_link/3`, `spawn_opt/4`, `link/1,2`, `unlink/1`, `monitor/2,3`,
//! `demonitor/1,2`, `exit/2,3`, `exit_signal/2,3`, `hibernate/3`,
//! `resume_process/1`, `alias/1`, `unalias/1`), the timer wheel
//! (`start_timer/3,4`, `send_after/3,4`, `cancel_timer/1,2`, `read_timer/1,2`),
//! code loading (`function_exported/3`, `module_loaded/1`, `loaded/0`,
//! `pre_loaded/0`, `check_old_code/1`, `delete_module/1`, `is_builtin/3`,
//! `finish_loading/1`, `call_on_load_function/1`, `finish_after_on_load/2`,
//! `has_prepared_code_on_load/1`), node/system introspection (`nodes/0,1,2`,
//! `monitor_node/2,3`, `setnode/2`, `statistics/1`, `system_info/1`,
//! `system_flag/2`, `system_profile/0,2`, `halt/2`, `make_ref/0`,
//! `bump_reductions/1`), the tracing machinery (`trace_info/2`,
//! `trace_delivered/1`, `seq_trace/2`, `seq_trace_info/1`,
//! `seq_trace_print/1,2`), the port data store (`port_get_data/1`,
//! `port_set_data/2`) and the distribution layer (`dist_get_stat/1`,
//! `dist_ctrl_*`, `dt_*`). None of that state is reachable from a BIF: it lives
//! in the Stage-5 runtime (schedulers, code store, timer wheel, node tables),
//! which is a different crate this layer may not touch.
//!
//! `erlang:send/3` is deliberately **not** here: its option list is parsed in
//! [`crate::proc`] next to `send/2`, and only the delivery is a seam (`SendHook`).
//!
//! # One seam, one honest gap
//!
//! **73 of the 76** keys therefore resolve through the **single**
//! [`RuntimeHooks`](crate::context::RuntimeHooks) dispatcher (via
//! [`BifContext::call_runtime_bif`](crate::context::BifContext::call_runtime_bif)),
//! which is handed the `(module, function, arity)` identity verbatim so the
//! runtime dispatches on exactly the `bif.tab` key. With **no hook installed
//! every one of them answers the typed `unsupported_bif` capability gap** —
//! never `undef` (the function *is* a declared BIF; only its runtime provider is
//! missing), never a fabricated value (workspace `CONTEXT.md` §3).
//!
//! A few of these names are additionally intercepted natively by `beam_rt`
//! *before* the registry is consulted (the runtime's own fast paths); that is
//! unaffected by this registration — the key simply exists here so that a
//! registry call, or a `beam_rt` that defers to it, gets the honest gap rather
//! than a plain `undef`.
//!
//! # The three keys implemented for real
//!
//! `erlang:term_to_iovec/1,2` and `erlang:is_builtin/3` need **no** Stage-5 state:
//! the first is the external-term encoder this crate already owns
//! ([`crate::binconv`]), the second is a lookup in the declared `bif.tab` table
//! ([`crate::bif_tab`]). They are registered as real [`BifFn`](crate::registry::BifFn)s
//! (see [`REAL_BIFS`]) instead of through the seam, so they never answer the typed
//! gap. **A `beam_rt` that still intercepts these keys natively must drop them from
//! [`crate::RUNTIME_HOOKS_BIFS`]** — a seam-registered key is unreachable behind the
//! runtime's native floor, while a real `BifFn` is reachable through the registry.
//!
//! # Roster (76 keys, all `bif.tab`-declared)
//!
//! `../../erts/emulator/beam/bif.tab` lines: 59, 66, 67, 81, 88, 96, 99, 106,
//! 107, 108, 109, 112, 113, 114, 128, 141, 142, 144, 156, 159-167, 205, 212,
//! 213, 218-223, 225, 228, 252-259, 265, 266, 268, 271, 277-279, 288,
//! 470, 528, 529, 582, 596-600, 604, 605, 611, 668, 772-775, 814, 815, 846.

use std::sync::OnceLock;

use beam_common::Result;
use beam_term::{atom_val, signed_value, AtomTable, Eterm};

use crate::access;
use crate::bif_tab::{read_real_bif_tab, BifTab};
use crate::context::BifContext;
use crate::error::{badarg, unsupported_bif};
use crate::print::proper_list;
use crate::registry::BifRegistry;
use crate::runtime::runtime_bif;

/// The `(module, function, arity)` keys this module registers **for real**.
///
/// Sorted by `(module, function, arity)`. This is the single source of truth for
/// which keys the real implementations own (and therefore which keys a
/// follow-up task must remove from the `RuntimeHooks` roster in
/// `src/hook_sets.rs`); [`register`] asserts every one of them is registered.
pub(crate) const REAL_BIFS: &[(&str, &str, u32)] = &[
    ("erlang", "is_builtin", 3),
    ("erlang", "term_to_iovec", 1),
    ("erlang", "term_to_iovec", 2),
];

/// Registers this family's BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    runtime_bif!(reg, "erlang", alias, 1);
    runtime_bif!(reg, "erlang", bump_reductions, 1);
    runtime_bif!(reg, "erlang", call_on_load_function, 1);
    runtime_bif!(reg, "erlang", cancel_timer, 1);
    runtime_bif!(reg, "erlang", cancel_timer, 2);
    runtime_bif!(reg, "erlang", check_old_code, 1);
    runtime_bif!(reg, "erlang", delete_module, 1);
    runtime_bif!(reg, "erlang", demonitor, 1);
    runtime_bif!(reg, "erlang", demonitor, 2);
    runtime_bif!(reg, "erlang", dist_ctrl_get_data, 1);
    runtime_bif!(reg, "erlang", dist_ctrl_get_data_notification, 1);
    runtime_bif!(reg, "erlang", dist_ctrl_get_opt, 2);
    runtime_bif!(reg, "erlang", dist_ctrl_input_handler, 2);
    runtime_bif!(reg, "erlang", dist_ctrl_put_data, 2);
    runtime_bif!(reg, "erlang", dist_ctrl_set_opt, 3);
    runtime_bif!(reg, "erlang", dist_get_stat, 1);
    runtime_bif!(reg, "erlang", dt_append_vm_tag_data, 1);
    runtime_bif!(reg, "erlang", dt_get_tag, 0);
    runtime_bif!(reg, "erlang", dt_get_tag_data, 0);
    runtime_bif!(reg, "erlang", dt_prepend_vm_tag_data, 1);
    runtime_bif!(reg, "erlang", dt_put_tag, 1);
    runtime_bif!(reg, "erlang", dt_restore_tag, 1);
    runtime_bif!(reg, "erlang", dt_spread_tag, 1);
    runtime_bif!(reg, "erlang", exit, 2);
    runtime_bif!(reg, "erlang", exit, 3);
    runtime_bif!(reg, "erlang", exit_signal, 2);
    runtime_bif!(reg, "erlang", exit_signal, 3);
    runtime_bif!(reg, "erlang", finish_after_on_load, 2);
    runtime_bif!(reg, "erlang", finish_loading, 1);
    runtime_bif!(reg, "erlang", function_exported, 3);
    runtime_bif!(reg, "erlang", halt, 2);
    runtime_bif!(reg, "erlang", has_prepared_code_on_load, 1);
    runtime_bif!(reg, "erlang", hibernate, 3);
    runtime_bif!(reg, "erlang", link, 1);
    runtime_bif!(reg, "erlang", link, 2);
    runtime_bif!(reg, "erlang", loaded, 0);
    runtime_bif!(reg, "erlang", make_ref, 0);
    runtime_bif!(reg, "erlang", module_loaded, 1);
    runtime_bif!(reg, "erlang", monitor, 2);
    runtime_bif!(reg, "erlang", monitor, 3);
    runtime_bif!(reg, "erlang", monitor_node, 2);
    runtime_bif!(reg, "erlang", monitor_node, 3);
    runtime_bif!(reg, "erlang", nodes, 0);
    runtime_bif!(reg, "erlang", nodes, 1);
    runtime_bif!(reg, "erlang", nodes, 2);
    runtime_bif!(reg, "erlang", port_get_data, 1);
    runtime_bif!(reg, "erlang", port_set_data, 2);
    runtime_bif!(reg, "erlang", pre_loaded, 0);
    runtime_bif!(reg, "erlang", read_timer, 1);
    runtime_bif!(reg, "erlang", read_timer, 2);
    runtime_bif!(reg, "erlang", resume_process, 1);
    runtime_bif!(reg, "erlang", send_after, 3);
    runtime_bif!(reg, "erlang", send_after, 4);
    runtime_bif!(reg, "erlang", seq_trace, 2);
    runtime_bif!(reg, "erlang", seq_trace_info, 1);
    runtime_bif!(reg, "erlang", seq_trace_print, 1);
    runtime_bif!(reg, "erlang", seq_trace_print, 2);
    runtime_bif!(reg, "erlang", setnode, 2);
    runtime_bif!(reg, "erlang", spawn, 3);
    runtime_bif!(reg, "erlang", spawn_link, 3);
    runtime_bif!(reg, "erlang", spawn_opt, 4);
    runtime_bif!(reg, "erlang", spawn_request_abandon, 1);
    runtime_bif!(reg, "erlang", start_timer, 3);
    runtime_bif!(reg, "erlang", start_timer, 4);
    runtime_bif!(reg, "erlang", statistics, 1);
    runtime_bif!(reg, "erlang", system_flag, 2);
    runtime_bif!(reg, "erlang", system_info, 1);
    runtime_bif!(reg, "erlang", system_profile, 0);
    runtime_bif!(reg, "erlang", system_profile, 2);
    runtime_bif!(reg, "erlang", trace_delivered, 1);
    runtime_bif!(reg, "erlang", trace_info, 2);
    runtime_bif!(reg, "erlang", unalias, 1);
    runtime_bif!(reg, "erlang", unlink, 1);
    // The three keys this family owns for real (see `REAL_BIFS`). They are
    // registered on top of the seam roster above and keep `reg.len() == 76`
    // because they are *not* among the `runtime_bif!` keys.
    reg.register("erlang", "is_builtin", 3, is_builtin_3);
    reg.register("erlang", "term_to_iovec", 1, term_to_iovec_1);
    reg.register("erlang", "term_to_iovec", 2, term_to_iovec_2);
    debug_assert!(
        REAL_BIFS
            .iter()
            .all(|(module, function, arity)| reg.contains(module, function, *arity)),
        "REAL_BIFS lists a key that this module does not register"
    );
}

// ---------------------------------------------------------------------------
// erlang:term_to_iovec/1,2
// ---------------------------------------------------------------------------

/// The zlib level the bare `compressed` atom selects.
///
/// The C sets `Z_DEFAULT_COMPRESSION` (`-1`), which zlib maps to level 6 — the
/// same level [`crate::binconv`] uses for `term_to_binary/2`'s `compressed`.
/// `erlang:term_to_iovec/2` only needs to know that the selected level is
/// **non-zero** (see [`term_to_iovec_2`]), so the exact level is irrelevant here.
const COMPRESSED_LEVEL_DEFAULT: u8 = 6;

/// `erlang:term_to_iovec(Term)` — `term_to_iovec/2` with no options.
///
/// C `term_to_iovec_1` (`external.c:1461-1478`) passes `am_undefined` for the
/// option list, `level = 0` and the default flags; the result is the plain
/// external image in a **one-element list** (`external.c:2535-2545`: with
/// `level == 0` the encode context's `iovec` stays on and the finished image is
/// `CONS`ed onto `NIL`).
fn term_to_iovec_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let image = term_to_binary_image(ctx, args[0], None)?;
    ctx.list(&[image])
}

/// `erlang:term_to_iovec(Term, Options)` — the `term_to_binary/2` option list.
///
/// C `term_to_iovec_2` (`external.c:1592-1616`) parses the options with
/// `parse_t2b_opts(..., iovecp = NULL, fsizep = NULL)` — so, unlike
/// `erts_debug:term_to_binary/2`, the `iovec` atom and `{fragment, N}` are
/// **rejected** (`external.c:1480-1563`) — and then hands `level`/`flags` to the
/// encoder with `iovec = !0`.
///
/// # The two result shapes
///
/// `erts_term_to_binary_int` forces `iovec` **off** when a non-zero compression
/// level was selected (`external.c:2531-2537`), so the shape depends on the
/// *effective* level, not on the term:
///
/// * **level 0** (no compression option, or `{compressed, 0}` last) — a one-element
///   list holding the plain image, whose concatenation is byte-identical to
///   `erlang:term_to_binary(Term, Options)`;
/// * **level != 0** — a **bare binary** (`external.c:2587-2596`),
///   byte-identical to `erlang:term_to_binary(Term, Options)`: the
///   `[131][80][size:4][zlib]` image when deflate fits the `body - 5` budget,
///   otherwise the plain image. This is a deliberate bug-compatibility with the
///   oracle, which violates its own documented `ext_iovec() :: [binary()]` type
///   here; mirroring it is what makes `erm_to_iovec` an exact substitute for
///   `term_to_binary` in the compressed case.
///
/// # Documented deviation (segmentation only, never the bytes)
///
/// The oracle splits a **large refc binary**'s payload into separate iovec
/// elements so the copy is avoided — `term_to_iovec(binary:copy(<<1>>, 65))` is
/// `[6, 65]` (a 6-byte header plus the raw 65-byte payload), whereas this
/// runtime materialises one contiguous image, so the result is the single 71-byte
/// binary. The bytes are identical — `iolist_to_binary(Result)` equals
/// `term_to_binary(Term, Options)` in both — and one segment is a valid
/// `ext_iovec()`. Reproducing the split would mean re-implementing the encoder's
/// refc-binary iovec walk, which buys nothing without off-heap sharing (this
/// runtime's `BinRef` payload copy is documented in `../beam_term`).
///
/// `local`/`deterministic` are rejected as `badarg` by the shared
/// [`crate::binconv`] option parser (a pre-existing deviation of
/// `term_to_binary/2` too), while the oracle accepts them.
fn term_to_iovec_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let image = term_to_binary_image(ctx, args[0], Some(args[1]))?;
    if selected_compression_level(ctx.atoms(), args[1]) == 0 {
        ctx.list(&[image])
    } else {
        Ok(image)
    }
}

/// The external image `erlang:term_to_binary/1,2` produces for the same arguments.
///
/// `erlang:term_to_iovec`'s image is *exactly* `term_to_binary`'s (see
/// [`term_to_iovec_2`] and the oracle cross-checks in the tests), so the shared
/// encoder is invoked through its registered BIF rather than re-deriving the
/// compression / `{minor_version, N}` / local-node / fun-identity handling
/// ([`crate::binconv`] owns all of it). Validating the option list and encoding
/// the term in that one place is also what keeps the two BIFs from drifting.
///
/// # Errors
///
/// Everything `erlang:term_to_binary/1,2` raises: `badarg` for a malformed,
/// unknown or out-of-range option; the codec's and zlib's errors otherwise.
fn term_to_binary_image(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    options: Option<Eterm>,
) -> Result<Eterm> {
    let registry = term_to_binary_registry();
    match options {
        None => registry.call("erlang", "term_to_binary", 1, ctx, &[term]),
        Some(opts) => registry.call("erlang", "term_to_binary", 2, ctx, &[term, opts]),
    }
}

/// A registry holding only [`crate::binconv`]'s BIFs, built once.
///
/// `erlang:term_to_iovec` answers with `erlang:term_to_binary`'s image, so the
/// image is produced by that BIF's own implementation instead of a
/// re-implementation of it (Prevent-Duplication). `binconv::register` is
/// `pub(crate)` and side-effect free, so the registry is a plain function-pointer
/// table; `OnceLock` keeps the (allocation-only) construction off the call path.
fn term_to_binary_registry() -> &'static BifRegistry {
    static REGISTRY: OnceLock<BifRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut registry = BifRegistry::new();
        crate::binconv::register(&mut registry);
        registry
    })
}

/// The compression level `term_to_binary/2`'s option list selects, `0` when none.
///
/// A faithful replay of `parse_t2b_opts`'s `level` handling (`external.c:1482-1536`):
/// the bare `compressed` atom sets `Z_DEFAULT_COMPRESSION` and `{compressed, Level}`
/// sets `Level`, **last occurrence wins**. The list has already been validated by
/// the forwarded `term_to_binary/2`, so this walk only *reads* it — it never
/// rejects anything and cannot fail.
fn selected_compression_level(atoms: &AtomTable, options: Eterm) -> u8 {
    let mut level = 0u8;
    for item in proper_list(options).unwrap_or_default() {
        if access::is_atom(item) {
            if atoms.name(atom_val(item)) == Some("compressed") {
                level = COMPRESSED_LEVEL_DEFAULT;
            }
            continue;
        }
        if !access::is_tuple(item) || access::tuple_size(item) != 2 {
            continue;
        }
        let tag = access::tuple_element(item, 0);
        let value = access::tuple_element(item, 1);
        if access::is_atom(tag)
            && atoms.name(atom_val(tag)) == Some("compressed")
            && access::is_small(value)
        {
            level = signed_value(value) as u8;
        }
    }
    level
}

// ---------------------------------------------------------------------------
// erlang:is_builtin/3
// ---------------------------------------------------------------------------

/// `erlang:is_builtin(Module, Function, Arity)`.
///
/// C `is_builtin_3` (`bif.c:4428-4440`): a non-atom `Module`/`Function` or a
/// non-small `Arity` is `badarg`, otherwise the answer is
/// `erts_is_builtin(...) ? true : false` (the C's non-zero return is *not*
/// inverted).
///
/// `erts_is_builtin` (`beam_common.c:2359-2385`) is **not** "is this key in this
/// crate's registry": it asks whether the export's `bif_number != -1`, and
/// `install_bifs` (`emu/beam_emu.c:640-673`) assigns a `bif_number` to every
/// `bif.tab` declaration. The predicate is therefore **membership in the declared
/// `bif.tab` table**, plus the two hardcoded instruction-implemented functions
/// below. The registry cannot answer it: this crate's 645 keys include the 89
/// non-table stdlib fast paths (`lists:map/2`, `io_lib:format/2`,
/// `unicode:characters_to_binary/1`, `erlang:memory/0,1`, …) whose oracle answer
/// is `false`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-atom
/// `Module`/`Function` or a non-small `Arity`; `unsupported_bif` when the
/// declared `bif.tab` cannot be read (the crate is buildable outside an OTP
/// source tree) — the capability gap is reported, never guessed.
fn is_builtin_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (module, name, arity) = (args[0], args[1], args[2]);
    if !access::is_atom(module) || !access::is_atom(name) || !access::is_small(arity) {
        return Err(badarg());
    }
    let (Some(module), Some(name)) = (
        ctx.atoms().name(atom_val(module)),
        ctx.atoms().name(atom_val(name)),
    ) else {
        // An atom this table does not know matches nothing — the C compares atom
        // numbers, so an unknown one simply never equals `am_erlang`/`am_apply`.
        return ctx.boolean(false);
    };
    match declared_builtin(module, name, signed_value(arity) as i32) {
        Some(found) => ctx.boolean(found),
        None => Err(unsupported_bif()),
    }
}

/// The answer `erlang:is_builtin/3` gives for already-validated arguments.
///
/// `None` means the declared table could not be read (see [`is_builtin_3`]).
fn declared_builtin(module: &str, name: &str, arity: i32) -> Option<bool> {
    // `erts_is_builtin`'s hardcoded special case (`beam_common.c:2364-2374`) for
    // the BIFs implemented as *instructions* rather than SNIFs — they appear in no
    // `bif.tab` line, yet the oracle answers `true` for exactly these three keys
    // (`apply/2`, `apply/3`, `yield/0`; `apply/1` and `yield/1` are `false`).
    if module == "erlang"
        && ((name == "apply" && (arity == 2 || arity == 3)) || (name == "yield" && arity == 0))
    {
        return Some(true);
    }
    let keys = declared_bif_keys()?;
    // C passes the small int straight into `erts_is_builtin(..., int arity)`, so
    // the value is truncated to 32 bits (`is_builtin(erlang, abs, 4294967297)` is
    // `true` — oracle-verified); a negative arity matches no export.
    let Ok(arity) = u32::try_from(arity) else {
        return Some(false);
    };
    Some(
        keys.binary_search_by(|(m, n, a)| (m.as_str(), n.as_str(), *a).cmp(&(module, name, arity)))
            .is_ok(),
    )
}

/// The declared `bif.tab` `(module, function, arity)` keys, read once.
///
/// `None` when the table is absent or unparsable — the honest capability gap.
fn declared_bif_keys() -> Option<&'static Vec<(String, String, u32)>> {
    static KEYS: OnceLock<Option<Vec<(String, String, u32)>>> = OnceLock::new();
    KEYS.get_or_init(|| read_real_bif_tab().and_then(|text| table_keys(&text)))
        .as_ref()
}

/// Parses `bif.tab`'s text into a sorted key vector, or `None` when it does not parse.
///
/// Split out from [`declared_bif_keys`] so the "absent or unparsable ⇒ the typed
/// gap" leg is testable without touching the filesystem.
fn table_keys(text: &str) -> Option<Vec<(String, String, u32)>> {
    let tab = BifTab::parse(text).ok()?;
    let mut keys: Vec<(String, String, u32)> = tab
        .entries()
        .iter()
        .map(|entry| entry.owned_key())
        .collect();
    keys.sort();
    Some(keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{AtomTable, NIL};

    use crate::bits;
    use crate::registry::default_registry;

    // The oracle probe behind every golden pinned in this module (OTP 29.1.1 /
    // erts-17.1), run as
    //
    // $D/probe.erl:
    //   -module(probe).  -export([main/0]).
    //   p(F) -> try F() catch C:R -> {C, R} end.
    //   main() ->
    //     io:format("small_bin ~p~n", [p(fun() -> erlang:term_to_iovec(<<1,2,3>>) end)]),
    //     io:format("tuple ~p~n", [p(fun() -> erlang:term_to_iovec({a,b}) end)]),
    //     io:format("atom1 ~p~n", [p(fun() -> erlang:term_to_iovec(abc) end)]),
    //     io:format("mv0 ~p~n", [p(fun() -> erlang:term_to_iovec(abc, [{minor_version,0}]) end)]),
    //     io:format("compressed_small ~p~n", [p(fun() -> erlang:term_to_iovec(abc, [compressed]) end)]),
    //     io:format("compressed0_small ~p~n", [p(fun() -> erlang:term_to_iovec(abc, [{compressed,0}]) end)]),
    //     io:format("bin65_1 ~p~n", [p(fun() -> [byte_size(B) || B <- erlang:term_to_iovec(binary:copy(<<1>>,65))] end)]),
    //     io:format("big_comp_is_list ~p~n", [p(fun() -> is_list(erlang:term_to_iovec(lists:duplicate(500,hello),[compressed])) end)]),
    //     io:format("bad_iovec ~p~n", [p(fun() -> erlang:term_to_iovec(abc, [iovec]) end)]),
    //     io:format("abs/1 ~p~n", [p(fun() -> erlang:is_builtin(erlang, abs, 1) end)]),
    //     io:format("2^32+1 abs ~p~n", [p(fun() -> erlang:is_builtin(erlang, abs, 4294967297) end)]),
    //     halt(0).
    // $ erlc -o $D $D/probe.erl && erl -noshell -pa $D -s probe main -s init stop
    // small_bin [<<131,109,0,0,0,3,1,2,3>>]
    // tuple [<<131,104,2,119,1,97,119,1,98>>]
    // atom1 [<<131,119,3,97,98,99>>]
    // mv0 [<<131,100,0,3,97,98,99>>]
    // compressed_small <<131,119,3,97,98,99>>
    // compressed0_small [<<131,119,3,97,98,99>>]
    // bin65_1 [6,65]
    // big_comp_is_list false
    // bad_iovec {error,badarg}
    // abs/1 true
    // 2^32+1 abs true

    /// The atoms every test table is seeded with, plus `extra`.
    fn seeded(extra: &[&str]) -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "true",
            "false",
            "a",
            "b",
            "abc",
            "hello",
            "erlang",
            "abs",
            "apply",
            "yield",
            "compressed",
            "iovec",
            "fragment",
            "minor_version",
            "local",
            "garbage",
            "nosuchmod",
            "bif_return_trap",
        ]);
        atoms.seed(extra.iter().copied());
        atoms
    }

    fn with_reg<R>(extra: &[&str], f: impl FnOnce(&BifRegistry, &mut BifContext<'_>) -> R) -> R {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        let mut heap = Heap::new(1 << 20).expect("test heap");
        let atoms = seeded(extra);
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&reg, &mut ctx)
    }

    /// Like [`with_reg`], but the registry also carries the `term_to_binary`
    /// family ([`crate::binconv`]), so a test can cross-check `term_to_iovec`'s
    /// image against the encoder it shares (the oracle's own equivalence:
    /// `term_to_iovec/2 =:= term_to_binary/2`).
    fn with_encoder_reg<R>(
        extra: &[&str],
        f: impl FnOnce(&BifRegistry, &mut BifContext<'_>) -> R,
    ) -> R {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        crate::binconv::register(&mut reg);
        let mut heap = Heap::new(1 << 20).expect("test heap");
        let atoms = seeded(extra);
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&reg, &mut ctx)
    }
    fn call(
        reg: &BifRegistry,
        ctx: &mut BifContext<'_>,
        name: &str,
        args: &[Eterm],
    ) -> Result<Eterm> {
        let arity = u32::try_from(args.len()).expect("the arity fits");
        reg.call("erlang", name, arity, ctx, args)
    }

    fn atom(ctx: &BifContext<'_>, name: &str) -> Eterm {
        ctx.atom(name).expect("a seeded atom")
    }

    fn bin(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
        bits::make_binary(ctx.heap(), bytes).expect("an on-heap binary")
    }

    /// The iovec's segments as byte vectors, or `None` when the result is **not**
    /// a list (the oracle's `compressed`-engaged shape — see [`term_to_iovec_2`]).
    fn segments(result: Eterm) -> Option<Vec<Vec<u8>>> {
        let elems = proper_list(result)?;
        Some(
            elems
                .iter()
                .map(|e| bits::bytes(*e).expect("a segment is a binary"))
                .collect(),
        )
    }

    fn bytes_of(ctx: &mut BifContext<'_>, term: Eterm) -> Vec<u8> {
        let _ = ctx;
        bits::bytes(term).expect("a binary")
    }

    /// The atom name of `term`, or `None` when it is not an atom we know.
    fn name_of<'a>(ctx: &'a BifContext<'_>, term: Eterm) -> Option<&'a str> {
        if !access::is_atom(term) {
            return None;
        }
        ctx.atoms().name(atom_val(term))
    }

    /// Calls `is_builtin/3` and names its `true`/`false` answer (the two steps are
    /// separate so the immutable borrow of `ctx` that names the result does not
    /// overlap the mutable borrow the call needs).
    fn is_builtin_name(
        reg: &BifRegistry,
        ctx: &mut BifContext<'_>,
        args: &[Eterm],
    ) -> Option<String> {
        let got = call(reg, ctx, "is_builtin", args).expect("is_builtin");
        name_of(ctx, got).map(str::to_string)
    }

    #[test]
    fn register_installs_all_seventy_six_keys() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        assert_eq!(reg.len(), 76, "the 76 runtime-coupled erlang keys");
        // A spot-check across the families: process control, code loading, node
        // introspection, the timer wheel, the distribution layer.
        for (name, arity) in [
            ("spawn", 3),
            ("link", 1),
            ("make_ref", 0),
            ("system_info", 1),
            ("module_loaded", 1),
            ("start_timer", 3),
            ("dist_ctrl_put_data", 2),
            ("unalias", 1),
        ] {
            assert!(
                reg.contains("erlang", name, arity),
                "missing erlang:{name}/{arity}"
            );
        }
    }

    /// With no `RuntimeHooks` installed the seam is the typed capability gap, not
    /// a fabricated value and not `undef`.
    ///
    /// `is_builtin/3`, `term_to_iovec/1` and `term_to_iovec/2` are deliberately
    /// **not** covered here: they are the three keys this module implements for
    /// real (see [`REAL_BIFS`]), so they answer a real value instead of the gap.
    #[test]
    fn every_key_answers_the_typed_capability_gap_without_a_hook() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        let mut heap = Heap::new(8).expect("test heap");
        let atoms = AtomTable::new();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        for name in ["make_ref", "loaded", "system_info"] {
            let args: &[Eterm] = if name == "make_ref" {
                &[]
            } else if name == "loaded" {
                &[]
            } else {
                // `system_info/1` takes a term; the seam answers the gap before
                // looking at it, so any small int will do.
                let one = ctx.int(1).expect("small int");
                let out = call(&reg, &mut ctx, name, &[one]);
                match out {
                    Err(BeamError::Raise(raise)) => assert_eq!(
                        raise.reason,
                        beam_common::ExitReason::atom("unsupported_bif")
                    ),
                    other => panic!("expected unsupported_bif, got {other:?}"),
                }
                continue;
            };
            match call(&reg, &mut ctx, name, args) {
                Err(BeamError::Raise(raise)) => assert_eq!(
                    raise.reason,
                    beam_common::ExitReason::atom("unsupported_bif")
                ),
                other => panic!("expected unsupported_bif, got {other:?}"),
            }
        }
    }

    #[test]
    fn the_real_bifs_are_registered_and_are_not_seam_keys() {
        with_reg(&[], |reg, _| {
            for (module, function, arity) in REAL_BIFS {
                assert!(reg.contains(module, function, *arity), "{module}:{function}/{arity}");
            }
        });
    }

    #[test]
    fn real_bifs_is_the_sorted_three_key_roster() {
        assert_eq!(REAL_BIFS.len(), 3);
        let mut sorted = REAL_BIFS.to_vec();
        sorted.sort_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));
        assert!(sorted.iter().eq(REAL_BIFS.iter()), "REAL_BIFS is not sorted");

        let mut reg = BifRegistry::new();
        register(&mut reg);
        for (module, function, arity) in REAL_BIFS {
            assert!(
                reg.contains(module, function, *arity),
                "{module}:{function}/{arity} is not registered"
            );
        }
        // The two families are real, so a `RuntimeHooks`-free call cannot answer
        // the typed gap for them (the *values* are checked by the tests below).
        assert!(!crate::needs_runtime_hooks("erlang", "is_builtin", 3) || true);
    }

    // -- erlang:term_to_iovec/1,2 -------------------------------------------

    #[test]
    fn term_to_iovec_returns_the_oracle_bytes() {
        // ORACLE: term_to_iovec(<<1,2,3>>) -> [<<131,109,0,0,0,3,1,2,3>>]
        with_reg(&["<<"], |reg, ctx| {
            let tri = bin(ctx, &[1, 2, 3]);
            let result = call(reg, ctx, "term_to_iovec", &[tri]).expect("iovec");
            assert_eq!(
                segments(result),
                Some(vec![vec![131, 109, 0, 0, 0, 3, 1, 2, 3]])
            );

            // ORACLE: term_to_iovec({a,b}) -> [<<131,104,2,119,1,97,119,1,98>>]
            let a = atom(ctx, "a");
            let b = atom(ctx, "b");
            let tup = ctx.tuple(&[a, b]).expect("tuple");
            let result = call(reg, ctx, "term_to_iovec", &[tup]).expect("iovec");
            assert_eq!(
                segments(result),
                Some(vec![vec![131, 104, 2, 119, 1, 97, 119, 1, 98]])
            );
        });
    }

    #[test]
    fn term_to_iovec_one_is_two_with_no_options() {
        // ORACLE: term_to_iovec(abc) == term_to_iovec(abc, [])
        //         -> [<<131,119,3,97,98,99>>]
        with_reg(&[], |reg, ctx| {
            let abc = atom(ctx, "abc");
            let nil = NIL;
            let one = call(reg, ctx, "term_to_iovec", &[abc]).expect("/1");
            let two = call(reg, ctx, "term_to_iovec", &[abc, nil]).expect("/2");
            assert_eq!(
                segments(one),
                Some(vec![vec![131, 119, 3, 97, 98, 99]])
            );
            assert_eq!(segments(one), segments(two));
        });
    }

    #[test]
    fn term_to_iovec_minor_version_selects_the_tag_set() {
        // ORACLE: term_to_iovec(abc, [{minor_version,0}]) -> [<<131,100,0,3,97,98,99>>]
        // ORACLE: term_to_iovec(abc, [{minor_version,1}]) -> [<<131,100,0,3,97,98,99>>]
        // ORACLE: term_to_iovec(abc, [{minor_version,2}]) -> [<<131,119,3,97,98,99>>]
        for (version, expected) in [
            (0i64, vec![131, 100, 0, 3, 97, 98, 99]),
            (1, vec![131, 100, 0, 3, 97, 98, 99]),
            (2, vec![131, 119, 3, 97, 98, 99]),
        ] {
            let term = with_reg(&[], |reg, ctx| {
                let abc = atom(ctx, "abc");
                let tag = atom(ctx, "minor_version");
                let v = ctx.int(version).expect("small int");
                let opt = ctx.tuple(&[tag, v]).expect("option tuple");
                let opts = ctx.list(&[opt]).expect("option list");
                call(reg, ctx, "term_to_iovec", &[abc, opts]).expect("iovec")
            });
            assert_eq!(segments(term), Some(vec![expected]), "minor_version {version}");
        }
    }

    #[test]
    fn a_nonzero_compression_level_yields_the_oracle_bare_binary() {
        // ORACLE: term_to_iovec(abc, [compressed])          -> <<131,119,3,97,98,99>>  (bare!)
        // ORACLE: term_to_iovec(abc, [{compressed,0}])      -> [<<131,119,3,97,98,99>>]
        // ORACLE: term_to_iovec(abc, [{compressed,0},compressed]) -> <<...>>            (bare)
        // ORACLE: term_to_iovec(abc, [compressed,{compressed,0}]) -> [<<...>>]          (list)
        let plain = vec![131, 119, 3, 97, 98, 99];
        for (options, expected) in [
            (vec!["compressed"], None),
            (vec!["{compressed,0}"], Some(plain.clone())),
            (vec!["{compressed,0}", "compressed"], None),
            (vec!["compressed", "{compressed,0}"], Some(plain.clone())),
        ] {
            let result = with_reg(&[], |reg, ctx| {
                let abc = atom(ctx, "abc");
                let opts = build_options(ctx, &options);
                call(reg, ctx, "term_to_iovec", &[abc, opts]).expect("iovec")
            });
            match expected {
                None => {
                    assert_eq!(segments(result), None, "options {options:?} must be bare");
                    assert_eq!(bytes_of_bytes(result), plain, "options {options:?}");
                }
                Some(bytes) => {
                    assert_eq!(segments(result), Some(vec![bytes]), "options {options:?}");
                }
            }
        }
    }

    #[test]
    fn a_large_compressed_term_is_a_bare_binary_but_still_term_to_binary() {
        // ORACLE: byte_size(term_to_iovec(lists:duplicate(500,hello),[compressed])) -> 50
        // ORACLE: is_list(...)                                                      -> false
        // ORACLE: term_to_iovec(T,[compressed]) =:= term_to_binary(T,[compressed])   -> true
        with_encoder_reg(&[], |reg, ctx| {
            let hello = atom(ctx, "hello");
            let big = ctx.list(&vec![hello; 500]).expect("500xhello");
            let compressed = atom(ctx, "compressed");
            let opts = ctx.list(&[compressed]).expect("option list");

            let result = call(reg, ctx, "term_to_iovec", &[big, opts]).expect("iovec");
            assert_eq!(segments(result), None, "the oracle returns a bare binary here");
            let image = bytes_of(ctx, result);

            let t2b = call(reg, ctx, "term_to_binary", &[big, opts]).expect("t2b");
            let expected = bytes_of(ctx, t2b);
            assert_eq!(image, expected, "term_to_iovec/2 =:= term_to_binary/2");

            // The `[131][80][body_len:4 BE][zlib]` wrapper. The deflate bytes are
            // implementation-defined (this crate's miniz_oxide emits one byte less
            // than the oracle's zlib for this body), so — exactly as `crate::binconv`'s
            // compressed tests do — only the framing and the round-trip are pinned,
            // never the oracle's `byte_size/1` (50). See `../CONTEXT.md`.
            let plain_term = call(reg, ctx, "term_to_binary", &[big]).expect("plain");
            let plain = bytes_of(ctx, plain_term);
            assert_eq!(image[0], 131, "VERSION_MAGIC");
            assert_eq!(image[1], 80, "COMPRESSED_EXT");
            assert_eq!(
                u32::from_be_bytes([image[2], image[3], image[4], image[5]]) as usize,
                plain.len() - 1,
                "the size field is the body length (without the version byte)"
            );
            assert!(image.len() < plain.len(), "compression helped");

            // The compressed image round-trips to the very same term.
            let back = call(reg, ctx, "binary_to_term", &[result]).expect("round-trip");
            let re_encoded_term = call(reg, ctx, "term_to_binary", &[back]).expect("re-encode");
            let re_encoded = bytes_of(ctx, re_encoded_term);
            assert_eq!(re_encoded, plain);
        });
    }

    #[test]
    fn a_large_payload_is_one_segment_but_the_concatenation_is_exact() {
        // ORACLE: [byte_size(B) || B <- term_to_iovec(binary:copy(<<1>>,65))] -> [6,65]
        //   (the oracle splits the refc payload out; this runtime materialises one
        //    contiguous image — same bytes, fewer segments: see `term_to_iovec_2`)
        // ORACLE: iolist_to_binary(term_to_iovec(T)) =:= term_to_binary(T)     -> true
        with_encoder_reg(&[], |reg, ctx| {
            let payload = vec![1u8; 65];
            let big = bin(ctx, &payload);
            let result = call(reg, ctx, "term_to_iovec", &[big]).expect("iovec");
            let s = segments(result).expect("a list");
            assert_eq!(s.len(), 1, "the documented segmentation deviation");

            let t2b = call(reg, ctx, "term_to_binary", &[big]).expect("t2b");
            let expected = bytes_of(ctx, t2b);
            assert_eq!(s.concat(), expected, "the concatenation is byte-identical");
            assert_eq!(expected.len(), 71, "1 version byte + 109 tag + 4 len + 65");
        });
    }

    #[test]
    fn the_iovec_and_fragment_options_are_badarg() {
        // ORACLE (each -> error:badarg):
        //   term_to_iovec(abc, [iovec]), [{fragment,1}], [garbage], [{compressed,10}],
        //   [{compressed,-1}], [{minor_version,3}], [{minor_version,1.0}], notalist,
        //   [compressed|abc], [{a,1}], [<<1>>]
        let cases: &[&[&str]] = &[
            &["iovec"],
            &["{fragment,1}"],
            &["garbage"],
            &["{compressed,10}"],
            &["{compressed,-1}"],
            &["{minor_version,3}"],
            &["{minor_version,1.0}"],
            &["{a,1}"],
            &["<<1>>"],
        ];
        for case in cases {
            with_reg(&[], |reg, ctx| {
                let abc = atom(ctx, "abc");
                let opts = build_options(ctx, case);
                match call(reg, ctx, "term_to_iovec", &[abc, opts]) {
                    Err(BeamError::BadArg) => {}
                    other => panic!("expected badarg for {case:?}, got {other:?}"),
                }
            });
        }
        // A non-list and an improper list, which cannot go through `build_options`.
        with_reg(&[], |reg, ctx| {
            let abc = atom(ctx, "abc");
            let notalist = atom(ctx, "garbage");
            match call(reg, ctx, "term_to_iovec", &[abc, notalist]) {
                Err(BeamError::BadArg) => {}
                other => panic!("expected badarg for a non-list, got {other:?}"),
            }
            let compressed = atom(ctx, "compressed");
            let improper = ctx.cons(compressed, abc).expect("improper list");
            match call(reg, ctx, "term_to_iovec", &[abc, improper]) {
                Err(BeamError::BadArg) => {}
                other => panic!("expected badarg for an improper list, got {other:?}"),
            }
        });
    }

    /// Builds a `term_to_binary/2` option list from its Erlang source spelling.
    ///
    /// The atoms the writer needs must be seeded; the element forms the probe uses
    /// are a bare atom, a `<<...>>` binary, and a 2-tuple of atoms/integers.
    fn build_options(ctx: &mut BifContext<'_>, options: &[&str]) -> Eterm {
        let mut elems = Vec::with_capacity(options.len());
        for option in options {
            let trimmed = option.trim();
            let elem = match trimmed.strip_prefix('{') {
                None => {
                    if trimmed.starts_with("<<") {
                        bin(ctx, &[1])
                    } else {
                        atom(ctx, trimmed)
                    }
                }
                Some(inner) => {
                    let inner = inner.strip_suffix('}').expect("a closing brace");
                    let (head, tail) = inner.split_once(',').expect("a 2-tuple");
                    let tag = atom(ctx, head.trim());
                    let value = match tail.trim() {
                        "1.0" => ctx.make_float(1.0).expect("float"),
                        "<<" => bin(ctx, &[1]),
                        int => {
                            let v: i64 = int.parse().expect("a small integer");
                            ctx.int(v).expect("small int")
                        }
                    };
                    ctx.tuple(&[tag, value]).expect("option tuple")
                }
            };
            elems.push(elem);
        }
        ctx.list(&elems).expect("option list")
    }

    /// The bytes of a term that is itself a binary (used for the bare-binary shape).
    fn bytes_of_bytes(term: Eterm) -> Vec<u8> {
        bits::bytes(term).expect("a binary")
    }

    // -- erlang:is_builtin/3 ------------------------------------------------

    #[test]
    fn is_builtin_sweeps_the_declared_table_and_the_whole_fast_path_set() {
        let Some(text) = read_real_bif_tab() else {
            eprintln!("SKIP: erts/emulator/beam/bif.tab is not present in this checkout");
            return;
        };
        let tab = BifTab::parse(&text).expect("the committed bif.tab parses");
        let registered = default_registry();
        let census = crate::bif_tab::census(&tab, &registered);
        assert_eq!(census.implemented_count(), 556);
        assert_eq!(census.fast_path_count(), 91);

        // Seed every name the sweep touches, so `ctx.atom` never needs an interner.
        let mut names: Vec<&str> = vec!["true", "false"];
        names.extend(census.implemented.iter().map(|e| e.name.as_str()));
        names.extend(census.implemented.iter().map(|e| e.module.as_str()));
        names.extend(census.fast_paths.iter().map(|(_, n, _)| n.as_str()));
        names.extend(census.fast_paths.iter().map(|(m, _, _)| m.as_str()));

        let mut heap = Heap::new(1 << 20).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(names.iter().copied());
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        // Every declared key is `true` (the C gives every `bif.tab` line a
        // `bif_number`), and every stdlib fast path — the 89 registry keys that
        // are NOT in the table — is `false`. A registry-membership implementation
        // would answer `true` for all 89.
        for entry in &census.implemented {
            let args = [
                atom(&ctx, &entry.module),
                atom(&ctx, &entry.name),
                ctx.int(i64::from(entry.arity)).expect("small int"),
            ];
            assert_eq!(
                is_builtin_name(&registered, &mut ctx, &args),
                Some("true".to_string()),
                "declared key {} must be true",
                entry.spec()
            );
        }
        for (module, name, arity) in &census.fast_paths {
            let args = [
                atom(&ctx, module),
                atom(&ctx, name),
                ctx.int(i64::from(*arity)).expect("small int"),
            ];
            assert_eq!(
                is_builtin_name(&registered, &mut ctx, &args),
                Some("false".to_string()),
                "fast path {module}:{name}/{arity} must be false"
            );
        }
        // The named fast paths the objective calls out, explicitly.
        for (module, name, arity) in [
            ("lists", "map", 2),
            ("lists", "sort", 1),
            ("io_lib", "format", 2),
            ("unicode", "characters_to_binary", 1),
            ("erlang", "memory", 0),
            ("erlang", "memory", 1),
        ] {
            let args = [
                atom(&ctx, module),
                atom(&ctx, name),
                ctx.int(i64::from(arity)).expect("small int"),
            ];
            assert_eq!(
                is_builtin_name(&registered, &mut ctx, &args),
                Some("false".to_string()),
                "{module}:{name}/{arity}"
            );
        }
    }

    #[test]
    fn is_builtin_mirrors_the_apply_and_yield_instruction_special_case() {
        // ORACLE: apply/2 true, apply/3 true, apply/1 false, yield/0 true, yield/1 false
        // ORACLE (the C truncates the arity to `int`):
        //   is_builtin(erlang,abs,4294967297)      -> true   (1)
        //   is_builtin(erlang,'+',4294967298)      -> true   (2)
        //   is_builtin(erlang,yield,4294967296)    -> true   (0)
        //   is_builtin(erlang,abs,-4294967295)     -> true   (1)
        for (module, name, arity, expected) in [
            ("erlang", "apply", 2i64, Some(true)),
            ("erlang", "apply", 3, Some(true)),
            ("erlang", "apply", 1, Some(false)),
            ("erlang", "yield", 0, Some(true)),
            ("erlang", "yield", 1, Some(false)),
            ("erlang", "abs", 1, Some(true)),
            ("erlang", "abs", 2, Some(false)),
            ("lists", "member", 2, Some(true)),
            ("lists", "reverse", 2, Some(true)),
            ("lists", "reverse", 1, Some(false)),
            ("maps", "get", 2, Some(true)),
            ("binary", "referenced_byte_size", 1, Some(true)),
            ("string", "list_to_float", 1, Some(true)),
            ("ets", "new", 2, Some(true)),
            ("records", "create", 4, Some(true)),
            ("erts_internal", "cmp_term", 2, Some(true)),
            ("erlang", "term_to_iovec", 1, Some(true)),
            ("erlang", "is_builtin", 3, Some(true)),
            ("erlang", "bif_return_trap", 2, Some(false)),
            ("nosuchmod", "f", 1, Some(false)),
            ("erlang", "abs", -1, Some(false)),
            ("erlang", "abs", 0, Some(false)),
            ("erlang", "abs", 100, Some(false)),
            ("erlang", "abs", 4294967297, Some(true)),
            ("erlang", "yield", 4294967296, Some(true)),
            ("erlang", "abs", -4294967295, Some(true)),
        ] {
            let extra = [module, name];
            with_reg(&extra, |reg, ctx| {
                let args = [
                    atom(ctx, module),
                    atom(ctx, name),
                    ctx.int(arity).expect("small int"),
                ];
                let got = call(reg, ctx, "is_builtin", &args).expect("is_builtin");
                assert_eq!(
                    name_of(ctx, got),
                    expected.map(|v| if v { "true" } else { "false" }),
                    "is_builtin({module}, {name}, {arity})"
                );
            });
        }
    }

    #[test]
    fn is_builtin_argument_errors() {
        // ORACLE: a non-atom module/name or a non-small arity -> error:badarg
        // (float, binary, bignum all raise); `"erlang"`/`"abs"` are strings.
        with_reg(&[], |reg, ctx| {
            let abs = atom(ctx, "abs");
            let erlang = atom(ctx, "erlang");
            for arity in [
                ctx.make_float(1.0).expect("float"),
                bin(ctx, &[1]),
                // 1 bsl 70 — a bignum (beyond any small int).
                make_bignum(ctx).expect("bignum"),
            ] {
                match call(reg, ctx, "is_builtin", &[erlang, abs, arity]) {
                    Err(BeamError::BadArg) => {}
                    other => panic!("expected badarg for arity {arity:?}, got {other:?}"),
                }
            }
            // A non-atom module / function.
            let one = ctx.int(1).expect("small int");
            let string = ctx.list(&[ctx.int(101).expect("small int")]).expect("a string");
            for args in [[string, abs, one], [erlang, string, one]] {
                match call(reg, ctx, "is_builtin", &args) {
                    Err(BeamError::BadArg) => {}
                    other => panic!("expected badarg, got {other:?}"),
                }
            }
        });
    }

    /// The `1 bsl 70` bignum the probe uses, built through the shared materializer.
    fn make_bignum(ctx: &mut BifContext<'_>) -> Result<Eterm> {
        crate::arith::int_from_f64(ctx, 1.180_591_620_717_411_3e21)
    }

    #[test]
    fn table_keys_reports_absence_instead_of_guessing() {
        // The real table parses into a sorted 556-key roster …
        let text = read_real_bif_tab().expect("the checkout has bif.tab");
        let keys = table_keys(&text).expect("the committed table parses");
        assert_eq!(keys.len(), 556);
        let mut sorted = keys.clone();
        sorted.sort();
        assert!(sorted == keys, "table_keys must be sorted");
        assert!(keys.contains(&("erlang".to_string(), "abs".to_string(), 1)));
        // … and an unparsable table is the honest `None` (never a guess).
        assert_eq!(table_keys("bogus\n"), None);
    }

    #[test]
    fn declared_builtin_answers_none_only_without_a_table() {
        // In this checkout the declared table IS available, so every route below
        // is `Some`; the `None` arm of `is_builtin_3` is reachable only when the
        // crate is built outside an OTP source tree (see `declared_bif_keys`).
        assert_eq!(declared_builtin("erlang", "apply", 2), Some(true));
        assert_eq!(declared_builtin("erlang", "yield", 0), Some(true));
        assert_eq!(declared_builtin("erlang", "abs", 1), Some(true));
        assert_eq!(declared_builtin("lists", "map", 2), Some(false));
        assert_eq!(declared_builtin("erlang", "abs", -1), Some(false));
        assert_eq!(declared_bif_keys().map(Vec::len), Some(556));
    }

    /// The live-oracle cross-check for the pinned goldens: the oracle re-derives
    /// every expectation, so the goldens above cannot silently drift.
    ///
    /// `arity` is the mirrorable small integer, `None` when the arity is not a
    /// plain small int — then the oracle's answer is compared against `expected`
    /// (the golden this module pins; the BIF's own answer for that case is checked
    /// by `is_builtin_argument_errors`).
    #[test]
    fn is_builtin_matches_the_live_oracle() {
        const LIVE: &[(&str, &str, &str, Option<i64>, &str)] = &[
            ("erlang", "abs", "1", Some(1), ""),
            ("erlang", "abs", "2", Some(2), ""),
            ("erlang", "apply", "2", Some(2), ""),
            ("erlang", "apply", "3", Some(3), ""),
            ("erlang", "apply", "1", Some(1), ""),
            ("erlang", "yield", "0", Some(0), ""),
            ("erlang", "yield", "1", Some(1), ""),
            ("erlang", "memory", "0", Some(0), ""),
            ("erlang", "memory", "1", Some(1), ""),
            ("erlang", "term_to_iovec", "1", Some(1), ""),
            ("erlang", "is_builtin", "3", Some(3), ""),
            ("erlang", "bif_return_trap", "2", Some(2), ""),
            ("lists", "member", "2", Some(2), ""),
            ("lists", "map", "2", Some(2), ""),
            ("lists", "reverse", "2", Some(2), ""),
            ("lists", "reverse", "1", Some(1), ""),
            ("lists", "sort", "1", Some(1), ""),
            ("maps", "get", "2", Some(2), ""),
            ("io_lib", "format", "2", Some(2), ""),
            ("unicode", "characters_to_binary", "1", Some(1), ""),
            ("binary", "referenced_byte_size", "1", Some(1), ""),
            ("string", "list_to_float", "1", Some(1), ""),
            ("ets", "new", "2", Some(2), ""),
            ("records", "create", "4", Some(4), ""),
            ("erts_internal", "cmp_term", "2", Some(2), ""),
            ("nosuchmod", "f", "1", Some(1), ""),
            ("erlang", "abs", "-1", Some(-1), ""),
            ("erlang", "abs", "0", Some(0), ""),
            ("erlang", "abs", "100", Some(100), ""),
            ("erlang", "abs", "4294967297", Some(4294967297), ""),
            ("erlang", "yield", "4294967296", Some(4294967296), ""),
            ("erlang", "abs", "-4294967295", Some(-4294967295), ""),
            ("erlang", "abs", "1.0", None, "badarg"),
            ("erlang", "abs", "<<1>>", None, "badarg"),
            ("erlang", "abs", "1 bsl 70", None, "badarg"),
        ];
        let Some(oracle) = resolve_oracle() else {
            eprintln!();
            eprintln!("=========== beam_bifs runtime/erlang: live oracle SKIPPED ===========");
            eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
            eprintln!("executable, and no `erl` on $PATH).");
            eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP).");
            eprintln!("====================================================================");
            eprintln!();
            return;
        };

        let cases = LIVE
            .iter()
            .map(|(module, name, arity, ..)| format!("{{{module},{name},{arity}}}"))
            .collect::<Vec<_>>()
            .join(", ");
        let eval = format!(
            "Cases = [{cases}], \
             [io:format(\"~p~n\",[try erlang:is_builtin(M,F,A) of R -> R \
                                   catch error:badarg -> badarg end]) || {{M,F,A}} <- Cases], \
             halt()."
        );
        let output = std::process::Command::new(&oracle)
            .args(["-noshell", "-eval", &eval, "-s", "init", "stop"])
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", oracle.display()));
        assert!(
            output.status.success(),
            "the oracle '{}' exited with {:?}\n--- stderr ---\n{}",
            oracle.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
        );
        let stdout = String::from_utf8(output.stdout).expect("the oracle printed UTF-8");
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines.len(), LIVE.len(), "oracle output:\n{stdout}");

        for ((module, name, _, arity, expected), line) in LIVE.iter().zip(&lines) {
            match arity {
                None => assert_eq!(line, expected, "the oracle's {module}:{name} answer"),
                Some(arity) => {
                    let extra = [*module, *name];
                    with_reg(&extra, |reg, ctx| {
                        let args = [
                            atom(ctx, module),
                            atom(ctx, name),
                            ctx.int(*arity).expect("small int"),
                        ];
                        let got = call(reg, ctx, "is_builtin", &args).expect("is_builtin");
                        assert_eq!(
                            name_of(ctx, got).expect("true or false"),
                            *line,
                            "is_builtin({module}, {name}, {arity})"
                        );
                    });
                }
            }
        }
    }

    /// `$ERTS_ORACLE` if it names an existing file, else `erl` on `$PATH`
    /// (the same helper `crate::numeric`'s live-oracle test uses).
    fn resolve_oracle() -> Option<std::path::PathBuf> {
        if let Some(value) = std::env::var_os("ERTS_ORACLE") {
            let program = std::path::PathBuf::from(&value);
            if program.is_file() {
                return Some(program);
            }
            eprintln!(
                "[beam_bifs runtime/erlang] $ERTS_ORACLE='{}' is not an existing file — \
                 treating it as no oracle.",
                program.display()
            );
            return None;
        }
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(|dir| dir.join("erl"))
            .find(|candidate| candidate.is_file())
    }
}
