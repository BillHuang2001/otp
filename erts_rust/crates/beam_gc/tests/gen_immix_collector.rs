//! End-to-end evidence for the config-driven collector seam ([`AnyCollector`]).
//!
//! Drives the **same** corpus through the copying and the GenImmix strategies behind one
//! public API and asserts the observable result is identical: the sequence of collection
//! kinds, the emptied nursery, every root relocated out of the nursery into the mature
//! generation, and the live graph reachable from each root structurally intact. This is
//! exactly what `beam_process` sees when it drives a collection at a safepoint.
//!
//! `unsafe` below only dereferences pointers this test allocated and wrote itself (or that
//! a collection just wrote); every block carries a `// SAFETY:` rationale.

use beam_gc::prelude::{AnyCollector, CollectionKind, GcConfig, HeapPolicy, MatureKind};
use beam_term::prelude::{
    arityval, boxed_val, is_arity_value, is_atom, is_list, list_val, make_atom, make_small,
    signed_value, Atom, Eterm, NIL,
};

/// Walks a proper list of small integers, returning them in order.
fn list_to_smalls(mut term: Eterm) -> Vec<i64> {
    let mut values = Vec::new();
    while is_list(term) {
        // SAFETY: `term` is a live cons cell (`is_list`), so both words are readable.
        let cell = list_val(term);
        values.push(signed_value(unsafe { *cell }));
        term = unsafe { *cell.add(1) };
    }
    assert_eq!(term, NIL, "the list must be proper");
    values
}

/// Builds `{atom, [1,2,3], {4,5}}` in the nursery; returns the outer term and the atom.
fn build_graph(c: &mut AnyCollector) -> (Eterm, Eterm) {
    let heap = c.young_mut().heap_mut();
    let atom = make_atom(Atom::from_raw(7));
    let cell3 = heap.cons(make_small(3), NIL).expect("cons cell");
    let cell2 = heap.cons(make_small(2), cell3).expect("cons cell");
    let list = heap.cons(make_small(1), cell2).expect("cons cell");
    let inner = heap
        .tuple(&[make_small(4), make_small(5)])
        .expect("inner tuple");
    let outer = heap.tuple(&[atom, list, inner]).expect("outer tuple");
    (outer, atom)
}

/// Whether `term` is resident in the selected strategy's mature generation.
fn in_mature(c: &AnyCollector, term: Eterm) -> bool {
    match c {
        AnyCollector::Copying(x) => x.mature().contains_term(term),
        AnyCollector::Immix(x) => x.mature().contains_term(term),
    }
}

/// Asserts the graph reachable from `root` has the shape/leaf values of `{atom,[1,2,3],{4,5}}`.
fn assert_graph(c: &AnyCollector, root: Eterm, atom: Eterm) {
    // SAFETY: `root` is a live 3-element tuple.
    let header = unsafe { *boxed_val(root) };
    assert!(is_arity_value(header));
    assert_eq!(arityval(header), 3);

    // SAFETY: elements 0..3 lie within the 3-element tuple just validated.
    let e0 = unsafe { *boxed_val(root).add(1) };
    let e1 = unsafe { *boxed_val(root).add(2) };
    let e2 = unsafe { *boxed_val(root).add(3) };

    assert!(is_atom(e0));
    assert_eq!(e0, atom, "the atom leaf is unchanged");

    assert!(is_list(e1));
    assert!(in_mature(c, e1), "the list must be resident in mature");
    assert_eq!(list_to_smalls(e1), vec![1, 2, 3]);

    assert!(in_mature(c, e2), "the inner tuple must be resident in mature");
    // SAFETY: `e2` is a live 2-element tuple.
    let inner_header = unsafe { *boxed_val(e2) };
    assert!(is_arity_value(inner_header));
    assert_eq!(arityval(inner_header), 2);
    // SAFETY: elements 0..2 lie within that tuple.
    assert_eq!(signed_value(unsafe { *boxed_val(e2).add(1) }), 4);
    assert_eq!(signed_value(unsafe { *boxed_val(e2).add(2) }), 5);
}

/// Promotes three successive graphs through `kind`'s collector (two minors then a major),
/// asserting the live graph is intact after every collection; returns the collection kinds.
fn drive(kind: MatureKind) -> Vec<CollectionKind> {
    let policy = HeapPolicy {
        fullsweep_after: 2,
        initial_mature_words: 512,
        ..HeapPolicy::default()
    };
    let config = GcConfig::copying(policy).with_mature(kind);
    let mut c = AnyCollector::from_config(&config).expect("collector");
    assert_eq!(c.kind(), kind, "the config selected the requested strategy");

    let mut kinds = Vec::new();
    let mut roots: Vec<Eterm> = Vec::new();
    let mut atoms: Vec<Eterm> = Vec::new();

    for round in 0..3 {
        let (outer, atom) = build_graph(&mut c);
        roots.push(outer);
        atoms.push(atom);

        let collection = c.collect(&mut roots).expect("collect");
        kinds.push(collection.kind);
        assert_eq!(c.young().used_words(), 0, "the nursery is emptied");

        for (root, atom) in roots.iter().zip(&atoms) {
            assert!(
                !c.young().contains_term(*root),
                "no root may stay in the nursery after round {round}"
            );
            assert!(
                in_mature(&c, *root),
                "every root must be relocated into the mature generation"
            );
            assert_graph(&c, *root, *atom);
        }
    }

    kinds
}

#[test]
fn the_same_corpus_stays_intact_under_both_strategies() {
    let copying = drive(MatureKind::Copying);
    let immix = drive(MatureKind::Immix);

    // The GenImmix mature space selects the same collections as the copying one, and the
    // corpus is young for the first two rounds (two minors) and due a fullsweep on the
    // third (a major) — so the escalating path is exercised for both strategies.
    assert_eq!(
        copying,
        vec![
            CollectionKind::Minor,
            CollectionKind::Minor,
            CollectionKind::Major
        ]
    );
    assert_eq!(copying, immix, "both strategies pick the same collection kinds");
}

#[test]
fn default_config_selects_the_copying_path() {
    let any = AnyCollector::from_config(&GcConfig::default()).expect("collector");
    assert_eq!(any.kind(), MatureKind::Copying);
    assert!(matches!(any, AnyCollector::Copying(_)));
    assert_eq!(any.policy(), &HeapPolicy::default());
}
