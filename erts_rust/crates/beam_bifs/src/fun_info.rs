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

//! Fun-introspection BIFs for `erlang`: `fun_info/2`, `fun_info_mfa/1`,
//! `fun_to_list/1` and `make_fun/3`.
//!
//! Ground truth (read-only): `../../erts/emulator/beam/erl_bif_info.c`
//! (`fun_info_2` at `:3925-4017`, `fun_info_mfa_1` at `:4020-4056`),
//! `../../erts/emulator/beam/bif.c` (`make_fun_3` at `:4474-4492`,
//! `fun_to_list_1` at `:4494-4504`), the `FUN_DEF` printer in
//! `../../erts/emulator/beam/erl_printf_term.c` (`:674-726`, plus
//! `print_atom_name` at `:235-330`) and the fun-entry population in
//! `../../erts/emulator/beam/emu/emu_load.c:645-650` /
//! `erl_fun.c:287-320`.
//!
//! # Roster
//!
//! `erlang:fun_info/2`, `erlang:fun_info_mfa/1`, `erlang:fun_to_list/1` and
//! `erlang:make_fun/3`. (`erlang:fun_info/1` is deliberately **not** registered:
//! `bif.tab` declares only `fun_info/2`, and C's `fun_info_2` with a
//! non-item second argument — `[]` included — falls to its `default` arm and
//! raises `badarg`, which the oracle confirms.)
//!
//! # `fun_info/2` — the item set
//!
//! C switches on the **second** argument (a single item atom; anything else,
//! a *list* included, is `badarg`) and returns the 2-tuple `{Item, Value}`,
//! reusing the argument itself as element 1. Oracle-verified on OTP 29.1.1
//! (`escript`, `erl_bif_info.c:3951-4014`):
//!
//! ```text
//! fun lists:reverse/1 : type=>external module=>lists arity=>1 name=>reverse env=>[]
//!                       new_index/new_uniq/index/uniq/refc => undefined
//!                       function/md5/nargs/foo/id/calls => badarg
//! fiprobe:mk/1 (local) : type=>local module=>fiprobe arity=>1 name=>'-mk/1-fun-0-'
//!                       new_index=>1 new_uniq=><<123,204,...>> index=>1 uniq=>85002344
//!                       refc=>1 env=>[7] function => badarg
//! ```
//!
//! # Honest gaps (never a fabricated value)
//!
//! The rewrite **inlines** the C `ErlFunThing.entry` pointer into the term
//! descriptor (`beam_term::fun`, the frozen layout owner; see also
//! `beam_engine::terms::fun`), so a `FunKind::Local` (`make_fun3`) fun carries an
//! opaque `(uniq:u64, index:u32)` pair and **no** module atom, no 16-byte MD5, no
//! `old_index` — i.e. no `ErlFunEntry` fields that the oracle's `module`, `name`,
//! `new_index`, `new_uniq`, `index` and `uniq` items are read from
//! (`erl_bif_info.c:3956-3980,4007-4011`). Those six items therefore return the
//! typed [`crate::error::unsupported_bif`] for a `Local` (or `Imported`) fun
//! rather than inventing a module/MD5/index. The same gap applies to
//! `fun_info_mfa/1` and `fun_to_list/1` of a local fun, both of which need the
//! resolved MFA (`#Fun<Module.OldIndex.OldUniq>`, `erl_printf_term.c:680-694`).
//!
//! `FunKind::Imported` is the M1 shape of a `NEW_FUN_EXT`-decoded fun. The C
//! decoder builds it as a **local** fun (`MAKE_FUN_HEADER(arity, num_free, 0)`,
//! `external.c:5127`) whose entry is stubbed by
//! `erts_fun_entry_get_or_make_stub` (`external.c:5162`), so the oracle reports
//! `{type,local}` / `{refc,1}` for it; this module reproduces exactly that for the
//! kind-independent items (`type`, `arity`, `env`, `refc`) and keeps the six
//! identity items as the honest typed gap (the descriptor cannot express the
//! oracle's loaded-vs-unloaded MFA resolution, nor the module's checksum-derived
//! `new_uniq` for the *bare* `Exported`/loaded distinction).
//!
//! # The arity split (`fun_to_list/1` vs `fun_info/2`)
//!
//! An `EXPORT_EXT` (external) fun stores its arity twice: the header's 8-bit
//! field and, in this runtime, descriptor word 3 — the inlined `Export`'s
//! `info.mfa.arity` ([`beam_term::fun::fun_true_arity`]). The C reads the two
//! separately, and so does this module:
//!
//! * **`fun_to_list/1` uses the TRUE arity** — the C `FUN_DEF` printer emits
//!   `ep->info.mfa.arity` (`erl_printf_term.c:721-723`), so an `EXPORT_EXT` fun
//!   with true arity 256 renders `"fun lists:reverse/256"` and 300 renders
//!   `"fun lists:reverse/300"` (oracle-verified). It is read through
//!   [`crate::access::fun_true_arity`].
//! * **`fun_info/2`'s `arity` item and `fun_info_mfa/1` use the header's 8-bit
//!   field** — the C reads `fun_arity(funp)` (`erl_bif_info.c:4003-4006`,
//!   `:4049-4052`), so those same funs report `0` and `44`
//!   (`beam_codec::term::fun_arity_term`). `erlang:is_function/2`
//!   ([`crate::type_test`], via [`crate::access::fun_arity`]) reads the same
//!   header field.
//!
//! # `make_fun/3`
//!
//! `bif.c:4474-4492`: both atoms plus a small integer, `0 <= A <= MAX_ARG` (255),
//! and **no existence check** (the oracle accepts `make_fun(no_such_mod,f,1)` and
//! reports `{module,no_such_mod}` / `{no_such_mod,f,1}`). The result is an
//! `EXPORT_EXT` lambda, i.e. a `FunKind::External` fun equal to `fun M:F/A`
//! (oracle: `make_fun(lists,reverse,1) =:= fun lists:reverse/1` ⇒ `true`), built
//! here by `beam_codec::term::make_fun_external` (the frozen-layout owner's
//! constructor).

use beam_common::Result;
use beam_term::{atom_val, Eterm, NIL};

use crate::access;
use crate::context::BifContext;
use crate::error;
use crate::registry::BifRegistry;

/// The `MAX_ARG` fun-arity ceiling (`erl_vm.h:48`, `bif.c:4486`).
const MAX_ARG: i64 = 255;

/// Registers this module's BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "fun_info", 2, fun_info_2);
    reg.register("erlang", "fun_info_mfa", 1, fun_info_mfa_1);
    reg.register("erlang", "fun_to_list", 1, fun_to_list_1);
    reg.register("erlang", "make_fun", 3, make_fun_3);
}

// ---------------------------------------------------------------------------
// erlang:fun_info/2
// ---------------------------------------------------------------------------

/// `erlang:fun_info/2` (`erl_bif_info.c:3925`).
///
/// A non-fun first argument, a non-atom second argument, or an item atom outside
/// the supported set is `badarg`; every value is oracle-pinned (see the module
/// docs), with the six local/imported identity items as the documented typed gap.
fn fun_info_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    // C checks the fun first, so `fun_info(42, [type])` is `badarg` too.
    if !access::is_function(fun) {
        return Err(error::badarg());
    }
    let what = args[1];
    if !access::is_atom(what) {
        // C's `switch (what)` matches no `am_*` for a list/tuple/number.
        return Err(error::badarg());
    }
    let item = match ctx.atoms().name(atom_val(what)) {
        Some(name) => name,
        // An atom index that is not interned has no item name.
        None => return Err(error::badarg()),
    };

    let external = beam_codec::term::fun_kind_term(fun)? == beam_codec::term::FunKind::External;

    let value = match item {
        // `am_type`: local (incl. a decoded `NEW_FUN_EXT`) vs external.
        "type" => ctx.atom(if external { "external" } else { "local" })?,
        // `am_arity`: the *closure* arity (total lambda arity minus the captures).
        // The C reads `fun_arity(funp)` — the **header's 8-bit field**, so an
        // `EXPORT_EXT` fun with true arity 256 reports `0` and true arity 300
        // reports `44` (oracle-verified). Only `fun_to_list/1` renders the true
        // arity; do not switch this to `access::fun_true_arity`.
        "arity" => ctx.int(beam_codec::term::fun_arity_term(fun)? as i64)?,
        // `am_env`: the free variables in `env` order; `[]` for an external fun.
        "env" => {
            let env = beam_codec::term::fun_env(fun)?;
            let owned = env.to_vec();
            ctx.list(&owned)?
        }
        // `am_refc`: always 1 for a local/imported fun, `undefined` for external.
        "refc" => {
            if external {
                ctx.atom("undefined")?
            } else {
                ctx.int(1)?
            }
        }
        // `am_undefined` items: the C reports `undefined` for every fun that is
        // not local. A local/imported fun needs `ErlFunEntry` fields the frozen
        // descriptor does not carry — the documented typed gap.
        "new_index" | "new_uniq" | "index" | "uniq" => {
            if external {
                ctx.atom("undefined")?
            } else {
                return Err(error::unsupported_bif());
            }
        }
        // `am_module`: the export's module atom for an external fun; a local fun
        // would need `ErlFunEntry.module` (absent from the descriptor).
        "module" => {
            if external {
                let name = atom_term_name(ctx, fun_module(fun)?)?;
                ctx.atom(&name)?
            } else {
                return Err(error::unsupported_bif());
            }
        }
        // `am_name`: the export's function atom for an external fun; a local fun
        // needs the resolved MFA (`erts_get_fun_mfa`).
        "name" => {
            if external {
                let name = atom_term_name(ctx, fun_function(fun)?)?;
                ctx.atom(&name)?
            } else {
                return Err(error::unsupported_bif());
            }
        }
        // Every other item atom (`function`, `md5`, `nargs`, …) is `badarg`.
        _ => return Err(error::badarg()),
    };

    // `TUPLE2(hp, what, val)` — the item argument is reused as element 1.
    ctx.tuple(&[what, value])
}

// ---------------------------------------------------------------------------
// erlang:fun_info_mfa/1
// ---------------------------------------------------------------------------

/// `erlang:fun_info_mfa/1` (`erl_bif_info.c:4020`).
///
/// `{Module, Function, Arity}` for an external fun; a non-fun is `badarg`. A
/// local/imported fun needs the resolved MFA (module + lambda name) the inlined
/// descriptor cannot supply — the documented typed gap.
fn fun_info_mfa_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    if !access::is_function(fun) {
        return Err(error::badarg());
    }
    if beam_codec::term::fun_kind_term(fun)? != beam_codec::term::FunKind::External {
        return Err(error::unsupported_bif());
    }
    let module = atom_term_name(ctx, fun_module(fun)?)?;
    let function = atom_term_name(ctx, fun_function(fun)?)?;
    let module = ctx.atom(&module)?;
    let function = ctx.atom(&function)?;
    // As for `fun_info/2`'s `arity`: the header's 8-bit field, not the true arity
    // (`erl_bif_info.c:4049-4052` reads `fun_arity`), so true arity 256 ⇒
    // `{lists,reverse,0}` (oracle-verified).
    let arity = ctx.int(beam_codec::term::fun_arity_term(fun)? as i64)?;
    ctx.tuple(&[module, function, arity])
}

// ---------------------------------------------------------------------------
// erlang:fun_to_list/1
// ---------------------------------------------------------------------------

/// `erlang:fun_to_list/1` (`bif.c:4494`).
///
/// An external fun renders through the C `FUN_DEF` printer as
/// `"fun Module:Function/Arity"` with the C's own atom quoting
/// (`print_atom_name`, `erl_printf_term.c:235-330` — *not* `io_lib`'s
/// `write_atom`: `make_fun(nonode@nohost, foo, 1)` ⇒ `"fun 'nonode@nohost':foo/1"`).
/// `Arity` is the **true** arity (`ep->info.mfa.arity`, descriptor word 3 for an
/// external fun) — so this reader differs from `fun_info/2`'s `arity` item and
/// `fun_info_mfa/1` for an arity above 255 (module docs).
/// A non-fun is `badarg`; a local/imported fun would need the `#Fun<M.OldIndex.
/// OldUniq>` triple from its `ErlFunEntry` — the documented typed gap.
fn fun_to_list_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    if !access::is_function(fun) {
        return Err(error::badarg());
    }
    if beam_codec::term::fun_kind_term(fun)? != beam_codec::term::FunKind::External {
        return Err(error::unsupported_bif());
    }
    let module = atom_term_name(ctx, fun_module(fun)?)?;
    let function = atom_term_name(ctx, fun_function(fun)?)?;
    // The `FUN_DEF` printer uses the **true** arity (`ep->info.mfa.arity`,
    // `erl_printf_term.c:721-723`), *not* the header's 8-bit field: an
    // `EXPORT_EXT` fun with true arity 256 renders `fun lists:reverse/256` while
    // `erlang:fun_info(F, arity)` reports `0` — the oracle-verified split.
    let arity = access::fun_true_arity(fun)?;
    let text = format!(
        "fun {}:{}/{}",
        quoted_atom(&module),
        quoted_atom(&function),
        arity
    );
    text_list(ctx, &text)
}

// ---------------------------------------------------------------------------
// erlang:make_fun/3
// ---------------------------------------------------------------------------

/// `erlang:make_fun/3` (`bif.c:4474`).
///
/// Two atoms and a small integer in `0..=MAX_ARG`, else `badarg`; the result is
/// the `EXPORT_EXT` lambda for `Module:Function/Arity` — identical to
/// `fun Module:Function/Arity` and reported by `fun_info/2` as
/// `{type,external}` / `{module,Module}` / `{name,Function}` / `{arity,Arity}`.
/// No existence check is performed (matching the C, which only asks for an
/// export *stub*).
fn make_fun_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (module, function, arity) = (args[0], args[1], args[2]);
    if !access::is_atom(module) || !access::is_atom(function) || !access::is_small(arity) {
        return Err(error::badarg());
    }
    let arity = beam_term::signed_value(arity);
    if !(0..=MAX_ARG).contains(&arity) {
        return Err(error::badarg());
    }
    beam_codec::term::make_fun_external(
        ctx.heap(),
        arity as usize,
        atom_val(module),
        atom_val(function),
    )
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// The `module` atom of a fun term (external/imported only).
fn fun_module(fun: Eterm) -> Result<beam_term::Atom> {
    beam_codec::term::fun_module(fun)
}

/// The `function` atom of an external fun term.
fn fun_function(fun: Eterm) -> Result<beam_term::Atom> {
    beam_codec::term::fun_function(fun)
}

/// The interned name of `atom`, or `BeamError::TermNotFound` when the atom index
/// is not in the context's table (the same error [`BifContext::atom`] yields).
fn atom_term_name(ctx: &BifContext<'_>, atom: beam_term::Atom) -> Result<String> {
    match ctx.atoms().name(atom) {
        Some(name) => Ok(name.to_string()),
        None => Err(beam_common::BeamError::TermNotFound),
    }
}

/// Builds the proper list of small integers (the string) `text` on the context
/// heap — `term2list_dsprintf`'s result, whose elements are the raw UTF-8 bytes
/// of the rendering (`erl_printf_term.c` copies the atom's encoded name).
fn text_list(ctx: &mut BifContext<'_>, text: &str) -> Result<Eterm> {
    let mut acc = NIL;
    for &byte in text.as_bytes().iter().rev() {
        acc = ctx.cons(ctx.int(byte as i64)?, acc)?;
    }
    Ok(acc)
}

/// `IS_LOWER` (`erl_printf_term.c:122-124`, `LATIN1`).
fn is_lower(c: u32) -> bool {
    (0x61..=0x7A).contains(&c) || (223..=255).contains(&c) && c != 247
}

/// `IS_UPPER` (`erl_printf_term.c:125-127`, `LATIN1`).
fn is_upper(c: u32) -> bool {
    (0x41..=0x5A).contains(&c) || (192..=222).contains(&c) && c != 215
}

/// `IS_DIGIT`.
fn is_digit(c: u32) -> bool {
    (0x30..=0x39).contains(&c)
}

/// Renders an atom name the way the C's `print_atom_name`
/// (`erl_printf_term.c:235-330`) does — the form the `FUN_DEF` printer uses for a
/// fun's module/function. **Note this differs from `io_lib`'s `write_atom`**: the
/// C's `IS_ALNUM` is `IS_LOWER | IS_UPPER | IS_DIGIT`, so `@` (and every other
/// punctuation character) forces quoting — oracle:
/// `erlang:fun_to_list(erlang:make_fun(nonode@nohost, foo, 1))` ⇒
/// `"fun 'nonode@nohost':foo/1"` while `io_lib:format("~p",[nonode@nohost])` ⇒
/// `nonode@nohost`.
///
/// The escaping matches the C: `'`, `\`, `\n`, `\f`, `\t`, `\r`, `\b`, `\v`, and
/// a zero-padded 3-digit octal escape for a code point below 32 or in
/// `128..=159` (127 is **not** escaped — oracle-verified for `0x01`, `0x1F`,
/// `0x0B`, `0x7F`).
fn quoted_atom(name: &str) -> String {
    let mut chars = name.chars();
    let need_quote = match chars.next() {
        // The empty atom prints as `''`.
        None => true,
        Some(first) => {
            !is_lower(first as u32)
                || chars.any(|c| {
                    let cp = c as u32;
                    !(is_digit(cp) || is_lower(cp) || is_upper(cp)) && c != '_'
                })
        }
    };
    let mut out = String::new();
    if need_quote {
        out.push('\'');
    }
    for c in name.chars() {
        let cp = c as u32;
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\u{8}' => out.push_str("\\b"),
            '\u{b}' => out.push_str("\\v"),
            _ if cp < 32 || (128..=159).contains(&cp) => {
                out.push_str(&format!("\\{cp:03o}"));
            }
            _ => out.push(c),
        }
    }
    if need_quote {
        out.push('\'');
    }
    out
}
