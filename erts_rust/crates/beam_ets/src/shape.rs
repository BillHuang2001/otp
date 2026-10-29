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

//! Word-shape predicates: the gate every interior read of a term word goes
//! through.
//!
//! A word's *shape* (its primary tag, plus whether the pointer bits it carries
//! are non-null) is the only thing a word-local test can establish; whether the
//! storage it names is still alive is the owner's contract. Each predicate here
//! therefore answers "is this word shaped like X?", never "is this word a live
//! X?", and every call site says so in its `// SAFETY:` rationale.

use beam_term::prelude::*;

/// The `[CAR, CDR]` cell pointer of a **cons-cell-shaped** word, or `None` when
/// `term` is not shaped like a cons cell.
///
/// This is the shape predicate every list read in this crate is gated on, and it
/// is deliberately stricter than `beam_term::list::is_list`. `is_list` mirrors the
/// C bit trick `(x & (TAG_PRIMARY_MASK - TAG_PRIMARY_LIST)) != 0` — that is
/// `!(x & 0x2)` — which is **also** true for every `TAG_PRIMARY_HEADER` word
/// (primary tag `0`), a word that is not a term at all (`beam_term`'s own test
/// `is_list_matches_only_list_and_header_patterns` asserts exactly that). A stale
/// or foreign caller-heap word passing that trick and reaching a `car` read is the
/// measured SIGSEGV this predicate exists to stop.
///
/// Accepts only a word whose [`primary_tag`] is [`PrimaryTag::List`] (so a
/// `TAG_PRIMARY_HEADER` word, every immediate — `NIL` included, its primary tag is
/// `IMMED1` — and every boxed word are rejected) **and** whose
/// [`beam_term::list::list_val`] is non-null. `list_val` masks `!TAG_PTR_MASK`,
/// i.e. `!0x7`, so both the pointer tag and the `TAG_LITERAL_PTR` literal bit are
/// cleared and the result is word-aligned; `make_list` only ever adds
/// `TAG_PRIMARY_LIST` to the cell address, so a genuine cons word always survives
/// both checks.
///
/// **No Erlang-representable term changes classification.** `is_list` is true only
/// for `TAG_PRIMARY_LIST` words plus header words, and a header word is never a
/// value, so every word that was treated as a list before is still treated as one.
///
/// **This is a shape test, not a liveness or reachability test.** A word whose low
/// two bits happen to be `01` but which does not point at a live cell cannot be
/// distinguished from a real cons cell by any word-local test (ERTS's C has the
/// same residual); liveness comes from the caller's ownership contract — a term a
/// caller stores anywhere must stay rooted or be copied into storage it owns, and
/// a term passed in must outlive the call.
pub(crate) fn cons_cell(term: Eterm) -> Option<*mut Eterm> {
    if primary_tag(term) != PrimaryTag::List {
        return None;
    }
    let cell = list_val(term);
    (!cell.is_null()).then_some(cell)
}

#[cfg(test)]
mod tests {
    //! Unit tests for the word-shape gate. The same contract is pinned through
    //! the public API by `tests/select_word_classification.rs`.

    use super::*;

    /// A leaked `[CAR, CDR]` cell plus the list word naming it.
    ///
    /// The cell is leaked because the walk needs it to outlive this function's
    /// stack frame; a test process leaking a handful of 16-byte cells is harmless.
    fn cons(head: Eterm, tail: Eterm) -> Eterm {
        let cell: &'static mut [Eterm; 2] = Box::leak(Box::new([head, tail]));
        make_list(cell.as_ptr())
    }

    /// A proper list of `elements`, built from leaked cells.
    fn proper_list(elements: &[Eterm]) -> Eterm {
        let mut acc = NIL;
        for element in elements.iter().rev() {
            acc = cons(*element, acc);
        }
        acc
    }

    /// Every header-shaped word the predicate must refuse: arityvals of several
    /// arities plus two non-arityval thing headers (a float and a map header).
    fn header_words() -> Vec<Eterm> {
        vec![
            make_arityval_zero(),
            make_arityval(1),
            make_arityval(2),
            make_arityval(5),
            make_header(1, FLOAT_SUBTAG),
            make_header(1, MAP_SUBTAG),
        ]
    }

    #[test]
    fn cons_cell_agrees_with_the_primary_tag_over_the_sample_words() {
        let mut words = header_words();
        words.extend_from_slice(&[
            make_small(7),
            make_atom(Atom::from_raw(3)),
            NIL,
            Eterm::from_raw(0x2),
            Eterm::from_raw(TAG_PRIMARY_LIST),
            Eterm::from_raw(usize::MAX),
            proper_list(&[make_small(1), make_small(2)]),
            cons(make_small(1), make_small(2)),
        ]);
        for word in words {
            let expected = (primary_tag(word) == PrimaryTag::List) && !list_val(word).is_null();
            assert_eq!(cons_cell(word).is_some(), expected, "for {word:?}");
        }
    }

    #[test]
    fn a_header_word_is_not_cons_shaped() {
        // The reason `is_list` alone is unsound: the bit trick is true for a header
        // word (primary tag 0), while the shape gate rejects it outright.
        for word in header_words() {
            assert!(is_list(word), "a header word passes the is_list bit trick");
            assert_eq!(primary_tag(word), PrimaryTag::Header);
            assert_eq!(cons_cell(word), None, "a header word is not cons-shaped");
        }
    }

    #[test]
    fn a_list_word_with_a_null_cell_pointer_is_refused() {
        // `raw == TAG_PRIMARY_LIST` is the cons *tag* with a zero cell pointer — the
        // null check, not the tag check, is what stops this one from dereferencing
        // address 0.
        let word = Eterm::from_raw(TAG_PRIMARY_LIST);
        assert!(is_list(word), "the tag bit alone looks like a cons cell");
        assert!(list_val(word).is_null());
        assert_eq!(cons_cell(word), None);
    }

    #[test]
    fn a_real_cons_cell_is_accepted_with_its_cell_pointer() {
        for word in [
            cons(make_small(1), NIL),
            proper_list(&[make_small(1), make_small(2), make_small(3)]),
        ] {
            let cell = cons_cell(word).expect("a genuine cons cell is accepted");
            assert_eq!(cell, list_val(word));
            assert!(!cell.is_null());
        }
    }
}
