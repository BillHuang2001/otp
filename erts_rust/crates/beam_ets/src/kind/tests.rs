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

//! Unit tests for [`super`] — the `ets:new/2` option model (`kind.rs`).
//!
//! Split out of `kind.rs` so that file stays within the crate's ~1000-line
//! guideline (crate-layer `CONTEXT.md`, Constraints).

use super::*;
use beam_common::BeamError;

#[test]
fn table_kind_atom_names_and_predicates() {
    assert_eq!(TableKind::Set.as_atom_name(), "set");
    assert_eq!(TableKind::OrderedSet.as_atom_name(), "ordered_set");
    assert_eq!(TableKind::Bag.as_atom_name(), "bag");
    assert_eq!(TableKind::DuplicateBag.as_atom_name(), "duplicate_bag");

    assert!(!TableKind::Set.is_multi());
    assert!(!TableKind::OrderedSet.is_multi());
    assert!(TableKind::Bag.is_multi());
    assert!(TableKind::DuplicateBag.is_multi());

    assert!(!TableKind::Set.is_ordered());
    assert!(TableKind::OrderedSet.is_ordered());
    assert!(!TableKind::Bag.is_ordered());
    assert!(!TableKind::DuplicateBag.is_ordered());
}

#[test]
fn table_kind_default_is_set() {
    assert_eq!(TableKind::default(), TableKind::Set);
}

#[test]
fn access_atom_names_and_default() {
    assert_eq!(Access::Public.as_atom_name(), "public");
    assert_eq!(Access::Protected.as_atom_name(), "protected");
    assert_eq!(Access::Private.as_atom_name(), "private");
    assert_eq!(Access::default(), Access::Protected);
}

#[test]
fn defaults_match_ets_new() {
    let opts = TableOptions::new();
    assert_eq!(opts.kind(), TableKind::Set);
    assert_eq!(opts.keypos(), 1);
    assert_eq!(opts.access(), Access::Protected);
    assert!(!opts.is_named());
    assert_eq!(opts.name(), None);
    assert!(!opts.is_compressed());
    assert!(!opts.write_concurrency());
    assert!(!opts.read_concurrency());
    assert_eq!(opts.heir(), None);
    assert_eq!(opts.decentralized_counters_option(), None);
    assert!(!opts.decentralized_counters());
}

#[test]
fn default_equals_new() {
    let from_new = TableOptions::new();
    let from_default = TableOptions::default();
    assert_eq!(from_new.kind(), from_default.kind());
    assert_eq!(from_new.keypos(), from_default.keypos());
    assert_eq!(from_new.access(), from_default.access());
    assert_eq!(from_new.is_named(), from_default.is_named());
    assert_eq!(from_new.name(), from_default.name());
    assert_eq!(from_new.is_compressed(), from_default.is_compressed());
}

#[test]
fn set_keypos_rejects_zero_but_accepts_positives() {
    let mut opts = TableOptions::new();
    assert!(matches!(opts.set_keypos(0), Err(BeamError::BadArg)));
    // A rejected set must not have mutated the option set.
    assert_eq!(opts.keypos(), 1);

    assert!(opts.set_keypos(3).is_ok());
    assert_eq!(opts.keypos(), 3);
}

#[test]
fn builder_methods_update_and_return_self() {
    let mut opts = TableOptions::new();
    opts.set_kind(TableKind::Bag)
        .set_access(Access::Public)
        .set_compressed(true)
        .set_write_concurrency(true)
        .set_read_concurrency(true)
        .set_named(true);

    assert_eq!(opts.kind(), TableKind::Bag);
    assert_eq!(opts.access(), Access::Public);
    assert!(opts.is_compressed());
    assert!(opts.write_concurrency());
    assert!(opts.read_concurrency());
    assert!(opts.is_named());
}

/// The measured `ets:info(T, decentralized_counters)` rule, row by row.
///
/// Every row is a live-oracle observation (system `erl`, OTP 29.1.1 /
/// erts-17.1) of `ets:new/2` followed by `ets:info(T, decentralized_counters)`
/// and reproduces the C `db_new`: the observable value is
/// `DB_FINE_LOCKED && is_decentralized_counters`, the explicit option
/// overrides the kind's default (which is `true` only for `ordered_set`), and
/// `DB_FINE_LOCKED` needs an effective `write_concurrency`.
#[test]
fn decentralized_counters_follows_the_measured_oracle_rule() {
    // (kind, access, write_concurrency, explicit option, oracle value)
    let rows: &[(TableKind, Access, bool, Option<bool>, bool)] = &[
        // (a) option omitted: only `ordered_set` + effective `write_concurrency`.
        (TableKind::Set, Access::Protected, false, None, false),
        (TableKind::OrderedSet, Access::Protected, false, None, false),
        (TableKind::Bag, Access::Protected, false, None, false),
        (
            TableKind::DuplicateBag,
            Access::Protected,
            false,
            None,
            false,
        ),
        (TableKind::Set, Access::Protected, true, None, false),
        (TableKind::OrderedSet, Access::Protected, true, None, true),
        (TableKind::Bag, Access::Protected, true, None, false),
        (
            TableKind::DuplicateBag,
            Access::Protected,
            true,
            None,
            false,
        ),
        // (b) an explicit option alone is gated on the fine-locked flag.
        (TableKind::Set, Access::Protected, false, Some(true), false),
        (
            TableKind::OrderedSet,
            Access::Protected,
            false,
            Some(true),
            false,
        ),
        (TableKind::Bag, Access::Protected, false, Some(true), false),
        // (c) the explicit option wins for a non-`ordered_set` kind.
        (TableKind::Set, Access::Protected, true, Some(true), true),
        (TableKind::Bag, Access::Protected, true, Some(true), true),
        (
            TableKind::DuplicateBag,
            Access::Protected,
            true,
            Some(true),
            true,
        ),
        // ... and it overrides `ordered_set`'s default both ways.
        (
            TableKind::OrderedSet,
            Access::Protected,
            true,
            Some(true),
            true,
        ),
        (
            TableKind::OrderedSet,
            Access::Protected,
            true,
            Some(false),
            false,
        ),
        (
            TableKind::OrderedSet,
            Access::Protected,
            false,
            Some(false),
            false,
        ),
        (TableKind::Set, Access::Protected, true, Some(false), false),
        // A `private` table never gets the fine-locked flag.
        (TableKind::OrderedSet, Access::Private, true, None, false),
        (TableKind::Set, Access::Private, true, Some(true), false),
        // `read_concurrency` does not affect it.
        (TableKind::OrderedSet, Access::Public, true, None, true),
    ];

    for &(kind, access, write_concurrency, explicit, expected) in rows {
        let mut opts = TableOptions::new();
        opts.set_kind(kind)
            .set_access(access)
            .set_write_concurrency(write_concurrency)
            .set_read_concurrency(write_concurrency)
            .set_decentralized_counters(explicit);
        assert_eq!(
            opts.decentralized_counters(),
            expected,
            "{kind:?} {access:?} write_concurrency={write_concurrency} \
             explicit={explicit:?}"
        );
        assert_eq!(opts.decentralized_counters_option(), explicit);
    }
}

/// Clearing the explicit option restores the kind's default; `TableOptions`
/// never hides the raw option.
#[test]
fn decentralized_counters_option_round_trips_and_resets() {
    let mut opts = TableOptions::new();
    opts.set_kind(TableKind::Set).set_write_concurrency(true);
    assert!(!opts.decentralized_counters());

    opts.set_decentralized_counters(Some(true));
    assert_eq!(opts.decentralized_counters_option(), Some(true));
    assert!(opts.decentralized_counters());

    opts.set_decentralized_counters(None);
    assert_eq!(opts.decentralized_counters_option(), None);
    assert!(!opts.decentralized_counters());

    // `{decentralized_counters,false}` on an `ordered_set` sticks.
    let mut ordered = TableOptions::new();
    ordered
        .set_kind(TableKind::OrderedSet)
        .set_write_concurrency(true)
        .set_decentralized_counters(Some(false));
    assert!(!ordered.decentralized_counters());
}

/// The creation-time heir is stored on the options **as given** (gift data
/// included — the copy into table storage happens later, in
/// [`Table::build`](crate::Table)) and can be cleared, exactly like
/// `{heir, Pid, Data}` / `{heir, none}` at creation. The arity-2
/// `{heir, Pid}` form is the `data: None` heir.
#[test]
fn creation_time_heir_is_stored_and_cleared() {
    let mut opts = TableOptions::new();
    assert_eq!(opts.heir(), None);

    let heir = Heir {
        owner: OwnerId::new(4),
        data: Some(make_small(11)),
    };
    opts.set_heir(Some(heir));
    assert_eq!(opts.heir(), Some(heir));
    assert_eq!(opts.heir().and_then(|h| h.data), Some(make_small(11)));

    // The arity-2 form: an heir with no gift data at all.
    let arity_two = Heir {
        owner: OwnerId::new(4),
        data: None,
    };
    opts.set_heir(Some(arity_two));
    assert_eq!(opts.heir(), Some(arity_two));
    assert_eq!(opts.heir().and_then(|h| h.data), None);

    opts.set_heir(None);
    assert_eq!(opts.heir(), None);
}

#[test]
fn chained_keypos_returns_self() {
    let mut opts = TableOptions::new();
    let returned = opts.set_keypos(2).unwrap();
    assert_eq!(returned.keypos(), 2);
    assert_eq!(opts.keypos(), 2);
}

#[test]
fn set_name_does_not_imply_named() {
    // `name` and `named_table` are independent, as in the oracle:
    // `ets:info(ets:new(n, []), named_table) =:= false` while `name` is `n`.
    let mut opts = TableOptions::new();
    assert!(!opts.is_named());

    let name = Atom::from_raw(7);
    opts.set_name(name);
    assert!(!opts.is_named());
    assert_eq!(opts.name(), Some(name));

    // The `named_table` option is what sets the flag (`set_named`).
    opts.set_named(true);
    assert!(opts.is_named());
    assert_eq!(opts.name(), Some(name));
}

#[test]
fn set_named_false_does_not_clear_name() {
    // `named_table` and the registered name are separate fields; clearing the
    // flag leaves the name intact (the name is only set via `set_name`).
    let mut opts = TableOptions::new();
    opts.set_name(Atom::from_raw(3));
    opts.set_named(false);
    assert!(!opts.is_named());
    assert_eq!(opts.name(), Some(Atom::from_raw(3)));
}

#[test]
fn table_kind_atom_name_round_trip() {
    for kind in [
        TableKind::Set,
        TableKind::OrderedSet,
        TableKind::Bag,
        TableKind::DuplicateBag,
    ] {
        assert_eq!(TableKind::from_atom_name(kind.as_atom_name()), Some(kind));
    }
    assert_eq!(TableKind::from_atom_name("nope"), None);
    assert_eq!(TableKind::from_atom_name(""), None);
}

#[test]
fn access_atom_name_round_trip() {
    for access in [Access::Public, Access::Protected, Access::Private] {
        assert_eq!(Access::from_atom_name(access.as_atom_name()), Some(access));
    }
    assert_eq!(Access::from_atom_name("nope"), None);
    assert_eq!(Access::from_atom_name(""), None);
}

#[test]
fn concurrency_atom_name_round_trip() {
    assert_eq!(Concurrency::None.as_atom_name(), "none");
    assert_eq!(Concurrency::Read.as_atom_name(), "read");
    assert_eq!(Concurrency::Write.as_atom_name(), "write");
    assert_eq!(Concurrency::ReadWrite.as_atom_name(), "read_write");

    for concurrency in [
        Concurrency::None,
        Concurrency::Read,
        Concurrency::Write,
        Concurrency::ReadWrite,
    ] {
        assert_eq!(
            Concurrency::from_atom_name(concurrency.as_atom_name()),
            Some(concurrency)
        );
    }
    assert_eq!(Concurrency::from_atom_name("nope"), None);
    assert_eq!(Concurrency::from_atom_name(""), None);
}

#[test]
fn options_concurrency_derives_from_flags() {
    let mut opts = TableOptions::new();
    assert_eq!(opts.concurrency(), Concurrency::None);

    opts.set_read_concurrency(true);
    assert_eq!(opts.concurrency(), Concurrency::Read);

    opts.set_write_concurrency(true);
    assert_eq!(opts.concurrency(), Concurrency::ReadWrite);

    opts.set_read_concurrency(false);
    assert_eq!(opts.concurrency(), Concurrency::Write);

    opts.set_write_concurrency(false);
    assert_eq!(opts.concurrency(), Concurrency::None);
}

/// A `private` table reports both concurrency flags as `false` however they
/// were requested — and whatever the order, since the rule is applied when the
/// flags are read. Verified against the system `erl` (OTP 29 / erts-17.1):
/// `ets:info(ets:new(t, [private, {write_concurrency,true},
/// {read_concurrency,true}]), write_concurrency) =:= false`.
#[test]
fn private_tables_force_the_concurrency_flags_off() {
    for access in [Access::Public, Access::Protected] {
        let mut opts = TableOptions::new();
        opts.set_access(access)
            .set_write_concurrency(true)
            .set_read_concurrency(true);
        assert!(
            opts.write_concurrency(),
            "{access:?} keeps write_concurrency"
        );
        assert!(opts.read_concurrency(), "{access:?} keeps read_concurrency");
        assert_eq!(opts.concurrency(), Concurrency::ReadWrite);
    }

    // The flag may be set before or after the access mode: both orders agree.
    let mut flag_first = TableOptions::new();
    flag_first
        .set_write_concurrency(true)
        .set_read_concurrency(true)
        .set_access(Access::Private);
    let mut access_first = TableOptions::new();
    access_first
        .set_access(Access::Private)
        .set_write_concurrency(true)
        .set_read_concurrency(true);
    for opts in [&flag_first, &access_first] {
        assert!(!opts.write_concurrency());
        assert!(!opts.read_concurrency());
        assert_eq!(opts.concurrency(), Concurrency::None);
    }

    // `compressed` is **not** forced off by `private` (the oracle reports
    // `compressed => true` for a private compressed table).
    let mut compressed = TableOptions::new();
    compressed.set_access(Access::Private).set_compressed(true);
    assert!(compressed.is_compressed());
    assert!(!compressed.write_concurrency());
}
#[test]
fn can_read_matrix() {
    let a = OwnerId::new(1);
    let b = OwnerId::new(2);

    // Public: anyone may read, regardless of owner/requester.
    for owner in [None, Some(a)] {
        for requester in [None, Some(a), Some(b)] {
            assert!(
                can_read(Access::Public, owner, requester),
                "public read owner={owner:?} requester={requester:?}"
            );
        }
    }

    // Protected: anyone may read, regardless of owner/requester.
    for owner in [None, Some(a)] {
        for requester in [None, Some(a), Some(b)] {
            assert!(
                can_read(Access::Protected, owner, requester),
                "protected read owner={owner:?} requester={requester:?}"
            );
        }
    }

    // Private: only an exact owner match may read.
    assert!(can_read(Access::Private, None, None)); // None == None
    assert!(!can_read(Access::Private, None, Some(a)));
    assert!(!can_read(Access::Private, Some(a), None));
    assert!(can_read(Access::Private, Some(a), Some(a)));
    assert!(!can_read(Access::Private, Some(a), Some(b)));
    assert!(can_read(Access::Private, Some(b), Some(b)));
    assert!(!can_read(Access::Private, Some(b), Some(a)));
}

#[test]
fn can_write_matrix() {
    let a = OwnerId::new(1);
    let b = OwnerId::new(2);

    // Public: anyone may write, regardless of owner/requester.
    for owner in [None, Some(a)] {
        for requester in [None, Some(a), Some(b)] {
            assert!(
                can_write(Access::Public, owner, requester),
                "public write owner={owner:?} requester={requester:?}"
            );
        }
    }

    // Protected: only an exact owner match may write.
    assert!(can_write(Access::Protected, None, None)); // None == None
    assert!(!can_write(Access::Protected, None, Some(a)));
    assert!(!can_write(Access::Protected, Some(a), None));
    assert!(can_write(Access::Protected, Some(a), Some(a)));
    assert!(!can_write(Access::Protected, Some(a), Some(b)));
    assert!(can_write(Access::Protected, Some(b), Some(b)));
    assert!(!can_write(Access::Protected, Some(b), Some(a)));

    // Private: only an exact owner match may write.
    assert!(can_write(Access::Private, None, None)); // None == None
    assert!(!can_write(Access::Private, None, Some(a)));
    assert!(!can_write(Access::Private, Some(a), None));
    assert!(can_write(Access::Private, Some(a), Some(a)));
    assert!(!can_write(Access::Private, Some(a), Some(b)));
    assert!(can_write(Access::Private, Some(b), Some(b)));
    assert!(!can_write(Access::Private, Some(b), Some(a)));
}

#[test]
fn access_matrix_grid_matches_db_masks() {
    // Exhaustive 3 access modes × owner ∈ {None, Some(a)} ×
    // requester ∈ {None, Some(a), Some(b)}, cross-checked against the legacy
    // status-mask model (DB_READ = PROTECTED|PUBLIC, DB_WRITE = PUBLIC) plus
    // the exact `requester == owner` fall-back in handle_lacking_permission.
    let a = OwnerId::new(1);
    let b = OwnerId::new(2);
    for access in [Access::Public, Access::Protected, Access::Private] {
        for owner in [None, Some(a)] {
            for requester in [None, Some(a), Some(b)] {
                let owner_ok = requester == owner;
                let expect_read = match access {
                    Access::Public | Access::Protected => true,
                    Access::Private => owner_ok,
                };
                let expect_write = match access {
                    Access::Public => true,
                    Access::Protected | Access::Private => owner_ok,
                };
                assert_eq!(
                    can_read(access, owner, requester),
                    expect_read,
                    "read access={access:?} owner={owner:?} requester={requester:?}"
                );
                assert_eq!(
                    can_write(access, owner, requester),
                    expect_write,
                    "write access={access:?} owner={owner:?} requester={requester:?}"
                );
            }
        }
    }
}

/// [`WriteConcurrency`] is the parsed `{write_concurrency, …}` surface: three
/// atom forms plus the hidden explicit lock granularity.
#[test]
fn write_concurrency_atom_names_round_trip() {
    // The atoms `ets:info(T, write_concurrency)` reports (table_info,
    // erl_db.c:5492-5506).
    assert_eq!(WriteConcurrency::Off.as_atom_name(), Some("false"));
    assert_eq!(WriteConcurrency::On.as_atom_name(), Some("true"));
    assert_eq!(WriteConcurrency::Auto.as_atom_name(), Some("auto"));
    // The fixed-lock form renders as `{debug_hash_fixed_number_of_locks, N}`, a
    // tuple — never an atom.
    assert_eq!(WriteConcurrency::FixedLocks(4).as_atom_name(), None);

    for mode in [
        WriteConcurrency::Off,
        WriteConcurrency::On,
        WriteConcurrency::Auto,
    ] {
        assert_eq!(
            WriteConcurrency::from_atom_name(mode.as_atom_name().expect("an atom form")),
            Some(mode)
        );
    }
    assert_eq!(WriteConcurrency::from_atom_name("nope"), None);
    assert_eq!(WriteConcurrency::from_atom_name(""), None);
    // `from_atom_name` covers the atom forms only; the tuple form is built by the
    // parser, so nothing parses back to it.
    assert_eq!(WriteConcurrency::from_atom_name("fixed_locks"), None);
}

/// `is_enabled` is the C's `is_fine_locked` *before* the `DB_PRIVATE` gate, and
/// `fixed_locks` exposes the requested lock count of the one tuple form.
#[test]
fn write_concurrency_predicates_and_lock_count() {
    assert!(!WriteConcurrency::Off.is_enabled());
    assert!(WriteConcurrency::On.is_enabled());
    assert!(WriteConcurrency::Auto.is_enabled());
    assert!(WriteConcurrency::FixedLocks(1).is_enabled());
    assert!(WriteConcurrency::FixedLocks(32768).is_enabled());

    assert_eq!(WriteConcurrency::Off.fixed_locks(), None);
    assert_eq!(WriteConcurrency::On.fixed_locks(), None);
    assert_eq!(WriteConcurrency::Auto.fixed_locks(), None);
    assert_eq!(WriteConcurrency::FixedLocks(4).fixed_locks(), Some(4));
    assert_eq!(
        WriteConcurrency::FixedLocks(32768).fixed_locks(),
        Some(32768)
    );

    assert_eq!(WriteConcurrency::default(), WriteConcurrency::Off);
}

/// The boolean `set_write_concurrency` API stays the `On`/`Off` projection of
/// the richer mode, so the existing `beam_bifs` call sites are unchanged.
#[test]
fn set_write_concurrency_maps_to_on_and_off() {
    let mut opts = TableOptions::new();
    assert_eq!(opts.write_concurrency_mode(), WriteConcurrency::Off);
    assert!(!opts.write_concurrency());

    opts.set_write_concurrency(true);
    assert!(opts.write_concurrency());
    assert_eq!(opts.write_concurrency_mode(), WriteConcurrency::On);

    opts.set_write_concurrency(false);
    assert!(!opts.write_concurrency());
    assert_eq!(opts.write_concurrency_mode(), WriteConcurrency::Off);

    // The richer forms are reachable through the mode setter, and the boolean
    // projection reports both as enabled.
    opts.set_write_concurrency_mode(WriteConcurrency::Auto);
    assert!(opts.write_concurrency());
    assert_eq!(opts.write_concurrency_mode(), WriteConcurrency::Auto);

    opts.set_write_concurrency_mode(WriteConcurrency::FixedLocks(128));
    assert!(opts.write_concurrency());
    assert_eq!(
        opts.write_concurrency_mode(),
        WriteConcurrency::FixedLocks(128)
    );
    assert_eq!(opts.write_concurrency_mode().fixed_locks(), Some(128));

    // ... and back to the boolean form.
    opts.set_write_concurrency(true);
    assert_eq!(opts.write_concurrency_mode(), WriteConcurrency::On);
}

/// `{write_concurrency, auto}` and the explicit lock granularity both ask for
/// item counters, so they derive `decentralized_counters => true` for **every**
/// kind — unlike `{write_concurrency, true}`, whose derived flag is `true` only
/// for an `ordered_set` (C `ets_new_2`, erl_db.c:2625-2659).
#[test]
fn auto_and_fixed_locks_derive_decentralized_counters() {
    for kind in [
        TableKind::Set,
        TableKind::OrderedSet,
        TableKind::Bag,
        TableKind::DuplicateBag,
    ] {
        let mut auto = TableOptions::new();
        auto.set_kind(kind)
            .set_write_concurrency_mode(WriteConcurrency::Auto);
        assert!(auto.write_concurrency(), "{kind:?} auto is fine-locked");
        assert_eq!(auto.write_concurrency_mode(), WriteConcurrency::Auto);
        assert_eq!(auto.write_concurrency_mode().as_atom_name(), Some("auto"));
        assert!(
            auto.decentralized_counters(),
            "{kind:?} auto keeps counters"
        );

        // The explicit lock granularity is rejected for an `ordered_set` by the
        // option parser, so only the hash kinds can be modelled with it here.
        if !kind.is_ordered() {
            let mut fixed = TableOptions::new();
            fixed
                .set_kind(kind)
                .set_write_concurrency_mode(WriteConcurrency::FixedLocks(4));
            assert!(
                fixed.write_concurrency(),
                "{kind:?} fixed locks are fine-locked"
            );
            assert_eq!(
                fixed.write_concurrency_mode(),
                WriteConcurrency::FixedLocks(4)
            );
            assert!(
                fixed.decentralized_counters(),
                "{kind:?} fixed locks keep counters"
            );
        }
    }

    // A plain `{write_concurrency, true}` still derives `false` for a `set` — the
    // new modes must not have widened that rule.
    let mut on = TableOptions::new();
    on.set_kind(TableKind::Set)
        .set_write_concurrency_mode(WriteConcurrency::On);
    assert!(on.write_concurrency());
    assert!(!on.decentralized_counters());

    // An option set that never mentions write concurrency derives `false` even
    // for an `ordered_set` (the fine-locked flag gates it).
    let ordered = TableOptions::new();
    assert!(!ordered.decentralized_counters());
}

/// The explicit `{decentralized_counters, Bool}` option overrides the derived
/// flag for the `auto`/fixed-locks forms too (C `ets_new_2`, erl_db.c:2719-2721).
#[test]
fn explicit_decentralized_counters_overrides_auto_and_fixed_locks() {
    for mode in [WriteConcurrency::Auto, WriteConcurrency::FixedLocks(2048)] {
        let mut opts = TableOptions::new();
        opts.set_write_concurrency_mode(mode)
            .set_decentralized_counters(Some(false));
        assert!(opts.write_concurrency(), "{mode:?} still fine-locks");
        assert!(
            !opts.decentralized_counters(),
            "{mode:?} + {{decentralized_counters,false}} reports false"
        );
        assert_eq!(opts.decentralized_counters_option(), Some(false));

        // Clearing the explicit option restores the derived `true`.
        opts.set_decentralized_counters(None);
        assert!(opts.decentralized_counters(), "{mode:?} derives true again");
    }

    // ... and the explicit `true` cannot turn the counters on for a mode that is
    // gated off (no effective `write_concurrency`).
    let mut off = TableOptions::new();
    off.set_decentralized_counters(Some(true));
    assert!(!off.decentralized_counters());
}

/// A `private` table never keeps the write-concurrency mode: the fine-locked flag
/// is normalized off, so `ets:info` reports `write_concurrency => false` and
/// `decentralized_counters => false` for **every** requested mode
/// (`if (is_fine_locked && !(status & DB_PRIVATE))`, erl_db.c:2748).
#[test]
fn private_forces_the_write_concurrency_mode_off() {
    for mode in [
        WriteConcurrency::On,
        WriteConcurrency::Auto,
        WriteConcurrency::FixedLocks(64),
    ] {
        for kind in [TableKind::Set, TableKind::OrderedSet] {
            let mut opts = TableOptions::new();
            opts.set_kind(kind)
                .set_write_concurrency_mode(mode)
                .set_access(Access::Private);
            assert!(
                !opts.write_concurrency(),
                "{kind:?} {mode:?} is not fine-locked"
            );
            assert_eq!(
                opts.write_concurrency_mode(),
                WriteConcurrency::Off,
                "{kind:?} {mode:?} normalizes to Off"
            );
            assert!(
                !opts.decentralized_counters(),
                "{kind:?} {mode:?} private has no counters"
            );
        }
    }

    // The normalization is order-independent: access mode first or mode first.
    let mut mode_first = TableOptions::new();
    mode_first
        .set_write_concurrency_mode(WriteConcurrency::Auto)
        .set_access(Access::Private);
    let mut access_first = TableOptions::new();
    access_first
        .set_access(Access::Private)
        .set_write_concurrency_mode(WriteConcurrency::Auto);
    for opts in [&mode_first, &access_first] {
        assert_eq!(opts.write_concurrency_mode(), WriteConcurrency::Off);
        assert!(!opts.write_concurrency());
    }

    // `concurrency()` is unchanged by the richer mode: it still reports the
    // read×write summary of the enabled flags.
    let mut auto = TableOptions::new();
    auto.set_write_concurrency_mode(WriteConcurrency::Auto)
        .set_read_concurrency(true);
    assert_eq!(auto.concurrency(), Concurrency::ReadWrite);
}
