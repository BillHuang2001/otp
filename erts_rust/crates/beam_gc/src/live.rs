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

//! The **read-only live-graph counter** a fullsweep sizes its fresh mature space from.
//!
//! A fullsweep (major collection) must size the space it leaves behind from the words it
//! actually **keeps**, never from the occupancy it started with. A fullsweep-sized
//! generational heap *is* full when the cadence fires — it holds the live set plus every
//! promotion the minors since the last fullsweep made, all of which is garbage by then — so
//! an occupancy-fed "live" estimate asks
//! [`HeapPolicy::mature_after_fullsweep`](crate::HeapPolicy::mature_after_fullsweep) for
//! room for the *garbage* as well as the live set, its `should_grow` predicate fires on
//! **every** fullsweep, and the mature generation creeps up one growth-table step per
//! fullsweep without bound — for a live set that never changes. The C reference instead
//! sizes from the post-sweep live fill
//! (`need_after = (HEAP_TOP(p) - HEAP_START(p)) + need + stack_size + S_RESERVED`,
//! `erts/emulator/beam/erl_gc.c:2072`).
//!
//! This module provides that count, and it is deliberately the *same* walk a collection
//! performs: object sizing is [`move_size`](crate::copy::move_size), reference enumeration is
//! [`for_each_ref`](crate::copy::for_each_ref) — the two `pub(crate)` primitives the copier
//! and the mark-region space share — so the counter's per-object sizing and child
//! enumeration cannot drift from the copy it is sizing for. The `#[cfg(test)]` module at the
//! bottom of this file pins the two to each other.
//!
//! Properties the counter must have (the objective it exists for):
//!
//! * **Read-only.** It writes no forwarding marker, so it can run immediately before a
//!   collection over the very same from-spaces (`Copier` relies on the headers still being
//!   headers).
//! * **Iterative.** The traversal is a worklist over reference words, never the real stack,
//!   so a 50 000-deep list cannot overflow it.
//! * **Each object counted once.** A bitset over the from-space ranges (one bit per word)
//!   records the objects already admitted, so a shared subterm — or a cycle — contributes
//!   its words exactly once, exactly as the copier's in-place forwarding markers make it.
//! * **Out-of-from-set references skipped.** An immediate, a literal, a foreign space and an
//!   off-heap pointer are all left out, precisely as
//!   [`forward_slot`](crate::copy::forward_slot) classifies
//!   [`Target::Foreign`](crate::copy::Target::Foreign).
//!
//! `unsafe` here is confined to reading the words the walk visits — the same guarantee the
//! copier relies on ([`Target::Live`](crate::copy::Target::Live) dereferences them too) — and
//! each block carries a `// SAFETY:` rationale.

use core::mem::size_of;

use beam_common::Result;
use beam_term::prelude::{is_header, primary_tag, ptr_val, Eterm, PrimaryTag};

use crate::copy::{for_each_ref, move_size};
use crate::immix::{lines_for, ImmixSpace};
use crate::space::{FromSet, HeapSpace};

/// The exact size of the live graph a fullsweep has to evacuate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Live {
    /// The words a copy of the graph occupies, **exactly** — the `GcStats::words_copied` an
    /// actual evacuation of the same from-set and roots reports.
    pub(crate) words: usize,
    /// The Immix **lines** a copy of the graph occupies, exactly: the per-object
    /// [`lines_for`] sum, which is what line-granular allocation actually charges (a
    /// two-word cell still occupies a whole line). This is deliberately *not*
    /// `lines_for(words)` — that rounds the total once, while a destination must hold one
    /// line per object.
    pub(crate) lines: usize,
}

/// Counts the live graph reachable from `roots` out of the evacuated ranges of `from` — see
/// the module docs. `roots` is borrowed immutably: the count never rewrites a term.
pub(crate) fn live(from: &FromSet, roots: &[Eterm]) -> Live {
    let mut visited = Visited::new(from);
    let mut work: Vec<*mut Eterm> = Vec::new();
    let mut words = 0usize;
    let mut lines = 0usize;

    for &root in roots {
        seed(root, &mut visited, &mut work);
    }

    // A worklist, so the traversal depth is bounded by the heap and not by the real stack:
    // a 50 000-cell list is counted without recursing once.
    while let Some(obj) = work.pop() {
        // SAFETY: `obj` was admitted only through `Visited::index`, i.e. only when it
        // addresses a word of one of the caller's from-space ranges — the same guarantee
        // `Copier` relies on before it dereferences a reference word
        // (`Target::Live`/`Target::Forwarded` both read `*ptr`). Its first word is
        // therefore readable.
        let first = unsafe { *obj };
        // A cons cell carries no header word: its first word is the car, a term that never
        // has the `Header` primary tag (`beam_term::tag::is_header`). That frozen shape test
        // is the one `Copier::complete` and the mark-region `scan_young` use.
        let total = if is_header(first) {
            move_size(first, obj)
        } else {
            2
        };
        words += total;
        lines += lines_for(total);

        // SAFETY: `obj` addresses a live object of exactly `total` words, all readable and
        // none of them written (this walk is read-only), so the canonical child-slot
        // enumeration is covered by this block. `for_each_ref` yields the address of a live
        // reference word inside the object `obj`, so reading it through `*slot` is covered
        // too.
        let walk: Result<()> = unsafe {
            for_each_ref(obj, total, &mut |slot| {
                let term = *slot;
                if matches!(primary_tag(term), PrimaryTag::Boxed | PrimaryTag::List) {
                    admit(ptr_val(term) as *const Eterm, &mut visited, &mut work);
                }
                Ok(())
            })
        };
        // The visit closure above is infallible, so the enumeration cannot fail; the
        // accumulator exists only because the shared primitive's signature is fallible.
        debug_assert!(walk.is_ok(), "the live counter's visit closure never fails");
    }

    Live { words, lines }
}

/// Counts the graph a **mark-region** fullsweep evacuates: the nursery's live region, plus
/// the whole span of the mark-region mature space (that space's own `contains` covers its
/// free lines as well, so its from-range is `base .. base + capacity`).
pub(crate) fn live_full(nursery: &HeapSpace, mature: &ImmixSpace, roots: &[Eterm]) -> Live {
    let mut from = FromSet::new();
    from.add(nursery);
    let base = mature.base() as usize;
    from.add_range(base, base + mature.capacity_words() * size_of::<Eterm>());
    live(&from, roots)
}

/// Admits `ptr` into the worklist when it names a from-space object that has not been counted
/// yet — a reference outside every range (a literal, a foreign space, an off-heap pointer) is
/// skipped, exactly as the copier's forwarding switch leaves its `Target::Foreign` alone.
fn admit(ptr: *const Eterm, visited: &mut Visited, work: &mut Vec<*mut Eterm>) {
    if let Some(index) = visited.index(ptr) {
        if visited.test_and_set(index) {
            work.push(ptr as *mut Eterm);
        }
    }
}

/// Seeds the worklist with `term` when it is a reference into a from-space range.
fn seed(term: Eterm, visited: &mut Visited, work: &mut Vec<*mut Eterm>) {
    if matches!(primary_tag(term), PrimaryTag::Boxed | PrimaryTag::List) {
        admit(ptr_val(term) as *const Eterm, visited, work);
    }
}

/// The visited set: **one bit per word** of every from-space range, so an object is counted
/// exactly once however many references reach it (a shared subterm, a cycle). Keying on the
/// object's first word is the same identity the copier's in-place forwarding marker uses, so
/// the two dedup in lockstep.
struct Visited {
    /// `(lo, hi, first_bit)` per range, `lo`/`hi` in byte addresses and `first_bit` the bit
    /// index the range starts at — the ranges are packed into one bit vector.
    ranges: Vec<(usize, usize, usize)>,
    bits: Vec<u64>,
}

impl Visited {
    /// Indexes the ranges of `from` (`from.ranges()` is byte-addressed; a range added from a
    /// space's live region spans a whole number of term words).
    fn new(from: &FromSet) -> Visited {
        let word = size_of::<Eterm>();
        let mut ranges = Vec::with_capacity(from.ranges().len());
        let mut total = 0usize;
        for &(lo, hi) in from.ranges() {
            ranges.push((lo, hi, total));
            total += (hi - lo) / word;
        }
        Visited {
            ranges,
            bits: vec![0; total.div_ceil(64)],
        }
    }

    /// The bit index of the word `ptr` addresses, or `None` when it lies outside every range.
    fn index(&self, ptr: *const Eterm) -> Option<usize> {
        let addr = ptr as usize;
        self.ranges.iter().find_map(|&(lo, hi, first)| {
            (lo <= addr && addr < hi).then(|| first + (addr - lo) / size_of::<Eterm>())
        })
    }

    /// Sets the bit at `index`, reporting whether it was previously clear (a newly seen
    /// object).
    fn test_and_set(&mut self, index: usize) -> bool {
        let word = index / 64;
        let mask = 1u64 << (index % 64);
        let newly_seen = self.bits[word] & mask == 0;
        self.bits[word] |= mask;
        newly_seen
    }
}

#[cfg(test)]
mod tests {
    //! The counter's tests, and the **invariant that makes an under-count impossible**: for
    //! the same from-set and roots, [`live`] must report exactly the `GcStats::words_copied`
    //! an actual evacuation reports (and [`Live::lines`] the exact per-object line need).
    //!
    //! A counter that *over*-counts only wastes heap; one that *under*-counts would size a
    //! destination the copy cannot fit — the half-forward the fill barrier exists to
    //! prevent — so the equality is asserted on real term graphs, including the cases where
    //! a naive walk goes wrong: a shared subterm, a cycle, and a 50 000-cell list.
    //!
    //! `unsafe` below is confined to the raw writes that build the graphs (a cycle, the deep
    //! chain) and to reading them back; every block carries a `// SAFETY:` rationale.

    use beam_term::prelude::{make_small, ptr_val, NIL};

    use crate::copy::Copier;

    use super::*;

    /// A `links`-cell `[a | ...]` chain built in `space`.
    fn cons_chain(space: &mut HeapSpace, links: i64) -> Eterm {
        let mut tail = NIL;
        for i in 0..links {
            tail = space
                .heap_mut()
                .cons(make_small(i), tail)
                .expect("the chain fits the space");
        }
        tail
    }

    /// The counter must be **read-only**: it leaves the from-space range and every root word
    /// byte-identical, so a fullsweep can size itself from it right before copying.
    #[test]
    fn the_counter_does_not_touch_the_from_space() {
        let mut space = HeapSpace::new(64).expect("nursery");
        let roots = vec![
            cons_chain(&mut space, 4),
            space
                .heap_mut()
                .tuple(&[make_small(7), make_small(8)])
                .expect("tuple"),
        ];

        let mut from = FromSet::new();
        from.add(&space);
        let top = space.top();
        let used = space.used_words();
        let before = roots.clone();

        let counted = live(&from, &roots);

        assert_eq!(space.top(), top, "the bump pointer was not moved");
        assert_eq!(space.used_words(), used, "no word was allocated");
        assert_eq!(roots, before, "no root word was rewritten");
        assert!(counted.words > 0, "the roots are live words");
    }

    /// A graph with nothing live counts nothing, and an all-immediate root set is skipped
    /// whole (an immediate is `Target::Foreign` to a collection).
    #[test]
    fn immediates_and_an_empty_heap_count_nothing() {
        let space = HeapSpace::new(32).expect("nursery");

        let mut from = FromSet::new();
        from.add(&space);
        assert_eq!(live(&from, &[]), Live { words: 0, lines: 0 });
        assert_eq!(
            live(&from, &[make_small(1), make_small(2), NIL]),
            Live { words: 0, lines: 0 },
            "immediates and the empty list are never copied"
        );

        let mut to = HeapSpace::new(32).expect("to-space");
        let mut roots = [make_small(1), NIL];
        let stats = Copier::new(&from, &mut to).run(&mut roots).expect("copy");
        assert_eq!(stats.words_copied, 0, "nothing to copy");
    }

    /// The chain and tuple case: sizes and the line need come out exactly.
    #[test]
    fn a_chain_and_a_tuple_count_exactly() {
        let mut space = HeapSpace::new(64).expect("nursery");
        let chain = cons_chain(&mut space, 4); // 4 cells = 8 words
        let tuple = space
            .heap_mut()
            .tuple(&[make_small(1), chain, make_small(2)])
            .expect("tuple"); // 4 words
        let mut roots = [tuple];

        let mut from = FromSet::new();
        from.add(&space);
        let counted = live(&from, &roots);
        assert_eq!(
            counted.words, 12,
            "4 cons cells (8 words) plus a 4-word 3-tuple"
        );
        // Line need is per object: 4 one-line cons cells plus one 4-word (one-line) tuple.
        assert_eq!(counted.lines, 5);

        let mut to = HeapSpace::new(64).expect("to-space");
        let stats = Copier::new(&from, &mut to).run(&mut roots).expect("copy");
        assert_eq!(stats.words_copied, 12, "the copier agrees word for word");
        assert_eq!(stats.objects_copied, 5, "five objects");
    }

    /// A **shared subterm** is counted once, however many references name it — the same
    /// single copy the copier makes.
    #[test]
    fn a_shared_subterm_is_counted_once() {
        let mut space = HeapSpace::new(64).expect("nursery");
        let shared = space
            .heap_mut()
            .tuple(&[make_small(1), make_small(2)])
            .expect("shared 3-word tuple");
        // Two tuples that both reference `shared`: a DAG, not a tree.
        let left = space.heap_mut().tuple(&[shared, make_small(3)]).expect("left");
        let right = space.heap_mut().tuple(&[shared, make_small(4)]).expect("right");
        let mut roots = [left, right];

        let mut from = FromSet::new();
        from.add(&space);
        let counted = live(&from, &roots);
        // 3 (left) + 3 (right) + 3 (shared, once) = 9, not 12.
        assert_eq!(counted.words, 9, "the shared tuple contributes its words once");

        let mut to = HeapSpace::new(64).expect("to-space");
        let stats = Copier::new(&from, &mut to).run(&mut roots).expect("copy");
        assert_eq!(stats.words_copied, 9, "the copier copies it once too");
        assert_eq!(stats.objects_copied, 3, "three distinct objects");
    }

    /// A **cycle** terminates and counts each object once: a collection copies it once
    /// (forwarding markers close the loop), and so must the counter (the bitset does).
    #[test]
    fn a_cycle_is_counted_once_and_terminates() {
        let mut space = HeapSpace::new(64).expect("nursery");
        // A two-cell cycle: `a.cdr = b`, `b.cdr = a`.
        let a = space.heap_mut().cons(make_small(1), NIL).expect("cell a");
        let b = space.heap_mut().cons(make_small(2), a).expect("cell b");
        // Close the cycle through raw writes (no allocator API makes a cycle).
        // SAFETY: `a` names a live 2-word cons cell this test just allocated in `space`, and
        // its second word is the cdr slot; the write stays inside the cell.
        unsafe {
            *ptr_val(a).add(1) = b;
        }

        let mut roots = [a];
        let mut from = FromSet::new();
        from.add(&space);
        let counted = live(&from, &roots);
        assert_eq!(counted.words, 4, "two cons cells, each counted once");
        assert_eq!(counted.lines, 2, "one line per cell");

        let mut to = HeapSpace::new(64).expect("to-space");
        let stats = Copier::new(&from, &mut to).run(&mut roots).expect("copy");
        assert_eq!(
            stats.words_copied, 4,
            "the copier also copies each cell once"
        );
        assert_eq!(stats.objects_copied, 2, "two cells");
    }

    /// The counter is **iterative**: a 50 000-cell list (the shape
    /// `tests/collector_adversarial.rs` uses to prove the copier is iterative) is counted
    /// without recursing — a recursive walk would overflow the test thread's stack.
    #[test]
    fn a_fifty_thousand_cell_list_does_not_recurse() {
        const CELLS: usize = 50_000;
        let mut space = HeapSpace::new(2 * CELLS + 64).expect("nursery");
        let head = cons_chain(&mut space, CELLS as i64);
        assert_eq!(space.used_words(), 2 * CELLS, "one cell is two words");

        let mut from = FromSet::new();
        from.add(&space);
        let counted = live(&from, &[head]);
        assert_eq!(counted.words, 2 * CELLS, "every cell is live");
        assert_eq!(counted.lines, CELLS, "one line per cell");

        let mut to = HeapSpace::new(2 * CELLS + 64).expect("to-space");
        let mut roots = [head];
        let stats = Copier::new(&from, &mut to).run(&mut roots).expect("copy");
        assert_eq!(stats.words_copied, 2 * CELLS, "the copier agrees");
    }

    /// References **out of the from-set** are skipped: a term that lives in another heap (a
    /// literal-like foreign space) is never counted, exactly as the copier leaves it
    /// `Foreign`.
    #[test]
    fn references_outside_the_from_set_are_skipped() {
        let mut other = HeapSpace::new(64).expect("a foreign space");
        let foreign = other
            .heap_mut()
            .tuple(&[make_small(5), make_small(6)])
            .expect("foreign tuple");

        let mut space = HeapSpace::new(64).expect("the evacuated space");
        let wrapper = space
            .heap_mut()
            .tuple(&[foreign, make_small(9)])
            .expect("wrapper");
        let mut roots = [wrapper];

        let mut from = FromSet::new();
        from.add(&space); // NOT `other`
        let counted = live(&from, &roots);
        assert_eq!(
            counted.words, 3,
            "only the 3-word wrapper: the foreign tuple is outside the set"
        );

        let mut to = HeapSpace::new(64).expect("to-space");
        let stats = Copier::new(&from, &mut to).run(&mut roots).expect("copy");
        assert_eq!(stats.words_copied, 3, "the copier leaves it verbatim");
    }

    /// The **invariant on a rich graph**: a mixture of tuples, chains, a shared subterm and a
    /// cycle, where the counter's word count must equal the collection's `words_copied` and
    /// its line count the collection's per-object line need.
    #[test]
    fn the_counter_equals_the_collection_on_a_mixed_graph() {
        let mut other = HeapSpace::new(64).expect("a foreign space");
        let foreign = other
            .heap_mut()
            .tuple(&[make_small(7), make_small(7)])
            .expect("foreign tuple");

        let mut space = HeapSpace::new(128).expect("nursery");
        let shared = space
            .heap_mut()
            .tuple(&[make_small(1), make_small(2)])
            .expect("shared");
        // A shared cell referenced from the roots *and* from itself (a cycle).
        let cell = space.heap_mut().cons(shared, NIL).expect("cell");
        // SAFETY: `cell` names a live 2-word cons cell in `space`; word 1 is its cdr slot.
        unsafe {
            *ptr_val(cell).add(1) = cell;
        }
        let big = space
            .heap_mut()
            .tuple(&[make_small(0); 20])
            .expect("a 21-word tuple");
        let mut roots = vec![
            shared,
            cell,
            shared, // the same object twice in the root set
            big,
            space
                .heap_mut()
                .tuple(&[foreign, make_small(3)])
                .expect("wrapper"),
            cons_chain(&mut space, 3),
            make_small(42), // an immediate
        ];

        let mut from = FromSet::new();
        from.add(&space);
        let counted = live(&from, &roots);

        let mut to = HeapSpace::new(space.capacity_words() + 64).expect("to-space");
        let stats = Copier::new(&from, &mut to).run(&mut roots).expect("copy");
        assert_eq!(
            counted.words, stats.words_copied,
            "the counter and the collection must agree word for word"
        );
        assert_eq!(
            stats.objects_copied, 7,
            "shared, cell, big, wrapper and three chain cells"
        );
        assert_eq!(
            counted.lines, 8,
            "one line each for the shared tuple, the cell, the wrapper and the three chain \
             cells, two for the 21-word tuple"
        );
    }

    /// The mark-region variant ([`live_full`]) counts the same live set out of a
    /// `GenImmixCollector`'s two spaces, and a real promotion copies exactly those words.
    #[test]
    fn live_full_matches_an_immix_promotion() {
        use crate::immix_collector::GenImmixCollector;
        use crate::policy::HeapPolicy;

        let policy = HeapPolicy {
            initial_young_words: 128,
            initial_mature_words: 256,
            ..HeapPolicy::default()
        };
        let mut collector = GenImmixCollector::from_policy(policy).expect("collector");
        let mut roots = Vec::new();
        for i in 0..8i64 {
            roots.push(
                collector
                    .young_mut()
                    .heap_mut()
                    .tuple(&[make_small(i), make_small(2 * i), make_small(3 * i)])
                    .expect("tuple"),
            );
        }

        let counted = live_full(collector.young(), collector.mature(), &roots);
        assert_eq!(counted.words, 8 * 4, "8 four-word tuples");
        assert_eq!(counted.lines, 8, "one line per tuple");

        let stats = collector.collect_minor(&mut roots).expect("promote");
        assert_eq!(
            counted.words, stats.words_copied,
            "the promotion copied exactly the counted words"
        );
        assert_eq!(
            collector.mature().used_lines(),
            counted.lines,
            "the promotion occupies exactly the counted lines"
        );
    }

    // -----------------------------------------------------------------------
    // Mature-generation exactness (`live_full`) and the off-heap corpus
    // -----------------------------------------------------------------------
    //
    // The tests above pin `live` (the copying fullsweep's counter). The ones below pin
    // `live_full` — the counter a [`GenImmixCollector`] fullsweep sizes itself from — against
    // a real collection through **both** of its mature paths (an in-place reclaim and a
    // relocating resize), and pin that an **off-heap** payload is never a heap word: a refc
    // binary is its 8-word on-heap pair whatever the payload's size.

    use beam_alloc::RefcBinary;
    use beam_term::prelude::{
        fun_words, is_refc_binary, map_words_capacity, refc_bytes, refc_size, size_object,
        write_fun_local, write_map, write_refc_binary, AtomTable, FunKind, ERL_REFC_BITS_SIZE,
    };

    use crate::immix_collector::GenImmixCollector;
    use crate::policy::HeapPolicy;

    /// A mark-region collector with the requested young/mature capacities (both rounded up to
    /// whole Immix blocks) and the default policy everywhere else.
    fn immix(young: usize, mature: usize) -> GenImmixCollector {
        let policy = HeapPolicy {
            initial_young_words: young,
            initial_mature_words: mature,
            ..HeapPolicy::default()
        };
        GenImmixCollector::from_policy(policy).expect("a mark-region collector")
    }

    /// Writes the frozen 8-word `[BinRef][ErlSubBits]` off-heap handle for `payload` at the top
    /// of `c`'s nursery and returns its root (the `ErlSubBits`).
    fn refc_handle(c: &mut GenImmixCollector, payload: &RefcBinary) -> Eterm {
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

    /// Runs the fullsweep and asserts the counter sized it **exactly**: the words the promotion
    /// copied equal [`Live::words`] and the lines it occupies equal [`Live::lines`]. The caller
    /// must have built the corpus in the nursery of a collector whose mature generation is
    /// still empty, so the whole live set is what the fullsweep copies.
    fn assert_major_matches(c: &mut GenImmixCollector, counted: Live, roots: &mut [Eterm]) {
        assert_eq!(
            c.mature().used_lines(),
            0,
            "the corpus is built in the nursery: the mature generation must start empty"
        );
        let stats = c.collect_major(roots).expect("a fullsweep");
        assert_eq!(
            counted.words, stats.words_copied,
            "the promotion must copy exactly the counted words"
        );
        assert_eq!(
            counted.lines,
            c.mature().used_lines(),
            "the promotion must occupy exactly the counted lines"
        );
    }

    /// One-line (3-word) and multi-line (17-word) objects: the word count is the frozen-layout
    /// footprint and the line count the per-object [`lines_for`] sum — and a destination sized
    /// from the word count alone would be **short** of the lines the copy needs, so the `lines`
    /// field is load-bearing.
    #[test]
    fn live_full_sizes_one_line_and_multi_line_objects_exactly() {
        let mut c = immix(4096, 512); // 2 whole blocks = 32 lines
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

        let counted = live_full(c.young(), c.mature(), &roots);
        assert_eq!(counted.words, 12 * 3 + 4 * 17, "twelve 3-word and four 17-word tuples");
        assert_eq!(counted.lines, 12 + 4 * 2, "one line each, two for the 17-word tuples");
        assert_eq!(
            counted.words,
            roots.iter().map(|&t| size_object(t)).sum::<usize>(),
            "the counter must equal the frozen-layout footprint"
        );
        // `.lines` is not merely `lines_for(words)`: 104 words round to 7 lines, but the 16
        // objects need 20 (line-granular allocation charges one line per object), so a region
        // sized from `.words` alone would be short by 13 lines.
        assert_eq!(lines_for(counted.words), 7);
        assert!(
            lines_for(counted.words) < counted.lines,
            "sizing from the word count alone ({} -> {} lines) is short of the {} the copy needs",
            counted.words,
            lines_for(counted.words),
            counted.lines
        );

        assert_major_matches(&mut c, counted, &mut roots);
    }

    /// A **shared subterm** and a **two-cell cycle** are counted once each, and the fullsweep
    /// copies exactly that (the bitset dedup matches the copier's in-place markers).
    #[test]
    fn live_full_counts_a_shared_subterm_and_a_cycle_once() {
        let mut c = immix(4096, 256); // one block = 16 lines
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
        let a = c.young_mut().heap_mut().cons(make_small(5), NIL).expect("cell a");
        let b = c.young_mut().heap_mut().cons(make_small(6), a).expect("cell b");
        // Close the cycle with a raw write (no allocator API makes a cycle).
        // SAFETY: `a` names a live 2-word cons cell this test allocated in the nursery, and
        // word 1 is its cdr slot; the write stays inside the cell.
        unsafe {
            *ptr_val(a).add(1) = b;
        }

        let mut roots = vec![left, right, a, shared, b];
        let counted = live_full(c.young(), c.mature(), &roots);
        assert_eq!(
            counted.words,
            3 + 3 + 3 + 2 + 2,
            "shared 3, left 3, right 3 and the two cells"
        );
        assert_eq!(counted.lines, 5, "one line per object");

        assert_major_matches(&mut c, counted, &mut roots);
    }

    /// A fullsweep whose policy target differs from the current capacity **relocates** the whole
    /// live set into a fresh region; the counter sizes that copy exactly too.
    #[test]
    fn live_full_matches_a_relocating_immix_major() {
        // A mature generation far larger than the live set: `mature_after_fullsweep` shrinks it,
        // so the fullsweep takes the relocating path.
        let mut c = immix(4096, 4096);
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

        let counted = live_full(c.young(), c.mature(), &roots);
        assert_eq!((counted.words, counted.lines), (104, 20));

        assert_eq!(c.mature().used_lines(), 0);
        let before = c.mature().capacity_words();
        let stats = c.collect_major(&mut roots).expect("a relocating fullsweep");
        assert_ne!(
            c.mature().capacity_words(),
            before,
            "the policy must have resized (relocated) the mature region"
        );
        assert_eq!(
            counted.words, stats.words_copied,
            "the relocation must copy exactly the counted words"
        );
        assert_eq!(
            counted.lines,
            c.mature().used_lines(),
            "the relocation must occupy exactly the counted lines"
        );
    }

    /// A `>32`-key HAMT map (head/internal/collision nodes) plus two funs: the counter equals the
    /// frozen-layout footprint and sizes the fullsweep exactly.
    #[test]
    fn live_full_counts_a_hamt_map_and_funs_exactly() {
        let mut c = immix(1 << 16, 1 << 15);
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
        // SAFETY: `fun_words(Local, 2) == 5` exclusively-owned words are reserved and the two
        // free variables are immediates, so the whole block is written before it escapes.
        let fun = unsafe {
            let hp = c
                .young_mut()
                .heap_mut()
                .alloc_words(fun_words(FunKind::Local, 2))
                .expect("the fun reservation");
            write_fun_local(hp, 2, 0x1234_5678_9ABC, 7, &[make_small(1), make_small(2)])
                .expect("a local fun")
        };
        assert_eq!(size_object(fun), fun_words(FunKind::Local, 2));

        let mut roots = vec![map, fun];
        let counted = live_full(c.young(), c.mature(), &roots);
        assert_eq!(
            counted.words,
            size_object(map) + size_object(fun),
            "the counter must equal the frozen-layout footprint of the HAMT map plus the fun"
        );

        assert_major_matches(&mut c, counted, &mut roots);
    }

    /// An **off-heap-heavy** graph: the counter counts the 8-word on-heap handle of a refc binary,
    /// never the off-heap payload — so it equals the frozen on-heap footprint however large the
    /// payload is, and sizes the fullsweep exactly.
    #[test]
    fn live_full_counts_only_the_on_heap_half_of_a_refc_binary() {
        let payloads: Vec<RefcBinary> = (0..6)
            .map(|i| RefcBinary::new(&vec![i as u8; 64 * 1024]).expect("an off-heap payload"))
            .collect();

        let mut c = immix(4096, 256);
        let mut roots = Vec::new();
        for payload in &payloads {
            roots.push(refc_handle(&mut c, payload));
        }
        roots.push(
            c.young_mut()
                .heap_mut()
                .tuple(&[make_small(0), make_small(7)])
                .expect("a holder tuple"),
        );

        let counted = live_full(c.young(), c.mature(), &roots);
        assert_eq!(counted.words, 6 * 8 + 3, "six 8-word handles plus a 3-word tuple");
        assert_eq!(counted.lines, 6 * 2 + 1, "two lines per pair, one for the holder");

        assert_major_matches(&mut c, counted, &mut roots);

        // 6 * 64 KiB of off-heap payload contributed exactly zero heap words, and the payload is
        // byte-identical through the move.
        for (i, root) in roots.iter().take(6).enumerate() {
            // SAFETY: `*root` is the live, promoted `ErlSubBits` of pair `i`, whose off-heap
            // payload `payloads[i]` is alive for the whole test.
            unsafe {
                assert!(is_refc_binary(*root), "pair {i}");
                assert_eq!(refc_size(*root), payloads[i].len() * 8, "pair {i}: the window");
                assert_eq!(
                    refc_bytes(*root),
                    payloads[i].as_bytes(),
                    "pair {i}: the off-heap bytes are untouched"
                );
            }
        }
    }

    /// The copying counter ([`live`]) leaves an off-heap payload out as well: a refc binary is
    /// exactly its 8-word on-heap pair, whatever the payload's size.
    #[test]
    fn the_counter_ignores_the_off_heap_payload() {
        for len in [1usize, 4096, 64 * 1024] {
            let payload = RefcBinary::new(&vec![0xABu8; len]).expect("a payload");
            let mut space = HeapSpace::new(64).expect("nursery");
            // SAFETY: exactly `ERL_REFC_BITS_SIZE` (8) exclusively-owned words are reserved;
            // `payload.data_ptr()` is a live off-heap pointer kept alive for this iteration, and
            // `null` is the empty chain link.
            let handle = unsafe {
                let hp = space
                    .heap_mut()
                    .alloc_words(ERL_REFC_BITS_SIZE)
                    .expect("8 words");
                write_refc_binary(
                    hp,
                    payload.data_ptr() as *mut Eterm,
                    core::ptr::null_mut(),
                    payload.data_ptr(),
                    0,
                    len * 8,
                )
            };

            let mut roots = [handle];
            let mut from = FromSet::new();
            from.add(&space);
            let counted = live(&from, &roots);
            assert_eq!(counted.words, 8, "len = {len}: only the on-heap pair is counted");
            assert_eq!(counted.lines, 2, "len = {len}: one line per pair object");

            let mut to = HeapSpace::new(8).expect("to-space");
            let stats = Copier::new(&from, &mut to).run(&mut roots).expect("copy");
            assert_eq!(stats.words_copied, counted.words, "len = {len}: the copier agrees");
            assert_eq!(stats.objects_copied, 2, "len = {len}: the SubBits and its BinRef");
        }
    }
}
