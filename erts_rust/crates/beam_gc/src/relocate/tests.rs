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

//! The relocation driver's own gates: the from-set constructor's skip/sort behaviour,
//! [`LinearDest`]'s classification and reservation, a full [`Relocator`]/[`compact`] run over
//! a small synthetic region with exact statistics, **root-slice idempotence** (the property
//! that makes a many-slice host correct), and the two up-front refusals (`BadArg` for an
//! unwalkable destination, `heap_exhausted` for a destination too small — with the source and
//! the roots provably untouched).
//!
//! `unsafe` below only builds synthetic regions this test owns (raw `Vec<Eterm>` words
//! standing in for a foreign arena such as a `ReservedHeap`) and steps pointers inside them;
//! every block carries a `// SAFETY:` rationale.

use core::mem::size_of;

use beam_common::{AllocKind, BeamError};
use beam_term::prelude::{
    header_arity, is_header, is_list, make_arityval, make_boxed, make_list, make_small, ptr_val,
    Eterm, NIL, THE_NON_VALUE,
};

use crate::space::HeapSpace;
use crate::{for_each_reachable_in, live_words_in};

use super::*;

/// A synthetic **foreign** region: a pre-reserved word vector whose first `used` words are the
/// terms a host built there. `FromSet::from_ranges`/`LinearDest` are handed its raw words,
/// exactly as they would be handed a `ReservedHeap`'s or an mmap'd arena's.
///
/// Every word starts as an immediate (`make_small(0)`), so even a mistaken read of an unused
/// word can never dereference anything.
struct Region {
    words: Vec<Eterm>,
    used: usize,
}

impl Region {
    fn new(capacity: usize) -> Region {
        Region {
            words: vec![make_small(0); capacity],
            used: 0,
        }
    }

    fn base(&self) -> *mut Eterm {
        self.words.as_ptr() as *mut Eterm
    }

    /// Reserves `n` words at the bump pointer (the host's own allocation step).
    fn alloc(&mut self, n: usize) -> *mut Eterm {
        assert!(
            self.used + n <= self.words.len(),
            "the synthetic region is big enough"
        );
        // SAFETY: `[used, used + n)` is inside this region's own allocation.
        let at = unsafe { self.base().add(self.used) };
        self.used += n;
        at
    }

    /// The words the mutator filled, in the `FromSet::from_ranges` argument shape.
    fn ranges(&self) -> Vec<(usize, usize)> {
        vec![(self.base() as usize, self.used)]
    }

    /// The live prefix as a from-set — the caller's three assertions (word-aligned, valid for
    /// the set's lifetime, a real filled prefix) hold by construction here.
    fn from_set(&self) -> FromSet {
        // SAFETY: `[base, base + used)` is this region's own filled prefix: word-aligned
        // (`Vec<Eterm>`), readable and writable for as long as `self` lives, one real live
        // extent, disjoint from itself, and untouched by any other thread.
        unsafe { FromSet::from_ranges(&self.ranges()) }
    }

    /// The region's whole word run, for byte-identity assertions.
    fn snapshot(&self) -> Vec<Eterm> {
        self.words.clone()
    }

    /// Builds the list `[values…]` and returns its head term.
    fn cons_list(&mut self, values: &[i64]) -> Eterm {
        let mut tail = NIL;
        for &value in values.iter().rev() {
            let at = self.alloc(2);
            // SAFETY: `at` is a freshly reserved, exclusively-owned 2-word cons cell.
            unsafe {
                *at = make_small(value);
                *at.add(1) = tail;
            }
            tail = make_list(at as *const Eterm);
        }
        tail
    }

    /// Builds the tuple `{…}` of `values` and returns it as a BOXED term.
    fn tuple(&mut self, values: &[Eterm]) -> Eterm {
        let at = self.alloc(values.len() + 1);
        // SAFETY: `at` is a freshly reserved, exclusively-owned `len + 1`-word tuple.
        unsafe {
            *at = make_arityval(values.len());
            for (index, &value) in values.iter().enumerate() {
                *at.add(index + 1) = value;
            }
        }
        make_boxed(at as *const Eterm)
    }
}

/// A source region holding a list `[1,2,3]` and the tuple `{List,42}` — 6 + 3 = 9 words,
/// 4 objects — plus the two roots. `roots[0]` reaches `roots[1]`, so forwarding either order
/// exercises the shared-subterm path.
fn source_region() -> (Region, [Eterm; 2]) {
    let mut source = Region::new(32);
    let list = source.cons_list(&[1, 2, 3]);
    let tuple = source.tuple(&[list, make_small(42)]);
    (source, [tuple, list])
}

/// The exact live volume of [`source_region`]'s roots.
const SOURCE_WORDS: usize = 9;

/// A source region holding two **independent** live objects — the tuple `{7,8,9}` (4 words) and
/// the list `[1,2,3]` (6 words). Neither root reaches the other, so per-slice live counts sum to
/// the exact total, which is the property a multi-slice preflight depends on.
fn independent_region() -> (Region, [Eterm; 2]) {
    let mut source = Region::new(32);
    let list = source.cons_list(&[1, 2, 3]);
    let tuple = source.tuple(&[make_small(7), make_small(8), make_small(9)]);
    (source, [tuple, list])
}

/// The exact live volume of [`independent_region`]'s roots.
const INDEPENDENT_WORDS: usize = 4 + 6;

/// A destination that publishes no contiguous copied run — the `BadArg` case.
struct NoRunDest {
    can_reserve: bool,
}

impl EvacDest for NoRunDest {
    unsafe fn classify_boxed(&self, _ptr: *const Eterm) -> Target {
        Target::Foreign
    }

    unsafe fn classify_cons(&self, _ptr: *const Eterm) -> Target {
        Target::Foreign
    }

    fn can_reserve(&self, _words: usize) -> bool {
        self.can_reserve
    }

    unsafe fn reserve(&mut self, words: usize) -> Result<*mut Eterm> {
        Err(heap_exhausted(words))
    }
}

// -----------------------------------------------------------------------
// `FromSet::from_ranges`
// -----------------------------------------------------------------------

#[test]
fn from_ranges_skips_zero_length_entries_and_sorts_by_lo() {
    let word = size_of::<Eterm>();
    // Out of `lo` order, with two empty entries interleaved.
    // SAFETY: no word of any range is read here; the entries are fabricated addresses (the
    // constructor only records geometry). The test asserts the *bookkeeping*.
    let set = unsafe {
        FromSet::from_ranges(&[(0x3000, 2), (0x1000, 1), (0x2000, 0), (0x1800, 3), (0x10, 0)])
    };

    assert_eq!(set.range_count(), 3, "the empty entries are skipped");
    assert_eq!(
        set.ranges(),
        &[
            (0x1000, 0x1000 + word),
            (0x1800, 0x1800 + 3 * word),
            (0x3000, 0x3000 + 2 * word),
        ],
        "the ranges are stored sorted by `lo`, as byte pairs"
    );
    assert_eq!(set.words(), 1 + 3 + 2, "the spans sum in term words");

    // `locate`/`contains` — the one geometry interpreter — agree with the stored shape.
    assert_eq!(set.locate(0x1800 + 2 * word), Some(1));
    assert_eq!(set.locate(0x1800 + 3 * word), None, "half-open at `hi`");
    assert!(set.contains(0x3000 as *const Eterm));
    assert!(!set.contains(0x1000usize.wrapping_sub(word) as *const Eterm));

    // An all-empty argument is the empty set, not an error.
    // SAFETY: as above — geometry only, nothing is dereferenced.
    let empty = unsafe { FromSet::from_ranges(&[(0x1000, 0)]) };
    assert!(empty.is_empty());
    assert_eq!(empty.words(), 0);
    assert_eq!(empty.range_count(), 0);
    // SAFETY: as above.
    assert!(unsafe { FromSet::from_ranges(&[]) }.is_empty());
}

#[test]
fn from_ranges_and_add_build_the_same_geometry() {
    // The constructor goes through the same single insertion point as `add`, so a set it
    // builds and one `add`ed from a real `HeapSpace` are indistinguishable — which is what
    // keeps `locate` the one interpreter of the geometry for both routes.
    let mut region = Region::new(16);
    region.cons_list(&[1, 2, 3]);
    let from_ranges = region.from_set();

    let mut space = HeapSpace::new(16).expect("space");
    let mut tail = NIL;
    for value in [3, 2, 1] {
        tail = space.heap_mut().cons(make_small(value), tail).expect("cons");
    }
    assert!(space.contains_term(tail));
    let mut added = FromSet::new();
    added.add(&space);

    // The same three cons cells, so the two sets cover the same span: one range each, six words.
    assert_eq!(from_ranges.range_count(), added.range_count());
    assert_eq!(from_ranges.words(), added.words());
    assert_eq!(from_ranges.words(), 6, "three 2-word cons cells");
    // Both cover exactly the same words: the fabricated region's range and the space's live
    // region have the same length, so compare the `contains` answer word by word.
    for offset in 0..added.words() {
        let width = size_of::<Eterm>();
        let in_ranges = from_ranges.ranges()[0].0 + offset * width;
        let in_added = added.ranges()[0].0 + offset * width;
        assert!(
            from_ranges.contains(in_ranges as *const Eterm),
            "every word of the range is covered"
        );
        assert!(added.contains(in_added as *const Eterm));
    }
    let beyond = from_ranges.ranges()[0].1;
    assert!(!from_ranges.contains(beyond as *const Eterm), "half-open");
}

// -----------------------------------------------------------------------
// `LinearDest`
// -----------------------------------------------------------------------

#[test]
fn linear_dest_classifies_and_reserves() {
    let mut source = Region::new(16);
    let list = source.cons_list(&[9]);
    let tuple = source.tuple(&[make_small(7), make_small(8)]);
    let from = source.from_set();

    let target = Region::new(16);
    // SAFETY: the destination is a separately allocated 16-word region, disjoint from the
    // source, valid and unmoved for this test's scope.
    let mut dest = unsafe { LinearDest::new(&from, target.base(), 16) };

    assert_eq!(dest.base(), target.base());
    assert_eq!(dest.capacity_words(), 16);
    assert_eq!(dest.used_words(), 0);
    assert_eq!(dest.available_words(), 16);
    assert_eq!(
        dest.copied_top(),
        Some(target.base()),
        "the copied run starts empty at the base"
    );

    // A from-space object not yet moved is `Live` — both shapes.
    // SAFETY: both pointers name objects inside the source region, which the from-set covers
    // and whose words the caller declared readable.
    unsafe {
        assert_eq!(dest.classify_boxed(ptr_val(tuple)), Target::Live);
        assert_eq!(dest.classify_cons(ptr_val(list)), Target::Live);
    }

    // An address outside every range is `Foreign`: a destination word (which is what makes a
    // second root pass a no-op) and a stack word both are.
    // SAFETY: the destination region is this test's own memory; classifying an address the
    // destination does not own is exactly what the `contains` test is for.
    unsafe {
        assert_eq!(
            dest.classify_boxed(target.base() as *const Eterm),
            Target::Foreign
        );
        let stack_word = make_small(1);
        assert_eq!(
            dest.classify_boxed(core::ptr::addr_of!(stack_word)),
            Target::Foreign
        );
    }

    // Reservation bumps `used` and advances the copied top; consecutive reservations are
    // consecutive words of the region and never alias.
    assert!(dest.can_reserve(3));
    // SAFETY: 3 words were just confirmed available.
    let first = unsafe { dest.reserve(3) }.expect("3 of 16 words");
    assert_eq!(first, target.base());
    assert_eq!(dest.used_words(), 3);
    assert_eq!(dest.available_words(), 13);
    assert_eq!(dest.copied_top(), Some(unsafe { target.base().add(3) }));
    // SAFETY: 2 of the remaining 13 words.
    let second = unsafe { dest.reserve(2) }.expect("2 of 13 words");
    assert_eq!(second, unsafe { target.base().add(3) });
    assert_eq!(dest.used_words(), 5);
    assert_eq!(
        dest.copied_top(),
        Some(unsafe { target.base().add(5) }),
        "the copied top advances monotonically with the reservations"
    );

    // The tail fits exactly; one more word is the typed refusal — an `out_of_memory`
    // (`AllocKind::Heap`) carrying the refused request, never a write past the region.
    assert!(dest.can_reserve(11));
    // SAFETY: exactly the remaining 11 words.
    unsafe { dest.reserve(11) }.expect("the tail fits");
    assert_eq!(dest.available_words(), 0);
    assert!(!dest.can_reserve(1));
    match unsafe { dest.reserve(1) } {
        Err(BeamError::Alloc(failure)) => {
            assert_eq!(failure.kind, AllocKind::Heap);
            assert_eq!(failure.requested, size_of::<Eterm>());
        }
        other => panic!("expected the typed heap exhaustion, got {other:?}"),
    }
    assert_eq!(dest.used_words(), 16, "the refusal changed nothing");
    assert_eq!(
        dest.copied_top(),
        Some(unsafe { target.base().add(16) }),
        "nor did it move the top"
    );

    // A zero-capacity destination accepts nothing but is otherwise well formed.
    // SAFETY: nothing is written when nothing can be reserved.
    let mut empty = unsafe { LinearDest::new(&from, target.base(), 0) };
    assert_eq!(empty.copied_top(), Some(target.base()));
    assert!(!empty.can_reserve(1));
    // SAFETY: the refusal precedes any write.
    assert!(unsafe { empty.reserve(1) }.is_err());
    assert_eq!(empty.used_words(), 0);
}

// -----------------------------------------------------------------------
// `compact` — the atomic single-slice form
// -----------------------------------------------------------------------

#[test]
fn compact_moves_the_live_graph_and_rewrites_the_roots() {
    let (source, roots_initial) = source_region();
    let source_words = source.used;
    let from = source.from_set();
    let mut roots = roots_initial;

    let target = Region::new(32);
    // SAFETY: a disjoint, dedicated 32-word region, valid and unmoved for this test's scope.
    let mut dest = unsafe { LinearDest::new(&from, target.base(), 32) };
    let report = compact(&from, &mut roots, &mut dest).expect("the destination has room");

    // Exactly the live graph: 3 cons cells (6 words) + a 3-word tuple.
    assert_eq!(report.stats.words_copied, SOURCE_WORDS);
    assert_eq!(report.stats.objects_copied, 4);
    assert_eq!(
        report.stats.words_reclaimed,
        source_words - SOURCE_WORDS,
        "the from-space words no live term named (the unused region tail is outside the range)"
    );
    assert_eq!(report.stats.minor_collections, 0);
    assert_eq!(
        report.stats.major_collections, 0,
        "a relocation is not a collection of a process heap"
    );
    assert_eq!(
        report.dest_words_used, SOURCE_WORDS,
        "a linear destination's used length is the copy volume"
    );
    assert_eq!(dest.used_words(), SOURCE_WORDS);

    // Every root now names a destination word and none names the source.
    for &root in &roots {
        assert!(
            !from.contains(ptr_val(root)),
            "no root still names the from-set"
        );
        assert!(
            ptr_val(root) as usize >= target.base() as usize,
            "the root names a destination word"
        );
        assert!(
            (ptr_val(root) as usize) < target.base() as usize + SOURCE_WORDS * size_of::<Eterm>()
        );
    }

    // The copies hold the same terms, with their references rewritten into the destination:
    // the tuple's element 0 is the (moved) list head, its element 1 is the original immediate.
    let (tuple, list) = (roots[0], roots[1]);
    // SAFETY: both roots name copied destination objects of known shape (a 2-tuple and a cons
    // cell) that the relocation just wrote.
    unsafe {
        assert!(is_header(*ptr_val(tuple)));
        assert_eq!(header_arity(*ptr_val(tuple)), 2);
        assert_eq!(*ptr_val(tuple).add(1), list, "the shared subterm is one copy");
        assert_eq!(*ptr_val(tuple).add(2), make_small(42));
    }
    // Walk the copied list: 1, 2, 3, nil.
    let mut cell = ptr_val(list);
    for value in [1, 2, 3] {
        // SAFETY: `cell` names a copied cons cell; its car is an immediate and its cdr is the
        // next copied cell of the same list (or NIL).
        unsafe {
            assert_eq!(*cell, make_small(value));
            let cdr = *cell.add(1);
            if value == 3 {
                assert_eq!(cdr, NIL);
            } else {
                assert!(is_list(cdr));
                cell = ptr_val(cdr);
            }
        }
    }

    // The source is consumed: each moved object's first word now holds the forwarding marker,
    // so the caller must not read it again (the documented failure contract).
    // SAFETY: the source region is still allocated for this test's scope.
    unsafe {
        assert!(
            !is_header(*ptr_val(roots_initial[0])),
            "the tuple's header word holds the BOXED forwarding marker"
        );
        assert!(
            ptr_val(*ptr_val(roots_initial[0])) as usize >= target.base() as usize,
            "and it names the copy"
        );
        assert_eq!(
            *ptr_val(roots_initial[1]),
            THE_NON_VALUE,
            "the cons cell's car holds the forwarding indicator"
        );
    }
}

#[test]
fn compact_refuses_a_small_destination_without_touching_anything() {
    let (source, roots_initial) = source_region();
    let from = source.from_set();
    let source_before = source.snapshot();
    let ranges_before = from.ranges().to_vec();
    let mut roots = roots_initial;

    // One word short of the exact live need.
    let target = Region::new(SOURCE_WORDS - 1);
    let target_before = target.snapshot();
    // SAFETY: the destination is a disjoint, dedicated region of exactly that size.
    let mut dest = unsafe { LinearDest::new(&from, target.base(), SOURCE_WORDS - 1) };
    let error = compact(&from, &mut roots, &mut dest).expect_err("8 words cannot hold 9");
    match error {
        BeamError::Alloc(failure) => {
            assert_eq!(failure.kind, AllocKind::Heap);
            assert_eq!(failure.requested, SOURCE_WORDS * size_of::<Eterm>());
        }
        other => panic!("expected the typed heap exhaustion, got {other:?}"),
    }

    // Nothing moved: the source words, the roots, and the from-set's ranges are byte
    // identical, and the destination is untouched.
    assert_eq!(source.snapshot(), source_before, "no forwarding marker");
    assert_eq!(roots, roots_initial, "no root rewritten");
    assert_eq!(from.ranges(), ranges_before.as_slice());
    assert_eq!(dest.used_words(), 0);
    assert_eq!(target.snapshot(), target_before);

    // A destination of exactly the right size is admitted and completes.
    let target = Region::new(SOURCE_WORDS);
    // SAFETY: a disjoint, dedicated region of exactly the needed size.
    let mut dest = unsafe { LinearDest::new(&from, target.base(), SOURCE_WORDS) };
    let report = compact(&from, &mut roots, &mut dest).expect("exactly enough");
    assert_eq!(report.stats.words_copied, SOURCE_WORDS);
    assert_eq!(report.dest_words_used, SOURCE_WORDS);
    assert_eq!(dest.available_words(), 0);
}

// -----------------------------------------------------------------------
// Root-slice idempotence
// -----------------------------------------------------------------------

#[test]
fn forward_roots_is_idempotent_on_the_same_slice() {
    let (source, roots_initial) = source_region();
    let from = source.from_set();
    let mut roots = roots_initial;

    let target = Region::new(32);
    // SAFETY: a disjoint, dedicated destination region.
    let mut dest = unsafe { LinearDest::new(&from, target.base(), 32) };
    let mut relocator = Relocator::new(&from, &mut dest).expect("a linear destination");

    relocator.forward_roots(&mut roots).expect("first pass");
    let after_first = roots;
    let words_after_first = relocator.words_copied();
    let objects_after_first = relocator.objects_copied();
    let dest_after_first = relocator.dest_words_used();
    let target_after_first = target.snapshot();
    assert_eq!(
        words_after_first, 5,
        "the 3-word tuple + the 2-word list head (the scan has not run yet)"
    );
    assert_eq!(objects_after_first, 2);
    assert_eq!(dest_after_first, 5);

    // Two more passes over the very same slice: a slot already rewritten names a destination
    // object, which the destination classifies `Foreign`, so nothing is copied, nothing is
    // rewritten and no destination word moves.
    for pass in 2..=3 {
        relocator
            .forward_roots(&mut roots)
            .unwrap_or_else(|error| panic!("pass {pass}: {error}"));
        assert_eq!(roots, after_first, "pass {pass} rewrote no root");
        assert_eq!(
            relocator.words_copied(),
            words_after_first,
            "pass {pass} copied no word"
        );
        assert_eq!(relocator.objects_copied(), objects_after_first);
        assert_eq!(relocator.dest_words_used(), dest_after_first);
    }
    assert_eq!(target.snapshot(), target_after_first);

    // The scan still completes the graph exactly once.
    let report = relocator.complete().expect("scan");
    assert_eq!(report.stats.words_copied, SOURCE_WORDS);
    assert_eq!(report.stats.objects_copied, 4);
    assert_eq!(report.dest_words_used, SOURCE_WORDS);
}

#[test]
fn overlapping_slices_copy_a_shared_object_once() {
    // The realistic host shape: roots scattered over several slices, one of which repeats an
    // element already forwarded. The union must copy exactly what one call over the union
    // would.
    let (source, roots_initial) = source_region();
    let from = source.from_set();
    let mut roots = roots_initial;

    let target = Region::new(32);
    // SAFETY: a disjoint, dedicated destination region.
    let mut dest = unsafe { LinearDest::new(&from, target.base(), 32) };
    let mut relocator = Relocator::new(&from, &mut dest).expect("a linear destination");

    // Slice 1: the tuple alone (which copies the tuple *and*, in the scan, nothing else yet).
    {
        let mut slice = [roots[0]];
        relocator.forward_roots(&mut slice).expect("slice 1");
        roots[0] = slice[0];
    }
    assert_eq!(relocator.words_copied(), 3, "only the tuple so far");
    assert_eq!(relocator.objects_copied(), 1);

    // Slice 2: both roots — the tuple again (a no-op) plus the list head.
    relocator.forward_roots(&mut roots).expect("slice 2");
    assert_eq!(
        relocator.words_copied(),
        3 + 2,
        "the tuple was not copied twice; the list head was copied once"
    );
    assert_eq!(relocator.objects_copied(), 2);

    let report = relocator.complete().expect("scan");
    assert_eq!(
        report.stats.words_copied, SOURCE_WORDS,
        "the same volume as one call over the union"
    );
    assert_eq!(report.stats.objects_copied, 4, "and the same object count");
    assert_eq!(report.dest_words_used, SOURCE_WORDS);

    // The shared subterm is one copy: the tuple's element 0 is the list root.
    // SAFETY: the tuple root names a copied 2-tuple.
    unsafe {
        assert_eq!(*ptr_val(roots[0]).add(1), roots[1]);
    }
}

// -----------------------------------------------------------------------
// The preflights
// -----------------------------------------------------------------------

#[test]
fn new_reports_badarg_for_an_unwalkable_destination() {
    let (source, roots_initial) = source_region();
    let from = source.from_set();
    let source_before = source.snapshot();
    let mut roots = roots_initial;

    // A destination with no contiguous copied run cannot be walked by the Cheney scan.
    let mut dest = NoRunDest { can_reserve: true };
    match Relocator::new(&from, &mut dest) {
        Err(BeamError::BadArg) => {}
        other => panic!(
            "expected BadArg for an unwalkable destination, got {:?}",
            other.map(|_| ())
        ),
    }
    // `new_checked` reports it too, and `compact` (which goes through it) leaves everything
    // byte-identical.
    assert!(matches!(
        Relocator::new_checked(&from, &mut dest, 0),
        Err(BeamError::BadArg)
    ));
    assert!(compact(&from, &mut roots, &mut dest).is_err());
    assert_eq!(source.snapshot(), source_before);
    assert_eq!(roots, roots_initial);
}

#[test]
fn new_checked_refuses_an_undersized_destination_before_mutating() {
    let (source, roots_initial) = source_region();
    let from = source.from_set();
    let source_before = source.snapshot();

    let target = Region::new(8);
    let target_before = target.snapshot();
    // SAFETY: a disjoint, dedicated 8-word destination region.
    let mut dest = unsafe { LinearDest::new(&from, target.base(), 8) };
    match Relocator::new_checked(&from, &mut dest, SOURCE_WORDS) {
        Err(BeamError::Alloc(failure)) => {
            assert_eq!(failure.kind, AllocKind::Heap);
            assert_eq!(failure.requested, SOURCE_WORDS * size_of::<Eterm>());
        }
        other => panic!("expected exhaustion, got {:?}", other.map(|_| ())),
    }
    assert_eq!(dest.used_words(), 0, "the refusal reserved nothing");
    assert_eq!(source.snapshot(), source_before, "and touched nothing");
    assert_eq!(target.snapshot(), target_before);
    assert_eq!(roots_initial[0], roots_initial[0]);

    // One word more than needed is admitted (an over-estimate is a conservative refusal, so
    // the caller passing a *sum* over overlapping slices is still served).
    let target = Region::new(32);
    // SAFETY: a disjoint, dedicated destination region.
    let mut dest = unsafe { LinearDest::new(&from, target.base(), 32) };
    let relocator =
        Relocator::new_checked(&from, &mut dest, SOURCE_WORDS + 1).expect("room to spare");
    assert_eq!(relocator.words_copied(), 0);
    assert_eq!(relocator.dest_words_used(), 0);
    drop(relocator);
    assert_eq!(source.snapshot(), source_before, "the source is untouched");
}

#[test]
fn a_multi_slice_preflight_compacts_the_union_atomically() {
    // The recommended many-slice recipe: one exact total, one preflight, one `forward_roots`
    // per slice, then `complete` — and the result carries the one-call volume. The two roots are
    // independent here (neither reaches the other), so the per-slice counts sum exactly.
    let (source, roots_initial) = independent_region();
    let from = source.from_set();
    let mut roots = roots_initial;
    let need = live_words_in(&from, &roots[..1]) + live_words_in(&from, &roots[1..]);
    assert_eq!(need, INDEPENDENT_WORDS, "disjoint slices sum to the volume");
    assert_eq!(
        live_words_in(&from, &roots),
        INDEPENDENT_WORDS,
        "and the whole slice agrees"
    );

    let target = Region::new(need);
    // SAFETY: a disjoint, dedicated destination region of exactly the preflighted size.
    let mut dest = unsafe { LinearDest::new(&from, target.base(), need) };
    let mut relocator = Relocator::new_checked(&from, &mut dest, need).expect("preflight");
    let (first, rest) = roots.split_at_mut(1);
    relocator.forward_roots(first).expect("slice 1");
    relocator.forward_roots(rest).expect("slice 2");
    let report = relocator.complete().expect("scan");

    assert_eq!(report.stats.words_copied, need);
    assert_eq!(report.dest_words_used, need);
    assert_eq!(dest.available_words(), 0, "the preflight was exact");
    for &root in &roots {
        assert!(!from.contains(ptr_val(root)), "every root moved");
    }
}

// -----------------------------------------------------------------------
// The public walking helpers
// -----------------------------------------------------------------------

#[test]
fn live_words_in_is_the_exact_copy_volume() {
    let (source, roots) = source_region();
    let from = source.from_set();
    assert_eq!(live_words_in(&from, &roots), SOURCE_WORDS);
    assert_eq!(live_words_in(&from, &[]), 0, "no roots, nothing to copy");
    assert_eq!(
        live_words_in(&from, &[make_small(1), NIL]),
        0,
        "immediates are never copied"
    );

    // Independent roots: the per-slice counts sum to the exact total.
    let (source, roots) = independent_region();
    let from = source.from_set();
    let split = live_words_in(&from, &roots[..1]) + live_words_in(&from, &roots[1..]);
    assert_eq!(split, INDEPENDENT_WORDS);
    assert_eq!(live_words_in(&from, &roots[..1]), 4, "the 3-element tuple");
    assert_eq!(live_words_in(&from, &roots[1..]), 6, "the 3-cell list");

    // An *overlapping* pair over-counts — the shared graph is walked twice. That is the safe
    // direction (the total is only a reservation request), and it is the one the documentation
    // warns about.
    let (source, roots) = source_region();
    let from = source.from_set();
    let overlapping = live_words_in(&from, &roots[..1]) + live_words_in(&from, &roots);
    assert_eq!(
        overlapping,
        2 * SOURCE_WORDS,
        "the tuple root alone already reaches the whole graph, so the pair counts it twice"
    );
}

#[test]
fn for_each_reachable_in_reports_the_live_boxed_objects() {
    let (source, roots) = source_region();
    let from = source.from_set();
    let base = source.base() as usize;
    let mut objects = Vec::new();
    for_each_reachable_in(&from, &roots, |object| {
        objects.push((object.address(), object.len(), object.header()))
    })
    .expect("the visitor is infallible, so the walk cannot fail");

    // Only the tuple carries a header word; the three cons cells are traversed but not
    // reported.
    assert_eq!(objects.len(), 1, "only the tuple is a boxed object");
    let (address, words, header) = objects[0];
    assert_eq!(address, ptr_val(roots[0]) as usize, "the reported tuple");
    assert_eq!(words, 3, "header + 2 elements");
    assert!(is_header(header));
    assert_eq!(header_arity(header), 2);
    assert!(address >= base);

    // After a compaction the same walk over the same from-set reports nothing: the roots name
    // the destination, which is outside every from-range.
    let target = Region::new(32);
    // SAFETY: a disjoint, dedicated destination region.
    let mut dest = unsafe { LinearDest::new(&from, target.base(), 32) };
    let mut roots = roots;
    compact(&from, &mut roots, &mut dest).expect("compact");
    let mut copies = 0usize;
    for_each_reachable_in(&from, &roots, |_object| copies += 1).expect("walk");
    assert_eq!(copies, 0, "the roots now name no from-space object");
}
