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

//! The access-enforcement **audit**: the corners of the observable `ets` surface
//! that the per-operation matrix in `ets_access.rs` does not reach, each
//! cross-checked against the live system `erl` (OTP 29 / erts-17.1).
//!
//! # The audited gate table
//!
//! Every `ets` verb resolves its table through `db_get_table_aux`
//! (`../../erts/emulator/beam/erl_db.c`), which rejects the call unless the
//! table's `status` carries the requested status mask (`if (!(tb->common.status
//! & what))` → `handle_lacking_permission` → `badarg`). The masks are
//! `DB_INFO = DB_PRIVATE|DB_PROTECTED|DB_PUBLIC` (never fails),
//! `DB_READ = DB_PROTECTED|DB_PUBLIC` and `DB_WRITE = DB_PUBLIC`
//! (`../../erts/emulator/beam/erl_db_util.h`), i.e. exactly the crate's
//! `kind::can_read`/`kind::can_write` over its live `access` field. Audited
//! verb-by-verb against the C call sites:
//!
//! | verb | C mask | crate gate |
//! |---|---|---|
//! | `first`/`last`/`next`/`prev` (+ `*_lookup`), `lookup`, `member`, `lookup_element/3,4`, `slot`, `match/2,3`, `match_object/2,3`, `select*`, `select_count`, `select_reverse*`, `tab2list`, `safe_fixtable` | `DB_READ` | [`TableAccess`] read check |
//! | `insert`/`insert_new`, `delete`, `delete_object`, `delete_all_objects`, `take`, `update_counter/3,4`, `update_element/3`, `match_delete`, `select_delete`, `select_replace`, `rename` | `DB_WRITE` | [`TableAccess`] write check |
//! | `info/1,2` | `DB_INFO` | none (deliberately ungated) |
//! | `setopts/2` (incl. `{protection,_}`), `give_away/3` | `DB_WRITE` + owner test | owner check (the write mask is implied: an owner always has write, a non-owner is rejected by the owner test) |
//!
//! **Verdict: no divergence.** The audit closes the four corners the matrix in
//! `ets_access.rs` does not reach — the `/4` default form and the `false`
//! direction of `ets:safe_fixtable/2` (`B` below), the multi-object
//! `lookup_element` gate (`C`), the field-by-field ungatedness of `ets:info/2`
//! and the introspection census (`A`), and the gate of the *stdlib* wrappers
//! `ets:foldl/3`/`ets:foldr/3`/`ets:match_delete/2`/`ets:setopts(T,[])` (`D`).
//!
//! # What is NOT this crate's
//!
//! Three of the audited verbs are not reachable from this crate, and the audit
//! pins their gate at the oracle so the layer that *does* own them can be checked
//! against it:
//!
//! * `ets:foldl/3`/`ets:foldr/3` are **stdlib Erlang**, not BIFs: they are
//!   `ets:first/1` + `ets:next_lookup/2` loops (`lib/stdlib/src/ets.erl`), so
//!   their gate *is* the read gate this crate implements on `first`/`next`. The
//!   sibling `beam_bifs` reimplements them (`src/ets/adopt.rs` `foldl_3`/
//!   `foldr_3` → `fold` → `take_table_gated(…, Gate::Read)`), which is the gate
//!   pinned here.
//! * `ets:tab2list/1` is `ets:match_object(T, '_')` and `ets:match_delete/2` is
//!   `ets:select_delete(T, ets:match_spec_compile([{Pattern,[],[true]}]))`
//!   (`lib/stdlib/src/ets.erl`) — a read and a write respectively.
//! * `ets:setopts(T, [])` is a **sibling-layer shape**: an empty option list is
//!   not an option this crate's `SetOpt` can express, so the owner test that the
//!   C applies *unconditionally* (`erl_db.c:3212`) is pinned at the oracle here
//!   and implemented in `beam_bifs`'s `setopts_2` (whose `parse_setopts` accepts
//!   `nil` and then applies the same owner check).
//!
//! The oracle is **required by default**: an absent Erlang fails these checks
//! rather than skipping them (`BEAM_ETS_ALLOW_NO_ORACLE=1` opts out;
//! `BEAM_ETS_REQUIRE_ORACLE=1` forces it).

mod common;

use std::collections::HashMap;
use std::time::Duration;

use beam_common::{BeamError, Result};
use beam_ets::{can_read, can_write, Access, OwnerId, Table, TableId, TableOptions};
use beam_term::prelude::*;
use common::*;

/// The wall-clock bound for the live-oracle probe (a wedged `erl` fails the test
/// instead of hanging the suite).
const ORACLE_TIMEOUT: Duration = Duration::from_secs(60);

/// The owning process's [`OwnerId`] in every cell of the audit.
const OWNER: u64 = 1;

/// The non-owning process's [`OwnerId`]: a live process that is never the caller.
const OTHER: u64 = 2;

/// The `{Prot, Who}` pair every fact key is prefixed with: the mode's atom and
/// `owner`/`other`.
const MODES: [(Access, &str); 3] = [
    (Access::Public, "public"),
    (Access::Protected, "protected"),
    (Access::Private, "private"),
];

/// Which side of the owner axis a cell is probed from.
#[derive(Debug, Clone, Copy)]
enum Who {
    Owner,
    Other,
}

/// Both sides, in the oracle program's order.
const WHOS: [Who; 2] = [Who::Owner, Who::Other];

impl Who {
    /// The field the fact key uses (`owner`/`other`).
    fn name(self) -> &'static str {
        match self {
            Who::Owner => "owner",
            Who::Other => "other",
        }
    }

    /// The requester the crate acts as.
    fn requester(self) -> Option<OwnerId> {
        match self {
            Who::Owner => Some(OwnerId::new(OWNER)),
            Who::Other => Some(OwnerId::new(OTHER)),
        }
    }

    /// Whether this side is the table's owner — the `OpGate`-free half of the
    /// audited gate table.
    fn is_owner(self) -> bool {
        matches!(self, Who::Owner)
    }
}

/// The table's owner.
fn owner() -> OwnerId {
    OwnerId::new(OWNER)
}

/// A `set` of `access` owned by [`owner`] and seeded with `{1, 10}` — the exact
/// seed of the oracle probe (`ets:new(probe, [set, Prot])` + `ets:insert/2`).
fn audit_table(access: Access) -> Table {
    let mut opts = TableOptions::new();
    opts.set_access(access);
    let mut table = Table::new(TableId::next(), opts).expect("a fresh table is constructible");
    table.set_owner(Some(owner()));
    {
        let mut heap = Heap::new(64);
        let atoms = AtomTable::new();
        let mut as_owner = table.as_requester(Some(owner()));
        as_owner
            .insert(object(&mut heap, 1, 10), &atoms)
            .expect("the owner may seed the table");
    }
    table
}

/// A `bag` of `access` owned by [`owner`] holding **two** objects under key `1`
/// (`{1,10}` and `{1,20}`) — the multi-object seed `ets:lookup_element/3` needs
/// to reach its list-returning branch.
fn audit_bag(access: Access) -> Table {
    let mut opts = TableOptions::new();
    opts.set_kind(beam_ets::TableKind::Bag).set_access(access);
    let mut table = Table::new(TableId::next(), opts).expect("a fresh table is constructible");
    table.set_owner(Some(owner()));
    {
        let mut heap = Heap::new(64);
        let atoms = AtomTable::new();
        let mut as_owner = table.as_requester(Some(owner()));
        as_owner
            .insert(object(&mut heap, 1, 10), &atoms)
            .expect("the owner may seed the table");
        as_owner
            .insert(object(&mut heap, 1, 20), &atoms)
            .expect("the owner may seed the table");
    }
    table
}

/// The oracle's rendering of an operation's outcome: `ok` when permitted,
/// `badarg` when denied. Anything else is a crate defect rather than an access
/// gate, so it panics — the "typed `badarg`, never a wrong result" rule.
fn outcome<T>(result: Result<T>) -> &'static str {
    match result {
        Ok(_) => "ok",
        Err(BeamError::BadArg) => "badarg",
        Err(other) => panic!("a gated operation must be `ok`/`badarg`, got {other:?}"),
    }
}

/// Run the audit probe once and return its `key -> value` facts.
///
/// `test` names the caller so a missing oracle reports the check that did not
/// run. The oracle is required by default (see the module docs).
fn oracle_facts(test: &str) -> HashMap<String, String> {
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            test,
            "no Erlang oracle found (no `$ERTS_ORACLE`, no `erl`/`escript` on `$PATH`)",
        );
        return HashMap::new();
    };
    let output = oracle::run_escript(
        &escript,
        AUDIT_ORACLE_PROGRAM,
        &[],
        "access_audit",
        ORACLE_TIMEOUT,
    );
    parse_facts(&output.stdout)
}

/// One `key -> value` line of the oracle probe (panics — loudly — on a missing
/// key, so a half-dead oracle cannot masquerade as a passing assertion).
fn oracle_fact<'a>(facts: &'a HashMap<String, String>, key: &str) -> &'a str {
    facts
        .get(key)
        .unwrap_or_else(|| panic!("the oracle printed no `{key} -> ...` line"))
        .as_str()
}

/// Parse the probe's `key -> value` lines into a map.
fn parse_facts(stdout: &[u8]) -> HashMap<String, String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| line.split_once(" -> "))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect()
}

/// The audit probe. For each protection mode × `{owner, other}` it emits:
///
/// * `A.<mode>.<who>.<field> -> ok|badarg` — `ets:info/2` for **every** one of the
///   oracle's 15 fields (the ungatedness claim, field by field);
/// * `B.<mode>.<who>.<form> -> …` — the read forms `ets_access.rs` does not reach:
///   `le_default` (`ets:lookup_element/4`, present key), `le_default_absent` (the
///   `/4` form on an absent key) and `fixtable_false` (`ets:safe_fixtable(T,false)`,
///   the *other* direction of the `true` the matrix probes);
/// * `D.<mode>.<who>.<wrapper> -> …` — the **stdlib/BIF wrappers** `foldl`, `foldr`,
///   `tab2list`, `match_delete` and the owner-gated `setopts_empty`
///   (`ets:setopts(T,[])`) / `setopts_protection`;
///
/// plus, once: `A.fields -> [Field, …]` (the `ets:info/1` census in oracle order)
/// and `C.<mode>.<who>.le_all -> …` (`ets:lookup_element/3` on a **bag**, the
/// list-returning branch).
///
/// Every operation runs on its own fresh, owner-seeded table, so no operation's
/// mutation can perturb another fact.
const AUDIT_ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript
%%! -noshell
main(_) ->
    P = fun(K, V) -> io:format("~s -> ~w~n", [K, V]) end,
    C = fun(E) -> try E() of _ -> ok catch error:badarg -> badarg end end,
    Parent = self(),
    %% One op on its own fresh table owned by *this* (parent) process.
    Run = fun(Prot, Who, Op) ->
        T = ets:new(probe, [set, Prot]),
        ets:insert(T, {1, 10}),
        R = case Who of
                owner -> C(fun() -> Op(T) end);
                other ->
                    Ref = make_ref(),
                    spawn(fun() -> Parent ! {Ref, C(fun() -> Op(T) end)} end),
                    receive {Ref, V} -> V after 5000 -> timeout end
            end,
        _ = try ets:delete(T) catch _:_ -> ok end,
        R
    end,
    %% The 15 fields `ets:info/1` reports, in the oracle's own order.
    Fields = [id, decentralized_counters, read_concurrency, write_concurrency,
              compressed, memory, owner, heir, name, size, node, named_table,
              type, keypos, protection],
    lists:foreach(
      fun(Prot) ->
        lists:foreach(
          fun(Who) ->
            %% (A) every `ets:info/2` field, from both sides of the owner axis.
            lists:foreach(
              fun(F) ->
                K = lists:flatten(io_lib:format("A.~p.~p.~p", [Prot, Who, F])),
                P(K, Run(Prot, Who, fun(T) -> ets:info(T, F) end))
              end, Fields),
            %% (B) the read forms the matrix does not reach.
            lists:foreach(
              fun({N, Op}) ->
                K = lists:flatten(io_lib:format("B.~p.~p.~s", [Prot, Who, N])),
                P(K, Run(Prot, Who, Op))
              end,
              [{le_default, fun(T) -> ets:lookup_element(T, 1, 1, 0) end},
               {le_default_absent, fun(T) -> ets:lookup_element(T, 9, 1, 0) end},
               {fixtable_false, fun(T) -> ets:safe_fixtable(T, false) end}]),
            %% (D) the stdlib wrappers and the empty `setopts/2` list.
            lists:foreach(
              fun({N, Op}) ->
                K = lists:flatten(io_lib:format("D.~p.~p.~s", [Prot, Who, N])),
                P(K, Run(Prot, Who, Op))
              end,
              [{foldl, fun(T) -> ets:foldl(fun(_, A) -> A end, 0, T) end},
               {foldr, fun(T) -> ets:foldr(fun(_, A) -> A end, 0, T) end},
               {tab2list, fun(T) -> ets:tab2list(T) end},
               {match_delete, fun(T) -> ets:match_delete(T, {'_','_'}) end},
               {setopts_empty, fun(T) -> ets:setopts(T, []) end},
               {setopts_protection, fun(T) -> ets:setopts(T, {protection, private}) end}])
          end, [owner, other])
      end, [public, protected, private]),
    %% The `ets:info/1` census: the field names, in the oracle's order.
    Census = ets:new(census, []),
    P("A.fields", [A || {A, _} <- ets:info(Census)]),
    %% (C) `ets:lookup_element/3` on a *bag*: the list-returning branch.
    Multi = fun(Prot, Who) ->
        T = ets:new(bagprobe, [bag, Prot]),
        ets:insert(T, {1, 10}), ets:insert(T, {1, 20}),
        R = case Who of
                owner -> C(fun() -> ets:lookup_element(T, 1, 2) end);
                other ->
                    Ref = make_ref(),
                    spawn(fun() -> Parent ! {Ref, C(fun() -> ets:lookup_element(T, 1, 2) end)} end),
                    receive {Ref, V} -> V after 5000 -> timeout end
            end,
        _ = try ets:delete(T) catch _:_ -> ok end,
        R
    end,
    lists:foreach(
      fun(Prot) ->
        lists:foreach(
          fun(Who) ->
            K = lists:flatten(io_lib:format("C.~p.~p.le_all", [Prot, Who])),
            P(K, Multi(Prot, Who))
          end, [owner, other])
      end, [public, protected, private]),
    halt(0).
"#;

/// `ets:info/1,2` is **never** access-gated, field by field: the oracle resolves
/// the table with the `DB_INFO` mask (all three protection bits), so a non-owner
/// of a `private` table — who is denied *every* read and *every* write — still
/// reads all 15 fields. The crate has no `info` wrapper at all (`Table::info`
/// needs no requester) and its only ungated projections are
/// `TableAccess::size`/`is_empty`, so this pins that the metadata a requester
/// observes does not depend on the requester.
#[test]
fn every_ets_info_field_is_ungated_for_every_requester() {
    let facts = oracle_facts("every_ets_info_field_is_ungated_for_every_requester");
    let fields = [
        "id",
        "decentralized_counters",
        "read_concurrency",
        "write_concurrency",
        "compressed",
        "memory",
        "owner",
        "heir",
        "name",
        "size",
        "node",
        "named_table",
        "type",
        "keypos",
        "protection",
    ];

    for (access, mode) in MODES {
        for who in WHOS {
            // The oracle: every field answers, from both sides, in every mode.
            for field in fields {
                let tag = format!("A.{mode}.{}.{field}", who.name());
                assert_eq!(
                    oracle_fact(&facts, &tag),
                    "ok",
                    "{tag}: `ets:info/2` must never be access-gated"
                );
            }

            // The crate: the same metadata through the requester's view — the
            // ungated projections (`size`/`is_empty`) plus the whole `TableInfo`.
            let mut table = audit_table(access);
            let info = table.info();
            let viewed = table.as_requester(who.requester());
            assert_eq!(outcome(viewed.size()), "ok", "{mode}.{}: size is ungated", who.name());
            assert_eq!(
                outcome(viewed.is_empty()),
                "ok",
                "{mode}.{}: is_empty is ungated",
                who.name()
            );
            assert_eq!(
                viewed.requester(),
                who.requester(),
                "{mode}.{}: the view reports its requester",
                who.name()
            );
            // Every field of the ungated struct is readable through the view's
            // requester — the requester cannot be a parameter of the metadata.
            let _ = (
                info.id,
                info.name,
                info.kind,
                info.keypos,
                info.access,
                info.size,
                info.memory,
                info.owner,
                info.heir,
                info.named_table,
                info.compressed,
                info.write_concurrency,
                info.read_concurrency,
                info.decentralized_counters,
            );
        }
    }
}

/// The `ets:info/1` **census** — the field names and their order — is the
/// oracle's own: the crate's [`TableInfo::entries`](beam_ets::TableInfo::entries)
/// renders exactly the 15 fields, in exactly the oracle's order, which is what
/// makes the "never gated" claim above cover the observable surface rather than a
/// subset of it.
#[test]
fn the_introspection_census_matches_the_live_oracle() {
    let facts = oracle_facts("the_introspection_census_matches_the_live_oracle");
    let rendered = oracle_fact(&facts, "A.fields");

    let mut atoms = AtomTable::new();
    let table = audit_table(Access::Protected);
    let entries = table.info().entries(&mut atoms);
    let names: Vec<&str> = entries
        .iter()
        .map(|(name, _)| {
            atoms
                .name(*name)
                .expect("every `ets:info/1` key is an interned atom")
        })
        .collect();
    let mine = format!("[{}]", names.join(","));

    assert_eq!(
        mine, rendered,
        "the `ets:info/1` census (fields and order) must be the oracle's"
    );
    assert_eq!(entries.len(), 15, "the oracle reports 15 `ets:info/1` fields");
}

/// The read forms the per-operation matrix does not reach are read-gated like
/// every other read:
///
/// * `ets:lookup_element/4` (`lookup_element_or` here) — the **default** form,
///   whose absent-key branch must not become an escape hatch around the gate
///   (`le_default` and `le_default_absent` answer identically while denied);
/// * `ets:safe_fixtable(T, false)` (`set_fixation(false)`) — the matrix probes the
///   `true` direction only, and the oracle takes the same `DB_READ` gate for both
///   (`erl_db.c:1148`).
#[test]
fn the_read_forms_the_matrix_omits_follow_the_read_gate() {
    let facts = oracle_facts("the_read_forms_the_matrix_omits_follow_the_read_gate");

    for (access, mode) in MODES {
        for who in WHOS {
            let allowed = can_read(access, Some(owner()), who.requester());
            let mut heap = Heap::new(64);
            let mut out = rebuild_heap_words(1024);
            let atoms = AtomTable::new();

            // `ets:lookup_element(Tab, Key, Pos, Default)` — present key.
            let present = outcome(
                audit_table(access)
                    .as_requester(who.requester())
                    .lookup_element_or(make_small(1), 1, make_small(0), &atoms),
            );
            // …and absent key: the default must not be reachable while denied.
            let absent = outcome(
                audit_table(access)
                    .as_requester(who.requester())
                    .lookup_element_or(make_small(9), 1, make_small(0), &atoms),
            );
            // `ets:safe_fixtable(Tab, false)`.
            let unfix = outcome(
                audit_table(access)
                    .as_requester(who.requester())
                    .set_fixation(false),
            );

            let _ = (&mut heap, &mut out);
            for (form, got) in [
                ("le_default", present),
                ("le_default_absent", absent),
                ("fixtable_false", unfix),
            ] {
                let tag = format!("B.{mode}.{}.{form}", who.name());
                assert_eq!(got, oracle_fact(&facts, &tag), "{tag}: crate vs oracle");
                assert_eq!(
                    got == "ok",
                    allowed,
                    "{tag}: a read form must follow the read gate"
                );
            }
        }
    }
}

/// The multi-object `ets:lookup_element/3` is read-gated **before** the crate's
/// scalar refusal: on a `bag` the scalar form has no answer in this crate
/// (`unsupported_ets`, a deliberate typed refusal — the oracle answers with the
/// *list* of every object's `Pos`-th element), but a denied requester must still
/// get the gate's `badarg`, never the capability marker. The list-returning
/// `lookup_element_all` is the form the BIF layer adopts for the multi kinds and
/// follows the gate exactly.
#[test]
fn the_multi_kind_lookup_element_is_gated_before_its_scalar_refusal() {
    let facts = oracle_facts("the_multi_kind_lookup_element_is_gated_before_its_scalar_refusal");

    for (access, mode) in MODES {
        for who in WHOS {
            let denied = !can_read(access, Some(owner()), who.requester());

            // The gated list form: `ok`/`badarg` exactly like a set's.
            let all = outcome(
                audit_bag(access)
                    .as_requester(who.requester())
                    .lookup_element_all(make_small(1), 2, &AtomTable::new()),
            );
            let tag = format!("C.{mode}.{}.le_all", who.name());
            assert_eq!(all, oracle_fact(&facts, &tag), "{tag}: crate vs oracle");
            assert_eq!(all == "ok", !denied, "{tag}: the list form is read-gated");

            // The scalar form: the gate runs first, so a denial is the gate's
            // `badarg` — the `unsupported_ets` refusal is for an *admitted*
            // requester only.
            let scalar = audit_bag(access)
                .as_requester(who.requester())
                .lookup_element(make_small(1), 2, &AtomTable::new());
            match scalar {
                Err(BeamError::BadArg) => assert!(
                    denied,
                    "{mode}.{}: an admitted requester must not be denied",
                    who.name()
                ),
                Err(other) => {
                    assert!(!denied, "{mode}.{}: a denied requester got the capability marker {other:?} instead of `badarg`", who.name());
                    assert!(
                        !matches!(other, BeamError::BadArg),
                        "the scalar refusal must not be `badarg`"
                    );
                }
                Ok(_) => panic!(
                    "{mode}.{}: the scalar form on a bag must refuse",
                    who.name()
                ),
            }
        }
    }
}

/// The **stdlib/BIF wrappers** inherit the gate of the primitive they are built
/// on — verified at the oracle and matched to the crate's gate for the primitive:
///
/// * `ets:foldl/3`/`ets:foldr/3` are walk loops over `ets:first/1` +
///   `ets:next_lookup/2`, so they are **read**-gated; the crate has no fold
///   (`TableAccess` carries none — the fold BIFs need a `FunCaller` and live in
///   `beam_bifs`), so what is pinned here is that the oracle's fold verdict
///   equals the crate's read gate, which is the gate on `first`/`next`;
/// * `ets:tab2list/1` is `ets:match_object(T,'_')` (**read**) and
///   `ets:match_delete/2` is `ets:select_delete/2` with a compiled
///   `[{Pattern,[],[true]}]` (**write**) — both matched against the crate's own
///   equivalents;
/// * `ets:setopts/2` is **owner**-gated *whatever* the mode and *whatever* the
///   option list: the C checks `tb->common.owner != BIF_P->common.id`
///   unconditionally (`erl_db.c:3212`), so a non-owner of even a `public` table
///   is `badarg` — including for the **empty** list, the shape this crate cannot
///   express (`SetOpt` has no empty form; `beam_bifs`'s `setopts_2` accepts `nil`
///   and then applies the same owner check).
#[test]
fn the_stdlib_wrappers_and_the_empty_setopts_inherit_their_gate() {
    let facts = oracle_facts("the_stdlib_wrappers_and_the_empty_setopts_inherit_their_gate");

    for (access, mode) in MODES {
        for who in WHOS {
            let read = can_read(access, Some(owner()), who.requester());
            let write = can_write(access, Some(owner()), who.requester());
            let is_owner = who.is_owner();
            let mut arena = Heap::new(64);
            let mut out = rebuild_heap_words(1024);
            let atoms = AtomTable::new();

            // (1) The folds: read-gated. The crate's `first` carries that gate,
            //     and the oracle's fold verdict must equal it.
            for fold in ["foldl", "foldr"] {
                let tag = format!("D.{mode}.{}.{fold}", who.name());
                let oracle = oracle_fact(&facts, &tag);
                assert_eq!(
                    oracle == "ok",
                    read,
                    "{tag}: a fold walk is read-gated (oracle `{oracle}`)"
                );
                // …and the crate's read gate is the gate on the primitive the
                // fold starts with (`ets:first/1`).
                let probe = outcome(
                    audit_table(access)
                        .as_requester(who.requester())
                        .first(&atoms),
                );
                assert_eq!(probe == "ok", read, "{tag}: the crate's `first` gate");
            }

            // (2) Themselves: `tab2list` (read) and `match_delete` (write).
            let tab2list = outcome(
                audit_table(access)
                    .as_requester(who.requester())
                    .tab2list(),
            );
            let tag = format!("D.{mode}.{}.tab2list", who.name());
            assert_eq!(tab2list, oracle_fact(&facts, &tag), "{tag}: crate vs oracle");

            let wildcard = {
                // The arena must outlive the call: the tuple lives on it.
                let mut watoms = AtomTable::new();
                arena
                    .tuple(&[
                        interned_atom(&mut watoms, "_"),
                        interned_atom(&mut watoms, "_"),
                    ])
                    .expect("test heap has room")
            };
            let match_delete = outcome(
                audit_table(access)
                    .as_requester(who.requester())
                    .match_delete(wildcard, &atoms, &mut out),
            );
            let tag = format!("D.{mode}.{}.match_delete", who.name());
            assert_eq!(
                match_delete,
                oracle_fact(&facts, &tag),
                "{tag}: crate vs oracle"
            );
            assert_eq!(match_delete == "ok", write, "{tag}: a delete is write-gated");

            // (3) `ets:setopts/2` — owner-gated however the option list is shaped.
            for form in ["setopts_empty", "setopts_protection"] {
                let tag = format!("D.{mode}.{}.{form}", who.name());
                assert_eq!(
                    oracle_fact(&facts, &tag) == "ok",
                    is_owner,
                    "{tag}: the ownership check is unconditional"
                );
            }
            // The crate's own owner gate, probed with a no-op option list (the
            // crate cannot express the empty list; an unchanged
            // `{protection, Current}` is the same "validate, then apply nothing"):
            let current = audit_table(access).access();
            let nil_change = outcome(
                audit_table(access)
                    .as_requester(who.requester())
                    .set_access(current),
            );
            assert_eq!(
                nil_change == "ok",
                is_owner,
                "{mode}.{}: `setopts` is owner-gated",
                who.name()
            );
            let _ = &mut arena;
        }
    }
}
