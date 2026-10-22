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
fn small_runtime() -> Runtime {
    Runtime::new(RuntimeConfig::default().with_heap_words(1024))
        .expect("a runtime with a small (but legal) term-heap region")
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
fn exit_pass(rt: &mut Runtime, value_roots: &[Eterm]) {
    pass(
        rt,
        PassInput {
            site: Site::Exit,
            window: open_window(),
            value_roots,
            withheld: None,
        },
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
        "Phase 1a moves no word: a region is dropped whole, never compacted"
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
