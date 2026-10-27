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
//! **The growth-retirement request is carried into the collection that follows it** — the
//! `need` term of the legacy C (`erts_garbage_collect(p, need, objv, nobj)`, `erl_gc.c:1490-1555`
//! / `:2061-2098`) as `Nursery::pending_growth_need` (`src/nursery.rs`) hands it to
//! `HeapPolicy::young_after_collection_with_need` / `young_after_fullsweep_with_need`.
//!
//! The growth seam cannot collect: a BIF result copy-out holds no published root set, so the
//! machine asks [`AnyCollector::grow_nursery`] for room and the seam **retires** the space it
//! replaces. If the next collection then sizes the nursery from the live set **alone**, it
//! installs a capacity far below the request, the following copy-out grows again, and the process
//! pays one collection plus one space allocate/free per copy-out — the grow -> collect -> shrink ->
//! grow cycle. The request is therefore recorded by the growth and spent by the collection.
//!
//! # The contract (asserted below)
//!
//! 1. **The request is carried.** A collection that follows a growth which really retired a space
//!    installs exactly `HeapPolicy::young_after_collection_with_need(chain_capacity, live, W)`,
//!    with `live` the round's own copy volume — so the surviving space covers the request.
//! 2. **It is one-shot.** A second collection with no intervening growth installs exactly the
//!    plain rule's answer `young_after_collection(capacity, live)`: the request was spent, the
//!    capacity does not ratchet, and it is never a function of the call count.
//! 3. **The cycle is broken.** A copy-out loop (`grow` when the lump does not fit, abandon the
//!    lump, then a door admission on `collection_debt() || should_collect`) reaches a settled
//!    capacity that covers the request once: the number of growth events is **O(1) in the round
//!    count**, the debt-driven admissions stop after the settling one, and the round count per
//!    collection follows the settled capacity (`collections <= rounds / (capacity / W) + k`).
//! 4. **The give-back seam still gives back.** After a grown chain, one
//!    [`AnyCollector::trim_retired`] installs the **plain** live-set target — the pending request
//!    is discarded, not banked.
//!
//! Both mature strategies run every case ([`GcConfig::copying`] / [`GcConfig::immix`]), because
//! the nursery chain and its collapse are shared. Nothing here measures time: every case is a
//! deterministic allocation sequence plus the policy's own figures.
use beam_gc::prelude::{
    AnyCollector, CollectionKind, GcConfig, HeapObservable, HeapPolicy, HeapSpace, MatureKind,
};
use beam_term::prelude::{make_small, Eterm};
// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------
/// Both mature strategies: the nursery chain — and therefore the carried request — is shared by
/// [`AnyCollector::Copying`] and [`AnyCollector::Immix`].
const KINDS: [MatureKind; 2] = [MatureKind::Copying, MatureKind::Immix];
/// The establishing live set: one tuple of arity 39, i.e. 40 words.
const LIVE_WORDS: usize = 40;
/// The room a copy-out asks the seam for. Larger than the nursery seed, so the first request
/// really outgrows the current space and retires it.
const REQUEST: usize = 2_500;
/// A collector for `policy` with `kind` **pinned** through the public configuration seam, so a
/// case is independent of the automatic mature-generation rule.
fn any_for(kind: MatureKind, policy: HeapPolicy) -> AnyCollector {
    let config = match kind {
        MatureKind::Copying => GcConfig::copying(policy),
        MatureKind::Immix => GcConfig::immix(policy),
    };
    AnyCollector::from_config(&config).expect("collector")
}
/// A policy whose mature generation has room for the minors these cases run (so the kind is
/// decided by the cadence, never by an escalation the case did not intend).
fn roomy_policy(initial_young_words: usize) -> HeapPolicy {
    HeapPolicy {
        initial_young_words,
        initial_mature_words: 32_768,
        ..HeapPolicy::default()
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
/// Writes 3-word `{1,2}` tuples into `space` until it has **fewer than** `keep_free` words
/// available — "the mutator filled the current space", the state a growth event answers.
fn fill_to(space: &mut HeapSpace, keep_free: usize) {
    let filler = [make_small(1), make_small(2)];
    while space.available_words() >= keep_free {
        if space.heap_mut().tuple(&filler).is_err() {
            break;
        }
    }
}
/// Writes one `words`-word tuple (header + `words - 1` elements) into the nursery and abandons
/// it — the BIF result a copy-out moves onto the process heap and the caller then drops.
fn write_lump(c: &mut AnyCollector, words: usize) {
    let elems = vec![make_small(3); words - 1];
    c.young_mut()
        .heap_mut()
        .tuple(&elems)
        .expect("the lump fits the room the growth installed");
}
/// The plain live-set target for the collection that ran (`need == 0`): what
/// [`AnyCollector::trim_retired`] installs.
fn plain_target(policy: &HeapPolicy, kind: CollectionKind, live_words: usize) -> usize {
    match kind {
        CollectionKind::Minor => policy.young_minor_target_words(live_words),
        CollectionKind::Major => policy.young_target_words(live_words),
    }
}
// ---------------------------------------------------------------------------
// 1. the request is carried, and it is one-shot
// ---------------------------------------------------------------------------
/// A growth that retires a space records the room it could not wait for; the collection that
/// follows spends it, and the next collection — with no growth before it — sizes on the live set
/// alone.
#[test]
fn the_request_is_carried_into_the_next_collection_and_spent_exactly_once() {
    for kind in KINDS {
        let policy = roomy_policy(233);
        let mut c = any_for(kind, policy.clone());
        let mut roots = build_live(&mut c, LIVE_WORDS);
        // The copy-out seam: it holds no published root set, so it cannot collect. It asks for
        // room instead, and the seam retires the space it replaces.
        let chain_before = c.heap_size_words();
        c.grow_nursery(REQUEST).expect("the growth seam grants the request");
        let chain = c.heap_size_words();
        assert!(
            chain > chain_before,
            "{kind:?}: the growth must retire a space ({chain_before} -> {chain})"
        );
        assert!(c.collection_debt(), "{kind:?}: a retired space is collection debt");
        // The collection that follows pays for the request it could not satisfy.
        let first = c.collect(&mut roots).expect("collect");
        let live = first.stats.words_copied;
        let paid = policy.young_after_collection_with_need(chain, live, REQUEST);
        assert_eq!(
            c.heap_size_words(),
            paid,
            "{kind:?}: the collapse must carry the growth's request (chain {chain}, live {live})"
        );
        assert!(
            paid >= REQUEST,
            "{kind:?}: the surviving nursery covers the request it was grown for"
        );
        // One-shot: no growth happened since, so the request is spent and the plain rule decides.
        let before = c.heap_size_words();
        let second = c.collect(&mut roots).expect("collect");
        let live_now = second.stats.words_copied;
        let plain = policy.young_after_collection(before, live_now);
        let inflated = policy.young_after_collection_with_need(before, live_now, REQUEST);
        assert_ne!(
            plain, inflated,
            "{kind:?}: the two rules must differ for this case to mean anything"
        );
        assert_eq!(
            c.heap_size_words(),
            plain,
            "{kind:?}: the request was consumed, so the capacity is the plain live-set figure"
        );
        assert!(
            c.heap_size_words() < inflated,
            "{kind:?}: the capacity does not ratchet onto the request"
        );
        eprintln!(
            "{kind:?}: chain {chain_before} -> {chain} for a {REQUEST}-word request; \
             the collection installed {paid} (live {live}); the next one {plain} \
             (was the need-inflated {inflated} an option it would have taken)"
        );
    }
}
// ---------------------------------------------------------------------------
// 2. the grow -> collect -> shrink -> grow cycle is broken
// ---------------------------------------------------------------------------
/// One copy-out round: the seam is asked for `REQUEST` words (a no-op once the settled capacity
/// covers it), the lump is written and abandoned, then the heap-admission door runs. The counters
/// are the witness: `growths` is `O(1)` in the round count, and after the settling admission the
/// collections are occupancy-driven only.
fn copy_out_loop(kind: MatureKind, initial_young_words: usize, rounds: usize) -> (usize, usize, usize, usize) {
    let policy = roomy_policy(initial_young_words);
    let mut c = any_for(kind, policy);
    let mut roots = build_live(&mut c, LIVE_WORDS);
    let (mut growths, mut collections, mut debt_admissions) = (0usize, 0usize, 0usize);
    for _ in 0..rounds {
        // The copy-out seam: room, not a collection (the machine's roots are unpublished here).
        if c.young().available_words() < REQUEST {
            c.grow_nursery(REQUEST).expect("the growth seam grants the request");
            growths += 1;
        }
        // The moved result, then abandoned: exactly the garbage a collection reclaims.
        write_lump(&mut c, REQUEST);
        // The heap-admission door (`beam_rt/src/context/gc.rs`): occupancy **or** chain debt.
        if c.collection_debt() {
            debt_admissions += 1;
        }
        if c.should_collect(REQUEST) || c.collection_debt() {
            c.ensure_capacity(REQUEST, &mut roots).expect("collect, then grow");
            collections += 1;
        }
    }
    eprintln!(
        "{kind:?}: {rounds} rounds with a {REQUEST}-word copy-out -> {growths} growths, \
         {collections} collections ({debt_admissions} on collection debt), settled nursery {} words",
        c.heap_size_words()
    );
    (growths, collections, debt_admissions, c.heap_size_words())
}
#[test]
fn the_copy_out_cycle_settles_on_a_capacity_that_covers_the_request() {
    const ROUNDS: usize = 200;
    for kind in KINDS {
        let (g, collections, debt, settled) = copy_out_loop(kind, 987, ROUNDS);
        let (g4, collections4, debt4, settled4) = copy_out_loop(kind, 987, 4 * ROUNDS);
        // The mechanism, as a derivation: the live set's own target does **not** cover the
        // request (which is what re-drove the seam on every copy-out), and the collection's
        // need-carrying target does.
        let policy = roomy_policy(987);
        let plain = policy.young_minor_target_words(LIVE_WORDS);
        let with_need = policy.young_minor_target_words_with_need(LIVE_WORDS, REQUEST);
        assert!(
            plain < REQUEST && with_need >= REQUEST,
            "{kind:?}: the two targets must bracket the request ({plain} < {REQUEST} <= {with_need})"
        );
        // The settled capacity covers the request, so the seam is a no-op afterwards.
        assert!(
            settled >= REQUEST && settled4 >= REQUEST,
            "{kind:?}: the settled nursery must cover the request ({settled} / {settled4})"
        );
        // Growths are **not** proportional to the round count: a fixed transient settles it.
        assert!(
            g4 <= g + 1,
            "{kind:?}: growths must not grow with the round count ({g} at {ROUNDS}, {g4} at {})",
            4 * ROUNDS
        );
        assert!(
            g <= 4,
            "{kind:?}: only the settling growths retire a space, got {g}"
        );
        // ... and neither are the debt-driven admissions: the chain stops being rebuilt.
        assert!(
            debt4 <= debt + 1,
            "{kind:?}: debt admissions must not grow with the round count ({debt} -> {debt4})"
        );
        // The collections follow the settled capacity: one per `settled / REQUEST` rounds.
        let per_round = (settled / REQUEST).max(1);
        assert!(
            collections <= ROUNDS / per_round + 2 * per_round,
            "{kind:?}: {collections} collections over {ROUNDS} rounds of a {per_round}-round capacity"
        );
        assert!(
            collections4 <= 4 * collections + 2 * per_round,
            "{kind:?}: collections scale with the round count, not faster ({collections} -> {collections4})"
        );
    }
}
// ---------------------------------------------------------------------------
// 3. the give-back seam still gives back (the request is discarded, not banked)
// ---------------------------------------------------------------------------
/// Several growth events with no collection in between, then **one** `trim_retired`: the chain is
/// one space and the capacity is the **plain** live-set target — not the need-inflated one the
/// collection inside the trim would have installed.
#[test]
fn the_give_back_seam_installs_the_plain_target_after_a_grown_chain() {
    for kind in KINDS {
        let policy = roomy_policy(233);
        let mut c = any_for(kind, policy.clone());
        let mut roots = build_live(&mut c, LIVE_WORDS);
        for event in 0..3 {
            let before = c.young_space_count();
            fill_to(c.young_mut(), REQUEST);
            c.grow_nursery(REQUEST).expect("the growth seam grants the request");
            assert_eq!(
                c.young_space_count(),
                before + 1,
                "{kind:?}: event {event} must retire a space"
            );
        }
        assert!(c.collection_debt(), "{kind:?}: the grown chain is collection debt");
        let collection = c.trim_retired(&mut roots).expect("trim");
        let plain = plain_target(&policy, collection.kind, collection.stats.words_copied);
        let inflated = policy.young_after_collection_with_need(
            c.heap_size_words(),
            collection.stats.words_copied,
            REQUEST,
        );
        assert_eq!(c.young_space_count(), 1, "{kind:?}: the trim collapses the chain");
        assert!(!c.collection_debt(), "{kind:?}: the trim pays the debt");
        assert_eq!(
            c.heap_size_words(),
            plain,
            "{kind:?}: the give-back installs the live set's own target for the kind that ran"
        );
        assert!(
            plain < inflated,
            "{kind:?}: the pending request is discarded, not banked ({plain} vs {inflated})"
        );
        eprintln!(
            "{kind:?}: {:?} trim -> {} words (the request would have paid for {inflated})",
            collection.kind, plain
        );
    }
}
