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

//! Fun-introspection BIFs for `erlang`: `fun_info/1`, `fun_info/2`,
//! `fun_info_mfa/1`, `fun_to_list/1` and `make_fun/3`.
//!
//! Ground truth (read-only): `../../erts/emulator/beam/erl_bif_info.c`
//! (`fun_info_2` at `:3925-4017`, `fun_info_mfa_1` at `:4020-4056`),
//! `../../erts/preloaded/src/erlang.erl` (`fun_info/1` + `fun_info_1/3`, the
//! Erlang-level whole-list form), `../../erts/emulator/beam/bif.c`
//! (`make_fun_3` at `:4474-4492`, `fun_to_list_1` at `:4494-4504`), the
//! `FUN_DEF` printer in `../../erts/emulator/beam/erl_printf_term.c`
//! (`:674-726`, plus `print_atom_name` at `:235-330`) and the fun-entry
//! population in `../../erts/emulator/beam/emu/emu_load.c:645-651` /
//! `erl_fun.c:287-320`.
//!
//! # Roster
//!
//! `erlang:fun_info/1`, `erlang:fun_info/2`, `erlang:fun_info_mfa/1`,
//! `erlang:fun_to_list/1` and `erlang:make_fun/3`. `fun_info/1` is the one
//! **fast path** here: `bif.tab` declares only `fun_info/2`, because the oracle
//! implements `/1` in Erlang (`erlang.erl`'s `fun_info/1` + `fun_info_1/3`).
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
//!                       new_index/new_uniq/index/uniq/refc/pid => undefined
//!                       function/md5/nargs/foo/id/calls => badarg
//! fiprobe:mk/1 (local) : type=>local module=>fiprobe arity=>1 name=>'-mk/1-fun-0-'
//!                       new_index=>1 new_uniq=><<123,204,...>> index=>1 uniq=>85002344
//!                       pid=><0.0.0> refc=>1 env=>[7] function => badarg
//! ```
//!
//! `pid` is the process that created the closure — `funp->creator` in the C
//! (`val = is_local_fun(funp) ? funp->creator : am_undefined`) — so an external
//! fun reports `undefined` and a local one the creator pid. The installed oracle
//! is OTP 29, which still carries both the `pid` arm and the `ErlFunThing.creator`
//! field; OTP 30 removed the field and dropped the arm, but the differential gate
//! compares against the installed `erl`, so the OTP-29 item is reproduced.
//!
//! # `fun_info/1` — the whole list
//!
//! `erlang.erl` builds it in Erlang: a fixed ten-key walk that calls `fun_info/2`
//! for each key and **drops** every `{Key, undefined}` pair, preserving the order
//! of what remains (`fun_info_1/3`). OTP 30's key list dropped `pid`; the installed
//! OTP-29 oracle still carries it, so this is the reproduced order:
//!
//! ```text
//! [pid, module, new_index, new_uniq, index, uniq, name, arity, env, type]
//! ```
//!
//! Oracle-verified on OTP 29.1.1:
//!
//! ```text
//! fun_info(fun lists:map/2)
//!   => [{module,lists},{name,map},{arity,2},{env,[]},{type,external}]
//! fun_info(fiprobe:mk(73))
//!   => [{pid,<0.0.0>},{module,fiprobe},{new_index,0},{new_uniq,<<16-byte MD5>>},
//!       {index,0},{uniq,111062176},{name,'-mk/1-fun-0-'},{arity,1},{env,[73]},
//!       {type,local}]
//! fun_info(42) => error:badarg
//! ```
//!
//! The values come from the **same** [`item_value`] the `/2` form uses, so `/1` and
//! `/2` can never disagree; the ten key atoms are *produced* here, so they are
//! interned on demand exactly like `type`'s `external`/`local`.
//!
//! # `FunKind::Imported` (`NEW_FUN_EXT`) — the described fun
//!
//! The C decoder builds a `NEW_FUN_EXT` into a **local** fun
//! (`MAKE_FUN_HEADER(arity, num_free, 0)`, `external.c:5127`) whose out-of-line
//! `ErlFunEntry` is stubbed from the wire fields by
//! `erts_fun_entry_get_or_make_stub` (`external.c:5162`) — so the oracle reports
//! `{type,local}` / `{refc,1}` for it, and every identity item is read from that
//! stub (`erl_bif_info.c:3956-3980`). This tree **inlines** those `ErlFunEntry`
//! fields into the term descriptor (`beam_term::fun`, the frozen layout owner), and
//! for `FunKind::Imported` the descriptor carries **the whole wire signature**:
//! `module`, the 16-byte MD5 `uniq`, `index`, `old_index` and `old_uniq`
//! (`beam_term::fun::FUN_DESC_WORDS_IMPORTED`). Those five identity items are
//! therefore answered exactly:
//!
//! ```text
//! fun_info(F, module)    => the wire module atom
//! fun_info(F, new_index) => the wire `index`     (`fe->index`)
//! fun_info(F, new_uniq)  => the wire 16-byte MD5 (`fe->uniq`)
//! fun_info(F, index)     => the wire `old_index` (`fe->old_index`)
//! fun_info(F, uniq)      => the wire `old_uniq`  (`fe->old_uniq`)
//! fun_to_list(F)         => "#Fun<Module.OldIndex.OldUniq>"
//! ```
//!
//! `index`/`old_index`/`old_uniq` are `int` fields in C (`erl_fun.h:31-59`) and are
//! read back sign-extended, so a wire value above `2^31 - 1` renders negative
//! (oracle-verified: a wire `old_uniq` of `0xFFFFFFFF` ⇒ `{uniq,-1}` and
//! `"#Fun<Mod.OldIndex.-1>"`).
//!
//! # `FunKind::Local` (`make_fun3`) — the compiled module's identity
//!
//! A `make_fun3` closure inlines only the opaque `(uniq: u64, index: u32)` pair, so
//! the C's `ErlFunEntry` fields are **not** in the term. They are recoverable for a
//! fun some **loaded module** declares, through the
//! [`LocalFunIdentity`](crate::context::LocalFunIdentity) seam (`beam_rt` installs
//! it; it owns the loaded modules), and every item the seam covers is answered from
//! it (`erl_bif_info.c:3951-4014`):
//!
//! ```text
//! fun_info(F, module)    => FunIdentity.module        (mfa->module, else fe->module)
//! fun_info(F, index)     => FunIdentity.old_index     (fe->old_index)
//! fun_info(F, uniq)      => FunIdentity.old_uniq      (fe->old_uniq)
//! fun_info(F, new_uniq)  => FunIdentity.uniq, binary  (fe->uniq, the module MD5)
//! fun_info(F, pid)       => FunIdentity.creator       (funp->creator)
//! fun_info(F, new_index) => the term's own index word  (fe->index)
//! ```
//!
//! `new_index` is the one identity item that needs **no** seam: the term's
//! descriptor word 2 *is* the `FunT` entry's `index` field, which the loader hands
//! `erts_fun_entry_put` as `fe->index` (`emu_load.c:645-651`), so it is read
//! straight back through `beam_codec::term::fun_index` — the same call the
//! `Imported` arm makes on its wire field.
//!
//! When the seam answers `Ok(None)` — the defining module is not loaded here, so the
//! identity is genuinely unknown (the `-eval` evaluator's template closures are the
//! standing example) — every seam-backed item is the typed capacity gap, never a
//! guessed value.
//!
//! The seam is consulted **at most once per `fun_info` call**: a runtime
//! implementation scans the loaded modules and hashes a `.beam`, so the ten-key
//! `fun_info/1` walk must not pay for it once per key. `/1` and `/2` resolve it at
//! the same point of the same [`item_value`] path, so the two still cannot disagree.
//!
//! # Honest gaps (never a fabricated value)
//!
//! Two things stay out of reach and are the typed
//! [`crate::error::unsupported_bif`] rather than a guess:
//!
//! * **`name`** — and therefore `fun_info_mfa/1`'s *function* element — needs the
//!   **loaded module's** fun table (`erts_get_fun_mfa`, `erl_bif_info.c:3944-3947`):
//!   the lambda name is the `FunT` entry's `function` atom, which **neither** the
//!   term descriptor **nor** the `LocalFunIdentity` seam carries (the seam's
//!   [`beam_codec::FunIdentity`] is exactly `module`/`uniq`/`old_index`/`old_uniq`/
//!   `creator`). `fun_info/1` of a `Local` fun reaches that item, so its whole-list
//!   form is the typed gap too — it raises instead of returning a list with the pair
//!   silently missing, which would be a wrong (short) list.
//! * **`fun_info_mfa/1` / `fun_to_list/1` of a `FunKind::Local` (`make_fun3`)
//!   fun** — both need the resolved MFA (the MFA's *function* atom, and
//!   `#Fun<Module.OldIndex.OldUniq>`, `erl_printf_term.c:680-694`), i.e. the same
//!   missing lambda name.
//!
//! # Produced atoms are interned on demand
//!
//! The value atoms this module **produces** — `type`'s `external`/`local` and the
//! `undefined` of the external-fun items — go through
//! [`BifContext::intern_atom`](crate::context::BifContext::intern_atom) (the
//! `AtomInterner` seam), never the lookup-only
//! [`BifContext::atom`](crate::context::BifContext::atom). Neither `external` nor
//! `local` is guaranteed to be in the runtime's table (neither is in `beam_rt`'s
//! `CORE_ATOMS`), and a lookup miss there is an **uncatchable**
//! [`BeamError::TermNotFound`](beam_common::BeamError::TermNotFound) that kills the
//! process instead of answering `{type,external}` — oracle:
//! `erlang:fun_info(fun lists:reverse/1, type)` ⇒ `{type,external}`.
//!
//! # The arity split (`fun_to_list/1` vs `fun_info/2`)//!
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
use beam_term::fun::FunKind;
use beam_term::{atom_val, Eterm, NIL};

use crate::access;
use crate::context::BifContext;
use crate::error;
use crate::registry::BifRegistry;

/// The `MAX_ARG` fun-arity ceiling (`erl_vm.h:48`, `bif.c:4486`).
const MAX_ARG: i64 = 255;

/// Registers this module's BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "fun_info", 1, fun_info_1);
    reg.register("erlang", "fun_info", 2, fun_info_2);
    reg.register("erlang", "fun_info_mfa", 1, fun_info_mfa_1);
    reg.register("erlang", "fun_to_list", 1, fun_to_list_1);
    reg.register("erlang", "make_fun", 3, make_fun_3);
}

/// The `erlang:fun_info/1` key order — the visited order `erlang.erl`'s
/// `fun_info/1` uses on the **installed** oracle (OTP 29.1.1). OTP 30 dropped
/// `pid` from that list (and the `ErlFunThing.creator` field it reads), but the
/// differential gate compares against the installed `erl`, so `/1` reproduces the
/// ten-key OTP-29 walk.
const FUN_INFO_KEYS: [&str; 10] = [
    "pid",
    "module",
    "new_index",
    "new_uniq",
    "index",
    "uniq",
    "name",
    "arity",
    "env",
    "type",
];

// ---------------------------------------------------------------------------
// erlang:fun_info/1
// ---------------------------------------------------------------------------

/// `erlang:fun_info/1` — the whole item list, the oracle's `fun_info_1/3` walk
/// (`../../erts/preloaded/src/erlang.erl`).
///
/// Ten keys in a fixed order ([`FUN_INFO_KEYS`]), each resolved by the *same*
/// [`item_value`] the `/2` form uses; a key whose value is the `undefined` atom is
/// **dropped** and the rest keep their order (the Erlang `fun_info_1/3` clause
/// `{K,undefined} -> fun_info_1(Ks, Fun, A)`). A non-fun is `error:badarg` before
/// any key is visited (the `erlang.erl` guard `when erlang:is_function(Fun)`).
///
/// The value atoms come from [`item_value`], but the ten **key** atoms are produced
/// *here*, so they go through
/// [`BifContext::intern_atom`](crate::context::BifContext::intern_atom) (the
/// `AtomInterner` seam) — an already-interned name is looked up, a new one is
/// interned on demand, never a fabricated index.
fn fun_info_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    if !access::is_function(fun) {
        return Err(error::badarg());
    }
    let kind = beam_codec::term::fun_kind_term(fun)?;
    // One seam consultation for the whole walk: a `make_fun3` fun's identity costs a
    // loaded-module scan (plus a `.beam` read and MD5) in the runtime, so the ten
    // keys must not each pay for it. `item_value` reports the typed gap for the
    // items a `None` identity cannot serve.
    let identity = if kind == FunKind::Local {
        ctx.local_fun_identity(fun)?
    } else {
        None
    };
    let mut pairs: Vec<Eterm> = Vec::with_capacity(FUN_INFO_KEYS.len());
    for key in FUN_INFO_KEYS {
        let item = ctx.intern_atom(key)?;
        let value = item_value(ctx, fun, kind, item_of(ctx, item), identity.as_ref())?;
        if let Some(value) = value {
            pairs.push(ctx.tuple(&[item, value])?);
        }
    }
    ctx.list(&pairs)
}

// ---------------------------------------------------------------------------
// erlang:fun_info/2
// ---------------------------------------------------------------------------

/// `erlang:fun_info/2` (`erl_bif_info.c:3925`).
///
/// A non-fun first argument, a non-atom second argument, or an item atom outside
/// the supported set is `badarg`; every value is oracle-pinned (see the module
/// docs). An `Imported` (`NEW_FUN_EXT`) fun answers its five identity items from
/// the inlined wire descriptor; a `Local` (`make_fun3`) fun answers them from the
/// [`LocalFunIdentity`](crate::context::LocalFunIdentity) seam where it can, and
/// `name` is the documented typed gap.
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
    // Classified as an owned value, not as a *borrow* of the context's atom table:
    // the `type`/`refc`/`new_index` arms **produce** the atoms `external`/`local`/
    // `undefined`, and producing an atom needs the mutable context
    // ([`BifContext::intern_atom`]).
    let item = item_of(ctx, what);

    let kind = beam_codec::term::fun_kind_term(fun)?;

    // One seam consultation, and only for an item that needs it: a `make_fun3`
    // fun's identity costs a loaded-module scan in the runtime, and `Item::Other`
    // must stay a pure `badarg`.
    let identity = if kind == FunKind::Local && item.needs_local_identity() {
        ctx.local_fun_identity(fun)?
    } else {
        None
    };

    let value = match item_value(ctx, fun, kind, item, identity.as_ref())? {
        Some(value) => value,
        // The C's `val = am_undefined` arms.
        None => ctx.intern_atom("undefined")?,
    };

    // `TUPLE2(hp, what, val)` — the item argument is reused as element 1.
    ctx.tuple(&[what, value])
}

/// The value of `item` for `fun` — the C `switch (what)` of `fun_info_2`
/// (`erl_bif_info.c:3951-4014`), shared by `fun_info/1` and `fun_info/2`.
///
/// `Ok(None)` is the C's `val = am_undefined` arms (the external fun's identity
/// items, `refc` and `pid`): `fun_info/2` renders them as the interned `undefined`
/// atom while `fun_info/1` **drops** the pair. Both forms derive the value from
/// this one function, so they cannot disagree.
///
/// `identity` is the [`LocalFunIdentity`](crate::context::LocalFunIdentity) answer
/// for a [`FunKind::Local`] fun, resolved once per call by the caller (`None` for
/// every other kind, and for a `Local` fun whose item does not need it).
///
/// # Errors
///
/// [`Item::Other`] is the C `default` arm ⇒ `badarg`. A `Local` fun whose identity
/// the seam cannot resolve (`identity == None`) is the typed
/// [`crate::error::unsupported_bif`] gap for every item the seam covers — never a
/// fabricated value.
fn item_value(
    ctx: &mut BifContext<'_>,
    fun: Eterm,
    kind: FunKind,
    item: Item,
    identity: Option<&beam_codec::FunIdentity>,
) -> Result<Option<Eterm>> {
    Ok(match item {
        // `am_type`: the C's `is_local_fun(funp) ? am_local : am_external`. A
        // decoded `NEW_FUN_EXT` is built as a local fun there, so an `Imported` fun
        // reports `local` here too. The two value atoms are produced here, so they
        // are interned on demand — neither `external` nor `local` is guaranteed to
        // be in the runtime's table, and the lookup-only `BifContext::atom` would
        // abort the whole process with an uncatchable `TermNotFound`.
        Item::Type => Some(ctx.intern_atom(match kind {
            FunKind::External => "external",
            FunKind::Local | FunKind::Imported => "local",
        })?),
        // `am_arity`: the *closure* arity (total lambda arity minus the captures).
        // The C reads `fun_arity(funp)` — the **header's 8-bit field**, so an
        // `EXPORT_EXT` fun with true arity 256 reports `0` and true arity 300
        // reports `44` (oracle-verified). Only `fun_to_list/1` renders the true
        // arity; do not switch this to `access::fun_true_arity`.
        Item::Arity => Some(ctx.int(beam_codec::term::fun_arity_term(fun)? as i64)?),
        // `am_env`: the free variables in `env` order; `[]` for an external fun.
        Item::Env => {
            let env = beam_codec::term::fun_env(fun)?;
            let owned = env.to_vec();
            Some(ctx.list(&owned)?)
        }
        // `am_refc`: always 1 for a local/imported fun, `undefined` for external.
        Item::Refc => match kind {
            FunKind::External => None,
            FunKind::Local | FunKind::Imported => Some(ctx.int(1)?),
        },
        // `am_pid` (OTP 29): `is_local_fun(funp) ? funp->creator : am_undefined`. A
        // decoded `NEW_FUN_EXT` is a local fun there and its `funp->creator` is the
        // wire creator pid, which this tree inlines in the descriptor — so the
        // `Imported` arm needs no seam. A `make_fun3` closure carries no creator, so
        // only the runtime (the seam) can name it.
        Item::Pid => match kind {
            FunKind::External => None,
            FunKind::Imported => {
                Some(creator_pid_term(ctx, beam_codec::term::fun_creator_pid(fun)?)?)
            }
            FunKind::Local => match identity {
                Some(identity) => Some(creator_pid_term(ctx, identity.creator)?),
                None => return Err(error::unsupported_bif()),
            },
        },
        // `am_new_index`: `make_small(fe->index)` — the lambda-table entry's
        // new-style `index` field (an `int` in C, hence sign-extended). A `make_fun3`
        // term inlines that very word as descriptor word 2 and an `Imported` one
        // carries it on the wire, so neither kind needs a seam. `undefined` for an
        // external fun.
        Item::NewIndex => match kind {
            FunKind::External => None,
            FunKind::Local | FunKind::Imported => {
                Some(ctx.int(beam_codec::term::fun_index(fun)? as i32 as i64)?)
            }
        },
        // `am_new_uniq`: the stubbed `ErlFunEntry`'s 16-byte module MD5
        // (`erts_new_binary_from_data(p, 16, fe->uniq)`). An `Imported` fun's
        // descriptor carries the wire MD5; a `Local` fun's identity does.
        Item::NewUniq => match kind {
            FunKind::External => None,
            FunKind::Imported => {
                let uniq = beam_codec::term::fun_uniq_bytes(fun)?;
                Some(beam_codec::term::make_binary(ctx.heap(), &uniq)?)
            }
            FunKind::Local => match identity {
                Some(identity) => {
                    Some(beam_codec::term::make_binary(ctx.heap(), &identity.uniq)?)
                }
                None => return Err(error::unsupported_bif()),
            },
        },
        // `am_index`: `make_small(fe->old_index)` — the fun's position in the
        // module's lambda (`FunT`) table, which the C loader passes as
        // `erts_fun_entry_put`'s `old_index` argument (`emu_load.c:645-651`) and the
        // `LocalFunIdentity` seam reproduces. Both kinds read it as an `int` and so
        // are sign-extended here too; `undefined` for an external fun.
        Item::Index => match kind {
            FunKind::External => None,
            FunKind::Imported => {
                Some(ctx.int(beam_codec::term::fun_old_index(fun)? as i32 as i64)?)
            }
            FunKind::Local => match identity {
                Some(identity) => Some(ctx.int(identity.old_index as i32 as i64)?),
                None => return Err(error::unsupported_bif()),
            },
        },
        // `am_uniq`: `make_small(fe->old_uniq)` — the `FunT` entry's legacy unique
        // number, an `int` in C. An `Imported` fun's descriptor carries the wire
        // field; a `Local` fun's identity carries the same `FunT` value.
        Item::Uniq => match kind {
            FunKind::External => None,
            FunKind::Imported => {
                Some(ctx.int(beam_codec::term::fun_old_uniq(fun)? as i32 as i64)?)
            }
            FunKind::Local => match identity {
                Some(identity) => Some(ctx.int(identity.old_uniq as i32 as i64)?),
                None => return Err(error::unsupported_bif()),
            },
        },
        // `am_module`: the export's module atom for an external fun; for a local fun
        // the C resolves `mfa->module` and falls back to `fe->module` — both the
        // defining module, which an `Imported` fun's wire descriptor names and a
        // `Local` fun's identity does.
        Item::Module => match kind {
            FunKind::External | FunKind::Imported => {
                let name = atom_term_name(ctx, fun_module(fun)?)?;
                Some(ctx.atom(&name)?)
            }
            FunKind::Local => match identity {
                Some(identity) => {
                    let name = atom_term_name(ctx, identity.module)?;
                    Some(ctx.atom(&name)?)
                }
                None => return Err(error::unsupported_bif()),
            },
        },
        // `am_name`: the export's function atom for an external fun; for a local fun
        // it is `mfa->function` — the *loaded module's* fun table, which neither the
        // descriptor nor the identity seam carries (an unloaded fun even yields
        // `[]`). The documented typed gap.
        Item::Name => match kind {
            FunKind::External => {
                let name = atom_term_name(ctx, fun_function(fun)?)?;
                Some(ctx.atom(&name)?)
            }
            FunKind::Local | FunKind::Imported => return Err(error::unsupported_bif()),
        },
        // Every other item atom (`function`, `md5`, `nargs`, …) is `badarg`.
        Item::Other => return Err(error::badarg()),
    })
}

/// The `pid` item's term for a fun creator: the internal pid immediate when the
/// creator is on **this** node, else the external `[header, node, creation, data]`
/// block.
///
/// A `make_fun3` closure is created by a local process, so its creator is an
/// internal pid — the C's `funp->creator` for such a fun, which is what the runtime
/// seam hands over (`erts_init_process_id`, `<0.0.0>`). The external form is kept
/// for a genuinely remote creator (the wire creator pid of an `Imported` fun), which
/// is the faithful term even though its *rendering* is the documented cross-crate
/// gap (`beam_bifs::term_conv::external_channel_gap`).
fn creator_pid_term(ctx: &mut BifContext<'_>, creator: beam_codec::FunPid) -> Result<Eterm> {
    let internal = ctx
        .local_node()
        .is_some_and(|(node, creation)| node == creator.node && creation == creator.creation)
        && u64::from(creator.number) <= beam_process::MAX_PROCESS_NUMBER
        && u64::from(creator.serial) <= beam_process::MAX_PROCESS_SERIAL;
    if internal {
        return Ok(beam_process::ProcessId::new(
            u64::from(creator.number),
            u64::from(creator.serial),
        )
        .to_eterm());
    }
    beam_codec::term::make_external_pid(
        ctx.heap(),
        beam_term::make_atom(creator.node),
        creator.number,
        creator.serial,
        creator.creation,
    )
}

/// A `fun_info/2` item — the C `switch (what)` of `fun_info_2`
/// (`erl_bif_info.c:3951-4014`).
///
/// The item is classified into an owned value instead of holding the `&str`
/// borrowed from `ctx.atoms()`, because three arms *produce* atoms and therefore
/// need the mutable context (`BifContext::intern_atom`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Item {
    /// `type` — `external`/`local`.
    Type,
    /// `arity` — the header's 8-bit closure arity.
    Arity,
    /// `env` — the free-variable list.
    Env,
    /// `refc` — `1` for a local/imported fun, `undefined` for an external one.
    Refc,
    /// `pid` (OTP 29) — `undefined` for an external fun; the creator pid for a
    /// local/imported one (`funp->creator`, read from the `Imported` wire
    /// descriptor or the [`LocalFunIdentity`](crate::context::LocalFunIdentity)
    /// seam).
    Pid,
    /// `new_index` — `undefined` for an external fun; the stubbed entry's `index`
    /// for a local/imported one (the term's own descriptor word 2 for `Local`, the
    /// wire field for `Imported`).
    NewIndex,
    /// `new_uniq` — `undefined` for an external fun; the stubbed entry's 16-byte
    /// MD5 as a binary for a local/imported one.
    NewUniq,
    /// `index` — `undefined` for an external fun; the stubbed entry's `old_index`
    /// for a local/imported one.
    Index,
    /// `uniq` — `undefined` for an external fun; the stubbed entry's `old_uniq`
    /// for a local/imported one.
    Uniq,
    /// `module`.
    Module,
    /// `name`.
    Name,
    /// Any other item atom (`function`, `md5`, `nargs`, `foo`, …) ⇒ `badarg`.
    Other,
}

impl Item {
    /// Whether this item's value for a [`FunKind::Local`] (`make_fun3`) fun needs
    /// the [`LocalFunIdentity`](crate::context::LocalFunIdentity) seam.
    ///
    /// `new_index` does not (the term's descriptor word 2 *is* `fe->index`), and
    /// `name` does not either (no seam can supply the lambda name — it is the typed
    /// gap for every kind).
    ///
    /// Used by `fun_info/2` to resolve the seam **once**, and only for an item that
    /// needs it: the runtime's implementation scans the loaded modules and hashes a
    /// `.beam`, so an item answered from the term alone must not pay for it.
    fn needs_local_identity(self) -> bool {
        matches!(
            self,
            Item::Pid | Item::NewUniq | Item::Index | Item::Uniq | Item::Module
        )
    }
}

/// Classifies the already-known-to-be-atom item term `what`.
///
/// An atom index that is absent from the context's table names no item, exactly
/// like an unknown item name: both are the C `default` arm ⇒ [`Item::Other`]
/// (`badarg`).
fn item_of(ctx: &BifContext<'_>, what: Eterm) -> Item {
    match ctx.atoms().name(atom_val(what)) {
        Some("type") => Item::Type,
        Some("arity") => Item::Arity,
        Some("env") => Item::Env,
        Some("refc") => Item::Refc,
        Some("pid") => Item::Pid,
        Some("new_index") => Item::NewIndex,
        Some("new_uniq") => Item::NewUniq,
        Some("index") => Item::Index,
        Some("uniq") => Item::Uniq,
        Some("module") => Item::Module,
        Some("name") => Item::Name,
        _ => Item::Other,
    }
}
// ---------------------------------------------------------------------------
// erlang:fun_info_mfa/1
// ---------------------------------------------------------------------------

/// `erlang:fun_info_mfa/1` (`erl_bif_info.c:4020`).
///
/// `{Module, Function, Arity}` for an external fun; a non-fun is `badarg`. A
/// local/imported fun needs the resolved MFA's *function* atom — the loaded
/// module's fun table (`erts_get_fun_mfa`) — which the inlined descriptor does not
/// carry, so both remain the documented typed gap.
fn fun_info_mfa_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    if !access::is_function(fun) {
        return Err(error::badarg());
    }
    if beam_codec::term::fun_kind_term(fun)? != FunKind::External {
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
///
/// An `Imported` (`NEW_FUN_EXT`) fun takes the printer's **local-fun** arm
/// (`erl_printf_term.c:676-694`): `"#Fun<Module.OldIndex.OldUniq>"`, with the raw
/// module atom name (no `print_atom_name` quoting) and the stubbed entry's signed
/// `old_index`/`old_uniq` — all three carried by its descriptor.
///
/// A non-fun is `badarg`; a `FunKind::Local` (`make_fun3`) fun would need the same
/// triple from an `ErlFunEntry` its opaque descriptor does not have — the
/// documented typed gap.
fn fun_to_list_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    if !access::is_function(fun) {
        return Err(error::badarg());
    }
    let text = match beam_codec::term::fun_kind_term(fun)? {
        FunKind::External => {
            let module = atom_term_name(ctx, fun_module(fun)?)?;
            let function = atom_term_name(ctx, fun_function(fun)?)?;
            // The `FUN_DEF` printer uses the **true** arity (`ep->info.mfa.arity`,
            // `erl_printf_term.c:721-723`), *not* the header's 8-bit field: an
            // `EXPORT_EXT` fun with true arity 256 renders `fun lists:reverse/256`
            // while `erlang:fun_info(F, arity)` reports `0` — the oracle-verified
            // split.
            let arity = access::fun_true_arity(fun)?;
            format!(
                "fun {}:{}/{}",
                quoted_atom(&module),
                quoted_atom(&function),
                arity
            )
        }
        FunKind::Imported => {
            let module = atom_term_name(ctx, fun_module(fun)?)?;
            // `fe->old_index` / `fe->old_uniq` are `int` fields (erl_fun.h:31-59)
            // printed through `PRINT_SWORD(...,'d', ...)` — sign-extended.
            let old_index = beam_codec::term::fun_old_index(fun)? as i32;
            let old_uniq = beam_codec::term::fun_old_uniq(fun)? as i32;
            format!("#Fun<{module}.{old_index}.{old_uniq}>")
        }
        FunKind::Local => return Err(error::unsupported_bif()),
    };
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

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable};

    /// The test `AtomInterner` — a `RefCell`-wrapped table, the shape a runtime
    /// installs when its interner **is** the atom table. Implementing the seam for
    /// `RefCell<AtomTable>` keeps the cases below free of a bespoke mock struct.
    impl crate::context::AtomInterner for std::cell::RefCell<AtomTable> {
        fn intern(&mut self, name: &str) -> beam_term::Atom {
            self.borrow_mut().intern(name)
        }
    }

    /// The item, module and function atoms of the cases below — deliberately
    /// **without** `external`, `local` and `undefined`, the three atoms this module
    /// *produces*. A lookup-only construction of them would therefore abort with an
    /// uncatchable `TermNotFound`.
    fn item_atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "type",
            "arity",
            "env",
            "refc",
            "new_index",
            "new_uniq",
            "index",
            "uniq",
            "module",
            "name",
            "lists",
            "reverse",
            "foo",
        ]);
        atoms
    }

    /// An `EXPORT_EXT` (external) `lists:reverse/arity` fun term on the context heap.
    fn external_fun(ctx: &mut BifContext<'_>, arity: usize) -> Eterm {
        let module = ctx.atom("lists").expect("seeded module");
        let function = ctx.atom("reverse").expect("seeded function");
        beam_codec::term::make_fun_external(ctx.heap(), arity, atom_val(module), atom_val(function))
            .expect("the external fun fits")
    }

    /// A `make_fun3` (`FunKind::Local`) fun term plus the storage owning its words
    /// (`[header, uniq, index]`, no free variables).
    fn local_fun(arity: u32, uniq: u64, index: u32) -> (Box<[Eterm]>, Eterm) {
        let header = beam_term::FUN_SUBTAG | ((arity as usize) << 8);
        let words: Box<[Eterm]> = vec![
            Eterm::from_raw(header),
            Eterm::from_raw(uniq as usize),
            Eterm::from_raw(index as usize),
        ]
        .into_boxed_slice();
        let term = beam_term::make_boxed(words.as_ptr());
        (words, term)
    }

    /// The name the *interner's* table has for the atom term `term` (the produced
    /// tag is interned there, not in the context's own table).
    fn interned_name(interner: &std::cell::RefCell<AtomTable>, term: Eterm) -> Option<String> {
        interner.borrow().name(atom_val(term)).map(str::to_owned)
    }

    /// `fun_info(F, type)` ⇒ `{type,external}` even when `external` is absent from the
    /// runtime's table: the BIF **produces** that atom, so it is interned through the
    /// `AtomInterner` seam. Before the fix this was an uncatchable
    /// `BeamError::TermNotFound` that killed the process (oracle:
    /// `erlang:fun_info(fun lists:reverse/1, type)` ⇒ `{type,external}`).
    #[test]
    fn fun_info_type_interns_external_on_demand() {
        let mut heap = Heap::new(4096).expect("test heap");
        let atoms = item_atoms();
        let mut interner = std::cell::RefCell::new(AtomTable::new());
        let tag = {
            let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
                .with_atom_interner(&mut interner);
            let fun = external_fun(&mut ctx, 1);
            let item = ctx.atom("type").expect("seeded item");
            let info = fun_info_2(&mut ctx, &[fun, item]).expect("type");
            assert_eq!(access::tuple_size(info), 2);
            // `TUPLE2(hp, what, val)` reuses the item argument as element 1.
            assert_eq!(access::tuple_element(info, 0), item);
            access::tuple_element(info, 1)
        };
        assert_eq!(interned_name(&interner, tag).as_deref(), Some("external"));
    }

    /// The same for a `make_fun3` local fun ⇒ `{type,local}` (`local` is likewise not
    /// guaranteed to be interned).
    #[test]
    fn fun_info_type_interns_local_on_demand() {
        let mut heap = Heap::new(4096).expect("test heap");
        let atoms = item_atoms();
        let mut interner = std::cell::RefCell::new(AtomTable::new());
        let (_words, fun) = local_fun(1, 0x1234, 1);
        let tag = {
            let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
                .with_atom_interner(&mut interner);
            let item = ctx.atom("type").expect("seeded item");
            let info = fun_info_2(&mut ctx, &[fun, item]).expect("type");
            access::tuple_element(info, 1)
        };
        assert_eq!(interned_name(&interner, tag).as_deref(), Some("local"));
    }

    /// The `undefined` value of the external-fun items is interned the same way
    /// (`refc` and the four `undefined` identity items).
    #[test]
    fn fun_info_undefined_values_are_interned_on_demand() {
        let mut heap = Heap::new(4096).expect("test heap");
        let atoms = item_atoms();
        let mut interner = std::cell::RefCell::new(AtomTable::new());
        let tags = {
            let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
                .with_atom_interner(&mut interner);
            let fun = external_fun(&mut ctx, 1);
            let mut tags = Vec::new();
            for item_name in ["refc", "new_index", "new_uniq", "index", "uniq"] {
                let item = ctx.atom(item_name).expect("seeded item");
                let info = fun_info_2(&mut ctx, &[fun, item]).expect(item_name);
                tags.push(access::tuple_element(info, 1));
            }
            tags
        };
        for tag in tags {
            assert_eq!(interned_name(&interner, tag).as_deref(), Some("undefined"));
        }
    }

    /// Without an interner the produced atom stays the typed `TermNotFound` — never
    /// a fabricated index and never a wrong tag.
    #[test]
    fn a_produced_atom_without_an_interner_is_the_typed_gap() {
        let mut heap = Heap::new(4096).expect("test heap");
        let atoms = item_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let fun = external_fun(&mut ctx, 1);
        let item = ctx.atom("type").expect("seeded item");
        assert!(matches!(
            fun_info_2(&mut ctx, &[fun, item]),
            Err(BeamError::TermNotFound)
        ));
    }

    /// The item classification is unchanged: an unknown item atom and a non-atom item
    /// are both the C `default` arm's `badarg`, and identifiers are still resolved.
    #[test]
    fn item_classification_is_unchanged() {
        let mut heap = Heap::new(4096).expect("test heap");
        let atoms = item_atoms();
        let mut interner = std::cell::RefCell::new(AtomTable::new());
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_atom_interner(&mut interner);
        let fun = external_fun(&mut ctx, 1);

        let foo = ctx.atom("foo").expect("seeded item");
        assert!(matches!(fun_info_2(&mut ctx, &[fun, foo]), Err(BeamError::BadArg)));
        assert!(matches!(
            fun_info_2(&mut ctx, &[fun, make_small(1)]),
            Err(BeamError::BadArg)
        ));
        // A non-fun is `badarg` before the item is examined.
        let type_item = ctx.atom("type").expect("seeded item");
        assert!(matches!(
            fun_info_2(&mut ctx, &[make_small(1), type_item]),
            Err(BeamError::BadArg)
        ));
        assert_eq!(item_of(&ctx, type_item), Item::Type);
        assert_eq!(item_of(&ctx, foo), Item::Other);
    }
}