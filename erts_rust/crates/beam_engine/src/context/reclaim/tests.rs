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

//! Unit tests for the arena's two reclaim tiers.
//!
//! These live here rather than in `tests/` because [`ContextHeap::with_geometry`] is
//! private (and `#[cfg(test)]`): the proofs below drive the arena with *exact* small
//! numbers (4- and 8-word reservations), which is what lets every word figure in a
//! [`HeapRewind`]/[`HeapReclaim`] report and in [`ContextHeapStats`] be asserted exactly
//! instead of bounded. `tests/heap_reclaim.rs` re-proves the same contracts through the
//! public [`MinimalContext`] doors at the production geometry.
//!
//! Being a descendant module, these tests can also **fabricate** marks (the fields are
//! private to `context`), which is how the forward/stale/foreign refusal arms are driven —
//! there is no public way to build an invalid [`HeapMark`], by design.

use super::*;
use beam_term::{
    arityval, is_nil, is_small, list_val, make_small, make_tuple, primary_tag,
    signed_value, tuple_val, Eterm, PrimaryTag, NIL,
};

/// One `Eterm`'s payload in bytes — what a bump-pointer step is measured in.
const WORD: usize = core::mem::size_of::<Eterm>();

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Write `words` as raw term words at `ptr`.
fn write_words(ptr: *mut Eterm, words: &[usize]) {
    for (i, word) in words.iter().enumerate() {
        // SAFETY: every caller passes a pointer `heap.alloc(n)` handed out with
        // `n >= words.len()`, so each write stays inside that exclusively-owned block.
        unsafe { ptr.add(i).write(Eterm::from_raw(*word)) };
    }
}

/// Read `n` raw term words at `ptr` (their `.raw()` payloads).
fn read_words(ptr: *const Eterm, n: usize) -> Vec<usize> {
    (0..n)
        // SAFETY: as `write_words` — `ptr` addresses at least `n` live words.
        .map(|i| unsafe { (*ptr.add(i)).raw() })
        .collect()
}

/// A one-element cons cell `[car | []]`, returning the LIST term.
fn cons(heap: &mut ContextHeap, car: Eterm) -> Eterm {
    let cell = heap.alloc(2).expect("the arena allocates a cons cell");
    // SAFETY: `alloc(2)` handed out exactly the cell's two words.
    unsafe {
        cell.write(car);
        cell.add(1).write(NIL);
    }
    beam_term::make_list(cell)
}

/// A tuple of `elements`, returning the BOXED term.
fn tuple_of(heap: &mut ContextHeap, elements: &[Eterm]) -> Eterm {
    let hp = heap
        .alloc(1 + elements.len())
        .expect("the arena allocates a tuple");
    // SAFETY: `alloc(1 + n)` handed out the header word plus one word per element.
    unsafe {
        hp.write(beam_term::make_arityval(elements.len()));
        for (i, element) in elements.iter().enumerate() {
            hp.add(1 + i).write(*element);
        }
    }
    make_tuple(hp)
}

/// A structural description of `term` with **no addresses in it** — the tags and the small
/// integers, recursively. Two graphs are the same graph iff their descriptions match, which
/// is how a relocation's "the structure and the values are unchanged" claim is asserted
/// without ever comparing a pointer.
fn shape(term: Eterm) -> String {
    // The primary tag, never the loose `is_list`/`is_boxed` bit tricks: those are also
    // true for a bare HEADER word (`is_list(0)` holds), which is exactly the confusion a
    // structural renderer must not fall into.
    match primary_tag(term) {
        PrimaryTag::Immed1 if is_nil(term) => "[]".to_string(),
        PrimaryTag::Immed1 if is_small(term) => format!("#{}", signed_value(term)),
        PrimaryTag::Immed1 => format!("immed:{:#x}", term.raw()),
        PrimaryTag::List => {
            // SAFETY: a LIST word built by `cons` addresses a live `[CAR, CDR]` cell.
            let (car, cdr) = unsafe { (*list_val(term), *list_val(term).add(1)) };
            format!("[{}|{}]", shape(car), shape(cdr))
        }
        PrimaryTag::Boxed => {
            // SAFETY: every BOXED word built by `tuple_of` addresses a live arityval header,
            // and its `arityval` elements follow it.
            let hp = tuple_val(term);
            let arity = unsafe { arityval(*hp) };
            let mut out = String::from("{");
            for i in 0..arity {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&shape(unsafe { *hp.add(1 + i) }));
            }
            out.push('}');
            out
        }
        PrimaryTag::Header => format!("header:{:#x}", term.raw()),
    }
}

/// Every region's written prefix, as raw words — a byte-level snapshot of the arena.
fn arena_words(heap: &ContextHeap) -> Vec<Vec<usize>> {
    heap.regions
        .iter()
        .map(|region| read_words(region.base(), region.used()))
        .collect()
}

// ---------------------------------------------------------------------------
// Tier A — the mark rewind
// ---------------------------------------------------------------------------

/// A rewind drops every region newer than the marked one, moves the surviving frontier's
/// bump pointer back to the marked `used` figure, leaves the words below the mark
/// byte-identical, and keeps the arena's cumulative figures continuous.
#[test]
fn a_rewind_drops_every_later_region_and_keeps_the_words_below_the_mark() {
    // Reserve == commit == 4 words, so `alloc(4)` exactly fills a reservation and the next
    // one needs a fresh region: region 0 (reserved 4), region 1 (reserved 8, committed 4),
    // region 2 (reserved 16).
    let mut heap = ContextHeap::with_geometry(4, 4);
    let a = heap.alloc(4).expect("region 0 serves 4 words");
    write_words(a, &[0xA0, 0xA1, 0xA2, 0xA3]);
    let b = heap.alloc(4).expect("region 1 serves 4 words");
    write_words(b, &[0xB0, 0xB1, 0xB2, 0xB3]);

    let mark = heap.mark();
    assert_eq!(mark.region(), Some(1), "the mark names the frontier region");
    assert_eq!(mark.used(), 4);
    assert_eq!(mark.base(), b as usize, "and its base address");
    assert!(heap.mark_matches(mark));

    // Above the mark: the frontier grows in place once, then a third region appears.
    let c = heap.alloc(4).expect("region 1 grows in place");
    assert_eq!(
        c as usize,
        b as usize + 4 * WORD,
        "the in-place grow keeps the bump pointer contiguous"
    );
    write_words(c, &[0xC0, 0xC1, 0xC2, 0xC3]);
    let d = heap.alloc(4).expect("region 2 serves 4 words");
    write_words(d, &[0xD0, 0xD1, 0xD2, 0xD3]);
    assert_eq!(heap.regions.len(), 3);

    let before = heap.stats();
    assert_eq!(before.live_words, 4 + 8 + 4, "every region was written");
    assert_eq!(before.used_words, before.live_words, "nothing released yet");

    let report = heap.rewind(mark).expect("a matching mark rewinds");
    assert_eq!(report.regions_dropped, 1, "only region 2 is newer than the mark");
    assert_eq!(
        report.words_released,
        4 + 4,
        "region 2's 4 words plus the frontier's 4 words above the mark"
    );
    assert_eq!(report.live_words, 8);
    assert_eq!(report.reserved_words, 4 + 8, "the survivors' reservations");
    assert_eq!(report.committed_words, 4 + 8);

    // The words below the mark are byte-identical: a rewind moves a pointer, not a word.
    assert_eq!(read_words(a, 4), vec![0xA0, 0xA1, 0xA2, 0xA3]);
    assert_eq!(read_words(b, 4), vec![0xB0, 0xB1, 0xB2, 0xB3]);

    let after = heap.stats();
    assert_eq!(after.regions, 2, "region 2's carrier is gone");
    assert_eq!(after.used_words, before.used_words, "cumulative demand is unchanged");
    assert_eq!(after.live_words, 8, "the words handed out right now DID fall");
    assert_eq!(after.released_words, 8);
    assert_eq!(after.peak_words, 4 + 8, "the survivors' high-water marks");
    assert!(
        !heap.mark_matches(mark),
        "a pre-rewind mark never matches again, even though region 1 is still the frontier"
    );
    assert!(
        !heap.mark_matches(HeapMark::EMPTY),
        "the empty mark's epoch is stale once the arena has been rewound"
    );

    // The released words are handed out again (the frontier really moved back) — and never
    // from inside a dropped region.
    let e = heap.alloc(4).expect("the frontier reuses the released words");
    assert_eq!(e as usize, c as usize, "the bump pointer resumed at the mark");
    let dropped = d as usize..d as usize + 4 * WORD;
    assert!(
        !dropped.contains(&(e as usize)),
        "a new allocation must never land in a dropped region's range"
    );
}

/// Every refusal arm leaves the arena byte-identical, and a mark is refused for good once a
/// rewind has run.
#[test]
fn a_foreign_forward_or_stale_mark_is_refused_and_changes_nothing() {
    let mut heap = ContextHeap::with_geometry(4, 4);
    let _a = heap.alloc(4).expect("region 0 serves 4 words");
    let b = heap.alloc(4).expect("region 1 serves 4 words");
    let live = heap.mark();
    assert_eq!(live.region(), Some(1));
    assert_eq!(live.base(), b as usize);

    let epoch = heap.epoch;
    let before = heap.stats();
    let words_before = arena_words(&heap);

    // Forward: claims a word the region never handed out.
    let forward = HeapMark {
        used: live.used() + 1,
        ..live
    };
    assert!(matches!(heap.rewind(forward), Err(BeamError::BadArg)));

    // A base address this arena never handed out (a mark from another arena).
    let foreign = HeapMark {
        base: live.base() + WORD,
        ..live
    };
    assert!(matches!(heap.rewind(foreign), Err(BeamError::BadArg)));

    // A region index this arena never reached.
    let absent = HeapMark {
        region: Some(heap.regions.len()),
        ..live
    };
    assert!(matches!(heap.rewind(absent), Err(BeamError::BadArg)));

    // A mark from an earlier epoch (what every pre-rewind mark becomes).
    let stale = HeapMark {
        epoch: epoch + 1,
        ..live
    };
    assert!(matches!(heap.rewind(stale), Err(BeamError::BadArg)));

    assert_eq!(
        heap.stats(),
        before,
        "a refused rewind must not change the census"
    );
    assert_eq!(arena_words(&heap), words_before, "nor write a single word");
    assert_eq!(heap.epoch, epoch, "nor bump the epoch");
    assert!(heap.mark_matches(live), "the live mark still matches");

    // A real rewind still works — and turns that same mark into a refused one.
    heap.rewind(live).expect("a matching mark rewinds");
    let rewound = heap.stats();
    assert!(!heap.mark_matches(live));
    assert!(matches!(heap.rewind(live), Err(BeamError::BadArg)));
    assert_eq!(heap.stats(), rewound, "the stale refusal changed nothing either");
}

/// An **epoch-matching** [`HeapMark::EMPTY`] is the arena-reset door: it drops every region.
#[test]
fn an_epoch_matching_empty_mark_is_the_arena_reset_door() {
    let mut heap = ContextHeap::with_geometry(4, 4);
    let _a = heap.alloc(4).expect("region 0 serves 4 words");
    let b = heap.alloc(4).expect("region 1 serves 4 words");
    let _c = heap.alloc(4).expect("region 1 grows in place");
    assert_eq!(heap.stats().regions, 2);
    assert_eq!(heap.stats().live_words, 4 + 8);

    let report = heap
        .rewind(HeapMark::EMPTY)
        .expect("an epoch-matching empty mark empties the arena");
    assert_eq!(report.regions_dropped, 2, "every region is newer than 'none'");
    assert_eq!(report.words_released, 4 + 8);
    assert_eq!(report.live_words, 0);
    assert_eq!(report.reserved_words, 0);
    assert_eq!(report.committed_words, 0);

    let stats = heap.stats();
    assert_eq!(stats.regions, 0);
    assert_eq!(stats.live_words, 0);
    assert_eq!(stats.reserved_words, 0);
    assert_eq!(stats.used_words, 12, "cumulative demand survives the reset");
    assert_eq!(stats.released_words, 12);
    assert!(
        !heap.mark_matches(HeapMark::EMPTY),
        "the epoch moved on, so the empty mark is spent"
    );
    assert!(
        matches!(heap.rewind(HeapMark::EMPTY), Err(BeamError::BadArg)),
        "and a second reset through it is a typed refusal, not a silent no-op"
    );

    // The emptied arena is fully usable again: the next allocation reserves a fresh first
    // region (and the old reservations are gone, so this is a new base).
    let d = heap.alloc(4).expect("the emptied arena allocates");
    assert_eq!(heap.stats().regions, 1);
    assert_eq!(heap.mark().base(), d as usize);
    assert_ne!(d as usize, b as usize, "the old regions' carriers were released");
}

/// A rewind **inside** one region releases words but keeps the region: the carrier, the
/// reservation, the committed limit and the peak are untouched, so no RSS falls.
#[test]
fn a_rewind_inside_one_region_only_moves_the_bump_pointer() {
    let mut heap = ContextHeap::with_geometry(8, 2);
    let a = heap.alloc(2).expect("region 0 serves 2 words");
    write_words(a, &[0x10, 0x11]);
    let mark = heap.mark();
    let b = heap.alloc(2).expect("region 0 grows in place");
    assert_eq!(b as usize, a as usize + 2 * WORD);
    write_words(b, &[0x20, 0x21]);

    let report = heap.rewind(mark).expect("a matching mark rewinds");
    assert_eq!(report.regions_dropped, 0, "no region is newer than the mark");
    assert_eq!(report.words_released, 2);
    assert_eq!(report.live_words, 2);
    assert_eq!(report.reserved_words, 8, "the carrier is untouched");
    assert_eq!(report.committed_words, 4, "so is the in-place-grown limit");

    let stats = heap.stats();
    assert_eq!(stats.peak_words, 4, "a reset leaves the peak alone");
    assert_eq!(stats.live_words, 2);
    assert_eq!(stats.released_words, 2);
    assert_eq!(stats.used_words, 4);

    assert_eq!(read_words(a, 2), vec![0x10, 0x11]);
    let c = heap.alloc(2).expect("the frontier reuses the released words");
    assert_eq!(c as usize, b as usize, "the bump pointer resumed at the mark");
}

// ---------------------------------------------------------------------------
// Tier B — the copying collapse
// ---------------------------------------------------------------------------

/// A reclaim collapses the arena into one fresh region, forwarding every root into it and
/// leaving the graph's structure and values unchanged.
#[test]
fn a_reclaim_collapses_the_arena_into_one_region_and_forwards_its_roots() {
    let mut heap = ContextHeap::with_geometry(64, 64);
    // A live graph: a 2-tuple whose first element is a cons cell.
    let cell = cons(&mut heap, make_small(7));
    let root = tuple_of(&mut heap, &[cell, make_small(9)]);
    let before_shape = shape(root);
    let old_cell = list_val(cell) as usize;

    // Unreachable garbage, big enough to need a second region (region 0 reserves 64 words).
    let garbage = heap.alloc(80).expect("a second region serves 80 words");
    write_words(garbage, &[0xEE; 80]);
    assert_eq!(heap.stats().regions, 2);
    assert_eq!(heap.stats().live_words, 5 + 80);

    let mark = heap.mark();
    let before = heap.stats();

    let mut roots = vec![root];
    let report = heap
        .reclaim(before.live_words, |reloc| reloc.forward_roots(&mut roots))
        .expect("the reclaim copies the live set")
        .expect("the arena had words to reclaim");

    assert_eq!(report.regions_dropped, 2, "every old region is dropped");
    assert_eq!(report.source_words, 85);
    assert_eq!(report.live_words, 5, "a 3-word tuple plus a 2-word cons cell");
    assert_eq!(report.words_reclaimed, 80, "the garbage stayed behind");
    assert_eq!(report.objects_copied, 2);
    assert_eq!(
        report.reserved_words, 85,
        "the survivor is the copy destination, so its reservation is the copy bound"
    );
    assert_eq!(report.committed_words, 85, "committed in full at construction");

    let stats = heap.stats();
    assert_eq!(stats.regions, 1, "one survivor region");
    assert_eq!(stats.live_words, 5);
    assert_eq!(
        stats.used_words, 85,
        "cumulative demand survives a reclaim"
    );
    assert_eq!(stats.released_words, 80);
    assert_eq!(stats.reserved_words, 85);
    assert_eq!(stats.committed_words, 85);
    assert_eq!(stats.peak_words, 5, "the survivor was never fuller");

    // The root was rewritten into the survivor, and the graph is the same graph.
    let moved = roots[0];
    assert_ne!(moved, root, "the root must name the copy, not the old words");
    assert_eq!(shape(moved), before_shape, "structure and values are unchanged");
    let base = tuple_val(moved) as usize;
    let end = base + stats.reserved_words * WORD;
    assert!(base <= end);
    // SAFETY: `moved` is a live 2-tuple in the survivor; its first element follows the header.
    let moved_cell = unsafe { *tuple_val(moved).add(1) };
    assert!(matches!(primary_tag(moved_cell), PrimaryTag::List));
    let moved_cell_ptr = list_val(moved_cell) as usize;
    assert_ne!(moved_cell_ptr, old_cell, "the cons cell moved too");
    assert!(
        (base..end).contains(&moved_cell_ptr),
        "the copied cons cell must live inside the survivor's reservation"
    );

    // Every mark taken before the reclaim is refused, by both doors.
    assert!(!heap.mark_matches(mark));
    assert!(matches!(heap.rewind(mark), Err(BeamError::BadArg)));
}

/// A destination smaller than the live volume is refused **before the first copy**, with the
/// arena byte-identical: no forwarding marker written, no root rewritten.
#[test]
fn a_destination_too_small_refuses_before_the_first_copy() {
    let mut heap = ContextHeap::with_geometry(64, 64);
    let cell = cons(&mut heap, make_small(7));
    let root = tuple_of(&mut heap, &[cell, make_small(9)]);
    let garbage = heap.alloc(80).expect("a second region serves 80 words");
    write_words(garbage, &[0xEE; 80]);

    let before = heap.stats();
    let words_before = arena_words(&heap);
    let mut roots = vec![root];

    let err = heap
        .reclaim(4, |reloc| reloc.forward_roots(&mut roots))
        .expect_err("a 4-word destination cannot hold the live set");
    assert!(
        matches!(err, BeamError::Alloc(_)),
        "the refusal is the typed heap_exhausted, got {err:?}"
    );

    assert_eq!(heap.stats(), before, "the refusal changed no census figure");
    assert_eq!(
        arena_words(&heap),
        words_before,
        "no forwarding marker was written into any region"
    );
    assert_eq!(roots, vec![root], "no root was rewritten");

    // ...and the arena is still fully usable, both to allocate and to reclaim.
    let _ = heap.alloc(8).expect("the refused arena still allocates");
    let source_words = heap.live_words();
    let mut roots = vec![root];
    let report = heap
        .reclaim(source_words, |reloc| reloc.forward_roots(&mut roots))
        .expect("a correctly sized reclaim still works")
        .expect("there was work to do");
    assert_eq!(report.live_words, 5);
}

/// `Ok(None)`: the two states in which there is nothing worth doing.
#[test]
fn a_reclaim_reports_none_when_there_is_nothing_to_do() {
    // No written word at all.
    let mut empty = ContextHeap::with_geometry(64, 64);
    assert_eq!(
        empty.reclaim(0, |_reloc| Ok(())).expect("no work, no failure"),
        None,
        "an arena that never handed out a word has nothing to copy or drop"
    );

    // One region whose used figure already fills its whole reservation.
    let mut full = ContextHeap::with_geometry(4, 4);
    let _ = full.alloc(4).expect("region 0 is exactly one reservation");
    assert_eq!(full.stats().regions, 1);
    assert_eq!(full.stats().live_words, full.stats().reserved_words);
    assert_eq!(
        full.reclaim(4, |_reloc| Ok(())).expect("nothing to do, no failure"),
        None,
        "there is no word to reclaim and no reservation that could shrink"
    );

    // A single region that is NOT full is still worth collapsing: `reclaim_heap`'s bound is
    // the live volume and the survivor is right-sized, so the reservation shrinks.
    let mut loose = ContextHeap::with_geometry(64, 64);
    let cell = cons(&mut loose, make_small(1));
    let mut roots = vec![cell];
    let report = loose
        .reclaim(loose.live_words(), |reloc| reloc.forward_roots(&mut roots))
        .expect("a loose region is reclaimable")
        .expect("there was work to do");
    assert_eq!(report.regions_dropped, 1);
    assert_eq!(report.live_words, 2);
}

/// An `Err` from the caller's own `forward` closure is unrecoverable (`beam_gc`'s documented
/// semantics), so the arena is left in its defined **empty** state rather than holding
/// forwarded marker words.
#[test]
fn an_error_from_the_forward_closure_leaves_the_arena_empty() {
    let mut heap = ContextHeap::with_geometry(64, 64);
    let cell = cons(&mut heap, make_small(7));
    let root = tuple_of(&mut heap, &[cell, make_small(9)]);
    let before = heap.stats();
    let mark = heap.mark();

    let mut roots = vec![root];
    let err = heap
        .reclaim(64, |reloc| {
            reloc.forward_roots(&mut roots)?;
            Err(BeamError::BadArg)
        })
        .expect_err("the closure's error propagates");
    assert!(matches!(err, BeamError::BadArg));

    let stats = heap.stats();
    assert_eq!(stats.regions, 0, "the abandoned arena holds no region");
    assert_eq!(stats.live_words, 0);
    assert_eq!(stats.reserved_words, 0);
    assert_eq!(stats.committed_words, 0);
    assert_eq!(
        stats.used_words, before.used_words,
        "the cumulative figure stays continuous across the abandonment"
    );
    assert_eq!(stats.released_words, before.live_words);
    assert!(
        !heap.mark_matches(mark),
        "the epoch moved on, so the pre-reclaim mark is refused"
    );

    let _ = heap.alloc(4).expect("the abandoned arena is usable again");
    assert_eq!(heap.stats().regions, 1);
}
