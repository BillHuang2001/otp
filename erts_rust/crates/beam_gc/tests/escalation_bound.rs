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

//! **The minor↔major escalation bound** — what [`AnyCollector::collect`] compares the mature
//! space's remaining room against.
//!
//! The C decides minor-vs-major on the promotion the minor would move, not on the young heap's
//! allocation history: `minor_collection` promotes only when `mature_size <= OLD_HEND(p) -
//! OLD_HTOP(p)` (`erts/emulator/beam/erl_gc.c:1486-1488`), and when that fails `garbage_collect`
//! runs a major with `F_NEED_FULLSWEEP` (`:1573-1576`, `:839-849`). The young heap's *usage*
//! (`HEAP_TOP(p) - HEAP_START(p)`, `:696`) is a fine-grained bound on that promotion only
//! because the C's young heap is a single block whose `HEAP_SIZE` is re-derived from the live set
//! at every collection — and because the C's `mature_size` is computed against the **active**
//! block (`mature = p->abandoned_heap ? p->abandoned_heap : p->heap`,
//! `mature_size = high_water - mature`, `:1403-1427`), never against the sum of every block a
//! delayed collection left behind (that sum is `ygen_usage`, used only for *sizing*, `:696`,
//! `:1952`, `:2061-2098`).
//!
//! This crate's nursery can legitimately carry **many** spaces (a no-relocation
//! [`grow_nursery`](AnyCollector::grow_nursery) retires the space it replaces), and a host may
//! bump-allocate into it without ever rewinding — a registered BIF's result copy-out does exactly
//! that — so the chain-summed fill ([`HeapSpace::used_words`](beam_gc::HeapSpace::used_words) over
//! every space) can exceed the mature room while the **live** young set fits in it with room to
//! spare. Keying the escalation on that fill made every collection escalate to a fullsweep
//! permanently, because a fullsweep's own sizing is a function of the live set — no fullsweep can
//! ever buy room for a dead figure.
//!
//! The contract pinned here:
//!
//! * **(a) a dead-inclusive fill does not force a fullsweep** — a nursery whose chain-summed fill
//!   is several times the mature space's remaining room, holding a tiny live set, keeps selecting
//!   [`CollectionKind::Minor`], round after round, with the minor counter advancing and the major
//!   counter untouched by anything but the `fullsweep_after` cadence;
//! * **(b) genuine room scarcity still escalates** — a nursery whose **live** young set does not
//!   fit the mature space selects [`CollectionKind::Major`] (the fix is a bounding change, never
//!   "never escalate");
//! * **(c) the kind-pinned minor refuses before copying** — in that same shape
//!   [`collect_kind`](AnyCollector::collect_kind)`(CollectionKind::Minor)` reports the typed heap
//!   exhaustion and leaves both spaces and every counter untouched (`collect_kind` never
//!   escalates).
//!
//! Deterministic counts only: no timing is measured or asserted.

use beam_common::BeamError;
use beam_gc::prelude::{AnyCollector, CollectionKind, GcConfig, HeapObservable, HeapPolicy};
use beam_term::prelude::{make_small, Eterm};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The words a `{a, 2a}` tuple occupies: the frozen layout is `arity + 1` (header plus elements).
const TUPLE_WORDS: usize = 3;

/// The mature capacity the dead-fill rounds are **constructed** with. A round keeps exactly one
/// live 3-word tuple, and `ROUNDS` of them cannot exhaust this room; what the rounds actually run
/// against is read back from the collector (a collection re-sizes the generation from the live
/// set, so the seed is not the steady-state capacity — the assertions below never assume it is).
const MATURE_WORDS: usize = 610;

/// The mature capacity the room-scarcity control starts from: far below the live set built below.
const TINY_MATURE_WORDS: usize = 8;

/// Rounds of the dead-fill shape. Long enough that a fill-keyed predicate (which escalates on the
/// first round and stays escalated) is unmistakable in the counters.
const ROUNDS: usize = 40;

/// How many times the mature space's remaining room a round's dead fill aims for. The fill only
/// has to *exceed* the room — that is the assertion — but several times the room is what makes the
/// subject unmistakable.
const FILL_MULTIPLE: usize = 2;

/// The hard bound on one round's dead fill, in tuples: 220 tuples = 660 words, comfortably more
/// than the largest fill the shape's mature room can hold here, and a fixed amount of work. It is
/// the **deterministic stopper**: the fill must stop on a tuple count, never on "the growth seam
/// refused", because a nursery that has not collected has no capability to bound itself — the
/// growth seam no longer refuses at a chain cap, so an unbounded loop would walk the growth table
/// to the allocator's limit.
const MAX_DEAD_TUPLES: usize = 220;

/// Builds a 3-word `{a, 2a}` tuple in `c`'s nursery, growing the nursery when it is full.
fn tuple(c: &mut AnyCollector, a: i64) -> Eterm {
    if c.young().available_words() < TUPLE_WORDS {
        // The no-relocation growth seam the host uses: it retires the current space (so the
        // chain's *fill* keeps growing while nothing is collected) and never moves a term.
        c.grow_nursery(TUPLE_WORDS)
            .expect("the nursery chain has room for one more tuple");
    }
    c.young_mut()
        .heap_mut()
        .tuple(&[make_small(a), make_small(2 * a)])
        .expect("a 3-word nursery tuple")
}

/// The words handed out across **every** nursery space — the chain-summed fill
/// ([`HeapObservable::heap_used_words`]), which counts the spaces a growth retired as well as the
/// current one. This is the dead-inclusive figure the escalation test must **ignore**: only the
/// live set the minor would promote is a room requirement, and the fill is an allocation history.
fn young_fill(c: &AnyCollector) -> usize {
    c.heap_used_words()
}
/// Fills the nursery with **dead** tuples (never rooted) until the chain-summed fill reaches
/// `target_words`, returning the number of words it allocated — the **dead-inclusive occupancy**
/// the escalation test must ignore.
///
/// Three deterministic stoppers, all of them checked before a tuple is allocated:
///
/// * `target_words` — the shape the round wants (a fill several times the mature room);
/// * [`MAX_DEAD_TUPLES`] — the hard bound, so the round is a fixed amount of work;
/// * an **unrequestable** growth (the growth table's ceiling, or the `max_heap_words` budget) also
///   ends the fill: the seam reports the typed exhaustion instead of installing a space, and a
///   collection here would destroy the very fill the round is about.
fn fill_with_dead_tuples(c: &mut AnyCollector, target_words: usize) -> usize {
    let start = young_fill(c);
    for a in 0..MAX_DEAD_TUPLES as i64 {
        if young_fill(c) >= target_words {
            break;
        }
        if c.grow_nursery(TUPLE_WORDS).is_err() {
            break;
        }
        let _ = tuple(c, a);
    }
    young_fill(c) - start
}

/// The mature space's `(used, capacity)` words.
fn mature_words(c: &AnyCollector) -> (usize, usize) {
    match c {
        AnyCollector::Copying(x) => (x.mature().used_words(), x.mature().capacity_words()),
        AnyCollector::Immix(x) => (x.mature().used_words(), x.mature().capacity_words()),
    }
}

/// A copying collector under `policy` sized so a round's live set is a handful of words.
fn collector(policy: HeapPolicy) -> AnyCollector {
    AnyCollector::from_config(&GcConfig::copying(policy)).expect("collector")
}

// ---------------------------------------------------------------------------
// (a) A dead-inclusive fill does not force a fullsweep
// ---------------------------------------------------------------------------

/// **The escalation test reads the live promotion, not the nursery's fill.**
///
/// Each round fills the nursery with dead tuples (retiring spaces through the no-relocation
/// growth seam, exactly as a host's BIF copy-out does) and keeps one small live root. Every round
/// then collects and the collection must be a **minor**: the chain-summed fill is several times
/// the mature space's remaining room, while the live set the minor promotes is a single tuple.
///
/// With the fill in the predicate the first round would escalate to a fullsweep and every later
/// round would too (a fullsweep sizes the mature space from the live set, so no fullsweep ever
/// buys room for a dead figure) — `minor_gcs` would stay `0` for the whole run.
#[test]
fn a_dead_inclusive_fill_keeps_selecting_minor() {
    let policy = HeapPolicy {
        initial_mature_words: MATURE_WORDS,
        ..HeapPolicy::default()
    };
    let mut c = collector(policy);

    let mut minors = 0usize;
    let mut escalations = 0usize;
    for round in 0..ROUNDS {
        let (mature_used, mature_capacity) = mature_words(&c);
        let available = mature_capacity - mature_used;
        let dead = fill_with_dead_tuples(&mut c, FILL_MULTIPLE * available);
        let fill = young_fill(&c);
        let mut roots = [tuple(&mut c, 1_000 + round as i64)];
        assert!(
            fill > available,
            "round {round}: the shape must carry a fill ({fill} words) larger than the mature \
             space's remaining room ({available} words), or it does not exercise the bound"
        );
        assert!(
            dead > TUPLE_WORDS,
            "round {round}: the fill comes from retreating spaces, not one tuple: {dead} words"
        );
        let collection = c.collect(&mut roots).expect("the collection runs");
        match collection.kind {
            CollectionKind::Minor => minors += 1,
            CollectionKind::Major => escalations += 1,
        }
        assert_eq!(
            collection.kind,
            CollectionKind::Minor,
            "round {round}: the dead fill ({fill} words) is not a live set, so the collection must \
             be a minor — it promoted {} words into {available} words of room",
            collection.stats.words_copied
        );
        assert!(
            collection.stats.words_copied <= available,
            "round {round}: the promotion the minor moved ({} words) must fit the room \
             the predicate measured ({available} words)",
            collection.stats.words_copied
        );
    }
    assert_eq!(
        escalations, 0,
        "no round may escalate: the mature space has room for every promotion, and the \
         fullsweep cadence (fullsweep_after = 65535) never fires in {ROUNDS} rounds"
    );
    assert_eq!(minors, ROUNDS, "every round is a minor");
    assert_eq!(
        c.stats().minor_collections,
        ROUNDS as u64,
        "the garbage_collection item's counter moves with the minors"
    );
    assert_eq!(
        c.stats().major_collections,
        0,
        "no fullsweep ran — the fill is not a live set"
    );
    assert_eq!(
        c.minor_gcs(),
        ROUNDS as u32,
        "the minors-since-major counter advanced"
    );
}

// ---------------------------------------------------------------------------
// (b) + (c) Genuine room scarcity still escalates (and the pinned minor refuses)
// ---------------------------------------------------------------------------

/// Builds a live set of `LIVE_TUPLES` tuples in the nursery and returns the root vector.
fn live_set(c: &mut AnyCollector) -> Vec<Eterm> {
    (0..20).map(|a| tuple(c, a)).collect()
}

/// **A live young set larger than the mature room escalates to a fullsweep.**
///
/// The control for the test above: the nursery holds only ~60 live words (no dead fill at all),
/// but the mature space has 8 words, so the promotion genuinely cannot fit and `collect` must run
/// a major. This is what keeps the fix a *bounding* change rather than "never escalate".
///
/// The **room** half is isolated by the pinned-minor control below: it asks for exactly the minor
/// and must see the typed refusal, which only the room test can produce.
#[test]
fn a_live_promotion_that_cannot_fit_still_escalates() {
    let policy = HeapPolicy {
        initial_mature_words: TINY_MATURE_WORDS,
        fullsweep_after: 65_535,
        ..HeapPolicy::default()
    };
    let mut c = collector(policy);
    let mut roots = live_set(&mut c);
    let live = c.young().used_words();
    assert!(
        live > TINY_MATURE_WORDS,
        "the shape needs a live set ({live} words) larger than the mature room \
         ({TINY_MATURE_WORDS} words)"
    );
    let collection = c.collect(&mut roots).expect("the fullsweep runs");
    assert_eq!(
        collection.kind,
        CollectionKind::Major,
        "a live promotion that cannot fit the mature space must escalate"
    );
    assert_eq!(c.stats().major_collections, 1);
    assert_eq!(c.stats().minor_collections, 0);
}

/// **The kind-pinned minor refuses with the typed heap exhaustion before a single word is
/// copied.**
///
/// Same shape as the escalation control, collected through
/// [`collect_kind`](AnyCollector::collect_kind)`: the pin is never escalated, so the refusal is
/// the caller's to see — and it must leave both spaces and every counter exactly as it found them
/// (a half-forwarded heap, or a silently-run fullsweep, would both be worse than the error).
#[test]
fn a_pinned_minor_refuses_before_copying_when_the_promotion_cannot_fit() {
    let policy = HeapPolicy {
        initial_mature_words: TINY_MATURE_WORDS,
        fullsweep_after: 65_535,
        ..HeapPolicy::default()
    };
    let mut c = collector(policy);
    let mut roots = live_set(&mut c);
    let young_before = (c.young().used_words(), c.young().capacity_words());
    let mature_before = mature_words(&c);
    let stats_before = c.stats();
    let err = c
        .collect_kind(CollectionKind::Minor, &mut roots)
        .expect_err("the promotion cannot fit the mature space");
    assert!(
        matches!(err, BeamError::Alloc(_)),
        "the refusal must be the typed heap exhaustion, got {err:?}"
    );
    assert_eq!(
        (c.young().used_words(), c.young().capacity_words()),
        young_before,
        "the nursery is untouched: nothing was evacuated"
    );
    assert_eq!(
        mature_words(&c),
        mature_before,
        "the mature space is untouched"
    );
    assert_eq!(
        (c.stats().minor_collections, c.stats().major_collections),
        (
            stats_before.minor_collections,
            stats_before.major_collections
        ),
        "no counter advanced — no collection ran"
    );
    assert_eq!(
        c.minor_gcs(),
        0,
        "a refused minor is not a minor: the garbage_collection counter stays put"
    );
}
