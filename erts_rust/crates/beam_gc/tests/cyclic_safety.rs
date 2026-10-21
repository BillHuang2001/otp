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

//! **Cycle safety** — the two whole-graph-vs-per-object boundaries a cyclic term can cross.
//!
//! Erlang terms are acyclic by construction; only *forged* memory (raw heap writes) can produce
//! a term reachable from itself. The runtime has exactly two places that walk a term graph, and
//! this file pins the contract of each on a forged cycle:
//!
//! * **(a) `beam_term::size::size_object_bounded`** — a *whole-graph* walk that ports
//!   `size_object_x` (`erts/emulator/beam/copy.c`). On a cyclic graph the unbounded C walk recurses
//!   forever; the bounded Rust entry point must instead return the **typed**
//!   [`TermError::TermGraphCyclic`] rather than hang, and it must do so even when the word budget
//!   is exhausted *before* the back-edge is reached (`TermGraphCyclic` outranks
//!   [`TermError::TermGraphTooLarge`], which a too-small budget on an **acyclic** graph yields).
//!   The two verdicts are distinct variants, so a caller can always tell them apart. This is
//!   verified through `beam_term`'s public API only — `size_object_bounded` lives in `beam_term`,
//!   not here.
//!
//! * **(b) the collector's per-object sizing** (`beam_gc::copy::move_size`) — the Cheney copier
//!   never walks the whole graph: it sizes one object at a time and uses the in-place forwarding
//!   pointer (the rewritten header/cons word) to stop re-copying an object it has already moved.
//!   A cyclic graph must therefore collect **correctly and terminate**: each object is copied
//!   exactly once ([`GcStats::objects_copied`]), roots are relocated, and the cycle closes on the
//!   *copies*. This is driven through **both** mature strategies via
//!   [`AnyCollector`] (`GcConfig::copying` / `GcConfig::immix`) and through the primitive
//!   [`beam_gc::collect`] entry, and pushed adversarially with a long chain whose last cell folds
//!   back to the head and a cycle reachable only through a deep nesting.
//!
//! **No structural comparison is used.** `beam_term::compare::eq` recurses forever on a cycle (it
//! has no visited set), so every post-collection check here *pointer-chases the copied words* and
//! asserts the exact cell values and the closure of the back-edge.
//!
//! Every cycle below is forged by **raw heap writes** in a [`HeapSpace`] — the workspace's
//! documented way to build a graph Erlang cannot express. `unsafe` is confined to those writes
//! and to reading back the pointers the collection just produced; each block carries a
//! `// SAFETY:` rationale.

use beam_alloc::default_allocator;
use beam_gc::prelude::{
    collect, AnyCollector, CollectionKind, GcConfig, HeapPolicy, HeapSpace, MatureKind,
};
use beam_term::prelude::{
    boxed_val, is_boxed, is_list, list_val, make_arityval, make_boxed, make_list, make_small,
    size_object, size_object_bounded, Eterm, TermError, NIL,
};

/// The length of the adversarial chain (`[0 | [1 | … [N-1 | back-to-head] … ]]]`).
const CHAIN_CELLS: usize = 512;

/// The depth of the adversarial nesting (a 1-tuple chain whose innermost element points back to
/// the outermost tuple, so the only cycle is reached through the whole nesting).
const NESTING_DEPTH: usize = 64;

/// The live-object count of the adversarial corpus: one self-loop cell, two 2-cycle cells, the
/// chain cells and the nested tuples.
const CORPUS_OBJECTS: usize = 1 + 2 + CHAIN_CELLS + NESTING_DEPTH;

/// The live-word count of the corpus: every object above is exactly 2 words.
const CORPUS_WORDS: usize = 2 * CORPUS_OBJECTS;

// ---------------------------------------------------------------------------
// Heap helpers
// ---------------------------------------------------------------------------

/// Allocates an empty `words`-word semi-space over the default allocator.
fn space(words: usize) -> HeapSpace {
    HeapSpace::with_allocator(default_allocator().clone(), words).expect("heap allocation")
}

// ---------------------------------------------------------------------------
// The forged cycles (raw heap writes — a graph Erlang cannot construct)
// ---------------------------------------------------------------------------

/// Builds `[1 | self]` — a cons cell whose `cdr` is a LIST-tagged pointer back to the cell
/// itself.
fn self_loop(space: &mut HeapSpace) -> Eterm {
    // SAFETY: `alloc_words` reserves 2 words for one cons cell and both are written before the
    // term is handed to any accessor; the cdr is a deliberate LIST-tagged self pointer (a cycle).
    unsafe {
        let hp = space.heap_mut().alloc_words(2).expect("self-loop cell");
        *hp = make_small(1);
        *hp.add(1) = make_list(hp as *const Eterm);
        make_list(hp as *const Eterm)
    }
}

/// Builds the two-cell cons cycle `A = [1 | B]`, `B = [2 | A]` and returns `A`.
fn two_cycle(space: &mut HeapSpace) -> Eterm {
    // SAFETY: two 2-word cons cells are reserved; each is fully written, with the other's
    // LIST-tagged pointer as its cdr (a deliberate cycle).
    unsafe {
        let px = space.heap_mut().alloc_words(2).expect("cell X");
        let py = space.heap_mut().alloc_words(2).expect("cell Y");
        *px = make_small(1);
        *px.add(1) = make_list(py as *const Eterm);
        *py = make_small(2);
        *py.add(1) = make_list(px as *const Eterm);
        make_list(px as *const Eterm)
    }
}

/// Builds the mixed **tuple/list** two-cycle `T = {L}`, `L = [1 | T]` and returns `T`.
fn tuple_list_two_cycle(space: &mut HeapSpace) -> Eterm {
    // SAFETY: a 2-word 1-tuple and a 2-word cons cell are reserved; each is fully written before
    // use, and the cons cell's cdr points back to the tuple (a deliberate cycle).
    unsafe {
        let pt = space.heap_mut().alloc_words(2).expect("tuple T");
        let pl = space.heap_mut().alloc_words(2).expect("list cell L");
        let t = make_boxed(pt as *const Eterm);
        *pt = make_arityval(1);
        *pt.add(1) = make_list(pl as *const Eterm);
        *pl = make_small(1);
        *pl.add(1) = t;
        t
    }
}

/// Builds a chain of `n` cons cells `[0 | [1 | … [n-1 | NIL] … ]]` and then folds the last cell's
/// `cdr` back to the head — a cycle behind a long acyclic prefix. Returns the head cell.
fn chain_with_back_edge(space: &mut HeapSpace, n: usize) -> Eterm {
    assert!(n >= 2, "a back-edge needs at least two cells");
    let mut cells = Vec::with_capacity(n);
    for _ in 0..n {
        // SAFETY: each `alloc_words(2)` reserves one cons cell; its two words are written in the
        // loop below before any accessor observes the cell.
        let p = unsafe { space.heap_mut().alloc_words(2).expect("chain cell") };
        cells.push(p);
    }
    // SAFETY: every `cells[i]` is a live 2-word cons cell of this space; the last cell's cdr is
    // patched to the head, closing exactly one back-edge.
    unsafe {
        for i in 0..n {
            *cells[i] = make_small(i as i64);
            *cells[i].add(1) = if i + 1 < n {
                make_list(cells[i + 1] as *const Eterm)
            } else {
                NIL
            };
        }
        *cells[n - 1].add(1) = make_list(cells[0] as *const Eterm);
    }
    make_list(cells[0] as *const Eterm)
}

/// Builds `depth` nested 1-tuples whose innermost element points back to the **outermost** tuple,
/// so the only cycle is reached through the whole nesting. Returns the outermost tuple.
fn deep_nested_cycle(space: &mut HeapSpace, depth: usize) -> Eterm {
    assert!(depth >= 2, "a cycle needs at least two nested tuples");
    let mut slots = Vec::with_capacity(depth);
    for _ in 0..depth {
        // SAFETY: each reservation is one 1-tuple (a header word plus one element word); both are
        // written in the loop below before any accessor observes the tuple.
        let p = unsafe { space.heap_mut().alloc_words(2).expect("nested tuple") };
        slots.push(p);
    }
    // SAFETY: every `slots[i]` is a live 2-word 1-tuple of this space; the innermost element is
    // patched to the outermost tuple, so the graph is cyclic only through the entire nesting.
    unsafe {
        for i in 0..depth {
            *slots[i] = make_arityval(1);
            *slots[i].add(1) = if i + 1 < depth {
                make_boxed(slots[i + 1] as *const Eterm)
            } else {
                make_boxed(slots[0] as *const Eterm)
            };
        }
    }
    make_boxed(slots[0] as *const Eterm)
}

// ---------------------------------------------------------------------------
// Post-collection verification (pointer chasing — never compare::eq)
// ---------------------------------------------------------------------------

/// Which forged shape a root is.
#[derive(Clone, Copy)]
enum Shape {
    /// `[1 | self]`.
    SelfLoop,
    /// `A = [1 | B]`, `B = [2 | A]`.
    TwoCycle,
    /// A `CHAIN_CELLS`-long cons chain whose last cdr folds back to the head.
    Chain,
    /// `NESTING_DEPTH` nested 1-tuples whose innermost element points back to the outermost.
    DeepNest,
}

/// Pointer-chases the copied graph rooted at `root` and asserts its exact values and that every
/// back-edge closes on the copies. `resident` is the caller's containment test for the
/// destination (so the same walker serves the primitive `to`-space and the `AnyCollector`).
fn assert_shape(root: Eterm, shape: Shape, resident: impl Fn(Eterm) -> bool) {
    match shape {
        Shape::SelfLoop => {
            assert!(is_list(root), "the self-loop root must stay a list");
            assert!(resident(root), "the self-loop root must be resident");
            let cell = list_val(root);
            // SAFETY: `root` is a live cons cell (established by `is_list`), so both words are
            // in-bounds and initialised.
            unsafe {
                assert_eq!(*cell, make_small(1), "the self-loop cell's car");
                assert_eq!(*cell.add(1), root, "the self-loop must close on the copy");
            }
        }
        Shape::TwoCycle => {
            assert!(resident(root), "the two-cycle root must be resident");
            let px = list_val(root);
            // SAFETY: `root` is a live cons cell; its cdr is the cycle's second cell.
            let cy = unsafe {
                assert_eq!(*px, make_small(1), "the two-cycle's first car");
                *px.add(1)
            };
            assert!(is_list(cy), "the cycle's second cell must stay a list");
            assert!(resident(cy), "the cycle's second cell must be resident");
            let py = list_val(cy);
            // SAFETY: `cy` is the live, copied second cell; both its words are in-bounds.
            unsafe {
                assert_eq!(*py, make_small(2), "the two-cycle's second car");
                assert_eq!(
                    *py.add(1),
                    root,
                    "the two-cycle must close between the copies"
                );
            }
        }
        Shape::Chain => {
            let mut term = root;
            for i in 0..CHAIN_CELLS {
                assert!(is_list(term), "chain cell {i} must be a list");
                assert!(resident(term), "chain cell {i} must be resident");
                let cell = list_val(term);
                // SAFETY: `term` is a live cons cell of the chain; both words are in-bounds.
                unsafe {
                    assert_eq!(*cell, make_small(i as i64), "the value at chain cell {i}");
                    let next = *cell.add(1);
                    if i + 1 < CHAIN_CELLS {
                        term = next;
                    } else {
                        assert_eq!(
                            next, root,
                            "the chain's back-edge must close on the copy of the head"
                        );
                    }
                }
            }
        }
        Shape::DeepNest => {
            let mut term = root;
            for i in 0..NESTING_DEPTH {
                assert!(is_boxed(term), "nested tuple {i} must be boxed");
                assert!(resident(term), "nested tuple {i} must be resident");
                let p = boxed_val(term);
                // SAFETY: `term` is a live 1-tuple of the nesting; the header and its single
                // element word are in-bounds.
                unsafe {
                    assert_eq!(*p, make_arityval(1), "nested tuple {i} is a 1-tuple");
                    let elem = *p.add(1);
                    if i + 1 < NESTING_DEPTH {
                        term = elem;
                    } else {
                        assert_eq!(
                            elem, root,
                            "the cycle must close through the whole nesting, on the copies"
                        );
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// (a) size_object_bounded — a whole-graph walk reports a cycle, never hangs
// ---------------------------------------------------------------------------

#[test]
fn bounded_sizer_reports_forged_cycles_as_a_typed_error_not_a_hang() {
    let mut space = space(64);
    let self_loop = self_loop(&mut space);
    let two_cycle = two_cycle(&mut space);
    let tuple_list = tuple_list_two_cycle(&mut space);

    for (name, term) in [
        ("cons self-loop", self_loop),
        ("cons two-cycle", two_cycle),
        ("tuple/list two-cycle", tuple_list),
    ] {
        // A budget that is never reached: the walk stops at the back-edge.
        assert_eq!(
            size_object_bounded(term, usize::MAX),
            Err(TermError::TermGraphCyclic),
            "{name}: an unlimited budget still reports the cycle"
        );
        assert_eq!(
            size_object_bounded(term, 1_000),
            Err(TermError::TermGraphCyclic),
            "{name}: a generous budget"
        );
        // The budget is spent *before* the back-edge here (each object is 2 words), yet "cyclic"
        // still wins over "too large": the error path runs the acyclicity pass.
        for limit in [0usize, 1] {
            assert_eq!(
                size_object_bounded(term, limit),
                Err(TermError::TermGraphCyclic),
                "{name}: limit {limit} is exhausted first, yet the verdict is still cyclic"
            );
        }
    }
}

#[test]
fn bounded_sizer_reports_cyclic_even_behind_a_long_prefix_that_exhausts_the_budget() {
    let mut space = space(4096);
    let root = chain_with_back_edge(&mut space, 1_000);

    // The budget is spent far before the back-edge; only the error-path acyclicity pass can see
    // the cycle — and it must, because "cyclic" outranks "too large".
    assert_eq!(
        size_object_bounded(root, 64),
        Err(TermError::TermGraphCyclic),
        "the budget is exhausted long before the back-edge"
    );
    assert_eq!(
        size_object_bounded(root, usize::MAX),
        Err(TermError::TermGraphCyclic)
    );
}

#[test]
fn bounded_sizer_distinguishes_too_large_from_cyclic_on_acyclic_graphs() {
    let mut space = space(4096);
    let mut list = NIL;
    for i in 0..1_000i64 {
        list = space
            .heap_mut()
            .cons(make_small(i), list)
            .expect("cons cell");
    }

    let exact = size_object(list);
    assert_eq!(exact, 2_000, "1_000 cons cells of 2 words each");

    // An acyclic graph sizes exactly at its budget and at an unbounded one.
    assert_eq!(size_object_bounded(list, exact), Ok(exact));
    assert_eq!(size_object_bounded(list, usize::MAX), Ok(exact));

    // One word short is a *distinct* typed verdict — never the cyclic one.
    assert_eq!(
        size_object_bounded(list, exact - 1),
        Err(TermError::TermGraphTooLarge),
        "a too-small budget on an acyclic graph is 'too large', never 'cyclic'"
    );

    // The two verdicts are separate variants, so a caller can always tell them apart.
    assert_ne!(TermError::TermGraphCyclic, TermError::TermGraphTooLarge);
}

// ---------------------------------------------------------------------------
// (b) the collector — per-object sizing is cycle-safe
// ---------------------------------------------------------------------------

/// The policy both strategies share: a nursery and a mature generation large enough that the
/// corpus's single minor collection is capacity-tight in neither space, and the default
/// `fullsweep_after` so the first collection is a **minor** (the promotion path).
///
/// The mature generation is sized well above the corpus's object count because the GenImmix
/// mature space allocates **line-granularly** (16 words per line, ≥ 1 line per object): the
/// corpus's [`CORPUS_OBJECTS`] objects need up to that many lines, and `16_384` words = 1_024
/// lines clears it, so the Immix path's first collection is a minor too (were it tighter, the
/// collector would legitimately escalate to a fullsweep instead — that is its documented
/// behaviour, not a defect).
fn collector_policy() -> HeapPolicy {
    HeapPolicy {
        fullsweep_after: 65_535,
        initial_young_words: 4_096,
        initial_mature_words: 16_384,
        min_heap_words: 4_096,
        ..HeapPolicy::default()
    }
}

/// Builds the collector `kind` is pinned to (`GcConfig::copying` / `GcConfig::immix`, an explicit
/// override of the automatic rule, so both mature strategies are reached regardless of size).
fn any_collector(kind: MatureKind) -> AnyCollector {
    let config = match kind {
        MatureKind::Copying => GcConfig::copying(collector_policy()),
        MatureKind::Immix => GcConfig::immix(collector_policy()),
    };
    AnyCollector::from_config(&config).expect("collector")
}

/// Writes the four forged shapes into `heap` and returns their roots plus their descriptions.
fn build_corpus(heap: &mut HeapSpace) -> (Vec<Eterm>, Vec<Shape>) {
    let roots = vec![
        self_loop(heap),
        two_cycle(heap),
        chain_with_back_edge(heap, CHAIN_CELLS),
        deep_nested_cycle(heap, NESTING_DEPTH),
    ];
    let shapes = vec![
        Shape::SelfLoop,
        Shape::TwoCycle,
        Shape::Chain,
        Shape::DeepNest,
    ];
    (roots, shapes)
}

#[test]
fn cyclic_graphs_collect_correctly_and_terminate_under_both_strategies() {
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let mut collector = any_collector(kind);

        // Build the corpus in the nursery.
        let (roots, shapes) = {
            let heap = collector.young_mut();
            build_corpus(heap)
        };
        let before = roots.clone();
        let mut roots = roots;

        // The first collection is a minor: every live object is promoted exactly once. The
        // copier's only cycle defence is its in-place forwarding pointer, so a cyclic graph must
        // neither hang nor double-copy.
        let collection = collector
            .collect(&mut roots)
            .expect("a cyclic graph must collect");
        assert_eq!(
            collection.kind,
            CollectionKind::Minor,
            "{kind:?}: the first collection is a minor"
        );
        assert_eq!(
            collection.stats.objects_copied, CORPUS_OBJECTS,
            "{kind:?}: each object is copied exactly once"
        );
        assert_eq!(
            collection.stats.words_copied, CORPUS_WORDS,
            "{kind:?}: 2 words per object, no double copy"
        );

        // Roots relocated: each root word changed and left the nursery.
        for (i, root) in roots.iter().enumerate() {
            assert_ne!(
                root.raw(),
                before[i].raw(),
                "{kind:?}: root {i} must be relocated"
            );
            assert!(
                !collector.young().contains_term(*root),
                "{kind:?}: root {i} must have left the nursery"
            );
        }

        // The graph is intact and every cycle closes on the copies. The nursery is empty after a
        // minor, so "resident elsewhere" is simply "no longer in the nursery".
        for (root, shape) in roots.iter().zip(shapes.iter()) {
            assert_shape(*root, *shape, |t| !collector.young().contains_term(t));
        }

        // Push it through a fullsweep too: the cycle now lives in the mature generation and must
        // survive the evacuation of the whole heap.
        collector
            .collect_major(&mut roots)
            .expect("a cyclic graph must survive a fullsweep");
        for (root, shape) in roots.iter().zip(shapes.iter()) {
            assert_shape(*root, *shape, |_| true);
        }
    }
}

#[test]
fn cyclic_graphs_collect_through_the_primitive_entry() {
    let mut from = space(4_096);
    let mut to = space(4_096);

    let (roots, shapes) = build_corpus(&mut from);
    let mut roots = roots;
    let stats = collect(&from, &mut to, &mut roots).expect("a cyclic graph must collect");

    assert_eq!(
        stats.objects_copied, CORPUS_OBJECTS,
        "each object is copied exactly once"
    );
    assert_eq!(stats.words_copied, CORPUS_WORDS);
    assert_eq!(
        to.used_words(),
        CORPUS_WORDS,
        "the to-space holds exactly the live words"
    );

    // Every root points into the to-space and nowhere in the from-space.
    for root in &roots {
        assert!(
            to.contains_term(*root),
            "each root must point into the to-space"
        );
        assert!(
            !from.contains_term(*root),
            "no root may still point into the from-space"
        );
    }

    // Pointer-chase the copies: values intact, every cycle closed (never `compare::eq`, which
    // recurses forever on a cycle).
    for (root, shape) in roots.iter().zip(shapes.iter()) {
        assert_shape(*root, *shape, |t| to.contains_term(t));
    }
}
