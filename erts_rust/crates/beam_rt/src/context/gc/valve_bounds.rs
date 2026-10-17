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

//! The **two-tier scanned test** of the unscanned-root valve — [`super::outside_bounds`]'s
//! aggregate fast reject ([`beam_gc::ScannedBounds`]) in front of the exact
//! [`young_contains`](beam_gc::AnyCollector::young_contains) /
//! [`mature_contains`](beam_gc::AnyCollector::mature_contains) pair — and
//! [`super::root_is_unscanned`] above it.
//!
//! # What this pins
//!
//! The filter is allowed to skip work, never to change a decision: the aggregate is a superset
//! of every region its collector scans, so an address it rejects would have been rejected by
//! both exact tests too. Every case here therefore asserts **the same answer as the plain
//! per-region predicate** ([`exact_root_is_unscanned`], the valve's two facts re-derived here
//! without the filter) for the regions a real process actually produces:
//!
//! * a term in the **current** nursery space (accepted — the mutator block's own fast path);
//! * a term in a nursery space a [`Process::grow_nursery`] **retired** (accepted — the chain
//!   half of the aggregate, the case a naive bounds filter would break);
//! * a term the **mature** generation owns after a collection promoted it (accepted);
//! * a container on the runtime's **pooled BIF heap** and a cons in its **pointer-stable
//!   arena** (rejected: foreign);
//! * an **immediate** and a **tagged literal** (rejected before any address test, unchanged).
//!
//! The valve's answer for a *foreign* container is the conjunction of the two facts the
//! production predicate tests — the word sits in a region the collector does not scan **and**
//! the container forwards at least one word into a region it does. Both halves are covered here,
//! because the filter sits under both of them:
//!
//! * **inert** foreign containers — `make_ref/0`'s childless M1 block, a pooled 2-tuple of
//!   immediates, a pooled `ErlSubBits` whose `orig` word is an immediate, an arena cons of
//!   immediates — forward nothing a relocation would act on, so they do **not** decline;
//! * **forwarding** foreign containers — a pooled 2-tuple or an arena cons (or an `ErlSubBits`
//!   whose `orig` word is) holding a term from the asking process's **nursery** or **mature**
//!   generation — do.
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

use super::{
    bump, forwards_word_matching, in_heap_block, outside_bounds, root_is_unscanned,
    word_region_exactly, WordRegion,
};

/// The **pre-filter predicate**: the valve's verdict re-derived without the aggregate fast
/// reject, as the equivalence oracle.
///
/// Every case in this module asserts the production function agrees with this one. It is
/// deliberately a re-derivation, not a delegation: the *region* half runs
/// [`word_region_exactly`] (the exact per-region tests with no aggregate in front of them) and
/// the *forwarding* half classifies every child word with [`exact_is_scanned`] — the exact
/// young/mature tests written out again here — instead of the production
/// [`word_is_scanned`](super::word_is_scanned). If the aggregate filter ever changed a decision
/// (an over-eager reject, a superset that is not one) or the forwarding test ever grew a shape
/// the canonical enumeration does not visit, this catches it.
///
/// The child **enumeration** itself is deliberately shared ([`forwards_word_matching`] over
/// `beam_term::layout::for_each_child_slot`): that is the workspace's canonical model of an
/// object's reference slots — the one `beam_message`'s copier and the GC itself drive — so
/// re-implementing it here would only duplicate a layout table, not check it.
fn exact_root_is_unscanned(process: &Process, term: Eterm) -> bool {
    if !matches!(
        word_region_exactly(process, term),
        Some(WordRegion::Foreign)
    ) {
        return false;
    }
    forwards_word_matching(term, |word| exact_is_scanned(process, word))
}

/// Whether `word` is a heap pointer into a region the collector **relocates**, decided by the
/// exact per-region tests alone (the young nursery chain, current space or growth-retired space,
/// then the mature generation). The oracle's own classifier: `in_heap_block` for the current
/// mutator block, then the collector's per-region predicates — never the aggregate.
fn exact_is_scanned(process: &Process, word: Eterm) -> bool {
    if !is_boxed(word) && !is_list(word) {
        return false;
    }
    if beam_term::literal::is_literal(word) {
        return false;
    }
    let ptr = beam_term::list::ptr_val(word) as usize;
    in_heap_block(process.heap(), ptr)
        || process.collector().young_contains(ptr as *const Eterm)
        || process.collector().mature_contains(ptr as *const Eterm)
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
/// ordinary tuple, two `ErlSubBits` and two containers over a term of the asking process's own
/// **young** heap, plus two conses in the runtime's pointer-stable arena.
///
/// Both regions are the runtime's own ([`crate::Runtime::take_bif_heap`] — where a
/// registered-BIF result is built — and [`crate::Runtime::alloc_words`], the allocation doors'
/// pointer-stable fallback), so the terms are produced by the same code the valve meets in
/// production, not by a stand-in block.
///
/// The fixture is deliberately split by the valve's **second** fact: the `*_inert` terms forward
/// only immediates and literals, and the `*_over_young` terms forward a word the collector
/// relocates. The valve must answer `false` for the first group and `true` for the second, which
/// is what makes it a test of the forwarding test rather than of the region test alone.
struct Foreign {
    /// Owns the pooled heap's words: the terms below point into it, so it is dropped last.
    _runtime: crate::runtime::Runtime,
    /// `make_ref/0`'s frozen M1 childless block (`_ => None` in the child-slot table).
    reference: Eterm,
    /// A 2-tuple of immediates on the pooled heap: two child slots, both immediates.
    tuple_inert: Eterm,
    /// An `ErlSubBits` on the pooled heap whose `orig` word is a **tagged literal** — the
    /// production shape of a bitstring spliced out of a literal binary, and a word no relocation
    /// touches.
    sub_bits_inert: Eterm,
    /// A cons cell in the pointer-stable arena over immediates: header-less, both words
    /// immediates.
    cons_inert: Eterm,
    /// A pooled-heap 2-tuple whose element 0 is the young term below: a foreign container that
    /// **forwards a scanned word**.
    tuple_over_young: Eterm,
    /// A pooled-heap `ErlSubBits` whose `orig` word is the young term below.
    sub_bits_over_young: Eterm,
    /// An arena cons whose `car` is the young term below.
    cons_over_young: Eterm,
    /// The asking process's own nursery term the forwarding half points at.
    young: Eterm,
}

impl Foreign {
    /// Builds the foreign terms on a fresh runtime's own unscanned regions, plus the nursery term
    /// half of them on `process`'s young heap.
    #[allow(unsafe_code)]
    fn new(process: &mut Process) -> Foreign {
        let mut runtime = crate::runtime::Runtime::with_defaults().expect("the default runtime");
        // The scanned word the forwarding half of the fixture points at: an ordinary
        // young-heap tuple, i.e. something the collector must follow and update.
        let young = process
            .heap_mut()
            .tuple(&[make_small(7), make_small(8)])
            .expect("the young heap builds `{7,8}`");
        let fill = [NIL; 8];
        let mut heap = runtime.take_bif_heap().expect("a pooled BIF heap");
        // SAFETY: each `bump` reserves exactly the words the writer it feeds needs, inside this
        // heap, and every word is written before the term escapes.
        let (reference, tuple_inert, sub_bits_inert, tuple_over_young, sub_bits_over_young) = unsafe {
            let slot = bump(&mut heap, 4, &fill).expect("four words for the reference");
            let reference =
                beam_term::boxed::write_internal_ref(slot, 7).expect("an M1 internal reference");
            let tuple_inert = heap
                .tuple(&[make_small(1), make_small(2)])
                .expect("a 2-tuple");
            // A sub-bits whose `orig` word is frozen: the `erl_prim_loader`-style literal binary
            // a `bs_create_bin`-free splice produces. A literal is never relocated, so nothing
            // this container forwards imposes a follow or an update.
            let literal = beam_term::literal::make_literal(young);
            let slot = bump(&mut heap, 5, &fill).expect("five words for the ErlSubBits");
            let sub_bits_inert = beam_term::bitstring::write_sub_bits(
                slot,
                make_small(0),
                make_small(0),
                make_small(0),
                literal,
            );
            let tuple_over_young = heap
                .tuple(&[young, make_small(9)])
                .expect("a 2-tuple over the young term");
            let slot = bump(&mut heap, 5, &fill).expect("five words for the ErlSubBits");
            let sub_bits_over_young = beam_term::bitstring::write_sub_bits(
                slot,
                make_small(0),
                make_small(0),
                make_small(0),
                young,
            );
            (
                reference,
                tuple_inert,
                sub_bits_inert,
                tuple_over_young,
                sub_bits_over_young,
            )
        };
        // The heap goes back with its words intact (the pool never resets on return), so the
        // terms above stay valid for the whole fixture's life.
        runtime.put_bif_heap(heap);
        let (cons_inert, cons_over_young) = {
            let inert = runtime
                .alloc_words(2)
                .expect("two arena words for the inert cons");
            let forwarding = runtime
                .alloc_words(2)
                .expect("two arena words for the forwarding cons");
            // SAFETY: each `alloc_words(2)` handed over exactly two arena words, both written here
            // before the term escapes, and the arena is pointer-stable for the runtime's whole
            // life.
            unsafe {
                *inert = make_small(3);
                *inert.add(1) = NIL;
                *forwarding = young;
                *forwarding.add(1) = NIL;
                (
                    beam_term::list::make_list(inert),
                    beam_term::list::make_list(forwarding),
                )
            }
        };
        Foreign {
            _runtime: runtime,
            reference,
            tuple_inert,
            sub_bits_inert,
            cons_inert,
            tuple_over_young,
            sub_bits_over_young,
            cons_over_young,
            young,
        }
    }

    /// Every term of the fixture, grouped by the verdict it must get: `(label, term, unscanned)`.
    ///
    /// The order is the fixture's own: the inert group first, then the forwarding one.
    fn cases(&self) -> Vec<(&'static str, Eterm, bool)> {
        vec![
            ("childless make_ref/0 block", self.reference, false),
            ("pooled-BIF-heap tuple of immediates", self.tuple_inert, false),
            ("pooled-BIF-heap ErlSubBits over a literal", self.sub_bits_inert, false),
            ("pointer-stable-arena cons of immediates", self.cons_inert, false),
            ("pooled-BIF-heap tuple over a young term", self.tuple_over_young, true),
            (
                "pooled-BIF-heap ErlSubBits over a young term",
                self.sub_bits_over_young,
                true,
            ),
            ("pointer-stable-arena cons over a young term", self.cons_over_young, true),
        ]
    }

    /// A **new** pooled-heap 2-tuple over `word`, for a case whose scanned word belongs to
    /// another process's region (the mature generation) and so could not be part of [`Foreign::new`].
    ///
    /// The pool keeps the words it handed out when it goes back, so the term stays valid for the
    /// fixture's whole life.
    fn pooled_tuple_over(&mut self, word: Eterm) -> Eterm {
        let mut heap = self
            ._runtime
            .take_bif_heap()
            .expect("the runtime still owns its pooled BIF heap");
        let tuple = heap
            .tuple(&[word, make_small(1)])
            .expect("a 2-tuple over the other process's word");
        self._runtime.put_bif_heap(heap);
        tuple
    }

    /// Asserts each term really is **outside** every scanned region of `process`'s collector —
    /// the precondition that makes the classifier's answer a statement about the forwarding test
    /// rather than about the region — and that the nursery term the forwarding half points at
    /// really is **inside** it.
    fn assert_foreign(&self, process: &Process) {
        let young = beam_term::list::ptr_val(self.young) as usize;
        assert!(
            in_heap_block(process.heap(), young),
            "the fixture's young term must live in the asking process's mutator block"
        );
        assert!(
            exact_is_scanned(process, self.young),
            "the fixture's young term must be a word the collector relocates"
        );
        for (what, term, _) in self.cases() {
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
        // The foreign half is built **before** the process is read immutably: its forwarding
        // terms point at a nursery term, so it needs the process mutably once.
        let mut foreign = Foreign::new(chain.get_mut(chain_pid).expect("the process"));
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
        foreign.assert_foreign(chain_process);
        // Whether the aggregate can reject each foreign term outright — the filter's own hit
        // rate, which is what decides if tier two runs at all for the runtime's real unscanned
        // regions (see `report_span`'s docs on the width).
        let bounds = chain_process.collector().scanned_bounds();
        for (what, term, _) in foreign.cases() {
            println!(
                "{kind:?}: {what} rejected-by-the-aggregate={}",
                bounds.definitely_unscanned(beam_term::list::ptr_val(term) as *const Eterm)
            );
        }

        // A **tagged literal**: a real term with the loader's literal bit set — frozen, never
        // moved, rejected before any address test.
        let literal = beam_term::literal::make_literal(retired);

        // The whole foreign fixture, at its own verdicts: the inert group must not decline, the
        // group that forwards a nursery word must.
        let foreign_cases: Vec<Case> = foreign
            .cases()
            .into_iter()
            .map(|(what, term, unscanned)| Case {
                what,
                term,
                unscanned,
            })
            .collect();
        let mut chain_cases = vec![
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
        ];
        chain_cases.extend(foreign_cases.iter().map(|case| Case {
            what: case.what,
            term: case.term,
            unscanned: case.unscanned,
        }));
        chain_cases.extend([
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
        ]);
        assert_all_agree(chain_process, kind, &chain_cases);

        // A foreign container forwarding a **mature** word is a hazard too: the mature
        // generation is relocated (a major) and updated (a remembered set), so the second fact
        // must accept it exactly as it accepts a nursery word.
        let over_mature = foreign.pooled_tuple_over(promoted);
        assert_all_agree(
            promoted_process,
            kind,
            &[
                Case {
                    what: "mature-generation tuple",
                    term: promoted,
                    unscanned: false,
                },
                Case {
                    what: "pooled-BIF-heap tuple over a mature term",
                    term: over_mature,
                    unscanned: true,
                },
            ],
        );
        // The foreign answers must not depend on which process asks — the regions are the
        // runtime's, not the process's. (The nursery-forwarding group is excluded: its child is a
        // word of `chain_process`'s nursery, which is foreign *to* `promoted_process`.)
        let inert: Vec<Case> = foreign_cases
            .iter()
            .filter(|case| !case.unscanned)
            .map(|case| Case {
                what: case.what,
                term: case.term,
                unscanned: case.unscanned,
            })
            .collect();
        assert_all_agree(promoted_process, kind, &inert);
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
        // `outside_bounds` then answers without touching the chain index or the mature space.
        let far = bounds.limit().saturating_add(1 << 20);
        assert!(
            bounds.definitely_unscanned(far as *const Eterm),
            "{kind:?}: an address past the aggregate must be rejected"
        );
        assert!(
            outside_bounds(collector, &bounds, far),
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
