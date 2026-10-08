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

//! Copy-correctness evidence for the [`beam_gc`] Cheney copier.
//!
//! These tests are the crate's required evidence that a collection (1) **updates the
//! roots** to point into the to-space, (2) **preserves the structure and leaf values**
//! of the live term graph, and (3) **drops unreachable garbage** (the to-space word
//! count is exactly the live size and `words_reclaimed` is the garbage size). They
//! exercise the public API only (`HeapSpace`, `FromSet`, `Copier`, `collect`) plus the
//! frozen term helpers of `beam_term`, the way `beam_process` would drive a collection.
//!
//! `unsafe` below only dereferences pointers that this test allocated and wrote itself
//! (or that the copier just wrote); every block carries a `// SAFETY:` rationale.

use beam_alloc::default_allocator;
use beam_gc::prelude::{collect, Copier, FromSet, HeapSpace};
use beam_term::prelude::{
    arityval, boxed_val, is_arity_value, is_atom, is_list, is_small, list_val, make_atom,
    make_boxed, make_header, make_small, signed_value, Atom, Eterm, ARITYVAL_SUBTAG, NIL,
    TAG_LITERAL_PTR,
};

/// Allocates an empty `words`-word semi-space over the default allocator.
fn space(words: usize) -> HeapSpace {
    HeapSpace::with_allocator(default_allocator().clone(), words).expect("heap allocation")
}

/// Walks a proper list of small integers, returning them in order.
fn list_to_smalls(mut term: Eterm) -> Vec<i64> {
    let mut values = Vec::new();
    while is_list(term) {
        // SAFETY: `term` is a live cons cell (established by `is_list`), so both its
        // car and cdr words are in-bounds and initialised.
        let cell = list_val(term);
        values.push(signed_value(unsafe { *cell }));
        term = unsafe { *cell.add(1) };
    }
    assert_eq!(term, NIL, "the list must be proper");
    values
}

#[test]
fn copy_preserves_structure_updates_roots_and_drops_garbage() {
    let mut from = space(64);
    let mut to = space(64);

    // The live graph `{atom, [1,2,3], {4,5}}`: a nested tuple holding an atom leaf, a
    // three-cell cons list and an inner boxed tuple.
    let atom = make_atom(Atom::from_raw(7));
    let cell3 = from.heap_mut().cons(make_small(3), NIL).expect("cons cell");
    let cell2 = from
        .heap_mut()
        .cons(make_small(2), cell3)
        .expect("cons cell");
    let list = from
        .heap_mut()
        .cons(make_small(1), cell2)
        .expect("cons cell");
    let inner = from
        .heap_mut()
        .tuple(&[make_small(4), make_small(5)])
        .expect("inner tuple");
    let outer = from
        .heap_mut()
        .tuple(&[atom, list, inner])
        .expect("outer tuple");

    // Deliberate garbage: an unreferenced `{7,8}` tuple.
    let _garbage = from
        .heap_mut()
        .tuple(&[make_small(7), make_small(8)])
        .expect("garbage tuple");

    // 4 (outer) + 3 (inner) + 6 (three cons cells) + 3 (garbage).
    assert_eq!(from.used_words(), 16);

    let mut roots = [outer];
    let stats = collect(&from, &mut to, &mut roots).expect("collect");

    // (a) Roots updated: the reference now points into the to-space, not the from-space.
    let copied = roots[0];
    assert!(
        to.contains_term(copied),
        "root must point into the to-space"
    );
    assert!(
        !from.contains_term(copied),
        "root must no longer point into the from-space"
    );

    // (b) Structural preservation: walk the copied graph from the updated root.
    // SAFETY: `copied` is a live 3-element tuple in the to-space.
    let outer_hdr = unsafe { *boxed_val(copied) };
    assert!(is_arity_value(outer_hdr));
    assert_eq!(arityval(outer_hdr), 3);

    // SAFETY: elements 0..3 are within the 3-element tuple just validated.
    let e0 = unsafe { *boxed_val(copied).add(1) };
    let e1 = unsafe { *boxed_val(copied).add(2) };
    let e2 = unsafe { *boxed_val(copied).add(3) };

    // element 0: the atom leaf, copied verbatim (an immediate is never moved).
    assert_eq!(e0, atom);
    assert!(is_atom(e0));

    // element 1: the [1,2,3] list, now resident in the to-space and intact.
    assert!(is_list(e1));
    assert!(
        to.contains_term(e1),
        "list must be resident in the to-space"
    );
    assert_eq!(list_to_smalls(e1), vec![1, 2, 3]);

    // element 2: the inner {4,5} tuple, copied as a distinct to-space object.
    assert!(to.contains_term(e2), "inner tuple must be resident");
    // SAFETY: `e2` is a live 2-element tuple in the to-space.
    let inner_hdr = unsafe { *boxed_val(e2) };
    assert!(is_arity_value(inner_hdr));
    assert_eq!(arityval(inner_hdr), 2);
    // SAFETY: elements 0..2 are within that tuple.
    let i0 = unsafe { *boxed_val(e2).add(1) };
    let i1 = unsafe { *boxed_val(e2).add(2) };
    assert_eq!(signed_value(i0), 4);
    assert_eq!(signed_value(i1), 5);

    // (c) Garbage dropped: the to-space holds exactly the 13 live words.
    assert_eq!(
        to.used_words(),
        13,
        "only the live 13 words are in the to-space"
    );
    assert_eq!(stats.words_copied, 13);
    assert_eq!(
        stats.objects_copied, 5,
        "outer + inner tuple + 3 cons cells"
    );
    assert_eq!(stats.words_reclaimed, 3, "the unreferenced {{7,8}} tuple");
}

#[test]
fn immediate_roots_are_left_unchanged() {
    let from = space(16);
    let mut to = space(16);

    let mut roots = [make_small(42), NIL, make_atom(Atom::from_raw(3))];
    let before = roots;

    let stats = collect(&from, &mut to, &mut roots).expect("collect");

    assert_eq!(roots, before, "immediates are never moved");
    assert!(is_small(roots[0]));
    assert_eq!(stats.words_copied, 0);
    assert_eq!(stats.objects_copied, 0);
    assert_eq!(stats.words_reclaimed, 0);
    assert_eq!(to.used_words(), 0);
}

#[test]
fn out_of_from_space_reference_is_left_verbatim() {
    let from = space(16);
    let mut to = space(16);

    // An object outside every from-space, referenced with the literal pointer bit set:
    // the copier must recognise it as untouchable and leave the word byte-identical.
    let external = [
        make_header(2, ARITYVAL_SUBTAG),
        make_small(11),
        make_small(22),
    ];
    let literal = Eterm::from_raw(make_boxed(external.as_ptr()).raw() | TAG_LITERAL_PTR);

    let mut roots = [literal, make_small(99)];
    let stats = collect(&from, &mut to, &mut roots).expect("collect");

    assert_eq!(
        roots[0].raw(),
        literal.raw(),
        "the out-of-from-space reference must be byte-identical"
    );
    assert_eq!(roots[1], make_small(99));
    assert_eq!(stats.words_copied, 0);
    assert_eq!(stats.objects_copied, 0);
    assert_eq!(to.used_words(), 0);
}

#[test]
fn pre_existing_to_space_content_is_not_rescanned() {
    let from = space(16);
    let mut to = space(16);

    // Place an object in the to-space *before* the collection: the copier's scan
    // origin is the to-space top at construction, so this object is never revisited.
    let resident = to
        .heap_mut()
        .tuple(&[make_small(1), make_small(2)])
        .expect("resident tuple");
    let before = resident;

    let mut roots = [resident];
    let stats = collect(&from, &mut to, &mut roots).expect("collect");

    assert_eq!(
        roots[0].raw(),
        before.raw(),
        "a reference into the to-space is out of the from-set"
    );
    assert_eq!(stats.words_copied, 0);
    assert_eq!(stats.objects_copied, 0);
    assert_eq!(to.used_words(), 3, "the resident tuple is untouched");
}

#[test]
fn copier_evacuates_from_multiple_from_spaces() {
    let mut young = space(16);
    let mut mature = space(16);
    let mut to = space(64);

    // A list whose head cell lives in the nursery and whose tail cell lives in the
    // mature space — the fullsweep (nursery + mature) case, evacuated as one graph.
    let tail = mature
        .heap_mut()
        .cons(make_small(2), NIL)
        .expect("tail cell");
    let head = young
        .heap_mut()
        .cons(make_small(1), tail)
        .expect("head cell");
    let tuple = mature
        .heap_mut()
        .tuple(&[make_small(3), make_small(4)])
        .expect("tuple");

    let mut set = FromSet::new();
    set.add(&young);
    set.add(&mature);
    assert_eq!(set.words(), young.used_words() + mature.used_words());

    let mut roots = [head, tuple];
    let stats = Copier::new(&set, &mut to).run(&mut roots).expect("run");

    assert!(to.contains_term(roots[0]), "list root now in the to-space");
    assert!(to.contains_term(roots[1]), "tuple root now in the to-space");
    assert!(!young.contains_term(roots[0]));
    assert!(!mature.contains_term(roots[0]));
    assert!(!mature.contains_term(roots[1]));

    // The list traverses intact across the former young/mature boundary.
    assert_eq!(list_to_smalls(roots[0]), vec![1, 2]);

    // 2 (head) + 2 (tail) + 3 (tuple) words copied.
    assert_eq!(stats.words_copied, 7);
    assert_eq!(stats.objects_copied, 3);
    assert_eq!(to.used_words(), 7);
}
