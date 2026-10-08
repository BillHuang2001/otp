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

//! The `persistent_term` BIFs: `get/0`, `get/1`, `get/2`, `put/2`, `put_new/2`,
//! `erase/1` and `info/0`.
//!
//! Ground truth (read-only): the entries in
//! `../../erts/emulator/beam/bif.tab` (`persistent_term:put/2:707`,
//! `get/1:708`, `get/0:709`, `erase/1:710`, `info/0:711`, `get/2:741`,
//! `put_new/2:840`) and their implementations in
//! `../../erts/emulator/beam/erl_bif_persistent.c` — `persistent_term_put_2:314`,
//! `persistent_term_put_new_2:319`, `persistent_term_get_0:324`,
//! `persistent_term_get_1:380`, `persistent_term_get_2:390`,
//! `persistent_term_erase_1:417`, `persistent_term_info_0:569`, and the shared
//! helpers `put_common:642`, `do_get_all:830` and `do_info:929`.
//!
//! # Roster
//!
//! `persistent_term:get/0`, `get/1`, `get/2`, `put/2`, `put_new/2`, `erase/1`,
//! `info/0` — all seven declared in `bif.tab` and invoked as the *module*
//! functions (the `persistent_term` wrappers are thin; there is no
//! `erts_internal:persistent_term_*` indirection to port).
//!
//! # The store is runtime-owned global state
//!
//! Persistent terms are a **node-global** key/value store held in a literal area
//! outside every process heap (`the_hash_table`, `erl_bif_persistent.c:187`) and
//! updated under a table-wide permission lock with thread-progress hand-off. That
//! machinery belongs to the Stage-5 runtime (`beam_rt`), which is a different crate
//! this layer may not touch, so every one of the seven BIFs resolves through the
//! optional [`PersistentTerm`](crate::context::PersistentTerm) seam. With **no hook
//! installed every key reports a typed `unsupported_bif` capability gap** — never
//! `[]`, never `undefined`, never `ok`, never a made-up count (workspace
//! `CONTEXT.md` §3: a wrong value is far worse than an honest gap).
//!
//! # What the BIF layer decides itself
//!
//! Unlike most seam-backed BIFs there is no argument the oracle rejects: any term
//! may be a key (probed: integers, tuples, lists, binaries and atoms are all
//! accepted by `put/2`). The two `badarg` cases are *store* outcomes the hook owns:
//!
//! * `get/1` of an absent key (`persistent_term_get_1:383`, `is_non_value(result)`);
//! * `put_new/2` of a key that already holds a **different** value
//!   (`put_common:730-733`, `new == am_true` on an unequal existing value).
//!
//! `put/2` never raises — an existing key is overwritten and returns `ok`
//! (`put_common:726-735` falls through to the update), and a key already holding
//! the *same* value is a no-op `ok` for both arities (`put_common:726-729`).
//!
//! # Oracle-pinned shapes (OTP 29.1.1 / erts-17.1)
//!
//! ```text
//! $ erl -noshell -eval 'io:format("~p~n",[persistent_term:put(k,1),
//!     persistent_term:get(k), persistent_term:get(nope,default),
//!     persistent_term:erase(k), persistent_term:erase(k)])' -s init stop
//! ok,1,default,true,false
//! ```
//!
//! so `erase/1` is **`true`/`false`**, not `ok` (the common wrapper
//! misconception): `am_true` on the erased path (`:539`) and `am_false` when the
//! key is absent (`:548`).
//!
//! `info/0` is a **flat map** `#{count => N, memory => Bytes}`
//! (`do_info:977`, `MAP2(hp, am_count, …, am_memory, …)`), not a proplist —
//! probed: `persistent_term:info()` ⇒ `#{count => 28, memory => 33392}`. The two
//! numbers are pure runtime bookkeeping (the entry count and the byte total of the
//! table plus its literal areas), so they come from the hook; this layer only
//! renders the map and never invents either figure.
//!
//! `get/0` is a list of `{Key, Value}` 2-tuples (`do_get_all:893`) whose **order is
//! unspecified** (a hash-table walk). The hook's order is preserved verbatim and
//! never pinned.

use beam_common::Result;
use beam_term::Eterm;

use crate::access;
use crate::context::BifContext;
use crate::error;
use crate::registry::BifRegistry;

/// `persistent_term:get/0` — every stored `{Key, Value}` pair, in the store's own
/// order (`persistent_term_get_0`, `erl_bif_persistent.c:324`; the pairs are built
/// by `do_get_all:830`).
///
/// The order is a hash-table walk and is **not** observable, so it is taken from
/// the hook verbatim (see the module docs). With no hook installed this is the
/// typed `unsupported_bif` capability gap — never an empty list.
fn get_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let pairs = ctx.persistent_term_get_all()?;
    let mut elems = Vec::with_capacity(pairs.len());
    for (key, value) in pairs {
        elems.push(ctx.tuple(&[key, value])?);
    }
    ctx.list(&elems)
}

/// `persistent_term:get(Key)` — the value stored under `Key`
/// (`persistent_term_get_1:380`).
///
/// An absent key is the oracle's `badarg` (`:383`, `is_non_value(result)`).
fn get_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    match ctx.persistent_term_get(args[0])? {
        Some(value) => Ok(value),
        None => Err(error::badarg()),
    }
}

/// `persistent_term:get(Key, Default)` — the value stored under `Key`, or
/// `Default` when the key is absent (`persistent_term_get_2:390`).
fn get_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    match ctx.persistent_term_get(args[0])? {
        Some(value) => Ok(value),
        None => Ok(args[1]),
    }
}

/// `persistent_term:put(Key, Value)` — stores `Value` under `Key` and returns `ok`
/// (`persistent_term_put_2:314` ⇒ `put_common(c_p, key, term, am_false)`).
///
/// `put/2` never raises: an existing key is overwritten and a key already holding
/// the same value is a no-op (`put_common:726-735`; probed: both
/// `put(k,1), put(k,1)` and `put(k,2)` are `ok`). The hook's outcome is therefore
/// ignored — only its failure propagates.
fn put_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let _stored = ctx.persistent_term_put(args[0], args[1], false)?;
    ctx.ok()
}

/// `persistent_term:put_new(Key, Value)` — stores `Value` under `Key` only when the
/// key is absent, returning `ok` (`persistent_term_put_new_2:319` ⇒
/// `put_common(c_p, key, term, am_true)`).
///
/// A key that already holds a **different** value is the oracle's `badarg`
/// (`put_common:730-733`); a key holding the *same* value is a no-op `ok`
/// (`:726-729`) — the hook reports which outcome it is.
fn put_new_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if ctx.persistent_term_put(args[0], args[1], true)? {
        ctx.ok()
    } else {
        Err(error::badarg())
    }
}

/// `persistent_term:erase(Key)` — erases `Key`, returning the atom `true` when it
/// was present and `false` when it was not
/// (`persistent_term_erase_1:417`; `:539` `am_true` / `:548` `am_false`).
///
/// The return is **not** `ok` (probed: `erase(k)` ⇒ `true`, then `false`).
fn erase_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let erased = ctx.persistent_term_erase(args[0])?;
    ctx.boolean(erased)
}

/// `persistent_term:info/0` — the runtime's bookkeeping map
/// `#{count => Count, memory => Memory}` (`persistent_term_info_0:569`, `do_info:929`,
/// the `MAP2` at `:977`).
///
/// Both figures are the hook's; the keys are interned here and the integers are
/// materialized exactly (a byte total beyond the 60-bit range becomes a bignum, the
/// shared [`crate::arith::int_from_mag`] promotion). The seam is consulted
/// **before** the atoms are resolved, so a context with no hook reports the typed
/// `unsupported_bif` gap rather than a `TermNotFound` for an un-interned key.
fn info_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let (count, memory) = ctx.persistent_term_info()?;
    let count_key = ctx.intern_atom("count")?;
    let memory_key = ctx.intern_atom("memory")?;
    let count_term = crate::arith::int_from_mag(ctx, false, &[count])?;
    let memory_term = crate::arith::int_from_mag(ctx, false, &[memory])?;
    access::make_flatmap(
        ctx.heap(),
        &[(count_key, count_term), (memory_key, memory_term)],
    )
}

/// Registers the seven `persistent_term` BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("persistent_term", "get", 0, get_0);
    reg.register("persistent_term", "get", 1, get_1);
    reg.register("persistent_term", "get", 2, get_2);
    reg.register("persistent_term", "put", 2, put_2);
    reg.register("persistent_term", "put_new", 2, put_new_2);
    reg.register("persistent_term", "erase", 1, erase_1);
    reg.register("persistent_term", "info", 0, info_0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_installs_all_seven_keys() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        assert_eq!(reg.len(), 7, "the seven persistent_term keys");
        for (name, arity) in [
            ("get", 0),
            ("get", 1),
            ("get", 2),
            ("put", 2),
            ("put_new", 2),
            ("erase", 1),
            ("info", 0),
        ] {
            assert!(
                reg.contains("persistent_term", name, arity),
                "missing persistent_term:{name}/{arity}"
            );
        }
    }
}
