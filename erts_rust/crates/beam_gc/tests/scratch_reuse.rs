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

//! **The collector's reusable walk scratch is a pure allocation saving** — two consecutive
//! collections over one collector, each with its own root set, report exactly what a freshly
//! constructed collector reports.
//!
//! Every collection walks the live graph before it copies it, and the state that walk needs (a
//! visited bitmap keyed on from-space **addresses**, a worklist, the from-set's range vector) is
//! owned once by the collector instead of being re-allocated per round (`beam_gc`'s
//! `reachable::Scratch`). Reuse is only sound because every walk zeroes the whole used prefix of
//! the bitmap and truncates the worklist **before** it admits anything, and because the range
//! vector is only ever installed through a clear-then-fill:
//!
//! * a stale set bit makes the next walk skip an object at an address it has *already* seen — an
//!   under-count, which through the room figures strands a live term;
//! * a stale range makes a collection evacuate a space that may already be freed.
//!
//! The second round therefore has to be *watched*, not assumed: it runs over a different from-set
//! (a different nursery generation — on this fixture the collapse re-installs a space of the same
//! capacity, so it is often literally the same addresses) and a different root set, and its
//! figures must match a one-collection reference collector **exactly**.
//!
//! # The contract (asserted below, for both mature strategies)
//!
//! 1. Round one evacuates its own root set (`words_copied` / `objects_copied` are that graph's).
//! 2. Round two — over the same collector, hence over the reused scratch — evacuates **its** root
//!    set and no trace of round one: its figures equal those of a fresh collector fed the same
//!    second-round heap and roots.
//! 3. Round one's graph is untouched (a minor never rescans the mature generation), and both
//!    graphs are reachable from their roots afterwards.
//!
//! The mature space is deliberately kept small while each round's nursery fill is large, so the
//! copying collector's room figure really comes from the live walk (not from the chain-summed
//! fill short-circuit) in **both** rounds — otherwise the reuse this file is about would not be
//! exercised at all.

use beam_gc::prelude::{AnyCollector, GcConfig, HeapPolicy, MatureKind};
use beam_term::prelude::{make_small, ptr_val, Eterm, NIL};

/// Both mature strategies, so the pin is strategy-independent.
const KINDS: [MatureKind; 2] = [MatureKind::Copying, MatureKind::Immix];

/// A collector for `policy` with `kind` **pinned**.
fn any_for(kind: MatureKind, policy: HeapPolicy) -> AnyCollector {
    AnyCollector::from_config(&GcConfig::new(policy).with_mature(kind)).expect("collector")
}

/// A 233-word nursery and a 90-word mature space (one mark-region block), so each round's fill
/// exceeds the mature room and the live walk decides the copy collector's room figure.
fn policy() -> HeapPolicy {
    HeapPolicy {
        initial_young_words: 233,
        initial_mature_words: 90,
        ..HeapPolicy::default()
    }
}

/// Round one's graph: the 5-word tuple `{1,2,3,4}`.
fn build_a(c: &mut AnyCollector) -> Eterm {
    c.young_mut()
        .heap_mut()
        .tuple(&[make_small(1), make_small(2), make_small(3), make_small(4)])
        .expect("a")
}

/// Round two's graph: the 4-word tuple `{5,6,7}` — a *different* size, so a mis-attributed round
/// is visible in `words_copied` **and** in `objects_copied`.
fn build_b(c: &mut AnyCollector) -> Eterm {
    c.young_mut()
        .heap_mut()
        .tuple(&[make_small(5), make_small(6), make_small(7)])
        .expect("b")
}

/// Buries the current nursery under dead 2-word cons cells, so the round's fill exceeds the
/// mature space's room (the live walk, not the fill short-circuit, then decides the copy
/// collector's minor room).
fn bury(c: &mut AnyCollector, cells: usize) {
    for i in 0..cells {
        c.young_mut()
            .heap_mut()
            .cons(make_small(i as i64), NIL)
            .expect("garbage cell");
    }
}

/// The figures round two must reproduce: `(words_copied, objects_copied)`.
const ROUND_ONE: (usize, usize) = (5, 1);
const ROUND_TWO: (usize, usize) = (4, 1);

#[test]
fn two_collections_over_one_collector_report_their_own_live_sets() {
    for kind in KINDS {
        let policy = policy();
        let mut c = any_for(kind, policy.clone());

        // Round one.
        let a = build_a(&mut c);
        bury(&mut c, 90);
        assert!(
            c.young().used_words() > 0,
            "{kind:?}: the mutator really allocated"
        );
        let mut roots = [a];
        let first = c.collect_minor(&mut roots).expect("the first minor");
        assert_eq!(
            (first.words_copied, first.objects_copied),
            ROUND_ONE,
            "{kind:?}: round one evacuates its own graph"
        );
        let a = roots[0];
        assert!(
            c.mature_contains(ptr_val(a) as *const Eterm),
            "{kind:?}: round one's graph is promoted into the mature generation"
        );

        // Round two — the same collector, so the same (now dirty) scratch, a different root set.
        let b = build_b(&mut c);
        bury(&mut c, 90);
        let mut roots = [b];
        let second = c.collect_minor(&mut roots).expect("the second minor");
        assert_eq!(
            (second.words_copied, second.objects_copied),
            ROUND_TWO,
            "{kind:?}: round two evacuates *its* graph and no trace of round one"
        );
        assert_eq!(c.minor_gcs(), 2, "{kind:?}: two minors ran");
        assert_eq!(
            c.young().used_words(),
            0,
            "{kind:?}: the nursery is empty again"
        );

        // Round one's graph is untouched by round two (a minor never rescans the mature
        // generation), and both graphs are reachable from their roots.
        assert!(
            c.mature_contains(ptr_val(a) as *const Eterm),
            "{kind:?}: round one's graph survived round two"
        );
        let both = [a, roots[0]];
        let mut reachable = 0usize;
        c.for_each_reachable_object(&both, |_header, _ptr| reachable += 1)
            .expect("the whole-heap walk");
        assert_eq!(
            reachable, 2,
            "{kind:?}: exactly the two promoted tuples are reachable"
        );

        // The reference: a **fresh** collector (a clean scratch) fed the same second-round heap
        // and roots. Same figures ⇒ the reuse changed nothing observable.
        let mut fresh = any_for(kind, policy);
        let fresh_b = build_b(&mut fresh);
        bury(&mut fresh, 90);
        let mut fresh_roots = [fresh_b];
        let reference = fresh
            .collect_minor(&mut fresh_roots)
            .expect("the reference minor");
        assert_eq!(
            (second.words_copied, second.objects_copied),
            (reference.words_copied, reference.objects_copied),
            "{kind:?}: the reused scratch must report what a fresh one does"
        );
    }
}

/// The same property one step up: a **fullsweep** between two minors — so the reused scratch goes
/// through the whole-heap (nursery chain *plus* mature) from-set and then back to a nursery-only
/// one — still leaves every round reporting its own live set.
///
/// This is the from-set transition a `Scratch` must survive: a fullsweep's range set is a
/// superset of a minor's, and the bitmap that covered it is longer than the next minor's needs.
#[test]
fn a_fullsweep_between_minors_reinstalls_the_from_set_each_time() {
    for kind in KINDS {
        let mut c = any_for(kind, policy());

        // A promoted graph, then a fullsweep that also carries a nursery term.
        let a = build_a(&mut c);
        let mut roots = [a];
        c.collect_minor(&mut roots).expect("the promoting minor");
        let a = roots[0];

        let carry = build_b(&mut c);
        let mut roots = [a, carry];
        let major = c.collect_major(&mut roots).expect("the fullsweep");
        // The fullsweep evacuates the nursery term and keeps the mature graph alive. *How* the
        // mature half is accounted differs between the two strategies — the copying collector
        // relocates the mature space too (so the 5-word tuple is copied again, `4 + 5`), while the
        // mark-region one reclaims it in place (it is a root, so it is marked and pinned, not
        // copied) — so the pin is the part both owe: the nursery term is copied and nothing is
        // lost.
        assert!(
            major.words_copied >= ROUND_TWO.0,
            "{kind:?}: the fullsweep copies the nursery term ({} words)",
            major.words_copied
        );
        let mut reachable = 0usize;
        c.for_each_reachable_object(&roots, |_header, _ptr| reachable += 1)
            .expect("the whole-heap walk");
        assert_eq!(
            reachable, 2,
            "{kind:?}: the fullsweep keeps both graphs reachable"
        );

        // A minor after the fullsweep: a nursery-only from-set, over a scratch whose last walk
        // covered the whole heap.
        let after = build_b(&mut c);
        let mut roots = [after];
        let minor = c
            .collect_minor(&mut roots)
            .expect("the minor after the fullsweep");
        assert_eq!(
            (minor.words_copied, minor.objects_copied),
            ROUND_TWO,
            "{kind:?}: the post-fullsweep minor reports only its own young live set"
        );
    }
}
