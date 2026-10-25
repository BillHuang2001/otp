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

//! Unit tests for the region-reclaim pass ([`super`]).
//!
//! Everything here drives the **safe** doors only — [`beam_alloc::Heap::cons`] to write onto the
//! store, [`Runtime::alloc_words`] for the arena, and a scan of
//! [`Runtime::for_each_off_heap_region`] to read a word back by comparing addresses (never a
//! dereference) — so a region that was wrongly dropped shows up as a *missing* address rather than
//! as a use-after-free the test harness cannot report. The crate is `#![deny(unsafe_code)]`.
//!
//! [`Runtime::alloc_words`]: crate::runtime::Runtime::alloc_words
//! [`Runtime::for_each_off_heap_region`]: crate::runtime::Runtime::for_each_off_heap_region

use core::mem::size_of;

use beam_term::Eterm;

use super::*;
use crate::config::RuntimeConfig;

/// A runtime whose store and arena regions are small enough that one growth retires a region.
///
/// The same shape `crate::context::pool::tests`'s `small_region_runtime` uses: only the *region*
/// size is small, so nothing else about the runtime changes.
///
/// The three region-reclaim action bits are then set **explicitly** (all on,
/// [`RegionReclaimConfig::default`]) because [`Runtime::new`] latches them from the
/// `BEAM_RT_REGION_RECLAIM` diagnostic gate: a case here asserts what an action *did*, so the
/// harness shell's environment must not be able to turn that action off underneath it. A case that
/// exercises the gate itself (see `the_move_off_leaves_a_partially_live_region_whole_and_the_move_on_collapses_it`)
/// sets its own configuration afterwards.
fn small_runtime() -> Runtime {
    let mut rt = Runtime::new(RuntimeConfig::default().with_heap_words(1024))
        .expect("a runtime with a small (but legal) term-heap region");
    rt.set_region_reclaim_enabled(RegionReclaimConfig::default());
    rt
}

/// A store window state that admits a pass (`Site::Exit`'s transient-pin rule aside).
fn open_window() -> WindowState {
    WindowState {
        open: true,
        pinned: None,
        lifetime_pinned: None,
    }
}

/// A pass at the exit site over `value_roots`, with the window reported as open and unpinned.
///
/// The move's corpus half is empty: these cases are about the **whole-region** action and the
/// pass's decline list, so every case that expects a drop gives the move nothing to evacuate.
fn exit_pass(rt: &mut Runtime, value_roots: &[Eterm]) {
    pass(
        rt,
        PassInput {
            site: Site::Exit,
            window: open_window(),
            value_roots,
            withheld: None,
        },
        &mut MoveSlots::default(),
    );
}

/// Writes one cons cell (2 words) onto the store's current region and answers its term.
fn cons_junk(rt: &mut Runtime) -> Eterm {
    rt.eval_heap
        .cons(beam_term::make_small(11), beam_term::make_small(22))
        .expect("a small region serves one cons cell")
}

/// Retires the store's current region by asking for one word more than it holds.
fn retire_current_region(rt: &mut Runtime) {
    let needed = rt.eval_heap.available() + 1;
    rt.grow_eval_heap(needed).expect("the store grows");
}

/// The store's observable shape: the current frontier, and each retired region's frontier.
fn store_shape(rt: &Runtime) -> (usize, Vec<usize>) {
    (
        rt.eval_heap.used(),
        rt.eval_heap_retired
            .iter()
            .map(|heap| heap.used())
            .collect(),
    )
}

/// Reads `count` words starting at `address` back out of **any** live region, without ever
/// dereferencing a pointer: the regions are enumerated and the one whose address span contains
/// `address` is copied from. `None` means no region holds that address any more — exactly the
/// evidence a wrongly-dropped region leaves.
fn read_words_at(rt: &mut Runtime, address: usize, count: usize) -> Option<Vec<Eterm>> {
    let width = size_of::<Eterm>();
    let mut found: Option<Vec<Eterm>> = None;
    rt.for_each_off_heap_region(|region| {
        if found.is_some() {
            return;
        }
        let base = region.as_ptr() as usize;
        let end = base + region.len() * width;
        if address >= base && address + count * width <= end {
            let start = (address - base) / width;
            found = Some(region[start..start + count].to_vec());
        }
    });
    found
}

/// Reads `count` words starting at `address` back out of the engine **arena**, without ever
/// dereferencing a pointer.
///
/// This is the arena's counterpart to [`read_words_at`]: that one scans the runtime's *own* store
/// regions ([`Runtime::for_each_off_heap_region`]) and deliberately not the arena, while a term
/// built through [`Runtime::alloc_words`] stands in the arena's ladder — which is exactly the set
/// of regions the pass's Tier A rewind drops. Asking the arena for the region whose address span
/// contains `address` (never a fabricated extent) makes "the region holding this term survived"
/// observable directly, at the term's own address.
///
/// [`Runtime::for_each_off_heap_region`]: crate::runtime::Runtime::for_each_off_heap_region
/// [`Runtime::alloc_words`]: crate::runtime::Runtime::alloc_words
fn read_arena_words_at(rt: &mut Runtime, address: usize, count: usize) -> Option<Vec<Eterm>> {
    let width = size_of::<Eterm>();
    let mut found: Option<Vec<Eterm>> = None;
    rt.arena.for_each_heap_region(|words| {
        if found.is_some() {
            return;
        }
        let base = words.as_mut_ptr() as usize;
        let end = base + words.len() * width;
        if address >= base && address + count * width <= end {
            let start = (address - base) / width;
            found = Some(words[start..start + count].to_vec());
        }
    });
    found
}

/// Asserts a decline was recorded with `reason` and that the pass moved no counter.
fn assert_declined(rt: &Runtime, before: RegionReclaimStats, reason: &'static str) {
    let after = rt.region_reclaim_stats();
    assert_eq!(
        after.reclaim_declines,
        before.reclaim_declines + 1,
        "the decline must be counted"
    );
    assert_eq!(after.reclaim_last_decline, Some(reason));
    assert_eq!(
        after.store_regions_dropped, before.store_regions_dropped,
        "a decline drops no region"
    );
    assert_eq!(
        after.store_words_reclaimed, before.store_words_reclaimed,
        "a decline reclaims no word"
    );
    assert_eq!(after.arena_reclaims, before.arena_reclaims, "no arena rewind");
}

#[test]
fn a_live_value_in_a_retired_region_is_retained_while_a_dead_one_is_dropped() {
    let mut rt = small_runtime();
    // The window opens first, so both growths below retire a region the window **created**.
    open(&mut rt);
    let live = cons_junk(&mut rt);
    let live_address = beam_term::list::ptr_val(live) as usize;
    let live_words = read_words_at(&mut rt, live_address, 2).expect("the live cell is readable");
    retire_current_region(&mut rt);
    let dead = cons_junk(&mut rt);
    let dead_address = beam_term::list::ptr_val(dead) as usize;
    retire_current_region(&mut rt);
    cons_junk(&mut rt);
    assert_eq!(rt.eval_heap_retired.len(), 2, "two regions were retired");

    exit_pass(&mut rt, &[live]);

    let stats = rt.region_reclaim_stats();
    assert_eq!(
        rt.eval_heap_retired.len(),
        1,
        "only the region the live term stands in survives"
    );
    assert_eq!(stats.store_regions_dropped, 1, "the dead region was dropped");
    assert_eq!(
        stats.store_words_reclaimed, 2,
        "one cons cell's worth of words"
    );
    assert_eq!(
        stats.store_compactions, 0,
        "six candidate words are below the move's threshold, so no compaction is attempted"
    );
    assert_eq!(
        stats.store_move_declines, 0,
        "below the threshold the move is *skipped*, not declined"
    );
    assert_eq!(stats.reclaim_declines, 0, "the pass did not decline");
    // The live cell is still at its own address, unchanged — read back through the region scan.
    assert_eq!(
        read_words_at(&mut rt, live_address, 2),
        Some(live_words),
        "the retained value still reads back word-for-word at the same address"
    );
    // The dropped region's address is gone from every live region.
    assert_eq!(
        read_words_at(&mut rt, dead_address, 2),
        None,
        "the dropped region's carrier was released"
    );
}

#[test]
fn the_switch_off_leaves_the_retired_chain_alone() {
    let mut rt = small_runtime();
    open(&mut rt);
    cons_junk(&mut rt);
    retire_current_region(&mut rt);
    cons_junk(&mut rt);
    retire_current_region(&mut rt);
    cons_junk(&mut rt);
    let before = store_shape(&rt);
    assert_eq!(before.1.len(), 2);
    assert_eq!(before, (2_usize, vec![2, 2]), "two words per retired region");

    rt.set_region_reclaim_enabled(RegionReclaimConfig::OFF);
    let stats_before = rt.region_reclaim_stats();
    exit_pass(&mut rt, &[]);

    let stats = rt.region_reclaim_stats();
    assert_eq!(stats.reclaim_declines, stats_before.reclaim_declines + 1);
    assert_eq!(
        stats.reclaim_last_decline,
        Some("the region reclamation switch is off")
    );
    assert_eq!(store_shape(&rt), before, "neither action ran: nothing moved");
}

#[test]
fn every_decline_records_its_reason_and_touches_nothing() {
    // Each case gets its own runtime: the decline paths are independent, and a shared runtime would
    // let one case's counters explain another's.

    // ---- 1. the switch is off ------------------------------------------------------------
    {
        let mut rt = small_runtime();
        open(&mut rt);
        cons_junk(&mut rt);
        retire_current_region(&mut rt);
        cons_junk(&mut rt);
        let shape = store_shape(&rt);
        rt.set_region_reclaim_enabled(RegionReclaimConfig::OFF);
        let before = rt.region_reclaim_stats();
        exit_pass(&mut rt, &[]);
        assert_declined(&rt, before, "the region reclamation switch is off");
        assert_eq!(store_shape(&rt), shape);
    }

    // ---- 2. no region-reclaim window is open ---------------------------------------------
    {
        let mut rt = small_runtime();
        open(&mut rt);
        cons_junk(&mut rt);
        close(&mut rt);
        let shape = store_shape(&rt);
        let before = rt.region_reclaim_stats();
        exit_pass(&mut rt, &[]);
        assert_declined(&rt, before, "no region-reclaim window is open");
        assert_eq!(store_shape(&rt), shape);
    }

    // ---- 3. the statement window is not open ---------------------------------------------
    {
        let mut rt = small_runtime();
        open(&mut rt);
        cons_junk(&mut rt);
        let shape = store_shape(&rt);
        let before = rt.region_reclaim_stats();
        pass(
            &mut rt,
            PassInput {
                site: Site::Exit,
                window: WindowState::default(),
                value_roots: &[],
                withheld: None,
            },
            &mut MoveSlots::default(),
        );
        assert_declined(&rt, before, "no statement window is open");
        assert_eq!(store_shape(&rt), shape);
    }

    // ---- 4. a runtime-lifetime pin is NOT a decline ---------------------------------------
    //
    // The pass roots itself in the runtime's **complete** host-side corpus
    // (`Runtime::for_each_root_mut` plus a raise's live reason), gathered at exactly these two
    // non-running sites, so a `stabilize_term` store's word is *found by the descent* and its
    // region is retained by evidence. The latch therefore cannot block the whole, region either;
    // it is kept only for the store move, which cannot rewrite a lifetime store's slot.
    {
        let mut rt = small_runtime();
        open(&mut rt);
        cons_junk(&mut rt);
        retire_current_region(&mut rt);
        cons_junk(&mut rt);
        let before = rt.region_reclaim_stats();
        pass(
            &mut rt,
            PassInput {
                site: Site::Exit,
                window: WindowState {
                    open: true,
                    pinned: Some("a lifetime store"),
                    lifetime_pinned: Some("a lifetime store"),
                },
                value_roots: &[],
                withheld: None,
            },
            &mut MoveSlots::default(),
        );
        let after = rt.region_reclaim_stats();
        assert_eq!(
            after.reclaim_declines, before.reclaim_declines,
            "a lifetime pin does not decline the whole-region pass"
        );
        assert_eq!(
            after.store_regions_dropped,
            before.store_regions_dropped + 1,
            "the dead retired region was still dropped"
        );
    }

    // ---- 5. a transient pin declines the BOUNDARY and not the exit ------------------------
    {
        let transient = WindowState {
            open: true,
            pinned: Some("a machine in flight"),
            lifetime_pinned: None,
        };
        let mut rt = small_runtime();
        open(&mut rt);
        cons_junk(&mut rt);
        retire_current_region(&mut rt);
        cons_junk(&mut rt);
        let shape = store_shape(&rt);
        let before = rt.region_reclaim_stats();
        pass(
            &mut rt,
            PassInput {
                site: Site::Boundary,
                window: transient,
                value_roots: &[],
                withheld: None,
            },
            &mut MoveSlots::default(),
        );
        assert_declined(&rt, before, "a machine in flight");
        assert_eq!(store_shape(&rt), shape, "the boundary declined");

        // The same window at the exit: the machine that pinned it was dropped inside the
        // statement, so the pass still runs — which is what makes a compile's retired chain
        // reachable at all.
        let before = rt.region_reclaim_stats();
        pass(
            &mut rt,
            PassInput {
                site: Site::Exit,
                window: transient,
                value_roots: &[],
                withheld: None,
            },
            &mut MoveSlots::default(),
        );
        let after = rt.region_reclaim_stats();
        assert_eq!(
            after.reclaim_declines, before.reclaim_declines,
            "a transient pin is not a decline at the exit site"
        );
        assert_eq!(
            after.store_regions_dropped,
            before.store_regions_dropped + 1,
            "the exit-site pass still dropped the dead retired region"
        );
    }

    // ---- 6. the caller holds a term it cannot hand over -----------------------------------
    {
        let mut rt = small_runtime();
        open(&mut rt);
        cons_junk(&mut rt);
        retire_current_region(&mut rt);
        cons_junk(&mut rt);
        let shape = store_shape(&rt);
        let before = rt.region_reclaim_stats();
        pass(
            &mut rt,
            PassInput {
                site: Site::Exit,
                window: open_window(),
                value_roots: &[],
                withheld: Some("the driver cannot hand its live term over"),
            },
            &mut MoveSlots::default(),
        );
        assert_declined(&rt, before, "the driver cannot hand its live term over");
        assert_eq!(store_shape(&rt), shape);
    }

    // ---- 7. a recorded `BeamError` may carry a live reason word ---------------------------
    //
    // Not reachable from here: `RunnerState::errors` is `pub(super)` to `crate::runner`, and it is
    // deliberately so — the map has no term accessor, which is exactly why the pass declines on it
    // (`RunnerState::has_errors`). The decline itself is one line of `refusal` and needs no
    // synthetic runtime state to be trusted; a test that reached into the map would have to be
    // written inside `crate::runner`, where the pass cannot be driven.

    // ---- 8. a machine in flight ----------------------------------------------------------
    //
    // Also not reachable: `Runtime::in_machine` is published only for the duration of a nested
    // `call_bif` (`crate::context::position`), i.e. from inside a BIF dispatch, and the pass runs
    // between statements. The branch exists so a future in-flight site that does reach here
    // declines rather than guessing.
}

#[test]
fn the_arena_rewind_fires_when_nothing_reachable_stands_above_the_mark_and_is_declined_when_something_does(
) {
    let mut rt = small_runtime();
    // A fresh arena has no region, so the mark is `HeapMark::EMPTY`.
    open(&mut rt);
    assert_eq!(rt.arena.heap_mark(), HeapMark::EMPTY, "a fresh arena");
    rt.alloc_words(4).expect("the arena serves 4 words");
    exit_pass(&mut rt, &[]);

    let stats = rt.region_reclaim_stats();
    assert_eq!(stats.arena_reclaims, 1, "the arena was rewound once");
    assert_eq!(stats.arena_words_reclaimed, 4, "exactly the 4 words written");
    assert_eq!(
        rt.arena.heap_stats().live_words,
        0,
        "nothing stands above the mark, so the arena collapsed to it"
    );
    // The cumulative-demand counter is *not* lowered by a rewind — it counts what the arena was
    // ever asked for, which is the figure `RetentionStats::arena_words` documents.
    assert!(
        rt.retention_stats().arena_words >= 4,
        "arena_words keeps its cumulative-demand meaning"
    );

    // Now let a reachable term stand above the mark: the pass must leave the arena byte-identical.
    open(&mut rt);
    let ptr = rt.alloc_words(2).expect("the arena serves 2 words");
    let term = beam_term::list::make_list(ptr);
    let live_before = rt.arena.heap_stats().live_words;
    exit_pass(&mut rt, &[term]);
    let after = rt.region_reclaim_stats();
    assert_eq!(
        after.arena_reclaims, stats.arena_reclaims,
        "a reachable term above the mark declines the rewind"
    );
    assert_eq!(
        rt.arena.heap_stats().live_words,
        live_before,
        "the arena is untouched"
    );
    // The descent reads that cons cell's two words, which are still the zero words the bump
    // allocator left there. `Eterm(0)` is a HEADER word, so the walk skips it as a null container —
    // nothing is dereferenced and no `unsafe` is needed for the descent to be safe.
}

#[test]
fn the_current_region_is_reset_to_the_windows_mark_and_a_rooted_word_blocks_it() {
    let mut rt = small_runtime();
    // A value written **before** the window opens stands below the mark, so it is never a
    // candidate: the pass may only prove things about the words this window wrote.
    cons_junk(&mut rt);
    open(&mut rt);
    let mark = rt.eval_heap.used();
    cons_junk(&mut rt);
    assert_eq!(rt.eval_heap.used(), mark + 2);

    exit_pass(&mut rt, &[]);

    assert_eq!(
        rt.eval_heap.used(),
        mark,
        "the current region's dead window is reclaimed by a reset to the mark"
    );
    let stats = rt.region_reclaim_stats();
    assert_eq!(stats.store_words_reclaimed, 2);
    assert_eq!(stats.store_regions_dropped, 0, "no region was dropped");

    // The same shape again, but this time the junk is named by a root: the reset must not fire.
    let junk = cons_junk(&mut rt);
    let used = rt.eval_heap.used();
    exit_pass(&mut rt, &[junk]);
    assert_eq!(
        rt.eval_heap.used(),
        used,
        "a rooted word inside the window blocks the reset"
    );
    assert_eq!(
        rt.region_reclaim_stats().store_words_reclaimed,
        stats.store_words_reclaimed,
        "nothing more was reclaimed"
    );
}

// ---- the store **move** (Phase 1b): the interleaved shape ------------------------------------

/// Fills the store's current region with dead cons cells until it holds at least `used` words (or
/// has no room for another cell) — the dead filler that makes a region *partially* live.
fn fill_with_dead_until(rt: &mut Runtime, used: usize) {
    while rt.eval_heap.used() < used && rt.eval_heap.available() >= 2 {
        cons_junk(rt);
    }
}

/// Builds one **partially live** region: a live 3-element tuple, then dead filler, then a growth
/// that retires the region. Answers the live value.
///
/// This is the shape the move exists for: the value is reachable from a caller slot, so the region
/// holding it can never be dropped whole, and one live word therefore pins every dead word beside
/// it.
fn build_partially_live_region(rt: &mut Runtime, tag: i64, fill_to: usize) -> Eterm {
    let value = rt
        .eval_heap
        .tuple(&[
            beam_term::make_small(tag),
            beam_term::make_small(tag + 1),
            beam_term::make_small(tag + 2),
        ])
        .expect("the store serves one 3-element tuple");
    fill_with_dead_until(rt, fill_to);
    retire_current_region(rt);
    value
}

/// A live value's current address and its words, read back through the region scan.
fn readback(rt: &mut Runtime, value: Eterm, count: usize) -> (usize, Vec<Eterm>) {
    let address = beam_term::tuple::tuple_val(value) as usize;
    let words = read_words_at(rt, address, count).expect("the live value is in a live region");
    (address, words)
}

/// A pass at the exit site that hands the move a real corpus.
///
/// `value_roots` is the *whole-region* action's corpus (a copy); the move forwards `slots`
/// instead, which is why a caller hands both over.
fn move_pass(rt: &mut Runtime, value_roots: &[Eterm], slots: &mut MoveSlots<'_>) {
    pass(
        rt,
        PassInput {
            site: Site::Exit,
            window: open_window(),
            value_roots,
            withheld: None,
        },
        slots,
    );
}

#[test]
fn the_move_collapses_several_partially_live_regions_into_one_and_every_value_reads_back() {
    let mut rt = small_runtime();
    open(&mut rt);
    // Three regions, each retaining one live value and therefore held whole by the whole-region
    // action — the interleaved shape.
    let mut a = build_partially_live_region(&mut rt, 10, 500);
    let mut b = build_partially_live_region(&mut rt, 20, 500);
    let mut c = build_partially_live_region(&mut rt, 30, 500);
    assert_eq!(rt.eval_heap_retired.len(), 3, "three regions were retired");
    let (address_a, words_a) = readback(&mut rt, a, 4);
    let (address_b, words_b) = readback(&mut rt, b, 4);
    let (address_c, words_c) = readback(&mut rt, c, 4);
    assert_ne!(address_a, address_b, "each value stands in its own region");

    move_pass(
        &mut rt,
        &[],
        &mut MoveSlots::new(vec![&mut a, &mut b, &mut c]),
    );

    let stats = rt.region_reclaim_stats();
    assert_eq!(stats.store_compactions, 1, "the move fired exactly once");
    assert_eq!(stats.store_move_declines, 0, "and declined nothing");
    assert_eq!(
        rt.eval_heap_retired.len(),
        0,
        "every source region was dropped, so the three regions are one"
    );
    assert_eq!(stats.store_regions_dropped, 4, "three retired + the emptied one");
    assert!(stats.store_words_reclaimed > 1000, "the dead filler was reclaimed");

    // Every retained value moved, and reads back word-for-word at its new address.
    for (tag, value, (old_address, old_words)) in [
        (10_i64, a, (address_a, words_a)),
        (20, b, (address_b, words_b)),
        (30, c, (address_c, words_c)),
    ] {
        let (new_address, new_words) = readback(&mut rt, value, 4);
        assert_ne!(new_address, old_address, "value {tag} moved");
        assert_eq!(
            new_words, old_words,
            "value {tag} is byte-for-byte identical at its new address"
        );
    }

    // ---- the same value pinned across a SECOND move ---------------------------------------
    let (address_a_first, words_a_first) = readback(&mut rt, a, 4);
    let mut second: Vec<Eterm> = (0..6)
        .map(|k| build_partially_live_region(&mut rt, 100 + k, 500))
        .collect();
    let mut slot_list: Vec<&mut Eterm> = Vec::with_capacity(second.len() + 1);
    slot_list.push(&mut a);
    slot_list.extend(second.iter_mut());
    move_pass(&mut rt, &[], &mut MoveSlots::new(slot_list));

    let stats = rt.region_reclaim_stats();
    assert_eq!(stats.store_compactions, 2, "the move fired a second time");
    let (address_a_second, words_a_second) = readback(&mut rt, a, 4);
    assert_ne!(
        address_a_second, address_a_first,
        "the value pinned across both moves moved with the first move's destination"
    );
    assert_eq!(
        words_a_second, words_a_first,
        "and is still byte-for-byte identical after the second move"
    );
    for value in &second {
        let (address, _) = readback(&mut rt, *value, 4);
        assert!(
            address != 0,
            "every value built for the second move is readable afterwards"
        );
    }
}

#[test]
fn the_move_off_leaves_a_partially_live_region_whole_and_the_move_on_collapses_it() {
    /// Builds the interleaved shape and answers (retired regions before, after; the stats).
    fn run(store_move: bool) -> (usize, usize, RegionReclaimStats) {
        let mut rt = small_runtime();
        rt.set_region_reclaim_enabled(RegionReclaimConfig {
            store_move,
            ..RegionReclaimConfig::default()
        });
        open(&mut rt);
        let mut a = build_partially_live_region(&mut rt, 10, 500);
        let mut b = build_partially_live_region(&mut rt, 20, 500);
        let mut c = build_partially_live_region(&mut rt, 30, 500);
        let before = rt.eval_heap_retired.len();
        // The whole-region action roots itself in `value_roots`, so a move-off run must name
        // its values there or their regions are dropped as dead.
        let roots = vec![a, b, c];
        move_pass(
            &mut rt,
            &roots,
            &mut MoveSlots::new(vec![&mut a, &mut b, &mut c]),
        );
        let stats = rt.region_reclaim_stats();
        // Every value must still read back either way — the A/B differs in *where*, not in
        // whether the runtime's values survive.
        for value in [a, b, c] {
            let (_, words) = readback(&mut rt, value, 4);
            assert_eq!(words.len(), 4, "the value is unchanged");
        }
        (before, rt.eval_heap_retired.len(), stats)
    }

    let (before, after, stats) = run(false);
    assert_eq!(before, 3, "three partially live regions");
    assert_eq!(
        after, 3,
        "with the move off, one live word pins each region and every one is retained whole"
    );
    assert_eq!(stats.store_compactions, 0, "no region was compacted");
    assert_eq!(stats.store_move_declines, 1, "the move was refused");
    assert_eq!(
        stats.store_move_last_decline,
        Some("the store move switch is off")
    );

    let (before, after, stats) = run(true);
    assert_eq!(before, 3, "the same shape");
    assert_eq!(after, 0, "with the move on, the three regions collapse into one");
    assert_eq!(stats.store_compactions, 1, "one compaction");
    assert_eq!(stats.store_move_declines, 0, "and no refusal");
}

// ---- the destination's capacity is the store's growth policy, not the live set -----------------

/// **A completed move leaves the store at least the capacity (and so the headroom) it had before.**
///
/// The store ([`Runtime::eval_heap`]) is the runtime's general-purpose `-eval`/single-pass region,
/// and exactly **one** of its writers grows on refusal: the table-less copy-out
/// ([`crate::context::RuntimeContext::move_result_into_runtime_store`] →
/// [`Runtime::grow_eval_heap`]). Every other writer copies into the region it finds and reports the
/// typed `error:system_limit` when the region is short —
/// [`Runtime::stabilize_term`] (the gateway to every runtime-lifetime store), the evaluator's own
/// term building (`crate::eval`'s `heap!`), and the native floor's `make_binary` replies
/// (`src/native/**`, e.g. a `filelib`/`prim_file`/`code` reply). So the move's destination capacity
/// is a **correctness** property, not a tuning knob.
///
/// The region-reclaim configuration is set **explicitly** by [`small_runtime`] (all three actions
/// on) rather than left to the `BEAM_RT_REGION_RECLAIM` gate [`Runtime::new`] latches, so no
/// environment can leave the move off and make this case fail for an environmental reason.
///
/// The shape this pins is the regression the old sizing produced: `need + max(need/8, 64)` sized
/// the destination from the *live set*, so collapsing three regions of a store that offered 8 192
/// words handed back a region of ~76 words — four orders of magnitude smaller — and the next
/// unrelated `stabilize_term` (a timer message, an exit reason, a runner outcome…) failed with
/// `error:system_limit`. The fix sizes the destination from the store's own growth policy
/// ([`Runtime::eval_store_growth_capacity`]) and only bounds it by what the move replaces, so the
/// capacity never shrinks while the collapse still releases the source regions.
#[test]
fn a_move_keeps_the_stores_capacity_and_the_headroom_the_growth_policy_gave_it() {
    let mut rt = small_runtime();
    open(&mut rt);
    let mut a = build_partially_live_region(&mut rt, 10, 500);
    let mut b = build_partially_live_region(&mut rt, 20, 500);
    let mut c = build_partially_live_region(&mut rt, 30, 500);
    let capacity_before = rt.eval_heap.capacity();
    let used_before = rt.eval_heap.used();
    assert_eq!(used_before, 0, "the third growth retired the filled region");
    assert!(
        capacity_before >= 8_192,
        "the store grew to serve three filled regions ({capacity_before} words)"
    );
    // The total capacity the move replaces: the region it installs *as* the store plus every
    // region it drops. The destination must stay at or below it, or the collapse would reserve
    // more than it frees.
    let source_total: usize = capacity_before
        + rt.eval_heap_retired
            .iter()
            .map(beam_alloc::Heap::capacity)
            .sum::<usize>();

    // A term the **pre-move** store could serve: it fits the current region's free words. The old
    // sizing made this exact copy fail after the move.
    let elems: Vec<Eterm> = (0..3_000).map(|index| beam_term::make_small(index % 100)).collect();
    let payload = beam_engine::terms::make_list(&mut rt.arena, &elems).expect("the arena is unbounded");
    let payload_words = beam_term::size_object(payload);
    assert_eq!(payload_words, 6_000, "a 3 000-element list is 6 000 words");
    assert!(
        payload_words <= capacity_before - used_before,
        "the premise: {payload_words} words fit the store before the move"
    );
    // The payload stands in the **arena**, above the window's mark — i.e. inside exactly the range
    // the pass's Tier A rewind drops when nothing reachable stands in it. A term held only in a
    // Rust local is reachable from no root family, so the caller must hand it over as a value root:
    // `PassInput::value_roots` is the caller's half of the pass's root set, and the pass cannot see
    // a local. Without the hand-over the rewind fires (correctly — nothing the runtime can
    // enumerate names the list) and the copy below reads a **released** region, which is sound only
    // by luck: the words usually still hold the list, and a concurrent test thread that reuses the
    // freed carrier makes the copy silently truncated.
    let payload_address = beam_term::list::ptr_val(payload) as usize;
    let payload_in_arena = read_arena_words_at(&mut rt, payload_address, 2)
        .expect("the payload's first cons cell stands in an arena region above the mark");
    let arena_live_before = rt.arena.heap_stats().live_words;
    let arena_reclaims_before = rt.region_reclaim_stats().arena_reclaims;

    move_pass(
        &mut rt,
        &[payload],
        &mut MoveSlots::new(vec![&mut a, &mut b, &mut c]),
    );
    let stats = rt.region_reclaim_stats();
    assert_eq!(stats.store_compactions, 1, "the move fired");
    assert_eq!(stats.store_move_declines, 0, "and declined nothing");
    // The rooted payload really does hold the arena's rewind, and it says so the way the descent
    // decided it: the region the list stands in survives, so the source of the copy below is live
    // at its own address. No counter and no timing is involved — the same address reads back
    // word-for-word.
    assert_eq!(
        stats.arena_reclaims, arena_reclaims_before,
        "a value root standing above the mark declines the arena rewind"
    );
    assert_eq!(
        rt.arena.heap_stats().live_words,
        arena_live_before,
        "so the arena is untouched"
    );
    assert_eq!(
        read_arena_words_at(&mut rt, payload_address, 2),
        Some(payload_in_arena),
        "and the payload still reads back word-for-word where it stands"
    );
    // ---- 1. headroom: the same live volume in the same store must have the same room -----------
    let live_after = rt.eval_heap.used();
    assert!(
        live_after < capacity_before,
        "the evacuated live set ({live_after} words) is a fraction of the region it came from"
    );
    assert!(
        rt.eval_heap.capacity() >= capacity_before,
        "the destination ({} words) must not be smaller than the region it replaces \
         ({capacity_before} words): a store has one growing consumer, so every other writer's \
         bound is this region's free words",
        rt.eval_heap.capacity()
    );
    assert!(
        rt.eval_heap.available() >= capacity_before - live_after,
        "the free words afterwards ({} words) must be at least what the pre-move store offered for \
         the same live volume ({} words)",
        rt.eval_heap.available(),
        capacity_before - live_after
    );

    // ---- 2. the win: bounded by what it replaces, so a multi-region collapse releases ----------
    assert!(
        rt.eval_heap.capacity() <= source_total,
        "the destination ({} words) may not reserve more than the move replaces ({source_total})",
        rt.eval_heap.capacity()
    );
    assert!(
        rt.eval_heap.capacity() < source_total,
        "collapsing {} regions must release capacity ({} < {source_total})",
        1 + stats.store_regions_dropped,
        rt.eval_heap.capacity()
    );

    // ---- 3. the behaviour the regression actually broke ----------------------------------------
    let copy = rt
        .stabilize_term(payload)
        .expect("a copy the pre-move store could serve must still be served after the move");
    assert_eq!(
        beam_term::size_object(copy),
        payload_words,
        "the copy is the whole payload"
    );
    assert!(beam_term::is_list(copy), "and it is still a list");
}

// ---- the post-move verification's forced hit (the `keep_both` obligation) ----------------------

/// Builds one partially live region whose live value is a **cons cell**: a 2-word value reachable
/// from a caller slot, then dead filler, then a growth that retires the region.
///
/// The tripwire's case uses a cons cell rather than a tuple for one reason, and it is a property of
/// the walker rather than of the move: the injected pre-move word is dereferenced by the descent
/// *after* its region's objects were forwarded, so its first word holds the cons marker
/// (`THE_NON_VALUE`) and its second the destination pointer — both of which the descent's guards
/// handle ([`crate::eval::reclaim::walk`]). A boxed object's forwarded header word holds the BOXED
/// marker, which no value descent can read; that residual is [`compact`]'s own documentation.
fn build_partially_live_cons_region(rt: &mut Runtime, tag: i64, fill_to: usize) -> Eterm {
    let value = rt
        .eval_heap
        .cons(beam_term::make_small(tag), beam_term::make_small(tag + 1))
        .expect("the store serves one cons cell");
    fill_with_dead_until(rt, fill_to);
    retire_current_region(rt);
    value
}

/// A cons value's address and its two words, read back through the region scan.
fn cons_readback(rt: &mut Runtime, value: Eterm) -> (usize, Vec<Eterm>) {
    let address = beam_term::list::ptr_val(value) as usize;
    let words = read_words_at(rt, address, 2).expect("the live value is in a live region");
    (address, words)
}

#[test]
fn a_forced_verification_hit_keeps_every_region_and_the_next_pass_classifies_them_on_evidence() {
    let mut rt = small_runtime();
    open(&mut rt);
    // The interleaved shape: three regions, each holding one live cons cell among dead filler. The
    // move's thresholds are met (a large dead fraction, well above `MIN_WINDOW_WORDS`), so the only
    // thing deciding the outcome below is the verification.
    let mut a = build_partially_live_cons_region(&mut rt, 10, 500);
    let mut b = build_partially_live_cons_region(&mut rt, 20, 500);
    let mut c = build_partially_live_cons_region(&mut rt, 30, 500);
    assert_eq!(rt.eval_heap_retired.len(), 3, "three regions were retired");
    let before = rt.region_reclaim_stats();
    // What every value must read back as, wherever it ends up. The *words* are captured now; the
    // *terms* are read from the slots after the pass, because a moved value's slot is forwarded
    // while a saved copy of the pre-move term would still name the evacuated address.
    let expected: Vec<Vec<Eterm>> = [a, b, c]
        .iter()
        .map(|value| cons_readback(&mut rt, *value).1)
        .collect();
    let sources: Vec<usize> = [a, b, c]
        .iter()
        .map(|value| beam_term::list::ptr_val(*value) as usize)
        .collect();

    // ---- the forced hit ----------------------------------------------------------------
    compact::tripwire::arm();
    move_pass(
        &mut rt,
        &[],
        &mut MoveSlots::new(vec![&mut a, &mut b, &mut c]),
    );
    compact::tripwire::disarm();

    let stats = rt.region_reclaim_stats();
    // ---- the `KeptBoth` obligation, stated absolutely -----------------------------------
    //
    // A `R_VERIFY_HIT` outcome is *not* a compaction: the sources are kept, the destination is
    // installed as a retired region, nothing is released and nothing is counted as reclaimed. The
    // figures are asserted against **zero** (not against a snapshot) because a fresh store has
    // compacted and dropped nothing, so a future change that quietly releases one source on this
    // path fails here rather than merely balancing against itself.
    assert_eq!(
        before.store_compactions, 0,
        "premise: this runtime has compacted nothing yet"
    );
    assert_eq!(
        before.store_regions_dropped, 0,
        "premise: and dropped no region yet"
    );
    assert_eq!(
        stats.store_compactions, 0,
        "a verification hit must not count as a compaction"
    );
    assert!(
        stats.store_move_hit.is_some(),
        "the hit is attributed, not merely counted"
    );
    assert_eq!(
        stats.store_move_declines,
        before.store_move_declines + 1,
        "the refusal is recorded on the move's own counters"
    );
    assert_eq!(
        stats.store_move_last_decline,
        Some(compact::R_VERIFY_HIT),
        "and with the verification's reason"
    );
    // The diagnostic itself, pinned: the forced hit's holder is named. The tripwire's injected word
    // is a **snapshot of the first caller slot** that stands in a source region (slot 0 — `a`), so
    // the attribution must answer `CallerSlot(0)` — not a family, not `OutsideCorpus`. That is the
    // finding a fix is driven by, so it is asserted rather than left to a report.
    assert_eq!(
        stats.store_move_hit,
        Some(MoveHitOwner::CallerSlot(0)),
        "the hit names the corpus slot that reproduces it"
    );
    assert_eq!(
        stats.store_move_hit_family(),
        None,
        "a caller slot is not a root family"
    );
    assert_eq!(
        stats.store_move_hit_slot(),
        Some(0),
        "and its index is the index within the caller's own slot list"
    );
    assert_eq!(
        stats.store_regions_dropped, before.store_regions_dropped,
        "keeping both means dropping nothing"
    );
    assert_eq!(
        stats.store_words_reclaimed, before.store_words_reclaimed,
        "and reclaiming nothing"
    );
    assert_eq!(
        rt.eval_heap_retired.len(),
        4,
        "every source region is still installed, plus the destination"
    );
    // Every source region is still usable: the copy did happen (that is why the sources hold the
    // forwarding marker), so the pre-move addresses must still be readable.
    for address in &sources {
        let words = read_words_at(&mut rt, *address, 2).expect("the source region is still installed");
        assert_eq!(
            words[0],
            beam_term::THE_NON_VALUE,
            "the evacuated cons cell's car holds the forwarding indicator, so the copy is real \
             and not a rollback"
        );
    }
    // Every previously reachable value reads back byte-for-byte at its forwarded address — which is
    // inside the destination, the region the rewritten roots now name.
    let (destination_base, destination_end) = {
        let region = rt
            .eval_heap_retired
            .last()
            .expect("the destination is the last installed region");
        (
            heap_region_base(region),
            heap_region_base(region) + region.used() * size_of::<Eterm>(),
        )
    };
    assert!(
        destination_end > destination_base,
        "the destination holds the copies it was installed for"
    );
    for (value, expected_words) in [a, b, c].iter().zip(expected.iter()) {
        let (address, words) = cons_readback(&mut rt, *value);
        assert_eq!(words, *expected_words, "the value is byte-for-byte identical");
        assert!(
            address >= destination_base && address < destination_end,
            "the value now stands in the installed destination, so the rewritten root is valid"
        );
    }

    // ---- the next, uninjected pass re-classifies on evidence ---------------------------
    //
    // The moving corpus is empty here and the values are named as value roots, so the move has
    // nothing to count (`need == 0` ⇒ skipped) and the whole-region action decides: the three
    // source regions hold no reachable word any more, the destination does.
    let held: Vec<Eterm> = [a, b, c].to_vec();
    let dropped_before = rt.region_reclaim_stats();
    exit_pass(&mut rt, &held);

    let stats = rt.region_reclaim_stats();
    assert_eq!(
        stats.store_compactions, dropped_before.store_compactions,
        "nothing moved on the evidence pass either"
    );
    assert_eq!(
        stats.store_regions_dropped,
        dropped_before.store_regions_dropped + 3,
        "the three evacuated source regions are dropped on evidence"
    );
    assert_eq!(
        rt.eval_heap_retired.len(),
        1,
        "only the destination survives, because it is the one region a value stands in"
    );
    for (value, expected_words) in [a, b, c].iter().zip(expected.iter()) {
        let (_, words) = cons_readback(&mut rt, *value);
        assert_eq!(
            words, *expected_words,
            "every value is still byte-for-byte identical after the sources were dropped"
        );
    }
}

// ---- the holder diagnostic's *classification* (not only its attribution) ----------------------

/// The diagnostic that explains a verification hit (and a walk refusal) has two halves: **who**
/// (the attributed corpus slot, pinned by the forced-hit case above) and **where** the word stands
/// ([`holder`]'s classification, which is what turns "a live word is in an old region" into an
/// addressable finding). This case pins the second half.
///
/// It reuses the forced-hit shape — a real corpus word whose pre-move copy the tripwire leaves in a
/// source region — so the classification is asked of a word that is genuinely there, and asks it
/// twice: once against a candidate range that contains the word (it must be named as a candidate
/// range, i.e. a region the pass would have dropped) and once against a word that stands elsewhere
/// (it must **not** be called a candidate, which is the answer that keeps a fix from looking at the
/// wrong region).
///
/// [`holder`]: super::holder
#[test]
fn the_diagnostic_classifies_a_word_against_the_candidate_ranges_it_stands_in() {
    let mut rt = small_runtime();
    open(&mut rt);
    let mut a = build_partially_live_cons_region(&mut rt, 10, 500);
    let mut b = build_partially_live_cons_region(&mut rt, 20, 500);
    let mut c = build_partially_live_cons_region(&mut rt, 30, 500);
    let sources: Vec<usize> = [a, b, c]
        .iter()
        .map(|value| beam_term::list::ptr_val(*value) as usize)
        .collect();

    compact::tripwire::arm();
    move_pass(
        &mut rt,
        &[],
        &mut MoveSlots::new(vec![&mut a, &mut b, &mut c]),
    );
    compact::tripwire::disarm();

    // The attributed owner: the tripwire's injected word is a snapshot of the first caller slot
    // that stands in a source region, so the hit names slot 0 — the same finding the forced-hit
    // case asserts, restated here because this case is about the *other* half of the report.
    let stats = rt.region_reclaim_stats();
    assert_eq!(
        stats.store_move_hit,
        Some(MoveHitOwner::CallerSlot(0)),
        "the hit is attributed to the corpus slot that reproduces it"
    );

    // The classification, asked of a **synthetic** candidate range built around the word that
    // really stands in a source region (a range is a `(lo, hi)` byte interval, so a test may hand
    // `locate` any interval it likes — that is exactly what makes this a unit test of the
    // classifier rather than of the move).
    let width = size_of::<Eterm>();
    let candidates = vec![(sources[0], sources[0] + 2 * width)];
    let inside = holder::locate(&mut rt, &candidates, sources[0]);
    assert!(
        inside.contains("candidate range #0"),
        "a word standing inside a candidate range must be named as one: {inside}"
    );
    assert!(
        inside.contains("a region the pass would drop"),
        "and the classification must say what a candidate range *means*: {inside}"
    );

    // The same question about a word that stands **outside** every candidate range: the
    // destination is a live store region, so the answer is "not a candidate" — the correction that
    // tells a reader the retained region is not the one to look at.
    let destination_base = {
        let region = rt
            .eval_heap_retired
            .last()
            .expect("the destination is the last installed region");
        heap_region_base(region)
    };
    let outside = holder::locate(&mut rt, &candidates, destination_base);
    assert!(
        !outside.contains("candidate range"),
        "a word outside every candidate range must not be called one: {outside}"
    );
    assert!(
        outside.contains("live store/bif region"),
        "and it must name the family it *does* belong to: {outside}"
    );

    // A word no enumerated region holds is reported as such — never dereferenced, never called a
    // dangle (the two families this diagnostic can enumerate are the store's regions and the
    // arena's).
    let unheld = holder::locate(&mut rt, &candidates, usize::MAX & !(width - 1));
    assert!(
        !unheld.contains("candidate range"),
        "an address outside every region is not a candidate either: {unheld}"
    );
    assert!(
        unheld.contains("no region this diagnostic enumerates"),
        "and the honest answer names what it checked: {unheld}"
    );
}

// ---- the closed shape: a refc binary carried through the exit-site move, byte-exactly ---------

/// The on-heap `[BinRef][ErlSubBits]` writers, which is the one piece of this case the crate
/// cannot express safely: a refc pair is written by raw pointer into a region word block.
///
/// The crate is `#![deny(unsafe_code)]`; this nested module is the single exemption, the same shape
/// `src/eval/reclaim.rs`'s own test helper (`pair`) and the `beam_message` integration suites use.
mod refc_fixture {
    #![allow(unsafe_code)]

    use beam_alloc::RefcBinary;
    use beam_term::Eterm;

    /// Writes the on-heap `[BinRef][ErlSubBits]` pair naming `payload`'s bytes onto `heap`'s
    /// current region, and answers the tagged root (the `ErlSubBits` word).
    ///
    /// The returned term borrows nothing: it names `payload`'s off-heap bytes by pointer, exactly
    /// as a binary the engine materialized would.
    pub(super) fn write_pair(
        heap: &mut beam_alloc::Heap,
        payload: &RefcBinary,
        bits: usize,
    ) -> Eterm {
        // SAFETY: `alloc_words` reserves the pair's 8 words, which `write_refc_binary` fully
        // writes. `payload` is a live `RefcBinary` owned by the caller for at least as long as the
        // returned term, so the pointer the pair stores stays dereferenceable.
        unsafe {
            let hp = heap
                .alloc_words(beam_term::refc::ERL_REFC_BITS_SIZE)
                .expect("the store has room for one refc pair");
            beam_term::refc::write_refc_binary(
                hp,
                payload.data_ptr().cast::<Eterm>(),
                core::ptr::null_mut(),
                payload.data_ptr().cast_const(),
                0,
                bits,
            )
        }
    }

    /// The payload bytes and bit count the live pair `pair` names — the read half of
    /// [`write_pair`], through the frozen accessors rather than through the raw pair words.
    pub(super) fn read_pair(pair: Eterm) -> (Vec<u8>, usize) {
        // SAFETY: `pair` is a live `[BinRef][ErlSubBits]` written by `write_pair`, and the
        // `RefcBinary` handle its bytes live behind is the caller's for the whole of this call.
        // The borrow ends at the `to_vec`, so nothing outlives the payload it points into.
        unsafe {
            (
                beam_term::refc::refc_bytes(pair).to_vec(),
                beam_term::refc::refc_size(pair),
            )
        }
    }
}

/// **A value holding an off-heap binary survives the exit-site pass byte-exactly**, and the pass
/// still reclaims the dead words beside it.
///
/// This is the "closed shape" the module docs claim and nothing else asserted: a `[BinRef]` /
/// `[ErlSubBits]` pair is an **ordinary block** to the move — the `BinRef` is a leaf and its `val`
/// is not a heap word — so the payload pointer is carried across verbatim while the pair itself is
/// evacuated into the destination. A move that got that wrong would leave the value naming a
/// released payload, and a length check alone would not see it: the assertions below compare the
/// **bytes** read back through [`beam_term::refc::refc_bytes`] against the bytes written, and they
/// also prove the pass did something (`store_compactions == 1`, `store_regions_dropped > 0`).
#[test]
fn a_refc_binary_value_survives_the_exit_site_move_byte_exactly() {
    const BYTES: usize = 4096;
    let mut rt = small_runtime();
    open(&mut rt);
    // Position-dependent bytes, so a shifted or truncated copy is visible in the comparison.
    let written: Vec<u8> = (0..BYTES).map(|i| ((i * 7) % 251) as u8).collect();
    let payload = beam_alloc::RefcBinary::new(&written).expect("the payload allocation");
    // The runtime's escaped-payload store owns a handle, exactly as a payload the engine built
    // would be adopted; the test keeps its own handle too, so the payload's life does not depend on
    // the store's bookkeeping.
    rt.eval_off_heap_mut().push_binary(payload.clone());
    // The tagged root the store now holds: the pair's `ErlSubBits` word.
    let mut value = refc_fixture::write_pair(&mut rt.eval_heap, &payload, BYTES * 8);
    // The interleaved shape the move exists for: one live value, then dead filler filling the
    // region (the move's threshold is `MIN_WINDOW_WORDS` words of candidates, so a region must be
    // filled to the brim for the move to be attempted at all), then a growth that retires its
    // region, then a dead tail in the current region.
    fill_with_dead_until(&mut rt, 1024);
    retire_current_region(&mut rt);
    cons_junk(&mut rt);
    assert_eq!(rt.eval_heap_retired.len(), 1, "one region was retired");
    let before = rt.region_reclaim_stats();
    assert_eq!(before.store_compactions, 0, "premise: nothing compacted yet");
    assert_eq!(before.store_regions_dropped, 0, "nor dropped");

    // Where the pair stands before the move: the `ErlSubBits` word's own address, which the frozen
    // layout makes readable without dereferencing anything (`boxed_val` is a tag accessor).
    let pair_address = beam_term::boxed::boxed_val(value) as usize;
    assert_eq!(
        read_words_at(&mut rt, pair_address, 8).map(|words| words.len()),
        Some(8),
        "the 8-word pair is written into a region the runtime holds"
    );

    move_pass(&mut rt, &[value], &mut MoveSlots::new(vec![&mut value]));

    let stats = rt.region_reclaim_stats();
    assert_eq!(stats.store_compactions, 1, "the exit-site move ran: {stats:?}");
    assert!(
        stats.store_regions_dropped >= 1,
        "and it released at least one source region: every dead word beside the value is gone"
    );
    assert!(
        stats.store_words_reclaimed > 0,
        "which is the whole point of the move"
    );
    assert_eq!(
        stats.store_move_hit, None,
        "no verification hit: the pair is a root the corpus does enumerate"
    );
    // The bytes, read back through the frozen `beam_term::refc` accessors — never a length check
    // alone and never a re-read of the (released) source region.
    let (read_back, bits) = refc_fixture::read_pair(value);
    assert_eq!(read_back.len(), BYTES, "the whole window is still named");
    assert_eq!(read_back, written, "and every byte of it is intact");
    assert_eq!(bits, BYTES * 8, "the window's bit count is unchanged");
    // The pair itself really moved (the move's own claim), and it is readable at its new address.
    let moved_address = beam_term::boxed::boxed_val(value) as usize;
    assert_ne!(
        moved_address, pair_address,
        "the block was evacuated into the destination"
    );
    assert!(
        read_words_at(&mut rt, moved_address, 8).is_some(),
        "and the destination region holds it"
    );
}