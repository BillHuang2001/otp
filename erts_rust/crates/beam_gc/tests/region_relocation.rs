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

//! Acceptance evidence, **from outside the crate**, for region-generic relocating compaction:
//! that a host holding a *foreign* word region — not a process heap — can evacuate it with
//! [`beam_gc::Relocator`]/[`beam_gc::compact`], and that the copy rules those drivers rest on
//! are the real ones.
//!
//! This is a separate crate, so it can only touch the **public** API: `FromSet::from_ranges`,
//! the `EvacDest`/`Target` seam, `LinearDest`, `Relocator`, `compact`, `Relocation` and the two
//! read-only helpers `for_each_reachable_in`/`live_words_in`. Nothing here reads
//! `beam_gc`'s internals, so a change that keeps the in-module suite green while narrowing
//! (or silently re-shaping) what a *host* can do fails here.
//!
//! # The contract pinned
//!
//! Over a **synthetic multi-region grid** — two disjoint source regions, one
//! `Vec<Eterm>`-backed and one `beam_alloc::ReservedHeap`-backed, whose graphs reference
//! *each other* — the suite pins:
//!
//! * **(a) structure survives.** Every reachable object comes through with identical
//!   structure (cons car/cdr, tuple elements, map key→value pairs, a fun's arity and
//!   free-variable values), the roots are rewritten to the copies, and the addresses
//!   actually change.
//! * **(b) only the live graph moves.** `words_copied` is exactly the live volume and the
//!   destination holds not one payload word of a dead object (proved by unique sentinel
//!   immediates, and by the dead objects' own source words still being untouched — they were
//!   never even descended into).
//! * **(c) root slices compose.** One `forward_roots` per slice of roots scattered over two
//!   Rust structs both take effect, the counters are the union of the slices, and repeating a
//!   slice is a **no-op** (the destination classifies an already-rewritten slot `Foreign`).
//! * **(d) the two shape-divergent terms.** A refc binary's frozen `[BinRef(3)][ErlSubBits(5)]`
//!   pair copies as **two separate blocks** (the `ErlSubBits` as its own 5 words, its `orig`
//!   word forwarded to the separately copied `BinRef`), and `BinRef.val` — the off-heap
//!   payload pointer — is copied **verbatim**, with the payload and its refcount untouched. A
//!   fun's inlined descriptor words are copied **opaquely** (never descended as terms) while
//!   every free variable in its env slot is relocated.
//! * **(e) an out-of-range root is left byte-identical** and a subtree it alone reaches is
//!   neither copied nor descended into.
//! * **(f) the two read-only accounts agree with the copy**: `live_words_in` is *exactly*
//!   `words_copied`, and `for_each_reachable_in`'s visited count/words equal
//!   `objects_copied`/`words_copied` — with the one documented asymmetry that the walk reports
//!   **boxed objects only** (a cons cell carries no header word: it is traversed, not
//!   reported), so on a graph with cons cells the two accounts differ by exactly the cells,
//!   which the word totals reconcile.
//! * **(g) a one-word-short destination is a typed refusal, not a torn state.** For `compact`
//!   — the atomic single-slice form — the source words, the roots and the destination are
//!   byte-identical after the refusal.
//!
//! # What the copy rules were audited to be
//!
//! The assertions encode what `src/copy.rs`/`src/relocate.rs` actually do, cross-checked
//! against `beam_term::layout` (the workspace's single owner of the per-object layout):
//! `move_size` returns `ERL_SUB_BITS_SIZE` (5) for `HEADER_SUB_BITS` — never
//! `object_words`'s 8-word *graph* size — while the `orig` slot's `BinRef` is forwarded as its
//! own 3-word child; the Cheney scan sizes every other object by its header and forwards
//! exactly [`beam_term::layout::for_each_child_slot`]'s offsets, so a fun's inlined descriptor
//! (2/3/10 words by kind) is skipped and only env slots move, and a flat map is sized by its
//! `size` word (`size + 3`) with its keys-tuple slot and value slots as children; a cons cell
//! has no header word, so both of its words are references.
//!
//! `unsafe` below only steps pointers into word areas **this test allocates itself** (raw
//! `Vec<Eterm>` words and a `ReservedHeap` reservation standing in for a foreign arena), and
//! reads words of a graph it built. Every `unsafe fn` has a `# Safety` section and every
//! `unsafe` block a preceding `// SAFETY:` rationale.

use core::mem::size_of;

use beam_alloc::{RefcBinary, ReservedHeap};
use beam_common::{AllocKind, BeamError, Result as BeamResult};
use beam_gc::prelude::{
    compact, for_each_reachable_in, live_words_in, EvacDest, FromSet, GcStats, LinearDest,
    Relocator, Target,
};
use beam_term::prelude::{
    boxed_val, fun_arity, fun_block_words, fun_env, fun_index, fun_kind_of_header, fun_num_free,
    fun_uniq, fun_words, is_refc_binary, make_small, map_get, map_pairs, map_size, ptr_val,
    refc_bytes, write_refc_binary, AtomTable, Eterm, FunKind, ERL_REFC_BITS_SIZE, HEADER_BIN_REF,
    HEADER_SUB_BITS, NIL,
};

// The fixtures — the foreign word regions, the readers and the two-region grid — live in their
// own file to keep this driver under the crate's ~1000-line target. `#[path]` is required: this
// file IS the test-crate root, and rustc resolves a crate root's submodules in the root's OWN
// directory (`tests/`), not in a `tests/<stem>/` directory — so a bare `mod fixtures;` would look
// for `tests/fixtures.rs` (which Cargo would then also build as its own test target).
#[path = "region_relocation/fixtures.rs"]
mod fixtures;

use fixtures::{
    build_grid, copied_run, from_set, header, list_cells, tuple_elements, Region, DEAD_A, DEAD_B,
    DEAD_C, DEAD_WORDS, LIVE_WORDS, OBJECTS,
};// ---------------------------------------------------------------------------
// (a) structure survives
// ---------------------------------------------------------------------------

#[test]
fn cross_region_graph_relocates_with_its_structure_intact() {
    let grid = build_grid();
    let from = from_set(&[&grid.a, &grid.b]);
    assert_eq!(from.range_count(), 2, "two disjoint source regions");
    assert_eq!(
        from.words(),
        LIVE_WORDS + DEAD_WORDS,
        "the from-set covers both live prefixes"
    );

    let mut roots = grid.roots;
    let before: [usize; 3] = grid.roots.map(|root| ptr_val(root) as usize);
    assert!(
        before
            .iter()
            .all(|&addr| from.contains(addr as *const Eterm)),
        "every root starts inside the from-set"
    );

    let target = Region::vec(LIVE_WORDS);
    // SAFETY: `target` is a separately allocated `LIVE_WORDS`-word area, disjoint from both
    // source regions, exclusively owned by this test and never moved or reallocated for its
    // whole scope.
    let mut dest = unsafe { LinearDest::new(&from, target.base(), LIVE_WORDS) };
    let report = compact(&from, &mut roots, &mut dest).expect("the destination is exactly enough");

    assert_eq!(report.stats.words_copied, LIVE_WORDS);
    assert_eq!(report.stats.objects_copied, OBJECTS);
    assert_eq!(
        report.stats.words_reclaimed, DEAD_WORDS,
        "only the two unreachable tuples"
    );
    assert_eq!(report.stats.minor_collections, 0);
    assert_eq!(
        report.stats.major_collections, 0,
        "a relocation is not a collection of a process heap"
    );
    assert_eq!(report.dest_words_used, LIVE_WORDS);
    assert_eq!(dest.used_words(), LIVE_WORDS);

    // Every root was rewritten to a destination word, and the address really changed.
    let base = target.base() as usize;
    let span = base..base + LIVE_WORDS * size_of::<Eterm>();
    for (index, &root) in roots.iter().enumerate() {
        assert_ne!(ptr_val(root) as usize, before[index], "root {index} moved");
        assert!(!from.contains(ptr_val(root)), "root {index} left the from-set");
        assert!(
            span.contains(&(ptr_val(root) as usize)),
            "root {index} names a destination word"
        );
    }

    // The tuple `{List, Tup}` — both of its elements crossed the region boundary to get here.
    let [tuple, fun, map] = roots;
    let elements = tuple_elements(tuple);
    assert_eq!(elements.len(), 2);
    assert_eq!(
        list_cells(elements[0]),
        vec![make_small(10), make_small(20)],
        "the list that lived in region B, car by car"
    );
    assert_eq!(
        tuple_elements(elements[1]),
        vec![make_small(30), make_small(40)],
        "the tuple that lived in region B"
    );

    // The fun: kind, arity, free-variable count, descriptor metadata, relocated environment.
    let hdr = header(fun);
    assert_eq!(fun_kind_of_header(hdr), FunKind::Local);
    assert_eq!(fun_arity(hdr), 2);
    assert_eq!(fun_num_free(hdr), 2);
    assert_eq!(
        fun_block_words(fun).expect("a live local fun"),
        fun_words(FunKind::Local, 2),
        "the block is `1 + desc + num_free` words"
    );
    // SAFETY: `fun` is the live, copied local fun.
    let env = unsafe { fun_env(fun) }.expect("a local fun's environment");
    assert_eq!(env.len(), 2);
    assert_eq!(env[0], elements[1], "the shared subterm is one copy");
    assert_eq!(env[1], make_small(5), "an immediate free variable is unchanged");
    // SAFETY: `fun` is the live, copied local fun.
    unsafe {
        assert_eq!(fun_uniq(fun).expect("a local fun"), 0xDEAD_BEEF);
        assert_eq!(fun_index(fun).expect("a local fun"), 11);
    }

    // The map: its keys-tuple slot and its value slot were forwarded, so every key still
    // resolves — the property a mis-sized or mis-walked flat map loses first.
    let atoms = AtomTable::new();
    assert_eq!(map_size(map, &atoms).expect("a flat map"), 1);
    assert_eq!(
        map_get(map, make_small(1), &atoms).expect("a flat map"),
        Some(elements[1]),
        "the value is the relocated region-B tuple"
    );
    assert_eq!(
        map_pairs(map, &atoms).expect("a flat map"),
        vec![(make_small(1), elements[1])]
    );
}

// ---------------------------------------------------------------------------
// (b) only the live graph moves
// ---------------------------------------------------------------------------

#[test]
fn unreachable_objects_are_not_copied() {
    let grid = build_grid();
    let from = from_set(&[&grid.a, &grid.b]);
    assert_eq!(
        live_words_in(&from, &grid.roots),
        LIVE_WORDS,
        "the read-only count is the exact live volume"
    );

    let mut roots = grid.roots;
    let target = Region::vec(LIVE_WORDS);
    // SAFETY: as in the structure test — a disjoint, dedicated, unmoved destination region.
    let mut dest = unsafe { LinearDest::new(&from, target.base(), LIVE_WORDS) };
    let report = compact(&from, &mut roots, &mut dest).expect("exactly enough room");

    assert_eq!(
        report.stats.words_copied, LIVE_WORDS,
        "not one dead word was copied"
    );
    assert_eq!(
        report.stats.words_reclaimed,
        from.words() - LIVE_WORDS,
        "every other from-set word is reported reclaimed"
    );
    assert_eq!(report.stats.words_reclaimed, DEAD_WORDS);

    // No copy of either dead object reached the destination: each carries a sentinel
    // immediate that appears nowhere else in the graph.
    let copied = copied_run(&target, dest.used_words());
    assert_eq!(copied.len(), LIVE_WORDS);
    for sentinel in [DEAD_A, DEAD_B, DEAD_C] {
        assert!(
            !copied.contains(&make_small(sentinel)),
            "the dead object's sentinel {sentinel:#x} must not appear in the destination"
        );
    }

    // The dead objects were not even *descended into*: their source words are still the
    // immediates the fixture wrote (a copied object's first word would hold a forwarding
    // marker instead). Reading them is safe precisely because no live term named them.
    assert_eq!(grid.a.word(grid.a.used() - 2), make_small(DEAD_A));
    assert_eq!(grid.b.word(grid.b.used() - 2), make_small(DEAD_B));
    assert_eq!(grid.b.word(grid.b.used() - 1), make_small(DEAD_C));
}

// ---------------------------------------------------------------------------
// (c) root slices compose, and a repeat is a no-op
// ---------------------------------------------------------------------------

#[test]
fn per_slice_forwarding_accumulates_and_repeats_are_no_ops() {
    /// A host's register home.
    struct Registers {
        first: Eterm,
    }
    /// A host's saved-argument home.
    struct Stack {
        rest: [Eterm; 2],
    }

    let grid = build_grid();
    let from = from_set(&[&grid.a, &grid.b]);
    let need = live_words_in(&from, &grid.roots);
    assert_eq!(need, LIVE_WORDS, "the preflight is exact");

    let mut registers = Registers {
        first: grid.roots[0],
    };
    let mut stack = Stack {
        rest: [grid.roots[1], grid.roots[2]],
    };

    let target = Region::vec(need);
    // SAFETY: a disjoint, dedicated, unmoved destination region of exactly the live volume.
    let mut dest = unsafe { LinearDest::new(&from, target.base(), need) };
    let mut relocator = Relocator::new_checked(&from, &mut dest, need).expect("the preflight");

    // One call per scattered slice.
    relocator
        .forward_roots(std::slice::from_mut(&mut registers.first))
        .expect("slice 1");
    let after_first = relocator.words_copied();
    relocator
        .forward_roots(std::slice::from_mut(&mut stack.rest[0]))
        .expect("slice 2");
    let after_second = relocator.words_copied();
    relocator
        .forward_roots(std::slice::from_mut(&mut stack.rest[1]))
        .expect("slice 3");
    let after_third = relocator.words_copied();

    // Each slice contributed, and the sum is the union: the three root *objects* only (the
    // scan that reaches their children comes with `complete`).
    assert!(after_first > 0, "slice 1 copied its root");
    assert!(after_second > after_first, "slice 2 copied its root");
    assert!(after_third > after_second, "slice 3 copied its root");
    assert_eq!(
        after_third,
        3 + fun_words(FunKind::Local, 2) + 4,
        "the 3-word tuple, the 5-word fun block and the 4-word map head"
    );
    assert_eq!(relocator.objects_copied(), 3, "one object per root, once");
    assert_eq!(relocator.dest_words_used(), after_third);

    let relocated = [registers.first, stack.rest[0], stack.rest[1]];

    // A repeat over the same slices, in a different grouping, copies nothing and rewrites
    // nothing: an already-rewritten slot names a destination word, which the destination
    // classifies `Foreign`.
    relocator
        .forward_roots(std::slice::from_mut(&mut registers.first))
        .expect("repeat 1");
    relocator.forward_roots(&mut stack.rest).expect("repeat 2");
    assert_eq!(relocator.words_copied(), after_third, "no repeat copied a word");
    assert_eq!(relocator.objects_copied(), 3, "nor an object");
    assert_eq!(
        [registers.first, stack.rest[0], stack.rest[1]],
        relocated,
        "nor rewrote a root"
    );

    let report = relocator.complete().expect("the scan");
    assert_eq!(
        report.stats.words_copied, need,
        "the whole graph, exactly once, as if one call had covered the union"
    );
    assert_eq!(report.stats.objects_copied, OBJECTS);
    assert_eq!(report.dest_words_used, need);
    assert_eq!(
        [registers.first, stack.rest[0], stack.rest[1]],
        relocated,
        "completing the scan rewrites no root"
    );
}

// ---------------------------------------------------------------------------
// (d) the two shape-divergent terms
// ---------------------------------------------------------------------------

#[test]
fn refc_binary_pair_copies_as_two_blocks_and_keeps_its_payload() {
    const PAYLOAD: &[u8] = b"a refc binary payload held off the process heap";

    let payload = RefcBinary::new(PAYLOAD).expect("the allocator satisfies the payload");
    let refcount_before = payload.refcount();
    let payload_ptr = payload.data_ptr();
    let size_bits = PAYLOAD.len() * 8;

    let mut source = Region::vec(ERL_REFC_BITS_SIZE);
    // SAFETY: exactly `ERL_REFC_BITS_SIZE` (8) words are reserved at the region's bump
    // pointer — the `write_refc_binary` contract — and `payload_ptr` addresses the live,
    // word-aligned off-heap payload, which `payload` keeps alive for the whole test; null is
    // the (empty) off-heap chain link.
    let root = unsafe {
        let hp = source.alloc(ERL_REFC_BITS_SIZE);
        write_refc_binary(
            hp,
            payload_ptr as *mut Eterm,
            core::ptr::null_mut(),
            payload_ptr,
            0,
            size_bits,
        )
    };
    assert_eq!(source.used(), ERL_REFC_BITS_SIZE);
    let source_root = root;

    let from = from_set(&[&source]);
    let mut roots = [root];
    let target = Region::vec(ERL_REFC_BITS_SIZE);
    // SAFETY: a disjoint, dedicated 8-word destination region.
    let mut dest = unsafe { LinearDest::new(&from, target.base(), ERL_REFC_BITS_SIZE) };
    let report = compact(&from, &mut roots, &mut dest).expect("the pair is exactly 8 words");

    // (ii) `move_size` sizes an `ErlSubBits` as its own 5-word block and the `BinRef` is
    // forwarded from its `orig` slot: **two** allocations, 5 + 3 = 8 words, 2 objects.
    assert_eq!(report.stats.words_copied, ERL_REFC_BITS_SIZE);
    assert_eq!(report.stats.objects_copied, 2, "the SubBits + the BinRef");
    assert_eq!(report.stats.words_reclaimed, 0, "the whole range was live");
    assert_eq!(report.dest_words_used, ERL_REFC_BITS_SIZE);

    // The `ErlSubBits` is the first copy, so it heads the destination run...
    let copied = roots[0];
    assert_ne!(ptr_val(copied), ptr_val(source_root), "the root moved");
    assert_eq!(
        ptr_val(copied) as usize,
        target.base() as usize,
        "the SubBits block heads the run"
    );
    // SAFETY: `copied` is the live, copied `ErlSubBits`, so its header word is in bounds.
    assert_eq!(unsafe { *boxed_val(copied) }, HEADER_SUB_BITS);

    // ...and its `orig` word (word 4) was forwarded to the **separately** copied `BinRef`,
    // which lands immediately after the 5-word block: that adjacency is the two-allocation
    // protocol made observable.
    // SAFETY: `copied` is a live, 5-word `ErlSubBits`, so word 4 is in bounds.
    let orig = unsafe { *boxed_val(copied).add(4) };
    assert_eq!(
        ptr_val(orig) as usize,
        target.base() as usize + ERL_SUB_BITS_WORDS * size_of::<Eterm>(),
        "the BinRef is its own allocation right after the SubBits block"
    );
    // SAFETY: `orig` is the live, copied `BinRef`, so words 0..=2 are in bounds.
    let (bin_ref_header, val, next) =
        unsafe { (*boxed_val(orig), *boxed_val(orig).add(1), *boxed_val(orig).add(2)) };
    assert_eq!(bin_ref_header, HEADER_BIN_REF, "`orig` names the copied BinRef");

    // (iv) `BinRef.val` — the off-heap payload pointer — is copied **verbatim**: the payload
    // never moves, and the refcount is not touched (a relocation is not a refcount event).
    assert_eq!(val.raw() as *mut u8, payload_ptr, "`val` is the payload pointer");
    assert_eq!(next.raw(), 0, "the empty off-heap chain link is verbatim");
    assert_eq!(payload.refcount(), refcount_before, "the refcount is unchanged");
    assert_eq!(payload.data_ptr(), payload_ptr, "the payload never moved");

    // The term still describes the same bytes.
    // SAFETY: `copied` is the live, copied refc binary and `payload` outlives the read.
    unsafe {
        assert!(is_refc_binary(copied));
        assert_eq!(refc_bytes(copied), PAYLOAD, "the off-heap bytes are untouched");
    }
}

/// `ERL_SUB_BITS_SIZE` — the words an `ErlSubBits` block occupies in the *copy* graph, as
/// opposed to the 8 words `beam_term::layout::object_words` reports for the whole pair.
const ERL_SUB_BITS_WORDS: usize = 5;

#[test]
fn fun_descriptor_is_opaque_and_its_free_variables_relocate() {
    let mut env_region = Region::vec(16);
    let env_tuple = env_region.tuple(&[make_small(1), make_small(2)]); // 3 words
    let mut fun_region = Region::reserved(16);
    // arity 3, two free variables — one compound (in the *other* region), one immediate.
    let fun = fun_region.local_fun(3, 0x0123_4567_89AB_CDEF, 7, &[env_tuple, make_small(5)]);
    assert_eq!(fun_region.used(), fun_words(FunKind::Local, 2));

    let from = from_set(&[&env_region, &fun_region]);
    let source_words = fun_region.snapshot();
    let before = ptr_val(fun) as usize;
    assert_eq!(
        before,
        fun_region.base() as usize,
        "the fun heads its own region"
    );

    let need = live_words_in(&from, &[fun]);
    assert_eq!(
        need,
        fun_words(FunKind::Local, 2) + 3,
        "the fun block plus the compound free variable"
    );

    let target = Region::vec(need);
    let mut roots = [fun];
    // SAFETY: a disjoint, dedicated destination region of exactly the live volume.
    let mut dest = unsafe { LinearDest::new(&from, target.base(), need) };
    let report = compact(&from, &mut roots, &mut dest).expect("exactly enough room");

    let copied = roots[0];
    assert_ne!(ptr_val(copied) as usize, before, "the fun moved");
    assert_eq!(
        ptr_val(copied) as usize,
        target.base() as usize,
        "the fun block heads the destination run"
    );
    assert_eq!(report.stats.words_copied, need);
    assert_eq!(report.stats.objects_copied, 2, "the fun block + the env tuple");

    // (iii) The inlined descriptor is copied **verbatim and opaquely**: words 0..=2 (header,
    // `uniq`, `index`) are the source's, and only the env slots were forwarded. A scan that
    // advanced by `thing_arityval + 1` would have read a descriptor word as a term here.
    for (offset, &source_word) in source_words[..=2].iter().enumerate() {
        assert_eq!(
            target.word(offset),
            source_word,
            "descriptor word {offset} is copied verbatim"
        );
    }    let hdr = header(copied);
    assert_eq!(fun_kind_of_header(hdr), FunKind::Local);
    assert_eq!(fun_arity(hdr), 3, "the header's arity field");
    assert_eq!(fun_num_free(hdr), 2);
    assert_eq!(
        fun_block_words(copied).expect("a live local fun"),
        fun_words(FunKind::Local, 2)
    );
    // SAFETY: `copied` is the live, copied local fun.
    unsafe {
        assert_eq!(fun_uniq(copied).expect("a local fun"), 0x0123_4567_89AB_CDEF);
        assert_eq!(fun_index(copied).expect("a local fun"), 7);
    }

    // Every free variable in the env slot was relocated: the compound one into the words
    // immediately after the fun block (the scan reached it through the env slot, never
    // through the descriptor), the immediate one unchanged.
    // SAFETY: `copied` is the live, copied local fun.
    let env = unsafe { fun_env(copied) }.expect("a local fun's environment");
    assert_eq!(env.len(), 2);
    assert_eq!(
        ptr_val(env[0]) as usize,
        target.base() as usize + fun_words(FunKind::Local, 2) * size_of::<Eterm>(),
        "the env tuple is its own copy, right after the fun block"
    );
    assert_ne!(
        ptr_val(env[0]) as usize, before,
        "the free variable's address changed with the fun's"
    );
    assert_eq!(
        tuple_elements(env[0]),
        vec![make_small(1), make_small(2)],
        "and the relocated tuple still holds its elements"
    );
    assert_eq!(env[1], make_small(5), "an immediate free variable is unchanged");
}

// ---------------------------------------------------------------------------
// (e) a root outside every range
// ---------------------------------------------------------------------------

#[test]
fn a_root_outside_the_ranges_is_left_byte_identical() {
    const OUTSIDE_A: i64 = 0x51DE_0001;
    const OUTSIDE_B: i64 = 0x51DE_0002;

    let mut live = Region::vec(16);
    let tuple_live = live.tuple(&[make_small(1), make_small(2)]);
    let mut outside = Region::vec(16);
    let tuple_outside = outside.tuple(&[make_small(OUTSIDE_A), make_small(OUTSIDE_B)]);
    let outside_before = outside.snapshot();

    // `from` names ONLY `live`. `outside` is a second live region the host keeps, so every
    // reference into it is classified `Foreign` — the shape a literal area, another
    // process's heap or an mmap'd arena has. (A bare off-heap payload pointer is not a term
    // word at all, so the out-of-set region is what stands in for that case here.)
    let from = from_set(&[&live]);
    let mut roots = [tuple_outside, make_small(1234), NIL, tuple_live];
    let outside_root_before = roots[0];

    assert_eq!(
        live_words_in(&from, &roots),
        3,
        "only the in-range tuple is copyable"
    );
    // The walk neither counts nor descends into the out-of-range tuple.
    let mut visited = Vec::new();
    for_each_reachable_in(&from, &roots, |object| visited.push(object.address()))
        .expect("the walk's visitor never fails");
    assert_eq!(visited, vec![ptr_val(tuple_live) as usize]);

    let target = Region::vec(3);
    // SAFETY: a disjoint, dedicated 3-word destination region.
    let mut dest = unsafe { LinearDest::new(&from, target.base(), 3) };
    let report = compact(&from, &mut roots, &mut dest).expect("exactly enough room");
    assert_eq!(report.stats.words_copied, 3);
    assert_eq!(report.stats.objects_copied, 1);

    // The three foreign roots are byte-identical, the region they name is untouched, and the
    // subtree they alone reach was neither copied nor descended into.
    assert_eq!(roots[0], outside_root_before, "an out-of-range root is not rewritten");
    assert_eq!(roots[1], make_small(1234), "an immediate is not a reference");
    assert_eq!(roots[2], NIL);
    assert_eq!(
        outside.snapshot(),
        outside_before,
        "the foreign region is byte-identical"
    );
    let copied = copied_run(&target, dest.used_words());
    for sentinel in [OUTSIDE_A, OUTSIDE_B] {
        assert!(
            !copied.contains(&make_small(sentinel)),
            "the out-of-range object's payload {sentinel:#x} was copied"
        );
    }

    // The one in-range root did move — so the refusal above is precision, not inaction.
    assert!(!from.contains(ptr_val(roots[3])));
    assert_eq!(ptr_val(roots[3]) as usize, target.base() as usize);
    assert_eq!(
        tuple_elements(roots[3]),
        vec![make_small(1), make_small(2)]
    );
}

// ---------------------------------------------------------------------------
// (f) the read-only accounts agree with the copy
// ---------------------------------------------------------------------------

#[test]
fn walk_and_live_counters_agree_with_the_relocation() {
    /// A graph with **no cons cells**, so the reachability walk (which reports boxed objects
    /// only) and the copier (which also counts cons cells) must agree exactly.
    fn build_boxed_only() -> (Region, [Eterm; 2]) {
        let mut region = Region::vec(32);
        let tuple = region.tuple(&[make_small(7), make_small(8)]);
        let map = region.flat_map(&[(make_small(3), tuple)]);
        (region, [tuple, map])
    }

    // Part 1 — boxed objects only: count and words must be identical.
    let (region, roots) = build_boxed_only();
    let from = from_set(&[&region]);
    let (mut objects, mut words) = (0usize, 0usize);
    for_each_reachable_in(&from, &roots, |object| {
        objects += 1;
        words += object.len();
    })
    .expect("the walk's visitor never fails");
    assert_eq!(objects, 3, "the tuple, the map head and the map's keys tuple");
    assert_eq!(words, live_words_in(&from, &roots), "the exact live volume");
    assert_eq!(words, LIVE_BOXED_ONLY_WORDS);

    // A *separate* fixture: the walk is read-only, but the copy is destructive, so the two
    // accounts must never share a partially-forwarded state.
    let (region2, mut roots2) = build_boxed_only();
    let from2 = from_set(&[&region2]);
    let target = Region::vec(words);
    // SAFETY: a disjoint, dedicated destination region of exactly the visited volume.
    let mut dest = unsafe { LinearDest::new(&from2, target.base(), words) };
    let report = compact(&from2, &mut roots2, &mut dest).expect("exactly enough room");
    assert_eq!(
        report.stats.objects_copied, objects,
        "the copy made every visited object"
    );
    assert_eq!(
        report.stats.words_copied, words,
        "and exactly the visited words"
    );
    assert_eq!(report.dest_words_used, words);

    // And the walk over the *result* reports nothing: the roots name no from-space object.
    let mut after = 0usize;
    for_each_reachable_in(&from2, &roots2, |_object| after += 1).expect("the walk");
    assert_eq!(after, 0, "the roots now name destination objects");

    // Part 2 — the cross-region grid, which has cons cells. `for_each_reachable_in` reports
    // **boxed objects only** (a cons cell carries no header word: it is traversed but not
    // reported), while `objects_copied` counts cons cells too. The two accounts therefore
    // differ by exactly the cells — which the word totals reconcile, since a cell is 2 words.
    let grid = build_grid();
    let from = from_set(&[&grid.a, &grid.b]);
    let (mut boxed, mut boxed_words) = (0usize, 0usize);
    for_each_reachable_in(&from, &grid.roots, |object| {
        boxed += 1;
        boxed_words += object.len();
    })
    .expect("the walk's visitor never fails");
    assert_eq!(
        live_words_in(&from, &grid.roots),
        LIVE_WORDS,
        "`live_words_in` counts cells too, so it is the exact copy volume"
    );

    let mut roots = grid.roots;
    let target = Region::vec(LIVE_WORDS);
    // SAFETY: a disjoint, dedicated destination region of exactly the live volume.
    let mut dest = unsafe { LinearDest::new(&from, target.base(), LIVE_WORDS) };
    let report = compact(&from, &mut roots, &mut dest).expect("exactly enough room");

    assert_eq!(report.stats.words_copied, LIVE_WORDS);
    let cons_cells = report.stats.objects_copied - boxed;
    assert_eq!(cons_cells, 2, "the fixture's list is two cells");
    assert_eq!(
        boxed_words + 2 * cons_cells,
        report.stats.words_copied,
        "the walk's words plus the cells are the copy's words"
    );
}

/// The boxed-only fixture's live volume: the tuple (arity 2 → 3), the map head (size 1 →
/// `1 + 3` = 4) and the map's keys tuple (`arityval(1)` + 1 key = 2).
const LIVE_BOXED_ONLY_WORDS: usize = 3 + 4 + 2;

// ---------------------------------------------------------------------------
// (g) an undersized destination is a typed refusal, not a torn state
// ---------------------------------------------------------------------------

#[test]
fn an_undersized_destination_is_refused_atomically() {
    let grid = build_grid();
    let from = from_set(&[&grid.a, &grid.b]);
    let need = live_words_in(&from, &grid.roots);
    let a_before = grid.a.snapshot();
    let b_before = grid.b.snapshot();
    let ranges_before = from.ranges().to_vec();
    let mut roots = grid.roots;

    let target = Region::vec(need - 1);
    let target_before = target.snapshot();
    // SAFETY: a disjoint, dedicated destination region one word short of the live volume.
    let mut dest = unsafe { LinearDest::new(&from, target.base(), need - 1) };
    match compact(&from, &mut roots, &mut dest) {
        Err(BeamError::Alloc(failure)) => {
            assert_eq!(failure.kind, AllocKind::Heap, "the typed heap exhaustion");
            assert_eq!(
                failure.requested,
                need * size_of::<Eterm>(),
                "the refused request is the exact live volume in bytes"
            );
        }
        other => panic!(
            "expected the typed heap exhaustion, got {:?}",
            other.map(|report| report.stats)
        ),
    }

    // The refusal happened before the first word moved: no forwarding marker, no rewritten
    // root, no reserved destination word.
    assert_eq!(roots, grid.roots, "no root was rewritten");
    assert_eq!(grid.a.snapshot(), a_before, "region A is byte-identical");
    assert_eq!(grid.b.snapshot(), b_before, "region B is byte-identical");
    assert_eq!(
        from.ranges(),
        ranges_before.as_slice(),
        "the from-set is unchanged"
    );
    assert_eq!(dest.used_words(), 0, "the destination reserved nothing");
    assert_eq!(target.snapshot(), target_before, "and wrote nothing");

    // Exactly one word more is admitted, so `need - 1` really is the boundary.
    let target = Region::vec(need);
    // SAFETY: a disjoint, dedicated destination region of exactly the live volume.
    let mut dest = unsafe { LinearDest::new(&from, target.base(), need) };
    let report = compact(&from, &mut roots, &mut dest).expect("exactly enough room");
    assert_eq!(report.stats.words_copied, need);
    assert_eq!(dest.available_words(), 0, "the preflight was exact");
}

// ---------------------------------------------------------------------------
// `dest_words_used`
// ---------------------------------------------------------------------------

#[test]
fn dest_words_used_is_the_linear_copy_volume() {
    // An empty from-set (a region the host reserved but never filled) copies nothing.
    let empty = Region::vec(8);
    let from = from_set(&[&empty]);
    assert!(from.is_empty(), "a zero-used region adds no range");
    let target = Region::vec(0);
    let mut roots = [make_small(1), NIL];
    // SAFETY: a zero-capacity destination over an allocation this test owns; nothing can be
    // reserved from it and the safety contract is vacuous for the words.
    let mut dest = unsafe { LinearDest::new(&from, target.base(), 0) };
    let report = compact(&from, &mut roots, &mut dest).expect("nothing to copy needs no room");
    assert_eq!(
        report.stats,
        GcStats::default(),
        "no word, no object, no collection counter"
    );
    assert_eq!(report.dest_words_used, 0);
    assert_eq!(roots, [make_small(1), NIL], "the roots are untouched");

    // The grid, with spare capacity: the figure is the copies, never the capacity.
    let grid = build_grid();
    let from = from_set(&[&grid.a, &grid.b]);
    let need = live_words_in(&from, &grid.roots);
    let mut roots = grid.roots;
    let target = Region::vec(need + 4);
    // SAFETY: a disjoint, dedicated destination region with four words to spare.
    let mut dest = unsafe { LinearDest::new(&from, target.base(), need + 4) };
    let report = compact(&from, &mut roots, &mut dest).expect("room to spare");
    assert_eq!(report.stats.words_copied, need);
    assert_eq!(
        report.dest_words_used, need,
        "pinned: `dest_words_used == words_copied` for a linear destination"
    );
    assert_eq!(dest.used_words(), need);
    assert_eq!(dest.available_words(), 4, "the spare words were never touched");
}

// ---------------------------------------------------------------------------
// The seam is implementable outside the crate
// ---------------------------------------------------------------------------

/// An **out-of-crate** destination: `beam_alloc::ReservedHeap`-backed and implemented here, so
/// the `EvacDest` seam is provably implementable by a host rather than only by `beam_gc`'s own
/// [`LinearDest`]. Every method delegates to an inner `LinearDest` over the reservation — which
/// is where the marker protocol lives, and exactly what the trait's contract asks a
/// destination *not* to reimplement.
struct ReservedDest<'a> {
    /// The reservation the copies land in: alive for as long as the destination, never
    /// relocated, so the inner destination's `base` stays valid.
    heap: ReservedHeap,
    inner: LinearDest<'a>,
}

impl<'a> ReservedDest<'a> {
    /// A destination over a fresh `words`-word `ReservedHeap` reservation.
    fn new(from: &'a FromSet, words: usize) -> ReservedDest<'a> {
        let heap = ReservedHeap::new(words, words)
            .expect("the default allocator satisfies the reservation");
        // SAFETY: the reservation is a `words`-word, exclusively-owned block that outlives the
        // returned destination (moved into it, never relocated during its lifetime), and it is
        // disjoint from `from` (a separate allocation).
        let inner = unsafe { LinearDest::new(from, heap.base(), words) };
        ReservedDest { heap, inner }
    }

    /// The reservation the copies landed in.
    fn heap(&self) -> &ReservedHeap {
        &self.heap
    }

    /// The words handed out — the relocated graph's used length.
    fn used_words(&self) -> usize {
        self.inner.used_words()
    }
}

impl EvacDest for ReservedDest<'_> {
    /// # Safety
    ///
    /// As [`EvacDest::classify_boxed`]: `ptr` may address anything, and the inner destination
    /// dereferences it only inside a from-range.
    unsafe fn classify_boxed(&self, ptr: *const Eterm) -> Target {
        // SAFETY: forwarded to the inner destination, whose containment test is what makes
        // dereferencing `ptr` safe.
        unsafe { self.inner.classify_boxed(ptr) }
    }

    /// # Safety
    ///
    /// As [`EvacDest::classify_cons`].
    unsafe fn classify_cons(&self, ptr: *const Eterm) -> Target {
        // SAFETY: as `classify_boxed`.
        unsafe { self.inner.classify_cons(ptr) }
    }

    fn can_reserve(&self, words: usize) -> bool {
        self.inner.can_reserve(words)
    }

    /// # Safety
    ///
    /// `words` must satisfy [`EvacDest::can_reserve`], which is the inner destination's own
    /// pre-check.
    unsafe fn reserve(&mut self, words: usize) -> BeamResult<*mut Eterm> {
        // SAFETY: the caller guarantees `words` fits, so the inner reservation stays in bounds.
        unsafe { self.inner.reserve(words) }
    }

    fn copied_top(&self) -> Option<*mut Eterm> {
        self.inner.copied_top()
    }
}

#[test]
fn an_out_of_crate_evac_dest_drives_the_same_relocation() {
    // The reference run: the crate's own linear destination.
    let grid = build_grid();
    let from = from_set(&[&grid.a, &grid.b]);
    let need = live_words_in(&from, &grid.roots);
    let reference_target = Region::vec(need);
    let mut reference_roots = grid.roots;
    // SAFETY: a disjoint, dedicated destination region of exactly the live volume.
    let mut linear = unsafe { LinearDest::new(&from, reference_target.base(), need) };
    let reference = compact(&from, &mut reference_roots, &mut linear).expect("exactly enough");

    // The same relocation, into the out-of-crate destination.
    let grid = build_grid();
    let from = from_set(&[&grid.a, &grid.b]);
    let mut roots = grid.roots;
    let mut dest = ReservedDest::new(&from, need);
    let report = compact(&from, &mut roots, &mut dest).expect("exactly enough room");

    assert_eq!(
        report, reference,
        "the seam drives the same copy: same words, objects and reclaim"
    );
    assert_eq!(dest.used_words(), need);
    assert_eq!(report.dest_words_used, need);

    // The graph is structurally the same, read out of the reservation it landed in.
    let base = dest.heap().base() as usize;
    let span = base..base + need * size_of::<Eterm>();
    let [tuple, fun, map] = roots;
    for (index, &root) in roots.iter().enumerate() {
        assert!(
            span.contains(&(ptr_val(root) as usize)),
            "root {index} names a word of the out-of-crate reservation"
        );
    }
    let elements = tuple_elements(tuple);
    assert_eq!(list_cells(elements[0]), vec![make_small(10), make_small(20)]);
    assert_eq!(tuple_elements(elements[1]), vec![make_small(30), make_small(40)]);
    let atoms = AtomTable::new();
    assert_eq!(
        map_get(map, make_small(1), &atoms).expect("a flat map"),
        Some(elements[1])
    );
    assert_eq!(fun_kind_of_header(header(fun)), FunKind::Local);
    // SAFETY: `fun` is the live, copied local fun.
    assert_eq!(
        unsafe { fun_env(fun) }.expect("a local fun's environment")[0],
        elements[1],
        "the shared subterm is still one copy"
    );
}

/// A destination that publishes **no contiguous copied run** — the shape a line-granular
/// destination (the mark-region space) has, and the case a host must drive itself.
struct UnwalkableDest;

impl EvacDest for UnwalkableDest {
    unsafe fn classify_boxed(&self, _ptr: *const Eterm) -> Target {
        Target::Foreign
    }

    unsafe fn classify_cons(&self, _ptr: *const Eterm) -> Target {
        Target::Foreign
    }

    fn can_reserve(&self, _words: usize) -> bool {
        true
    }

    unsafe fn reserve(&mut self, _words: usize) -> BeamResult<*mut Eterm> {
        // Unreachable: `Relocator::new`/`compact` refuse this destination before any copy, so
        // no reservation is ever attempted.
        Err(BeamError::BadArg)
    }

    // `copied_top` keeps its default (`None`), which is what declares the destination
    // unwalkable.
}

#[test]
fn an_unwalkable_destination_is_refused_before_mutating() {
    let grid = build_grid();
    let from = from_set(&[&grid.a, &grid.b]);
    let a_before = grid.a.snapshot();
    let b_before = grid.b.snapshot();
    let mut roots = grid.roots;

    let mut dest = UnwalkableDest;
    assert!(
        matches!(Relocator::new(&from, &mut dest), Err(BeamError::BadArg)),
        "a destination with no contiguous copied run is `BadArg`"
    );
    assert!(
        matches!(
            Relocator::new_checked(&from, &mut dest, 0),
            Err(BeamError::BadArg)
        ),
        "the preflighted form reports the shape error too"
    );
    assert!(
        matches!(compact(&from, &mut roots, &mut dest), Err(BeamError::BadArg)),
        "and so does the one-shot form"
    );

    // Every refusal happened before the first word moved.
    assert_eq!(roots, grid.roots, "no root rewritten");
    assert_eq!(grid.a.snapshot(), a_before, "region A is byte-identical");
    assert_eq!(grid.b.snapshot(), b_before, "region B is byte-identical");
}
