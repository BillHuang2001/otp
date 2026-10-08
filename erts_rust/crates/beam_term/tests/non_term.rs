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

//! A word that is **not a term** must never crash a `beam_term` entry point.
//!
//! The C ABI's `THE_NON_VALUE` (`0` in a release build, `make_header(0, FLOAT_SUBTAG)`
//! in a debug build — the word a NIF/`enif_*` callback hands back to mean "no term")
//! is the canonical stray word: its primary tag is `HEADER`, which the C's
//! `is_boxed` bit trick *accepts*, so the C's own predicates and comparison deref a
//! garbage (in release, `NULL`) `boxed_val` pointer. A sibling runtime used to abort
//! on exactly that when a NIF returned `THE_NON_VALUE` and the engine compared it.
//!
//! This file pins the crate-wide policy for every **safe** public entry point:
//!
//! * it never panics and never aborts (no `#[should_panic]` anywhere here — the test
//!   process dying *is* the failure);
//! * it never dereferences the word — the result is a **typed error** or a
//!   **defined ordering**;
//! * comparison never calls a non-term equal to a real term;
//! * every valid term behaves exactly as before (the last test).
//!
//! It consumes the crate only through its public API (`beam_term::prelude::*`).

use core::cmp::Ordering;

use beam_term::prelude::*;

/// Every "word that is not a term" shape the crate must tolerate: the ABI's
/// `THE_NON_VALUE` (both builds' spellings), a bare `FLOAT` header word (what a
/// 1-word "flonum" header looks like) and an arbitrary other `HEADER`-tagged word.
fn non_terms() -> Vec<Eterm> {
    vec![
        THE_NON_VALUE,
        make_header(0, FLOAT_SUBTAG),
        HEADER_FLONUM,
        Eterm::from_raw(0x3c),
        Eterm::from_raw(0x100),
    ]
}

fn atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["a", "b", "ok", "false"]);
    atoms
}

/// The guard itself: a non-term is not a term word, and every one of the words above
/// really is a bare header.
#[test]
fn a_bare_header_word_is_not_a_term_word() {
    for nt in non_terms() {
        assert!(is_header(nt), "{nt:?} is a header word");
        assert!(!is_term_word(nt), "{nt:?} is not a term word");
    }
    // ... and no valid term is ever a header word.
    let mut heap = Heap::new(32);
    for t in [
        make_small(0),
        NIL,
        make_atom(atoms().lookup("a").unwrap()),
        heap.tuple(&[make_small(1)]).unwrap(),
        heap.cons(make_small(1), NIL).unwrap(),
        tuple0(),
    ] {
        assert!(is_term_word(t), "{t:?} is a term word");
    }
}

/// Comparison: a non-term never dereferences, never panics, sorts **after** every
/// real term (so it is never `Equal` to one), and two non-terms order by raw word.
#[test]
fn comparison_never_dereferences_a_non_term() {
    let atoms = atoms();
    let mut heap = Heap::new(64);
    let samples = [
        make_small(0),
        make_small(-7),
        make_atom(atoms.lookup("a").unwrap()),
        NIL,
        heap.tuple(&[make_small(1), make_small(2)]).unwrap(),
        heap.cons(make_small(1), NIL).unwrap(),
        tuple0(),
    ];
    for nt in non_terms() {
        // The identical word compares equal to itself — it *is* the same word.
        assert_eq!(cmp(nt, nt, &atoms), Ordering::Equal);
        assert_eq!(cmp_arith(nt, nt, &atoms), Ordering::Equal);
        assert!(eq(nt, nt, &atoms));
        for t in samples {
            assert_eq!(cmp(nt, t, &atoms), Ordering::Greater, "{nt:?} vs {t:?}");
            assert_eq!(cmp(t, nt, &atoms), Ordering::Less, "{t:?} vs {nt:?}");
            assert_eq!(cmp_arith(nt, t, &atoms), Ordering::Greater);
            assert_eq!(cmp_arith(t, nt, &atoms), Ordering::Less);
            assert!(!eq(nt, t, &atoms));
            assert!(!eq(t, nt, &atoms));
        }
    }
    // The relation stays a total order over non-terms too (different raw words).
    for x in non_terms() {
        for y in non_terms() {
            let xy = cmp(x, y, &atoms);
            assert_eq!(xy, cmp(y, x, &atoms).reverse());
            assert_eq!(xy.is_eq(), x.raw() == y.raw());
        }
    }
}

/// Classification and the `is_*` predicates: a non-term is `TermClass::NonValue`,
/// every type test is `false`, and nothing is dereferenced.
#[test]
fn classification_never_dereferences_a_non_term() {
    for nt in non_terms() {
        // SAFETY: the whole point — `class` and the predicates are total for a
        // non-term word (they test the primary tag first).
        unsafe {
            assert_eq!(class(nt), TermClass::NonValue);
            assert!(!is_integer(nt));
            assert!(!is_number(nt));
            assert!(!is_float(nt));
            assert!(!is_big(nt));
            assert!(!is_map(nt));
            assert!(!is_flatmap(nt));
            assert!(!is_hashmap(nt));
            assert!(!is_record(nt));
            assert!(!is_any_fun(nt));
            assert!(!is_internal_ref(nt));
            assert!(!is_external_ref(nt));
            assert!(!is_external_pid(nt));
            assert!(!is_external_port(nt));
            assert!(!is_bitstring(nt));
            assert!(!is_binary(nt));
            assert!(!is_bin_ref(nt));
            assert!(!is_boxed_term(nt));
            assert!(!is_tuple(nt));
            assert!(!is_tuple_arity(nt, 1));
            assert!(!is_refc_binary(nt));
        }
        assert!(!is_boxed_term(nt));
        // `is_list` / `is_small` / `is_nil` are pure raw-bit predicates (the C bit
        // tricks) — they never dereference, so they are allowed to answer either
        // way for a non-term; only the invariant "no dereference, no panic" holds.
        let _ = (is_list(nt), is_small(nt), is_nil(nt));
        assert!(!is_zero_sized(nt));
        assert!(is_non_value(nt) || is_value(nt)); // both answers are defined
    }
}

/// Hashing: the internal structural hash is total; the two compatibility-pinned
/// ports (`phash2`/`phash`) refuse with a typed `NotATerm` error.
#[test]
fn hashing_never_dereferences_a_non_term() {
    let atoms = atoms();
    for nt in non_terms() {
        // Total: a defined value, no dereference.
        let h = term_hash(nt, &atoms);
        assert_eq!(h, term_hash(nt, &atoms), "deterministic");
        assert_eq!(term_hash(nt, &atoms), h);
        // The pinned ERTS hashes refuse rather than substitute a value.
        assert_eq!(make_hash2(nt, &atoms), Err(TermError::NotATerm));
        assert_eq!(make_hash(nt, &atoms), Err(TermError::NotATerm));
    }
    // `eq ⇒ equal hash` still holds across the non-term boundary: the only non-term
    // `eq` calls equal to another word is the identical word.
    for x in non_terms() {
        for y in non_terms() {
            if eq(x, y, &atoms) {
                assert_eq!(term_hash(x, &atoms), term_hash(y, &atoms));
            }
        }
    }
}

/// Size and the numeric conversions: a non-term is not a term graph (0 words) and is
/// not a number (`NaN` / `None`) — never a dereference.
#[test]
fn size_and_numeric_conversions_never_dereference_a_non_term() {
    for nt in non_terms() {
        assert_eq!(size_object(nt), 0);
        assert!(number_as_f64(nt).is_nan());
        assert_eq!(big_to_f64(nt), None);
    }
}

/// The map/fun/record layout helpers report a typed error for a non-term (they used
/// to test `is_boxed`, which accepts a bare header word, and deref it).
#[test]
fn layout_helpers_report_a_non_term() {
    let atoms = atoms();
    for nt in non_terms() {
        assert_eq!(map_size(nt, &atoms), Err(TermError::BadMap));
        assert_eq!(map_get(nt, make_small(0), &atoms), Err(TermError::BadMap));
        assert_eq!(map_pairs(nt, &atoms), Err(TermError::BadMap));
        assert_eq!(map_pairs_storage_order(nt), Err(TermError::BadMap));
        assert_eq!(fun_block_words(nt), Err(TermError::BadFun));
        // SAFETY: the record readers are total for a non-term word — that is the
        // property under test.
        unsafe {
            assert_eq!(record_field_count(nt), Err(TermError::BadRecord));
            assert_eq!(record_definition(nt), Err(TermError::BadRecord));
            assert_eq!(record_definition_module(nt), Err(TermError::BadRecord));
            assert_eq!(record_definition_name(nt), Err(TermError::BadRecord));
            assert_eq!(record_definition_hash(nt), Err(TermError::BadRecord));
            assert_eq!(record_definition_is_exported(nt), Err(TermError::BadRecord));
            assert_eq!(record_definition_keys(nt), Err(TermError::BadRecord));
            assert_eq!(record_definition_field_order(nt), Err(TermError::BadRecord));
            assert_eq!(record_declaration_keys(nt), Err(TermError::BadRecord));
            assert_eq!(record_get(nt, make_small(0), &atoms), Err(TermError::BadRecord));
            assert!(!record_is_native(nt, atoms.lookup("a").unwrap(), atoms.lookup("b").unwrap()));
        }
    }
}

/// **No behaviour change for valid input**: a well-formed set of terms still
/// compares, hashes and sizes exactly as documented.
#[test]
fn valid_terms_are_unaffected() {
    let atoms = atoms();
    let mut heap = Heap::new(128);
    let tup = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
    let tail = heap.cons(make_small(2), NIL).unwrap();
    let list = heap.cons(make_small(1), tail).unwrap();

    // The documented term order: number < atom < tuple < nil < list.
    let ordered = [
        make_small(0),
        make_small(3),
        make_atom(atoms.lookup("a").unwrap()),
        tup,
        NIL,
        list,
    ];
    for (i, x) in ordered.iter().enumerate() {
        for (j, y) in ordered.iter().enumerate() {
            assert_eq!(cmp(*x, *y, &atoms), i.cmp(&j), "sample {i} vs {j}");
            assert_eq!(eq(*x, *y, &atoms), i == j, "sample {i} vs {j}");
            assert_eq!(
                term_hash(*x, &atoms) == term_hash(*y, &atoms),
                i == j,
                "hash consistency, sample {i} vs {j}"
            );
        }
    }

    // Pinned sizes (`size_object_x`): immediates 0, a cons cell 2 (+ tail), a tuple
    // `arity + 1`.
    assert_eq!(size_object(make_small(4)), 0);
    assert_eq!(size_object(NIL), 0);
    assert_eq!(size_object(tup), 3);
    assert_eq!(size_object(list), 4);

    // The pinned compatibility hashes still answer for valid terms (frozen vector
    // from the crate's own `make_hash` suite: `phash2(0, 2^32) == 3_175_731_469`,
    // i.e. `make_hash2(make_small(0)) == 0xBD49_D10D`).
    assert_eq!(make_hash2(make_small(0), &atoms), Ok(3_175_731_469));
    assert!(make_hash(make_small(0), &atoms).is_ok());
}
