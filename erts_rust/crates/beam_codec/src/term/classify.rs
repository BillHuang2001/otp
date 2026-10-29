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
//! Tag-first classification of a **boxed** term: one header read yields the
//! [`BoxedTag`] the encoder dispatches on, instead of a chain of per-family
//! predicates.
//!
//! This is the term-layer half of the encoder's dispatch (see
//! [`crate::encode::Encoder::encode`]): the raw header word is read here so the
//! safe `encode` module never touches a heap word itself (the crate confines
//! `unsafe` to this `term` subtree).
use beam_term::prelude::{boxed_val, header_subtag, is_boxed_term, BoxedTag, Eterm};

/// The [`BoxedTag`] of `term` — its header's subtag — or `None` when `term` is not
/// a boxed term (a small/atom/`NIL`, a cons cell, or a bare header word) or carries
/// a reserved subtag `tag_val_def` would abort on.
///
/// `is_boxed_term` is the guard: it admits exactly a `BOXED` primary tag, so the
/// header read below is of a live thing block. Its result is therefore a pure
/// function of the term's tag, never a guess — and matches the per-family
/// predicates (`is_float`, `is_tuple`, …) it replaces, because each of those tests
/// the same header word.
pub fn boxed_tag(term: Eterm) -> Option<BoxedTag> {
    if !is_boxed_term(term) {
        return None;
    }
    // SAFETY: `is_boxed_term` established that `term` is a boxed pointer to a live
    // thing block, so its first word is that block's (initialised) header word.
    let header = unsafe { *boxed_val(term) };
    BoxedTag::from_raw(header_subtag(header))
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_term::prelude::{make_boxed, make_header, make_list, make_small, ARITYVAL_SUBTAG, NIL};

    #[test]
    fn a_non_boxed_term_has_no_boxed_tag() {
        assert_eq!(boxed_tag(make_small(7)), None);
        assert_eq!(boxed_tag(NIL), None);
        let cell = [make_small(1), NIL];
        assert_eq!(boxed_tag(make_list(cell.as_ptr())), None);
        // A bare header word is not a term at all.
        assert_eq!(boxed_tag(make_header(0, ARITYVAL_SUBTAG)), None);
    }

    #[test]
    fn a_tuple_header_classifies_as_arityval() {
        // `[arityval(0)]` is the empty tuple's block.
        let block = [make_header(0, ARITYVAL_SUBTAG)];
        let tuple = make_boxed(block.as_ptr());
        assert_eq!(boxed_tag(tuple), Some(BoxedTag::ArityVal));
    }

    #[test]
    fn a_reserved_subtag_is_none() {
        // `0x04` is one of the reserved subtags `BoxedTag::from_raw` rejects.
        let block = [make_header(0, 0x04)];
        let term = make_boxed(block.as_ptr());
        assert_eq!(boxed_tag(term), None);
    }
}
