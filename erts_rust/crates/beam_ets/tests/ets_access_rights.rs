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

//! The **rights corners**: what a `ets:give_away/3` transfer does to the giver's
//! rights, *when* a `{protection, _}` change takes effect, and whether a denied
//! continuation resume consumes the continuation.
//!
//! `tests/ets_access.rs` pins the per-operation gate (one operation, one
//! requester) and `tests/ets_access_audit.rs` pins the corners of that gate. Both
//! probe a table whose owner never changes. This suite pins the three remaining
//! *timing/ownership* corners, each against the live system `erl` (OTP 29.1.1 /
//! erts-17.1):
//!
//! 1. **What a `give_away/3` costs the giver**
//!    ([`the_give_away_rights_matrix_matches_the_live_oracle`]). After the transfer
//!    the giver is an ordinary non-owner: on a `protected` table it keeps READ and
//!    loses WRITE, on a `private` table it loses both, and on a `public` table it
//!    keeps everything — while `ets:setopts/2` stays **owner**-gated in all three
//!    modes (the ownership operations never consult `protection`).
//! 2. **When a protection flip takes effect**
//!    ([`the_protection_flip_takes_effect_for_the_next_op_and_matches_the_live_oracle`]):
//!    `ets:setopts(T, {protection, _})` applies to the *next* operation, and
//!    `ets:info/2` stays ungated across every flip.
//! 3. **A denied continuation resume is not a consumption**
//!    ([`a_denied_continuation_resume_stays_resumable_and_matches_the_live_oracle`]):
//!    a `badarg` on `ets:select/1` leaves the continuation intact, so the same
//!    continuation resumes to the same chunk once the gate reopens.
//!
//! # The host-owned half
//!
//! Whole-table `ets:delete/1` has **no** `beam_ets` primitive: the runtime takes
//! the table out of its registry under a write gate
//! (`beam_bifs/src/ets.rs::delete_1` → `take_table_gated(ctx, handle,
//! Gate::Write)`). The oracle's facts for it are therefore pinned here (`badarg`
//! from a non-owner of a `protected` table, `true` from a non-owner of a `public`
//! one) and the crate side is recorded as [`None`] — *host-owned* — in the trace,
//! with the exact `beam_bifs` call site named in [`HOST_ONLY_DELETE`].
//!
//! The oracle is **required by default** (a missing Erlang fails these checks;
//! `BEAM_ETS_ALLOW_NO_ORACLE=1` opts out with a loud skip, `BEAM_ETS_REQUIRE_ORACLE=1`
//! forces it).

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use beam_common::BeamError;
use beam_ets::{Access, OwnerId, SetOpt, Table, TableId, TableOptions};
use beam_term::prelude::*;
use common::oracle;
use common::*;

/// Wall-clock bound for the oracle child.
const ORACLE_TIMEOUT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// The shared shape both sides compare
// ---------------------------------------------------------------------------

/// The observable outcome of one operation, gate-wise.
///
/// Deliberately two-valued: every `ets` gate answers `ok` (whatever the value) or
/// `error:badarg`, so a richer enum would only invite the test to pin *values* this
/// suite does not own (the value pins live in `tests/ets_access.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// The operation returned (`true`, a list, a count, a pid comparison, …).
    Ok,
    /// The operation raised `error:badarg` — the gate denied it.
    BadArg,
}

/// The verdict of one crate operation: `Ok` or `badarg`, never anything else.
///
/// Any other outcome (a typed `unsupported_ets` raise, an allocation failure, …)
/// is a **bug in the probe**, not a gate answer, so it panics rather than being
/// folded into one of the two columns.
fn verdict(result: beam_common::Result<()>) -> Verdict {
    match result {
        Ok(()) => Verdict::Ok,
        Err(BeamError::BadArg) => Verdict::BadArg,
        Err(other) => panic!("a gated operation must be ok/badarg, got {other:?}"),
    }
}

/// The verdict of a boolean fact the oracle prints as `true`/`false`.
///
/// Every boolean tag in this suite is pinned [`Verdict::Ok`] (the oracle prints
/// `true`), so a `false` here is a column mismatch naming the tag rather than a
/// silent pass. A boolean is never a gate denial; reporting it as
/// [`Verdict::BadArg`] is only what makes the mismatch visible.
fn bool_verdict(cond: bool) -> Verdict {
    if cond {
        Verdict::Ok
    } else {
        Verdict::BadArg
    }
}

/// The keys of `{Key, Value}` objects, in the order they were produced.
fn keys_of(objects: &[Eterm]) -> Vec<i64> {
    objects.iter().map(|object| pair_of(*object).0).collect()
}

/// A crate-side trace: `(tag, Some(verdict))` for a tag this crate can express,
/// `(tag, None)` for a **host-owned** tag (no crate primitive exists).
type Trace = Vec<(&'static str, Option<Verdict>)>;

/// One parsed oracle line: the raw `~w`-printed value and the verdict it implies.
#[derive(Debug)]
struct OracleLine {
    value: String,
    verdict: Verdict,
}

/// Parse the oracle's `<tag> -> <value>` lines.
///
/// The embedded programs wrap every operation in
/// `try {ok, F()} catch error:badarg -> badarg; Cl:R -> {Cl, R} end`, so `badarg`
/// is the only *denial* the wrapper can report: any other exception renders as a
/// `{Class, Reason}` tuple and panics here (it would mean the oracle diverged from
/// the model, not that a gate closed).
fn parse_oracle(stdout: &[u8]) -> BTreeMap<String, OracleLine> {
    let text = String::from_utf8_lossy(stdout);
    let mut lines = BTreeMap::new();
    for line in text.lines() {
        let Some((tag, value)) = line.trim().split_once(" -> ") else {
            continue;
        };
        let value = value.trim().to_owned();
        let verdict = match value.as_str() {
            "badarg" => Verdict::BadArg,
            other if other.starts_with('{') => panic!(
                "the oracle raised a non-badarg exception (`{other}`) for `{tag}`; the probe \
                 wrapper only ever reports badarg as a denial"
            ),
            _ => Verdict::Ok,
        };
        lines.insert(tag.to_owned(), OracleLine { value, verdict });
    }
    lines
}

/// Run the shared rights oracle program and return its parsed lines.
fn run_rights_oracle(test: &str) -> Option<BTreeMap<String, OracleLine>> {
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            test,
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable `erl`/`escript` on $PATH)",
        );
        return None;
    };
    let output = oracle::run_escript(&escript, RIGHTS_PROGRAM, &[], "rights", ORACLE_TIMEOUT);
    Some(parse_oracle(&output.stdout))
}

/// The `(tag, value, verdict)` of an oracle line, panicking with the whole transcript
/// when the tag is missing.
fn line<'a>(
    lines: &'a BTreeMap<String, OracleLine>,
    tag: &str,
    transcript: &str,
) -> &'a OracleLine {
    lines.get(tag).unwrap_or_else(|| {
        panic!("the oracle printed no `{tag} ->` line:\n{transcript}")
    })
}

// ---------------------------------------------------------------------------
// Test A — the give-away rights matrix
// ---------------------------------------------------------------------------

/// The 15 facts of the give-away matrix, **in the order both sides print them**.
///
/// The first three are ownership facts, the next seven are the *giver's* view after
/// the transfer, and the last five are the *recipient's*. Only the final
/// `recipient.delete_table` is host-owned (whole-table `ets:delete/1`).
const MATRIX_TAGS: [&str; 15] = [
    "before.owner_is_self",
    "giveaway",
    "after.owner_is_child",
    "giver.insert",
    "giver.lookup",
    "giver.size",
    "giver.delete_key",
    "giver.fixtable",
    "giver.select_replace",
    "giver.setopts",
    "recipient.insert",
    "recipient.lookup",
    "recipient.owner_is_self",
    "recipient.setopts_ok",
    "recipient.delete_table",
];

/// The oracle-measured matrix for `access`, one verdict per [`MATRIX_TAGS`] entry.
///
/// * `protected` — the giver keeps READ (`lookup`, `safe_fixtable` is a READ) and
///   loses every WRITE; `setopts` is owner-gated.
/// * `private` — the giver loses read too, **except** `ets:info(T, size)`, which is
///   never gated.
/// * `public` — the giver keeps full read/write rights; `setopts` is **still**
///   owner-gated (the giver is no longer the owner).
fn pinned(access: Access) -> [Verdict; 15] {
    use Verdict::{BadArg, Ok};
    match access {
        Access::Protected => [
            Ok, Ok, Ok, // before.owner_is_self, giveaway, after.owner_is_child
            BadArg, Ok, Ok, BadArg, Ok, BadArg, BadArg, // giver.*
            Ok, Ok, Ok, Ok, Ok, // recipient.*
        ],
        Access::Private => [
            Ok, Ok, Ok, // before.owner_is_self, giveaway, after.owner_is_child
            BadArg, BadArg, Ok, BadArg, BadArg, BadArg, BadArg, // giver.*
            Ok, Ok, Ok, Ok, Ok, // recipient.*
        ],
        Access::Public => [
            Ok, Ok, Ok, // before.owner_is_self, giveaway, after.owner_is_child
            Ok, Ok, Ok, Ok, Ok, Ok, BadArg, // giver.*
            Ok, Ok, Ok, Ok, Ok, // recipient.*
        ],
    }
}

/// The protections, in the oracle program's order, with the atom it prints.
const PROTECTIONS: [(Access, &str); 3] = [
    (Access::Protected, "protected"),
    (Access::Private, "private"),
    (Access::Public, "public"),
];

/// A fresh table with `access` and `owner`, the crate-side fixture of every
/// protection row.
fn crate_table(access: Access, owner: OwnerId) -> Table {
    let mut opts = TableOptions::new();
    opts.set_access(access);
    let mut table = Table::new(TableId::next(), opts).expect("a fresh table is constructible");
    table.set_owner(Some(owner));
    table
}

/// Run the give-away matrix on the crate side for `access` and return the trace.
///
/// The operations run **in [`MATRIX_TAGS`] order** — the same order as the oracle
/// program — because they mutate the table and so are order-dependent.
fn crate_matrix(access: Access) -> Trace {
    let owner = OwnerId::new(1);
    let recipient = OwnerId::new(2);
    let mut atoms = AtomTable::new();
    let mut arena = Heap::new(64);
    let mut out = rebuild_heap_words(4096);
    let replace_spec = key_keeping_spec(&mut arena, &mut atoms);

    let mut table = crate_table(access, owner);
    table
        .as_requester(Some(owner))
        .insert(object(&mut arena, 1, 10), &atoms)
        .expect("the owner seeds the table");

    let mut trace: Trace = Vec::with_capacity(MATRIX_TAGS.len());
    trace.push((
        "before.owner_is_self",
        Some(bool_verdict(table.owner() == Some(owner))),
    ));
    trace.push((
        "giveaway",
        Some(verdict(
            table
                .as_requester(Some(owner))
                .give_away(Some(recipient)),
        )),
    ));
    trace.push((
        "after.owner_is_child",
        Some(bool_verdict(table.owner() == Some(recipient))),
    ));
    trace.push((
        "giver.insert",
        Some(verdict(table.as_requester(Some(owner)).insert(
            object(&mut arena, 2, 20),
            &atoms,
        ))),
    ));
    trace.push((
        "giver.lookup",
        Some(verdict(
            table
                .as_requester(Some(owner))
                .lookup(make_small(1), &atoms)
                .map(|_| ()),
        )),
    ));
    trace.push((
        "giver.size",
        Some(verdict(
            table.as_requester(Some(owner)).size().map(|_| ()),
        )),
    ));
    trace.push((
        "giver.delete_key",
        Some(verdict(
            table
                .as_requester(Some(owner))
                .delete(make_small(1), &atoms),
        )),
    ));
    trace.push((
        "giver.fixtable",
        Some(verdict(
            table.as_requester(Some(owner)).set_fixation(true),
        )),
    ));
    // Whatever the gate decided, the (new) owner clears the flag so the table is
    // left unfixed for the rest of the row.
    table
        .as_requester(Some(recipient))
        .set_fixation(false)
        .expect("the owner may unfix");
    trace.push((
        "giver.select_replace",
        Some(verdict(
            table
                .as_requester(Some(owner))
                .select_replace(&replace_spec, &atoms, &mut out)
                .map(|_| ()),
        )),
    ));
    trace.push((
        "giver.setopts",
        Some(verdict(
            table
                .as_requester(Some(owner))
                .setopts(SetOpt::Access(Access::Public)),
        )),
    ));
    trace.push((
        "recipient.insert",
        Some(verdict(table.as_requester(Some(recipient)).insert(
            object(&mut arena, 3, 30),
            &atoms,
        ))),
    ));
    trace.push((
        "recipient.lookup",
        Some(verdict(
            table
                .as_requester(Some(recipient))
                .lookup(make_small(1), &atoms)
                .map(|_| ()),
        )),
    ));
    trace.push((
        "recipient.owner_is_self",
        Some(bool_verdict(table.owner() == Some(recipient))),
    ));
    trace.push((
        "recipient.setopts_ok",
        Some(verdict(
            table
                .as_requester(Some(recipient))
                .setopts(SetOpt::Access(Access::Protected)),
        )),
    ));
    // No crate primitive: whole-table `ets:delete/1` is the runtime's.
    trace.push(("recipient.delete_table", None));
    trace
}

/// A `ets:give_away/3` transfer rewrites the giver's rights — and the live oracle,
/// the pinned model and the crate all agree, row by row.
#[test]
fn the_give_away_rights_matrix_matches_the_live_oracle() {
    const NAME: &str = "the_give_away_rights_matrix_matches_the_live_oracle";
    let Some(lines) = run_rights_oracle(NAME) else {
        return;
    };
    let transcript = format!("{lines:#?}");

    for (access, name) in PROTECTIONS {
        let expected = pinned(access);
        let trace = crate_matrix(access);
        assert_eq!(
            trace.len(),
            MATRIX_TAGS.len(),
            "every protection row has one fact per tag"
        );

        for (index, tag) in MATRIX_TAGS.iter().enumerate() {
            let full = format!("{name}.{tag}");
            let oracle_line = line(&lines, &full, &transcript);
            let (crate_tag, crate_verdict) = trace[index];
            assert_eq!(crate_tag, *tag, "the crate trace is in the oracle's order");

            assert_eq!(
                oracle_line.verdict, expected[index],
                "`{full}`: the oracle printed `{}` where the pinned model says {:?} — the model \
                 is stale",
                oracle_line.value, expected[index]
            );

            match crate_verdict {
                Some(actual) => assert_eq!(
                    actual, expected[index],
                    "`{full}`: the crate answered {actual:?} where the oracle pins {:?} \
                     (oracle printed `{}`)",
                    expected[index], oracle_line.value
                ),
                None => assert_eq!(
                    expected[index],
                    Verdict::Ok,
                    "`{full}` is host-owned, so the pinned model may only record the oracle's \
                     success"
                ),
            }
        }

        // The recipient's whole-table delete is the single host-owned fact of a row.
        let host_owned = trace.iter().filter(|(_, verdict)| verdict.is_none()).count();
        assert_eq!(
            host_owned, 1,
            "{name}: exactly one tag must be host-owned (`recipient.delete_table`), found \
             {host_owned}"
        );
    }
}

/// The gift-carrying transfer's crate obligation: the returned [`Transfer`] names
/// the recipient it went to and the giver it came from.
///
/// One compact assertion — the full gift coverage (deep copy, `{'ETS-TRANSFER', …}`
/// shape, memory accounting) lives in `tests/ets_ownership.rs`.
#[test]
fn the_gift_transfer_names_the_recipient_and_the_giver() {
    let owner = OwnerId::new(1);
    let recipient = OwnerId::new(2);
    let mut arena = Heap::new(16);
    let gift = object(&mut arena, 5, 50);
    let mut table = crate_table(Access::Protected, owner);

    let transfer = table
        .as_requester(Some(owner))
        .give_away_with_gift(Some(recipient), gift)
        .expect("the owner may transfer with a gift");
    assert_eq!(transfer.to, recipient, "the transfer names the new owner");
    assert_eq!(transfer.from, Some(owner), "…and the giver");
    assert_eq!(table.owner(), Some(recipient));
}

// ---------------------------------------------------------------------------
// Test B — the protection flip's timing
// ---------------------------------------------------------------------------

/// The seven gap-free flip facts, in the oracle program's order.
const FLIP_TAGS: [&str; 7] = [
    "flip.protected.nonowner_lookup",
    "flip.owner.setopts_private",
    "flip.private.nonowner_lookup",
    "flip.private.nonowner_info_size",
    "flip.private.nonowner_setopts",
    "flip.owner.setopts_public",
    "flip.public.nonowner_insert",
];

/// The oracle-measured verdicts of [`FLIP_TAGS`]: a flip applies to the **next**
/// operation, and `ets:info/2`'s `size` never closes.
const FLIP_PINNED: [Verdict; 7] = [
    Verdict::Ok,     // nonowner lookup, protected
    Verdict::Ok,     // owner setopts private
    Verdict::BadArg, // nonowner lookup, now private
    Verdict::Ok,     // nonowner info size, still ungated
    Verdict::BadArg, // nonowner setopts, owner-gated
    Verdict::Ok,     // owner setopts public
    Verdict::Ok,     // nonowner insert, now public
];

/// The two whole-table-delete facts, in the oracle program's order.
const DELETE_TAGS: [&str; 2] = ["del.protected.nonowner", "del.public.nonowner"];

/// The oracle-measured verdicts of [`DELETE_TAGS`]: `ets:delete/1` is
/// `DB_WRITE`-gated, so a non-owner may delete a `public` table and not a
/// `protected` one.
const DELETE_PINNED: [Verdict; 2] = [Verdict::BadArg, Verdict::Ok];

/// The tags that are **host-owned**: no crate primitive exists for a whole-table
/// delete, so the crate side is `None` and the fact is pinned at the oracle only.
///
/// Each entry is `(tag, pinned verdict, the exact host call site that owns it)`.
const HOST_ONLY_DELETE: [(&str, Verdict, &str); 3] = [
    (
        "flip.public.nonowner_delete_table",
        Verdict::Ok,
        "beam_bifs/src/ets.rs::delete_1 -> take_table_gated(ctx, handle, Gate::Write), then \
         TableRegistry::remove/forget_profile",
    ),
    (
        "del.protected.nonowner",
        Verdict::BadArg,
        "beam_bifs/src/ets.rs::delete_1 -> take_table_gated(ctx, handle, Gate::Write)",
    ),
    (
        "del.public.nonowner",
        Verdict::Ok,
        "beam_bifs/src/ets.rs::delete_1 -> take_table_gated(ctx, handle, Gate::Write), then \
         TableRegistry::remove/forget_profile",
    ),
];

/// A protection change applies to the *next* operation — and the crate's gate and
/// the oracle's agree at every step, including the flips back.
#[test]
fn the_protection_flip_takes_effect_for_the_next_op_and_matches_the_live_oracle() {
    const NAME: &str = "the_protection_flip_takes_effect_for_the_next_op_and_matches_the_live_oracle";
    let Some(lines) = run_rights_oracle(NAME) else {
        return;
    };
    let transcript = format!("{lines:#?}");

    // The oracle column of the flip section.
    for (index, tag) in FLIP_TAGS.iter().enumerate() {
        let oracle_line = line(&lines, tag, &transcript);
        assert_eq!(
            oracle_line.verdict, FLIP_PINNED[index],
            "`{tag}`: the oracle printed `{}` where the pinned model says {:?}",
            oracle_line.value, FLIP_PINNED[index]
        );
    }
    // …and of the whole-table-delete section.
    for (index, tag) in DELETE_TAGS.iter().enumerate() {
        let oracle_line = line(&lines, tag, &transcript);
        assert_eq!(
            oracle_line.verdict, DELETE_PINNED[index],
            "`{tag}`: the oracle printed `{}` where the pinned model says {:?}",
            oracle_line.value, DELETE_PINNED[index]
        );
    }
    // The host-owned tags: the oracle's own answer is recorded in the table.
    for (tag, expected, _) in HOST_ONLY_DELETE {
        let oracle_line = line(&lines, tag, &transcript);
        assert_eq!(
            oracle_line.verdict, expected,
            "`{tag}`: the oracle printed `{}` where the handoff table says {expected:?}",
            oracle_line.value
        );
    }
    // The two "the table is really gone / really there" follow-ups.
    for (tag, expected) in [
        ("flip.public.table_gone", "undefined"),
        ("del.protected.table_alive", "0"),
        ("del.public.table_gone", "undefined"),
    ] {
        assert_eq!(
            line(&lines, tag, &transcript).value,
            expected,
            "`{tag}`: the oracle's rendering changed"
        );
    }

    // The crate column: the same seven flips, in the same order, on fresh tables.
    let owner = OwnerId::new(1);
    let other = OwnerId::new(2);
    let atoms = AtomTable::new();
    let mut arena = Heap::new(64);
    let mut table = crate_table(Access::Protected, owner);
    table
        .as_requester(Some(owner))
        .insert(object(&mut arena, 1, 10), &atoms)
        .expect("the owner seeds the table");

    let mut trace: Trace = Vec::with_capacity(FLIP_TAGS.len());
    trace.push((
        "flip.protected.nonowner_lookup",
        Some(verdict(
            table
                .as_requester(Some(other))
                .lookup(make_small(1), &atoms)
                .map(|_| ()),
        )),
    ));
    trace.push((
        "flip.owner.setopts_private",
        Some(verdict(
            table
                .as_requester(Some(owner))
                .setopts(SetOpt::Access(Access::Private)),
        )),
    ));
    trace.push((
        "flip.private.nonowner_lookup",
        Some(verdict(
            table
                .as_requester(Some(other))
                .lookup(make_small(1), &atoms)
                .map(|_| ()),
        )),
    ));
    trace.push((
        "flip.private.nonowner_info_size",
        Some(verdict(
            table.as_requester(Some(other)).size().map(|_| ()),
        )),
    ));
    let before_denial = table.access();
    trace.push((
        "flip.private.nonowner_setopts",
        Some(verdict(
            table
                .as_requester(Some(other))
                .setopts(SetOpt::Access(Access::Private)),
        )),
    ));
    assert_eq!(
        table.access(),
        before_denial,
        "a denied `setopts` must change nothing (it is owner-gated before the mutation)"
    );
    trace.push((
        "flip.owner.setopts_public",
        Some(verdict(
            table
                .as_requester(Some(owner))
                .setopts(SetOpt::Access(Access::Public)),
        )),
    ));
    trace.push((
        "flip.public.nonowner_insert",
        Some(verdict(table.as_requester(Some(other)).insert(
            object(&mut arena, 2, 20),
            &atoms,
        ))),
    ));

    for (index, (tag, crate_verdict)) in trace.iter().enumerate() {
        assert_eq!(*tag, FLIP_TAGS[index], "the crate trace is in the oracle's order");
        assert_eq!(
            crate_verdict.expect("every flip tag is crate-expressible"),
            FLIP_PINNED[index],
            "`{tag}`: the crate answered {crate_verdict:?} where the oracle pins {:?}",
            FLIP_PINNED[index]
        );
    }

    // Introspection needs no requester at all, in every mode.
    let info = table.info();
    assert_eq!(info.access, Access::Public, "the last flip is the live mode");
    assert_eq!(info.size, 2, "the non-owner's insert landed on the public table");
    assert_eq!(table.size(), 2);

    // The three host-owned tags are exactly the whole-table deletes: there is no
    // crate primitive to compare against.
    assert_eq!(
        HOST_ONLY_DELETE.len(),
        3,
        "exactly three whole-table-delete facts are host-owned"
    );
    for (tag, _, site) in HOST_ONLY_DELETE {
        assert!(
            site.contains("delete_1"),
            "`{tag}` must name the beam_bifs`ets:delete/1` call site it is handed off to, got \
             `{site}`"
        );
    }
}

// ---------------------------------------------------------------------------
// Test C — a denied continuation resume is not a consumption
// ---------------------------------------------------------------------------

/// An `ordered_set` of `{K, K*10}` for `K` in `1..=10`, chunked in threes while the
/// protection flips underneath the continuation.
///
/// Measured: `cont.first -> [1,2,3]`, a `private` non-owner resume is `badarg`, and
/// the **same** continuation then resumes to `[4,5,6]` — twice, identically (a
/// continuation is immutable, and a denial does not consume it).
const CONTINUATION_PROGRAM: &str = r#"#!/usr/bin/env escript
main(_) ->
    P = fun(K, V) -> io:format("~s -> ~w~n", [K, V]) end,
    C = fun(F) -> try {ok, F()} catch error:badarg -> badarg; Cl:R -> {Cl, R} end end,
    R = fun({ok, V}) -> V; (X) -> X end,
    Me = self(),
    Eval = fun(Fun) -> Child = get(child), Child ! {eval, Me, Fun},
                      receive {eval, X} -> X end end,
    Child = spawn(fun() -> child() end),
    put(child, Child),
    T5 = ets:new(conttab, [protected, ordered_set]),
    [ets:insert(T5, {K, K * 10}) || K <- lists:seq(1, 10)],
    Keys = fun(Res) -> case Res of
                           '$end_of_table' -> '$end_of_table';
                           {Chunk, _Next} -> [element(1, O) || O <- Chunk]
                       end end,
    {C1, Cont1} = ets:select(T5, [{{'$1','_'}, [], ['$_']}], 3),
    P("cont.first", [element(1, O) || O <- C1]),
    P("cont.owner.setopts_private", R(C(fun() -> ets:setopts(T5, {protection, private}) end))),
    P("cont.private.nonowner_resume", R(Eval(fun() -> Keys(ets:select(Cont1)) end))),
    P("cont.owner.setopts_protected",
      R(C(fun() -> ets:setopts(T5, {protection, protected}) end))),
    P("cont.protected.nonowner_resume", R(Eval(fun() -> Keys(ets:select(Cont1)) end))),
    P("cont.protected.nonowner_resume_again", R(Eval(fun() -> Keys(ets:select(Cont1)) end))),
    P("cont.size_after", ets:info(T5, size)),
    Child ! stop,
    ok.

child() ->
    receive
        {eval, From, Fun} ->
            Res = try {ok, Fun()} catch error:badarg -> badarg; C:E -> {C, E} end,
            From ! {eval, Res},
            child();
        stop -> ok
    end.
"#;

/// A denied resume must not consume the continuation: the same `&cont` resumes to
/// the same chunk once the gate reopens.
#[test]
fn a_denied_continuation_resume_stays_resumable_and_matches_the_live_oracle() {
    const NAME: &str = "a_denied_continuation_resume_stays_resumable_and_matches_the_live_oracle";
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            NAME,
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable `erl`/`escript` on $PATH)",
        );
        return;
    };
    let output = oracle::run_escript(
        &escript,
        CONTINUATION_PROGRAM,
        &[],
        "continuation",
        ORACLE_TIMEOUT,
    );
    let lines = parse_oracle(&output.stdout);
    let transcript = format!("{lines:#?}");

    for (tag, expected) in [
        ("cont.first", "[1,2,3]"),
        ("cont.owner.setopts_private", "true"),
        ("cont.private.nonowner_resume", "badarg"),
        ("cont.owner.setopts_protected", "true"),
        ("cont.protected.nonowner_resume", "[4,5,6]"),
        ("cont.protected.nonowner_resume_again", "[4,5,6]"),
        ("cont.size_after", "10"),
    ] {
        assert_eq!(
            line(&lines, tag, &transcript).value,
            expected,
            "`{tag}`: the oracle's continuation observation changed"
        );
    }

    // The crate side: the identical script through the access-enforcing view.
    let owner = OwnerId::new(1);
    let other = OwnerId::new(2);
    let mut atoms = AtomTable::new();
    let mut arena = Heap::new(1024);
    let spec = whole_object_spec(&mut arena, &mut atoms);
    let mut out = rebuild_heap_words(4096);

    let mut opts = TableOptions::new();
    opts.set_access(Access::Protected);
    opts.set_kind(beam_ets::TableKind::OrderedSet);
    let mut table = Table::new(TableId::next(), opts).expect("a fresh table is constructible");
    table.set_owner(Some(owner));
    {
        let mut writer = table.as_requester(Some(owner));
        for key in 1..=10i64 {
            writer
                .insert(object(&mut arena, key, key * 10), &atoms)
                .expect("the owner seeds the table");
        }
    }

    let (first, continuation) = table
        .as_requester(Some(owner))
        .select_limit(&spec, 3, &atoms, &mut out)
        .expect("the owner may chunk a protected table");
    assert_eq!(keys_of(&first), vec![1, 2, 3], "`cont.first` is `[1,2,3]`");
    let continuation = continuation.expect("three of ten objects leave more to read");

    table
        .as_requester(Some(owner))
        .setopts(SetOpt::Access(Access::Private))
        .expect("the owner closes the table");

    // A denial, twice — and the continuation survives both.
    for attempt in 1..=2 {
        let denied = table
            .as_requester(Some(other))
            .select_continue(&continuation, &atoms, &mut out);
        assert!(
            matches!(denied, Err(BeamError::BadArg)),
            "resume #{attempt} of a private table by a non-owner must be badarg, got {denied:?}"
        );
    }

    table
        .as_requester(Some(owner))
        .setopts(SetOpt::Access(Access::Protected))
        .expect("the owner reopens the table");

    // The same continuation, twice, yields the identical chunk.
    for attempt in 1..=2 {
        let (chunk, _next) = table
            .as_requester(Some(other))
            .select_continue(&continuation, &atoms, &mut out)
            .unwrap_or_else(|error| {
                panic!("resume #{attempt} after the gate reopened must succeed, got {error:?}")
            });
        assert_eq!(
            keys_of(&chunk),
            vec![4, 5, 6],
            "resume #{attempt} must yield `[4,5,6]` — a continuation is immutable and reusable"
        );
    }
    assert_eq!(table.size(), 10, "`cont.size_after` is 10");
}

// ---------------------------------------------------------------------------
// The shared oracle program (Tests A and B)
// ---------------------------------------------------------------------------

/// The give-away matrix and the protection-flip/deletion timing, in one program.
///
/// A spawned child performs the recipient-side operations, so a real
/// `{'ETS-TRANSFER', Tid, FromPid, GiftData}` arrives before they run (the child
/// consumes it). Every operation goes through the `badarg`-only wrapper: a
/// `{Class, Reason}` tuple would panic the parse, so a gate answer is always one of
/// `ok`/`badarg`.
const RIGHTS_PROGRAM: &str = r#"#!/usr/bin/env escript
main(_) ->
    P = fun(K, V) -> io:format("~s -> ~w~n", [K, V]) end,
    C = fun(F) -> try {ok, F()} catch error:badarg -> badarg; Cl:R -> {Cl, R} end end,
    R = fun({ok, V}) -> V; (X) -> X end,
    Me = self(),
    Eval = fun(Fun) -> Child = get(child), Child ! {eval, Me, Fun},
                      receive {eval, X} -> X end end,
    Child = spawn(fun() -> child() end),
    put(child, Child),
    lists:foreach(
      fun(Prot) ->
          T = ets:new(gatab, [Prot]),
          ets:insert(T, {1, 10}),
          F = fun(S) -> lists:flatten(io_lib:format("~p.~s", [Prot, S])) end,
          P(F("before.owner_is_self"), ets:info(T, owner) =:= self()),
          P(F("giveaway"), R(C(fun() -> ets:give_away(T, Child, gift_data) end))),
          P(F("after.owner_is_child"), ets:info(T, owner) =:= Child),
          P(F("giver.insert"), R(C(fun() -> ets:insert(T, {2, 20}) end))),
          P(F("giver.lookup"), R(C(fun() -> ets:lookup(T, 1) end))),
          P(F("giver.size"), R(C(fun() -> ets:info(T, size) end))),
          P(F("giver.delete_key"), R(C(fun() -> ets:delete(T, 1) end))),
          P(F("giver.fixtable"), R(C(fun() -> ets:safe_fixtable(T, true) end))),
          P(F("giver.select_replace"),
            R(C(fun() -> ets:select_replace(T, [{{'$1','$2'}, [], [{{'$1',99}}]}]) end))),
          P(F("giver.setopts"), R(C(fun() -> ets:setopts(T, {protection, public}) end))),
          P(F("recipient.insert"), R(Eval(fun() -> ets:insert(T, {3, 30}) end))),
          P(F("recipient.lookup"), R(Eval(fun() -> ets:lookup(T, 1) end))),
          P(F("recipient.owner_is_self"), R(Eval(fun() -> ets:info(T, owner) =:= self() end))),
          P(F("recipient.setopts_ok"),
            R(Eval(fun() -> ets:setopts(T, {protection, protected}) end))),
          P(F("recipient.delete_table"), R(Eval(fun() -> ets:delete(T) end)))
      end, [protected, private, public]),

    %% The protection-flip timing section: each flip applies to the next operation.
    T2 = ets:new(flictab, [protected]),
    ets:insert(T2, {1, 10}),
    P("flip.protected.nonowner_lookup", R(Eval(fun() -> ets:lookup(T2, 1) end))),
    P("flip.owner.setopts_private", R(C(fun() -> ets:setopts(T2, {protection, private}) end))),
    P("flip.private.nonowner_lookup", R(Eval(fun() -> ets:lookup(T2, 1) end))),
    P("flip.private.nonowner_info_size", R(Eval(fun() -> ets:info(T2, size) end))),
    P("flip.private.nonowner_setopts",
      R(Eval(fun() -> ets:setopts(T2, {protection, private}) end))),
    P("flip.owner.setopts_public", R(C(fun() -> ets:setopts(T2, {protection, public}) end))),
    P("flip.public.nonowner_insert", R(Eval(fun() -> ets:insert(T2, {2, 20}) end))),
    P("flip.public.nonowner_delete_table", R(Eval(fun() -> ets:delete(T2) end))),
    P("flip.public.table_gone", case ets:info(T2) of undefined -> undefined; _ -> alive end),

    %% The whole-table-delete section: DB_WRITE, so public yes / protected no.
    T3 = ets:new(delprot, [protected]),
    P("del.protected.nonowner", R(Eval(fun() -> ets:delete(T3) end))),
    P("del.protected.table_alive", R(C(fun() -> ets:info(T3, size) end))),
    T4 = ets:new(delpub, [public]),
    P("del.public.nonowner", R(Eval(fun() -> ets:delete(T4) end))),
    P("del.public.table_gone", case ets:info(T4) of undefined -> undefined; _ -> alive end),
    Child ! stop,
    ok.

child() ->
    receive
        {'ETS-TRANSFER', _Tid, _From, _Data} -> child();
        {eval, From, Fun} ->
            Res = try {ok, Fun()} catch error:badarg -> badarg; C:E -> {C, E} end,
            From ! {eval, Res},
            child();
        stop -> ok
    end.
"#;
