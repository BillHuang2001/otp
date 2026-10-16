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

//! **Mature-generation and off-heap-heavy steady state** for the exact-reachable-live counter
//! ([`crate::live`](beam_gc) — `live` / `live_full`, the read-only counter a fullsweep sizes
//! its fresh destination from).
//!
//! The counter's contract is a two-sided exactness: its **word** count must equal exactly the
//! `GcStats::words_copied` a real collection of the same from-set and roots reports, and its
//! **line** count exactly the per-object `lines_for` need a line-granular destination charges
//! (an under-count would size a destination a copy cannot fit — the half-forward the fill
//! barrier exists to prevent). `src/live.rs` pins the counter **directly** against a real
//! collection; this file pins the same contract from **outside** the crate — through the public
//! collector API only — and adds the two regimes that counter exists for:
//!
//! * **(a) the mature generation.** A `GenImmixCollector` fullsweep sizes its region from
//!   `live_full`. Corpora of one-line (3-word) objects, multi-line (17-word) objects, a shared
//!   subterm, a cycle and a `>32`-key HAMT map with funs are driven through **both** mature
//!   paths — the **in-place reclaim** (the policy keeps the region) and the **relocating
//!   resize** (the policy shrinks or grows it) — and the words the copy wrote and the lines it
//!   occupies are asserted against the corpus's independently known on-heap footprint.
//! * **(b) off-heap-heavy processes.** A root set dominated by the frozen 8-word
//!   `[BinRef][ErlSubBits]` handle of a refcounted binary, plus an `Mso` registered **only** in
//!   the per-process `OffHeap` list (no on-heap reference at all). The counter and the
//!   collection must agree on the **on-heap** word count — `6 * 64 KiB` of off-heap payload is
//!   not a heap word — and the moved handle must be relocated while the off-heap payload stays
//!   byte-identical.
//! * **(c) a bounded steady state.** A long (240-round) loop at the **default cadence**
//!   (`fullsweep_after = 65535`) over a bounded off-heap-heavy live set plus fresh garbage each
//!   round, through **both** strategies pinned explicitly. The nursery capacity, the mature
//!   capacity and the total heap must be **constant** across the second half of the loop — the
//!   property a garbage-inflated mature sizing would break by creeping the capacity up one growth
//!   step per fullsweep — the nursery must be empty after every collection, and the counter must
//!   still match the real collection's copy. The mark-region **resident line count** is *not*
//!   constant there — the region is reclaimed in place by a fullsweep and then promoted into
//!   between fullsweeps — so it is asserted to **cycle within a pinned, bounded band** (never
//!   creeping), not to sit at a single value.
//!
//! ## Determinism
//!
//! Every corpus is deterministic and every **assertion** is on a deterministic count (words,
//! lines, capacities). No timing is measured or asserted. The only `unsafe` below touches
//! words this test reserved and wrote itself (the refc-binary pair, the cycle's back-edge) and
//! carries a `// SAFETY:` rationale.

use beam_alloc::RefcBinary;
use beam_gc::prelude::{
    lines_for, AnyCollector, CollectionKind, GcConfig, GenImmixCollector, HeapPolicy, MatureKind,
    Mso, OffHeap, LINE_WORDS,
};
use beam_term::prelude::{
    boxed_val, fun_words, is_refc_binary, make_small, map_words_capacity, ptr_val, refc_bytes,
    refc_size, size_object, write_fun_external, write_fun_local, write_map, write_refc_binary,
    Atom, AtomTable, Eterm, FunKind, ERL_REFC_BITS_SIZE, NIL,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A mark-region collector with the requested young/mature capacities (both rounded up to whole
/// Immix blocks) and the default policy everywhere else.
fn immix(young: usize, mature: usize) -> GenImmixCollector {
    let policy = HeapPolicy {
        initial_young_words: young,
        initial_mature_words: mature,
        ..HeapPolicy::default()
    };
    GenImmixCollector::from_policy(policy).expect("a mark-region collector")
}

/// Writes the frozen 8-word `[BinRef][ErlSubBits]` off-heap handle for `payload` at the top of
/// `c`'s nursery and returns its root (the `ErlSubBits`).
fn refc_handle(c: &mut AnyCollector, payload: &RefcBinary) -> Eterm {
    // SAFETY: exactly `ERL_REFC_BITS_SIZE` (8) exclusively-owned words are reserved;
    // `payload.data_ptr()` is a live, word-aligned pointer to `payload.len() * 8` bits of
    // off-heap storage the caller keeps alive for the whole test, and `null` is the (empty)
    // off-heap chain link.
    unsafe {
        let hp = c
            .young_mut()
            .heap_mut()
            .alloc_words(ERL_REFC_BITS_SIZE)
            .expect("8 words for the pair");
        write_refc_binary(
            hp,
            payload.data_ptr() as *mut Eterm,
            core::ptr::null_mut(),
            payload.data_ptr(),
            0,
            payload.len() * 8,
        )
    }
}

/// Whether `term` is resident in the strategy's mature generation.
fn in_mature(c: &AnyCollector, term: Eterm) -> bool {
    match c {
        AnyCollector::Copying(x) => x.mature().contains_term(term),
        AnyCollector::Immix(x) => x.mature().contains_term(term),
    }
}

/// The mature generation's capacity in words.
fn mature_capacity(c: &AnyCollector) -> usize {
    match c {
        AnyCollector::Copying(x) => x.mature().capacity_words(),
        AnyCollector::Immix(x) => x.mature().capacity_words(),
    }
}

// ---------------------------------------------------------------------------
// Part (a): the mature generation
// ---------------------------------------------------------------------------

/// One corpus of the maturity tests: the builder plus its independently known on-heap footprint.
#[derive(Clone, Copy)]
struct Corpus {
    name: &'static str,
    /// The nursery capacity — large enough for the whole corpus.
    young: usize,
    /// The mature capacity that makes the policy **keep** the region, when a whole-block
    /// capacity in the keep band also covers the corpus's line need. `None` when no such
    /// capacity exists (a leaf-dense HAMT map needs more lines than any kept region can hold,
    /// so its fullsweep always relocates).
    mature_in_place: Option<usize>,
    /// The mature capacity that makes the policy **resize** the region (the relocating path).
    mature_relocate: usize,
    /// The independently known on-heap word count.
    words: usize,
    /// The independently known per-object line need, `None` when only the in-crate counter can
    /// know it (the HAMT map's node/leaf count).
    lines: Option<usize>,
    /// Whether `sum size_object(root)` equals `words` — false for the shared/cyclic corpus,
    /// which `size_object` sizes once per *occurrence*.
    size_object_agrees: bool,
}

/// Twelve 3-word (one-line) tuples and four 17-word (two-line) tuples.
fn build_tuples(c: &mut GenImmixCollector) -> Vec<Eterm> {
    let mut roots = Vec::new();
    for i in 0..12i64 {
        roots.push(
            c.young_mut()
                .heap_mut()
                .tuple(&[make_small(i), make_small(2 * i)])
                .expect("a 3-word tuple"),
        );
    }
    for i in 0..4i64 {
        roots.push(
            c.young_mut()
                .heap_mut()
                .tuple(&[make_small(i); 16])
                .expect("a 17-word tuple"),
        );
    }
    roots
}

/// A shared 3-word subterm referenced from two containers, plus a two-cell cons cycle.
fn build_shared_and_cycle(c: &mut GenImmixCollector) -> Vec<Eterm> {
    let shared = c
        .young_mut()
        .heap_mut()
        .tuple(&[make_small(1), make_small(2)])
        .expect("shared");
    let left = c
        .young_mut()
        .heap_mut()
        .tuple(&[shared, make_small(3)])
        .expect("left");
    let right = c
        .young_mut()
        .heap_mut()
        .tuple(&[shared, make_small(4)])
        .expect("right");
    let a = c
        .young_mut()
        .heap_mut()
        .cons(make_small(5), NIL)
        .expect("cell a");
    let b = c
        .young_mut()
        .heap_mut()
        .cons(make_small(6), a)
        .expect("cell b");
    // Close the cycle with a raw write (no allocator API makes a cycle).
    // SAFETY: `a` names a live 2-word cons cell this test allocated in the nursery, and word 1
    // is its cdr slot; the write stays inside the cell.
    unsafe {
        *ptr_val(a).add(1) = b;
    }
    vec![left, right, a, shared, b]
}

/// A `500`-key HAMT map (head-array/head-bitmap/inner/collision nodes), a local closure with two
/// immediates and an exported fun.
fn build_hamt_and_funs(c: &mut GenImmixCollector) -> Vec<Eterm> {
    let atoms = AtomTable::new();
    let n = 500usize;
    let input: Vec<(Eterm, Eterm)> = (0..n as i64)
        .map(|i| (make_small(i), make_small(3 * i)))
        .collect();
    // SAFETY: `map_words_capacity(n)` exclusively-owned, writable words are reserved and
    // `write_map` writes a contiguous, well-formed prefix of them.
    let map = unsafe {
        let hp = c
            .young_mut()
            .heap_mut()
            .alloc_words(map_words_capacity(n))
            .expect("the map reservation");
        write_map(hp, &input, &atoms).expect("a 500-key HAMT map")
    };
    // SAFETY: `fun_words(Local, 2) == 5` exclusively-owned words; the two free variables are
    // immediates, so the whole block is written before the term escapes.
    let fun = unsafe {
        let hp = c
            .young_mut()
            .heap_mut()
            .alloc_words(fun_words(FunKind::Local, 2))
            .expect("the fun reservation");
        write_fun_local(hp, 2, 0x1234_5678_9ABC, 7, &[make_small(1), make_small(2)])
            .expect("a local fun")
    };
    // SAFETY: `fun_words(External, 0) == 4` exclusively-owned words, all written by the call.
    let ext = unsafe {
        let hp = c
            .young_mut()
            .heap_mut()
            .alloc_words(fun_words(FunKind::External, 0))
            .expect("the export reservation");
        write_fun_external(hp, 3, Atom::from_raw(1), Atom::from_raw(2)).expect("an exported fun")
    };
    vec![map, fun, ext]
}

/// The three corpora, in a fixed order.
fn corpora() -> Vec<(Corpus, fn(&mut GenImmixCollector) -> Vec<Eterm>)> {
    vec![
        (
            Corpus {
                name: "one-line + multi-line tuples",
                young: 4096,
                // 512 words = 2 whole blocks = 32 lines: the policy keeps it (a 104-word live
                // set needs 20 lines) and the promotion fits without escalating.
                mature_in_place: Some(512),
                mature_relocate: 4096,
                words: 12 * 3 + 4 * 17,
                lines: Some(12 + 4 * 2),
                size_object_agrees: true,
            },
            build_tuples,
        ),
        (
            Corpus {
                name: "shared subterm + cycle",
                young: 4096,
                // 256 words = 1 whole block = 16 lines; the 13-word corpus needs 5.
                mature_in_place: Some(256),
                mature_relocate: 4096,
                words: 3 + 3 + 3 + 2 + 2,
                lines: Some(5),
                // `size_object` sizes a shared subterm once per occurrence and stops at a cycle
                // back-edge, so it is *not* the live footprint here.
                size_object_agrees: false,
            },
            build_shared_and_cycle,
        ),
        (
            Corpus {
                name: "HAMT map + funs",
                young: 1 << 16,
                // A leaf-dense HAMT map needs far more lines than any kept region of its word
                // size, so its fullsweep always relocates — no in-place capacity exists. One
                // Immix block is far below the corpus's live words (and its line need), so the
                // policy's grow branch fires and the fullsweep takes the relocating
                // (resizing) path, which is the path this test covers for this corpus.
                mature_in_place: None,
                mature_relocate: 1 << 8,
                words: 0, // filled in by `check_hamt_corpus`
                lines: None,
                size_object_agrees: true,
            },
            build_hamt_and_funs,
        ),
    ]
}

/// Drives one corpus through a `GenImmixCollector` fullsweep and asserts the counter-governed
/// copy is exactly the corpus's known on-heap footprint.
fn run_one(
    corpus: &Corpus,
    build: fn(&mut GenImmixCollector) -> Vec<Eterm>,
    mature: usize,
    relocate: bool,
    words: usize,
) {
    let mut c = immix(corpus.young, mature);
    let mut roots = build(&mut c);
    let before = c.mature().capacity_words();
    let stats = c.collect_major(&mut roots).expect("a fullsweep");
    let ctx = format!("{} (mature = {mature}, relocate = {relocate})", corpus.name);

    if relocate {
        assert_ne!(
            c.mature().capacity_words(),
            before,
            "{ctx}: the policy must take the relocating (resizing) path"
        );
    } else {
        assert_eq!(
            c.mature().capacity_words(),
            before,
            "{ctx}: the policy must take the in-place path"
        );
    }
    assert_eq!(
        stats.words_copied, words,
        "{ctx}: the counter-sized copy must write exactly the known live words"
    );
    if let Some(lines) = corpus.lines {
        assert_eq!(
            c.mature().used_lines(),
            lines,
            "{ctx}: the copy must occupy exactly the known line need"
        );
    } else {
        // The exact line need of a HAMT trie is only knowable to the counter itself (it is
        // pinned there, in `src/live.rs`); here we can still assert the destination covered the
        // word-derived lower bound and the copy fit exactly.
        assert!(
            c.mature().used_lines() >= lines_for(words),
            "{ctx}: the destination must cover at least `lines_for(words)` lines"
        );
    }
    assert_eq!(
        c.young().used_words(),
        0,
        "{ctx}: the nursery is empty after the fullsweep"
    );
    if corpus.size_object_agrees {
        assert_eq!(
            roots.iter().map(|&t| size_object(t)).sum::<usize>(),
            words,
            "{ctx}: the independent frozen-layout footprint agrees"
        );
    }
}

/// **(a)** The **in-place** mature reclaim path: the policy keeps the region and the promotion
/// is sized from `live_full`.
#[test]
fn an_in_place_immix_major_copies_exactly_the_counted_live_set() {
    for (corpus, build) in corpora() {
        let Some(mature) = corpus.mature_in_place else {
            continue;
        };
        let words = if corpus.words == 0 {
            // The HAMT corpus: measure the footprint independently, with `size_object`.
            let mut probe = immix(corpus.young, corpus.mature_relocate);
            let roots = build(&mut probe);
            roots.iter().map(|&t| size_object(t)).sum::<usize>()
        } else {
            corpus.words
        };
        run_one(&corpus, build, mature, false, words);
    }
}

/// **(a)** The **relocating** mature resize path: the policy resizes the region, so the whole
/// live set is copied into a fresh one — sized, and checked, from the counter.
#[test]
fn a_relocating_immix_major_copies_exactly_the_counted_live_set() {
    for (corpus, build) in corpora() {
        let words = if corpus.words == 0 {
            let mut probe = immix(corpus.young, corpus.mature_relocate);
            let roots = build(&mut probe);
            roots.iter().map(|&t| size_object(t)).sum::<usize>()
        } else {
            corpus.words
        };
        run_one(&corpus, build, corpus.mature_relocate, true, words);
    }
}

/// **(a)** One-line objects are the case that makes the counter's `lines` field load-bearing: a
/// region sized from the *word* count alone is short of the lines line-granular allocation
/// actually charges, so a fullsweep that ignored `lines` would run out of room mid-copy.
#[test]
fn one_line_objects_need_more_lines_than_the_word_count_suggests() {
    let mut c = immix(4096, 512);
    let mut roots = build_tuples(&mut c);
    let words: usize = roots.iter().map(|&t| size_object(t)).sum();
    assert_eq!(words, 104);

    let stats = c.collect_major(&mut roots).expect("a fullsweep");
    assert_eq!(stats.words_copied, words);

    // The copy really occupies `used_lines()` lines — 20 for 16 one-and-two-line objects — while
    // the word count alone would only account for `lines_for(104) == 7`.
    assert_eq!(c.mature().used_lines(), 20);
    assert_eq!(lines_for(words), 7);
    assert!(
        lines_for(words) < c.mature().used_lines(),
        "a region sized from the words alone ({words} -> {} lines) could not hold the {} lines \
         the copy needs",
        lines_for(words),
        c.mature().used_lines()
    );
}

// ---------------------------------------------------------------------------
// Part (b): an off-heap-heavy root set
// ---------------------------------------------------------------------------

/// The number of refc-binary handles in the off-heap-heavy corpus.
const PAIRS: usize = 6;
/// The on-heap words of that corpus: six 8-word pairs plus the 3-word holder tuple that carries
/// the off-heap `Mso`'s slot index.
const ON_HEAP_WORDS: usize = PAIRS * ERL_REFC_BITS_SIZE + 3;
/// The lines those words occupy: two per pair plus one for the holder tuple.
const ON_HEAP_LINES: usize = PAIRS * 2 + 1;

/// The largest resident line count the mark-region steady-state loop may reach in its second
/// half — a pinned ceiling, not the (weaker) "never more than the region's capacity" invariant.
/// The region is reclaimed in place at each fullsweep and then promoted into until it has no
/// free line left, so the count cycles between [`ON_HEAP_LINES`] and this bound rather than
/// sitting at a constant; pinning the bound keeps "no leak" a real assertion. The constant
/// region holds 272 lines and the cycle stops at 260 because the next round's 13-line promotion
/// no longer fits the 12 free lines that leaves, which is what escalates the following round to
/// a fullsweep.
const IMMIX_RESIDENT_LINES_CEILING: usize = 260;
/// The off-heap payload size per binary.
const PAYLOAD_BYTES: usize = 64 * 1024;

/// **(b)** Off-heap-heavy: the counter and a real collection agree on the **on-heap** word count,
/// the moved handle is relocated, and neither the off-heap binary payload nor the `Mso` (which
/// has no on-heap reference at all) contributes a heap word or moves.
#[test]
fn an_offheap_heavy_graph_counts_only_its_on_heap_words() {
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let payloads: Vec<RefcBinary> = (0..PAIRS)
            .map(|i| RefcBinary::new(&vec![i as u8; PAYLOAD_BYTES]).expect("a payload"))
            .collect();
        // The off-heap list owns a reference to each payload plus one `Mso` that no heap term
        // references — its only on-heap trace is the slot index inside the holder tuple.
        let mut off_heap = OffHeap::new();
        for payload in &payloads {
            off_heap.insert(payload.clone());
        }
        let mso = Mso::new(vec![1u8, 2, 3, 4], 4096);
        let slot = off_heap.insert(mso.clone());
        drop(mso); // the list now owns the only reference
        assert_eq!(
            slot, PAIRS,
            "{kind:?}: the MSO took the slot after the binaries"
        );
        assert_eq!(off_heap.len(), PAIRS + 1, "{kind:?}");
        assert_eq!(
            off_heap.overhead_bytes(),
            PAIRS * PAYLOAD_BYTES + 4096,
            "{kind:?}: the off-heap bookkeeping is entirely off the heap"
        );

        let policy = HeapPolicy {
            initial_young_words: 4096,
            initial_mature_words: 512, // 2 whole blocks = 32 lines, kept by the policy
            ..HeapPolicy::default()
        };
        let config = match kind {
            MatureKind::Copying => GcConfig::copying(policy),
            MatureKind::Immix => GcConfig::immix(policy),
        };
        let mut c = AnyCollector::from_config(&config).expect("a collector");
        assert_eq!(c.kind(), kind, "the explicit pin must select the strategy");

        let mut roots: Vec<Eterm> = Vec::new();
        for payload in &payloads {
            roots.push(refc_handle(&mut c, payload));
        }
        roots.push(
            c.young_mut()
                .heap_mut()
                .tuple(&[make_small(slot as i64), make_small(7)])
                .expect("the holder tuple"),
        );
        let handle_before = boxed_val_of(roots[0]);

        let stats = c.collect_major(&mut roots).expect("a fullsweep");
        assert_eq!(
            stats.words_copied, ON_HEAP_WORDS,
            "{kind:?}: only the on-heap handles are copied — the off-heap payloads are not"
        );
        assert_eq!(c.young().used_words(), 0, "{kind:?}: the nursery is empty");

        // The destination holds exactly the on-heap footprint, never the 6 x 64 KiB payloads.
        match &c {
            AnyCollector::Copying(x) => assert_eq!(
                x.mature().used_words(),
                ON_HEAP_WORDS,
                "{kind:?}: the copying mature space holds the on-heap words"
            ),
            AnyCollector::Immix(x) => assert_eq!(
                x.mature().used_lines(),
                ON_HEAP_LINES,
                "{kind:?}: the mark-region destination holds the on-heap lines"
            ),
        }

        // Every handle was relocated out of the nursery, and its off-heap reference is byte-for-
        // byte the one written in the nursery.
        for (i, root) in roots.iter().take(PAIRS).enumerate() {
            let ctx = format!("{kind:?} handle {i}");
            assert!(
                !c.young().contains_term(*root),
                "{ctx}: the handle must leave the nursery"
            );
            assert!(
                in_mature(&c, *root),
                "{ctx}: the handle must be relocated into mature"
            );
            // SAFETY: `*root` is the live, promoted `ErlSubBits` of pair `i`, whose off-heap
            // payload `payloads[i]` is alive for the whole test.
            unsafe {
                assert!(is_refc_binary(*root), "{ctx}");
                assert_eq!(
                    refc_size(*root),
                    PAYLOAD_BYTES * 8,
                    "{ctx}: the window is preserved"
                );
                assert_eq!(
                    refc_bytes(*root),
                    payloads[i].as_bytes(),
                    "{ctx}: the off-heap bytes are untouched"
                );
                let orig = *boxed_val(*root).add(4);
                assert!(
                    in_mature(&c, orig),
                    "{ctx}: the BinRef must be forwarded as well"
                );
                let val = *boxed_val(orig).add(1);
                let next = *boxed_val(orig).add(2);
                assert_eq!(
                    val.raw() as *mut u8,
                    payloads[i].data_ptr(),
                    "{ctx}: the off-heap pointer is byte-identical"
                );
                assert_eq!(next.raw(), 0, "{ctx}: the chain link is byte-identical");
            }
        }
        assert_ne!(
            boxed_val_of(roots[0]),
            handle_before,
            "{kind:?}: the handle really moved to a new address"
        );

        // The MSO — reached by no heap term at all — is untouched: same refcount, payload intact.
        let entry = off_heap.get(slot).expect("the MSO slot resolves");
        let m = entry.as_mso().expect("an MSO entry");
        assert_eq!(
            m.refcount(),
            1,
            "{kind:?}: the MSO refcount is stable across the move"
        );
        assert_eq!(m.apparent_size(), 4096, "{kind:?}");
        assert_eq!(
            m.downcast_ref::<Vec<u8>>().expect("the payload").as_slice(),
            &[1u8, 2, 3, 4][..],
            "{kind:?}: the MSO payload is intact"
        );
    }
}

/// The address of a boxed term's first word.
fn boxed_val_of(term: Eterm) -> usize {
    // `boxed_val` is a `const fn` pointer accessor, not an `unsafe` one; the caller passes a live
    // boxed term it just built or a collection just wrote.
    boxed_val(term) as usize
}

// ---------------------------------------------------------------------------
// Part (c): a bounded steady state
// ---------------------------------------------------------------------------

/// Rounds of the steady-state loop (the second half is measured).
const ROUNDS: usize = 240;
/// Garbage tuples built and dropped every round.
const GARBAGE_TUPLES: usize = 300;
/// Elements per garbage tuple (`arity + 1` = 9 words each).
const GARBAGE_ARITY: usize = 8;
/// The nursery capacity — pinned through `min_heap_words` so the bounded re-size rule cannot
/// shrink it below the round's allocation.
const YOUNG_WORDS: usize = 4096;
/// The mature capacity: one whole Immix block, small enough that a promotion escalates to a
/// fullsweep every round (so the loop exercises the counter-driven mature sizing each round).
const MATURE_WORDS: usize = 256;

/// One round's sizing snapshot (deterministic counts only).
struct Snapshot {
    young_capacity: usize,
    mature_capacity: usize,
    immix_lines: Option<usize>,
    total_heap_size: usize,
}

fn snapshot(c: &AnyCollector) -> Snapshot {
    let mature_capacity = mature_capacity(c);
    Snapshot {
        young_capacity: c.young().capacity_words(),
        mature_capacity,
        immix_lines: match c {
            AnyCollector::Copying(_) => None,
            AnyCollector::Immix(x) => Some(x.mature().used_lines()),
        },
        total_heap_size: c.young().capacity_words() + mature_capacity,
    }
}

/// **(c)** Runs the bounded off-heap-heavy loop for one strategy.
fn run_steady_state(label: &str, kind: MatureKind) {
    let payloads: Vec<RefcBinary> = (0..PAIRS)
        .map(|i| RefcBinary::new(&vec![i as u8; 4096]).expect("a payload"))
        .collect();
    let mut off_heap = OffHeap::new();
    for payload in &payloads {
        off_heap.insert(payload.clone());
    }
    let mso = Mso::new(vec![0u8; 64], 64);
    let slot = off_heap.insert(mso.clone());
    drop(mso);

    let policy = HeapPolicy {
        // The DEFAULT cadence: a fullsweep only every 65535 minors, exactly what a plain
        // process runs. The fullsweeps this loop does take are escalation-driven (the mature
        // generation fills), which is the sizing path the counter governs.
        fullsweep_after: 65535,
        initial_young_words: YOUNG_WORDS,
        min_heap_words: YOUNG_WORDS,
        initial_mature_words: MATURE_WORDS,
        ..HeapPolicy::default()
    };
    let config = match kind {
        MatureKind::Copying => GcConfig::copying(policy),
        MatureKind::Immix => GcConfig::immix(policy),
    };
    let mut c = AnyCollector::from_config(&config).expect("a collector");
    assert_eq!(c.kind(), kind, "the explicit pin must select the strategy");

    let mut samples: Vec<Snapshot> = Vec::with_capacity(ROUNDS);
    let mut majors = 0usize;
    for round in 0..ROUNDS {
        // A fresh bounded off-heap-heavy live set: six refc handles plus the holder tuple.
        let mut roots: Vec<Eterm> = Vec::new();
        for payload in &payloads {
            roots.push(refc_handle(&mut c, payload));
        }
        roots.push(
            c.young_mut()
                .heap_mut()
                .tuple(&[make_small(slot as i64), make_small(7)])
                .expect("the holder tuple"),
        );
        // Fresh garbage the collection must reclaim (the previous round's live set is garbage
        // too — the root vector below replaces it).
        let mut sink: Vec<Eterm> = Vec::with_capacity(GARBAGE_TUPLES);
        for i in 0..GARBAGE_TUPLES {
            sink.push(
                c.young_mut()
                    .heap_mut()
                    .tuple(&[make_small(-(i as i64)); GARBAGE_ARITY])
                    .expect("a garbage tuple"),
            );
        }
        drop(sink);

        let collection = c.collect(&mut roots).expect("collect");
        if collection.kind == CollectionKind::Major {
            majors += 1;
        }
        // The counter still matches a real collection: the mature generation held only garbage
        // (every round rebuilds the live set), so the whole live set is the bounded on-heap
        // graph the counter reports.
        assert_eq!(
            collection.stats.words_copied, ON_HEAP_WORDS,
            "{label} round {round}: the collection must copy exactly the on-heap live set"
        );
        assert!(
            collection.stats.words_reclaimed > 0,
            "{label} round {round}: the round's garbage must be reclaimed"
        );
        assert_eq!(
            c.young().used_words(),
            0,
            "{label} round {round}: the nursery is empty after every collection"
        );

        // Every root survived into the mature generation, with its off-heap payload untouched.
        for (i, root) in roots.iter().take(PAIRS).enumerate() {
            assert!(
                !c.young().contains_term(*root),
                "{label} round {round}: handle {i} stayed in the nursery"
            );
            assert!(
                in_mature(&c, *root),
                "{label} round {round}: handle {i} was not relocated into mature"
            );
        }
        // SAFETY: `roots[0]` is a live, promoted refc handle; `payloads[0]` is alive.
        unsafe {
            assert_eq!(
                refc_bytes(roots[0]),
                payloads[0].as_bytes(),
                "{label} round {round}: the off-heap payload is untouched"
            );
        }
        assert_eq!(
            off_heap
                .get(slot)
                .expect("the MSO slot resolves")
                .as_mso()
                .expect("an MSO entry")
                .refcount(),
            1,
            "{label} round {round}: the MSO refcount is stable"
        );

        let snap = snapshot(&c);
        if let Some(lines) = snap.immix_lines {
            // The region is line-granular and, once it has promotion room, is **promoted into**
            // between fullsweeps (each round promotes the round's live set into it) and
            // reclaimed *in place* by a fullsweep. The resident line count is therefore not
            // pinned to the live set: it is at least the live set's lines (nothing the roots
            // reach is ever lost) and at most the region's own constant line capacity (no term
            // is ever allocated outside the region).
            assert!(
                (ON_HEAP_LINES..=mature_capacity(&c) / LINE_WORDS).contains(&lines),
                "{label} round {round}: the region must hold at least the live set's \
                 {ON_HEAP_LINES} lines and never more than its own capacity, got {lines}"
            );
        }
        samples.push(snap);
    }

    // ---- the second half must be constant (no capacity creep) ----
    let half = &samples[ROUNDS / 2..];
    let young: Vec<usize> = half.iter().map(|s| s.young_capacity).collect();
    let mature: Vec<usize> = half.iter().map(|s| s.mature_capacity).collect();
    let total: Vec<usize> = half.iter().map(|s| s.total_heap_size).collect();
    let lines: Vec<usize> = half.iter().filter_map(|s| s.immix_lines).collect();

    let min_max = |v: &[usize]| {
        (
            *v.iter().min().expect("non-empty"),
            *v.iter().max().expect("non-empty"),
        )
    };
    let (y_lo, y_hi) = min_max(&young);
    let (m_lo, m_hi) = min_max(&mature);
    let (t_lo, t_hi) = min_max(&total);
    assert_eq!(
        y_lo, y_hi,
        "{label}: the nursery capacity must be constant, got {young:?}"
    );
    assert_eq!(
        m_lo, m_hi,
        "{label}: the mature capacity must be constant, got {mature:?}"
    );
    assert_eq!(
        t_lo, t_hi,
        "{label}: the total heap must be constant, got {total:?}"
    );
    assert_eq!(
        t_lo,
        y_lo + m_lo,
        "{label}: the total is the nursery plus the mature capacity"
    );
    if kind == MatureKind::Immix {
        assert_eq!(
            lines.len(),
            half.len(),
            "{label}: every round exposes the line count"
        );
        let (l_lo, l_hi) = min_max(&lines);
        // The region is reclaimed **in place** by a fullsweep and then promoted into until it
        // has no free line left, so — unlike the capacities above, which are all constant — the
        // resident line count **cycles** between the live set's line need and the region's line
        // capacity. The "no leak" meaning is that the cycle is **bounded** and keeps returning
        // to the floor: it never exceeds the (constant) region capacity, and a fullsweep really
        // does reclaim the region down to the live set rather than accumulate.
        assert_eq!(
            l_lo, ON_HEAP_LINES,
            "{label}: a fullsweep must reclaim the region down to the live set's line need, got \
             min {l_lo} ({lines:?})"
        );
        assert!(
            l_hi <= IMMIX_RESIDENT_LINES_CEILING,
            "{label}: the resident line count must be bounded by the pinned ceiling \
             {IMMIX_RESIDENT_LINES_CEILING}, got max {l_hi} ({lines:?})"
        );
        assert!(
            l_lo < l_hi,
            "{label}: the region must genuinely be promoted into between fullsweeps, got a flat \
             {l_lo} ({lines:?})"
        );
    }

    eprintln!(
        "[{label}] {ROUNDS} rounds at the default cadence over a {ON_HEAP_WORDS}-word \
         off-heap-heavy live set: nursery {y_lo} words, mature {m_lo} words, total {t_lo} words \
         — all constant across the second half{}{} ({majors} fullsweeps, the counter-sized path)",
        if kind == MatureKind::Immix {
            ", resident lines "
        } else {
            ""
        },
        match kind {
            MatureKind::Immix => format!("{}..{}", min_max(&lines).0, min_max(&lines).1),
            MatureKind::Copying => String::new(),
        }
    );
    // The counter governs the fresh mature space of a **fullsweep**; the loop must really run
    // that path (a mature generation that only ever grows by minor promotions would never size
    // anything from `live_full`).
    assert!(
        majors > 0,
        "{label}: the loop must exercise the counter-sized fullsweep path, got 0 fullsweeps"
    );
}

/// **(c)** The copying strategy's bounded steady state.
#[test]
fn the_offheap_heavy_heap_reaches_a_bounded_steady_state_copying() {
    run_steady_state("copying", MatureKind::Copying);
}

/// **(c)** The mark-region strategy's bounded steady state (nursery, mature capacity and total
/// heap constant; the resident line count cycling within a pinned, bounded band).
#[test]
fn the_offheap_heavy_heap_reaches_a_bounded_steady_state_immix() {
    run_steady_state("immix", MatureKind::Immix);
}
