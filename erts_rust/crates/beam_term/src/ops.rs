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

//! Cross-cutting classifiers: the virtual term class (`tag_val_def`), numeric
//! predicates and the magic non-value.
//!
//! Ground truth: the "Backwards compatibility definitions" block of
//! `erts/emulator/beam/erl_term.h` — the `*_DEF` virtual tags fit the BEAM term
//! order `number < atom < ref < fun < port < pid < tuple < record < map < nil <
//! cons < binary`, which comparison and `terms` code rely on.

use crate::boxed::{boxed_val, is_big, make_header};
use crate::eterm::Eterm;
use crate::immediate::{is_immediate, is_small};
use crate::tag::{
    is_term_word, ARITYVAL_SUBTAG, BIN_REF_SUBTAG, EXTERNAL_PID_SUBTAG, EXTERNAL_PORT_SUBTAG,
    EXTERNAL_REF_SUBTAG, FLOAT_SUBTAG, FUN_SUBTAG, HEAP_BITS_SUBTAG, MAP_SUBTAG, NEG_BIG_SUBTAG,
    POS_BIG_SUBTAG, RECORD_SUBTAG, REF_SUBTAG, SUB_BITS_SUBTAG, TAG_HEADER_MASK, TAG_IMMED1_MASK,
    TAG_IMMED1_PID, TAG_IMMED1_PORT, TAG_IMMED1_SMALL, TAG_IMMED2_ATOM, TAG_IMMED2_MASK,
    TAG_IMMED2_NIL, TAG_PRIMARY_BOXED, TAG_PRIMARY_HEADER, TAG_PRIMARY_IMMED1, TAG_PRIMARY_LIST,
    TAG_PRIMARY_MASK,
};
use crate::tuple::tuple0;

/// The virtual term class (`tag_val_def`), ordered as BEAM's term order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TermClass {
    /// A bitstring (heap or sub bits).
    Bitstring,
    /// A cons cell.
    List,
    /// The empty list `[]`.
    Nil,
    /// A map.
    Map,
    /// A native record.
    Record,
    /// A tuple.
    Tuple,
    /// An internal pid.
    Pid,
    /// An external pid.
    ExternalPid,
    /// An internal port.
    Port,
    /// An external port.
    ExternalPort,
    /// A fun/lambda.
    Fun,
    /// An internal or external reference.
    Ref,
    /// An external reference.
    ExternalRef,
    /// An atom.
    Atom,
    /// A float.
    Float,
    /// A bignum.
    Big,
    /// A small integer.
    Small,
    /// An off-heap binary reference (not a "real" term).
    BinRef,
    /// A word that is **not a term at all** — a bare header word, i.e. one whose
    /// primary tag is `HEADER` (see [`crate::tag::is_term_word`]). This is the C
    /// ABI's `THE_NON_VALUE` (`0`, or `make_header(0, FLOAT_SUBTAG)` in a debug
    /// build) and any other stray header word.
    ///
    /// The C's `tag_val_def` has **no** answer for such a word: the switch falls
    /// through to `erl_assert_error("tag_val_def error")`, which is a debug
    /// assertion (a no-op that falls off the end of a non-`void` function —
    /// undefined behaviour — in a release build). This variant is that missing
    /// answer: classifying a non-term is a defined, documented result instead of a
    /// panic, and every `is_*` test written against a class is `false` for it, so
    /// no caller can mistake it for a real term. It sorts **after** every real
    /// class in `class_rank` terms (see [`crate::compare`]).
    NonValue,
}

/// `THE_NON_VALUE` — the magic non-value. Zero in release; a `FLOAT` header in
/// debug builds (mirrors the C `ET_DEBUG` scheme for catching unwarranted
/// assumptions about zero).
pub const THE_NON_VALUE: Eterm = if cfg!(debug_assertions) {
    make_header(0, FLOAT_SUBTAG)
} else {
    Eterm::from_raw(0)
};

/// `is_non_value(x)`.
#[inline]
pub const fn is_non_value(x: Eterm) -> bool {
    x.raw() == THE_NON_VALUE.raw()
}

/// `is_value(x)`.
#[inline]
pub const fn is_value(x: Eterm) -> bool {
    !is_non_value(x)
}

/// `is_zero_sized(x)` — an immediate, or the shared empty tuple.
#[inline]
pub fn is_zero_sized(x: Eterm) -> bool {
    is_immediate(x) || x.raw() == tuple0().raw()
}

/// `is_integer(x)` — a small integer or a bignum.
///
/// A word that is not a term at all (a bare header word, e.g. the ABI's
/// `THE_NON_VALUE`) is `false`, never a dereference.
///
/// # Safety
///
/// If `x` is a boxed term, its header word must be a live, aligned term word.
#[inline]
pub unsafe fn is_integer(x: Eterm) -> bool {
    if !is_term_word(x) {
        return false;
    }
    is_small(x) || unsafe { is_big(x) }
}

/// `is_number(x)` — an integer or a float.
///
/// A word that is not a term at all (a bare header word, e.g. the ABI's
/// `THE_NON_VALUE`) is `false`, never a dereference.
///
/// # Safety
///
/// If `x` is a boxed term, its header word must be a live, aligned term word.
#[inline]
pub unsafe fn is_number(x: Eterm) -> bool {
    if !is_term_word(x) {
        return false;
    }
    unsafe { is_integer(x) || crate::boxed::is_float(x) }
}

/// `tag_val_def(x)` — classify a term word.
///
/// A word that is not a term at all (a bare header word: the ABI's
/// `THE_NON_VALUE` and friends, see [`crate::tag::is_term_word`]) classifies as
/// [`TermClass::NonValue`] — never a panic and never a dereference. The C has no
/// answer for such a word at all (`erl_assert_error` in debug, undefined
/// behaviour in release), so this is a documented, panic-free extension.
///
/// The two remaining `panic!`s are unreachable without forged memory: they fire
/// only when a **boxed term's own header word** carries an unknown subtag, and
/// this crate writes every header it hands out. They mirror the C's
/// `erl_assert_error` internal-consistency assertion rather than a runtime
/// error.
///
/// # Safety
///
/// If `x` is a boxed term, its header word must be a live, aligned term word of
/// one of the known subtags.
#[inline]
pub unsafe fn class(x: Eterm) -> TermClass {
    match x.raw() & TAG_PRIMARY_MASK {
        TAG_PRIMARY_HEADER => TermClass::NonValue,
        TAG_PRIMARY_LIST => TermClass::List,
        TAG_PRIMARY_BOXED => {
            let hdr = unsafe { *boxed_val(x) };
            match hdr.raw() & TAG_HEADER_MASK {
                ARITYVAL_SUBTAG => TermClass::Tuple,
                RECORD_SUBTAG => TermClass::Record,
                POS_BIG_SUBTAG | NEG_BIG_SUBTAG => TermClass::Big,
                REF_SUBTAG => TermClass::Ref,
                FLOAT_SUBTAG => TermClass::Float,
                FUN_SUBTAG => TermClass::Fun,
                EXTERNAL_PID_SUBTAG => TermClass::ExternalPid,
                EXTERNAL_PORT_SUBTAG => TermClass::ExternalPort,
                EXTERNAL_REF_SUBTAG => TermClass::ExternalRef,
                MAP_SUBTAG => TermClass::Map,
                BIN_REF_SUBTAG => TermClass::BinRef,
                HEAP_BITS_SUBTAG | SUB_BITS_SUBTAG => TermClass::Bitstring,
                other => panic!("class: invalid boxed header subtag {other:#x}"),
            }
        }
        TAG_PRIMARY_IMMED1 => match x.raw() & TAG_IMMED1_MASK {
            TAG_IMMED1_PID => TermClass::Pid,
            TAG_IMMED1_PORT => TermClass::Port,
            TAG_IMMED1_SMALL => TermClass::Small,
            _ => match x.raw() & TAG_IMMED2_MASK {
                TAG_IMMED2_ATOM => TermClass::Atom,
                TAG_IMMED2_NIL => TermClass::Nil,
                other => panic!("class: invalid immediate2 tag {other:#x}"),
            },
        },
        // Unreachable: the primary tag is two bits, so the mask above always
        // selects one of the four arms.
        _ => TermClass::NonValue,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxed::{make_arityval, make_boxed, HEADER_FLONUM};
    use crate::immediate::{make_atom, make_small, NIL};
    use crate::list::{is_list, make_list};
    use crate::tuple::{is_tuple, make_tuple};

    #[test]
    fn class_of_immediates() {
        unsafe {
            assert_eq!(class(make_small(7)), TermClass::Small);
            assert_eq!(class(make_atom(crate::Atom::from_raw(3))), TermClass::Atom);
            assert_eq!(class(NIL), TermClass::Nil);
        }
    }

    #[test]
    fn class_of_boxed_and_list() {
        let mut tup_words = [make_arityval(1), make_small(0)];
        let tup = make_tuple(tup_words.as_mut_ptr());
        let cons_words = [make_small(0), NIL];
        let cons = make_list(cons_words.as_ptr());
        let float_words = [HEADER_FLONUM, make_small(0)];
        let float = make_boxed(float_words.as_ptr());
        unsafe {
            assert_eq!(class(tup), TermClass::Tuple);
            assert_eq!(class(cons), TermClass::List);
            assert_eq!(class(float), TermClass::Float);
            assert!(is_tuple(tup));
            assert!(is_list(cons));
        }
    }

    #[test]
    fn non_value_definition() {
        // Release builds use 0; debug builds use a FLOAT header. Both must be
        // distinguishable from real values.
        if cfg!(debug_assertions) {
            assert_eq!(THE_NON_VALUE, make_header(0, FLOAT_SUBTAG));
        } else {
            assert_eq!(THE_NON_VALUE.raw(), 0);
        }
        assert!(is_non_value(THE_NON_VALUE));
        assert!(!is_value(THE_NON_VALUE));
        assert!(is_value(make_small(0)));
        assert!(is_value(NIL));
    }

    #[test]
    fn a_non_term_classifies_as_non_value() {
        // Every word whose primary tag is `HEADER` is not a term: `class` answers
        // `NonValue` (the C panics/UBs here) and no predicate comes near a
        // dereference.
        let mut bare_headers = vec![
            THE_NON_VALUE,
            make_header(0, FLOAT_SUBTAG),
            crate::boxed::HEADER_FLONUM,
            make_header(1, FUN_SUBTAG),
        ];
        // A few more raw header words, including the `HEADER`-tagged `0`.
        bare_headers.push(Eterm::from_raw(0x3C));
        for word in bare_headers {
            assert!(crate::tag::is_header(word), "test word {word:?} is a header");
            assert!(!is_term_word(word));
            // SAFETY: `class` is total for a non-term word — that is the property
            // under test (a valid term would carry its usual invariants).
            assert_eq!(unsafe { class(word) }, TermClass::NonValue);
            // SAFETY: as above.
            assert!(!unsafe { is_integer(word) });
            // SAFETY: as above.
            assert!(!unsafe { is_number(word) });
            assert!(!is_zero_sized(word));
        }
        // Every *valid* term is still a term word, and still classifies as before.
        assert!(is_term_word(make_small(0)));
        assert!(is_term_word(NIL));
        assert!(is_term_word(tuple0()));
        // SAFETY: all three are live terms.
        unsafe {
            assert_eq!(class(make_small(0)), TermClass::Small);
            assert_eq!(class(NIL), TermClass::Nil);
            assert_eq!(class(tuple0()), TermClass::Tuple);
        }
    }

    #[test]
    fn integer_and_number_predicates() {
        let big_words = [crate::boxed::make_header(1, POS_BIG_SUBTAG), make_small(0)];
        let big = make_boxed(big_words.as_ptr());
        let float_words = [HEADER_FLONUM, make_small(0)];
        let float = make_boxed(float_words.as_ptr());
        unsafe {
            assert!(is_integer(make_small(1)));
            assert!(is_integer(big));
            assert!(!is_integer(float));
            assert!(is_number(make_small(1)));
            assert!(is_number(big));
            assert!(is_number(float));
            assert!(!is_number(NIL));
            assert!(!is_number(make_atom(crate::Atom::from_raw(1))));
        }
    }

    #[test]
    fn zero_sized_terms() {
        assert!(is_zero_sized(make_small(0)));
        assert!(is_zero_sized(NIL));
        assert!(is_zero_sized(tuple0()));
        let cons_words = [make_small(0), NIL];
        assert!(!is_zero_sized(make_list(cons_words.as_ptr())));
    }
}
