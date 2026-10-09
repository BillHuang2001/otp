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

//! Term-classification helpers — the engine's **only** `unsafe`.
//!
//! `beam_term`'s boxed-term predicates (`is_integer`, `is_tuple`, `is_map`, …)
//! are `unsafe fn`: each dereferences the header word of a boxed term, which is
//! only sound when the term is a *live machine word*. The interpreter cannot
//! classify a term for the `Test`/`test_arity` opcodes without them, so this
//! module is the single place that carries `#[allow(unsafe_code)]` under the
//! crate-wide `#![deny(unsafe_code)]` (mirroring `beam_codec::term`). No other
//! module in this crate uses `unsafe`.
//!
//! # Safety argument
//!
//! Every wrapper is called with a term the machine already holds — an X-register
//! or Y-slot value, or the result of a BIF (`ExecutionContext::call_bif`) — never
//! an unvalidated raw word. The *immediate* fast paths (small ints, atoms,
//! `NIL`, pids/ports) are handled by the safe predicates the interpreter calls
//! directly ([`beam_term::is_small`], [`beam_term::is_atom`], …); the helpers
//! here only ever dereference a term word after `beam_term` has confirmed it is
//! boxed.

use beam_term::Eterm;

/// `is_integer(x)` — a small integer or a bignum.
pub(crate) fn is_integer(x: Eterm) -> bool {
    // SAFETY: `x` is a live machine word; if boxed, its header is a live term word.
    unsafe { beam_term::is_integer(x) }
}

/// `is_number(x)` — an integer or a float.
pub(crate) fn is_number(x: Eterm) -> bool {
    // SAFETY: as [`is_integer`].
    unsafe { beam_term::is_number(x) }
}

/// `is_float(x)` — a boxed float (`HEADER_FLONUM`).
pub(crate) fn is_float(x: Eterm) -> bool {
    // SAFETY: `x` is a live machine word; `is_float` checks `is_boxed` before
    // dereferencing the header.
    unsafe { beam_term::is_float(x) }
}

/// `is_tuple(x)`.
pub(crate) fn is_tuple(x: Eterm) -> bool {
    // SAFETY: as [`is_float`].
    unsafe { beam_term::is_tuple(x) }
}

/// `is_tuple_arity(x, arity)`.
///
/// The caller must pass `arity <= beam_term::MAX_ARITYVAL` (a larger arity can
/// never match a real tuple header and would trip `beam_term`'s range assert).
///
/// For a **boxed** operand this is C's `test_arity` (`emu/instrs.tab:1055`) exactly
/// — `tuple_val(p) == boxed_val(p)` — and it is strictly safer for an immediate,
/// where C dereferences an out-of-term word (Handoff #6, behaviourally equivalent).
pub(crate) fn is_tuple_arity(x: Eterm, arity: usize) -> bool {
    // SAFETY: as [`is_float`]; `arity` is bounded by the caller.
    unsafe { beam_term::is_tuple_arity(x, arity) }
}

/// `is_map(x)`.
pub(crate) fn is_map(x: Eterm) -> bool {
    // SAFETY: as [`is_float`].
    unsafe { beam_term::is_map(x) }
}

/// `is_record(x)` — a boxed native record (`RECORD_SUBTAG`).
///
/// This is the C `is_record` macro (`erl_term.h:437`), **not** a tuple test: a
/// tuple carries the `ARITYVAL` subtag, so `is_record({a,b,c})` is `false`.
pub(crate) fn is_record(x: Eterm) -> bool {
    // SAFETY: as [`is_float`].
    unsafe { beam_term::is_record(x) }
}

/// `is_binary(x)` — a bitstring whose size is a whole number of bytes.
pub(crate) fn is_binary(x: Eterm) -> bool {
    // SAFETY: as [`is_float`].
    unsafe { beam_term::is_binary(x) }
}

/// `is_bitstring(x)`.
pub(crate) fn is_bitstring(x: Eterm) -> bool {
    // SAFETY: as [`is_float`].
    unsafe { beam_term::is_bitstring(x) }
}

/// `is_internal_ref(x)` — an on-heap reference.
pub(crate) fn is_internal_ref(x: Eterm) -> bool {
    // SAFETY: as [`is_float`].
    unsafe { beam_term::is_internal_ref(x) }
}

/// `is_external_pid(x)`.
pub(crate) fn is_external_pid(x: Eterm) -> bool {
    // SAFETY: as [`is_float`].
    unsafe { beam_term::is_external_pid(x) }
}

/// `is_external_port(x)`.
pub(crate) fn is_external_port(x: Eterm) -> bool {
    // SAFETY: as [`is_float`].
    unsafe { beam_term::is_external_port(x) }
}

/// `is_external_ref(x)`.
pub(crate) fn is_external_ref(x: Eterm) -> bool {
    // SAFETY: as [`is_float`].
    unsafe { beam_term::is_external_ref(x) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_term::{make_arityval, make_atom, make_small, make_tuple, Atom, NIL};
    #[test]
    fn immediates_never_dereference_a_header() {
        // Immediate fast paths: safe predicates agree with the boxed wrappers.
        assert!(is_integer(make_small(7)));
        assert!(is_number(make_small(7)));
        assert!(!is_float(make_small(7)));
        assert!(!is_tuple(make_small(7)));
        assert!(!is_map(NIL));
        assert!(!is_bitstring(make_atom(Atom::from_raw(1))));
        assert!(!is_binary(NIL));
        assert!(!is_internal_ref(NIL));
        assert!(!is_external_pid(NIL));
        assert!(!is_external_port(NIL));
        assert!(!is_external_ref(NIL));
        assert!(!is_tuple_arity(NIL, 0));
    }

    #[test]
    fn boxed_tuple_is_classified() {
        let mut words = [make_arityval(1), make_small(0)];
        let t = make_tuple(words.as_mut_ptr());
        assert!(is_tuple(t));
        assert!(is_tuple_arity(t, 1));
        assert!(!is_tuple_arity(t, 2));
        assert!(!is_map(t));
    }

    #[test]
    fn boxed_non_tuples_never_match_any_arity() {
        // The equivalence with C's `test_arity` (`emu/instrs.tab:1055`): for a boxed
        // operand both read the very same header word, which is an `ARITYVAL` only
        // for a tuple — so a float / bignum / record never matches *any* arity.
        let mut float_words = [beam_term::HEADER_FLONUM, make_small(0)];
        let float = beam_term::make_boxed(float_words.as_mut_ptr());

        let mut big_words = [
            beam_term::make_header(1, beam_term::POS_BIG_SUBTAG),
            make_small(0),
        ];
        let big = beam_term::make_boxed(big_words.as_mut_ptr());

        let mut record_words = [
            beam_term::make_header(2, beam_term::RECORD_SUBTAG),
            make_small(0),
            make_small(0),
        ];
        let record = beam_term::make_boxed(record_words.as_mut_ptr());

        for x in [float, big, record] {
            assert!(!is_tuple(x));
            for arity in 0..=4 {
                assert!(!is_tuple_arity(x, arity));
            }
        }
    }

    #[test]
    fn a_record_is_not_a_tuple_and_vice_versa() {
        // `is_record` is the `RECORD_SUBTAG` test, not a tuple test: a tuple is
        // *not* a native record (`erlang:is_record/1`: `is_record({a,b,c}) == false`)
        // and a record is not a tuple.
        let record_words = [
            beam_term::make_header(2, beam_term::RECORD_SUBTAG),
            make_small(0),
            make_small(0),
        ];
        let record = beam_term::make_boxed(record_words.as_ptr());
        assert!(is_record(record));
        assert!(!is_tuple(record));

        let mut tuple_words = [make_arityval(2), make_small(0), make_small(0)];
        let tuple = make_tuple(tuple_words.as_mut_ptr());
        assert!(!is_record(tuple));

        // Immediates are never records.
        assert!(!is_record(NIL));
        assert!(!is_record(make_small(7)));
    }
}
