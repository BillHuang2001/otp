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

//! The public **reachability enumeration** — `for_each_reachable_object` on [`Collector`],
//! [`GenImmixCollector`] and [`AnyCollector`], and the **safe** `for_each_reachable_object_view`
//! siblings that report the same objects as a read-only [`ReachableObject`] view so a visitor
//! needs **no `unsafe`** of its own.
//!
//! This is the **marking half** of an off-heap liveness scan: a caller that owns an off-heap
//! object list can enumerate the heap to discover which on-heap handles still name a payload,
//! instead of retaining every entry it cannot decide. The file pins the properties a host
//! relies on:
//!
//! 1. **Whole-heap and exact.** A live graph spanning the young nursery (including a
//!    **chained** nursery after [`grow_nursery`]) and the mature generation (after a
//!    fullsweep) yields exactly the reachable boxed objects, **once each**, and **no garbage**
//!    object. The walk is **read-only**.
//! 2. **Off-heap handles are found.** A live refc binary's `ErlSubBits`/`BinRef` handle **is**
//!    enumerated, so a caller can read the payload pointer its `BinRef` names; an
//!    **unreachable** one is not. The **safe view** does the whole probe with zero `unsafe`.
//! 3. **Iterative and cycle-safe.** A 50 000-object chain neither recurses nor hangs; a cycle
//!    visits each object once and terminates — including a cons-cell ring, whose cells are
//!    traversed but not reported (they carry no header word).
//! 4. **Strategy-agnostic.** [`AnyCollector`] forwards the enumeration to whichever mature
//!    strategy it selected, for both strategies — the raw and the view form alike.
//!
//! The walk is **layout-agnostic** — it reports the reachable objects, and the *caller* applies
//! its own predicate to the reported objects. Every `unsafe` block below is confined to
//! building a term graph (a cycle, a refc pair) or reading a reported object's own words, and
//! carries a `// SAFETY:` rationale. The view-based tests deliberately keep their **visitor**
//! `unsafe`-free.

use beam_alloc::RefcBinary;
use beam_common::Result;
use beam_gc::prelude::{
    AnyCollector, Collector, GcConfig, HeapPolicy, HeapSpace, MatureKind, ReachableObject,
};
use beam_term::prelude::{
    boxed_val, make_small, ptr_val, write_refc_binary, Eterm, ERL_REFC_BITS_SIZE, HEADER_BIN_REF,
    HEADER_SUB_BITS, NIL,
};

/// Runs a collector's `for_each_reachable_object` and returns the `(header, address)` pairs it
/// reported. `run` is the collector's own method, so the exact same helper serves all three
/// collector types.
fn enumerate(
    run: impl FnOnce(&mut dyn FnMut(Eterm, *const Eterm)) -> Result<()>,
) -> Vec<(Eterm, usize)> {
    let mut out: Vec<(Eterm, usize)> = Vec::new();
    run(&mut |header, ptr| out.push((header, ptr as usize)))
        .expect("the reachability walk never fails");
    out
}

/// Runs a collector's `for_each_reachable_object_view` and returns the `(header, address)`
/// pairs it reported — the **safe** sibling of [`enumerate`], reading the view's own accessors
/// instead of a raw pointer.
fn enumerate_view(
    run: impl FnOnce(&mut dyn FnMut(ReachableObject<'_>)) -> Result<()>,
) -> Vec<(Eterm, usize)> {
    let mut out: Vec<(Eterm, usize)> = Vec::new();
    run(&mut |obj: ReachableObject<'_>| out.push((obj.header(), obj.address())))
        .expect("the reachability walk never fails");
    out
}

/// The header addresses of the reported objects, sorted, for a set comparison.
fn addresses(seen: &[(Eterm, usize)]) -> Vec<usize> {
    let mut addrs: Vec<usize> = seen.iter().map(|(_, addr)| *addr).collect();
    addrs.sort_unstable();
    addrs
}

/// A sorted address list for the given terms.
fn term_addresses(terms: &[Eterm]) -> Vec<usize> {
    let mut addrs: Vec<usize> = terms.iter().map(|t| ptr_val(*t) as usize).collect();
    addrs.sort_unstable();
    addrs
}

/// A tuple from `elems` in `space` (a nursery space).
fn tuple_in(space: &mut HeapSpace, elems: &[Eterm]) -> Eterm {
    space.heap_mut().tuple(elems).expect("nursery tuple")
}

// ---------------------------------------------------------------------------
// 1. Whole-heap: young + mature + a chained nursery, exact and read-only
// ---------------------------------------------------------------------------

#[test]
fn the_enumeration_covers_young_mature_and_a_chained_nursery() {
    let policy = HeapPolicy {
        initial_young_words: 64,
        initial_mature_words: 256,
        ..HeapPolicy::default()
    };
    let mut c = Collector::new(policy).expect("collector");

    // `m = {1, 2}` in the nursery; a fullsweep promotes it, so the mature generation holds a
    // live object.
    let m = tuple_in(c.young_mut(), &[make_small(1), make_small(2)]);
    let mut roots = vec![m];
    c.collect_major(&mut roots).expect("fullsweep");
    let m = roots[0];
    assert!(
        c.mature().contains_term(m),
        "the tuple must be mature-resident after a fullsweep"
    );

    // `y = {m, 3}` in the current nursery — a young -> mature edge.
    let y = tuple_in(c.young_mut(), &[m, make_small(3)]);

    // Retire the current space (the nursery becomes a **chain**) and build `z = {y, 4}` in the
    // fresh space — a reference across the growth boundary.
    c.grow_nursery(4096).expect("grow");
    assert!(c.young_space_count() >= 2, "the nursery chain is live");
    let z = tuple_in(c.young_mut(), &[y, make_small(4)]);

    // Garbage: allocated in the fresh space, never referenced from a root.
    let _garbage = tuple_in(c.young_mut(), &[make_small(9)]);

    let roots = vec![m, y, z];
    let roots_before = roots.clone();
    let used_before = c.young().used_words();

    let seen = enumerate(|visit| c.for_each_reachable_object(&roots, visit));

    assert_eq!(
        addresses(&seen),
        term_addresses(&[m, y, z]),
        "exactly the reachable boxed objects, once each, across young + mature + the chain"
    );

    // Read-only: no root was rewritten and no word was allocated.
    assert_eq!(roots, roots_before, "the enumeration rewrote no root");
    assert_eq!(
        c.young().used_words(),
        used_before,
        "the enumeration allocated nothing"
    );
}

// ---------------------------------------------------------------------------
// 2. Off-heap refc-binary handles are found; unreachable ones are not
// ---------------------------------------------------------------------------

/// Writes a full refc binary (`[BinRef(3)][ErlSubBits(5)]`) for `payload` into `space` and
/// returns the tagged root (the `ErlSubBits`).
fn write_pair(space: &mut HeapSpace, payload: &RefcBinary) -> Eterm {
    let size_bits = payload.as_bytes().len() * 8;
    // SAFETY: exactly `ERL_REFC_BITS_SIZE` (8) exclusively-owned words are reserved at `hp`
    // (the `write_refc_binary` contract); `payload.data_ptr()` is a live, word-aligned pointer
    // to `size_bits` bits of off-heap storage (kept alive by `payload` for the whole test), and
    // `null` is the (empty) off-heap chain link.
    unsafe {
        let hp = space
            .heap_mut()
            .alloc_words(ERL_REFC_BITS_SIZE)
            .expect("8 words for the pair");
        write_refc_binary(
            hp,
            payload.data_ptr() as *mut Eterm,
            core::ptr::null_mut(),
            payload.data_ptr(),
            0,
            size_bits,
        )
    }
}

#[test]
fn a_live_off_heap_binary_handle_is_found_and_an_unreachable_one_is_not() {
    let policy = HeapPolicy {
        initial_young_words: 256,
        ..HeapPolicy::default()
    };
    let mut c = Collector::new(policy).expect("collector");

    let live_payload = RefcBinary::new(b"the live off-heap binary payload").expect("payload");
    let dead_payload = RefcBinary::new(b"an unreachable off-heap payload").expect("payload");

    // The live pair, reachable through a tuple (not a bare root); the dead pair is never rooted.
    let live = write_pair(c.young_mut(), &live_payload);
    let _dead = write_pair(c.young_mut(), &dead_payload);
    let holder = tuple_in(c.young_mut(), &[live]);

    let roots = vec![holder];
    let seen = enumerate(|visit| c.for_each_reachable_object(&roots, visit));

    // The caller's own predicate: `HEADER_SUB_BITS` marks the handle, `HEADER_BIN_REF` names the
    // off-heap payload through its `val` word — exactly the scan a host runs to find which
    // off-heap slots are still referenced.
    let mut saw_sub_bits = false;
    let mut named: Vec<usize> = Vec::new();
    for (header, addr) in &seen {
        if *header == HEADER_SUB_BITS {
            saw_sub_bits = true;
        }
        if *header == HEADER_BIN_REF {
            // SAFETY: `addr` is a live, reachable 3-word `BinRef`; word 1 is its `val` pointer.
            let val = unsafe { *(*addr as *const Eterm).add(1) };
            named.push(val.raw());
        }
    }

    assert!(saw_sub_bits, "the live `ErlSubBits` handle must be enumerated");
    assert!(
        named.contains(&(live_payload.data_ptr() as usize)),
        "the live `BinRef` must name the live off-heap payload"
    );
    assert!(
        !named.contains(&(dead_payload.data_ptr() as usize)),
        "the unreachable `BinRef` must not be enumerated"
    );
}

// ---------------------------------------------------------------------------
// 3. Iterative and cycle-safe
// ---------------------------------------------------------------------------

#[test]
fn a_deep_chain_of_50_000_objects_is_iterative_and_exact() {
    const N: usize = 50_000;
    let policy = HeapPolicy {
        initial_young_words: 4 * N,
        ..HeapPolicy::default()
    };
    let mut c = Collector::new(policy).expect("collector");

    // A chain of `N` one-element tuples: each is a boxed object referencing the next, so the
    // walk's worklist is `N` deep. A recursive walk would overflow the stack here.
    let mut term = make_small(0);
    for _ in 0..N {
        term = tuple_in(c.young_mut(), &[term]);
    }

    let roots = vec![term];
    let seen = enumerate(|visit| c.for_each_reachable_object(&roots, visit));
    assert_eq!(
        seen.len(),
        N,
        "every one of the {N} chained tuples is visited exactly once"
    );
}

#[test]
fn a_cyclic_graph_visits_each_object_once_and_terminates() {
    let policy = HeapPolicy {
        initial_young_words: 128,
        ..HeapPolicy::default()
    };
    let mut c = Collector::new(policy).expect("collector");

    // A three-tuple **ring**: `a -> b -> c -> a`.
    let a = tuple_in(c.young_mut(), &[NIL]);
    let b = tuple_in(c.young_mut(), &[a]);
    let cc = tuple_in(c.young_mut(), &[b]);
    // SAFETY: `a` is a live 1-element tuple this test just allocated; its element 0 lives at
    // `boxed_val(a).add(1)`, and the write stays inside the tuple's two words.
    unsafe {
        *boxed_val(a).add(1) = cc;
    }

    let roots = vec![a];
    let seen = enumerate(|visit| c.for_each_reachable_object(&roots, visit));
    assert_eq!(
        addresses(&seen),
        term_addresses(&[a, b, cc]),
        "the cycle visits each object once and terminates"
    );

    // A cons-cell **ring** is traversed (so the boxed object it names is reached) but its cells
    // are not reported — they carry no header word.
    let cell_a = c.young_mut().heap_mut().cons(make_small(1), NIL).expect("cell a");
    let cell_b = c.young_mut().heap_mut().cons(make_small(2), cell_a).expect("cell b");
    let leaf = tuple_in(c.young_mut(), &[make_small(7)]);
    // SAFETY: `cell_a` is a live 2-word cons cell; word 0 is the car and word 1 the cdr, and
    // both writes stay inside the cell.
    unsafe {
        *ptr_val(cell_a).add(0) = leaf; // hang the boxed object on the ring
        *ptr_val(cell_a).add(1) = cell_b; // close the ring
    }

    let seen = enumerate(|visit| c.for_each_reachable_object(&[cell_a], visit));
    assert_eq!(seen.len(), 1, "only the boxed object is reported");
    assert_eq!(
        seen[0].1,
        ptr_val(leaf) as usize,
        "the boxed object named through the cons ring is reached"
    );
}

// ---------------------------------------------------------------------------
// 4. Strategy-agnostic: AnyCollector forwards for both strategies
// ---------------------------------------------------------------------------

#[test]
fn any_collector_forwards_the_enumeration_for_both_strategies() {
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let policy = HeapPolicy {
            initial_young_words: 256,
            initial_mature_words: 512,
            ..HeapPolicy::default()
        };
        let mut c = AnyCollector::from_config(&GcConfig::new(policy).with_mature(kind))
            .expect("collector");
        assert_eq!(c.kind(), kind, "{kind:?}: the strategy was pinned");

        // `m` is promoted (mature-resident) by a fullsweep.
        let m = tuple_in(c.young_mut(), &[make_small(1), make_small(2)]);
        let mut roots = vec![m];
        c.collect_major(&mut roots).expect("fullsweep");
        let m = roots[0];
        assert!(
            c.mature_contains(ptr_val(m) as *const Eterm),
            "{kind:?}: `m` must be mature-resident"
        );

        // A young graph referencing the mature object, plus garbage.
        let y = tuple_in(c.young_mut(), &[m, make_small(3)]);
        let _garbage = tuple_in(c.young_mut(), &[make_small(9)]);

        let roots = vec![m, y];
        let seen = enumerate(|visit| c.for_each_reachable_object(&roots, visit));
        assert_eq!(
            addresses(&seen),
            term_addresses(&[m, y]),
            "{kind:?}: `AnyCollector` forwards the enumeration to the selected strategy"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. The SAFE view: a visitor needs no `unsafe`
// ---------------------------------------------------------------------------

/// The off-heap `BinRef` probe a host runs — with **zero `unsafe`** in the visiting closure.
/// `obj.word(1)` reads the `BinRef`'s `val` word for us; the raw form needed
/// `unsafe { *ptr.add(1) }`.
#[test]
fn the_safe_view_finds_a_live_off_heap_binary_with_no_unsafe() {
    let policy = HeapPolicy {
        initial_young_words: 256,
        ..HeapPolicy::default()
    };
    let mut c = Collector::new(policy).expect("collector");

    let live_payload = RefcBinary::new(b"the live off-heap binary payload").expect("payload");
    let dead_payload = RefcBinary::new(b"an unreachable off-heap payload").expect("payload");

    let live = write_pair(c.young_mut(), &live_payload);
    let _dead = write_pair(c.young_mut(), &dead_payload);
    let holder = tuple_in(c.young_mut(), &[live]);

    let roots = vec![holder];

    // NOTE: this closure contains **no `unsafe`** — reading the payload pointer is the view's
    // job. The caller's predicate is exactly the one a host applies.
    let mut saw_sub_bits = false;
    let mut named: Vec<usize> = Vec::new();
    c.for_each_reachable_object_view(&roots, |obj| {
        // Accessor sanity, on a real object.
        assert_eq!(obj.word(0), Some(obj.header()));
        assert_eq!(obj.words()[0], obj.header());
        assert_eq!(obj.words().len(), obj.len());
        assert!(!obj.is_empty());
        assert_eq!(obj.address(), obj.words().as_ptr() as usize);

        if obj.header() == HEADER_SUB_BITS {
            saw_sub_bits = true;
        }
        if obj.header() == HEADER_BIN_REF {
            assert_eq!(obj.len(), 3, "a BinRef is exactly [header, val, next]");
            let val = obj.word(1).expect("the BinRef's `val` word is in range");
            named.push(val.raw());
            assert!(obj.word(obj.len()).is_none(), "a word past the end is None");
        }
    })
    .expect("the reachability walk never fails");

    assert!(saw_sub_bits, "the live `ErlSubBits` handle must be enumerated");
    assert!(
        named.contains(&(live_payload.data_ptr() as usize)),
        "the live `BinRef` must name the live off-heap payload"
    );
    assert!(
        !named.contains(&(dead_payload.data_ptr() as usize)),
        "the unreachable `BinRef` must not be enumerated"
    );
}

// ---------------------------------------------------------------------------
// 6. The view reports exactly the raw walk's objects (young + mature + chain)
// ---------------------------------------------------------------------------

#[test]
fn the_safe_view_reports_exactly_the_raw_walks_objects() {
    // The same young + mature + chained-nursery shape the raw enumeration test pins.
    let policy = HeapPolicy {
        initial_young_words: 64,
        initial_mature_words: 256,
        ..HeapPolicy::default()
    };
    let mut c = Collector::new(policy).expect("collector");

    let m = tuple_in(c.young_mut(), &[make_small(1), make_small(2)]);
    let mut roots = vec![m];
    c.collect_major(&mut roots).expect("fullsweep");
    let m = roots[0];
    assert!(c.mature().contains_term(m), "`m` is mature-resident");

    let y = tuple_in(c.young_mut(), &[m, make_small(3)]);
    c.grow_nursery(4096).expect("grow");
    let z = tuple_in(c.young_mut(), &[y, make_small(4)]);
    let _garbage = tuple_in(c.young_mut(), &[make_small(9)]);

    let roots = vec![m, y, z];

    let raw = addresses(&enumerate(|visit| c.for_each_reachable_object(&roots, visit)));
    let view = addresses(&enumerate_view(|visit| {
        c.for_each_reachable_object_view(&roots, visit)
    }));

    assert_eq!(
        view, raw,
        "the safe view reports exactly the raw walk's object addresses"
    );
    assert_eq!(
        view,
        term_addresses(&[m, y, z]),
        "and only the reachable objects, once each"
    );
}

// ---------------------------------------------------------------------------
// 7. The view shares the raw walk's guarantees (iterative, cycle-safe)
// ---------------------------------------------------------------------------

#[test]
fn the_safe_view_is_iterative_and_cycle_safe() {
    // A 50 000-object chain: the view walk is the same worklist, so it cannot recurse either.
    const N: usize = 50_000;
    let policy = HeapPolicy {
        initial_young_words: 4 * N,
        ..HeapPolicy::default()
    };
    let mut c = Collector::new(policy).expect("collector");

    let mut term = make_small(0);
    for _ in 0..N {
        term = tuple_in(c.young_mut(), &[term]);
    }
    let roots = vec![term];
    let seen = enumerate_view(|visit| c.for_each_reachable_object_view(&roots, visit));
    assert_eq!(
        seen.len(),
        N,
        "every one of the {N} chained tuples is visited exactly once by the view"
    );

    // A three-tuple **ring**: each object once, then terminate.
    let a = tuple_in(c.young_mut(), &[NIL]);
    let b = tuple_in(c.young_mut(), &[a]);
    let cc = tuple_in(c.young_mut(), &[b]);
    // SAFETY: `a` is a live 1-element tuple this test just allocated; its element 0 lives at
    // `boxed_val(a).add(1)`, and the write stays inside the tuple's two words.
    unsafe {
        *boxed_val(a).add(1) = cc;
    }

    let roots = vec![a];
    let seen = enumerate_view(|visit| c.for_each_reachable_object_view(&roots, visit));
    assert_eq!(
        addresses(&seen),
        term_addresses(&[a, b, cc]),
        "the view walk terminates on the cycle and visits each object once"
    );
}

// ---------------------------------------------------------------------------
// 8. The view is strategy-agnostic and its accessors are exact
// ---------------------------------------------------------------------------

#[test]
fn any_collector_forwards_the_safe_view_for_both_strategies() {
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let policy = HeapPolicy {
            initial_young_words: 256,
            initial_mature_words: 512,
            ..HeapPolicy::default()
        };
        let mut c = AnyCollector::from_config(&GcConfig::new(policy).with_mature(kind))
            .expect("collector");

        let m = tuple_in(c.young_mut(), &[make_small(1), make_small(2)]);
        let mut roots = vec![m];
        c.collect_major(&mut roots).expect("fullsweep");
        let m = roots[0];

        let y = tuple_in(c.young_mut(), &[m, make_small(3)]);
        let _garbage = tuple_in(c.young_mut(), &[make_small(9)]);

        let roots = vec![m, y];
        let view = addresses(&enumerate_view(|visit| {
            c.for_each_reachable_object_view(&roots, visit)
        }));
        let raw = addresses(&enumerate(|visit| c.for_each_reachable_object(&roots, visit)));

        assert_eq!(
            view,
            term_addresses(&[m, y]),
            "{kind:?}: `AnyCollector` forwards the safe view to the selected strategy"
        );
        assert_eq!(
            view, raw,
            "{kind:?}: the view and the raw walk agree for the selected strategy"
        );
    }
}

#[test]
fn the_view_accessors_are_exact_for_a_known_object() {
    let policy = HeapPolicy {
        initial_young_words: 64,
        ..HeapPolicy::default()
    };
    let mut c = Collector::new(policy).expect("collector");

    // A 3-element tuple: `[header, e0, e1, e2]`.
    let t = tuple_in(c.young_mut(), &[make_small(1), make_small(2), make_small(3)]);
    let roots = vec![t];

    let mut visited = 0usize;
    c.for_each_reachable_object_view(&roots, |obj| {
        visited += 1;
        assert_eq!(obj.len(), 4, "a 3-element tuple is [header, e0, e1, e2]");
        assert_eq!(obj.words().len(), obj.len());
        assert!(!obj.is_empty());
        assert_eq!(obj.word(0), Some(obj.header()));
        assert_eq!(obj.words()[0], obj.header());
        assert_eq!(obj.address(), obj.words().as_ptr() as usize);
        assert_eq!(obj.address(), ptr_val(t) as usize, "the view's identity is the header address");
        assert_eq!(obj.word(1), Some(make_small(1)));
        assert_eq!(obj.word(2), Some(make_small(2)));
        assert_eq!(obj.word(3), Some(make_small(3)));
        assert_eq!(obj.word(4), None, "a word past the end is None");
        assert_eq!(obj.word(usize::MAX), None);
    })
    .expect("the reachability walk never fails");

    assert_eq!(visited, 1, "the single rooted tuple is visited once");
}
