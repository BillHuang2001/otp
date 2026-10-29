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

//! Word classification at the match-spec boundary: a spec word that is **not**
//! shaped like a cons cell must be a typed `badarg`, never a read.
//!
//! `beam_term::list::is_list` mirrors the C bit trick `!(x & 0x2)`, which is also
//! true for every `TAG_PRIMARY_HEADER` word — a word that is not a term at all.
//! The spec decode walk used to be gated on it alone, so a header-shaped spec word
//! (or a header-shaped guards/body list inside an otherwise well-formed clause)
//! reached a `car` dereference: a measured SIGSEGV. `select::list_elements` now
//! gates every cell read on the `cons_cell` shape predicate — primary tag
//! `TAG_PRIMARY_LIST` **and** a non-null cell pointer — so those shapes are the
//! `badarg` the oracle raises for a non-list spec.
//!
//! ## The stated residual
//!
//! The shape test cannot validate **reachability**. A word whose low two bits
//! happen to be `01` but which points at freed or foreign memory is
//! indistinguishable from a genuine cons cell by any word-local test — ERTS's C
//! has exactly the same residual — so the randomised sweep below deliberately
//! excludes `LIST`-tagged words (it only asserts about words that *cannot* be a
//! cons pointer). The real contract is the ownership rule upstream: a spec word a
//! caller stores anywhere must stay rooted or be copied into its own storage, and
//! the spec term passed to `compile`/`match_spec_run` must stay live for the call.
//!
//! ## The nested positions that are now a typed `badarg`, not a read
//!
//! A header word nested anywhere inside an otherwise well-formed spec is the
//! `badarg` the oracle raises for a malformed spec. Three shapes are pinned below
//! (plus the clause-position one); the **first two used to SIGSEGV**, because
//! `pattern::decode` and `expr::decode` only had the `is_list`-guarded cons walk:
//!
//! * `[{HeaderWord, [], ['$_']}]` — the head pattern is a header word;
//! * `[{[], HeaderWord, ['$_']}]` — the guards list is a header word;
//! * `[{[], [], HeaderWord}]` — the body list is a header word.
//!
//! Both `decode`s now reject a non-term word (`beam_term::tag::is_term_word`) up
//! front and recurse, so the check fires at every nesting depth, and every list
//! read in the crate walks through `beam_ets::shape::cons_cell` (primary tag
//! `TAG_PRIMARY_LIST` **and** a non-null cell pointer) rather than `is_list`.
//!
//! `pattern::matches`'s walk is covered too: an object whose **element** is a tuple
//! holding a header word, matched against a list-pattern head, is a non-match (or a
//! binding), never a dereference.

use beam_alloc::Heap;
use beam_common::BeamError;
use beam_ets::prelude::*;
use beam_term::prelude::*;

/// The atoms the specs below reference.
fn atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    for name in ["_", "$_", "$1", "true", "false"] {
        atoms.intern(name);
    }
    atoms
}

fn at(atoms: &AtomTable, name: &str) -> Eterm {
    make_atom(atoms.lookup(name).expect("interned by `atoms`"))
}

/// Header-shaped words: arityvals of several arities (a tuple header is a valid
/// word in a tuple, never as a standalone term) plus two non-arityval thing headers.
fn header_words() -> Vec<(&'static str, Eterm)> {
    vec![
        ("arityval 0", make_arityval_zero()),
        ("arityval 1", make_arityval(1)),
        ("arityval 2", make_arityval(2)),
        ("arityval 5", make_arityval(5)),
        ("float header", make_header(1, FLOAT_SUBTAG)),
        ("map header", make_header(1, MAP_SUBTAG)),
    ]
}

/// Assert that `result` is the `badarg` the oracle raises for a malformed spec.
fn assert_bad_arg<T: core::fmt::Debug>(result: Result<T, BeamError>, what: &str) {
    match result {
        Err(BeamError::BadArg) => {}
        other => panic!("{what}: expected Err(BadArg), got {other:?}"),
    }
}

#[test]
fn compile_refuses_every_header_shaped_spec_word() {
    let atoms = atoms();
    for (name, word) in header_words() {
        // The old guard (`is_list`) is true for these — a header word's primary tag
        // is 0, and `!(x & 0x2)` cannot tell it from a cons cell.
        assert!(is_list(word), "{name}: a header word passes the is_list bit trick");
        assert_bad_arg(compile(word, &atoms), name);
    }
}

#[test]
fn compile_refuses_every_other_non_list_spec_word() {
    let atoms = atoms();
    let words = [
        ("small 1", make_small(1)),
        ("small 0", make_small(0)),
        ("small -1", make_small(-1)),
        ("an atom", at(&atoms, "_")),
        ("boxed-shaped", Eterm::from_raw(0x2)),
        ("all ones", Eterm::from_raw(usize::MAX)),
        ("bare cons tag", Eterm::from_raw(TAG_PRIMARY_LIST)),
    ];
    for (name, word) in words {
        assert_bad_arg(compile(word, &atoms), name);
    }
}

#[test]
fn compile_treats_nil_as_an_empty_spec() {
    // Pinned current behaviour: `[]` *is* a proper list, so it decodes to a spec
    // with zero clauses (nothing to match, nothing to fail).
    let atoms = atoms();
    let spec = compile(NIL, &atoms).expect("the empty spec is well formed");
    assert!(spec.clauses.is_empty(), "[] decodes to zero clauses");
}

#[test]
fn compile_refuses_a_header_word_inside_a_well_formed_clause() {
    // The three positions the hardening pass closed, plus the clause position: the
    // spec list itself parses, and the header word is reached one level in. The
    // head and guards shapes used to SIGSEGV.
    let atoms = atoms();
    let mut heap = Heap::new(1024).expect("test heap");
    let header = make_arityval(1);
    let underscore = at(&atoms, "_");
    let whole_object = at(&atoms, "$_");

    // A well-formed control first: this proves the shape below is otherwise valid.
    let body_list = heap.cons(whole_object, NIL).expect("test heap has room");
    let clause = heap.tuple(&[underscore, NIL, body_list]).expect("heap has room");
    let spec = heap.cons(clause, NIL).expect("test heap has room");
    assert!(
        compile(spec, &atoms).is_ok(),
        "the control clause {{'_', [], ['$_']}} compiles"
    );

    // The head pattern is a header word: previously a SIGSEGV (the head decode fell
    // through to the `is_list`-guarded cons walk, and `!(x & 0x2)` is true for a
    // header word). `pattern::decode` now rejects a non-term word up front.
    let clause = heap
        .tuple(&[header, NIL, body_list])
        .expect("test heap has room");
    let spec = heap.cons(clause, NIL).expect("test heap has room");
    assert_bad_arg(compile(spec, &atoms), "a header word as the head pattern");

    // The guards list is a header word.
    let clause = heap
        .tuple(&[underscore, header, body_list])
        .expect("test heap has room");
    let spec = heap.cons(clause, NIL).expect("test heap has room");
    assert_bad_arg(compile(spec, &atoms), "a header word as the guards list");

    // The body list is a header word.
    let clause = heap
        .tuple(&[underscore, NIL, header])
        .expect("test heap has room");
    let spec = heap.cons(clause, NIL).expect("test heap has room");
    assert_bad_arg(compile(spec, &atoms), "a header word as the body list");

    // The clause itself (an element of the spec list) is a header word: it reaches
    // `tuple_elements`, which rejects it without a read.
    let spec = heap.cons(header, NIL).expect("test heap has room");
    assert_bad_arg(compile(spec, &atoms), "a header word as a clause");

}

#[test]
fn match_spec_run_refuses_a_header_shaped_spec() {
    let atoms = atoms();
    let mut heap = Heap::new(1024).expect("test heap");
    // The element list is a proper list holding one 2-tuple, so the run would
    // succeed if the spec were valid.
    let element = heap
        .tuple(&[make_small(1), make_small(2)])
        .expect("test heap has room");
    let input = heap.cons(element, NIL).expect("test heap has room");

    for (name, word) in header_words() {
        assert_bad_arg(
            match_spec_run(input, word, &atoms, &mut heap),
            &format!("match_spec_run with a {name} spec"),
        );
    }
    assert_bad_arg(
        match_spec_run(input, Eterm::from_raw(usize::MAX), &atoms, &mut heap),
        "match_spec_run with the all-ones spec",
    );
}

#[test]
fn match_spec_run_walks_an_object_list_without_reading_a_header_word() {
    // `pattern::matches`'s cons walk: the head is a **list pattern**
    // (`['$1']` = `Cons(Bind(1), Literal([]))`) and the objects exercise a tuple that
    // contains a header word, a list whose element *is* a header word, and a plain
    // list. None of the three may be dereferenced through a header/shape mismatch:
    // the tuple is refused by the cons-cell shape test, and the header word rides
    // through the `Bind` (a binding reads nothing) and the `Literal([])` tail.
    //
    // Residual, unchanged: a word whose low bits look like a cons pointer but which
    // points at freed/foreign memory is unverifiable by any word-local test — the
    // ownership rule upstream (a stored word must stay rooted or be copied) is the
    // contract there. This test therefore only builds words that *cannot* be a cons
    // pointer.
    let atoms = atoms();
    let mut heap = Heap::new(1024).expect("test heap");
    let header = make_arityval(1);
    let bind1 = at(&atoms, "$1");
    let whole_object = at(&atoms, "$_");

    // The head pattern `['$1']`.
    let head_nil = heap.cons(bind1, NIL).expect("test heap has room");
    // `['$_']` body list.
    let body_list = heap.cons(whole_object, NIL).expect("test heap has room");
    let clause = heap.tuple(&[head_nil, NIL, body_list]).expect("test heap has room");
    let spec = heap.cons(clause, NIL).expect("test heap has room");
    compile(spec, &atoms).expect("the list-pattern head compiles");

    // `{1, HeaderWord}` — a tuple containing a header word.
    let tuple_with_header = heap
        .tuple(&[make_small(1), header])
        .expect("test heap has room");
    // `[HeaderWord]` — a one-element list whose only element is a header word.
    let list_with_header = heap.cons(header, NIL).expect("test heap has room");
    // `[1, 2]` — a two-element list, so `['$1']` does not match it.
    let two = heap.cons(make_small(2), NIL).expect("test heap has room");
    let one_two = heap.cons(make_small(1), two).expect("test heap has room");

    let (input, tuple_with_header, list_with_header) = {
        // Built in reverse so every cons cell stays alive on `heap` for the run.
        let rest = heap.cons(list_with_header, NIL).expect("test heap has room");
        let rest = heap.cons(tuple_with_header, rest).expect("test heap has room");
        let input = heap.cons(one_two, rest).expect("test heap has room");
        (input, tuple_with_header, list_with_header)
    };

    let results = match_spec_run(input, spec, &atoms, &mut heap).expect("no crash, no error");
    // `[1, 2]` has two elements, so the one-element list pattern does not match it;
    // the tuple is not a cons at all; `[HeaderWord]` matches and its `'$_'` value is
    // the element itself (a `'$_'` body aliases, it does not copy — so the raw word
    // is identical).
    assert_eq!(results.len(), 1, "only the one-element list matches: {results:?}");
    assert_eq!(
        results[0].raw(),
        list_with_header.raw(),
        "the matched object is the `[HeaderWord]` list itself"
    );
    assert_ne!(
        results[0].raw(),
        tuple_with_header.raw(),
        "the tuple containing a header word never matched the list pattern"
    );
}

#[test]
fn key_of_refuses_a_header_shaped_object() {
    // `key_of` routes through `is_tuple` -> `beam_term::boxed::is_boxed_term`, which
    // rejects a header word before dereferencing, so this path was already safe.
    for (name, word) in header_words() {
        assert_bad_arg(key_of(word, 1), name);
    }
    for (name, word) in [
        ("small 1", make_small(1)),
        ("NIL", NIL),
        ("all ones", Eterm::from_raw(usize::MAX)),
    ] {
        assert_bad_arg(key_of(word, 1), name);
    }
    // A boxed-shaped word (raw `0x2`) is deliberately NOT probed here: `is_tuple`
    // dereferences it, so it belongs to the "cons-shaped low bits pointing at
    // foreign memory" residual instead.
}

/// One SplitMix64 step — the sweep's fixed, `rand`-free word generator.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The shape test, written independently of `cons_cell` (raw masks rather than
/// `primary_tag`/`list_val`), so the sweep cross-checks the two derivations.
fn looks_like_cons(word: Eterm) -> bool {
    let raw = word.raw();
    (raw & TAG_PRIMARY_MASK) == TAG_PRIMARY_LIST && (raw & !TAG_PTR_MASK) != 0
}

#[test]
fn a_deterministic_word_sweep_only_ever_sees_typed_errors() {
    let atoms = atoms();
    let mut heap = Heap::new(1024).expect("test heap");

    let mut sampled = 0usize;
    let mut probed = 0usize;
    for seed in [1_u64, 0xDEAD_BEEF, 0x1234_5678_9ABC_DEF0] {
        let mut state = seed;
        for _ in 0..1340 {
            let word = Eterm::from_raw(splitmix64(&mut state) as usize);
            sampled += 1;

            // `cons_cell`'s shape test, in its two public spellings.
            let tag = primary_tag(word);
            let pointer_is_non_null = !list_val(word).is_null();
            assert_eq!(
                looks_like_cons(word),
                tag == PrimaryTag::List && pointer_is_non_null,
                "the two spellings of the shape test disagree for {word:?} (raw {:#x})",
                word.raw()
            );

            // Only words that CANNOT be a cons pointer are probed: a `LIST`-tagged
            // word may name freed/foreign memory (the documented residual), and
            // reading through it is exactly what no word-local test can rule out.
            if tag == PrimaryTag::List || is_nil(word) {
                continue;
            }
            probed += 1;
            assert_bad_arg(compile(word, &atoms), "swept spec word");
            assert_bad_arg(
                match_spec_run(NIL, word, &atoms, &mut heap),
                "swept match_spec_run spec word",
            );
        }
    }
    assert_eq!(sampled, 4020, "the sweep is a fixed, reproducible sample");
    assert!(
        probed > sampled / 2,
        "most random words are not list-tagged ({probed} of {sampled} probed)"
    );
}

#[test]
fn the_explicit_corner_words_are_typed_errors() {
    let atoms = atoms();
    let mut heap = Heap::new(1024).expect("test heap");

    // The header corner: every arity, arity 0 included.
    for (name, word) in header_words() {
        assert_bad_arg(compile(word, &atoms), name);
        assert_bad_arg(match_spec_run(NIL, word, &atoms, &mut heap), name);
    }
    // The remaining explicit corners named by the hardening contract.
    let corners = [
        ("raw 0 (the zero-arity header)", Eterm::from_raw(0)),
        ("small 0", make_small(0)),
        ("small -1", make_small(-1)),
        ("an atom word", at(&atoms, "_")),
        ("a boxed-shaped word", Eterm::from_raw(0x2)),
        ("the all-ones word", Eterm::from_raw(usize::MAX)),
        ("the bare cons tag", Eterm::from_raw(TAG_PRIMARY_LIST)),
    ];
    for (name, word) in corners {
        assert_bad_arg(compile(word, &atoms), name);
        assert_bad_arg(match_spec_run(NIL, word, &atoms, &mut heap), name);
    }
    // `NIL` is the one non-`LIST` word that is a *valid* spec (an empty one), and
    // running it over the empty list matches nothing.
    assert!(compile(NIL, &atoms).is_ok());
    assert_eq!(
        match_spec_run(NIL, NIL, &atoms, &mut heap).expect("an empty spec runs"),
        Vec::new()
    );
}
