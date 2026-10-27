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

//! **The explicit capacity-give-back seam** — `AnyCollector::trim_retired` and the two nursery
//! trim methods behind it (`Nursery::trim_after_minor` / `Nursery::trim_after_fullsweep`,
//! `src/nursery.rs`, driven from `src/trim.rs`).
//!
//! A grown nursery is a **chain** of spaces: `grow_nursery` retires the space it replaces (live
//! words stay at their address, nothing moves) and nothing frees a retired space except a
//! collection. A collection does collapse the chain, but it sizes the surviving space through the
//! policy's **keep band** ([`HeapPolicy::young_after_collection`] /
//! [`HeapPolicy::young_after_fullsweep`]), which deliberately retains a capacity inside
//! `[target, max(MINOR_SHRINK_FLOOR_WORDS, YOUNG_SHRINK_FACTOR x target)]`. `trim_retired` is the
//! explicit request to give that capacity back: it collects (which is what makes the retired
//! spaces' words provably dead) and then installs the live set's target **unconditionally**.
//!
//! # The contract (asserted below)
//!
//! 1. **One trim collapses a grown chain and pays the debt.** After N growth events that really
//!    retire spaces (`young_space_count() > 1`, `collection_debt() == true`), a single
//!    `trim_retired` leaves `young_space_count() == 1` and `collection_debt() == false`.
//! 2. **The installed capacity is the live set's target, computed from the round's own copy
//!    volume** — [`HeapPolicy::young_minor_target_words`] of `Collection::stats::words_copied`
//!    after a minor, [`HeapPolicy::young_target_words`] of it after a fullsweep. Not a banded
//!    third value.
//! 3. **Capacity the keep band would have KEPT is handed back.** A nursery whose capacity sits
//!    inside the band around a small live set's target survives a collection unchanged
//!    (`young_after_collection(capacity, live) == capacity`); `trim_retired` on the same live set
//!    installs the target, which is strictly smaller.
//! 4. **A trimmed chain is still a working collector.** Rooted terms survive the trim intact
//!    (structurally equal to a freshly built witness, same graph size), and a following
//!    `collect_minor` over the same fresh graph copies exactly what an **untrimmed twin**
//!    collector copies — same `words_copied`, same `objects_copied`.
//! 5. **Both mature strategies.** Every case runs through `AnyCollector::from_config` with
//!    `GcConfig::copying()` and with `GcConfig::immix()`: the nursery chain is shared, so the
//!    give-back is strategy-independent.
//!
//! Every corpus is deterministic (an allocation sequence, never a clock): no case asserts or
//! measures timing. The root-set contract is the caller's claim — a complete root slice at a
//! safepoint — exactly as for `collect`; the cases below pass the live set they built.
use beam_gc::prelude::{
    AnyCollector, Collection, CollectionKind, GcConfig, HeapObservable, HeapPolicy, HeapSpace,
    MatureKind, MINOR_SHRINK_FLOOR_WORDS, YOUNG_SHRINK_FACTOR,
};
use beam_term::compare::eq as term_eq;
use beam_term::prelude::{make_small, size_object, AtomTable, Eterm};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Both mature strategies: the nursery chain (and therefore the give-back) is shared by
/// [`AnyCollector::Copying`] and [`AnyCollector::Immix`].
const KINDS: [MatureKind; 2] = [MatureKind::Copying, MatureKind::Immix];

/// The word count of the establishing live term: one tuple of arity 39 (header + 39 elements).
const LIVE_WORDS: usize = 40;

/// The growth request every growth event below asks for, in words. Small enough that one fill of
/// the current space outgrows it (so the event really retires a space) and small enough to be
/// granted by the growth table from the crate's default seed.
const GROW_REQUEST: usize = 64;

/// A collector for `policy` with `kind` **pinned** through the public configuration seam, so a
/// case is independent of the automatic mature-generation rule.
fn any_for(kind: MatureKind, policy: HeapPolicy) -> AnyCollector {
    let config = match kind {
        MatureKind::Copying => GcConfig::copying(policy),
        MatureKind::Immix => GcConfig::immix(policy),
    };
    AnyCollector::from_config(&config).expect("collector")
}

/// Writes 3-word `{1,2}` tuples into `space` until it has **fewer than** `keep_free` words
/// available — "the mutator filled the current space", the state a growth event answers.
///
/// Returns the number of tuples written. They are unrooted garbage: exactly what a collection
/// reclaims and a trim must not keep the capacity for.
fn fill_to(space: &mut HeapSpace, keep_free: usize) -> usize {
    let filler = [make_small(1), make_small(2)];
    let mut tuples = 0;
    while space.available_words() >= keep_free {
        if space.heap_mut().tuple(&filler).is_err() {
            break;
        }
        tuples += 1;
    }
    tuples
}

/// Drives `events` growth events with **no collection in between**, asserting that each one really
/// **retires** a space (the chain grows by one every time).
fn drive_growth(c: &mut AnyCollector, hint: usize, events: usize) {
    for event in 0..events {
        let before = c.young_space_count();
        let filled = fill_to(c.young_mut(), hint);
        assert!(
            filled > 0,
            "event {event}: the current space must hold room to fill"
        );
        assert!(
            c.young().available_words() < hint,
            "event {event}: the request must outgrow the current space, or the seam is a no-op"
        );
        c.grow_nursery(hint)
            .expect("the growth seam grants a small request");
        assert_eq!(
            c.young_space_count(),
            before + 1,
            "event {event}: a granted growth retires the space it replaces"
        );
    }
}

/// Builds a **live term of exactly `live_words` words** in the nursery — one tuple of arity
/// `live_words - 1` — and returns it as a one-element root set.
fn build_live(c: &mut AnyCollector, live_words: usize) -> Vec<Eterm> {
    let elems = vec![make_small(7); live_words - 1];
    let term = c
        .young_mut()
        .heap_mut()
        .tuple(&elems)
        .expect("the live term fits the nursery");
    vec![term]
}

/// Builds a `{a, 2a}` tuple (3 words) in `c`'s nursery.
fn build_pair(c: &mut AnyCollector, a: i64) -> Eterm {
    c.young_mut()
        .heap_mut()
        .tuple(&[make_small(a), make_small(2 * a)])
        .expect("a 3-word nursery tuple")
}

/// Whether `term` is structurally identical to a freshly built `{a, 2a}` tuple.
///
/// The witness is written into a fresh [`beam_term::heap::Heap`] (kept alive for the comparison)
/// and a fresh [`AtomTable`] resolves atoms, so the check reads no raw heap word and re-derives no
/// term shape — the pattern `tests/generational_cadence.rs` established.
fn is_pair(term: Eterm, a: i64) -> bool {
    let atoms = AtomTable::new();
    let mut heap = beam_term::heap::Heap::new(64);
    let expected = heap
        .tuple(&[make_small(a), make_small(2 * a)])
        .expect("the witness tuple");
    term_eq(term, expected, &atoms)
}

/// The live set's target for the collection that ran: the minor rule's figure after a minor, the
/// whole-heap figure after a fullsweep. This is exactly what `trim_retired` installs.
fn live_target_for(policy: &HeapPolicy, collection: &Collection) -> usize {
    match collection.kind {
        CollectionKind::Minor => policy.young_minor_target_words(collection.stats.words_copied),
        CollectionKind::Major => policy.young_target_words(collection.stats.words_copied),
    }
}

/// A policy whose mature generation has plenty of room for the minors these cases run, so the
/// kind is decided by the cadence (a minor) and never by an escalation the case did not intend.
fn roomy_policy(initial_young_words: usize) -> HeapPolicy {
    HeapPolicy {
        initial_young_words,
        initial_mature_words: 32_768,
        ..HeapPolicy::default()
    }
}

// ---------------------------------------------------------------------------
// 1. one trim collapses a grown chain and pays the debt
// ---------------------------------------------------------------------------

/// N growth events that really retire spaces, then **one** `trim_retired`: the chain is one space,
/// the debt is cleared, and the surviving capacity is the live set's target rather than the chain
/// sum the growth seam left behind.
#[test]
fn one_trim_collapses_a_grown_chain_onto_the_live_sets_target() {
    for kind in KINDS {
        let policy = roomy_policy(233);
        let mut c = any_for(kind, policy.clone());
        let mut roots = build_live(&mut c, LIVE_WORDS);
        drive_growth(&mut c, GROW_REQUEST, 6);
        assert!(
            c.young_space_count() > 1,
            "{kind:?}: six growth events leave a chain, got {} space(s)",
            c.young_space_count()
        );
        assert!(
            c.collection_debt(),
            "{kind:?}: a retired space is exactly the collection debt"
        );
        let grown = c.heap_size_words();
        assert!(
            grown > policy.young_minor_target_words(LIVE_WORDS),
            "{kind:?}: the chain sum ({grown} words) is far above the live set's target"
        );

        let collection = c
            .trim_retired(&mut roots)
            .expect("the trim's collection and its install");
        assert_eq!(
            collection.kind,
            CollectionKind::Minor,
            "{kind:?}: the cadence is not due and the mature space has room, so a minor runs"
        );
        assert_eq!(
            collection.stats.words_copied, LIVE_WORDS,
            "{kind:?}: the round evacuated the one live term and reclaimed the rest"
        );
        assert_eq!(
            c.young_space_count(),
            1,
            "{kind:?}: the trim leaves exactly one nursery space"
        );
        assert!(
            !c.collection_debt(),
            "{kind:?}: nothing is retained after a trim"
        );
        assert_eq!(
            c.heap_size_words(),
            live_target_for(&policy, &collection),
            "{kind:?}: the installed capacity is the live set's target for the kind that ran"
        );
        assert_eq!(
            c.heap_size_words(),
            policy.young_minor_target_words(LIVE_WORDS),
            "{kind:?}: a minor trims onto the young live set's target"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. capacity the keep band would keep is handed back
// ---------------------------------------------------------------------------

/// A nursery **inside its keep band** survives a collection unchanged — that band is the policy's
/// default and stays untouched — and `trim_retired` is the explicit way to give that capacity back.
#[test]
fn a_capacity_the_keep_band_keeps_is_handed_back_by_the_trim() {
    // The live set's minor target, and the band the collection's own rule keeps around it.
    let probe = HeapPolicy::default();
    let target = probe.young_minor_target_words(LIVE_WORDS);
    let band_upper = MINOR_SHRINK_FLOOR_WORDS.max(YOUNG_SHRINK_FACTOR * target);
    // A capacity inside the band that is also a curated growth-table entry, so the chain can
    // really be built at it rather than at a size no space would ever be installed at.
    let kept_capacity = probe.next_words(target, 0).max(2_586);
    assert!(
        kept_capacity >= target && kept_capacity <= band_upper,
        "the case needs a capacity inside the keep band [{target}, {band_upper}], got \
         {kept_capacity}"
    );
    assert_eq!(
        probe.young_after_collection(kept_capacity, LIVE_WORDS),
        kept_capacity,
        "the keep band retains {kept_capacity} words for a {LIVE_WORDS}-word live set"
    );
    // The fullsweep's band is narrower about a large nursery (it measures the whole live set
    // exactly), so this capacity is the *minor* band's to keep — which is the collection a
    // nursery in this state actually runs. The trim's figure is the minor target either way.
    assert!(
        probe.young_after_fullsweep(kept_capacity, LIVE_WORDS) < kept_capacity,
        "a fullsweep would already give this capacity back; the minor band is what retains it"
    );

    for kind in KINDS {
        let policy = roomy_policy(kept_capacity);
        let mut c = any_for(kind, policy.clone());
        // A first collection on the empty nursery: the band keeps the seeded capacity, so the
        // pre-trim state below is the *rule's* answer and not merely the constructor's seed.
        c.collect_minor(&mut [])
            .expect("the settling minor on an empty nursery");
        assert_eq!(
            c.heap_size_words(),
            kept_capacity,
            "{kind:?}: the keep band retains the seeded capacity"
        );
        let mut roots = build_live(&mut c, LIVE_WORDS);
        assert_eq!(
            c.heap_size_words(),
            policy.young_after_collection(kept_capacity, LIVE_WORDS),
            "{kind:?}: a collection on this live set would keep the capacity as it stands"
        );

        let collection = c
            .trim_retired(&mut roots)
            .expect("the trim's collection and its install");
        assert_eq!(
            collection.kind,
            CollectionKind::Minor,
            "{kind:?}: a minor ran, so the young live set is the figure"
        );
        assert_eq!(
            collection.stats.words_copied, LIVE_WORDS,
            "{kind:?}: the live set is the establishing tuple"
        );
        assert_eq!(
            c.heap_size_words(),
            target,
            "{kind:?}: the trim installs the target the band would have kept the capacity above"
        );
        assert!(
            c.heap_size_words() < kept_capacity,
            "{kind:?}: the words the band retained are given back"
        );
        assert_eq!(c.young_space_count(), 1, "{kind:?}");
        assert!(!c.collection_debt(), "{kind:?}");
    }
}

// ---------------------------------------------------------------------------
// 3. a trimmed chain still collects the same graph as an untrimmed twin
// ---------------------------------------------------------------------------

/// Rooted terms survive the trim intact, and a following collection over the same fresh graph
/// copies exactly what an **untrimmed twin** collector copies.
#[test]
fn a_trimmed_nursery_keeps_its_terms_and_collects_like_an_untrimmed_twin() {
    for kind in KINDS {
        let policy = roomy_policy(610);
        let mut trimmed = any_for(kind, policy.clone());
        let mut twin = any_for(kind, policy.clone());

        // The same establishment in both: a `{11,22}` pair and a `LIVE_WORDS`-word tuple, settled
        // into the mature generation by one minor.
        let pair_t = build_pair(&mut trimmed, 11);
        let tuple_t = build_live(&mut trimmed, LIVE_WORDS);
        let mut roots_t = vec![pair_t, tuple_t[0]];
        trimmed
            .collect_minor(&mut roots_t)
            .expect("the establishing minor (trimmed)");
        let pair_w = build_pair(&mut twin, 11);
        let tuple_w = build_live(&mut twin, LIVE_WORDS);
        let mut roots_w = vec![pair_w, tuple_w[0]];
        twin.collect_minor(&mut roots_w)
            .expect("the establishing minor (twin)");

        // The same growth history: identical fill/grow sequences, so identical chains.
        drive_growth(&mut trimmed, GROW_REQUEST, 4);
        drive_growth(&mut twin, GROW_REQUEST, 4);
        assert_eq!(
            trimmed.heap_size_words(),
            twin.heap_size_words(),
            "{kind:?}: the two collectors walked the same growth sequence"
        );
        assert!(trimmed.collection_debt(), "{kind:?}");

        // The give-back, on one of them only.
        trimmed
            .trim_retired(&mut roots_t)
            .expect("the trim's collection and its install");
        assert_eq!(trimmed.young_space_count(), 1, "{kind:?}");
        assert!(!trimmed.collection_debt(), "{kind:?}");
        assert!(twin.collection_debt(), "{kind:?}: the twin keeps its chain");

        // The rooted terms are intact after the trim: same structure, same graph size.
        assert!(
            is_pair(roots_t[0], 11),
            "{kind:?}: the rooted pair survived the trim structurally intact"
        );
        assert_eq!(
            size_object(roots_t[1]),
            LIVE_WORDS,
            "{kind:?}: the rooted {LIVE_WORDS}-word tuple survived whole"
        );
        assert!(
            is_pair(roots_w[0], 11),
            "{kind:?}: the twin's pair is the same term"
        );

        // A following collection over the same fresh graph: identical counts.
        let mut fresh_t = build_live(&mut trimmed, LIVE_WORDS);
        let mut fresh_w = build_live(&mut twin, LIVE_WORDS);
        let stats_t = trimmed
            .collect_minor(&mut fresh_t)
            .expect("the minor on the trimmed chain");
        let stats_w = twin
            .collect_minor(&mut fresh_w)
            .expect("the minor on the untrimmed chain");
        assert_eq!(
            stats_t.words_copied, stats_w.words_copied,
            "{kind:?}: a trimmed chain copies exactly what an untrimmed one copies"
        );
        assert_eq!(
            stats_t.objects_copied, stats_w.objects_copied,
            "{kind:?}: and the same number of objects"
        );
        assert_eq!(
            stats_t.words_copied, LIVE_WORDS,
            "{kind:?}: the fresh graph is the only live young set"
        );
        assert_eq!(stats_t.objects_copied, 1, "{kind:?}: one tuple, one object");
    }
}

// ---------------------------------------------------------------------------
// 4. a second trim is bounded by the policy floor, never a ratchet
// ---------------------------------------------------------------------------

/// Trimming again with **nothing** in the nursery installs the empty live set's target — the
/// policy's floor — so the seam converges instead of ratcheting, and the floor is a real lower
/// bound on what it installs.
#[test]
fn a_second_trim_with_nothing_young_installs_the_policy_floor() {
    for kind in KINDS {
        let policy = roomy_policy(2_586);
        let mut c = any_for(kind, policy.clone());
        let mut roots = build_live(&mut c, LIVE_WORDS);
        drive_growth(&mut c, GROW_REQUEST, 3);
        let first = c
            .trim_retired(&mut roots)
            .expect("the first trim's collection and install");
        assert_eq!(
            c.heap_size_words(),
            live_target_for(&policy, &first),
            "{kind:?}: the first trim installs the live set's target"
        );
        assert!(
            c.heap_size_words() > policy.nursery_floor_words(),
            "{kind:?}: the live set's target sits above the floor"
        );

        // Nothing young is reachable any more: the first trim's collection moved the live term
        // into the mature generation, so the nursery's live set is empty.
        let second = c
            .trim_retired(&mut roots)
            .expect("the second trim's collection and install");
        assert_eq!(second.kind, CollectionKind::Minor, "{kind:?}");
        assert_eq!(
            second.stats.words_copied, 0,
            "{kind:?}: the live term is in the mature generation, not the nursery"
        );
        assert_eq!(
            c.heap_size_words(),
            policy.young_minor_target_words(0),
            "{kind:?}: an empty young live set installs the empty-target capacity"
        );
        assert_eq!(
            policy.young_minor_target_words(0),
            policy.nursery_floor_words(),
            "{kind:?}: which is the policy's nursery floor"
        );
        assert_eq!(c.young_space_count(), 1, "{kind:?}");
        assert!(!c.collection_debt(), "{kind:?}");
    }
}
