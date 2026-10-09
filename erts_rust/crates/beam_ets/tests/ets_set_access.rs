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

//! `ets:setopts(Tab, {protection, Access})` — the **post-creation** access setter.
//!
//! `ets:new/2`'s `protection` was the only way to choose a table's access mode
//! until [`Table::set_access`] / [`SetOpt::Access`] landed; this suite pins the
//! whole observable surface of the runtime change:
//!
//! * [`Table::set_access`] is the owner-level primitive (no access check) and
//!   `Table::access()` / `Table::info().access` report the **current** mode;
//! * [`Table::setopts`] with [`SetOpt::Access`] applies the oracle's **owner
//!   gate** — a non-owner is `badarg` and the mode is left untouched (the gate
//!   runs before the mutation);
//! * [`TableAccess::set_access`] is the access-checked entry point (the one a
//!   `beam_bifs` bridge routes `ets:setopts/2` through);
//! * the checks the mode drives ([`Table::check_read`]/[`Table::check_write`],
//!   [`Table::as_requester`]) follow the change immediately;
//! * the change must **not** re-normalize the creation-time fine-locking flags:
//!   `write_concurrency` / `decentralized_counters` keep the value the
//!   `ets:new/2` option set decided, so
//!   `ets:new(T, [public, {write_concurrency, true}])` followed by
//!   `ets:setopts(T, {protection, private})` still reports
//!   `write_concurrency => true`, while a table *created* `private` with the same
//!   option reports `false` (probed against the live `erl`).
//!
//! The final test re-probes those facts against the system oracle; it skips loudly
//! without `erl`/`escript` and `BEAM_ETS_REQUIRE_ORACLE=1` turns the skip into a
//! failure.

mod common;

use std::time::Duration;

use beam_common::BeamError;
use beam_ets::{Access, OwnerId, SetOpt, Table, TableId, TableKind, TableOptions};
use beam_term::prelude::*;
use common::oracle;
use common::*;

/// A wall-clock bound so a wedged oracle fails the test instead of hanging it.
const ORACLE_TIMEOUT: Duration = Duration::from_secs(30);

/// A fresh table of `kind`, created with `access` and, when `write_concurrency`,
/// with the `{write_concurrency, true}` option.
fn table(kind: TableKind, access: Access, write_concurrency: bool) -> Table {
    let mut opts = TableOptions::new();
    opts.set_kind(kind)
        .set_access(access)
        .set_write_concurrency(write_concurrency);
    Table::new(TableId::next(), opts).expect("a fresh table is constructible")
}

/// The owner-level primitive: no access check, and both `access()` and
/// `info().access` follow the change.
#[test]
fn set_access_is_the_owner_level_primitive() {
    let mut t = table(TableKind::Set, Access::Protected, false);
    assert_eq!(t.access(), Access::Protected, "the creation-time mode");
    assert_eq!(t.info().access, Access::Protected);

    // No owner is needed — this is the primitive the runtime itself calls.
    assert_eq!(t.owner(), None);
    t.set_access(Access::Private);
    assert_eq!(t.access(), Access::Private);
    assert_eq!(t.info().access, Access::Private, "info reports the change");

    t.set_access(Access::Public);
    assert_eq!(t.access(), Access::Public);
    assert_eq!(t.info().access, Access::Public);
    // Turning it back does not disturb the objects.
    assert_eq!(t.size(), 0);
}

/// `setopts(SetOpt::Access(_))` is owner-gated: the owner succeeds, a non-owner
/// (and an anonymous caller) is `badarg` with the mode left unchanged.
#[test]
fn setopts_access_is_owner_gated() {
    let mut t = table(TableKind::Set, Access::Public, false);
    let owner = OwnerId::new(1);
    let other = OwnerId::new(2);
    t.set_owner(Some(owner));

    t.setopts(Some(owner), SetOpt::Access(Access::Private))
        .expect("the owner may change the protection");
    assert_eq!(t.access(), Access::Private);
    assert_eq!(t.info().access, Access::Private);

    // A non-owner, and an anonymous caller, are both denied — and change nothing.
    for caller in [Some(other), None] {
        assert!(
            matches!(
                t.setopts(caller, SetOpt::Access(Access::Public)),
                Err(BeamError::BadArg)
            ),
            "only the owner may change the protection: {caller:?}"
        );
        assert_eq!(t.access(), Access::Private, "unchanged after a denied call");
        assert_eq!(t.info().access, Access::Private);
    }

    // The owner can move it back.
    t.setopts(Some(owner), SetOpt::Access(Access::Public))
        .expect("the owner may change the protection back");
    assert_eq!(t.access(), Access::Public);
}

/// [`TableAccess::set_access`] is the access-checked entry point: it supplies the
/// requester and applies the same owner gate.
#[test]
fn the_access_checked_view_can_change_the_protection() {
    let mut t = table(TableKind::Set, Access::Public, false);
    let owner = OwnerId::new(1);
    let other = OwnerId::new(2);
    t.set_owner(Some(owner));

    // The owner, through the view.
    t.as_requester(Some(owner))
        .set_access(Access::Private)
        .expect("the owner may change the protection through the view");
    assert_eq!(t.access(), Access::Private);

    // A non-owner, through the same view: `badarg`, nothing changed.
    assert!(matches!(
        t.as_requester(Some(other)).set_access(Access::Public),
        Err(BeamError::BadArg)
    ));
    assert_eq!(t.access(), Access::Private);
    assert_eq!(t.info().access, Access::Private, "the denied call changed nothing");

    // The generic `setopts` entry point carries the same option (and the same
    // gate): `TableAccess::setopts(SetOpt::Access(_))` is `Table::setopts` with the
    // view's requester supplied as the caller.
    assert!(matches!(
        t.as_requester(Some(other)).setopts(SetOpt::Access(Access::Public)),
        Err(BeamError::BadArg)
    ));
    assert_eq!(t.access(), Access::Private, "still unchanged");
    t.as_requester(Some(owner))
        .setopts(SetOpt::Access(Access::Public))
        .expect("the owner may change the protection through the generic entry point");
    assert_eq!(t.access(), Access::Public);
    assert_eq!(t.info().access, Access::Public);
}

/// The mode change is enforced **immediately** by the read/write checks the BIF
/// layer routes every `ets:*` call through.
#[test]
fn a_runtime_protection_change_is_enforced_by_the_access_checks() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(64);
    let mut t = table(TableKind::Set, Access::Public, false);
    let owner = OwnerId::new(1);
    let other = OwnerId::new(2);
    t.set_owner(Some(owner));
    let object = object(&mut heap, 1, 10);

    // While `public`, a non-owner may read and write.
    t.as_requester(Some(other))
        .insert(object, &atoms)
        .expect("a `public` table admits any writer");
    assert_eq!(
        t.as_requester(Some(other)).lookup(make_small(1), &atoms).unwrap().len(),
        1
    );
    assert!(t.check_read(Some(other)).is_ok());
    assert!(t.check_write(Some(other)).is_ok());

    // Move it to `private`: the requester's checks turn to `badarg` at once.
    t.as_requester(Some(owner))
        .set_access(Access::Private)
        .expect("the owner may change the protection");
    assert!(matches!(
        t.as_requester(Some(other)).lookup(make_small(1), &atoms),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        t.as_requester(Some(other)).insert(object, &atoms),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(t.check_read(Some(other)), Err(BeamError::BadArg)));
    assert!(matches!(t.check_write(Some(other)), Err(BeamError::BadArg)));
    // The owner still has full access.
    assert!(t.check_read(Some(owner)).is_ok());
    assert!(t.check_write(Some(owner)).is_ok());
    assert_eq!(
        t.as_requester(Some(owner)).lookup(make_small(1), &atoms).unwrap().len(),
        1
    );

    // And moving it back to `public` re-admits the other requester.
    t.as_requester(Some(owner))
        .set_access(Access::Public)
        .expect("the owner may change the protection back");
    assert!(t.as_requester(Some(other)).lookup(make_small(1), &atoms).is_ok());
}

/// The runtime change touches the **access mode only**: the creation-time
/// fine-locking normalization is never recomputed.
#[test]
fn changing_the_protection_does_not_renormalize_the_fine_locking_flags() {
    // Created `public` with `{write_concurrency, true}`: the flag is on.
    let mut t = table(TableKind::Set, Access::Public, true);
    assert!(t.info().write_concurrency);

    t.set_access(Access::Private);
    assert_eq!(t.access(), Access::Private);
    assert!(
        t.info().write_concurrency,
        "a runtime move to `private` must not re-normalize `write_concurrency`"
    );

    // …whereas the same option at *creation* with `private` normalizes it off.
    let created_private = table(TableKind::Set, Access::Private, true);
    assert!(!created_private.info().write_concurrency);

    // `decentralized_counters` is derived from the same un-renormalized flag: an
    // `ordered_set` (whose default is `true`) keeps it across the change.
    let mut ordered = table(TableKind::OrderedSet, Access::Public, true);
    assert!(ordered.info().decentralized_counters);
    let owner = OwnerId::new(1);
    ordered.set_owner(Some(owner));
    ordered
        .setopts(Some(owner), SetOpt::Access(Access::Private))
        .expect("the owner may change the protection");
    assert_eq!(ordered.access(), Access::Private);
    assert!(ordered.info().write_concurrency);
    assert!(ordered.info().decentralized_counters);
}

/// The oracle probe: it prints one line per fact and the model reproduces every
/// one of them. Skips loudly without an oracle; `BEAM_ETS_REQUIRE_ORACLE=1` turns
/// the skip into a failure.
#[test]
fn protection_change_matches_the_live_oracle() {
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            "protection_change_matches_the_live_oracle",
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable `erl`/`escript` on $PATH)",
        );
        return;
    };
    let output = oracle::run_escript(
        &escript,
        PROTECTION_ORACLE_PROGRAM,
        &[],
        "set_access",
        ORACLE_TIMEOUT,
    );
    let observed: Vec<String> = std::str::from_utf8(&output.stdout)
        .expect("the oracle prints UTF-8")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();

    // (a) A non-owner's `{protection, private}` on a `public` table is `badarg` —
    //     observed *before* the owner's own change, so it proves nothing changed.
    // (b) The owner's change is `true` and `ets:info(T, protection)` follows it.
    // (c) The runtime change does not re-normalize `write_concurrency` /
    //     `decentralized_counters`, while a table *created* `private` is normalized
    //     off.
    let expected = [
        "a_non_owner {error,badarg}",
        "b_before public",
        "b_result true",
        "b_after private",
        "c_write_concurrency_after true",
        "c_write_concurrency_created_private false",
        "c_decentralized_before true",
        "c_decentralized_after true",
    ];
    assert_eq!(
        observed,
        expected,
        "the oracle's protection-change observations changed — the model is stale:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );

    // The model reproduces the same facts, through the public API.
    let owner = OwnerId::new(1);
    let non_owner = OwnerId::new(2);

    // (a) a non-owner cannot change the protection, and nothing changes.
    let mut t = table(TableKind::Set, Access::Public, true);
    t.set_owner(Some(owner));
    assert!(matches!(
        t.as_requester(Some(non_owner)).set_access(Access::Private),
        Err(BeamError::BadArg)
    ));
    assert_eq!(t.info().access, Access::Public, "b_before");

    // (b) the owner may, and `info().access` follows.
    assert!(
        t.as_requester(Some(owner)).set_access(Access::Private).is_ok(),
        "b_result"
    );
    assert_eq!(t.info().access, Access::Private, "b_after");

    // (c) the creation-time fine-locking flags are not re-normalized…
    assert!(t.info().write_concurrency, "c_write_concurrency_after");
    // …while a table created `private` with the same option is normalized off.
    let created_private = table(TableKind::Set, Access::Private, true);
    assert!(
        !created_private.info().write_concurrency,
        "c_write_concurrency_created_private"
    );

    // `decentralized_counters` rides the same flag (`ordered_set` default: true).
    let mut ordered = table(TableKind::OrderedSet, Access::Public, true);
    ordered.set_owner(Some(owner));
    assert!(ordered.info().decentralized_counters, "c_decentralized_before");
    assert!(ordered.as_requester(Some(owner)).set_access(Access::Private).is_ok());
    assert_eq!(ordered.info().access, Access::Private);
    assert!(ordered.info().decentralized_counters, "c_decentralized_after");
}

/// The oracle program: a non-owner's denied `{protection, …}`, an owner's accepted
/// one, and the fine-locking flags before and after.
const PROTECTION_ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript

main(_) ->
    T1 = ets:new(pub_table, [set, public, {write_concurrency, true}]),
    Parent = self(),
    spawn(fun() ->
        Result = try ets:setopts(T1, {protection, private}) of
                     V -> {ok, V}
                 catch
                     error:R -> {error, R}
                 end,
        Parent ! {non_owner_setopts, Result}
    end),
    NonOwner = receive {non_owner_setopts, R} -> R end,

    %% Read *after* the denied call: it must not have changed anything.
    Before = ets:info(T1, protection),
    Result = ets:setopts(T1, {protection, private}),
    After = ets:info(T1, protection),
    WcAfter = ets:info(T1, write_concurrency),

    T2 = ets:new(priv_table, [set, private, {write_concurrency, true}]),
    WcCreatedPrivate = ets:info(T2, write_concurrency),

    T3 = ets:new(os_table, [ordered_set, public, {write_concurrency, true}]),
    DcBefore = ets:info(T3, decentralized_counters),
    true = ets:setopts(T3, {protection, private}),
    DcAfter = ets:info(T3, decentralized_counters),

    io:format("a_non_owner ~p~n", [NonOwner]),
    io:format("b_before ~p~n", [Before]),
    io:format("b_result ~p~n", [Result]),
    io:format("b_after ~p~n", [After]),
    io:format("c_write_concurrency_after ~p~n", [WcAfter]),
    io:format("c_write_concurrency_created_private ~p~n", [WcCreatedPrivate]),
    io:format("c_decentralized_before ~p~n", [DcBefore]),
    io:format("c_decentralized_after ~p~n", [DcAfter]),
    halt(0).
"#;
