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

//! **Adversarial copy-correctness tests** — cases deliberately built to *break* the
//! collector rather than to confirm it on a friendly corpus.
//!
//! Every case is driven through **both** mature strategies
//! ([`AnyCollector::Copying`](beam_gc::AnyCollector::Copying) and
//! [`AnyCollector::Immix`](beam_gc::AnyCollector::Immix), built with the explicit pins
//! `GcConfig::copying`/`GcConfig::immix` so the automatic rule cannot substitute one for the
//! other) and, where it is the meaningful primitive to attack, through the raw copying path
//! (`HeapSpace` + [`collect`](beam_gc::collect)). After **every** collection each case
//! asserts the same three things:
//!
//! * **no wrong result** — the copied graph is walked by hand (never with
//!   `beam_term::compare::eq`/`size_object`, both of which *recurse* and would themselves
//!   blow up on the deep corpora below) and every leaf still holds its value;
//! * **no panic** — the collector returns a typed error where it cannot proceed (one case
//!   pins that a too-small destination is a typed `BeamError::Alloc`, with nothing copied);
//! * **no leak** — the nursery is empty afterwards, every root was relocated into the
//!   destination space, and the off-heap accounting (refcounts, list length, destructor
//!   count) is exactly what it was before.
//!
//! The six cases: (1) a 100 000-cell cons list / 50 000-deep tuple chain (the copier must
//! be **iterative**); (2) a live graph ending **exactly** at the space capacity, collected
//! across a minor and a major; (3) an off-heap refc binary behind an on-heap
//! `[BinRef][ErlSubBits]` pair, freed exactly once when swept dead; (4) all three fun kinds
//! with boxed free variables; (5) a 10 000-element tuple plus a flat and a >32-key HAMT map;
//! (6) an off-heap MSO whose destructor must run exactly once.
//!
//! `unsafe` below only dereferences pointers this test allocated and wrote itself (or that a
//! collection just wrote); every block carries a `// SAFETY:` rationale and copies the
//! conventions of `tests/{copy_correctness,gc_pressure_offheap,refc_binary_copy,fun_copy,
//! map_copy}.rs`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use beam_alloc::{
    default_allocator, AllocRef, AllocatorConfig, CarrierAllocator, Heap, RefcBinary,
};
use beam_common::BeamError;
use beam_gc::prelude::{
    collect, AnyCollector, GcConfig, HeapPolicy, HeapSpace, MatureKind, Mso, OffHeap, OffHeapEntry,
};
use beam_term::prelude::{
    arityval, bin_ref_parts, boxed_val, fun_arity, fun_creator_pid, fun_env, fun_function,
    fun_index, fun_kind_of_header, fun_module, fun_num_free, fun_uniq, fun_uniq_bytes, fun_words,
    is_arity_value, is_atom, is_boxed, is_fun_header, is_list, is_refc_binary, is_small, list_val,
    make_small, map_get, map_pairs, map_size, map_words_capacity, refc_bytes, refc_size,
    signed_value, write_fun_external, write_fun_imported, write_fun_local, write_map,
    write_refc_binary, Atom, AtomTable, Eterm, FunKind, FunPid, ERL_REFC_BITS_SIZE, NIL,
};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// The depth of the cons list the deep case builds (≥ the 100 000 the objective asks for).
const DEEP_CELLS: usize = 100_000;
/// The depth of the nested tuple chain the deep case builds (`1`-element tuples).
const DEEP_CHAIN: usize = 50_000;
/// A generous upper bound on any list walk here: a cyclic or non-terminating list must fail
/// loudly rather than hang the test binary.
const WALK_BOUND: usize = 2_000_000;

/// Allocates an empty `words`-word semi-space over the default allocator.
fn space(words: usize) -> HeapSpace {
    HeapSpace::with_allocator(default_allocator().clone(), words).expect("heap allocation")
}

/// An isolated carrier allocator, so a case can observe an off-heap buffer being freed
/// through `AllocStats` without racing the process-wide registry allocator.
fn test_allocator(name: &str) -> AllocRef {
    CarrierAllocator::new(AllocatorConfig::builder().name(name).build())
        .expect("allocator construction maps nothing")
        .share()
}

/// A policy whose **explicit** mature size is tuned per strategy.
///
/// The mark-region space rounds every object up to a whole 16-word line, so a corpus of
/// many small objects needs a much larger *word* budget there than the copying space needs
/// for the very same graph. Both pins name a strategy explicitly, so the automatic rule
/// cannot substitute one for the other.
fn policy_for(
    kind: MatureKind,
    young: usize,
    copying_mature: usize,
    immix_mature: usize,
) -> HeapPolicy {
    let mature = match kind {
        MatureKind::Copying => copying_mature,
        MatureKind::Immix => immix_mature,
    };
    HeapPolicy {
        initial_young_words: young,
        initial_mature_words: mature,
        min_heap_words: young,
        ..HeapPolicy::default()
    }
}

/// Builds the collector for `kind` with `policy` through the **explicit** pin, and checks
/// the pin really beat the automatic rule.
fn collector(kind: MatureKind, policy: &HeapPolicy) -> AnyCollector {
    let config = match kind {
        MatureKind::Copying => GcConfig::copying(policy.clone()),
        MatureKind::Immix => GcConfig::immix(policy.clone()),
    };
    let c = AnyCollector::from_config(&config).expect("collector construction");
    assert_eq!(
        c.kind(),
        kind,
        "the explicit pin must select the requested strategy"
    );
    c
}

/// The two strategies, in a fixed order.
const BOTH: [MatureKind; 2] = [MatureKind::Copying, MatureKind::Immix];

/// Whether `term` is resident in the strategy's mature generation.
fn in_mature(c: &AnyCollector, term: Eterm) -> bool {
    match c {
        AnyCollector::Copying(x) => x.mature().contains_term(term),
        AnyCollector::Immix(x) => x.mature().contains_term(term),
    }
}

/// The post-collection invariants every strategy must show: an empty nursery and every root
/// relocated out of it into the mature generation.
fn assert_collected_no_leak(c: &AnyCollector, roots: &[Eterm], ctx: &str) {
    assert_eq!(
        c.young().used_words(),
        0,
        "{ctx}: the nursery must be empty after a collection"
    );
    for (i, root) in roots.iter().enumerate() {
        assert!(
            !c.young().contains_term(*root),
            "{ctx}: root {i} stayed in the nursery"
        );
        assert!(
            in_mature(c, *root),
            "{ctx}: root {i} was not relocated into the mature generation"
        );
    }
}

// ---------------------------------------------------------------------------
// Iterative walkers (deep graphs must not grow the real stack)
// ---------------------------------------------------------------------------

/// Reserves `cells` cons cells from `heap` and returns them as one proper list whose
/// elements are `0..cells` in order.
///
/// Iterative by construction (a loop, not recursion), so building a 100 000-cell list is
/// itself stack-safe.
fn build_deep_list(heap: &mut Heap, cells: usize) -> Eterm {
    let mut head = NIL;
    for i in (0..cells as i64).rev() {
        head = heap.cons(make_small(i), head).expect("deep cons cell");
    }
    head
}

/// Reserves `depth` `1`-element tuples from `heap`, each pointing at the next, and returns
/// the outermost one; the innermost element is the immediate `-1`.
///
/// Iterative by construction.
fn build_deep_tuple_chain(heap: &mut Heap, depth: usize) -> Eterm {
    let mut term = make_small(-1);
    for _ in 0..depth {
        term = heap.tuple(&[term]).expect("deep tuple");
    }
    term
}

/// Walks a proper list of small integers **iteratively**, returning them in order.
fn walk_list_smalls(mut term: Eterm) -> Vec<i64> {
    let mut values = Vec::new();
    while is_list(term) {
        // SAFETY: `term` is a live cons cell (`is_list`), so both of its words are in-bounds
        // and initialised.
        let cell = list_val(term);
        values.push(signed_value(unsafe { *cell }));
        term = unsafe { *cell.add(1) };
        assert!(
            values.len() <= WALK_BOUND,
            "the list must terminate (walked {WALK_BOUND} cells)"
        );
    }
    assert_eq!(term, NIL, "the list must be proper");
    values
}

/// Walks the `1`-element tuple chain `term` **iteratively**, returning its tuple depth and
/// the innermost immediate element.
fn walk_tuple_chain(mut term: Eterm) -> (usize, i64) {
    let mut depth = 0usize;
    while is_boxed(term) {
        // SAFETY: `term` is a live boxed term (`is_boxed`), so its header word is readable.
        let header = unsafe { *boxed_val(term) };
        assert!(is_arity_value(header), "every chain link must be a tuple");
        assert_eq!(arityval(header), 1, "every chain link must be a 1-tuple");
        // SAFETY: the arity-1 tuple has exactly one element at word 1.
        term = unsafe { *boxed_val(term).add(1) };
        depth += 1;
        assert!(
            depth <= WALK_BOUND,
            "the tuple chain must terminate (walked {WALK_BOUND} links)"
        );
    }
    assert!(is_small(term), "the chain must end in a small integer");
    (depth, signed_value(term))
}

/// Reads the elements of the tuple `t` **iteratively** into a vector (each must be a small
/// integer).
fn walk_tuple_smalls(t: Eterm) -> Vec<i64> {
    // SAFETY: `t` is a live boxed term the caller built; its header word is readable.
    let header = unsafe { *boxed_val(t) };
    assert!(is_arity_value(header), "the root must be a tuple");
    let n = arityval(header);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        // SAFETY: element `i` lies inside the live `n`-element tuple just validated.
        let e = unsafe { *boxed_val(t).add(1 + i) };
        assert!(is_small(e), "element {i} must be a small integer");
        out.push(signed_value(e));
    }
    out
}

// ---------------------------------------------------------------------------
// Off-heap liveness walk (the caller-side predicate an owning process supplies)
// ---------------------------------------------------------------------------

/// Records the off-heap payload pointer of every refc binary reachable from `term`.
///
/// This is the process-side liveness walk the collector deliberately does *not* perform; it
/// only handles the shapes these cases build (tuples, cons lists, funs, refc binaries).
fn collect_refc_payloads(term: Eterm, out: &mut Vec<*mut u8>) {
    if term == NIL || is_small(term) || is_atom(term) {
        return;
    }
    if is_list(term) {
        // SAFETY: `term` is a live cons cell, so both words are readable.
        let cell = list_val(term);
        collect_refc_payloads(unsafe { *cell }, out);
        collect_refc_payloads(unsafe { *cell.add(1) }, out);
        return;
    }
    if !is_boxed(term) {
        return;
    }
    // SAFETY: `term` is a live boxed term, so its header word is readable.
    let header = unsafe { *boxed_val(term) };
    // SAFETY: `term` is a live boxed term; `is_refc_binary` reads only its header/`orig`.
    if unsafe { is_refc_binary(term) } {
        // SAFETY: `term` is a live refc binary; `orig` (word 4) is a live `BinRef` term.
        let orig = unsafe { *boxed_val(term).add(4) };
        // SAFETY: `orig` is a live `BinRef`, so words 1 and 2 are in-bounds.
        let (val, _next) = unsafe { bin_ref_parts(orig) };
        out.push(val as *mut u8);
        return;
    }
    if is_fun_header(header) {
        // SAFETY: `term` is a live fun, so its environment words are readable.
        if let Ok(env) = unsafe { fun_env(term) } {
            for entry in env {
                collect_refc_payloads(*entry, out);
            }
        }
        return;
    }
    if is_arity_value(header) {
        let n = arityval(header);
        for i in 0..n {
            // SAFETY: element `i` lies inside the live `n`-element tuple just validated.
            collect_refc_payloads(unsafe { *boxed_val(term).add(1 + i) }, out);
        }
    }
}

/// The set of off-heap payload pointers the live roots still reference.
fn live_payloads(roots: &[Eterm]) -> Vec<*mut u8> {
    let mut out = Vec::new();
    for &root in roots {
        collect_refc_payloads(root, &mut out);
    }
    out
}

/// The liveness predicate an owning process passes to [`OffHeap::sweep`]: a binary entry is
/// live iff a reachable on-heap reference names its payload (these cases register no MSO
/// here, so anything else is treated as live).
fn binary_is_live(entry: &OffHeapEntry, live: &[*mut u8]) -> bool {
    match entry.as_binary() {
        Some(binary) => live.contains(&binary.data_ptr()),
        None => true,
    }
}

/// Writes the 8-word `[BinRef][ErlSubBits]` pair for `payload` at the heap top.
fn write_refc(heap: &mut Heap, payload: &RefcBinary) -> Eterm {
    let size_bits = payload.len() * 8;
    // SAFETY: exactly `ERL_REFC_BITS_SIZE` (8) exclusively-owned words are reserved;
    // `payload.data_ptr()` is a live, word-aligned pointer to `size_bits` bits of off-heap
    // storage kept alive by the caller, and `null` is the (empty) off-heap chain link.
    unsafe {
        let hp = heap
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

// ---------------------------------------------------------------------------
// Maps (the `i -> 3*i` pair set, both interiors)
// ---------------------------------------------------------------------------

/// The `i -> 3*i` input pair set (`n` distinct small-integer keys, so no hash collisions).
fn pairs(n: usize) -> Vec<(Eterm, Eterm)> {
    (0..n as i64)
        .map(|i| (make_small(i), make_small(i * 3)))
        .collect()
}

/// Reserves [`map_words_capacity`]`(n)` words and writes the `n`-key map there.
///
/// The reservation is the *sound upper bound*, so the trailing slack is never part of the
/// map graph; it is pre-filled with an immediate so no word is ever read uninitialised.
fn write_map_n(heap: &mut Heap, atoms: &AtomTable, n: usize) -> Eterm {
    let cap = map_words_capacity(n);
    // SAFETY: `cap` exclusively-owned, writable words are reserved; each is initialised to a
    // well-formed immediate, and `write_map` then overwrites a contiguous prefix of them
    // with a well-formed map graph whose root is returned.
    unsafe {
        let hp = heap.alloc_words(cap).expect("reserve the map block");
        for i in 0..cap {
            *hp.add(i) = make_small(0);
        }
        write_map(hp, &pairs(n), atoms).expect("build the map")
    }
}

/// Asserts `map` is the well-formed `i -> 3*i` map over `n` keys.
fn assert_map_pairs(map: Eterm, n: usize, atoms: &AtomTable) {
    assert_eq!(map_size(map, atoms).expect("map_size"), n, "map size");
    for i in 0..n as i64 {
        assert_eq!(
            map_get(map, make_small(i), atoms).expect("map_get"),
            Some(make_small(i * 3)),
            "key {i} must still resolve to its value"
        );
    }
    let keys: Vec<Eterm> = map_pairs(map, atoms)
        .expect("map_pairs")
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    let expected: Vec<Eterm> = (0..n as i64).map(make_small).collect();
    assert_eq!(keys, expected, "the key order must be preserved");
}

// ---------------------------------------------------------------------------
// Case 1: deeply nested terms
// ---------------------------------------------------------------------------

/// A 100 000-cell cons list and a 50 000-deep tuple chain survive every collection, and the
/// copier's traversal (and this test's walkers) must be **iterative**: a recursive copier
/// would blow the real stack long before this depth.
#[test]
fn deeply_nested_terms_are_copied_iteratively_and_intact() {
    let live = DEEP_CELLS * 2 + DEEP_CHAIN * 2;

    // ---- primitive path: a from-space filled *exactly* to its capacity ----------------
    let mut from = space(live);
    let list = build_deep_list(from.heap_mut(), DEEP_CELLS);
    let chain = build_deep_tuple_chain(from.heap_mut(), DEEP_CHAIN);
    assert_eq!(
        from.used_words(),
        live,
        "the deep graph is exactly the space"
    );
    assert_eq!(from.used_words(), from.capacity_words());

    let mut to = space(live);
    let mut roots = [list, chain];
    let stats = collect(&from, &mut to, &mut roots).expect("collect");
    assert_eq!(stats.words_copied, live);
    assert_eq!(to.used_words(), live);
    assert!(to.contains_term(roots[0]) && to.contains_term(roots[1]));

    let values = walk_list_smalls(roots[0]);
    assert_eq!(values.len(), DEEP_CELLS);
    assert!(
        values.iter().enumerate().all(|(i, &v)| v == i as i64),
        "the copied deep list must hold `0..{DEEP_CELLS}` in order"
    );
    let (depth, leaf) = walk_tuple_chain(roots[1]);
    assert_eq!(depth, DEEP_CHAIN, "every chain link survived");
    assert_eq!(leaf, -1, "the innermost element is unchanged");

    // ---- both strategies -------------------------------------------------------------
    for kind in BOTH {
        // The mark-region space rounds each one-line object to a whole 16-word line, so the
        // 150 000 live objects need a much larger word budget there.
        let policy = policy_for(kind, 400_000, 400_000, 2_500_000);
        let mut c = collector(kind, &policy);

        let heap = c.young_mut().heap_mut();
        let list = build_deep_list(heap, DEEP_CELLS);
        let chain = build_deep_tuple_chain(heap, DEEP_CHAIN);
        assert_eq!(
            c.young().used_words(),
            live,
            "{kind:?}: the graph is the nursery"
        );

        let mut roots = vec![list, chain];
        c.collect_minor(&mut roots).expect("minor");
        assert_collected_no_leak(&c, &roots, "deep minor");

        let values = walk_list_smalls(roots[0]);
        assert_eq!(
            values.len(),
            DEEP_CELLS,
            "{kind:?}: the list survived the minor"
        );
        assert!(
            values.iter().enumerate().all(|(i, &v)| v == i as i64),
            "{kind:?}: the copied deep list must hold `0..{DEEP_CELLS}` in order"
        );
        let (depth, leaf) = walk_tuple_chain(roots[1]);
        assert_eq!(depth, DEEP_CHAIN, "{kind:?}: the chain survived the minor");
        assert_eq!(leaf, -1, "{kind:?}: the innermost element is unchanged");

        // A fullsweep must relocate the whole 150 000-object graph a second time.
        c.collect_major(&mut roots).expect("major");
        assert_collected_no_leak(&c, &roots, "deep major");

        let values = walk_list_smalls(roots[0]);
        assert_eq!(
            values.len(),
            DEEP_CELLS,
            "{kind:?}: the list survived the fullsweep"
        );
        assert!(values.iter().enumerate().all(|(i, &v)| v == i as i64));
        let (depth, leaf) = walk_tuple_chain(roots[1]);
        assert_eq!(
            depth, DEEP_CHAIN,
            "{kind:?}: the chain survived the fullsweep"
        );
        assert_eq!(leaf, -1);
    }
}

// ---------------------------------------------------------------------------
// Case 2: the exact heap boundary
// ---------------------------------------------------------------------------

/// Fills `heap` to *exactly* `slots` words with one `slots`-word tuple (`slots - 1`
/// elements plus the arity header), leaving `used_words() == capacity_words()`.
fn fill_exactly(heap: &mut Heap, slots: usize) -> Eterm {
    assert!(slots >= 1, "a tuple needs at least its header word");
    let elems = vec![make_small(0x5A); slots - 1];
    heap.tuple(&elems).expect("fill the space exactly")
}

/// A live graph whose **last live word equals the space capacity** is collected across a
/// minor and a major, on both strategies and on the raw copying path; a destination one
/// word short is a typed error with nothing copied.
#[test]
fn a_live_graph_ending_exactly_at_capacity_is_collected() {
    const CAP: usize = 4096;

    // ---- primitive path: `to` sized *exactly* to the live set -------------------------
    let mut from = space(CAP);
    let root = fill_exactly(from.heap_mut(), CAP);
    assert_eq!(from.used_words(), CAP);
    assert_eq!(
        from.used_words(),
        from.capacity_words(),
        "the from-space is exactly full"
    );

    let mut to = space(CAP);
    let mut roots = [root];
    let stats = collect(&from, &mut to, &mut roots).expect("collect");
    assert_eq!(stats.words_copied, CAP);
    assert_eq!(
        to.used_words(),
        CAP,
        "the to-space ends exactly at capacity"
    );
    assert!(to.contains_term(roots[0]));
    assert!(!from.contains_term(roots[0]));
    let elems = walk_tuple_smalls(roots[0]);
    assert_eq!(elems.len(), CAP - 1);
    assert!(elems.iter().all(|&v| v == 0x5A));

    // One word short: the sole (CAP-word) live object cannot be reserved, so the copier
    // reports the typed heap exhaustion and writes nothing — the from-space stays intact.
    let mut from_short = space(CAP);
    let root_short = fill_exactly(from_short.heap_mut(), CAP);
    let mut to_short = space(CAP - 1);
    let mut roots_short = [root_short];
    let err = collect(&from_short, &mut to_short, &mut roots_short).expect_err("must not fit");
    assert!(matches!(err, BeamError::Alloc(_)), "got {err:?}");
    assert_eq!(to_short.used_words(), 0, "nothing was copied");
    assert_eq!(
        roots_short[0].raw(),
        root_short.raw(),
        "the root is untouched"
    );

    // ---- both strategies -------------------------------------------------------------
    for kind in BOTH {
        let policy = policy_for(kind, CAP, 8 * CAP, 32 * CAP);
        let mut c = collector(kind, &policy);

        // A **minor** at the boundary: the nursery is exactly full.
        let big = fill_exactly(c.young_mut().heap_mut(), CAP);
        assert_eq!(
            c.young().used_words(),
            c.young().capacity_words(),
            "{kind:?}: the nursery is exactly full before the minor"
        );
        let mut roots = vec![big];
        c.collect_minor(&mut roots).expect("minor");
        assert_collected_no_leak(&c, &roots, "boundary minor");
        let elems = walk_tuple_smalls(roots[0]);
        assert_eq!(elems.len(), CAP - 1, "{kind:?}");
        assert!(elems.iter().all(|&v| v == 0x5A), "{kind:?}");

        // A **major** at the boundary: refill the (re-sized) nursery to exactly its
        // capacity, so the fullsweep again starts from a completely full space.
        let cap = c.young().capacity_words();
        let big = fill_exactly(c.young_mut().heap_mut(), cap);
        assert_eq!(c.young().used_words(), c.young().capacity_words());
        let mut roots = vec![roots[0], big];
        c.collect_major(&mut roots).expect("major");
        assert_collected_no_leak(&c, &roots, "boundary major");
        for root in &roots {
            let elems = walk_tuple_smalls(*root);
            assert!(elems.len() >= CAP - 1, "{kind:?}");
            assert!(elems.iter().all(|&v| v == 0x5A), "{kind:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// Case 3: an off-heap refc binary behind an on-heap pair
// ---------------------------------------------------------------------------

/// The off-heap payload of case 3.
const REFC_BYTES: &[u8] = b"an adversarial off-heap refc binary payload, 0123456789";

/// An off-heap refc binary reachable from an on-heap `[BinRef][ErlSubBits]` pair survives
/// both a minor and a major with its refcount intact, its off-heap pointer byte-identical
/// and its payload readable, then is released exactly once when its handle becomes garbage.
#[test]
fn an_off_heap_refc_binary_root_survives_and_is_freed_exactly_once() {
    for kind in BOTH {
        let alloc = test_allocator("adversarial-refc");
        let payload = RefcBinary::with_allocator(alloc.clone(), REFC_BYTES).expect("payload");
        assert_eq!(alloc.stats().live_blocks, 1);
        assert_eq!(payload.refcount(), 1);

        // The off-heap list owns a second reference, so the single decrement a sweep
        // performs is observable.
        let mut off_heap = OffHeap::new();
        let slot = off_heap.insert(payload.clone());
        assert_eq!(slot, 0);
        assert_eq!(payload.refcount(), 2);
        assert_eq!(off_heap.overhead_bytes(), REFC_BYTES.len());

        let policy = policy_for(kind, 4096, 8192, 32 * 1024);
        let mut c = collector(kind, &policy);
        let handle = write_refc(c.young_mut().heap_mut(), &payload);
        let mut roots = vec![handle];

        // Phase 1: the handle is live across a minor and a fullsweep.
        c.collect_minor(&mut roots).expect("minor");
        assert_collected_no_leak(&c, &roots, "refc minor");
        // SAFETY: the copied root is a live refc binary whose payload `payload` keeps alive.
        assert!(unsafe { is_refc_binary(roots[0]) });
        // SAFETY: as above — a live `ErlSubBits`.
        unsafe {
            assert_eq!(refc_size(roots[0]), REFC_BYTES.len() * 8);
            assert_eq!(refc_bytes(roots[0]), REFC_BYTES);
        }
        // The `orig` (the lower-addressed `BinRef`) was forwarded into the mature space and
        // its off-heap `val` pointer is byte-identical to the payload.
        // SAFETY: `roots[0]` is the live copied `ErlSubBits`, so word 4 is in-bounds.
        let orig = unsafe { *boxed_val(roots[0]).add(4) };
        assert!(in_mature(&c, orig), "{kind:?}: `orig` must be forwarded");
        // SAFETY: `orig` is a live `BinRef`, so words 1 and 2 are in-bounds.
        let (val, next) = unsafe { bin_ref_parts(orig) };
        assert_eq!(
            val as *mut u8,
            payload.data_ptr(),
            "{kind:?}: the off-heap pointer must be left verbatim"
        );
        assert_eq!(
            next as usize, 0,
            "{kind:?}: the (empty) chain link is left verbatim"
        );

        let live = live_payloads(&roots);
        assert_eq!(live, vec![payload.data_ptr()], "{kind:?}");
        assert_eq!(
            off_heap.sweep(|e| binary_is_live(e, &live)),
            0,
            "{kind:?}: a live handle keeps the entry"
        );
        assert_eq!(payload.refcount(), 2, "{kind:?}");

        c.collect_major(&mut roots).expect("major");
        assert_collected_no_leak(&c, &roots, "refc major");
        // SAFETY: as above — the fullswept root is still a live refc binary.
        unsafe {
            assert!(is_refc_binary(roots[0]));
            assert_eq!(refc_bytes(roots[0]), REFC_BYTES);
        }
        assert_eq!(
            off_heap.sweep(|e| binary_is_live(e, &live_payloads(&roots))),
            0,
            "{kind:?}: still reachable, so still kept"
        );
        assert_eq!(payload.refcount(), 2, "{kind:?}");

        // Phase 2: the handle is dropped from the roots; the fullsweep reclaims the pair and
        // the now-unreachable off-heap entry is released exactly once.
        let mut none: [Eterm; 0] = [];
        c.collect_major(&mut none).expect("major");
        assert_eq!(c.young().used_words(), 0, "{kind:?}: the nursery is empty");
        let live = live_payloads(&none);
        assert!(
            live.is_empty(),
            "{kind:?}: no reachable handle names the payload"
        );
        assert_eq!(
            off_heap.sweep(|e| binary_is_live(e, &live)),
            1,
            "{kind:?}: the dead handle releases exactly one entry"
        );
        assert!(off_heap.is_empty(), "{kind:?}");
        assert_eq!(
            payload.refcount(),
            1,
            "{kind:?}: exactly one reference released"
        );

        // The final reference frees the buffer exactly once (a leak would leave a live
        // block; a double free would trip the allocator's plausibility check).
        let before = alloc.stats();
        assert_eq!(before.live_blocks, 1, "{kind:?}");
        drop(payload);
        let after = alloc.stats();
        assert_eq!(after.live_blocks, 0, "{kind:?}: the buffer was freed");
        assert_eq!(
            after.free_calls,
            before.free_calls + 1,
            "{kind:?}: the buffer was freed exactly once"
        );
    }
}

// ---------------------------------------------------------------------------
// Case 4: funs with a free-variable environment
// ---------------------------------------------------------------------------

/// Writes a `Local` fun reserving exactly [`fun_words`] words.
fn write_local(heap: &mut Heap, arity: usize, uniq: u64, index: u32, env: &[Eterm]) -> Eterm {
    let words = fun_words(FunKind::Local, env.len());
    // SAFETY: `words` exclusively-owned, writable words are reserved; every one is written
    // by `write_fun_local`, and each `env` entry is a live term of this heap.
    unsafe {
        let hp = heap.alloc_words(words).expect("reserve local fun");
        write_fun_local(hp, arity, uniq, index, env).expect("write local fun")
    }
}

/// Writes an `External` fun reserving exactly its block.
fn write_external(heap: &mut Heap, arity: usize, module: Atom, function: Atom) -> Eterm {
    let words = fun_words(FunKind::External, 0);
    // SAFETY: the 4 words `write_fun_external` writes are exclusively owned and reserved.
    unsafe {
        let hp = heap.alloc_words(words).expect("reserve external fun");
        write_fun_external(hp, arity, module, function).expect("write external fun")
    }
}

/// Writes an `Imported` fun (the whole `NEW_FUN_EXT` descriptor) reserving exactly its block.
fn write_imported(
    heap: &mut Heap,
    arity: usize,
    module: Atom,
    uniq: &[u8; 16],
    index: u32,
    creator: FunPid,
    env: &[Eterm],
) -> Eterm {
    let words = fun_words(FunKind::Imported, env.len());
    // SAFETY: `words` exclusively-owned, writable words are reserved; every one is written
    // by `write_fun_imported`, and each `env` entry is a live term of this heap.
    unsafe {
        let hp = heap.alloc_words(words).expect("reserve imported fun");
        write_fun_imported(hp, arity, module, uniq, index, 0, 0, creator, env)
            .expect("write imported fun")
    }
}

/// All three fun kinds, each with **boxed** free variables, survive a minor and a major
/// with their kind, metadata and environment intact and every boxed free variable relocated.
#[test]
fn funs_with_boxed_free_variables_round_trip_across_collections() {
    const UNIQ: [u8; 16] = [
        0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF,
        0x00,
    ];

    for kind in BOTH {
        let mut table = AtomTable::new();
        table.seed(["mod", "call", "node"]);
        let module = table.intern("mod");
        let function = table.intern("call");
        let node = table.intern("node");
        let creator = FunPid {
            node,
            number: 4,
            serial: 5,
            creation: 6,
        };

        let policy = policy_for(kind, 4096, 8192, 32 * 1024);
        let mut c = collector(kind, &policy);

        let heap = c.young_mut().heap_mut();
        // Compound free variables that must themselves be relocated by the fun's scan.
        let free_tuple = heap
            .tuple(&[make_small(1), make_small(2)])
            .expect("free tuple");
        let free_cons = heap.cons(make_small(3), NIL).expect("free cons");
        let imported_env = heap
            .tuple(&[make_small(6), make_small(7), make_small(8)])
            .expect("imported env");
        // A `Local` fun: env = [free_tuple, free_cons, 5].
        let local = write_local(
            heap,
            3,
            0xDEAD_BEEF,
            11,
            &[free_tuple, free_cons, make_small(5)],
        );
        // An `External` fun: no environment.
        let external = write_external(heap, 2, module, function);
        // An `Imported` fun: env = [imported_env].
        let imported = write_imported(heap, 1, module, &UNIQ, 21, creator, &[imported_env]);

        let mut roots = vec![local, external, imported];
        c.collect_minor(&mut roots).expect("minor");
        assert_collected_no_leak(&c, &roots, "fun minor");
        assert_funs_intact(&c, &roots, kind, module, function, &UNIQ, creator, "minor");

        c.collect_major(&mut roots).expect("major");
        assert_collected_no_leak(&c, &roots, "fun major");
        assert_funs_intact(&c, &roots, kind, module, function, &UNIQ, creator, "major");
    }
}

/// Asserts the three copied funs still describe themselves, with every boxed free variable
/// relocated into the mature space.
#[allow(clippy::too_many_arguments)]
fn assert_funs_intact(
    c: &AnyCollector,
    roots: &[Eterm],
    kind: MatureKind,
    module: Atom,
    function: Atom,
    uniq: &[u8; 16],
    creator: FunPid,
    stage: &str,
) {
    let ctx = format!("{kind:?}/{stage}");
    let header = |t: Eterm| {
        // SAFETY: `t` is a live boxed fun whose owning space keeps it unmoved here.
        unsafe { *boxed_val(t) }
    };

    let local = roots[0];
    assert_eq!(fun_kind_of_header(header(local)), FunKind::Local, "{ctx}");
    assert_eq!(fun_arity(header(local)), 3, "{ctx}");
    assert_eq!(fun_num_free(header(local)), 3, "{ctx}");
    // SAFETY: `local` is the live, copied local fun, so its descriptor is readable.
    unsafe {
        assert_eq!(fun_uniq(local).expect("uniq"), 0xDEAD_BEEF, "{ctx}");
        assert_eq!(fun_index(local).expect("index"), 11, "{ctx}");
    }
    // SAFETY: `local` is the live, copied 6-word local fun.
    let env = unsafe { fun_env(local) }.expect("local env");
    assert_eq!(env.len(), 3, "{ctx}");
    assert!(
        in_mature(c, env[0]),
        "{ctx}: the boxed tuple must be relocated"
    );
    assert!(
        in_mature(c, env[1]),
        "{ctx}: the boxed cons must be relocated"
    );
    assert_eq!(
        env[2],
        make_small(5),
        "{ctx}: the immediate free variable is unchanged"
    );
    assert_eq!(
        walk_tuple_smalls(env[0]),
        vec![1, 2],
        "{ctx}: the tuple free variable kept its values"
    );
    assert_eq!(
        walk_list_smalls(env[1]),
        vec![3],
        "{ctx}: the cons free variable kept its value"
    );

    let external = roots[1];
    assert_eq!(
        fun_kind_of_header(header(external)),
        FunKind::External,
        "{ctx}"
    );
    assert_eq!(fun_arity(header(external)), 2, "{ctx}");
    assert_eq!(fun_num_free(header(external)), 0, "{ctx}");
    // SAFETY: `external` is the live, copied external fun.
    unsafe {
        assert_eq!(fun_module(external).expect("module"), module, "{ctx}");
        assert_eq!(fun_function(external).expect("function"), function, "{ctx}");
    }

    let imported = roots[2];
    assert_eq!(
        fun_kind_of_header(header(imported)),
        FunKind::Imported,
        "{ctx}"
    );
    assert_eq!(fun_arity(header(imported)), 1, "{ctx}");
    assert_eq!(fun_num_free(header(imported)), 1, "{ctx}");
    // SAFETY: `imported` is the live, copied imported fun.
    unsafe {
        assert_eq!(fun_module(imported).expect("module"), module, "{ctx}");
        assert_eq!(fun_index(imported).expect("index"), 21, "{ctx}");
        assert_eq!(
            fun_uniq_bytes(imported).expect("uniq bytes"),
            *uniq,
            "{ctx}: the 16-byte uniq is preserved"
        );
        assert_eq!(
            fun_creator_pid(imported).expect("creator"),
            creator,
            "{ctx}: the creator pid is preserved"
        );
    }
    // SAFETY: `imported` is the live, copied imported fun.
    let env = unsafe { fun_env(imported) }.expect("imported env");
    assert_eq!(env.len(), 1, "{ctx}");
    assert!(
        in_mature(c, env[0]),
        "{ctx}: the imported env must be relocated"
    );
    assert_eq!(walk_tuple_smalls(env[0]), vec![6, 7, 8], "{ctx}");
}

// ---------------------------------------------------------------------------
// Case 5: a large tuple plus both map interiors
// ---------------------------------------------------------------------------

/// A 10 000-element tuple and both map interiors (a flat map and a >32-key HAMT map) survive
/// a minor and a fullsweep intact.
#[test]
fn a_large_tuple_and_both_map_shapes_survive() {
    const BIG: usize = 10_000; // one 10 001-word tuple
    const FLAT_KEYS: usize = 12; // <= MAP_SMALL_MAP_LIMIT => a flat map
    const HAMT_KEYS: usize = 500; // > MAP_SMALL_MAP_LIMIT => a HAMT trie

    for kind in BOTH {
        let atoms = AtomTable::new();
        let policy = policy_for(kind, 200_000, 200_000, 1_000_000);
        let mut c = collector(kind, &policy);

        let heap = c.young_mut().heap_mut();
        let elems: Vec<Eterm> = (0..BIG as i64).map(make_small).collect();
        let big = heap.tuple(&elems).expect("big tuple");
        let flat = write_map_n(heap, &atoms, FLAT_KEYS);
        let hamt = write_map_n(heap, &atoms, HAMT_KEYS);

        let mut roots = vec![big, flat, hamt];
        c.collect_minor(&mut roots).expect("minor");
        assert_collected_no_leak(&c, &roots, "large minor");
        assert_large_roots(&roots, &atoms, BIG, FLAT_KEYS, HAMT_KEYS, kind, "minor");

        c.collect_major(&mut roots).expect("major");
        assert_collected_no_leak(&c, &roots, "large major");
        assert_large_roots(&roots, &atoms, BIG, FLAT_KEYS, HAMT_KEYS, kind, "major");
    }
}

/// Asserts the large tuple's elements and both maps round-trip after a collection.
fn assert_large_roots(
    roots: &[Eterm],
    atoms: &AtomTable,
    big: usize,
    flat_keys: usize,
    hamt_keys: usize,
    kind: MatureKind,
    stage: &str,
) {
    let ctx = format!("{kind:?}/{stage}");
    let elements = walk_tuple_smalls(roots[0]);
    assert_eq!(elements.len(), big, "{ctx}: the tuple arity");
    assert!(
        elements.iter().enumerate().all(|(i, &v)| v == i as i64),
        "{ctx}: every tuple element kept its value"
    );
    assert_map_pairs(roots[1], flat_keys, atoms);
    assert_map_pairs(roots[2], hamt_keys, atoms);
}

// ---------------------------------------------------------------------------
// Case 6: an off-heap MSO
// ---------------------------------------------------------------------------

/// The MSO's apparent (Erlang-visible) size in bytes.
const MSO_APPARENT: usize = 4096;

/// An MSO payload that counts how many times it has been dropped.
struct DropPayload {
    drops: Arc<AtomicUsize>,
    tag: u32,
    words: Vec<u32>,
}

impl DropPayload {
    fn new(drops: &Arc<AtomicUsize>) -> DropPayload {
        DropPayload {
            drops: Arc::clone(drops),
            tag: 0xABCD,
            words: vec![10, 20, 30, 40],
        }
    }
}

impl Drop for DropPayload {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// An off-heap MSO (referenced only through its [`OffHeap::insert`] slot index) is untouched
/// across a minor and a major — payload intact, refcount stable — and its destructor runs
/// **exactly once** when it is swept dead.
#[test]
fn an_off_heap_mso_is_untouched_and_its_destructor_runs_exactly_once() {
    for kind in BOTH {
        let drops = Arc::new(AtomicUsize::new(0));
        let handle = Mso::new(DropPayload::new(&drops), MSO_APPARENT);
        assert_eq!(handle.refcount(), 1);

        let mut off_heap = OffHeap::new();
        let slot = off_heap.insert(handle.clone());
        assert_eq!(slot, 0);
        assert_eq!(handle.refcount(), 2);
        // The list now owns the *only* reference, so the release the sweep performs is the
        // one that runs the destructor.
        drop(handle);
        assert_eq!(
            off_heap
                .get(slot)
                .expect("the entry is present")
                .as_mso()
                .expect("an MSO entry")
                .refcount(),
            1
        );
        assert_eq!(off_heap.overhead_bytes(), MSO_APPARENT);

        // The on-heap reference to an MSO is the slot index inside an ordinary term.
        let policy = policy_for(kind, 4096, 8192, 32 * 1024);
        let mut c = collector(kind, &policy);
        let holder = c
            .young_mut()
            .heap_mut()
            .tuple(&[make_small(slot as i64), make_small(7)])
            .expect("holder tuple");
        let mut roots = vec![holder];

        c.collect_minor(&mut roots).expect("minor");
        assert_collected_no_leak(&c, &roots, "mso minor");
        c.collect_major(&mut roots).expect("major");
        assert_collected_no_leak(&c, &roots, "mso major");

        // The slot still resolves, the payload is intact and the refcount never moved.
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "{kind:?}: nothing was dropped"
        );
        let entry = off_heap.get(slot).expect("the slot still resolves");
        let mso = entry.as_mso().expect("still an MSO entry");
        assert_eq!(mso.refcount(), 1, "{kind:?}: the refcount is stable");
        assert_eq!(mso.apparent_size(), MSO_APPARENT, "{kind:?}");
        let payload = mso.downcast_ref::<DropPayload>().expect("the payload type");
        assert_eq!(payload.tag, 0xABCD, "{kind:?}: the payload is intact");
        assert_eq!(payload.words, vec![10, 20, 30, 40], "{kind:?}");
        assert_eq!(off_heap.len(), 1, "{kind:?}");
        assert_eq!(off_heap.overhead_bytes(), MSO_APPARENT, "{kind:?}");
        // The holder kept the slot index (a small integer, never moved by a collection).
        let elements = walk_tuple_smalls(roots[0]);
        assert_eq!(elements, vec![slot as i64, 7], "{kind:?}");

        // Swept dead: exactly one release, and the destructor runs exactly once.
        assert_eq!(off_heap.sweep(|_| false), 1, "{kind:?}");
        assert!(off_heap.is_empty(), "{kind:?}");
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "{kind:?}: the destructor must run exactly once"
        );
        // A second sweep (and dropping the list) must not run it again.
        assert_eq!(off_heap.sweep(|_| false), 0, "{kind:?}");
        drop(off_heap);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "{kind:?}: the destructor must not run twice"
        );
    }
}
