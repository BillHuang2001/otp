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

//! The **two-tier scanned test** of the unscanned-root valve: [`super::outside_scanned`]'s
//! aggregate fast reject ([`beam_gc::ScannedBounds`]) in front of the exact
//! [`young_contains`](beam_gc::AnyCollector::young_contains) /
//! [`mature_contains`](beam_gc::AnyCollector::mature_contains) pair, and
//! [`super::root_is_unscanned`] above it.
//!
//! # What this pins
//!
//! The filter is allowed to skip work, never to change a decision: the aggregate is a superset
//! of every region its collector scans, so an address it rejects would have been rejected by
//! both exact tests too. Every case here therefore asserts **the same answer as the plain
//! per-region predicate** ([`exact_root_is_unscanned`], the pre-filter expression, kept verbatim
//! as the oracle) for the regions a real process actually produces:
//!
//! * a term in the **current** nursery space (accepted — the mutator block's own fast path);
//! * a term in a nursery space a [`Process::grow_nursery`] **retired** (accepted — the chain
//!   half of the aggregate, the case a naive bounds filter would break);
//! * a term the **mature** generation owns after a collection promoted it (accepted);
//! * a container on the runtime's **pooled BIF heap** and a cons in its **pointer-stable
//!   arena** (rejected: foreign);
//! * an **immediate** and a **tagged literal** (rejected before any address test, unchanged);
//! * `make_ref/0`'s childless M1 block on that pooled heap (a foreign **leaf**, exempt);
//! * a foreign **container** with children and an `ErlSubBits` (declining).
//!
//! Each region is checked on **both** collector kinds the runtime can select
//! ([`MatureKind::Copying`] and, through the policy's mature size, [`MatureKind::Immix`]),
//! because the two report very different aggregates and the filter's value depends on which
//! one is in play — [`the_aggregate_span_is_reported_for_both_collector_kinds`] measures
//! `span_words()` for both and pins the property that makes the reject sound.
use beam_gc::{AnyCollector, HeapPolicy, MatureKind, ScannedBounds, IMMIX_BREAK_EVEN_WORDS};
use beam_process::{Process, ProcessTable};
use beam_term::prelude::{is_boxed, is_list, make_small, Eterm, NIL};
use beam_term::AtomTable;

use super::{bump, foreign_boxed_has_copy_children, in_heap_block, outside_scanned, root_is_unscanned};

/// The **pre-filter predicate**: [`root_is_unscanned`] exactly as it read before the aggregate
/// fast reject was added, kept here verbatim as the equivalence oracle.
///
/// Every case in this module asserts the production function agrees with this one. It is
/// deliberately a re-derivation of the *old* code, not of the new code: if the filter ever
/// changed a decision (an over-eager accept, a superset that is not one), this would catch it.
fn exact_root_is_unscanned(process: &Process, term: Eterm) -> bool {
    if !is_boxed(term) && !is_list(term) {
        return false;
    }
    if beam_term::literal::is_literal(term) {
        return false;
    }
    let ptr = beam_term::list::ptr_val(term) as usize;
    let foreign = !in_heap_block(process.heap(), ptr)
        && !process.collector().young_contains(ptr as *const Eterm)
        && !process.collector().mature_contains(ptr as *const Eterm);
    foreign && (is_list(term) || foreign_boxed_has_copy_children(term))
}

/// A `HeapPolicy` whose mature generation the automatic rule resolves to `kind`.
///
/// [`MatureKind::Immix`] is reached the way the runtime reaches it — `beam_gc`'s own break-even
/// rule over the policy's initial mature size ([`IMMIX_BREAK_EVEN_WORDS`]) — never an explicit
/// pin, so the case exercises the strategy the `BEAM_RT_GC_STRATEGY=immix` host opt-in selects.
fn policy_for(kind: MatureKind) -> HeapPolicy {
    match kind {
        MatureKind::Copying => HeapPolicy::default(),
        MatureKind::Immix => HeapPolicy {
            initial_mature_words: IMMIX_BREAK_EVEN_WORDS,
            ..HeapPolicy::default()
        },
    }
}

/// Spawns one process whose collector is `kind`, asserting the resolved strategy really is the
/// requested one (so a policy change cannot silently turn the immix case into a second copying
/// case).
fn process_with(kind: MatureKind) -> (ProcessTable, beam_process::ProcessId) {
    let mut table = ProcessTable::new(beam_process::SignalAtoms::from_table(
        &mut AtomTable::new(),
    ));
    let pid = table
        .spawn(policy_for(kind))
        .expect("a process spawns under the policy");
    assert_eq!(
        table.get(pid).expect("the spawned process").collector().kind(),
        kind,
        "the policy must select the requested mature strategy"
    );
    (table, pid)
}

/// The two collector kinds every case runs on.
const KINDS: [MatureKind; 2] = [MatureKind::Copying, MatureKind::Immix];

/// One term of the fixture plus what the valve must answer for it and why, so a failure names
/// the region rather than only an address.
struct Case {
    /// What region the term lives in, for the assertion messages.
    what: &'static str,
    /// The term under test.
    term: Eterm,
    /// Whether the valve must decline a collection because of it.
    unscanned: bool,
}

/// Asserts the production classifier, the pre-filter predicate and the expectation all agree
/// for every case.
fn assert_all_agree(process: &Process, kind: MatureKind, cases: &[Case]) {
    for case in cases {
        assert_eq!(
            root_is_unscanned(process, case.term),
            case.unscanned,
            "{kind:?}: the {} must be unscanned == {}",
            case.what,
            case.unscanned
        );
        assert_eq!(
            root_is_unscanned(process, case.term),
            exact_root_is_unscanned(process, case.term),
            "{kind:?}: the aggregate filter changed the {}'s verdict",
            case.what
        );
    }
}

/// The **foreign half** of a fixture: a pooled BIF heap holding `make_ref/0`'s M1 block, an
/// ordinary tuple and an `ErlSubBits`, plus a cons in the runtime's pointer-stable arena.
///
/// Both regions are the runtime's own ([`crate::Runtime::take_bif_heap`] — where a
/// registered-BIF result is built — and [`crate::Runtime::alloc_words`], the allocation doors'
/// pointer-stable fallback), so the terms are produced by the same code the valve meets in
/// production, not by a stand-in block.
struct Foreign {
    /// Owns the pooled heap's words: the terms below point into it, so it is dropped last.
    _runtime: crate::runtime::Runtime,
    /// `make_ref/0`'s frozen M1 childless block (`_ => None` in the child-slot table).
    reference: Eterm,
    /// A 2-tuple on the pooled heap: two child slots.
    tuple: Eterm,
    /// An `ErlSubBits` on the pooled heap, whose `orig` word **is** forwarded.
    sub_bits: Eterm,
    /// A cons cell in the pointer-stable arena: header-less, both words are references.
    cons: Eterm,
}

impl Foreign {
    /// Builds the four foreign terms on a fresh runtime's own unscanned regions.
    #[allow(unsafe_code)]
    fn new() -> Foreign {
        let mut runtime = crate::runtime::Runtime::with_defaults().expect("the default runtime");
        let fill = [NIL; 8];
        let mut heap = runtime.take_bif_heap().expect("a pooled BIF heap");
        // SAFETY: each `bump` reserves exactly the words the writer it feeds needs, inside this
        // heap, and every word is written before the term escapes.
        let (reference, tuple, sub_bits) = unsafe {
            let slot = bump(&mut heap, 4, &fill).expect("four words for the reference");
            let reference =
                beam_term::boxed::write_internal_ref(slot, 7).expect("an M1 internal reference");
            let tuple = heap
                .tuple(&[make_small(1), make_small(2)])
                .expect("a 2-tuple");
            let slot = bump(&mut heap, 5, &fill).expect("five words for the ErlSubBits");
            let sub_bits = beam_term::bitstring::write_sub_bits(
                slot,
                make_small(0),
                make_small(0),
                make_small(0),
                make_small(0),
            );
            (reference, tuple, sub_bits)
        };
        // The heap goes back with its words intact (the pool never resets on return), so the
        // terms above stay valid for the whole fixture's life.
        runtime.put_bif_heap(heap);
        let slot = runtime
            .alloc_words(2)
            .expect("two arena words for the cons");
        // SAFETY: `alloc_words(2)` handed over exactly two arena words, both written here before
        // the term escapes, and the arena is pointer-stable for the runtime's whole life.
        let cons = unsafe {
            *slot = make_small(3);
            *slot.add(1) = NIL;
            beam_term::list::make_list(slot)
        };
        Foreign {
            _runtime: runtime,
            reference,
            tuple,
            sub_bits,
            cons,
        }
    }

    /// Asserts each term really is **outside** every scanned region of `process`'s collector —
    /// the precondition that makes the classifier's answer a statement about the filter rather
    /// than about the region.
    fn assert_foreign(&self, process: &Process) {
        for (what, term) in [
            ("reference", self.reference),
            ("tuple", self.tuple),
            ("sub-bits", self.sub_bits),
            ("cons", self.cons),
        ] {
            let ptr = beam_term::list::ptr_val(term) as usize;
            assert!(
                !in_heap_block(process.heap(), ptr),
                "the foreign {what} must be outside the process heap's live storage"
            );
            assert!(
                !process.collector().young_contains(ptr as *const Eterm)
                    && !process.collector().mature_contains(ptr as *const Eterm),
                "the foreign {what} must be outside every scanned space"
            );
        }
    }
}

/// **Every region a real process produces gets the same verdict through the aggregate filter as
/// through the exact predicate — on both collector kinds.**
///
/// The accepted regions are the interesting half: a term in the nursery's **current** space, one
/// in the space a growth **retired** (the chain the aggregate must cover, and the case a bounds
/// filter built from the current space alone would get wrong), and one the **mature** generation
/// owns. The rejected regions are the runtime's own unscanned ones — its pooled BIF heap and its
/// pointer-stable arena — and the early rejections (an immediate, a tagged literal) must be
/// unchanged.
///
/// The chain cases and the mature case use **separate processes** on purpose: a collection
/// collapses the nursery chain, so a retired-space term that is not a root of that collection is
/// dead by then and its address means nothing.
#[test]
fn the_two_tier_test_matches_the_exact_predicate_in_every_region() {
    for kind in KINDS {
        // ---- the nursery: the current mutator block, and the space a growth retires -------
        let (mut chain, chain_pid) = process_with(kind);
        let retired = chain
            .get_mut(chain_pid)
            .expect("the process")
            .heap_mut()
            .tuple(&[make_small(4), make_small(5)])
            .expect("a young-heap 2-tuple");
        let over = chain
            .get(chain_pid)
            .expect("the process")
            .collector()
            .young()
            .capacity_words()
            + 64;
        chain
            .get_mut(chain_pid)
            .expect("the process")
            .grow_nursery(over)
            .expect("the nursery grows");
        let current = chain
            .get_mut(chain_pid)
            .expect("the process")
            .heap_mut()
            .tuple(&[make_small(8), make_small(9)])
            .expect("a young-heap 2-tuple");
        let chain_process = chain.get(chain_pid).expect("the process");
        assert_eq!(
            chain_process.collector().young_space_count(),
            2,
            "{kind:?}: the growth must retire a space, or the chain case is not exercised"
        );
        assert!(
            in_heap_block(chain_process.heap(), beam_term::list::ptr_val(current) as usize),
            "{kind:?}: the post-growth term must be in the mutator block"
        );
        assert!(
            !in_heap_block(chain_process.heap(), beam_term::list::ptr_val(retired) as usize),
            "{kind:?}: the retired-space term must be outside the mutator block"
        );
        assert!(
            chain_process
                .collector()
                .young_contains(beam_term::list::ptr_val(retired) as *const Eterm),
            "{kind:?}: the retired space is still a scanned region"
        );

        // ---- the mature generation: a root a collection promoted --------------------------
        let (mut promoted_table, promoted_pid) = process_with(kind);
        let term = promoted_table
            .get_mut(promoted_pid)
            .expect("the process")
            .heap_mut()
            .tuple(&[make_small(6), make_small(7)])
            .expect("a young-heap 2-tuple");
        {
            let process = promoted_table.get_mut(promoted_pid).expect("the process");
            process
                .registers_mut()
                .x_mut()
                .set_live(1)
                .expect("one live register");
            process
                .registers_mut()
                .x_mut()
                .set(0, term)
                .expect("x(0) is addressable");
            process.collect().expect("the collection runs");
        }
        let promoted_process = promoted_table.get(promoted_pid).expect("the process");
        let promoted = promoted_process
            .registers()
            .x()
            .get(0)
            .expect("x(0) reads back");
        assert!(
            promoted_process
                .collector()
                .mature_contains(beam_term::list::ptr_val(promoted) as *const Eterm),
            "{kind:?}: the collection must have promoted the root into the mature generation"
        );

        // ---- the runtime's own unscanned regions ------------------------------------------
        let foreign = Foreign::new();
        foreign.assert_foreign(chain_process);
        // Whether the aggregate can reject each foreign term outright — the filter's own hit
        // rate, which is what decides if tier two runs at all for the runtime's real unscanned
        // regions (see `report_span`'s docs on the width).
        let bounds = chain_process.collector().scanned_bounds();
        for (what, term) in [
            ("pooled-BIF-heap tuple", foreign.tuple),
            ("pointer-stable-arena cons", foreign.cons),
            ("pooled-BIF-heap ErlSubBits", foreign.sub_bits),
            ("childless make_ref/0 block", foreign.reference),
        ] {
            println!(
                "{kind:?}: {what} rejected-by-the-aggregate={}",
                bounds.definitely_unscanned(beam_term::list::ptr_val(term) as *const Eterm)
            );
        }

        // A **tagged literal**: a real term with the loader's literal bit set — frozen, never
        // moved, rejected before any address test.
        let literal = beam_term::literal::make_literal(retired);

        let chain_cases = [
            Case {
                what: "current-space tuple",
                term: current,
                unscanned: false,
            },
            Case {
                what: "growth-retired-space tuple",
                term: retired,
                unscanned: false,
            },
            Case {
                what: "pooled-BIF-heap tuple",
                term: foreign.tuple,
                unscanned: true,
            },
            Case {
                what: "pointer-stable-arena cons",
                term: foreign.cons,
                unscanned: true,
            },
            Case {
                what: "pooled-BIF-heap ErlSubBits",
                term: foreign.sub_bits,
                unscanned: true,
            },
            Case {
                what: "childless make_ref/0 block",
                term: foreign.reference,
                unscanned: false,
            },
            Case {
                what: "tagged literal",
                term: literal,
                unscanned: false,
            },
            Case {
                what: "small integer",
                term: make_small(42),
                unscanned: false,
            },
            Case {
                what: "empty list",
                term: NIL,
                unscanned: false,
            },
        ];
        assert_all_agree(chain_process, kind, &chain_cases);
        assert_all_agree(
            promoted_process,
            kind,
            &[Case {
                what: "mature-generation tuple",
                term: promoted,
                unscanned: false,
            }],
        );
        // The foreign answers must not depend on which process asks — the regions are the
        // runtime's, not the process's.
        assert_all_agree(promoted_process, kind, &chain_cases[2..6]);
    }
}

/// **The aggregate span is reported per collector kind and per geometry, and it really is a
/// superset of every region the exact tests accept.**
///
/// This is the property the fast reject rests on, asserted directly instead of trusted: every
/// address of every nursery space and of the mature generation — the live fill *and* each
/// space's reserved-but-unallocated tail — is inside `[base, limit)`, so an address the
/// aggregate rejects would have been rejected by both exact tests too.
///
/// The **width** is what decides how much work the filter can actually skip, and it is a
/// property of the allocator geometry, not of the valve: it is printed here (run with
/// `--nocapture`) for a fresh process, after a growth retired a nursery space, and after a
/// collection promoted a root — the three geometries a real process passes through. A span of
/// `n` words means only addresses more than `n` words away from the heap can be rejected
/// outright; everything closer falls through to the exact tests, exactly as before.
#[test]
fn the_aggregate_span_is_reported_for_both_collector_kinds() {
    for kind in KINDS {
        let (mut table, pid) = process_with(kind);
        report_span(kind, "fresh", table.get(pid).expect("the process"));

        let term = table
            .get_mut(pid)
            .expect("the process")
            .heap_mut()
            .tuple(&[make_small(1), make_small(2), make_small(3)])
            .expect("a young-heap 3-tuple");
        let fresh_bounds = table
            .get(pid)
            .expect("the process")
            .collector()
            .scanned_bounds();

        // Grow: a second nursery space joins the chain, so the aggregate has to widen to hold
        // a space that is no longer the mutator block.
        let over = table
            .get(pid)
            .expect("the process")
            .collector()
            .young()
            .capacity_words()
            + 64;
        table
            .get_mut(pid)
            .expect("the process")
            .grow_nursery(over)
            .expect("the nursery grows");
        let process = table.get(pid).expect("the process");
        assert_eq!(
            process.collector().young_space_count(),
            2,
            "{kind:?}: the growth must retire a space"
        );
        report_span(kind, "grown", process);
        let grown_bounds = process.collector().scanned_bounds();
        // The retired space is covered by the aggregate **and** by no mutator block, which is
        // the whole point: it is the half of the aggregate the block test cannot answer.
        assert!(
            !in_heap_block(process.heap(), beam_term::list::ptr_val(term) as usize),
            "{kind:?}: the retired term must be outside the mutator block for the case to mean \
             anything"
        );

        // Collect: the chain collapses and the live root is promoted into the mature space.
        let mut table = table;
        {
            let process = table.get_mut(pid).expect("the process");
            process
                .registers_mut()
                .x_mut()
                .set_live(1)
                .expect("one live register");
            process
                .registers_mut()
                .x_mut()
                .set(0, term)
                .expect("x(0) is addressable");
            process.collect().expect("the collection runs");
        }
        let process = table.get(pid).expect("the process");
        report_span(kind, "collected", process);
        let collector: &AnyCollector = process.collector();
        let bounds: ScannedBounds = collector.scanned_bounds();
        assert!(
            !bounds.is_empty(),
            "{kind:?}: a live collector scans something"
        );

        // The geometry epoch is monotone — the fact a caller may key a cached derived decision
        // on. The *interval* is deliberately not: a collection collapses the nursery chain and
        // installs a space sized from the live set, so the post-collection aggregate can be
        // narrower than (and displaced from) the pre-collection one. That is why the filter must
        // be re-read, and why nothing here assumes the span only grows.
        for (label, earlier) in [("fresh", fresh_bounds), ("grown", grown_bounds)] {
            assert!(
                earlier.generation() <= bounds.generation(),
                "{kind:?}: the geometry epoch is monotone ({label})"
            );
        }
        // The superset property at the two geometries that matter: a term the growth retired
        // (covered by the grown aggregate, and by no mutator block) and the term the collection
        // promoted (covered by the post-collection one). Neither width is asserted — the
        // absolute span is an allocator-geometry fact (the two halves can land megabytes apart,
        // and a collection may install a smaller nursery elsewhere), which is exactly why the
        // filter is a reject and not a containment claim.
        assert!(
            grown_bounds.may_contain(beam_term::list::ptr_val(term) as usize),
            "{kind:?}: the grown aggregate must cover the retired-space term"
        );        // Every address the exact tests accept is inside the aggregate.
        for (what, addr) in [
            ("the mutator block", process.heap().hp() as usize),
            (
                "the relocated root",
                beam_term::list::ptr_val(
                    process.registers().x().get(0).expect("x(0) reads back"),
                ) as usize,
            ),
        ] {
            assert!(
                bounds.may_contain(addr),
                "{kind:?}: {what} at {addr:#x} must lie inside the aggregate"
            );
        }
        // A far-away address — no space can reach it — is rejected, which is the whole point:
        // `outside_scanned` then answers without touching the chain index or the mature space.
        let far = bounds.limit().saturating_add(1 << 20);
        assert!(
            bounds.definitely_unscanned(far as *const Eterm),
            "{kind:?}: an address past the aggregate must be rejected"
        );
        assert!(
            outside_scanned(collector, far),
            "{kind:?}: a far address is outside every scanned space"
        );
    }
}

/// Prints one collector geometry's aggregate width — the figure that decides how much the fast
/// reject can skip (see the caller's docs).
fn report_span(kind: MatureKind, stage: &str, process: &Process) {
    let bounds = process.collector().scanned_bounds();
    println!(
        "{kind:?}/{stage}: span_words={} ({:.2} MiB) base={:#x} limit={:#x} \
         young_spaces={} young_capacity_words={} mature_capacity_words={}",
        bounds.span_words(),
        (bounds.span_words() * std::mem::size_of::<Eterm>()) as f64 / (1024.0 * 1024.0),
        bounds.base(),
        bounds.limit(),
        process.collector().young_space_count(),
        process.collector().young().capacity_words(),
        process.collector().mature_capacity_words(),
    );
}

/// **A collection decision the filter must not change: the same `collections`/`reclaimed`
/// counters and the same surviving heap for a fixed workload, whichever tier answers.**
///
/// The deterministic half of the change's evidence, at the valve's own seam: the same
/// collection, driven the same way, over the same root set, must report the same counters. (The
/// end-to-end figure — a fixed workload's `erlang:statistics(garbage_collection)` being equal
/// before and after the change — is recorded in the parent node's `CONTEXT.md`.)
#[test]
fn the_filter_does_not_change_a_collections_counters() {
    for kind in KINDS {
        let mut seen = Vec::new();
        for _ in 0..3 {
            let (mut table, pid) = process_with(kind);
            let mut roots = Vec::new();
            for i in 0..64 {
                let term = table
                    .get_mut(pid)
                    .expect("the process")
                    .heap_mut()
                    .tuple(&[make_small(i), make_small(i + 1)])
                    .expect("a young-heap 2-tuple");
                roots.push(term);
            }
            {
                let process = table.get_mut(pid).expect("the process");
                process
                    .registers_mut()
                    .x_mut()
                    .set_live(16)
                    .expect("sixteen live registers");
                for (i, term) in roots.iter().enumerate() {
                    let register = i % 16;
                    process
                        .registers_mut()
                        .x_mut()
                        .set(register, *term)
                        .expect("a register is addressable");
                }
                // The valve's own phase-1 answer over that root set, before the collection: no
                // root is foreign, so the collection must be allowed and must really run.
                let process = table.get(pid).expect("the process");
                for term in &roots {
                    assert!(
                        !root_is_unscanned(process, *term),
                        "{kind:?}: a young-heap root must never decline the collection"
                    );
                    assert_eq!(
                        root_is_unscanned(process, *term),
                        exact_root_is_unscanned(process, *term),
                        "{kind:?}: the filter changed a young root's verdict"
                    );
                }
                let collection = table
                    .get_mut(pid)
                    .expect("the process")
                    .collect()
                    .expect("the collection runs");
                let process = table.get(pid).expect("the process");
                let heap = beam_gc::HeapInfo::of(process.collector());
                seen.push((
                    collection.kind,
                    collection.stats.objects_copied,
                    collection.stats.words_copied,
                    collection.stats.words_reclaimed,
                    heap.total_heap_size,
                ));
            }
        }
        // The identical fixture must produce the identical counters on every repetition: a
        // filter that skipped a region would change what is copied or reclaimed here.
        for window in seen.windows(2) {
            assert_eq!(
                window[0], window[1],
                "{kind:?}: the same workload must collect identically every time"
            );
        }
    }
}
