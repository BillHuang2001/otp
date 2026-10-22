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
//
//! **The arena's two reclaim tiers, through their PUBLIC doors, at the PRODUCTION
//! geometry.**
//!
//! `src/context/reclaim/tests.rs` drives `ContextHeap::with_geometry` with 4- and 8-word
//! reservations, which is what lets every word figure there be asserted *exactly*. This
//! suite re-proves the same contracts over the real [`MinimalContext`] API only
//! ([`MinimalContext::heap_stats`], [`MinimalContext::heap_mark`],
//! [`MinimalContext::heap_mark_matches`], [`MinimalContext::for_each_heap_region`],
//! [`ExecutionContext::alloc_words`], [`MinimalContext::heap_rewind`],
//! [`MinimalContext::reclaim_heap`], [`MinimalContext::reclaim_heap_with_capacity`]),
//! against the geometry a real context has: a first region reserving [`RESERVE_WORDS`]
//! (4 MiB) and committing [`COMMIT_WORDS`] (64 KiB), and every later region reserving
//! double the previous one. A second region therefore needs a request bigger than what is
//! left of the frontier's *reservation* — a handful of bulk allocations, never a loop of
//! small ones.
//!
//! # What a unit test can and cannot prove about memory
//!
//! **This suite does not measure RSS, and no unit test can.** A test process's resident
//! set moves for reasons that have nothing to do with the arena (the test harness's own
//! threads, other concurrently running tests, the allocator's page cache), and a
//! reservation is one anonymous, *lazily faulted* `mmap` whose untouched pages are not
//! resident at all. The assertions below are therefore about **regions and words**: a
//! dropped region's carrier is released (so `heap_stats().regions` falls and
//! `reserved_words` falls with it) and a survivor's reservation is exactly the figure the
//! report names. That *is* the memory claim, because of how `beam_alloc::ReservedHeap`
//! works: `reset()` only moves the bump pointer and frees **nothing**; a region's whole
//! reserved block is released in its `Drop` and nowhere else. So "the region count and the
//! reserved words fell" is precisely "a carrier (one `mmap`) was released", and
//! "the survivor's reservation is the copy bound" is precisely "the arena's address space
//! was replaced by a right-sized one".
//!
//! # The division of labour with the module suite
//!
//! * Refusal arms that need a **fabricated** mark (a forward mark, a replaced-region mark)
//!   are the module suite's: `HeapMark`'s fields are private, so no public door can build
//!   an invalid one. What is reachable here is driven here — a mark from **another arena**
//!   (same shape, different base address), a mark taken before a rewind (epoch), and
//!   `HeapMark::EMPTY` once the arena's epoch has moved.
//! * Everything else — the two tiers end to end, both `Ok(None)` arms, the pre-copy
//!   capacity refusal, the allocation-follows-the-mark behaviour and the machine-rooted
//!   forward — is proven here, at production geometry, through the public API only.
//!
//! # Running it
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_engine --test heap_reclaim
//! ```

use beam_common::BeamError;
use beam_engine::{ExecutionContext, HeapMark, Machine, MinimalContext};
use beam_term::{
    arityval, is_nil, is_small, list_val, make_arityval, make_list, make_small, make_tuple,
    primary_tag, signed_value, tuple_val, Eterm, PrimaryTag, NIL,
};

/// One `Eterm`'s payload in bytes — what a bump-pointer step is measured in.
const WORD: usize = core::mem::size_of::<Eterm>();

/// A first region's **reservation** in words (4 MiB), matching `HEAP_RESERVE_WORDS`.
///
/// It bounds how far one region can grow in place, so a request larger than what is left
/// of it is what makes a fresh, doubled region appear.
const RESERVE_WORDS: usize = 4 * 1024 * 1024 / WORD;

// ---------------------------------------------------------------------------
// Helpers (raw-word access is confined to this test crate and every use is annotated)
// ---------------------------------------------------------------------------

/// Write `words` as raw term words at `ptr`.
fn write_words(ptr: *mut Eterm, words: &[usize]) {
    for (i, word) in words.iter().enumerate() {
        // SAFETY: every caller passes a pointer `alloc_words(n)` handed out with
        // `n >= words.len()`, so each write stays inside that exclusively-owned block.
        unsafe { ptr.add(i).write(Eterm::from_raw(*word)) };
    }
}

/// Write `word` into all `n` words at `ptr` — what makes a bulk block's *whole* written
/// prefix safe to snapshot (an unwritten word is uninitialised storage).
fn fill(ptr: *mut Eterm, n: usize, word: usize) {
    for i in 0..n {
        // SAFETY: the caller passes a pointer `alloc_words(n)` handed out with at least
        // `n` exclusively-owned words, so index `i` is in bounds.
        unsafe { ptr.add(i).write(Eterm::from_raw(word)) };
    }
}

/// A one-element cons cell `[car | []]`, returning the LIST term.
fn cons(ctx: &mut MinimalContext, car: Eterm) -> Eterm {
    let cell = ctx.alloc_words(2).expect("the arena allocates a cons cell");
    // SAFETY: `alloc_words(2)` handed out exactly the cell's two words.
    unsafe {
        cell.write(car);
        cell.add(1).write(NIL);
    }
    make_list(cell)
}

/// A tuple of `elements`, returning the BOXED term.
fn tuple_of(ctx: &mut MinimalContext, elements: &[Eterm]) -> Eterm {
    let hp = ctx
        .alloc_words(1 + elements.len())
        .expect("the arena allocates a tuple");
    // SAFETY: `alloc_words(1 + n)` handed out the header word plus one word per element.
    unsafe {
        hp.write(make_arityval(elements.len()));
        for (i, element) in elements.iter().enumerate() {
            hp.add(1 + i).write(*element);
        }
    }
    make_tuple(hp)
}

/// A structural description of `term` with **no addresses in it** — the tags and the small
/// integers, recursively.
///
/// Two graphs are the same graph iff their descriptions match, which is how a relocation's
/// "the structure and the values are unchanged" claim is asserted without ever comparing a
/// pointer (the copies necessarily live at different addresses).
fn shape(term: Eterm) -> String {
    // The primary tag, never the loose `is_list`/`is_boxed` bit tricks: those are also true
    // for a bare HEADER word, which is exactly the confusion a structural renderer must not
    // fall into.
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
///
/// Only a prefix this test **wrote** may be read (an allocation hands out uninitialised
/// storage), so callers snapshot before growing the arena with bulk blocks they never
/// wrote, or `fill` such a block first.
fn arena_words(ctx: &mut MinimalContext) -> Vec<Vec<usize>> {
    let mut regions = Vec::new();
    ctx.for_each_heap_region(|words| {
        regions.push(words.iter().map(|word| word.raw()).collect::<Vec<usize>>());
    });
    regions
}

/// Every region's `(base address, used words)` — the address span an allocation must never
/// land in once the region is dropped.
fn region_ranges(ctx: &mut MinimalContext) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    ctx.for_each_heap_region(|words| {
        ranges.push((words.as_mut_ptr() as usize, words.len()));
    });
    ranges
}

/// Whether `address` lies inside one of `ranges` (a region's `[base, base + words)` span).
fn inside(ranges: &[(usize, usize)], address: usize) -> bool {
    ranges
        .iter()
        .any(|(start, words)| (*start..start + words * WORD).contains(&address))
}

/// Grow `ctx` until it holds three regions, and answer the two bulk requests' sizes.
///
/// Each request is larger than what is left of the frontier's **reservation**, so only a
/// fresh, doubled region can serve it: region 0 reserves [`RESERVE_WORDS`], region 1 twice
/// that, region 2 twice again. The words are never written (`alloc_words` writes nothing
/// and a reservation is lazily faulted), so this costs no resident memory and no payload
/// write — but it does mean no caller may snapshot a bulk region's prefix.
fn grow_to_three_regions(ctx: &mut MinimalContext) {
    let _ = ctx
        .alloc_words(RESERVE_WORDS)
        .expect("a fresh region serves the first bulk request");
    let _ = ctx
        .alloc_words(2 * RESERVE_WORDS)
        .expect("a second fresh region serves the second bulk request");
    assert_eq!(
        ctx.heap_stats().regions,
        3,
        "two outgrowing requests must each add one region"
    );
}

// ---------------------------------------------------------------------------
// Tier A — the mark rewind
// ---------------------------------------------------------------------------

/// **Tier A end to end.** A rewind drops every region newer than the marked one, keeps the
/// words below the mark byte-identical (interior term pointers included), lowers
/// `live_words` while `used_words` stays cumulative, accounts the difference in
/// `released_words`, spends every pre-rewind mark, and makes the next allocation resume at
/// the mark — inside a live region.
#[test]
fn a_public_rewind_drops_the_grown_regions_and_keeps_the_words_below_the_mark() {
    let mut ctx = MinimalContext::new();
    // A small term graph below the mark: the words a rewind must preserve.
    let cell = cons(&mut ctx, make_small(11));
    let root = tuple_of(&mut ctx, &[cell, make_small(22)]);
    let root_shape = shape(root);
    assert_eq!(
        root_shape, "{[#11|[]],#22}",
        "the renderer names tags and values, never an address"
    );

    let mark = ctx.heap_mark();
    assert_eq!(mark.region(), Some(0), "the mark names the only region");
    assert_eq!(mark.used(), 5, "a 2-word cons cell plus a 3-word tuple");
    assert!(ctx.heap_mark_matches(mark));

    let before = ctx.heap_stats();
    assert_eq!(before.regions, 1);
    assert_eq!(before.live_words, 5);
    assert_eq!(before.released_words, 0);
    assert_eq!(before.used_words, 5);
    let below = arena_words(&mut ctx);
    assert_eq!(below.len(), 1, "one region, holding the graph");
    assert_eq!(below[0].len(), 5);

    grow_to_three_regions(&mut ctx);
    // Recorded while all three regions are live: their spans are pairwise disjoint, so an
    // allocation served from the survivor can never fall inside a dropped one's range.
    let spans = region_ranges(&mut ctx);
    assert_eq!(spans.len(), 3);
    let grown = ctx.heap_stats();
    assert_eq!(
        grown.live_words,
        5 + RESERVE_WORDS + 2 * RESERVE_WORDS,
        "every region was written"
    );
    assert!(
        grown.reserved_words > before.reserved_words,
        "the grown arena reserves more address space"
    );
    let late = ctx.heap_mark();
    assert_eq!(late.region(), Some(2), "the mark now names the frontier");

    let report = ctx
        .heap_rewind(mark)
        .expect("an epoch-matching mark rewinds");
    assert_eq!(
        report.regions_dropped, 2,
        "every region newer than the marked one"
    );
    assert_eq!(
        report.words_released,
        RESERVE_WORDS + 2 * RESERVE_WORDS,
        "the dropped regions' words (nothing was written above the mark)"
    );
    assert_eq!(report.live_words, 5);
    assert_eq!(
        report.reserved_words, before.reserved_words,
        "only the marked region's reservation survives"
    );
    assert_eq!(report.committed_words, before.committed_words);

    let after = ctx.heap_stats();
    assert_eq!(after.regions, 1, "the later regions' carriers are gone");
    assert_eq!(
        after.live_words, 5,
        "the words handed out right now fell..."
    );
    assert_eq!(
        after.used_words, grown.used_words,
        "...while cumulative demand did not"
    );
    assert_eq!(after.released_words, report.words_released);
    assert_eq!(
        after.used_words,
        after.live_words + after.released_words,
        "`used_words` is exactly `live_words + released_words`"
    );
    assert_eq!(
        after.reserved_words, before.reserved_words,
        "the reserved words fell back to the survivor's"
    );
    assert!(after.reserved_words < grown.reserved_words);

    // A rewind moves a pointer, not a word: the surviving prefix is byte-identical,
    // interior term pointers included.
    assert_eq!(arena_words(&mut ctx), below);
    assert_eq!(
        shape(root),
        root_shape,
        "the graph's structure and values are unchanged"
    );
    let live_range = mark.base()..mark.base() + before.reserved_words * WORD;
    assert!(
        live_range.contains(&(list_val(cell) as usize)),
        "the cons cell still lives in the surviving region"
    );
    // SAFETY: the tuple lives in the surviving region and its first element follows the
    // header word; the cons cell it names is the one above.
    assert_eq!(
        unsafe { *tuple_val(root).add(1) },
        cell,
        "the preserved word still names that very cons cell"
    );

    // Every pre-rewind mark is spent: one is refused by the predicate, the other by the door.
    assert!(!ctx.heap_mark_matches(mark), "the consumed mark is spent");
    assert!(
        !ctx.heap_mark_matches(late),
        "so is the pre-rewind frontier mark"
    );
    let settled = ctx.heap_stats();
    assert!(
        matches!(ctx.heap_rewind(mark), Err(BeamError::BadArg)),
        "a second rewind through the same mark is a typed refusal"
    );
    assert!(matches!(ctx.heap_rewind(late), Err(BeamError::BadArg)));
    assert_eq!(
        ctx.heap_stats(),
        settled,
        "a refusal changes no census figure"
    );
    assert_eq!(arena_words(&mut ctx), below, "and writes not a single word");

    // The released words are handed out again, from inside the surviving region — never
    // from the span of a region the rewind dropped.
    let fresh = ctx
        .alloc_words(4)
        .expect("the rewound arena serves the next allocation");
    assert_eq!(
        fresh as usize,
        mark.base() + 5 * WORD,
        "the bump pointer resumed exactly at the mark"
    );
    assert!(
        live_range.contains(&(fresh as usize)),
        "and the returned address is inside a live region"
    );
    assert!(
        !inside(&spans[1..], fresh as usize),
        "no allocation may land in a dropped region's span"
    );
}

/// **The refusal arms the public door can reach**, each leaving the arena byte-identical:
/// a mark from **another arena**, `HeapMark::EMPTY` once the epoch has moved, and a mark
/// taken before a rewind.
#[test]
fn a_foreign_or_pre_rewind_mark_is_refused_and_leaves_the_arena_byte_identical() {
    let mut ctx = MinimalContext::new();
    let cell = cons(&mut ctx, make_small(5));
    let _root = tuple_of(&mut ctx, &[cell, make_small(6)]);
    let mine = ctx.heap_mark();
    assert_eq!(mine.region(), Some(0));

    // A mark from a DIFFERENT arena: same shape (region 0 at epoch 0), but its own base
    // address — the one property a mark carries that an address can never coincide on.
    let mut other = MinimalContext::new();
    let _ = other.alloc_words(2).expect("the other arena allocates");
    let foreign = other.heap_mark();
    assert_eq!(foreign.region(), Some(0));
    assert_ne!(foreign.base(), mine.base(), "separate reservations");
    assert!(
        !ctx.heap_mark_matches(foreign),
        "another arena's mark never names this frontier"
    );

    let before = ctx.heap_stats();
    let words_before = arena_words(&mut ctx);
    assert!(
        matches!(ctx.heap_rewind(foreign), Err(BeamError::BadArg)),
        "a foreign mark is a typed refusal"
    );
    assert_eq!(
        ctx.heap_stats(),
        before,
        "the refusal changed no census figure"
    );
    assert_eq!(arena_words(&mut ctx), words_before, "nor wrote a word");

    // A real rewind works, and spends both the mark it used and `EMPTY`.
    ctx.heap_rewind(mine).expect("a matching mark rewinds");
    let rewound = ctx.heap_stats();
    let words_rewound = arena_words(&mut ctx);
    assert!(!ctx.heap_mark_matches(mine), "the used mark is spent");
    assert!(
        !ctx.heap_mark_matches(HeapMark::EMPTY),
        "the empty mark's epoch is spent too"
    );
    assert!(
        matches!(ctx.heap_rewind(mine), Err(BeamError::BadArg)),
        "a pre-rewind mark is refused for good"
    );
    assert!(
        matches!(ctx.heap_rewind(HeapMark::EMPTY), Err(BeamError::BadArg)),
        "so is the spent reset door"
    );
    assert_eq!(
        ctx.heap_stats(),
        rewound,
        "the stale refusals changed no census figure"
    );
    assert_eq!(
        arena_words(&mut ctx),
        words_rewound,
        "nor wrote a word either"
    );
}

/// **The arena-reset door**: an epoch-matching `HeapMark::EMPTY` drops every region, and
/// the emptied arena allocates again from a brand-new first region.
///
/// Nothing here asserts that the fresh region's address avoids the dropped regions' spans,
/// and that is deliberate: a reset releases **every** carrier, so the allocator is free to
/// place the next region's `mmap` at one of the freed addresses — a reuse that is correct,
/// not stale. The "never in a dropped region" guarantee is only assertable while a
/// survivor keeps its own words alive, which is
/// [`a_public_rewind_drops_the_grown_regions_and_keeps_the_words_below_the_mark`]'s case.
#[test]
fn an_epoch_matching_empty_mark_resets_the_arena_and_the_emptied_arena_allocates_again() {
    let mut ctx = MinimalContext::new();
    let _cell = cons(&mut ctx, make_small(1));
    grow_to_three_regions(&mut ctx);
    let before = ctx.heap_stats();

    let report = ctx
        .heap_rewind(HeapMark::EMPTY)
        .expect("an epoch-matching empty mark empties the arena");
    assert_eq!(
        report.regions_dropped, 3,
        "every region is newer than 'none'"
    );
    assert_eq!(report.words_released, before.live_words);
    assert_eq!(report.live_words, 0);
    assert_eq!(report.reserved_words, 0);
    assert_eq!(report.committed_words, 0);

    let after = ctx.heap_stats();
    assert_eq!(after.regions, 0, "every carrier was released");
    assert_eq!(after.live_words, 0);
    assert_eq!(after.reserved_words, 0);
    assert_eq!(after.committed_words, 0);
    assert_eq!(
        after.used_words, before.live_words,
        "cumulative demand is untouched by a reset"
    );
    assert_eq!(after.released_words, before.live_words);
    assert!(
        !ctx.heap_mark_matches(HeapMark::EMPTY),
        "the reset spent the empty mark"
    );

    let fresh = ctx
        .alloc_words(4)
        .expect("the emptied arena allocates again");
    assert_eq!(ctx.heap_stats().regions, 1, "a fresh first region");
    let address = fresh as usize;
    let live = region_ranges(&mut ctx);
    assert_eq!(live.len(), 1);
    assert_eq!(
        address, live[0].0,
        "the emptied arena's first allocation is the new region's base"
    );
    assert!(
        inside(&live, address),
        "it is served from the one live region"
    );
}

// ---------------------------------------------------------------------------
// Tier B — the copying collapse
// ---------------------------------------------------------------------------

/// **Tier B end to end.** A reclaim collapses the arena into one fresh survivor, rewriting
/// every forwarded root into it, keeping the graph's structure and values (compared by an
/// address-free render), dropping the unreachable words, and spending every pre-reclaim
/// mark by both doors.
#[test]
fn a_public_reclaim_collapses_the_arena_and_forwards_its_roots() {
    let mut ctx = MinimalContext::new();
    // A live graph: a 2-tuple whose first element is a cons cell.
    let cell = cons(&mut ctx, make_small(7));
    let root = tuple_of(&mut ctx, &[cell, make_small(9)]);
    let root_shape = shape(root);
    let old_cell = list_val(cell) as usize;

    // Unreachable garbage, big enough to force a second region.
    let garbage = ctx
        .alloc_words(RESERVE_WORDS)
        .expect("a fresh region serves the garbage");
    // One marker word, so the block is demonstrably part of the arena's written prefix —
    // a marker the reclaim must NOT copy (it is reachable from no root).
    write_words(garbage, &[0xEEEE_EEEE]);
    let source = ctx.heap_stats();
    assert_eq!(source.regions, 2);
    assert_eq!(source.live_words, 5 + RESERVE_WORDS);
    let pre_mark = ctx.heap_mark();
    assert_eq!(pre_mark.region(), Some(1));

    let mut roots = vec![root];
    let report = ctx
        .reclaim_heap(|reloc| reloc.forward_roots(&mut roots))
        .expect("the reclaim copies the live set")
        .expect("the arena had words to reclaim");

    assert_eq!(report.regions_dropped, 2, "every old region is dropped");
    assert_eq!(report.source_words, source.live_words);
    assert_eq!(
        report.live_words, 5,
        "a 3-word tuple plus a 2-word cons cell"
    );
    assert_eq!(
        report.words_reclaimed, RESERVE_WORDS,
        "the garbage stayed behind"
    );
    assert_eq!(report.objects_copied, 2);
    assert_eq!(
        report.reserved_words, source.live_words,
        "the survivor IS the copy destination, so its reservation is the copy bound"
    );
    assert_eq!(
        report.committed_words, source.live_words,
        "committed in full at construction"
    );

    let after = ctx.heap_stats();
    assert_eq!(after.regions, 1, "one survivor region");
    assert_eq!(after.live_words, 5);
    assert_eq!(
        after.used_words, source.used_words,
        "cumulative demand survives a reclaim"
    );
    assert_eq!(after.released_words, RESERVE_WORDS);
    assert_eq!(
        after.reserved_words, source.live_words,
        "the new geometry is the copy bound, not the old reservations"
    );
    assert_eq!(after.committed_words, source.live_words);
    assert_eq!(after.peak_words, 5, "the survivor was never fuller");
    assert!(
        after.live_words < source.live_words,
        "the unreachable words are gone"
    );

    // The root was rewritten into the survivor, and the graph is the same graph.
    let moved = roots[0];
    assert_ne!(
        moved, root,
        "the root must name the copy, not the old words"
    );
    assert_eq!(
        shape(moved),
        root_shape,
        "structure and values are unchanged"
    );
    let survivor = region_ranges(&mut ctx);
    assert_eq!(survivor.len(), 1);
    assert_eq!(survivor[0].1, 5, "the survivor holds exactly the copies");
    let base = tuple_val(moved) as usize;
    assert!(
        inside(&survivor, base),
        "the copied tuple names the survivor's range"
    );
    // SAFETY: `moved` is a live 2-tuple in the survivor; its first element follows the
    // header word.
    let moved_cell = unsafe { *tuple_val(moved).add(1) };
    assert!(matches!(primary_tag(moved_cell), PrimaryTag::List));
    assert_ne!(
        list_val(moved_cell) as usize,
        old_cell,
        "the cons cell moved too"
    );
    assert!(
        inside(&survivor, list_val(moved_cell) as usize),
        "the copied cons cell lives inside the survivor's reservation"
    );
    assert_eq!(shape(moved_cell), "[#7|[]]");

    // Every mark taken before the reclaim is refused, by both doors — and the new one matches.
    assert!(!ctx.heap_mark_matches(pre_mark));
    let settled = ctx.heap_stats();
    assert!(matches!(ctx.heap_rewind(pre_mark), Err(BeamError::BadArg)));
    assert_eq!(
        ctx.heap_stats(),
        settled,
        "the stale refusal changed nothing"
    );
    assert!(
        ctx.heap_mark_matches(ctx.heap_mark()),
        "a fresh mark of the survivor matches"
    );

    // The survivor is a normal allocation frontier.
    let next = ctx.alloc_words(3).expect("the survivor keeps allocating");
    assert!(
        inside(&region_ranges(&mut ctx), next as usize),
        "allocations continue inside the survivor"
    );
}

/// A destination capacity below the live volume is refused **before the first copy**, with
/// the arena and the roots byte-identical; the same arena then reclaims successfully
/// through the self-sizing door.
#[test]
fn a_capacity_below_the_live_volume_is_refused_before_the_first_copy() {
    let mut ctx = MinimalContext::new();
    let cell = cons(&mut ctx, make_small(3));
    let root = tuple_of(&mut ctx, &[cell, make_small(4)]);
    let root_shape = shape(root);
    // Garbage that stays inside region 0 (so its whole prefix is written, and therefore
    // safe to snapshot): a reclaim of it still has words to take back.
    const GARBAGE: usize = 20_000;
    let garbage = ctx
        .alloc_words(GARBAGE)
        .expect("region 0 grows in place to serve the garbage");
    fill(garbage, GARBAGE, 0x2222_2222);
    assert_eq!(ctx.heap_stats().regions, 1);

    let before = ctx.heap_stats();
    let words_before = arena_words(&mut ctx);
    assert_eq!(words_before[0].len(), 5 + GARBAGE);
    let mut roots = vec![root];

    let err = ctx
        .reclaim_heap_with_capacity(8, |reloc| reloc.forward_roots(&mut roots))
        .expect_err("an 8-word destination cannot hold the live set");
    assert!(
        matches!(err, BeamError::Alloc(_)),
        "the refusal is the typed heap_exhausted, got {err:?}"
    );
    assert_eq!(ctx.heap_stats(), before, "no census figure moved");
    assert_eq!(
        arena_words(&mut ctx),
        words_before,
        "no forwarding marker was written into any word"
    );
    assert_eq!(roots, vec![root], "no root was rewritten");

    // ...and the same arena still reclaims, through the self-sizing door.
    let report = ctx
        .reclaim_heap(|reloc| reloc.forward_roots(&mut roots))
        .expect("a correctly sized reclaim still works")
        .expect("there was work to do");
    assert_eq!(report.source_words, before.live_words);
    assert_eq!(report.live_words, 5);
    assert_eq!(
        report.reserved_words, before.live_words,
        "`reclaim_heap` sizes the survivor at its own copy bound"
    );
    assert_eq!(shape(roots[0]), root_shape);
    assert_eq!(ctx.heap_stats().regions, 1);
}

/// The two `Ok(None)` arms at production geometry: an arena that never handed out a word,
/// and a single region whose used figure already fills its whole reservation. Neither one
/// changes any state — in particular neither bumps the epoch, so the marks taken before the
/// call still match.
#[test]
fn a_reclaim_reports_none_when_there_is_nothing_to_do() {
    // Arm 1: the arena holds no written word.
    let mut empty = MinimalContext::new();
    assert_eq!(
        empty
            .reclaim_heap(|_reloc| Ok(()))
            .expect("no work, no failure"),
        None,
        "an arena that never handed out a word has nothing to copy or drop"
    );
    assert_eq!(empty.heap_stats().regions, 0);
    assert!(
        empty.heap_mark_matches(HeapMark::EMPTY),
        "a no-op reclaim does not bump the epoch"
    );

    // Arm 2: one region whose used figure already fills its whole reservation, so there is
    // no word to reclaim and no reservation that could shrink.
    let mut full = MinimalContext::new();
    let _ = full
        .alloc_words(RESERVE_WORDS)
        .expect("the first region reserves exactly this many words");
    let stats = full.heap_stats();
    assert_eq!(stats.regions, 1);
    assert_eq!(stats.live_words, RESERVE_WORDS);
    assert_eq!(
        stats.live_words, stats.reserved_words,
        "the region is full to its whole reservation"
    );
    let mark = full.heap_mark();
    assert_eq!(
        full.reclaim_heap(|_reloc| Ok(()))
            .expect("nothing to do, no failure"),
        None,
        "there is no word to reclaim and no reservation that could shrink"
    );
    assert_eq!(
        full.heap_stats(),
        stats,
        "the no-op changed no census figure"
    );
    assert!(
        full.heap_mark_matches(mark),
        "and did not spend the mark it was called with"
    );
}

// ---------------------------------------------------------------------------
// The machine-rooted forward — the exact call pattern a driver uses
// ---------------------------------------------------------------------------

/// **A `Machine` forwards its own root set into the relocator** — the host call pattern a
/// driver uses, pinned end to end: terms are built in the arena, one of them is installed
/// as `x(0)`, and the reclaim is driven through
/// [`Machine::for_each_root_mut`](beam_engine::Machine::for_each_root_mut) instead of a
/// test-owned `Vec`. Only what the machine's root set reaches is copied; the re-rooted
/// `x(0)` names the survivor and renders identically.
#[test]
fn a_machine_rooted_reclaim_forwards_the_machines_own_root_set() {
    let mut ctx = MinimalContext::new();
    let cell = cons(&mut ctx, make_small(13));
    let root = tuple_of(&mut ctx, &[cell, make_small(17)]);
    let root_shape = shape(root);

    // The machine's only root: x(0). `x_mut` notes the whole-file dirty extent, so the
    // hand-off below covers the file's prefix (the remaining slots are the refill `NIL`,
    // which the relocator leaves byte-identical).
    let mut machine = Machine::new();
    machine.x_mut()[0] = root;

    // Garbage the machine's roots do NOT reach: it must not be copied.
    let garbage = ctx.alloc_words(40).expect("the arena serves the garbage");
    fill(garbage, 40, 0x3333_3333);

    let report = ctx
        .reclaim_heap(|reloc| machine.for_each_root_mut(&mut |slots| reloc.forward_roots(slots)))
        .expect("the reclaim copies the machine's root set")
        .expect("there was work to do");
    assert_eq!(
        report.live_words, 5,
        "only the words the machine's root set reached were copied"
    );
    assert_eq!(report.words_reclaimed, 40);

    let moved = machine.x(0).expect("x(0) is inside the register file");
    assert_ne!(moved, root, "the machine's root names the copy");
    assert_eq!(shape(moved), root_shape, "the graph is unchanged");
    let survivor = region_ranges(&mut ctx);
    assert_eq!(survivor.len(), 1);
    assert!(
        inside(&survivor, tuple_val(moved) as usize),
        "the re-rooted x(0) is inside the survivor"
    );
}
